import AppKit
import CoreText
import os.log
import DiffPrototypeCore

// Temporary spike-only palette. The real app must consume Rust-generated tokens.
@MainActor enum SpikePalette {
    static let background = NSColor(srgbRed: 0.08, green: 0.08, blue: 0.08, alpha: 1)
    static let text = NSColor(srgbRed: 0.94, green: 0.94, blue: 0.94, alpha: 1)
    static let secondary = NSColor(srgbRed: 158/255, green: 154/255, blue: 150/255, alpha: 1)
    static let add = NSColor(srgbRed: 0.10, green: 0.22, blue: 0.13, alpha: 1)
    static let remove = NSColor(srgbRed: 0.26, green: 0.11, blue: 0.10, alpha: 1)
    static let addStrong = NSColor(srgbRed: 0.14, green: 0.35, blue: 0.18, alpha: 1)
    static let removeStrong = NSColor(srgbRed: 0.42, green: 0.16, blue: 0.13, alpha: 1)
    static let selection = NSColor(srgbRed: 0.25, green: 0.30, blue: 0.39, alpha: 0.85)
}

@MainActor private final class CachedLine: NSObject {
    let line: CTLine
    init(_ text: String, font: NSFont) {
        line = CTLineCreateWithAttributedString(NSAttributedString(string: text, attributes: [
            .font: font, .foregroundColor: SpikePalette.text
        ]))
    }
}

/// One native drawing surface; it never creates one view per source row.
@MainActor final class DiffRowView: NSView {
    private(set) var document: DiffDocument
    let rowHeight: CGFloat = 22
    let gutter: CGFloat = 142
    private let font = NSFont.monospacedSystemFont(ofSize: 13, weight: .regular)
    private var cache: [Int: CachedLine] = [:]
    private var cacheRing = Array(repeating: -1, count: 2000)
    private var cacheSlot = 0
    private let numberStyle: NSParagraphStyle = {
        let style = NSMutableParagraphStyle(); style.alignment = .right; return style
    }()
    private var anchor: TextPosition?
    private var focus: TextPosition?
    private var accessibleRows: [NSAccessibilityElement] = []
    private(set) var drawnRows = 0
    var selectionChanged: ((String) -> Void)?
    var openMeasurement: OpenMeasurement?
    var projectionDiagnostics: ProjectionDiagnostics?
    override var isFlipped: Bool { true }
    override var acceptsFirstResponder: Bool { true }
    override var isOpaque: Bool { true }

    init(document: DiffDocument) {
        self.document = document
        super.init(frame: NSRect(x: 0, y: 0, width: 1600, height: CGFloat(document.visible.count) * 22))
        cache.reserveCapacity(2000)
        setAccessibilityElement(true)
        setAccessibilityIdentifier("prototype.diff")
        setAccessibilityRole(.list)
        setAccessibilityLabel("unified diff, \(document.rows.count) source rows; prototype")
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) is unavailable") }

