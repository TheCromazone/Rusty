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
        let controller = PrototypeApplicationController()
        app.delegate = controller
        func argument(after flag: String) -> String? {
            guard let index = CommandLine.arguments.firstIndex(of: flag), CommandLine.arguments.indices.contains(index + 1) else { return nil }
            return CommandLine.arguments[index + 1]
        }
        controller.openFixture(count: CommandLine.arguments.contains("--100k") ? 100_000 : 20_000,
                               collapsed: !CommandLine.arguments.contains("--expanded"),
                               metricsPath: argument(after: "--metrics-file"),
                               warmCache: CommandLine.arguments.contains("--measure-memory"),
                               diagnosticsPath: argument(after: "--projection-diagnostics"))
        let menu = NSMenu()
        let appItem = NSMenuItem(); menu.addItem(appItem)
        let appMenu = NSMenu(); appItem.submenu = appMenu
        appMenu.addItem(withTitle: "quit prototype", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        let editItem = NSMenuItem(); menu.addItem(editItem)
        let edit = NSMenu(title: "edit"); editItem.submenu = edit
        edit.addItem(withTitle: "copy", action: #selector(DiffRowView.copy(_:)), keyEquivalent: "c")
        edit.addItem(withTitle: "select all", action: #selector(DiffRowView.selectAll(_:)), keyEquivalent: "a")
        app.mainMenu = menu
        controller.documentWindow?.present()
        app.activate(ignoringOtherApps: true)
        withExtendedLifetime(controller) { app.run() }
    }
}
