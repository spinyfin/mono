import Foundation

/// Timing of the resync-driven refetch. See [[ResyncRefetchGate]].
struct ResyncRefetchPolicy: Equatable, Sendable {
    /// Quiet time after a resync before the refetch set goes out, so the
    /// resyncs of one overflow (and the invalidations around them) share it.
    var window: Duration
    /// Minimum gap after the first refetch of a storm. Doubles with each
    /// further refetch the storm causes.
    var initialBackoff: Duration
    /// Ceiling for the doubling.
    var maxBackoff: Duration
    /// Time after a refetch with no resync pending before the storm is over
    /// and the next resync starts again from `window` and `initialBackoff`.
    var quietReset: Duration

    /// Chosen from the traffic in
    /// docs/investigations/app-engine-request-audit-2026-10-19.md §4.2: an
    /// overflow loop re-triggered itself about twice a second (433 resyncs in
    /// three minutes), and the refill from one refetch set arrives within a
    /// second or two. `window` matches the 150 ms work-invalidation burst.
    /// `initialBackoff` of 1 s already cuts that loop to one set per second,
    /// the doubling reaches `maxBackoff` after five refetches, and 30 s is
    /// the slowest the board may lag a sustained storm — one set per 30 s is
    /// two sets a minute against the 120 the loop produced. `quietReset`
    /// of 10 s is several times the refill time, so a storm still being fed
    /// is never mistaken for a quiet one, while an isolated overflow later
    /// is answered promptly instead of at the stale ceiling.
    static let standard = ResyncRefetchPolicy(
        window: .milliseconds(150),
        initialBackoff: .seconds(1),
        maxBackoff: .seconds(30),
        quietReset: .seconds(10)
    )
}

/// The decisions behind coalescing and backing off `resync_required`, with
/// time passed in so the exponential schedule is testable without a clock.
///
/// Every resync marks the gate dirty. A dirty gate is fired once, at the
/// later of `window` after the trigger and the end of the backoff the last
/// fire set up; resyncs that arrive while dirty add nothing, because the one
/// pending fire already runs after all of them (the trailing refetch).
struct ResyncRefetchGate: Equatable, Sendable {
    var policy: ResyncRefetchPolicy
    private(set) var isDirty = false
    /// Fires since the storm began; the exponent of the current backoff.
    private(set) var streak = 0
    private(set) var lastFireAt: ContinuousClock.Instant?
    private(set) var nextAllowedAt: ContinuousClock.Instant?

    init(policy: ResyncRefetchPolicy = .standard) {
        self.policy = policy
    }

    /// The gap the next fire must leave after the previous one.
    var currentBackoff: Duration {
        var backoff = policy.initialBackoff
        for _ in 0..<streak where backoff < policy.maxBackoff { backoff *= 2 }
        return min(backoff, policy.maxBackoff)
    }

    /// Record a resync. Returns `true` when it is the one that makes the gate
    /// dirty, and so the caller must arm the timer; `false` when a fire is
    /// already pending and this resync is absorbed by it.
    mutating func noteResync(at now: ContinuousClock.Instant) -> Bool {
        guard !isDirty else { return false }
        if let lastFireAt, lastFireAt.duration(to: now) >= policy.quietReset {
            streak = 0
            nextAllowedAt = nil
        }
        isDirty = true
        return true
    }

    /// How long from `now` until a dirty gate may fire.
    func delay(at now: ContinuousClock.Instant) -> Duration {
        guard let nextAllowedAt else { return policy.window }
        return max(policy.window, now.duration(to: nextAllowedAt))
    }

    /// The refetch set went out: clear the dirty flag and start the next
    /// backoff.
    mutating func noteFire(at now: ContinuousClock.Instant) {
        isDirty = false
        lastFireAt = now
        nextAllowedAt = now + currentBackoff
        streak += 1
    }

    /// Drop a pending fire without counting it, for when the connect burst
    /// covers it.
    mutating func cancel() {
        isDirty = false
    }
}

/// `resync_required` means the engine's session queue overflowed and dropped
/// pushes the app can't name. The reply to the refetch it triggers refills
/// that same queue, so answering each resync with a burst feeds the loop;
/// instead resyncs are collapsed into one refetch set per window and spaced
/// out by an exponential backoff, with a trailing refetch so the state the
/// last dropped push covered is always re-read.
extension ChatViewModel {
    /// A resync arrived. Nothing is sent here; see [[fireResyncRefetch]].
    func handleResyncRequired() {
        let now = ContinuousClock.now
        guard resyncRefetchGate.noteResync(at: now) else { return }
        let delay = resyncRefetchGate.delay(at: now)
        pendingResyncRefetchTask?.cancel()
        pendingResyncRefetchTask = Task { [weak self] in
            try? await Task.sleep(for: delay)
            guard !Task.isCancelled else { return }
            self?.fireResyncRefetch()
        }
    }

    /// Re-read every piece of state the app keeps from a push the engine may
    /// have evicted — the pushes the session queue can drop are the
    /// uncorrelated ones, never replies:
    ///
    /// - `work_invalidated` on `work.product.*`: the product's coalesced
    ///   burst, which also covers the tree's attention items, groups and
    ///   planner-run summaries, the deferred-scope list, the background-work
    ///   poll and every open execution history.
    /// - `work_invalidated` on `work.products`: `list_products`. Its reply
    ///   fetches a tree only when no product is selected, so it adds no
    ///   second tree.
    /// - `worker_live_states_list`: pushed when a state changes, never on a
    ///   timer, so a dropped one stays stale until asked for.
    /// - `engine_health_result` and `git_hub_auth_state`.
    /// - comment invalidations: each open comment layer reloads once.
    ///
    /// `list_live_status_disabled_slots` is not here: the engine never
    /// pushes it, so there is nothing to have lost. Nor are the other
    /// uncorrelated pushes (design-doc revalidation results, driver quota
    /// refreshes, probe replies): each is the one-shot follow-up to an
    /// explicit user action, not topic state the app keeps current, and the
    /// view that asked already offers its own retry.
    func fireResyncRefetch() {
        pendingResyncRefetchTask = nil
        guard resyncRefetchGate.isDirty else { return }
        resyncRefetchGate.noteFire(at: .now)
        engine.sendListProducts()
        engine.sendListWorkerLiveStates()
        engine.sendGetEngineHealth()
        engine.sendGitHubAuthStatus()
        commentBridge.reloadOpenLayers()
        scheduleInvalidationRefetch(productID: currentSelectedProductID, includesAllOpenConsumers: true)
    }

    /// Drop a pending resync refetch. A (re)connect re-reads all of it, and
    /// a disconnect would queue the requests for the next connection.
    func cancelPendingResyncRefetch() {
        pendingResyncRefetchTask?.cancel()
        pendingResyncRefetchTask = nil
        resyncRefetchGate.cancel()
    }
}
