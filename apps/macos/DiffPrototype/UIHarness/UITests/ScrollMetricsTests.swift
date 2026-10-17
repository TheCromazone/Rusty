import XCTest

final class ScrollMetricsTests: XCTestCase {
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
