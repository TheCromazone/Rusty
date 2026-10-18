import AppKit
import Foundation
import DiffPrototypeCore

func milliseconds(_ start: ContinuousClock.Instant) -> Double {
    let parts = start.duration(to: .now).components
    return Double(parts.seconds) * 1000 + Double(parts.attoseconds) / 1e15
}

if CommandLine.arguments.contains("--benchmark") {
    var reports: [[String: Any]] = []
    for count in [20_000, 100_000] {
        let start = ContinuousClock.now
        let rows = Fixtures.rows(count: count)
        let fixtureMS = milliseconds(start)
        let projectStart = ContinuousClock.now
        var document = DiffDocument(rows: rows)
        let projectionMS = milliseconds(projectStart)
        let collapsedCount = document.visible.count
        let expandStart = ContinuousClock.now
        document.setCollapsed(false)
        let expandMS = milliseconds(expandStart)
        let copyStart = ContinuousClock.now
        let copy = document.copy(from: TextPosition(row: 0, utf16Column: 0),
                                 to: TextPosition(row: rows.count - 1, utf16Column: (rows.last!.text as NSString).length))
        let copyMS = milliseconds(copyStart)
        reports.append(["source_rows": count, "collapsed_rows": collapsedCount,
                        "fixture_ms": fixtureMS, "projection_ms": projectionMS,
                        "expand_ms": expandMS, "new_side_copy_ms": copyMS, "copy_utf8_bytes": copy.utf8.count])
    }
    let report: [String: Any] = ["kind": "model_only_release_baseline", "measurements": reports,
        "os": ProcessInfo.processInfo.operatingSystemVersionString,
        "not_measured": ["GUI scrolling", "hitch ratio", "window launch", "VoiceOver"]]
    let data = try JSONSerialization.data(withJSONObject: report, options: [.prettyPrinted, .sortedKeys])
    print(String(decoding: data, as: UTF8.self))
} else if let flag = CommandLine.arguments.firstIndex(of: "--render-check"), CommandLine.arguments.indices.contains(flag + 1) {
    try MainActor.assumeIsolated { try renderCheck(directory: CommandLine.arguments[flag + 1]) }
} else {
    MainActor.assumeIsolated {
        let app = NSApplication.shared
        app.setActivationPolicy(.regular)
        let count = CommandLine.arguments.contains("--100k") ? 100_000 : 20_000
        let baseline = physicalFootprint()
        let start = ContinuousClock.now
        let rows = Fixtures.rows(count: count)
        let fixtureMS = milliseconds(start)
        let outputFlag = CommandLine.arguments.firstIndex(of: "--metrics-file")
        let output = outputFlag.flatMap { CommandLine.arguments.indices.contains($0 + 1) ? CommandLine.arguments[$0 + 1] : nil }
        let measurement = OpenMeasurement(baseline: baseline, fixtureMS: fixtureMS, rows: count, output: output,
                                          warmCache: CommandLine.arguments.contains("--measure-memory"))
        let document = DiffDocument(rows: rows, collapsed: !CommandLine.arguments.contains("--expanded"))
        let view = DiffRowView(document: document)
        view.openMeasurement = measurement
        if let flag = CommandLine.arguments.firstIndex(of: "--projection-diagnostics"), CommandLine.arguments.indices.contains(flag + 1) {
            view.projectionDiagnostics = ProjectionDiagnostics(path: CommandLine.arguments[flag + 1])
        }
        let window = NSWindow(contentRect: NSRect(x: 120, y: 140, width: 1180, height: 760),
                              styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
        window.title = "rusty · native diff prototype · \(count) rows"
        window.setAccessibilityIdentifier("prototype.window")
        let root = NSView(frame: window.contentView!.bounds)
        root.autoresizingMask = [.width, .height]
        let scroll = NSScrollView(frame: NSRect(x: 0, y: 38, width: root.bounds.width, height: root.bounds.height - 38))
        scroll.autoresizingMask = [.width, .height]
        scroll.hasVerticalScroller = true; scroll.hasHorizontalScroller = true
        scroll.documentView = view; scroll.drawsBackground = true; scroll.backgroundColor = SpikePalette.background
        let toggle = NSButton(title: "expand / collapse context", target: view, action: #selector(Controls.toggle(_:)))
        let controls = Controls(view)
        toggle.target = controls
        toggle.frame = NSRect(x: 12, y: 5, width: 210, height: 28)
        toggle.setAccessibilityIdentifier("prototype.toggleContext")
        let status = NSTextField(labelWithString: "\(count) rows · synthetic fixture · ⌘C copies new side")
        status.frame = NSRect(x: 236, y: 11, width: 900, height: 20)
        status.autoresizingMask = [.width]
        view.selectionChanged = { status.stringValue = $0 }
        root.addSubview(scroll); root.addSubview(toggle); root.addSubview(status)
        window.contentView = root
        let menu = NSMenu()
        let appItem = NSMenuItem(); menu.addItem(appItem)
        let appMenu = NSMenu(); appItem.submenu = appMenu
        appMenu.addItem(withTitle: "quit prototype", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        let editItem = NSMenuItem(); menu.addItem(editItem)
        let edit = NSMenu(title: "edit"); editItem.submenu = edit
        edit.addItem(withTitle: "copy", action: #selector(DiffRowView.copy(_:)), keyEquivalent: "c")
        edit.addItem(withTitle: "select all", action: #selector(DiffRowView.selectAll(_:)), keyEquivalent: "a")
        app.mainMenu = menu
        window.makeKeyAndOrderFront(nil)
        window.makeFirstResponder(view)
        app.activate(ignoringOtherApps: true)
        withExtendedLifetime((window, controls)) { app.run() }
    }
}

@MainActor final class Controls: NSObject {
    let view: DiffRowView
    init(_ view: DiffRowView) { self.view = view }
    @objc func toggle(_ sender: Any?) { view.toggleCollapse() }
}
