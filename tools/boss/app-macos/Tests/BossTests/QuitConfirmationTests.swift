import AppKit
import XCTest
@testable import Boss

/// Pins the Cmd-Q confirmation for tmux-hosted, legacy-hosted, and mixed
/// running sets by building the production `NSAlert` (not just the copy
/// helper). The dialog must still appear in every case; only the body
/// text and destructive chrome change. Hosting mode is derived from each
/// worker's own `tmuxHosted` (its actual dispatch-time hosting mode),
/// never from the current `workers.tmux_hosting` setting value — that is
/// the whole point of `HostingMakeup.classify`.
@MainActor
final class QuitConfirmationTests: XCTestCase {

    // MARK: - No agents: no dialog

    func testNoAlertWhenNoAgentsAreWorking() {
        XCTAssertNil(QuitConfirmation.alert(agentCount: 0, hostingMakeup: .allLegacy))
        XCTAssertNil(QuitConfirmation.alert(agentCount: 0, hostingMakeup: .allTmux))
    }

    // MARK: - Legacy hosting: existing termination warning is unchanged

    func testLegacySingularCopyIsUnchanged() {
        XCTAssertEqual(
            QuitConfirmation.informativeText(agentCount: 1, hostingMakeup: .allLegacy),
            "1 agent is currently working. Quitting will terminate them and discard any unsaved progress."
        )
    }

    func testLegacyPluralCopyIsUnchanged() {
        XCTAssertEqual(
            QuitConfirmation.informativeText(agentCount: 3, hostingMakeup: .allLegacy),
            "3 agents are currently working. Quitting will terminate them and discard any unsaved progress."
        )
    }

    func testLegacyAlertIsDestructiveAndKeepsCancelAsDefault() {
        let alert = unwrappedAlert(agentCount: 2, hostingMakeup: .allLegacy)
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
        let text = QuitConfirmation.informativeText(agentCount: 1, hostingMakeup: .allTmux)
        XCTAssertEqual(
            text,
            "1 agent is currently working. It keeps running after you quit. Quitting does not terminate it."
        )
        assertTmuxCopyDoesNotOverpromise(text)
    }

    func testTmuxPluralCopySaysTheAgentsKeepRunning() {
        let text = QuitConfirmation.informativeText(agentCount: 3, hostingMakeup: .allTmux)
        XCTAssertEqual(
            text,
            "3 agents are currently working. They keep running after you quit. Quitting does not terminate them."
        )
        assertTmuxCopyDoesNotOverpromise(text)
    }

