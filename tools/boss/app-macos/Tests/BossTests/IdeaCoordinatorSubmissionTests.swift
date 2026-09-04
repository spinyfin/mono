import XCTest
@testable import Boss

/// `ChatViewModel.coordinatorSubmissionText(for:)` decides whether an
/// Ideas draft needs a defusing prefix before being pasted into the
/// coordinator pane, so a draft that happens to start with `/` (e.g. a
/// proposal opening with a path or a shell command example) is not
/// silently swallowed as a Claude Code slash command instead of landing
/// as prompt text.
final class IdeaCoordinatorSubmissionTests: XCTestCase {
    func testOrdinaryDraftIsUnchanged() {
        let text = "Add retry/backoff to the X client.\n\nMore detail here."
        XCTAssertEqual(ChatViewModel.coordinatorSubmissionText(for: text), text)
    }

    func testSlashFirstLineGetsALeadingSpace() {
        let text = "/etc/hosts needs a new entry for the staging host."
        XCTAssertEqual(
            ChatViewModel.coordinatorSubmissionText(for: text),
            " /etc/hosts needs a new entry for the staging host."
        )
    }

    func testSlashOnlyOnALaterLineIsUnaffected() {
        let text = "Here's the plan:\n/etc/hosts needs updating too."
        XCTAssertEqual(ChatViewModel.coordinatorSubmissionText(for: text), text)
    }

    func testEmptyDraftIsUnaffected() {
        XCTAssertEqual(ChatViewModel.coordinatorSubmissionText(for: ""), "")
    }

    func testBareSlashCommandLookalikeIsEscaped() {
        let text = "/compact"
        XCTAssertEqual(ChatViewModel.coordinatorSubmissionText(for: text), " /compact")
    }
}
