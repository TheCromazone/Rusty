import AppKit
import DiffPrototypeCore

/// Owns only the standalone synthetic inspector. There is no worker or session
/// supervisor here. Closing its window drops the document graph independently
/// of the application run loop.
@MainActor final class PrototypeWindowController: NSWindowController, NSWindowDelegate {
    private(set) var diffView: DiffRowView?
    private(set) var controls: Controls?
    private(set) var status: NSTextField?
    private var scrollView: NSScrollView?
    var onClose: ((PrototypeWindowController) -> Void)?
    private var detached = false

    init(rows: [DiffRow], collapsed: Bool, measurement: OpenMeasurement?, diagnosticsPath: String?) {
        let window = NSWindow(contentRect: NSRect(x: 120, y: 140, width: 1180, height: 760),
                              styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
        // ARC and this controller own the window; legacy close-time release
        // must not compete with that ownership.
        window.isReleasedWhenClosed = false
        super.init(window: window)
        window.delegate = self
        window.title = "rusty · native diff prototype · \(rows.count) rows"
        window.setAccessibilityIdentifier("prototype.window")

        let view = DiffRowView(document: DiffDocument(rows: rows, collapsed: collapsed))
        view.openMeasurement = measurement
        if let diagnosticsPath { view.projectionDiagnostics = ProjectionDiagnostics(path: diagnosticsPath) }
        let root = NSView(frame: window.contentView!.bounds)
        root.autoresizingMask = [.width, .height]
        let scroll = NSScrollView(frame: NSRect(x: 0, y: 38, width: root.bounds.width, height: root.bounds.height - 38))
        scroll.autoresizingMask = [.width, .height]
        scroll.hasVerticalScroller = true; scroll.hasHorizontalScroller = true
        scroll.documentView = view; scroll.drawsBackground = true; scroll.backgroundColor = SpikePalette.background
        let controls = Controls(view)
        let toggle = NSButton(title: "expand / collapse context", target: controls, action: #selector(Controls.toggle(_:)))
        toggle.frame = NSRect(x: 12, y: 5, width: 210, height: 28)
        toggle.setAccessibilityIdentifier("prototype.toggleContext")
        let status = NSTextField(labelWithString: "\(rows.count) rows · synthetic fixture · ⌘C copies new side")
        status.frame = NSRect(x: 236, y: 11, width: 900, height: 20)
        status.autoresizingMask = [.width]
        view.selectionChanged = { [weak status] in status?.stringValue = $0 }
        root.addSubview(scroll); root.addSubview(toggle); root.addSubview(status)
        window.contentView = root
        diffView = view; self.controls = controls; self.status = status; scrollView = scroll
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is unavailable") }

    func present() {
        showWindow(nil)
        window?.makeFirstResponder(diffView)
    }

    override func close() {
        guard !detached else { return }
        super.close()
        detachDocument()
    }

    func windowWillClose(_ notification: Notification) {
        guard let closing = notification.object as? NSWindow, closing === window else { return }
        detachDocument()
    }

    private func detachDocument() {
        guard !detached else { return }
        detached = true
        let completed = onClose
        onClose = nil
        diffView?.selectionChanged = nil
        diffView?.openMeasurement = nil
        diffView?.projectionDiagnostics = nil
        scrollView?.documentView = nil
        window?.delegate = nil
        window?.contentView = nil
        window = nil
        controls = nil; status = nil; diffView = nil; scrollView = nil
        completed?(self)
    }
}

/// Lives for the application loop, but holds no closed document. This is a
/// lifecycle owner for the spike, not the future coding-session application.
@MainActor final class PrototypeApplicationController: NSObject, NSApplicationDelegate {
    private(set) var documentWindow: PrototypeWindowController?

    @discardableResult
    func openFixture(count: Int, collapsed: Bool = true, metricsPath: String? = nil,
                     warmCache: Bool = false, diagnosticsPath: String? = nil) -> PrototypeWindowController {
        documentWindow?.close()
        let baseline = physicalFootprint()
        let start = ContinuousClock.now
        let rows = Fixtures.rows(count: count)
        let measurement = OpenMeasurement(baseline: baseline, fixtureMS: milliseconds(start), rows: count,
                                          output: metricsPath, warmCache: warmCache)
        let owner = PrototypeWindowController(rows: rows, collapsed: collapsed,
                                              measurement: measurement, diagnosticsPath: diagnosticsPath)
        owner.onClose = { [weak self] closed in
            guard self?.documentWindow === closed else { return }
            self?.documentWindow = nil
        }
        documentWindow = owner
        return owner
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }
}

@MainActor final class Controls: NSObject {
    let view: DiffRowView
    init(_ view: DiffRowView) { self.view = view }
    @objc func toggle(_ sender: Any?) { view.toggleCollapse() }
}
