import AppKit
import XCTest

@testable import Boss

/// Pins the quit-confirmation alert against both hosting modes by
/// constructing the real `NSAlert` the app would present. The factory
/// and the AppDelegate wiring are both exercised so a copy-only helper
/// that never reaches `applicationShouldTerminate` cannot go green.
@MainActor
final class QuitConfirmationTests: XCTestCase {

    // MARK: - Legacy hosting (workers.tmux_hosting off)

    func testLegacyAlertKeepsExistingTerminationWarningForOneAgent() {
        let alert = QuitConfirmation.makeAlert(activeAgentCount: 1, tmuxHostingEnabled: false)
        XCTAssertEqual(alert.messageText, "Quit Boss?")
        XCTAssertEqual(
            alert.informativeText,
            "1 agent is currently working. Quitting will terminate them and discard any unsaved progress."
        )
        assertSharedChrome(alert, destructiveQuit: true, style: .warning)
    }

    func testLegacyAlertKeepsExistingTerminationWarningForSeveralAgents() {
        let alert = QuitConfirmation.makeAlert(activeAgentCount: 3, tmuxHostingEnabled: false)
        XCTAssertEqual(
            alert.informativeText,
            "3 agents are currently working. Quitting will terminate them and discard any unsaved progress."
        )
        assertSharedChrome(alert, destructiveQuit: true, style: .warning)
    }

    // MARK: - Tmux hosting (workers.tmux_hosting on)

    func testTmuxAlertSaysASingleAgentKeepsRunning() {
        let alert = QuitConfirmation.makeAlert(activeAgentCount: 1, tmuxHostingEnabled: true)
        XCTAssertEqual(alert.messageText, "Quit Boss?")
        XCTAssertEqual(
            alert.informativeText,
            "1 agent is currently working. Quitting will not terminate it — it keeps running."
        )
        assertSharedChrome(alert, destructiveQuit: false, style: .informational)
        assertTmuxCopyDoesNotPromiseRelaunch(alert.informativeText)
        assertTmuxCopyDoesNotClaimTermination(alert.informativeText)
    }

    func testTmuxAlertSaysSeveralAgentsKeepRunning() {
        let alert = QuitConfirmation.makeAlert(activeAgentCount: 4, tmuxHostingEnabled: true)
        XCTAssertEqual(
            alert.informativeText,
            "4 agents are currently working. Quitting will not terminate them — they keep running."
        )
        assertSharedChrome(alert, destructiveQuit: false, style: .informational)
        assertTmuxCopyDoesNotPromiseRelaunch(alert.informativeText)
        assertTmuxCopyDoesNotClaimTermination(alert.informativeText)
    }

    // MARK: - AppDelegate wiring through live settings + worker snapshot

    func testAppDelegateAlertFollowsTmuxHostingSettingWhenWorkersAreLive() {
        let (delegate, model, live) = makeWiredDelegate()
        live.update(states: [liveState(slotId: 1, activity: .working)])

        model.applyEventForTest(.settingsList(settings: [tmuxSetting(enabled: false)]))
        let legacy = delegate.makeQuitConfirmationAlert()
        XCTAssertNotNil(legacy)
        XCTAssertEqual(
            legacy?.informativeText,
            "1 agent is currently working. Quitting will terminate them and discard any unsaved progress."
        )
        XCTAssertEqual(legacy?.alertStyle, .warning)
        XCTAssertEqual(legacy?.buttons[1].hasDestructiveAction, true)

        model.applyEventForTest(.settingsList(settings: [tmuxSetting(enabled: true)]))
        let tmux = delegate.makeQuitConfirmationAlert()
        XCTAssertNotNil(tmux)
        XCTAssertEqual(
            tmux?.informativeText,
            "1 agent is currently working. Quitting will not terminate it — it keeps running."
        )
        XCTAssertEqual(tmux?.alertStyle, .informational)
        XCTAssertEqual(tmux?.buttons[1].hasDestructiveAction, false)
        assertTmuxCopyDoesNotPromiseRelaunch(tmux?.informativeText ?? "")
    }

