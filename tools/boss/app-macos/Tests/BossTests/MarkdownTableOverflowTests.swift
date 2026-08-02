import AppKit
import SwiftUI
import XCTest

@testable import Boss

/// Guards the one property the wide-table work is actually about: **no cell
/// content is ever unreachable**.
///
/// The existing `MarkdownDocumentChromeTests` only cover
/// `MarkdownDocumentMeasure`, a pure string function — nothing there would
/// notice a table whose columns are painted past a scroll extent that cannot
/// reach them. These tests measure the real AppKit scroll geometry the
/// document renders into, so a regression that strands a column fails here.
///
/// Every case is hosted in a real `NSWindow`. That is not incidental: a bare
/// offscreen `NSHostingView` never gives its scroll views real scroll
/// geometry (its document view stays zero-width, measured), so the layout it
/// renders is not the layout a user sees. Screenshots taken that way are what
/// kept this symptom alive across several rounds of review — see the capture
/// limits in `tools/boss/app-macos/README.md`.
@MainActor
final class MarkdownTableOverflowTests: XCTestCase {
    /// The table from the review screenshots, reconstructed from the README
    /// "Overrides" list it was generated from: six columns, monospaced
    /// identifiers, and a prose `Notes` column — the case the operator
    /// reported as clipped.
    static let proseTable = """
    # Settings reference

    | Setting | File | Location | Kind | Default | Notes |
    | --- | --- | --- | --- | --- | --- |
    | `BOSS_SOCKET_PATH` | `tools/boss/app-macos/README.md` | Overrides section | env var | `/tmp/boss-engine.sock` | unix socket path used by the app to reach the engine |
    | `BOSS_ENGINE_AUTOSTART` | `tools/boss/app-macos/README.md` | Overrides section | env var | `1` | set 0 to disable app-managed engine launch |
    | `BOSS_ENGINE_CMD` | `tools/boss/app-macos/README.md` | Overrides section | env var | `derived` | custom command used when auto-start is enabled |
    | `BOSS_ENGINE_PID_PATH` | `tools/boss/app-macos/README.md` | Overrides section | env var | `/tmp/boss-engine.pid` | engine pid file path |
    | `BOSS_ENGINE_FORCE_RESTART` | `tools/boss/app-macos/README.md` | Overrides section | env var | `0` | set 1 to force-restart the engine on app launch |
    | `BOSS_ENGINE_STOP_ON_EXIT` | `tools/boss/app-macos/README.md` | Overrides section | env var | `0` | set 1 to stop engine when app exits |
    | `BOSS_SHOW_SYSTEM_MESSAGES` | `tools/boss/app-macos/README.md` | Overrides section | env var | `0` | set 1 to include internal system status messages |

    A trailing paragraph after the table.
    """

    /// A table with far more columns than the document column can seat. Cell
    /// padding puts a floor under every column, so no amount of wrapping gets
    /// this under the viewport width — it is the case that must stay
    /// reachable by scrolling rather than being compressed away.
    static let manyColumnTable: String = {
        let headers = (1...30).map { "col\($0)" }
        return """
            | \(headers.joined(separator: " | ")) |
            | \(headers.map { _ in "---" }.joined(separator: " | ")) |
            | \(headers.map { _ in "value" }.joined(separator: " | ")) |
            """
    }()

    /// The real `async-markdown-viewer` scene width (`BossMacApp.swift`), i.e.
    /// what a user actually sees — narrower than any harness capture.
    private static let sceneWidth: CGFloat = 760

    // MARK: - Harness

    private struct Metrics {
        var clipWidth: CGFloat
        var contentWidth: CGFloat
        /// Right edge actually reachable after scrolling as far right as the
        /// scroll view will go.
        var reachableMaxX: CGFloat
        var overhang: CGFloat { contentWidth - clipWidth }

        var debugDescription: String {
            "clip=\(clipWidth) content=\(contentWidth) "
                + "overhang=\(overhang) reachableMaxX=\(reachableMaxX)"
        }
    }

    private func settle(_ hosting: NSView) {
        for _ in 0..<12 {
            hosting.layoutSubtreeIfNeeded()
            RunLoop.current.run(until: Date().addingTimeInterval(0.05))
        }
    }

