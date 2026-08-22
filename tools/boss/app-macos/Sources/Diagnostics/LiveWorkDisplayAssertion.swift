import Foundation

/// Holds an idle-**display**-sleep assertion for exactly as long as this app
/// is hosting at least one live worker pane.
///
/// ## Why this is separate from the App Nap opt-out
///
/// `AppDelegate` holds a process-lifetime token with
/// `.userInitiatedAllowingIdleSystemSleep` (see `BossMacApp.swift`). That
/// option set opts out of **App Nap only** — its own doc comment says it
/// deliberately does not pin the display or the system awake, because the
/// 2026-07-15 incident it was written for was about RPC handling staying
/// prompt *during* sleep. It was never a sleep lock, and it never prevented
/// the display from idling off.
///
/// That distinction is load-bearing: with zero active displays,
/// `ghostty_surface_new` cannot succeed at all (CoreVideo refuses to build
/// a display link over an empty display set — measured, see
/// [[SpawnCapability]]). So an unattended fleet whose display idled off
/// could not spawn a worker, which is the failure this type addresses at
/// the root.
///
/// ## The trade-off, stated explicitly
///
/// An assertion held unconditionally would keep the operator's display
/// awake for as long as Boss is running — user-hostile, and a battery cost
/// they did not ask for. So it is scoped to live work: acquired on the
/// 0 → 1 live-pane transition, released on 1 → 0. An idle Boss asserts
/// nothing and the display sleeps normally.
///
/// ## What it deliberately does NOT do
///
/// `.idleDisplaySleepDisabled` suppresses the **idle** display-sleep timer.
/// It cannot (and must not) override a deliberate lock, a lid close, or a
/// manual `pmset displaysleepnow`. A host can therefore still reach zero
/// active displays while workers are live — which is precisely why the
/// engine-side defer/requeue path exists rather than this assertion being
/// treated as a complete fix.
@MainActor
final class LiveWorkDisplayAssertion {
    static let shared = LiveWorkDisplayAssertion()

    private var token: NSObjectProtocol?
    private let begin: (ProcessInfo.ActivityOptions, String) -> NSObjectProtocol
    private let end: (NSObjectProtocol) -> Void

    /// `.userInitiated` (which also keeps the *system* awake while real work
    /// is in flight) plus the display-sleep suppression this exists for.
    /// Spelled out as a constant so a test can assert the exact option set —
    /// the whole defect was an option set that did less than its callers
    /// believed.
    static let activityOptions: ProcessInfo.ActivityOptions = [
        .userInitiated, .idleDisplaySleepDisabled,
    ]

    static let activityReason = "Boss is hosting live worker panes; a worker pane cannot be created with no active display"

    /// Production initializer uses `ProcessInfo`; tests inject recorders.
    init(
        begin: @escaping (ProcessInfo.ActivityOptions, String) -> NSObjectProtocol = {
            ProcessInfo.processInfo.beginActivity(options: $0, reason: $1)
        },
        end: @escaping (NSObjectProtocol) -> Void = { ProcessInfo.processInfo.endActivity($0) }
    ) {
        self.begin = begin
        self.end = end
    }

    /// `true` while the assertion is held.
    var isHeld: Bool { token != nil }

    /// Converge the assertion to `liveWorkerCount`. Idempotent: calling it
    /// repeatedly with the same count neither stacks tokens nor churns them,
    /// so callers can invoke it from every slot mutation without tracking
    /// transitions themselves.
    func update(liveWorkerCount: Int) {
        if liveWorkerCount > 0 {
            guard token == nil else { return }
            token = begin(Self.activityOptions, Self.activityReason)
        } else {
            guard let held = token else { return }
            // Unlike the App Nap token this one IS released — that is the
            // entire point of scoping it to live work.
            end(held)
            token = nil
        }
    }
}
