import Foundation
import XCTest
@testable import RustyProtocol

final class ProtocolTests: XCTestCase {
    func testRustFixturesRoundTripCanonically() throws {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "protocol", withExtension: "ndjson", subdirectory: "Fixtures"))
        let data = try Data(contentsOf: url)
        var parser = FrameDecoder()
        var frames: [Data] = []
        // Deliberately split JSON, escaped newlines and Unicode scalar bytes.
        for offset in stride(from: 0, to: data.count, by: 3) {
            frames += try parser.feed(data[offset..<min(offset + 3, data.count)])
        }
        try parser.finish()
        XCTAssertEqual(frames.count, 9)
        let hello = try JSONDecoder().decode(Event.self, from: frames[0])
        guard case .object(let helloPayload) = hello.payload else { return XCTFail("hello payload") }
        XCTAssertEqual(helloPayload["schema_hash"], .string(protocolSchemaHash))
        XCTAssertEqual(protocolSchemaHash.count, 64)
        for frame in frames {
            let decoded = try WireFrame.decode(frame)
            if case .event(let event) = decoded { _ = try event.decodedPayload() }
            let encoded = try decoded.encoded().dropLast()
            let before = try JSONSerialization.jsonObject(with: frame) as? NSDictionary
            let after = try JSONSerialization.jsonObject(with: Data(encoded)) as? NSDictionary
            XCTAssertEqual(before, after)
        }
        let approval = try JSONDecoder().decode(Request.self, from: frames[3])
        let payload = try JSONEncoder().encode(approval.params)
        let params = try JSONDecoder().decode(ApprovalParams.self, from: payload)
        XCTAssertEqual(params.approval_id, "approval-1")
        XCTAssertEqual(params.operation_digest, "sha256:fixture-operation")
        XCTAssertEqual(params.typed, "yes")
        XCTAssertTrue(params.allowed)
    }
    func testAdditiveEnvelopeAndMissingOptionalFields() throws {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "forward-compat", withExtension: "ndjson", subdirectory: "Fixtures"))
        let data = try Data(contentsOf: url)
        var parser = FrameDecoder()
        let frames = try parser.feed(data)
        try parser.finish()
        XCTAssertEqual(frames.count, 3)
        guard case .event(let event) = try WireFrame.decode(frames[0]) else { return XCTFail("event required") }
        XCTAssertEqual(event.kind, "future.optional.notice")
        for frame in frames.dropFirst() {
            guard case .request(let request) = try WireFrame.decode(frame),
                  case .ApprovalDecide(let params) = try request.command() else { return XCTFail("approval required") }
            XCTAssertNil(params.typed)
            XCTAssertFalse(params.allowed)
        }
    }
    func testFramingRejectsOversizeAndPartialEOF() throws {
        var parser = FrameDecoder()
        XCTAssertThrowsError(try parser.feed(Data(repeating: 120, count: maxFrameBytes + 1))) {
            XCTAssertEqual($0 as? FramingError, .tooLarge)
        }
        parser = FrameDecoder()
        _ = try parser.feed(Data("{}".utf8))
        XCTAssertThrowsError(try parser.finish()) { XCTAssertEqual($0 as? FramingError, .truncated) }
    }
    func testMalformedAndWrongVersionFailClosed() throws {
        XCTAssertThrowsError(try WireFrame.decode(Data("broken".utf8)))
        let unknown = Request(protocol: 1, request_id: "r", method: "unknown", params: .object([:]))
        XCTAssertThrowsError(try WireFrame.decode(JSONEncoder().encode(unknown)))
        let request = Request(protocol: 99, request_id: "r", method: "snapshot", params: .object([:]))
        XCTAssertThrowsError(try WireFrame.decode(JSONEncoder().encode(request))) {
            XCTAssertEqual($0 as? FramingError, .unsupportedVersion)
        }
    }
    func testRecordedWorkerJourneyDecodesTypedPayloads() throws {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "recorded-worker", withExtension: "ndjson", subdirectory: "Fixtures"))
        var parser = FrameDecoder()
        let frames = try parser.feed(Data(contentsOf: url))
        try parser.finish()
        var kinds = Set<String>()
        var last: UInt64 = 0
        for frame in frames {
            guard case .event(let event) = try WireFrame.decode(frame) else { return XCTFail("event required") }
            _ = try event.decodedPayload()
            XCTAssertGreaterThan(event.seq, last)
            last = event.seq
            kinds.insert(event.kind)
            if event.kind == "hello" {
                guard case .HelloPayload(let hello) = try event.decodedPayload() else { return XCTFail("typed hello") }
                XCTAssertEqual(hello.schema_hash, protocolSchemaHash)
                XCTAssertEqual(event.seq, 1)
            }
            if event.kind == "message.delta" {
                guard case .MessageDeltaPayload(let delta) = try event.decodedPayload() else {
                    return XCTFail("typed message delta required")
                }
                XCTAssertFalse(delta.text.isEmpty)
            }
        }
        for kind in ["session.ready", "session.snapshot", "approval.requested", "approval.resolved", "tool.proposed", "tool.started", "tool.completed", "message.delta", "message.completed", "run.usage", "run.completed"] {
            XCTAssertTrue(kinds.contains(kind), "missing \(kind)")
        }
    }
    func testEveryCommandRejectsUnknownParameterKeys() throws {
        let cases: [(String, [String: JSONValue])] = [
            ("message.submit", ["text": .string("hello")]),
            ("run.cancel", ["run_id": .string("run")]),
            ("approval.decide", ["approval_id": .string("a"), "operation_digest": .string("d"), "allowed": .bool(true)]),
            ("snapshot", [:]),
            ("inspect", ["operation": .string("tree.snapshot"), "arguments": .object([:])]),
            ("shutdown", [:]),
        ]
        for (method, params) in cases {
            let valid = Request(protocol: 1, request_id: "key-test", method: method, params: .object(params))
            XCTAssertNoThrow(try valid.command(), "valid \(method)")
            var extra = params
            extra["unexpected_authority"] = .bool(true)
            let invalid = Request(protocol: 1, request_id: "key-test", method: method, params: .object(extra))
            XCTAssertThrowsError(try invalid.command(), "extra field in \(method)")
        }
    }
    func testRequestIdsAndTypedPayloadsFailClosed() throws {
        for id in ["", String(repeating: "x", count: 129), "line\nbreak", "café", "a/b", "a b"] {
            let request = Request(protocol: 1, request_id: id, method: "snapshot", params: .object([:]))
            XCTAssertThrowsError(try request.command())
        }
        let malformed = Event(protocol: 1, instance_id: "host", seq: 1, kind: "approval.requested", payload: .object(["approval_id": .string("a")]))
        XCTAssertThrowsError(try malformed.decodedPayload())
    }
    func testSequenceAndUnknownCostRemainLossless() throws {
        let event = Event(protocol: 1, instance_id: "host", seq: UInt64.max, kind: "snapshot", payload: .object(["cost": .null, "uncertain": .bool(true)]))
        XCTAssertEqual(try WireFrame.decode(WireFrame.event(event).encoded().dropLast()), .event(event))
    }
}
