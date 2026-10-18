import Foundation

public enum FramingError: Error, Equatable {
    case tooLarge, truncated, unsupportedVersion, invalidFrame
}

/// Bounded NDJSON framing shared by local pipes and the future SSH transport.
/// A framing error is fatal: discard this decoder and close the connection.
public struct FrameDecoder: Sendable {
    private var pending = Data()
    public init() {}
    public mutating func feed(_ bytes: Data) throws -> [Data] {
        var frames: [Data] = []
        var start = bytes.startIndex
        while start < bytes.endIndex {
            let newline = bytes[start...].firstIndex(of: 10)
            let end = newline.map { bytes.index(after: $0) } ?? bytes.endIndex
            let count = bytes.distance(from: start, to: end)
            guard pending.count <= maxFrameBytes - count else { throw FramingError.tooLarge }
            pending.append(bytes[start..<end])
            if newline != nil {
                pending.removeLast()
                frames.append(pending)
                pending = Data()
            }
            start = end
        }
        return frames
    }
    public func finish() throws {
        guard pending.isEmpty else { throw FramingError.truncated }
    }
}

public enum WireFrame: Equatable, Sendable {
    case request(Request), event(Event)
    public static func decode(_ data: Data) throws -> WireFrame {
        guard data.count < maxFrameBytes else { throw FramingError.tooLarge }
        let decoder = JSONDecoder()
        if let request = try? decoder.decode(Request.self, from: data) {
            guard request.protocol == protocolVersion else { throw FramingError.unsupportedVersion }
            _ = try request.command()
            return .request(request)
        }
        do {
            let event = try decoder.decode(Event.self, from: data)
            guard event.protocol == protocolVersion else { throw FramingError.unsupportedVersion }
            return .event(event)
        } catch let error as FramingError { throw error }
        catch { throw FramingError.invalidFrame }
    }
    public func encoded() throws -> Data {
        let encoder = JSONEncoder()
        let bytes: Data
        switch self {
        case .request(let value): bytes = try encoder.encode(value)
        case .event(let value): bytes = try encoder.encode(value)
        }
        guard bytes.count < maxFrameBytes else { throw FramingError.tooLarge }
        var result = bytes
        result.append(10)
        return result
    }
}
