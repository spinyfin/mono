import AppKit
import XCTest
@testable import Boss

/// Pins the Cmd-Q confirmation for tmux-hosted workers by building the
/// production `NSAlert` (not just the copy helper). The dialog must still
/// appear; only the body text is the survival copy, and the button is not
/// destructive.
@MainActor
final class QuitConfirmationTests: XCTestCase {

    // MARK: - No agents: no dialog

    func testNoAlertWhenNoAgentsAreWorking() {
        XCTAssertNil(QuitConfirmation.alert(agentCount: 0))
    }

    // MARK: - Tmux hosting: agents survive; dialog is not dropped

    func testSingularCopySaysTheAgentKeepsRunning() {
        let text = QuitConfirmation.informativeText(agentCount: 1)
        XCTAssertEqual(
            text,
            "1 agent is currently working. It keeps running after you quit. Quitting does not terminate it."
        )
        assertTmuxCopyDoesNotOverpromise(text)
    }

    func testPluralCopySaysTheAgentsKeepRunning() {
        let text = QuitConfirmation.informativeText(agentCount: 3)
        XCTAssertEqual(
            text,
            "3 agents are currently working. They keep running after you quit. Quitting does not terminate them."
        )
        assertTmuxCopyDoesNotOverpromise(text)
    }

    func testAlertIsStillShownAndIsNotDestructive() {
        let alert = unwrappedAlert(agentCount: 2)
        XCTAssertEqual(alert.messageText, "Quit Boss?")
        XCTAssertEqual(
            alert.informativeText,
            "2 agents are currently working. They keep running after you quit. Quitting does not terminate them."
        )
        XCTAssertEqual(alert.buttons[0].title, "Cancel")
        XCTAssertEqual(alert.buttons[1].title, "Quit Anyway")
        XCTAssertEqual(alert.buttons[0].keyEquivalent, "\r")
        XCTAssertEqual(alert.buttons[1].keyEquivalent, "")
        XCTAssertFalse(
            alert.buttons[1].hasDestructiveAction,
            "quit is not destructive to tmux-hosted agents, so the button must not use red chrome"
        )
        XCTAssertEqual(alert.alertStyle, .warning)
        alert.layout()
        XCTAssertFalse(alert.informativeText.isEmpty)
        assertTmuxCopyDoesNotOverpromise(alert.informativeText)
    }

    // MARK: - AppDelegate wiring

    func testAppDelegateUsesSurvivalCopy() {
        let (delegate, _) = makeDelegate(agentCount: 2)
        let alert = delegate.makeQuitConfirmationAlert()
        XCTAssertNotNil(alert, "tmux hosting must not drop the quit confirmation")
        XCTAssertEqual(
            alert?.informativeText,
            "2 agents are currently working. They keep running after you quit. Quitting does not terminate them."
        )
        XCTAssertEqual(alert?.buttons[1].hasDestructiveAction, false)
    }

    func testAppDelegateSingularCopy() {
        let (delegate, _) = makeDelegate(agentCount: 1)
        XCTAssertEqual(
            delegate.makeQuitConfirmationAlert()?.informativeText,
            "1 agent is currently working. It keeps running after you quit. Quitting does not terminate it."
        )
    }

    func testAppDelegateSkipsAlertWhenNoLiveWorkers() {
        let (delegate, _) = makeDelegate(agentCount: 0)
        XCTAssertNil(delegate.makeQuitConfirmationAlert())
    }

    // MARK: - Helpers

    private func unwrappedAlert(agentCount: Int) -> NSAlert {
        let alert = QuitConfirmation.alert(agentCount: agentCount)
        XCTAssertNotNil(alert, "expected an alert for agentCount=\(agentCount)")
        return alert!
    }

    private func assertTmuxCopyDoesNotOverpromise(_ text: String) {
        let lowered = text.lowercased()
        XCTAssertFalse(lowered.contains("discard"), "tmux copy must not claim work is discarded, got: \(text)")
        XCTAssertFalse(lowered.contains("unsaved"), "tmux copy must not claim unsaved progress is lost, got: \(text)")
        XCTAssertFalse(lowered.contains("reattach"), "tmux copy must not promise pane re-attachment, got: \(text)")
        XCTAssertFalse(lowered.contains("relaunch"), "tmux copy must not promise panes come back on relaunch, got: \(text)")
        XCTAssertFalse(lowered.contains("come back"), "tmux copy must not promise panes come back, got: \(text)")
        XCTAssertFalse(lowered.contains("when you reopen"), "tmux copy must not promise panes come back, got: \(text)")
        XCTAssertFalse(
            lowered.contains("will terminate"),
            "tmux copy must not claim quitting terminates agents, got: \(text)"
        )
        XCTAssertTrue(lowered.contains("keep"), "tmux copy must say agents keep running, got: \(text)")
        XCTAssertTrue(lowered.contains("does not terminate"), "tmux copy must say quit is non-destructive, got: \(text)")
    }

    private func makeDelegate(agentCount: Int) -> (AppDelegate, ChatViewModel) {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        if agentCount > 0 {
            let states = (1...agentCount).map { liveState(slotId: $0) }
            model.liveWorkerStates.update(states: states)
        }
        let delegate = AppDelegate()
        delegate.chatModel = model
        delegate.liveWorkerStates = model.liveWorkerStates
        return (delegate, model)
    }

    private func liveState(slotId: Int, activity: WorkerActivity = .working) -> WorkerLiveState {
        WorkerLiveState(
            slotId: slotId,
            runId: "exec-\(slotId)",
            model: "claude-opus-4-7",
            shellPid: 1000 + Int32(slotId),
            lastEventAt: "2026-06-01T00:00:00Z",
            currentTool: nil,
            lastToolEndedAt: nil,
            activity: activity,
            liveStatus: "Working",
            liveStatusAt: "2026-06-01T00:00:00Z",
            recoveryStatus: nil,
            tmuxHosted: true
        )
    }
}
