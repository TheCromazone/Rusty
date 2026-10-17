import XCTest
@testable import DiffPrototypeCore

final class DiffModelTests: XCTestCase {
    private func context(_ text: String, _ n: Int = 1) -> DiffRow {
        DiffRow(kind: .context, oldLine: n, newLine: n, text: text)
    }
    func testUTF8RangesMapEmojiCombiningCJKAndCRLF() {
        for text in ["a😀b", "e\u{301}家", "👩🏽‍💻\t家\r\n", "\u{FEFF}no final newline", ""] {
            let mapping = UTF16Map(text)
            var bytes = 0; var units = 0
            XCTAssertEqual(mapping.range(bytes: 0..<0), NSRange(location: 0, length: 0))
            for scalar in text.unicodeScalars {
                let end = bytes + scalar.utf8.count
                XCTAssertEqual(mapping.range(bytes: bytes..<end), NSRange(location: units, length: scalar.utf16.count))
                if scalar.utf8.count > 1 { XCTAssertNil(mapping.range(bytes: (bytes + 1)..<end)) }
                bytes = end; units += scalar.utf16.count
            }
            XCTAssertEqual(mapping.range(bytes: 0..<bytes), NSRange(location: 0, length: units))
            XCTAssertNil(mapping.range(bytes: bytes..<(bytes + 1)))
        }
    }
    func testLossyUTF8InputHasExplicitReplacementBeforeMapping() {
        let text = String(decoding: [0x66, 0x80, 0xff, 0x67], as: UTF8.self)
        XCTAssertEqual(text, "f��g")
        XCTAssertEqual(UTF16Map(text).range(bytes: 1..<7), NSRange(location: 1, length: 2))
        // Original invalid-byte offsets must never be applied after lossy decode.
        XCTAssertNil(UTF16Map(text).range(bytes: 1..<2))
    }
    func testHundredThousandCharacterLineMapping() {
        let text = String(repeating: "家", count: 100_000)
        let map = UTF16Map(text)
        XCTAssertEqual(map.range(bytes: 299_997..<300_000), NSRange(location: 99_999, length: 1))
    }
    func testCollapseExpandPreservesEveryOriginalRowOnce() {
        for count in [0, 1, 8, 9, 20_000, 100_000] {
            var doc = DiffDocument(rows: Fixtures.rows(count: count))
            XCTAssertEqual(doc.visible.flatMap { Array($0.source) }, Array(0..<count))
            if let marker = doc.visible.firstIndex(where: \.collapsed) {
                let block = doc.visible[marker].source
                doc.expand(at: marker)
                XCTAssertFalse(doc.visible.contains { $0.collapsed && $0.source == block })
                XCTAssertEqual(doc.visible.flatMap { Array($0.source) }, Array(0..<count))
            }
            doc.setCollapsed(false)
            XCTAssertEqual(doc.visible.count, count)
            XCTAssertTrue(doc.visible.allSatisfy { !$0.collapsed && $0.source.count == 1 })
            doc.setCollapsed(true)
            XCTAssertEqual(doc.visible.flatMap { Array($0.source) }, Array(0..<count))
        }
    }
    func testCopySidesAndPatchAcrossCollapsedContext() {
        var rows = (1...20).map { context("line \($0)", $0) }
        rows.insert(DiffRow(kind: .deletion, oldLine: 11, newLine: nil, text: "old"), at: 10)
        rows.insert(DiffRow(kind: .addition, oldLine: nil, newLine: 11, text: "new"), at: 11)
        let doc = DiffDocument(rows: rows)
        let a = TextPosition(row: 0, utf16Column: 0)
        let b = TextPosition(row: rows.count - 1, utf16Column: 7)
        XCTAssertTrue(doc.visible.contains(where: \.collapsed))
        let new = doc.copy(from: a, to: b)
        XCTAssertTrue(new.contains("line 5\nline 6"))
        XCTAssertTrue(new.contains("\nnew\n")); XCTAssertFalse(new.contains("\nold\n"))
        XCTAssertEqual(new, doc.copy(from: b, to: a))
        XCTAssertTrue(doc.copy(from: a, to: b, side: .old).contains("\nold\n"))
        XCTAssertTrue(doc.copy(from: a, to: b, side: .patch).contains("\n-old\n+new\n"))
    }
    func testCopyPartialGraphemeExpandsWithoutBreakingUnicode() {
        let doc = DiffDocument(rows: [context("a👩🏽‍💻b"), context("e\u{301}家")])
        XCTAssertEqual(doc.copy(from: .init(row: 0, utf16Column: 2), to: .init(row: 0, utf16Column: 3)), "👩🏽‍💻")
        XCTAssertEqual(doc.copy(from: .init(row: 1, utf16Column: 1), to: .init(row: 1, utf16Column: 2)), "e\u{301}")
        XCTAssertEqual(doc.copy(from: .init(row: 0, utf16Column: 2), to: .init(row: 0, utf16Column: 2)), "")
    }
    func testCopyClampsColumnsAndRejectsInvalidRows() {
        let doc = DiffDocument(rows: [context("abc")])
        XCTAssertEqual(doc.copy(from: .init(row: 0, utf16Column: -10), to: .init(row: 0, utf16Column: 99)), "abc")
        XCTAssertEqual(doc.copy(from: .init(row: -1, utf16Column: 0), to: .init(row: 0, utf16Column: 1)), "")
    }
    func testFixtureLineNumbersAreContinuousOnEachSide() {
        let rows = Fixtures.rows(count: 20_000)
        let old = rows.compactMap(\.oldLine), new = rows.compactMap(\.newLine)
        XCTAssertEqual(old, Array(1...old.count)); XCTAssertEqual(new, Array(1...new.count))
    }
    func testFoldMarkerSelectionIncludesHiddenRowsInBothDirections() {
        let rows = (0..<20).map { context("line \($0)", $0 + 1) }
        let doc = DiffDocument(rows: rows)
        let marker = doc.visible.firstIndex(where: \.collapsed)!
        let downAnchor = TextPosition(row: 0, utf16Column: 0)
        let down = doc.selectionPosition(visibleIndex: marker, utf16Column: 0, anchor: downAnchor)!
        XCTAssertEqual(down.row, 16)
        XCTAssertTrue(doc.copy(from: downAnchor, to: down).hasSuffix("line 16"))
        let upAnchor = TextPosition(row: 19, utf16Column: 7)
        let up = doc.selectionPosition(visibleIndex: marker, utf16Column: 0, anchor: upAnchor)!
        XCTAssertEqual(up.row, 3)
        XCTAssertTrue(doc.copy(from: upAnchor, to: up).hasPrefix("line 3\nline 4"))
        XCTAssertNil(doc.selectionPosition(visibleIndex: -1, utf16Column: 0, anchor: nil))
    }
    func testProjectionNeverMutatesLineNumbers() {
        let rows = Fixtures.rows(count: 20_000)
        var doc = DiffDocument(rows: rows)
        doc.setCollapsed(false); doc.setCollapsed(true)
        XCTAssertEqual(doc.rows.map(\.oldLine), rows.map(\.oldLine))
        XCTAssertEqual(doc.rows.map(\.newLine), rows.map(\.newLine))
    }
}