    func testTmuxAlertIsStillShownAndIsNotDestructive() {
        let alert = unwrappedAlert(agentCount: 2, hostingMakeup: .allTmux)
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

    // MARK: - Mixed hosting: the dialog must name the split, not pick one

    func testMixedCopyNamesBothCounts() {
        let text = QuitConfirmation.informativeText(
            agentCount: 3,
            hostingMakeup: .mixed(tmuxCount: 1, legacyCount: 2)
        )
        XCTAssertEqual(
            text,
            "3 agents are currently working. These use different hosting modes: "
                + "1 keeps running after you quit; 2 will be terminated, discarding their unsaved progress."
        )
    }

    func testMixedCopyDoesNotClaimUniformSurvivalOrTermination() {
        let text = QuitConfirmation.informativeText(
            agentCount: 4,
            hostingMakeup: .mixed(tmuxCount: 2, legacyCount: 2)
        )
        let lowered = text.lowercased()
        XCTAssertTrue(lowered.contains("keeps running") || lowered.contains("keep running"))
        XCTAssertTrue(lowered.contains("terminated"))
    }

    func testMixedAlertIsDestructiveBecauseTheLegacyHalfIsKilled() {
        let alert = unwrappedAlert(agentCount: 2, hostingMakeup: .mixed(tmuxCount: 1, legacyCount: 1))
        XCTAssertTrue(
            alert.buttons[1].hasDestructiveAction,
            "a mixed set still terminates the legacy-hosted half, so the button must use red chrome"
        )
    }

    // MARK: - HostingMakeup.classify: derived per-worker, not from the setting

    func testClassifyAllTmuxWhenEveryActiveWorkerIsTmuxHosted() {
        XCTAssertEqual(QuitConfirmation.HostingMakeup.classify([true, true]), .allTmux)
    }

    func testClassifyAllLegacyWhenEveryActiveWorkerIsLegacyHosted() {
        XCTAssertEqual(QuitConfirmation.HostingMakeup.classify([false, false]), .allLegacy)
    }

    func testClassifyMixedWhenActiveWorkersDisagree() {
        XCTAssertEqual(
            QuitConfirmation.HostingMakeup.classify([true, false, true]),
            .mixed(tmuxCount: 2, legacyCount: 1)
        )
    }

    func testClassifyFoldsUnknownHostingModeIntoLegacy() {
        // `nil` (an older engine, or a worker kind with no local pane)
        // must not be silently dropped or read as tmux — the
        // conservative bucket is legacy, since that is the mode a wrong
        // guess costs the user unsaved work.
        XCTAssertEqual(QuitConfirmation.HostingMakeup.classify([nil, nil]), .allLegacy)
        XCTAssertEqual(
            QuitConfirmation.HostingMakeup.classify([true, nil]),
            .mixed(tmuxCount: 1, legacyCount: 1)
        )
    }

    // MARK: - AppDelegate wiring: hosting mode comes from each active worker, not the setting

    func testAppDelegateLegacyPathUsesTerminationWarning() {
        let (delegate, _) = makeDelegate(agentCount: 1, tmuxHosted: [false])
        let alert = delegate.makeQuitConfirmationAlert()
        XCTAssertEqual(
            alert?.informativeText,
            "1 agent is currently working. Quitting will terminate them and discard any unsaved progress."
        )
        XCTAssertEqual(alert?.buttons[1].hasDestructiveAction, true)
    }

    func testAppDelegateTmuxPathUsesSurvivalCopy() {
        let (delegate, _) = makeDelegate(agentCount: 2, tmuxHosted: [true, true])
        let alert = delegate.makeQuitConfirmationAlert()
        XCTAssertNotNil(alert, "tmux hosting must not drop the quit confirmation")
        XCTAssertEqual(
            alert?.informativeText,
            "2 agents are currently working. They keep running after you quit. Quitting does not terminate them."
        )
        XCTAssertEqual(alert?.buttons[1].hasDestructiveAction, false)
    }

    /// The core regression this revision fixes: a legacy-hosted worker
    /// still running while the setting has since been flipped on must
    /// not be told it will survive quit, and vice versa. Both directions
    /// are pinned because the settings-value bug was symmetric.
    func testAppDelegateReadsEachWorkersActualModeNotTheCurrentSetting() {
        // Worker was dispatched under legacy hosting; the setting has
        // since been turned on. It still gets torn down by quit.
        let (legacyStillRunning, legacyModel) = makeDelegate(agentCount: 1, tmuxHosted: [false])
        legacyModel.engineSettings = [
            EngineSetting(
                key: "workers.tmux_hosting",
                description: "Host workers in tmux",
                defaultEnabled: true,
                enabled: true
            ),
        ]
        XCTAssertTrue(legacyModel.tmuxHostingEnabled, "setting is on")
        XCTAssertEqual(
            legacyStillRunning.makeQuitConfirmationAlert()?.informativeText,
            "1 agent is currently working. Quitting will terminate them and discard any unsaved progress.",
            "the running worker was dispatched under legacy hosting and must still be warned about as such"
        )
        XCTAssertEqual(legacyStillRunning.makeQuitConfirmationAlert()?.buttons[1].hasDestructiveAction, true)

        // Worker was dispatched under tmux hosting; the setting has
        // since been turned off. It still survives quit.
        let (tmuxStillRunning, tmuxModel) = makeDelegate(agentCount: 1, tmuxHosted: [true])
        tmuxModel.engineSettings = [
            EngineSetting(
                key: "workers.tmux_hosting",
                description: "Host workers in tmux",
                defaultEnabled: true,
                enabled: false
            ),
        ]
        XCTAssertFalse(tmuxModel.tmuxHostingEnabled, "setting is off")
        XCTAssertEqual(
            tmuxStillRunning.makeQuitConfirmationAlert()?.informativeText,
            "1 agent is currently working. It keeps running after you quit. Quitting does not terminate it.",
            "the running worker was dispatched under tmux hosting and must still be described as surviving quit"
        )
        XCTAssertEqual(tmuxStillRunning.makeQuitConfirmationAlert()?.buttons[1].hasDestructiveAction, false)
    }

    func testAppDelegateMixedPathNamesTheSplit() {
        let (delegate, _) = makeDelegate(agentCount: 3, tmuxHosted: [true, false, false])
        let alert = delegate.makeQuitConfirmationAlert()
        XCTAssertEqual(
            alert?.informativeText,
            "3 agents are currently working. These use different hosting modes: "
                + "1 keeps running after you quit; 2 will be terminated, discarding their unsaved progress."
        )
        XCTAssertEqual(alert?.buttons[1].hasDestructiveAction, true)
    }

    func testAppDelegateTreatsMissingSettingsAsLegacyWhenWorkerModeIsUnknown() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.liveWorkerStates.update(states: [liveState(slotId: 1, tmuxHosted: nil)])
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
        let (delegate, _) = makeDelegate(agentCount: 0, tmuxHosted: [])
        XCTAssertNil(delegate.makeQuitConfirmationAlert())
    }

    // MARK: - Helpers

    private func unwrappedAlert(agentCount: Int, hostingMakeup: QuitConfirmation.HostingMakeup) -> NSAlert {
        let alert = QuitConfirmation.alert(
            agentCount: agentCount,
            hostingMakeup: hostingMakeup
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
        tmuxHosted: [Bool?]
    ) -> (AppDelegate, ChatViewModel) {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        if agentCount > 0 {
            XCTAssertEqual(tmuxHosted.count, agentCount, "one hosting flag per active worker")
            let states = (1...agentCount).map { liveState(slotId: $0, tmuxHosted: tmuxHosted[$0 - 1]) }
            model.liveWorkerStates.update(states: states)
        }
        let delegate = AppDelegate()
        delegate.chatModel = model
        delegate.liveWorkerStates = model.liveWorkerStates
        return (delegate, model)
    }

    private func liveState(slotId: Int, tmuxHosted: Bool?) -> WorkerLiveState {
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
            recoveryStatus: nil,
            tmuxHosted: tmuxHosted
        )
    }
}
