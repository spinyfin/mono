import Foundation

/// Coalesces a pane's keystrokes into at most one engine report per wall-clock
/// second.
///
/// The engine compares the reported stamp against tmux's `#{client_activity}`,
/// which tmux records in whole seconds — so a second report inside the same
/// second carries no information the first did not. Typing at speed would
/// otherwise put a socket write behind every keypress on the main actor.
///
/// The pid is part of the key, not just the timestamp: when a viewer is
/// rebuilt, the replacement client must report its own pid immediately even
/// if its first keystroke lands in the same second as the outgoing one's
/// last — otherwise the engine keeps judging the new client against a pid
/// that no longer exists.
struct TmuxClientInputThrottle {
    private var lastReported: (pid: Int32, epoch: Int64)?

    /// The epoch second to report for a keystroke delivered at `now` into the
    /// client with pid `clientPid`, or `nil` when an identical report has
    /// already gone out.
    mutating func stamp(clientPid: Int32, now: Date) -> Int64? {
        let epoch = Int64(now.timeIntervalSince1970)
        if let last = lastReported, last.pid == clientPid, last.epoch == epoch {
            return nil
        }
        lastReported = (clientPid, epoch)
        return epoch
    }
}

/// Reports keyboard input the app delivers into one tmux-hosted pane, so the
/// engine can tell a viewer whose input path has died from one nobody is
/// typing into.
///
/// Those two look identical from the tmux server: `#{client_activity}`
/// advances only when a client sends the server something, so a frozen value
/// describes an idle client and a wedged client equally well. The app is the
/// only place that knows input was *attempted*, which is what this supplies.
/// See `tools/boss/docs/tmux-client-input-wedge.md`.
///
/// Only for panes whose pty runs `tmux attach-session`: a directly-spawned
/// worker shell has no tmux client, and reporting one would name a session
/// the engine could then try to reconcile.
@MainActor
final class TmuxClientInputReporter {
    /// tmux session this pane's client is attached to.
    private let sessionName: String
    /// Reads the live pid of the `tmux attach-session` process. A closure
    /// rather than a stored value: Ghostty creates surfaces asynchronously,
    /// so the pid does not exist yet when the reporter is built, and the
    /// engine needs whichever client is live *now*.
    private let clientPid: () -> Int32
    private var throttle = TmuxClientInputThrottle()
    /// Installed by the view layer; forwards to the engine socket.
    private let send: (_ sessionName: String, _ clientPid: Int32, _ lastInputEpoch: Int64) -> Void

    init(
        sessionName: String,
        clientPid: @escaping () -> Int32,
        send: @escaping (String, Int32, Int64) -> Void
    ) {
        self.sessionName = sessionName
        self.clientPid = clientPid
        self.send = send
    }

    /// Production wiring: read the pid off the pane's surface, and fire on
    /// every input the app delivers into it.
    convenience init(
        sessionName: String,
        session: TerminalPaneSession,
        send: @escaping (String, Int32, Int64) -> Void
    ) {
        self.init(
            sessionName: sessionName,
            clientPid: { [weak session] in session?.shellPid ?? 0 },
            send: send
        )
        session.onInputDelivered = { [weak self] in
            self?.inputDelivered(at: Date())
        }
    }

    /// `at` is injected so the throttle is testable without sleeping.
    func inputDelivered(at now: Date) {
        // Before the pty's foreground process group exists there is no
        // client pid to name, and a report the engine cannot match to a
        // client is worse than none — it would sit in the store looking
        // permanently unanswered.
        let pid = clientPid()
        guard pid > 0 else { return }
        guard let epoch = throttle.stamp(clientPid: pid, now: now) else { return }
        send(sessionName, pid, epoch)
    }
}
