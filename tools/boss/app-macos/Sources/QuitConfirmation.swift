import AppKit

/// Cmd-Q confirmation shown when agents are still working.
///
/// Hosting mode changes what quit actually does, so the copy has to
/// follow each *running worker's actual dispatch-time hosting mode*
/// (`LiveWorkerState.tmuxHosted`) rather than the current
/// `workers.tmux_hosting` setting value:
///
/// - Legacy (app-owned pty): quitting the app tears down those panes.
///   The original terminate-and-discard wording stays accurate.
/// - Tmux hosting: quit does not touch tmux. The engine is launched
///   detached and `EngineProcessController.stop()` is a no-op unless
///   `BOSS_ENGINE_STOP_ON_EXIT=1`. Detaching `tmux attach-session`
///   clients leaves the sessions (and the agents) running.
///
/// The setting and a worker's actual hosting mode diverge whenever the
/// setting is toggled while a worker dispatched under the old value is
/// still running — `workers.tmux_hosting`'s own settings doc says
/// disabling it "affects only new dispatches; already-running tmux
/// workers keep their durable teardown path". Reading the setting alone
/// makes the dialog confidently wrong in either direction whenever that
/// happens, so [`HostingMakeup`] is derived per-worker instead. A mixed
/// running set gets its own copy naming the split, rather than
/// collapsing to either claim.
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

    /// The hosting-mode makeup of the currently active workers, derived
    /// from each worker's own `tmuxHosted` rather than the settings
    /// value — see the type doc above for why that distinction matters.
    enum HostingMakeup: Equatable {
        case allTmux
        case allLegacy
        case mixed(tmuxCount: Int, legacyCount: Int)

        /// Classify from each active worker's resolved hosting mode.
        /// A worker with no reported mode (`nil` — an older engine that
        /// predates the field, or a worker kind with no local pane at
        /// all) is folded into the legacy bucket: the conservative
        /// read, since legacy is the mode a wrong guess costs the
        /// user unsaved work, not just an inaccurate "safe" claim.
        /// `flags` is expected non-empty; callers already gate on
        /// `agentCount > 0` before reaching this.
        static func classify(_ flags: [Bool?]) -> HostingMakeup {
            let tmuxCount = flags.filter { $0 == true }.count
            let legacyCount = flags.count - tmuxCount
            if legacyCount == 0 {
                return .allTmux
            }
            if tmuxCount == 0 {
                return .allLegacy
            }
            return .mixed(tmuxCount: tmuxCount, legacyCount: legacyCount)
        }
    }

    static func informativeText(agentCount: Int, hostingMakeup: HostingMakeup) -> String {
        let agentWord = agentCount == 1 ? "agent is" : "agents are"
        let prefix = "\(agentCount) \(agentWord) currently working. "
        switch hostingMakeup {
        case .allTmux:
            let pronoun = agentCount == 1 ? "It" : "They"
            let verb = agentCount == 1 ? "keeps" : "keep"
            let object = agentCount == 1 ? "it" : "them"
            return prefix
                + "\(pronoun) \(verb) running after you quit. "
                + "Quitting does not terminate \(object)."
        case .allLegacy:
            return prefix + "Quitting will terminate them and discard any unsaved progress."
        case .mixed(let tmuxCount, let legacyCount):
            let tmuxClause = tmuxCount == 1
                ? "1 keeps running after you quit"
                : "\(tmuxCount) keep running after you quit"
            let legacyClause = legacyCount == 1
                ? "1 will be terminated, discarding its unsaved progress"
                : "\(legacyCount) will be terminated, discarding their unsaved progress"
            return prefix + "These use different hosting modes: \(tmuxClause); \(legacyClause)."
        }
    }

    /// `nil` when there is nothing to confirm (no working agents). The
    /// alert is still produced under tmux hosting — quit is not silent
    /// just because it is non-destructive to the agents.
    @MainActor
    static func alert(agentCount: Int, hostingMakeup: HostingMakeup) -> NSAlert? {
        guard agentCount > 0 else { return nil }
        let alert = NSAlert()
        alert.messageText = messageText
        alert.informativeText = informativeText(
            agentCount: agentCount,
            hostingMakeup: hostingMakeup
        )
        alert.addButton(withTitle: cancelButtonTitle)
        alert.addButton(withTitle: quitButtonTitle)
        alert.alertStyle = .warning
        // Make Cancel (index 0) the default so a stray Cmd-Q doesn't
        // accidentally confirm through the dialog.
        alert.buttons[0].keyEquivalent = "\r"
        alert.buttons[1].keyEquivalent = ""
        // Red chrome unless quit is non-destructive to *every* running
        // agent — a mixed set still kills the legacy-hosted half.
        alert.buttons[1].hasDestructiveAction = hostingMakeup != .allTmux
        return alert
    }
}
