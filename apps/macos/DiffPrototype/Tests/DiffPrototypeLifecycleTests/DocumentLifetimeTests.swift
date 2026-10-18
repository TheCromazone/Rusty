import AppKit
import Darwin
import XCTest
@testable import DiffPrototype

@MainActor private final class DocumentProbes {
    weak var owner: PrototypeWindowController?
    weak var window: NSWindow?
    weak var controls: Controls?
    weak var view: DiffRowView?
    weak var status: NSTextField?

    var released: Bool { owner == nil && window == nil && controls == nil && view == nil && status == nil }
}

/// Exercises the same owner used by main, without a worker, AX-driven close,
/// application termination, or a second implementation of the view hierarchy.
@MainActor final class DocumentLifetimeTests: XCTestCase {
    private func drain(until condition: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(5)
        while !condition(), Date() < deadline {
            RunLoop.main.run(until: Date().addingTimeInterval(0.01))
        }
        return condition()
    }

    private func fdCount() throws -> Int {
        try FileManager.default.contentsOfDirectory(atPath: "/dev/fd").count
    }

    func testRepeatedWarmDocumentsReleaseWhileApplicationOwnerStaysAlive() throws {
        let app = NSApplication.shared
        app.setActivationPolicy(.prohibited)
        let application = PrototypeApplicationController()
        let previousDelegate = app.delegate
        app.delegate = application
        defer { app.delegate = previousDelegate }
        var baseline: Int?
        var observations: [[String: Any]] = []

        // Three warm-up cycles allow AppKit/font initialization before the fd
        // comparison. Twenty further cycles exercise release, not RSS timing.
        for iteration in 0..<23 {
            let probes = DocumentProbes()
            var heldAXRows: [Any] = []
            autoreleasepool {
                let owner = application.openFixture(count: 20_000)
                probes.owner = owner; probes.window = owner.window
                probes.controls = owner.controls; probes.view = owner.diffView; probes.status = owner.status
                XCTAssertNotNil(probes.window)
                XCTAssertNotNil(probes.controls)
                XCTAssertNotNil(probes.view)
                owner.diffView?.primeLineCacheForMemoryMeasurement()
                XCTAssertEqual(owner.diffView?.cachedLineCount, 2000)
                heldAXRows = owner.diffView?.accessibilityChildren() ?? []
                XCTAssertFalse(heldAXRows.isEmpty)
                owner.diffView?.toggleCollapse()
                owner.diffView?.toggleCollapse()
                if iteration.isMultiple(of: 2) {
                    owner.close()
                } else {
                    // Covers the window-delegate path used by a user close,
                    // rather than only calling our controller's override.
                    owner.window?.performClose(nil)
                }
                XCTAssertNil(application.documentWindow)
            }
            let released = drain { probes.released }
            XCTAssertTrue(released, "document graph retained after cycle \(iteration)")
            // AX children remain strongly held here: their weak parent must
            // not keep the document view alive after its owner has closed.
            XCTAssertFalse(heldAXRows.isEmpty)
            heldAXRows.removeAll()
            XCTAssertNil(application.documentWindow)
            guard released else { break }
            let count = try fdCount()
            if iteration == 2 { baseline = count }
            if iteration >= 3 {
                let expected = try XCTUnwrap(baseline)
                XCTAssertLessThanOrEqual(abs(count - expected), 2, "fd growth after cycle \(iteration)")
                var row: [String: Any] = ["iteration": iteration - 2, "pid": getpid(),
                                          "open_fds": count, "all_weak_probes_released": released]
                if let footprint = physicalFootprint() { row["diagnostic_phys_footprint_bytes"] = footprint }
                observations.append(row)
            }
        }
        withExtendedLifetime(application) {}
        XCTAssertEqual(observations.count, 20)
        let receipt: [String: Any] = [
            "kind": "synthetic_native_document_owner_lifecycle", "warmup_cycles": 3,
            "observed_cycles": observations.count, "fd_baseline": baseline.map { $0 as Any } ?? NSNull(),
            "fd_tolerance": 2, "observations": observations,
            "rss_or_performance_qualified": false,
            "scope": "Actual prototype owner/window/controls/view/status release only; no worker, event queue or supervisor exists in this target"
        ]
        let data = try JSONSerialization.data(withJSONObject: receipt, options: [.sortedKeys, .prettyPrinted])
        if let output = ProcessInfo.processInfo.environment["RUSTY_NATIVE_LIFECYCLE_RECEIPT"] {
            let url = URL(fileURLWithPath: output)
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            try data.write(to: url, options: .atomic)
        }
        print(String(decoding: data, as: UTF8.self))
    }

    func testCloseDetachesDocumentEvenWhileWindowControllerIsRetained() throws {
        _ = NSApplication.shared
        let application = PrototypeApplicationController()
        var retainedOwner: PrototypeWindowController?
        weak var view: DiffRowView?
        weak var controls: Controls?
        weak var status: NSTextField?
        weak var window: NSWindow?
        autoreleasepool {
            // Construction must share this pool with teardown. Otherwise
            // AppKit's creation autoreleases can survive in XCTest's outer
            // pool and look like retention by the closed controller.
            let owner = application.openFixture(count: 2500)
            retainedOwner = owner
            XCTAssertNil(owner.contentViewController, "synthetic owner has no separate view-controller graph")
            view = owner.diffView; controls = owner.controls; status = owner.status; window = owner.window
            owner.diffView?.primeLineCacheForMemoryMeasurement()
            XCTAssertEqual(owner.diffView?.cachedLineCount, 2000)
            owner.close()
            owner.close() // Close is idempotent, including owner notification.
        }
        let owner = try XCTUnwrap(retainedOwner)
        XCTAssertNil(application.documentWindow)
        XCTAssertNil(owner.window)
        XCTAssertNil(owner.diffView)
        XCTAssertNil(owner.controls)
        let released = drain { view == nil && controls == nil && status == nil && window == nil }
        XCTAssertTrue(released, "document graph must release while its controller remains strongly held")
        XCTAssertNil(view, "closed controller retained the document view")
        XCTAssertNil(controls, "closed controller retained controls")
        XCTAssertNil(status, "closed controller retained the status field")
        XCTAssertNil(window, "closed controller retained its window")
        withExtendedLifetime((owner, retainedOwner)) {}
    }
}
