import Foundation

public enum RowKind: String, Sendable { case context, addition, deletion }

public struct DiffRow: Sendable {
    public let kind: RowKind
    public let oldLine: Int?
    public let newLine: Int?
    public let text: String
    public let intraline: [NSRange]
    public init(kind: RowKind, oldLine: Int?, newLine: Int?, text: String, intraline: [NSRange] = []) {
        self.kind = kind; self.oldLine = oldLine; self.newLine = newLine
        self.text = text; self.intraline = intraline
    }
}

/// Rust sends UTF-8 byte ranges. NSString/CoreText use UTF-16 offsets.
/// This linear map accepts scalar boundaries, including boundaries inside a
/// composed grapheme, and rejects ranges that split UTF-8 sequences.
public struct UTF16Map: Sendable {
    private let text: String
    public let byteCount: Int
    public init(_ text: String) { self.text = text; byteCount = text.utf8.count }
    public func range(bytes: Range<Int>) -> NSRange? {
        guard bytes.lowerBound >= 0, bytes.upperBound <= byteCount else { return nil }
        var utf8 = 0; var utf16 = 0
        var start: Int? = bytes.lowerBound == 0 ? 0 : nil
        if bytes.upperBound == 0 { return NSRange(location: 0, length: 0) }
        for scalar in text.unicodeScalars {
            utf8 += scalar.utf8.count; utf16 += scalar.utf16.count
            if utf8 == bytes.lowerBound { start = utf16 }
            if utf8 == bytes.upperBound {
                return start.map { NSRange(location: $0, length: utf16 - $0) }
            }
            if utf8 > bytes.upperBound { return nil }
        }
        return nil
    }
}

public struct VisibleRow: Equatable, Sendable {
    public let source: Range<Int>
    public let collapsed: Bool
}

public struct TextPosition: Equatable, Comparable, Sendable {
    public let row: Int
    public let utf16Column: Int
    public init(row: Int, utf16Column: Int) { self.row = row; self.utf16Column = utf16Column }
    public static func < (lhs: Self, rhs: Self) -> Bool {
        lhs.row == rhs.row ? lhs.utf16Column < rhs.utf16Column : lhs.row < rhs.row
    }
}

public enum CopySide: Sendable { case new, old, patch }

/// O(N) initial projection and collapse rebuild; draw and selection hit-tests
/// index this array directly. No per-row AppKit views or layout objects.
public struct DiffDocument: Sendable {
    public let rows: [DiffRow]
    public private(set) var visible: [VisibleRow] = []
    public private(set) var isCollapsed = true
    private var expandedStarts: Set<Int> = []
    public init(rows: [DiffRow], collapsed: Bool = true) {
        self.rows = rows; isCollapsed = collapsed; rebuild()
    }
    public mutating func setCollapsed(_ value: Bool) {
        isCollapsed = value; expandedStarts.removeAll(keepingCapacity: true); rebuild()
    }
    public mutating func expand(at index: Int) {
        guard visible.indices.contains(index), visible[index].collapsed else { return }
        expandedStarts.insert(visible[index].source.lowerBound); rebuild()
    }
    private mutating func rebuild() {
        visible.removeAll(keepingCapacity: true)
        visible.reserveCapacity(rows.count)
        var i = 0
        while i < rows.count {
            if isCollapsed && rows[i].kind == .context {
                let start = i
                while i < rows.count && rows[i].kind == .context { i += 1 }
                let end = i
                if end - start > 8 {
                    let hidden = (start + 3)..<(end - 3)
                    for j in start..<(start + 3) { visible.append(VisibleRow(source: j..<(j + 1), collapsed: false)) }
                    if expandedStarts.contains(hidden.lowerBound) {
                        for j in hidden { visible.append(VisibleRow(source: j..<(j + 1), collapsed: false)) }
                    } else { visible.append(VisibleRow(source: hidden, collapsed: true)) }
                    for j in (end - 3)..<end { visible.append(VisibleRow(source: j..<(j + 1), collapsed: false)) }
                } else {
                    for j in start..<end { visible.append(VisibleRow(source: j..<(j + 1), collapsed: false)) }
                }
            } else {
                visible.append(VisibleRow(source: i..<(i + 1), collapsed: false)); i += 1
            }
        }
    }
    /// A focus moving down into folded context includes the full hidden block;
    /// moving up includes it from its beginning. Source coordinates stay stable.
    public func selectionPosition(visibleIndex: Int, utf16Column: Int, anchor: TextPosition?) -> TextPosition? {
        guard visible.indices.contains(visibleIndex) else { return nil }
        let item = visible[visibleIndex]
        if item.collapsed, let anchor, anchor.row < item.source.lowerBound {
            let last = item.source.upperBound - 1
            return TextPosition(row: last, utf16Column: (rows[last].text as NSString).length)
        }
        return TextPosition(row: item.source.lowerBound, utf16Column: item.collapsed ? 0 : utf16Column)
    }
    /// New-side copy omits deleted rows; signed copy includes +/- prefixes.
    /// Selection across a collapsed block includes its hidden source rows.
    public func copy(from anchor: TextPosition, to focus: TextPosition, side: CopySide = .new) -> String {
        let first = min(anchor, focus), last = max(anchor, focus)
        guard rows.indices.contains(first.row), rows.indices.contains(last.row) else { return "" }
        var result: [String] = []
        for i in first.row...last.row {
            let row = rows[i]
            if side == .new && row.kind == .deletion { continue }
            if side == .old && row.kind == .addition { continue }
            let value = row.text as NSString
            let low = i == first.row ? max(0, min(first.utf16Column, value.length)) : 0
            let high = i == last.row ? max(low, min(last.utf16Column, value.length)) : value.length
            // Expand partial UTF-16 selections to valid composed sequences.
            let selected = NSRange(location: low, length: high - low)
            let safe = selected.length == 0 ? selected : value.rangeOfComposedCharacterSequences(for: selected)
            let prefix = side == .patch ? (row.kind == .addition ? "+" : row.kind == .deletion ? "-" : " ") : ""
            result.append(prefix + value.substring(with: safe))
        }
        return result.joined(separator: "\n")
    }
}

public enum Fixtures {
    public static func rows(count: Int) -> [DiffRow] {
        var rows: [DiffRow] = []; rows.reserveCapacity(max(0, count))
        var oldLine = 1; var newLine = 1
        for i in 0..<max(0, count) {
            let kind: RowKind = i % 200 == 20 ? .deletion : i % 200 == 21 ? .addition : .context
            let suffix = kind == .deletion ? "previous" : "updated"
            let text = "let item_\(i) = \"café 家 👩🏽‍💻 \(suffix)\""
            let span = (text as NSString).range(of: suffix)
            rows.append(DiffRow(kind: kind, oldLine: kind == .addition ? nil : oldLine,
                                newLine: kind == .deletion ? nil : newLine, text: text,
                                intraline: kind == .context ? [] : [span]))
            if kind != .addition { oldLine += 1 }; if kind != .deletion { newLine += 1 }
        }
        return rows
    }
}
