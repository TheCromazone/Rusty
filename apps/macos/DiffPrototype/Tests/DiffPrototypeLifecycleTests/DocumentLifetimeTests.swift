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

@MainActor private final class PresentedDocumentProbes {
    weak var owner: PrototypeWindowController?
    weak var window: NSWindow?
    weak var root: NSView?
    weak var scroll: NSScrollView?
    weak var controls: Controls?
    weak var view: DiffRowView?
    weak var status: NSTextField?

    var initialized: Bool {
        owner != nil && window != nil && root != nil && scroll != nil && controls != nil && view != nil && status != nil
    }
    var released: Bool {
        owner == nil && window == nil && root == nil && scroll == nil && controls == nil && view == nil && status == nil
    }
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
            "scope": "Offscreen prototype owner/window/controls/view/status release only; no presented draw, external AX client, worker, event queue or supervisor is exercised"
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

    /// This opt-in path shows real windows and must run only in a coordinated
    /// GUI window. Default package tests never bring these windows forward.
    func testPresentedDrawnDocumentsReleaseAfterProgrammaticWindowClose() throws {
        try XCTSkipUnless(ProcessInfo.processInfo.environment["RUSTY_NATIVE_PRESENTED_LIFECYCLE"] == "1",
                          "Requires an explicitly coordinated GUI window; presents 23 native windows")
        let app = NSApplication.shared
        let previousPolicy = app.activationPolicy()
        let previousDelegate = app.delegate
        app.setActivationPolicy(.regular)
        let application = PrototypeApplicationController()
        app.delegate = application
        defer {
            application.documentWindow?.close()
            app.delegate = previousDelegate
            app.setActivationPolicy(previousPolicy)
        }
        var baseline: Int?
        var observations: [[String: Any]] = []
        for iteration in 0..<23 {
            let probes = PresentedDocumentProbes()
            var heldAXRows: [Any] = []
            var didDraw = false
            var initialGraphPresent = false
            var graphPresentBeforeClose = false
            var firstResponderMatches = false
            var windowIsKey = false
            weak var heldAXElement: NSAccessibilityElement?
            var heldAXParentCleared = false
            var paintedRows = 0
            var windowNumber = -1
            autoreleasepool {
                let owner = application.openFixture(count: 20_000)
                probes.owner = owner; probes.window = owner.window
                probes.root = owner.window?.contentView
                probes.scroll = owner.diffView?.enclosingScrollView
                probes.controls = owner.controls; probes.view = owner.diffView; probes.status = owner.status
                initialGraphPresent = probes.initialized
                XCTAssertNotNil(probes.owner)
                XCTAssertNotNil(probes.window)
                XCTAssertNotNil(probes.root)
                XCTAssertNotNil(probes.scroll)
                XCTAssertNotNil(probes.controls)
                XCTAssertNotNil(probes.view)
                XCTAssertNotNil(probes.status)
                owner.present()
                app.activate(ignoringOtherApps: true)
                didDraw = drain { owner.window?.isVisible == true && (owner.diffView?.drawnRows ?? 0) > 0 }
                XCTAssertTrue(didDraw, "presented window did not draw within the deadline, cycle \(iteration)")
                firstResponderMatches = owner.diffView != nil && owner.window?.firstResponder === owner.diffView
                XCTAssertTrue(firstResponderMatches, "drawn diff view must be the first responder, cycle \(iteration)")
                windowIsKey = owner.window?.isKeyWindow ?? false
                paintedRows = owner.diffView?.drawnRows ?? 0
                windowNumber = owner.window?.windowNumber ?? -1
                XCTAssertGreaterThan(windowNumber, 0)
                owner.diffView?.primeLineCacheForMemoryMeasurement()
                XCTAssertEqual(owner.diffView?.cachedLineCount, 2000)
                // These are real view-owned AX objects, but this direct getter
                // does NOT exercise an external AX client or VoiceOver.
                heldAXRows = owner.diffView?.accessibilityChildren() ?? []
                XCTAssertFalse(heldAXRows.isEmpty)
                heldAXElement = heldAXRows.first as? NSAccessibilityElement
                XCTAssertNotNil(heldAXElement, "the held AX rows must include a real accessibility element")
                graphPresentBeforeClose = probes.initialized
                XCTAssertTrue(graphPresentBeforeClose, "all seven weak probes must remain live immediately before close")
                XCTAssertNotNil(probes.owner)
                XCTAssertNotNil(probes.window)
                XCTAssertNotNil(probes.root)
                XCTAssertNotNil(probes.scroll)
                XCTAssertNotNil(probes.controls)
                XCTAssertNotNil(probes.view)
                XCTAssertNotNil(probes.status)
                owner.window?.performClose(nil)
                XCTAssertNil(application.documentWindow)
            }
            let released = drain { probes.released }
            XCTAssertTrue(released, "presented document graph retained after cycle \(iteration)")
            XCTAssertNil(probes.owner)
            XCTAssertNil(probes.window)
            XCTAssertNil(probes.root)
            XCTAssertNil(probes.scroll)
            XCTAssertNil(probes.controls)
            XCTAssertNil(probes.view)
            XCTAssertNil(probes.status)
            // Keep getter-produced AX elements alive while checking that their
            // nonowning parent does not keep any part of the graph alive.
            XCTAssertFalse(heldAXRows.isEmpty)
            autoreleasepool {
                XCTAssertNotNil(heldAXElement, "held AX element must survive until its holding array is cleared")
                heldAXParentCleared = heldAXElement != nil && heldAXElement?.accessibilityParent() == nil
                XCTAssertTrue(heldAXParentCleared, "held AX element must have a nil parent after graph release")
                XCTAssertNil(heldAXElement?.accessibilityParent())
            }
            heldAXRows.removeAll()
            let heldAXElementReleased = drain { heldAXElement == nil }
            XCTAssertTrue(heldAXElementReleased, "AX element retained after clearing its holding array, cycle \(iteration)")
            XCTAssertNil(heldAXElement)
            guard initialGraphPresent && graphPresentBeforeClose && didDraw && firstResponderMatches && released
                    && heldAXParentCleared && heldAXElementReleased else { break }
            let count = try fdCount()
            if iteration == 2 { baseline = count }
            if iteration >= 3 {
                let expected = try XCTUnwrap(baseline)
                XCTAssertLessThanOrEqual(abs(count - expected), 2, "fd growth after presented cycle \(iteration)")
                observations.append(["iteration": iteration - 2, "pid": getpid(), "window_number": windowNumber,
                                     "all_weak_probes_non_nil_before_present": initialGraphPresent,
                                     "all_weak_probes_non_nil_before_close": graphPresentBeforeClose,
                                     "all_weak_probes_nil_after_close": released,
                                     "diff_view_is_first_responder_after_draw": firstResponderMatches,
                                     "window_is_key_after_draw": windowIsKey,
                                     "held_ax_element_parent_nil_after_graph_release": heldAXParentCleared,
                                     "held_ax_element_nil_after_holding_array_clear": heldAXElementReleased,
                                     "drawn_rows": paintedRows, "open_fds": count,
                                     "direct_ax_getter_exercised": true, "external_ax_client_exercised": false])
            }
        }
        withExtendedLifetime(application) {}
        XCTAssertEqual(observations.count, 20)
        let receipt: [String: Any] = [
            "kind": "presented_drawn_synthetic_document_owner_lifecycle", "warmup_cycles": 3,
            "observed_cycles": observations.count, "fd_baseline": baseline.map { $0 as Any } ?? NSNull(),
            "fd_tolerance": 2, "observations": observations,
            "external_ax_client_or_voiceover_qualified": false, "rss_or_performance_qualified": false,
            "scope": "Actual presented window and AppKit draw, followed by programmatic performClose and graph release; direct AX getter only; no worker or supervisor"
        ]
        let data = try JSONSerialization.data(withJSONObject: receipt, options: [.sortedKeys, .prettyPrinted])
        if let output = ProcessInfo.processInfo.environment["RUSTY_NATIVE_PRESENTED_LIFECYCLE_RECEIPT"] {
            let url = URL(fileURLWithPath: output)
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            try data.write(to: url, options: .atomic)
        }
        print(String(decoding: data, as: UTF8.self))
    }
}
