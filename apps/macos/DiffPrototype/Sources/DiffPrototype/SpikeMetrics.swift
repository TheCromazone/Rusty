import AppKit
import Darwin
import DiffPrototypeCore
import os.log

@MainActor enum SpikeMetrics {
    static let log = OSLog(subsystem: "dev.rusty.diff-prototype", category: .pointsOfInterest)
}

func physicalFootprint() -> UInt64? {
    var info = task_vm_info_data_t()
    var count = mach_msg_type_number_t(MemoryLayout<task_vm_info_data_t>.size / MemoryLayout<integer_t>.size)
    let result = withUnsafeMutablePointer(to: &info) { pointer in
        pointer.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
            task_info(mach_task_self_, task_flavor_t(TASK_VM_INFO), $0, &count)
        }
    }
    return result == KERN_SUCCESS ? info.phys_footprint : nil
}

/// The first-paint span starts when synthetic input rows are ready (standing in
/// for receipt from a worker) and ends at the first AppKit draw completion. It
/// does NOT measure a displayed/compositor-presented frame or process launch.
@MainActor final class OpenMeasurement {
    let received = ContinuousClock.now
    let signpost = OSSignpostID(log: SpikeMetrics.log)
    let baseline: UInt64?
    let fixtureMS: Double
    let rowCount: Int
    let output: String?
    let warmCache: Bool
    var completed = false
    init(baseline: UInt64?, fixtureMS: Double, rows: Int, output: String?, warmCache: Bool) {
        self.baseline = baseline; self.fixtureMS = fixtureMS; rowCount = rows
        self.output = output; self.warmCache = warmCache
        os_signpost(.begin, log: SpikeMetrics.log, name: "DocumentReceiptToFirstPaint", signpostID: signpost)
    }
    func firstPaintCompleted(view: DiffRowView) {
        guard !completed else { return }; completed = true
        let firstPaintMS = milliseconds(received)
        os_signpost(.end, log: SpikeMetrics.log, name: "DocumentReceiptToFirstPaint", signpostID: signpost)
        let afterFirstPaint = physicalFootprint()
        var report: [String: Any] = [
            "kind": "onscreen_first_draw_and_phys_footprint",
            "rows": rowCount, "synthetic_fixture_ms": fixtureMS,
            "document_receipt_to_first_appkit_draw_end_ms": firstPaintMS,
            "first_paint_cached_lines": view.cachedLineCount,
            "first_paint_view_frame": NSStringFromRect(view.frame),
            "first_paint_view_bounds": NSStringFromRect(view.bounds),
            "first_paint_visible_rect": NSStringFromRect(view.visibleRect),
            "first_paint_clip_bounds": view.enclosingScrollView.map { NSStringFromRect($0.contentView.bounds) } ?? "no scroll view",
            "first_paint_window_frame": view.window.map { NSStringFromRect($0.frame) } ?? "no window",
            "load_average": ProcessInfo.processInfo.environment["RUSTY_SPIKE_LOAD_NOTE"] ?? "see captured uptime",
            "span_definition": "OpenMeasurement.init after Fixtures.rows returns through DiffRowView.draw first completion; excludes fixture generation, process launch and compositor presentation"
        ]
        if let baseline { report["phys_footprint_before_fixture_bytes"] = baseline }
        if let afterFirstPaint {
            report["phys_footprint_after_first_paint_bytes"] = afterFirstPaint
            if let baseline { report["first_paint_phys_footprint_delta_bytes"] = Int64(afterFirstPaint) - Int64(baseline) }
        }
        // C3 worst-cache measurement is a separate run; never enable while
        // recording scrolling. It retains the actual view and 2,000 CTLines.
        if warmCache {
            view.primeLineCacheForMemoryMeasurement()
            report["warm_cached_lines"] = view.cachedLineCount
            if let after = physicalFootprint() {
                report["phys_footprint_after_warm_cache_bytes"] = after
                if let baseline {
                    let delta = Int64(after) - Int64(baseline)
                    report["warm_cache_phys_footprint_delta_bytes"] = delta
                    report["under_50_decimal_MB_target"] = delta < 50_000_000
                }
            }
            let mapperBaseline = physicalFootprint()
            let text = String(repeating: "家", count: 100_000)
            let map = DiffPrototypeCore.UTF16Map(text)
            let range = map.range(bytes: 299_997..<300_000)
            report["mapper_last_utf16_location"] = range?.location
            withExtendedLifetime((text, map)) {
                if let before = mapperBaseline, let after = physicalFootprint() {
                    report["mapper_with_100k_character_string_phys_footprint_delta_bytes"] = Int64(after) - Int64(before)
                }
            }
        }
        guard let output else { return }
        do {
            let data = try JSONSerialization.data(withJSONObject: report, options: [.sortedKeys, .prettyPrinted])
            try data.write(to: URL(fileURLWithPath: output), options: .atomic)
        } catch {
            fputs("measurement receipt write failed: \(error)\n", stderr)
        }
    }
}

/// Opt-in diagnostic snapshots, deliberately disabled during performance runs.
/// Records branches actually painted and AX children actually requested. It is
/// not proof that WindowServer presented the same frame.
@MainActor final class ProjectionDiagnostics {
    let output: URL
    private var report: [String: Any] = ["pid": ProcessInfo.processInfo.processIdentifier]
    init(path: String) { output = URL(fileURLWithPath: path) }
    func record(_ kind: String, view: DiffRowView, first: Int, end: Int) {
        let rows: [[String: Any]] = (first..<min(end, first + 128)).map { index in
            let visible = view.document.visible[index]
            return ["visual_index": index, "source_start": visible.source.lowerBound,
                    "source_end": visible.source.upperBound, "collapsed": visible.collapsed]
        }
        report[kind] = ["at": Date().timeIntervalSince1970,
                        "collapsed": view.document.isCollapsed,
                        "projection_count": view.document.visible.count,
                        "visible_rect": NSStringFromRect(view.visibleRect),
                        "rows": rows]
        do {
            let data = try JSONSerialization.data(withJSONObject: report, options: [.sortedKeys, .prettyPrinted])
            try data.write(to: output, options: .atomic)
        } catch { fputs("projection diagnostic write failed: \(error)\n", stderr) }
    }
}