    func testAppDelegateSuppressesAlertWhenNoAgentsAreWorking() {
        let (delegate, model, live) = makeWiredDelegate()
        model.applyEventForTest(.settingsList(settings: [tmuxSetting(enabled: true)]))
        live.update(states: [liveState(slotId: 1, activity: .idle)])
        XCTAssertNil(delegate.makeQuitConfirmationAlert())
    }

    func testUnloadedSettingsDefaultToLegacyTerminationWarning() {
        let (delegate, _, live) = makeWiredDelegate()
        live.update(states: [
            liveState(slotId: 1, activity: .working),
            liveState(slotId: 2, activity: .waitingForInput),
        ])
        // engineSettings is empty until the settings snapshot lands;
        // tmuxHostingEnabled is then false.
        let alert = delegate.makeQuitConfirmationAlert()
        XCTAssertEqual(
            alert?.informativeText,
            "2 agents are currently working. Quitting will terminate them and discard any unsaved progress."
        )
    }

    func testNilChatModelDefaultsToLegacyTerminationWarning() {
        let delegate = AppDelegate()
        let live = LiveWorkerStateStore()
        live.update(states: [liveState(slotId: 1, activity: .spawning)])
        delegate.liveWorkerStates = live
        let alert = delegate.makeQuitConfirmationAlert()
        XCTAssertEqual(
            alert?.informativeText,
            "1 agent is currently working. Quitting will terminate them and discard any unsaved progress."
        )
    }

    // MARK: - Helpers

    private func assertSharedChrome(
        _ alert: NSAlert,
        destructiveQuit: Bool,
        style: NSAlert.Style
    ) {
        XCTAssertEqual(alert.buttons.map(\.title), ["Cancel", "Quit Anyway"])
        XCTAssertEqual(alert.buttons[0].keyEquivalent, "\r")
        XCTAssertEqual(alert.buttons[1].keyEquivalent, "")
        XCTAssertEqual(alert.buttons[1].hasDestructiveAction, destructiveQuit)
        XCTAssertEqual(alert.alertStyle, style)
    }

    private func assertTmuxCopyDoesNotPromiseRelaunch(_ text: String) {
        let lowered = text.lowercased()
        XCTAssertFalse(lowered.contains("relaunch"), "tmux copy must not promise panes return on relaunch: \(text)")
        XCTAssertFalse(lowered.contains("reattach"), "tmux copy must not promise pane re-attachment: \(text)")
        XCTAssertFalse(lowered.contains("come back"), "tmux copy must not promise panes come back: \(text)")
    }

    private func assertTmuxCopyDoesNotClaimTermination(_ text: String) {
        XCTAssertFalse(
            text.contains("will terminate them"),
            "tmux copy must not reuse the legacy termination warning: \(text)"
        )
        XCTAssertFalse(
            text.contains("discard any unsaved progress"),
            "tmux copy must not claim progress is discarded: \(text)"
        )
    }

    private func makeWiredDelegate() -> (AppDelegate, ChatViewModel, LiveWorkerStateStore) {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        let delegate = AppDelegate()
        delegate.chatModel = model
        delegate.liveWorkerStates = model.liveWorkerStates
        return (delegate, model, model.liveWorkerStates)
    }

    private func tmuxSetting(enabled: Bool) -> EngineSetting {
        EngineSetting(
            key: "workers.tmux_hosting",
            description: "Host workers in tmux",
            defaultEnabled: false,
            enabled: enabled
        )
    }

    private func liveState(slotId: Int, activity: WorkerActivity) -> WorkerLiveState {
        WorkerLiveState(
            slotId: slotId,
            runId: "exec-\(slotId)",
            model: "claude-opus-4-7",
            shellPid: 1000 + Int32(slotId),
            lastEventAt: "2026-08-28T00:00:00Z",
            currentTool: nil,
            lastToolEndedAt: nil,
            activity: activity,
            liveStatus: nil,
            liveStatusAt: nil,
            recoveryStatus: nil
        )
    }
}
