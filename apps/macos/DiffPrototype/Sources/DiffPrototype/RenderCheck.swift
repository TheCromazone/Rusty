import AppKit
import DiffPrototypeCore

/// Offscreen paint test, deliberately not a claim about wheel scrolling,
/// display compositor, window screenshots, or accessibility traversal.
@MainActor func renderCheck(directory: String) throws {
    let app = NSApplication.shared
    app.setActivationPolicy(.prohibited)
    let folder = URL(fileURLWithPath: directory, isDirectory: true)
    try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
    var reports: [[String: Any]] = []
    for (count, collapsed) in [(20_000, true), (20_000, false), (100_000, false)] {
        let build = ContinuousClock.now
        let view = DiffRowView(document: DiffDocument(rows: Fixtures.rows(count: count), collapsed: collapsed))
        let buildMS = milliseconds(build)
        let viewport = NSRect(x: 0, y: 0, width: 1180, height: 704)
        guard let bitmap = view.bitmapImageRepForCachingDisplay(in: viewport) else {
            throw NSError(domain: "DiffPrototype", code: 1, userInfo: [NSLocalizedDescriptionKey: "bitmap allocation failed"])
        }
        var paints: [Double] = []
        for _ in 0..<10 {
            let begin = ContinuousClock.now
            view.cacheDisplay(in: viewport, to: bitmap)
            paints.append(milliseconds(begin))
        }
        precondition(view.drawnRows > 0 && view.drawnRows <= 33, "viewport paint visited offscreen rows")
        let file = "offscreen-\(count)-\(collapsed ? "collapsed" : "expanded").png"
        guard let png = bitmap.representation(using: .png, properties: [:]) else {
            throw NSError(domain: "DiffPrototype", code: 2, userInfo: [NSLocalizedDescriptionKey: "PNG encoding failed"])
        }
        try png.write(to: folder.appendingPathComponent(file))
        let warm = Array(paints.dropFirst()).sorted()
        reports.append(["source_rows": count, "collapsed": collapsed, "visible_projection_rows": view.document.visible.count,
                        "viewport_painted_rows": view.drawnRows, "model_and_view_init_ms": buildMS,
                        "cold_paint_ms": paints[0], "warm_paint_median_ms": warm[warm.count / 2],
                        "warm_paint_max_ms": warm.last!, "image": file])
    }
    let data = try JSONSerialization.data(withJSONObject: ["kind": "offscreen_native_paint_only", "measurements": reports,
        "not_measured": ["GUI scroll events", "Instruments hitch ratio", "compositor frames", "VoiceOver", "real worker integration"]],
        options: [.prettyPrinted, .sortedKeys])
    try data.write(to: folder.appendingPathComponent("render-check.json"))
    print(String(decoding: data, as: UTF8.self))
}
