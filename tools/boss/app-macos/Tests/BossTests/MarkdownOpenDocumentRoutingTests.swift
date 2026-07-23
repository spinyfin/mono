import AppKit
import XCTest
@testable import Boss

/// Covers the OS open-document / File ▸ Open ▸ in-app-renderer routing
/// added for `.md`/`.markdown`/`.mdown` document-type registration:
/// [[ChatViewModel.openLocalMarkdownFile(url:allowOSFallback:)]] and
/// `AppDelegate.application(_:open:)`'s pending-open buffer. Uses the same
/// `designRendererOpener` / `urlOpener` injection pattern established in
/// `ProjectDesignDocAffordanceTests`.
@MainActor
final class MarkdownOpenDocumentRoutingTests: XCTestCase {
    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }

    // MARK: - ChatViewModel.openLocalMarkdownFile

    /// With `designRendererOpener` stubbed, the renderer is invoked with a
    /// `DesignRendererContent` whose `title` is the filename sans extension
    /// and `filePath` is the URL's path — and `urlOpener` is never touched.
    func testOpenLocalMarkdownFileInvokesRendererWhenWired() {
        let model = makeModel()
        model.urlOpener = { url in
            XCTFail("urlOpener should not be invoked when the renderer is wired, got \(url)")
        }
        var rendered: [DesignRendererContent] = []
        model.designRendererOpener = { rendered.append($0) }

        let url = URL(fileURLWithPath: "/tmp/some-doc.md")
        model.openLocalMarkdownFile(url: url)

        XCTAssertEqual(rendered.count, 1)
        XCTAssertEqual(rendered.first?.title, "some-doc")
        XCTAssertEqual(rendered.first?.filePath, url.path)
    }

    /// The File ▸ Open panel path (`allowOSFallback` defaults to `true`)
    /// still falls back to `urlOpener` when the renderer isn't wired —
    /// this is the headless/test-safe legacy behavior, not the
    /// OS-open-document path.
    func testOpenLocalMarkdownFileFallsBackToURLOpenerWhenAllowed() {
        let model = makeModel()
        var opened: [URL] = []
        model.urlOpener = { opened.append($0) }

        let url = URL(fileURLWithPath: "/tmp/some-doc.md")
        model.openLocalMarkdownFile(url: url)

        XCTAssertEqual(opened, [url])
    }

    /// The OS open-document path passes `allowOSFallback: false`. When the
    /// renderer isn't wired, the open must be dropped rather than handed to
    /// `urlOpener` — falling back there would bounce a LaunchServices-
    /// delivered event straight back to the OS, which can self-dispatch to
    /// Boss now that it's a registered `.md` handler.
    func testOpenLocalMarkdownFileDropsWhenOSFallbackDisallowedAndRendererNotWired() {
        let model = makeModel()
        model.urlOpener = { url in
            XCTFail("urlOpener must not be invoked for an OS-delivered open with the renderer unwired, got \(url)")
        }

        let url = URL(fileURLWithPath: "/tmp/some-doc.md")
        model.openLocalMarkdownFile(url: url, allowOSFallback: false)
        // No assertion beyond the urlOpener trap above: the open is
        // silently (but loggedly) dropped.
    }

    // MARK: - AppDelegate.application(_:open:)

    /// Non-markdown URLs in the open-document event are filtered out; each
    /// markdown URL is forwarded to the renderer exactly once.
    func testApplicationOpenFiltersNonMarkdownAndForwardsMarkdownOnce() {
        let delegate = AppDelegate()
        let model = makeModel()
        model.urlOpener = { url in
            XCTFail("urlOpener must not be invoked, got \(url)")
        }
        var rendered: [DesignRendererContent] = []
        model.designRendererOpener = { rendered.append($0) }
        delegate.chatModel = model

        let markdownURL = URL(fileURLWithPath: "/tmp/notes.md")
        let otherURL = URL(fileURLWithPath: "/tmp/notes.txt")
        delegate.application(NSApplication.shared, open: [otherURL, markdownURL])

        XCTAssertEqual(rendered.count, 1)
        XCTAssertEqual(rendered.first?.filePath, markdownURL.path)
    }

    /// An open-document event delivered before the renderer is wired is
    /// buffered, then replayed exactly once — not duplicated — once
    /// `designRendererOpener` is wired. This is the flush-ordering fix:
    /// buffering must be gated on the renderer being wired, not merely on
    /// `chatModel` being assigned (the two are set from independent SwiftUI
    /// `.task` blocks with no ordering guarantee).
    func testBufferedOpenIsReplayedExactlyOnceAfterRendererWired() {
        let delegate = AppDelegate()
        let model = makeModel()
        model.urlOpener = { url in
            XCTFail("urlOpener must not be invoked for a buffered OS-delivered open, got \(url)")
        }

        // Simulate the outer `.task` (assigns `chatModel`) winning the race
        // against the inner `.task` (wires `designRendererOpener`).
        delegate.chatModel = model

        let markdownURL = URL(fileURLWithPath: "/tmp/notes.md")
        delegate.application(NSApplication.shared, open: [markdownURL])

        var rendered: [DesignRendererContent] = []
        // Nothing should have rendered yet — the event must still be
        // buffered because the renderer isn't wired.
        XCTAssertTrue(rendered.isEmpty)

        // Now the inner `.task` wires the renderer — this must flush the
        // buffered open exactly once.
        model.designRendererOpener = { rendered.append($0) }
        XCTAssertEqual(rendered.count, 1)
        XCTAssertEqual(rendered.first?.filePath, markdownURL.path)

        // Re-wiring (or any later didSet firing) must not replay the
        // already-flushed open a second time.
        model.designRendererOpener = { rendered.append($0) }
        XCTAssertEqual(rendered.count, 1)
    }
}