    /// Hosts a document in a real `NSWindow`, the way the app runs it.
    private func inWindow(_ source: String, width: CGFloat) -> NSView {
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: width, height: 820),
            styleMask: [.titled, .resizable], backing: .buffered, defer: false)
        let hosting = NSHostingView(rootView: chrome(source))
        hosting.frame = NSRect(x: 0, y: 0, width: width, height: 820)
        window.contentView = hosting
        window.orderFront(nil)
        settle(hosting)
        return hosting
    }

    private func chrome(_ source: String) -> MarkdownDocumentChrome {
        MarkdownDocumentChrome(title: "Settings reference", source: source, commentsEnabled: false)
    }

    private func scrollViews(in root: NSView) -> [NSScrollView] {
        var found: [NSScrollView] = []
        func walk(_ view: NSView) {
            if let scroll = view as? NSScrollView { found.append(scroll) }
            view.subviews.forEach(walk)
        }
        walk(root)
        return found
    }

    private func hasScrollViewAncestor(_ view: NSView) -> Bool {
        var ancestor = view.superview
        while let current = ancestor {
            if current is NSScrollView { return true }
            ancestor = current.superview
        }
        return false
    }

    /// The table's own horizontal scroll view: the innermost one, i.e. the
    /// scroll view nested inside the document's vertical scroll view.
    private func tableMetrics(in root: NSView, file: StaticString = #filePath, line: UInt = #line)
        throws -> Metrics
    {
        let nested = scrollViews(in: root).filter(hasScrollViewAncestor)
        let scroll = try XCTUnwrap(
            nested.last, "no nested (table) scroll view found", file: file, line: line)
        let content = try XCTUnwrap(
            scroll.documentView, "table scroll view has no document view", file: file, line: line)

        let clip = scroll.contentView.bounds.width
        let overhang = max(0, content.frame.width - clip)
        scroll.contentView.scroll(to: NSPoint(x: overhang, y: scroll.contentView.bounds.minY))
        scroll.reflectScrolledClipView(scroll.contentView)
        return Metrics(
            clipWidth: clip,
            contentWidth: content.frame.width,
            reachableMaxX: scroll.documentVisibleRect.maxX)
    }

    // MARK: - No content is ever unreachable

    /// The property that matters, stated once: whatever width the table ends
    /// up at, scrolling the container to its limit must reveal the table's
    /// right edge. A regression that paints columns past a scroll extent too
    /// short to reach them fails here.
    private func assertFullyReachable(
        _ metrics: Metrics, _ label: String, file: StaticString = #filePath, line: UInt = #line
    ) {
        XCTAssertGreaterThanOrEqual(
            metrics.reachableMaxX, metrics.contentWidth - 0.5,
            "\(label): table content is unreachable — \(metrics.debugDescription)",
            file: file, line: line)
    }

    func testProseTableIsFullyReachableAtTheRealSceneWidth() throws {
        let metrics = try tableMetrics(in: inWindow(Self.proseTable, width: Self.sceneWidth))
        assertFullyReachable(metrics, "prose table @\(Self.sceneWidth)")
    }

    /// The escape valve: a table that cannot be made to fit must overflow and
    /// stay scrollable to its right edge, rather than being painted past a
    /// scroll extent too short to reach it. This is the failure `maxWidth`
    /// would reintroduce.
    func testTableTooWideToFitStaysReachableByScrolling() throws {
        let metrics = try tableMetrics(
            in: inWindow(Self.manyColumnTable, width: Self.sceneWidth))
        XCTAssertGreaterThan(
            metrics.overhang, 0,
            "a 30-column table should overflow its viewport — \(metrics.debugDescription)")
        assertFullyReachable(metrics, "30-column table @\(Self.sceneWidth)")
    }

    // MARK: - Compressible content wraps instead of running off the viewport

    /// A prose table has no unbreakable content, so it must fit the document
    /// column: cells wrap. Before `ProposeWidthLayout`, the horizontal scroll
    /// view's `nil` width proposal meant every cell reported its full
    /// single-line width, so this table hung ~480pt past the viewport at the
    /// real scene width and a reader had to scroll sideways to finish a
    /// sentence.
    func testProseTableWrapsToFitInsteadOfOverflowing() throws {
        for width in [Self.sceneWidth, 1200] {
            let metrics = try tableMetrics(in: inWindow(Self.proseTable, width: width))
            XCTAssertEqual(
                metrics.overhang, 0, accuracy: 1,
                "prose table @\(width) should wrap to fit — \(metrics.debugDescription)")
        }
    }

    /// A narrow table must not be stretched to the document column: it keeps
    /// its natural width and the surrounding box is what grows, so its border
    /// sits on the same centered axis as the prose around it.
    func testNarrowTableKeepsItsNaturalWidth() throws {
        let narrow = """
            | A | B |
            | --- | --- |
            | one | two |
            """
        let metrics = try tableMetrics(in: inWindow(narrow, width: Self.sceneWidth))
        XCTAssertEqual(
            metrics.overhang, 0, accuracy: 1,
            "a narrow table should fit its viewport — \(metrics.debugDescription)")
        assertFullyReachable(metrics, "narrow table @\(Self.sceneWidth)")
    }
}
