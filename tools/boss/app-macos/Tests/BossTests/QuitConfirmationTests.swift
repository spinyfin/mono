import AppKit
import XCTest
@testable import Boss

/// Pins the Cmd-Q confirmation for both pane-hosting modes by building
/// the production `NSAlert` (not just the copy helper). The dialog must
/// still appear under tmux hosting; only the body text and destructive
/// chrome change.
@MainActor
final class QuitConfirmationTests: XCTestCase {

    // MARK: - No agents: no dialog

    func testNoAlertWhenNoAgentsAreWorking() {
        XCTAssertNil(QuitConfirmation.alert(agentCount: 0, tmuxHostingEnabled: false))
        XCTAssertNil(QuitConfirmation.alert(agentCount: 0, tmuxHostingEnabled: true))
    }

    // MARK: - Legacy hosting: existing termination warning is unchanged

    func testLegacySingularCopyIsUnchanged() {
        XCTAssertEqual(
            QuitConfirmation.informativeText(agentCount: 1, tmuxHostingEnabled: false),
            "1 agent is currently working. Quitting will terminate them and discard any unsaved progress."
        )
    }

    func testLegacyPluralCopyIsUnchanged() {
        XCTAssertEqual(
            QuitConfirmation.informativeText(agentCount: 3, tmuxHostingEnabled: false),
            "3 agents are currently working. Quitting will terminate them and discard any unsaved progress."
        )
    }

    func testLegacyAlertIsDestructiveAndKeepsCancelAsDefault() {
        let alert = unwrappedAlert(agentCount: 2, tmuxHostingEnabled: false)
        XCTAssertEqual(alert.messageText, "Quit Boss?")
        XCTAssertEqual(
            alert.informativeText,
            "2 agents are currently working. Quitting will terminate them and discard any unsaved progress."
        )
        XCTAssertEqual(alert.buttons[0].title, "Cancel")
        XCTAssertEqual(alert.buttons[1].title, "Quit Anyway")
        XCTAssertEqual(alert.buttons[0].keyEquivalent, "\r")
        XCTAssertEqual(alert.buttons[1].keyEquivalent, "")
        XCTAssertTrue(alert.buttons[1].hasDestructiveAction)
        XCTAssertEqual(alert.alertStyle, .warning)
        alert.layout()
        XCTAssertFalse(alert.informativeText.isEmpty)
    }

    // MARK: - Tmux hosting: agents survive; dialog is not dropped

    func testTmuxSingularCopySaysTheAgentKeepsRunning() {
        let text = QuitConfirmation.informativeText(agentCount: 1, tmuxHostingEnabled: true)
        XCTAssertEqual(
            text,
            "1 agent is currently working. It keeps running after you quit. Quitting does not terminate it."
        )
        assertTmuxCopyDoesNotOverpromise(text)
    }

    func testTmuxPluralCopySaysTheAgentsKeepRunning() {
        let text = QuitConfirmation.informativeText(agentCount: 3, tmuxHostingEnabled: true)
        XCTAssertEqual(
            text,
            "3 agents are currently working. They keep running after you quit. Quitting does not terminate them."
        )
        assertTmuxCopyDoesNotOverpromise(text)
    }

    func testTmuxAlertIsStillShownAndIsNotDestructive() {
        let alert = unwrappedAlert(agentCount: 2, tmuxHostingEnabled: true)
        XCTAssertEqual(alert.messageText, "Quit Boss?")
        XCTAssertEqual(
            alert.informativeText,
            "2 agents are currently working. They keep running after you quit. Quitting does not terminate them."
        )
        XCTAssertEqual(alert.buttons[0].title, "Cancel")
        XCTAssertEqual(alert.buttons[1].title, "Quit Anyway")
        XCTAssertEqual(alert.buttons[0].keyEquivalent, "\r")
        XCTAssertFalse(
            alert.buttons[1].hasDestructiveAction,
            "quit is not destructive to tmux-hosted agents, so the button must not use red chrome"
        )
        alert.layout()
        XCTAssertFalse(alert.informativeText.isEmpty)
        assertTmuxCopyDoesNotOverpromise(alert.informativeText)
    }

    // MARK: - AppDelegate wiring: hosting flag comes from the engine setting

    func testAppDelegateLegacyPathUsesTerminationWarning() {
        let (delegate, _) = makeDelegate(agentCount: 1, tmuxHostingEnabled: false)
        let alert = delegate.makeQuitConfirmationAlert()
        XCTAssertEqual(
            alert?.informativeText,
            "1 agent is currently working. Quitting will terminate them and discard any unsaved progress."
        )
        XCTAssertEqual(alert?.buttons[1].hasDestructiveAction, true)
    }

    func testAppDelegateTmuxPathUsesSurvivalCopy() {
        let (delegate, _) = makeDelegate(agentCount: 2, tmuxHostingEnabled: true)
        let alert = delegate.makeQuitConfirmationAlert()
        XCTAssertNotNil(alert, "tmux hosting must not drop the quit confirmation")
        XCTAssertEqual(
            alert?.informativeText,
            "2 agents are currently working. They keep running after you quit. Quitting does not terminate them."
        )
        XCTAssertEqual(alert?.buttons[1].hasDestructiveAction, false)
    }

    func testAppDelegateTreatsMissingSettingsAsLegacy() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.liveWorkerStates.update(states: [liveState(slotId: 1)])
        XCTAssertTrue(model.engineSettings.isEmpty)
        XCTAssertFalse(model.tmuxHostingEnabled)

        let delegate = AppDelegate()
        delegate.chatModel = model
        delegate.liveWorkerStates = model.liveWorkerStates

        XCTAssertEqual(
            delegate.makeQuitConfirmationAlert()?.informativeText,
            "1 agent is currently working. Quitting will terminate them and discard any unsaved progress."
        )
    }

    func testAppDelegateSkipsAlertWhenNoLiveWorkers() {
        let (delegate, _) = makeDelegate(agentCount: 0, tmuxHostingEnabled: true)
        XCTAssertNil(delegate.makeQuitConfirmationAlert())
    }

    // MARK: - Helpers

    private func unwrappedAlert(agentCount: Int, tmuxHostingEnabled: Bool) -> NSAlert {
        let alert = QuitConfirmation.alert(
            agentCount: agentCount,
            tmuxHostingEnabled: tmuxHostingEnabled
        )
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

    private func makeDelegate(
        agentCount: Int,
        tmuxHostingEnabled: Bool
    ) -> (AppDelegate, ChatViewModel) {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.engineSettings = [
            EngineSetting(
                key: "workers.tmux_hosting",
                description: "Host workers in tmux",
                defaultEnabled: true,
                enabled: tmuxHostingEnabled
            ),
        ]
        XCTAssertEqual(model.tmuxHostingEnabled, tmuxHostingEnabled)
        if agentCount > 0 {
            let states = (1...agentCount).map { liveState(slotId: $0) }
            model.liveWorkerStates.update(states: states)
        }
        let delegate = AppDelegate()
        delegate.chatModel = model
        delegate.liveWorkerStates = model.liveWorkerStates
        return (delegate, model)
    }

    private func liveState(slotId: Int) -> WorkerLiveState {
        WorkerLiveState(
            slotId: slotId,
            runId: "exec-\(slotId)",
            model: "claude-opus-4-7",
            shellPid: 1000 + Int32(slotId),
            lastEventAt: "2026-06-01T00:00:00Z",
            currentTool: nil,
            lastToolEndedAt: nil,
            activity: .working,
            liveStatus: "Working",
            liveStatusAt: "2026-06-01T00:00:00Z",
            recoveryStatus: nil
        )
    }
}
