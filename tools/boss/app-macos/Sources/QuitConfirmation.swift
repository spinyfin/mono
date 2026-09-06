import AppKit

/// Cmd-Q confirmation shown when agents are still working.
///
/// Hosting mode changes what quit actually does, so the copy has to
/// follow `workers.tmux_hosting` rather than always claiming destruction:
///
/// - Legacy (app-owned pty): quitting the app tears down those panes.
///   The original terminate-and-discard wording stays accurate.
/// - Tmux hosting: quit does not touch tmux. The engine is launched
///   detached and `EngineProcessController.stop()` is a no-op unless
///   `BOSS_ENGINE_STOP_ON_EXIT=1`. Detaching `tmux attach-session`
///   clients leaves the sessions (and the agents) running.
///
/// This dialog does **not** promise that panes reappear on relaunch —
/// worker pane re-attachment on app registration is unbuilt. It also
/// does **not** mention engine replacement: that is a *next-launch*
/// `EngineProcessController.start()` fingerprint mismatch, not a
/// consequence of this quit. Warning about it here would make a
/// non-destructive quit sound destructive.
enum QuitConfirmation {
    static let messageText = "Quit Boss?"
    static let cancelButtonTitle = "Cancel"
    static let quitButtonTitle = "Quit Anyway"

    static func informativeText(agentCount: Int, tmuxHostingEnabled: Bool) -> String {
        let agentWord = agentCount == 1 ? "agent is" : "agents are"
        let prefix = "\(agentCount) \(agentWord) currently working. "
        if tmuxHostingEnabled {
            let pronoun = agentCount == 1 ? "It" : "They"
            let verb = agentCount == 1 ? "keeps" : "keep"
            let object = agentCount == 1 ? "it" : "them"
            return prefix
                + "\(pronoun) \(verb) running after you quit. "
                + "Quitting does not terminate \(object)."
        }
        return prefix + "Quitting will terminate them and discard any unsaved progress."
    }

    /// `nil` when there is nothing to confirm (no working agents). The
    /// alert is still produced under tmux hosting — quit is not silent
    /// just because it is non-destructive to the agents.
    @MainActor
    static func alert(agentCount: Int, tmuxHostingEnabled: Bool) -> NSAlert? {
        guard agentCount > 0 else { return nil }
        let alert = NSAlert()
        alert.messageText = messageText
        alert.informativeText = informativeText(
            agentCount: agentCount,
            tmuxHostingEnabled: tmuxHostingEnabled
        )
        alert.addButton(withTitle: cancelButtonTitle)
        alert.addButton(withTitle: quitButtonTitle)
        alert.alertStyle = .warning
        // Make Cancel (index 0) the default so a stray Cmd-Q doesn't
        // accidentally confirm through the dialog.
        alert.buttons[0].keyEquivalent = "\r"
        alert.buttons[1].keyEquivalent = ""
        // Red chrome only when quit actually kills the agents.
        alert.buttons[1].hasDestructiveAction = !tmuxHostingEnabled
        return alert
    }
}