    override func accessibilityChildren() -> [Any]? {
        let first = max(0, Int(visibleRect.minY / rowHeight))
        let end = min(document.visible.count, Int(ceil(visibleRect.maxY / rowHeight)))
        guard first < end else { accessibleRows = []; return [] }
        accessibleRows = (first..<end).map { index in
            let item = document.visible[index]
            let element = NSAccessibilityElement()
            element.setAccessibilityRole(.staticText)
            element.setAccessibilityParent(self)
            element.setAccessibilityIdentifier("prototype.row.\(item.source.lowerBound)")
            let label: String
            if item.collapsed {
                label = "\(item.source.count) unchanged lines, collapsed"
            } else {
                let row = document.rows[item.source.lowerBound]
                let number = row.newLine ?? row.oldLine ?? 0
                label = "\(row.kind.rawValue) line \(number): \(row.text.prefix(300))"
            }
            element.setAccessibilityLabel(label)
            let local = NSRect(x: 0, y: CGFloat(index) * rowHeight, width: bounds.width, height: rowHeight)
            let frame = window?.convertToScreen(convert(local, to: nil)) ?? .zero
            element.setAccessibilityFrame(frame)
            return element
        }
        projectionDiagnostics?.record("accessibility", view: self, first: first, end: end)
        return accessibleRows
    }
    func resizeDocument() {
        setFrameSize(NSSize(width: max(1600, enclosingScrollView?.contentSize.width ?? 0),
                            height: max(1, CGFloat(document.visible.count) * rowHeight)))
        needsDisplay = true
        NSAccessibility.post(element: self, notification: .layoutChanged)
    }
    func toggleCollapse() {
        let source = document.visible.indices.contains(Int(visibleRect.minY / rowHeight))
            ? document.visible[Int(visibleRect.minY / rowHeight)].source.lowerBound : 0
        document.setCollapsed(!document.isCollapsed)
        resizeDocument()
        let target = document.visible.firstIndex(where: { $0.source.contains(source) }) ?? 0
        scroll(NSPoint(x: 0, y: CGFloat(target) * rowHeight))
    }
    private func line(_ index: Int) -> CTLine {
        if let hit = cache[index] { return hit.line }
        // Huge lines are deliberately truncated in this spike. Selection/copy
        // retain the full source. The production inspector must page long lines.
        let signpost = OSSignpostID(log: SpikeMetrics.log)
        os_signpost(.begin, log: SpikeMetrics.log, name: "CTLineCreate", signpostID: signpost)
        defer { os_signpost(.end, log: SpikeMetrics.log, name: "CTLineCreate", signpostID: signpost) }
        let text = String(document.rows[index].text.prefix(4096))
        let value = CachedLine(text, font: font)
        // A strict FIFO ring keeps exactly at most 2,000 cached CTLines.
        // Dictionary lookup and eviction are O(1); no array shifts on scroll.
        let evicted = cacheRing[cacheSlot]
        if evicted >= 0 { cache.removeValue(forKey: evicted) }
        cacheRing[cacheSlot] = index; cacheSlot = (cacheSlot + 1) % cacheRing.count
        cache[index] = value
        return value.line
    }
    func primeLineCacheForMemoryMeasurement() {
        for index in 0..<min(2000, document.rows.count) { _ = line(index) }
    }
    var cachedLineCount: Int { cache.count }
    override func draw(_ dirtyRect: NSRect) {
        let signpost = OSSignpostID(log: SpikeMetrics.log)
        os_signpost(.begin, log: SpikeMetrics.log, name: "DiffDraw", signpostID: signpost)
        defer {
            os_signpost(.end, log: SpikeMetrics.log, name: "DiffDraw", signpostID: signpost)
            if drawnRows > 0 { openMeasurement?.firstPaintCompleted(view: self) }
        }
        SpikePalette.background.setFill(); dirtyRect.fill()
        guard let ctx = NSGraphicsContext.current?.cgContext else { return }
        let viewport = dirtyRect.intersection(visibleRect)
        guard !viewport.isEmpty else { return }
        let first = max(0, Int(viewport.minY / rowHeight))
        let end = min(document.visible.count, Int(ceil(viewport.maxY / rowHeight)))
        drawnRows = max(0, end - first)
        for i in first..<end {
            let visible = document.visible[i]
            let y = CGFloat(i) * rowHeight
            let rect = NSRect(x: 0, y: y, width: bounds.width, height: rowHeight)
            if visible.collapsed {
                if let anchor, let focus {
                    let low = min(anchor, focus), high = max(anchor, focus)
                    if low.row < visible.source.upperBound && high.row >= visible.source.lowerBound {
                        SpikePalette.selection.setFill(); rect.fill()
                    }
                }
                SpikePalette.secondary.setFill()
                NSRect(x: 0, y: y, width: 2, height: rowHeight).fill()
                ("▸ \(visible.source.count) unchanged lines · click to expand" as NSString).draw(
                    at: NSPoint(x: gutter, y: y + 3), withAttributes: [.font: font, .foregroundColor: SpikePalette.secondary])
                continue
            }
            let index = visible.source.lowerBound
            let row = document.rows[index]
            if row.kind != .context {
                (row.kind == .addition ? SpikePalette.add : SpikePalette.remove).setFill(); rect.fill()
            }
            let ctLine = line(index)
            for span in row.intraline {
                let length = CTLineGetStringRange(ctLine).length
                guard span.location >= 0, span.length >= 0, span.location <= length else { continue }
                let right = span.location + min(span.length, length - span.location)
                let x1 = CTLineGetOffsetForStringIndex(ctLine, span.location, nil)
                let x2 = CTLineGetOffsetForStringIndex(ctLine, right, nil)
                (row.kind == .addition ? SpikePalette.addStrong : SpikePalette.removeStrong).setFill()
                NSRect(x: gutter + x1, y: y + 2, width: max(1, x2 - x1), height: rowHeight - 4).fill()
            }
            if let anchor, let focus {
                let low = min(anchor, focus), high = max(anchor, focus)
                if row.kind != .deletion && index >= low.row && index <= high.row {
                    let left = index == low.row ? low.utf16Column : 0
                    let right = index == high.row ? high.utf16Column : (row.text as NSString).length
                    let x1 = CTLineGetOffsetForStringIndex(ctLine, min(left, CTLineGetStringRange(ctLine).length), nil)
                    let x2 = index < high.row ? bounds.width - gutter
                        : CTLineGetOffsetForStringIndex(ctLine, min(right, CTLineGetStringRange(ctLine).length), nil)
                    SpikePalette.selection.setFill()
                    NSRect(x: gutter + x1, y: y + 1, width: max(0, x2 - x1), height: rowHeight - 2).fill()
                }
            }
            let sign = row.kind == .addition ? "+" : row.kind == .deletion ? "−" : " "
            let numbers: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: SpikePalette.secondary, .paragraphStyle: numberStyle]
            ((row.oldLine.map(String.init) ?? "") as NSString).draw(in: NSRect(x: 4, y: y + 3, width: 50, height: rowHeight), withAttributes: numbers)
            ((row.newLine.map(String.init) ?? "") as NSString).draw(in: NSRect(x: 62, y: y + 3, width: 50, height: rowHeight), withAttributes: numbers)
            (sign as NSString).draw(at: NSPoint(x: 123, y: y + 3), withAttributes: [.font: font, .foregroundColor: SpikePalette.secondary])
            ctx.saveGState()
            ctx.translateBy(x: gutter, y: y + 17)
            ctx.scaleBy(x: 1, y: -1)
            ctx.textMatrix = .identity
            ctx.textPosition = .zero
            CTLineDraw(ctLine, ctx)
            ctx.restoreGState()
        }
        projectionDiagnostics?.record("draw", view: self, first: first, end: end)
    }
    private func position(_ event: NSEvent) -> TextPosition? {
        let point = convert(event.locationInWindow, from: nil)
        let index = max(0, min(document.visible.count - 1, Int(point.y / rowHeight)))
        guard document.visible.indices.contains(index) else { return nil }
        let visible = document.visible[index]
        if visible.collapsed { return document.selectionPosition(visibleIndex: index, utf16Column: 0, anchor: anchor) }
        let row = visible.source.lowerBound
        let hit = CTLineGetStringIndexForPosition(line(row), CGPoint(x: max(0, point.x - gutter), y: 0))
        return TextPosition(row: row, utf16Column: hit == kCFNotFound ? 0 : hit)
    }
    override func mouseDown(with event: NSEvent) {
        window?.makeFirstResponder(self)
        let point = convert(event.locationInWindow, from: nil)
        let i = Int(point.y / rowHeight)
        if document.visible.indices.contains(i), document.visible[i].collapsed, !event.modifierFlags.contains(.shift) {
            document.expand(at: i); resizeDocument(); return
        }
        guard let point = position(event) else { return }
        if !event.modifierFlags.contains(.shift) || anchor == nil { anchor = point }
        focus = point; needsDisplay = true
    }
    override func mouseDragged(with event: NSEvent) {
        autoscroll(with: event); focus = position(event); needsDisplay = true
    }
    @objc func copy(_ sender: Any?) { copySelection(side: .new) }
    @objc func copyPatch(_ sender: Any?) { copySelection(side: .patch) }
    private func copySelection(side: CopySide) {
        guard let anchor, let focus else { return }
        let text = document.copy(from: anchor, to: focus, side: side)
        NSPasteboard.general.clearContents(); NSPasteboard.general.setString(text, forType: .string)
        selectionChanged?("copied \(text.utf8.count) bytes")
    }
    @objc override func selectAll(_ sender: Any?) {
        guard let last = document.rows.last else { return }
        anchor = TextPosition(row: 0, utf16Column: 0)
        focus = TextPosition(row: document.rows.count - 1, utf16Column: (last.text as NSString).length)
        needsDisplay = true
    }
    override func menu(for event: NSEvent) -> NSMenu? {
        let menu = NSMenu()
        menu.addItem(withTitle: "copy new-side text", action: #selector(copy(_:)), keyEquivalent: "")
        menu.addItem(withTitle: "copy with +/- signs", action: #selector(copyPatch(_:)), keyEquivalent: "")
        return menu
    }
}
