import AppKit

/// Cmd-Q confirmation shown when agents are still working.
///
/// Hosting mode changes what quitting actually does. Legacy app-owned
/// panes die with this process. Tmux-hosted workers keep running: the
/// quit path does not touch tmux, the engine is launched detached, and
/// `processController.stop()` returns immediately unless
/// `BOSS_ENGINE_STOP_ON_EXIT=1`. The alert must describe the mode
/// currently in effect (`workers.tmux_hosting`), not a single
/// termination warning for both.
@MainActor
enum QuitConfirmation {
    static let messageText = "Quit Boss?"
    static let cancelButtonTitle = "Cancel"
    static let quitButtonTitle = "Quit Anyway"

    /// Informative text for the quit alert.
    ///
    /// `tmuxHostingEnabled` is the operator-facing boolean projection of
    /// `workers.tmux_hosting` (on only when every pool is in the set).
    /// A missing or unloaded setting is treated as off, matching
    /// `ChatViewModel.tmuxHostingEnabled`'s conservative default.
    ///
    /// The tmux wording does not mention relaunch or pane re-attachment:
    /// worker panes are not re-attached on app registration today. It
    /// also does not mention engine replacement: this quit does not
    /// replace the engine. Replacement happens later, on a subsequent
    /// launch whose bundled engine fingerprint differs, via
    /// `EngineProcessController.terminateEngine`.
    static func informativeText(activeAgentCount: Int, tmuxHostingEnabled: Bool) -> String {
        let agentWord = activeAgentCount == 1 ? "agent is" : "agents are"
        let prefix = "\(activeAgentCount) \(agentWord) currently working. "
        if tmuxHostingEnabled {
            if activeAgentCount == 1 {
                return prefix + "Quitting will not terminate it — it keeps running."
            }
            return prefix + "Quitting will not terminate them — they keep running."
        }
        return prefix + "Quitting will terminate them and discard any unsaved progress."
    }

    /// Fully configured `NSAlert` used by `applicationShouldTerminate`.
    /// Tests construct this rather than asserting against source strings
    /// in isolation, so a wiring miss that leaves the alert on the
    /// legacy copy is visible.
    static func makeAlert(activeAgentCount: Int, tmuxHostingEnabled: Bool) -> NSAlert {
        let alert = NSAlert()
        alert.messageText = messageText
        alert.informativeText = informativeText(
            activeAgentCount: activeAgentCount,
            tmuxHostingEnabled: tmuxHostingEnabled
        )
        alert.addButton(withTitle: cancelButtonTitle)
        alert.addButton(withTitle: quitButtonTitle)
        alert.alertStyle = tmuxHostingEnabled ? .informational : .warning

        // Make Cancel (index 0) the default so a stray Cmd-Q doesn't
        // accidentally confirm through the dialog.
        alert.buttons[0].keyEquivalent = "\r"
        alert.buttons[1].keyEquivalent = ""
        // Legacy quit actually kills the workers; tmux quit does not.
        alert.buttons[1].hasDestructiveAction = !tmuxHostingEnabled
        return alert
    }
}
