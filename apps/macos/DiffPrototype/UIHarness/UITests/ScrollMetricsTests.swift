import XCTest

final class ScrollMetricsTests: XCTestCase {
    /// Read actual AppKit drawing geometry on a fresh launch. A scroll bar's
    /// accessibility value alone does not prove the code gutter is onscreen.
    @MainActor func testInitialViewportStartsAtFirstColumn() throws {
        let receipt = FileManager.default.temporaryDirectory
            .appendingPathComponent("rusty-initial-viewport-\(UUID().uuidString).json")
        defer { try? FileManager.default.removeItem(at: receipt) }
        let app = XCUIApplication()
        app.launchArguments = ["--metrics-file", receipt.path]
        app.launch()
        defer { app.terminate() }
        XCTAssertTrue(app.scrollViews.firstMatch.waitForExistence(timeout: 15))
        let deadline = Date().addingTimeInterval(15)
        while !FileManager.default.fileExists(atPath: receipt.path), Date() < deadline {
            Thread.sleep(forTimeInterval: 0.05)
        }
        let report = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: receipt)) as? [String: Any])
        let visible = NSRectFromString(try XCTUnwrap(report["first_paint_visible_rect"] as? String))
        let clip = NSRectFromString(try XCTUnwrap(report["first_paint_clip_bounds"] as? String))
        XCTAssertEqual(visible.minX, 0, accuracy: 0.01)
        XCTAssertEqual(visible.minY, 0, accuracy: 0.01)
        XCTAssertEqual(clip.minX, 0, accuracy: 0.01)
        XCTAssertGreaterThan(visible.width, 142, "Initial viewport must include the code gutter and source")
    }

    @MainActor func testPaintedProjectionAndAccessibilityAgreeThroughFolding() throws {
        let receipt = FileManager.default.temporaryDirectory
            .appendingPathComponent("rusty-projection-\(UUID().uuidString).json")
        defer { try? FileManager.default.removeItem(at: receipt) }
        let app = XCUIApplication()
        app.launchArguments = ["--projection-diagnostics", receipt.path]
        app.launch()
        defer { app.terminate() }
        XCTAssertTrue(app.scrollViews.firstMatch.waitForExistence(timeout: 15))
        func verify(_ name: String, collapsed: Bool, firstFold: Bool, sourceAfter: Int) throws {
            let marker = app.staticTexts["prototype.row.3"]
            XCTAssertTrue(marker.waitForExistence(timeout: 10))
            XCTAssertEqual(marker.label.contains("14 unchanged lines, collapsed"), firstFold)
            let deadline = Date().addingTimeInterval(10)
            var report: [String: Any] = [:]
            repeat {
                if let data = try? Data(contentsOf: receipt), let value = try? JSONSerialization.jsonObject(with: data) as? [String: Any] { report = value }
                if let draw = report["draw"] as? [String: Any], let ax = report["accessibility"] as? [String: Any],
                   draw["collapsed"] as? Bool == collapsed, ax["collapsed"] as? Bool == collapsed,
                   let rows = draw["rows"] as? [[String: Any]], rows.count > 4,
                   rows[3]["collapsed"] as? Bool == firstFold { break }
                Thread.sleep(forTimeInterval: 0.05)
            } while Date() < deadline
            for kind in ["draw", "accessibility"] {
                let state = try XCTUnwrap(report[kind] as? [String: Any])
                XCTAssertEqual(state["collapsed"] as? Bool, collapsed, "\(name) \(kind)")
                let rows = try XCTUnwrap(state["rows"] as? [[String: Any]])
                XCTAssertGreaterThan(rows.count, 4)
                guard rows.count > 4 else { continue }
                XCTAssertEqual(rows[3]["source_start"] as? Int, 3)
                XCTAssertEqual(rows[3]["collapsed"] as? Bool, firstFold)
                XCTAssertEqual(rows[4]["source_start"] as? Int, sourceAfter)
            }
            let snapshot = XCTAttachment(screenshot: app.screenshot())
            snapshot.name = name; snapshot.lifetime = .keepAlways; add(snapshot)
            let diagnostic = XCTAttachment(data: try JSONSerialization.data(withJSONObject: report, options: [.sortedKeys, .prettyPrinted]), uniformTypeIdentifier: "public.json")
            diagnostic.name = "\(name)-projection"; diagnostic.lifetime = .keepAlways; add(diagnostic)
        }
        try verify("fresh-collapsed", collapsed: true, firstFold: true, sourceAfter: 17)
        app.buttons["prototype.toggleContext"].click()
        try verify("expanded", collapsed: false, firstFold: false, sourceAfter: 4)
        app.buttons["prototype.toggleContext"].click()
        try verify("collapsed-again", collapsed: true, firstFold: true, sourceAfter: 17)
        app.scrollViews.firstMatch.coordinate(withNormalizedOffset: .zero)
            .withOffset(CGVector(dx: 200, dy: 77)).click()
        try verify("single-fold-expanded", collapsed: true, firstFold: false, sourceAfter: 4)
    }

    @MainActor private func scrollFixture(arguments: [String]) {
        let app = XCUIApplication()
        app.launchArguments = arguments
        app.launch()
        let diff = app.scrollViews.firstMatch
        XCTAssertTrue(diff.waitForExistence(timeout: 15))
        let options = XCTMeasureOptions()
        options.iterationCount = 10
        options.invocationOptions = [.manuallyStop]
        measure(metrics: [XCTOSSignpostMetric.scrollingAndDecelerationMetric], options: options) {
            diff.swipeUp(velocity: .fast)
            stopMeasuring()
            diff.swipeDown(velocity: .fast)
        }
        app.terminate()
    }
    @MainActor func testScroll20kCollapsed() { scrollFixture(arguments: []) }
    @MainActor func testScroll20kExpanded() { scrollFixture(arguments: ["--expanded"]) }
    @MainActor func testScroll100kExpanded() { scrollFixture(arguments: ["--100k", "--expanded"]) }
}
