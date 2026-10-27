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

    /// The window matches product invalidation bursts so resync shares them.
    /// The initial backoff must exceed one reply set's queue refill time;
    /// the cap bounds board staleness during a sustained storm, and the quiet
    /// reset must exceed the slowest refill. Measured margins are recorded in
    /// docs/investigations/resync-overflow-validation-2026-10-23.md.
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
    /// The product refetch for the pending resync already joined the
    /// coalesced burst, so the fire must not schedule a second one.
    private(set) var productBurstJoined = false

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

    /// The pending resync's product refetch joined the coalesced burst.
    mutating func noteProductBurstJoined() {
        productBurstJoined = true
    }

    /// The refetch set went out: clear the dirty flag and start the next
    /// backoff.
    mutating func noteFire(at now: ContinuousClock.Instant) {
        isDirty = false
        productBurstJoined = false
        lastFireAt = now
        nextAllowedAt = now + currentBackoff
        streak += 1
    }

    /// Drop a pending fire without counting it, for when the connect burst
    /// covers it.
    mutating func cancel() {
        isDirty = false
        productBurstJoined = false
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
        reviewNotifier.suspendDecisionReminders()
        let now = ContinuousClock.now
        let armsTimer = resyncRefetchGate.noteResync(at: now)
        let delay = resyncRefetchGate.delay(at: now)
        // Outside a backoff the product refetch joins the coalesced burst
        // right away (every resync, so a burst that fires between two of
        // them is followed by another), sharing a window with any
        // invalidation already pending instead of starting one after it.
        // During a backoff it joins when the gate fires.
        if delay <= resyncRefetchGate.policy.window {
            scheduleInvalidationRefetch(productID: currentSelectedProductID, includesAllOpenConsumers: true)
            resyncRefetchGate.noteProductBurstJoined()
        }
        guard armsTimer else { return }
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
    /// - `ci_remediation_*`, `ci_failure_cleared`, and `conflict_resolution_*`
    ///   pushes: attempt lists and Activity; the tree recovers active CI chips.
    /// - comment invalidations: each open comment layer reloads once.
    /// - `engine_pool_config`: pool sizes and coordinator model, pushed once
    ///   at session registration, so a dropped one is never re-sent. The
    ///   `get_engine_pool_config` read replies with the same event and has
    ///   none of registration's side effects.
    /// - driver quota: a cached read when a snapshot is loaded or a refresh
    ///   is awaited, so a lost completion push can't leave the pane on
    ///   "Checking…" with Refresh disabled.
    /// - automations: the list and open history, only while the tab is mounted.
    /// - design docs: the open reader and async-viewer documents, whose
    ///   revalidated bodies arrive as pushes.
    ///
    /// `list_live_status_disabled_slots` is not here: the engine never
    /// pushes it, so there is nothing to have lost.
    func fireResyncRefetch() {
        pendingResyncRefetchTask = nil
        guard resyncRefetchGate.isDirty else { return }
        engine.sendSubscribe(topics: ["work.decisions"])
        let productJoined = resyncRefetchGate.productBurstJoined
        resyncRefetchGate.noteFire(at: .now)
        refreshRemediationState()
        if activityIsVisible { refreshEngineAttempts() }
        engine.sendListProducts()
        engine.sendListWorkerLiveStates()
        engine.sendGetEngineHealth()
        engine.sendGitHubAuthStatus()
        engine.sendGetEnginePoolConfig()
        commentBridge.reloadOpenLayers()
        sendResyncOnlyReads()
        if !productJoined {
            scheduleInvalidationRefetch(productID: currentSelectedProductID, includesAllOpenConsumers: true)
        }
    }

    /// Recover cached driver quota and open design docs, whose state survives
    /// disconnects, plus the automations list and open history only while its
    /// tab is mounted. Connect delegates its automation refresh here when owed.
    func sendResyncOnlyReads() {
        refreshVisibleAutomations()
        if resyncQuotaReadOwed || isRefreshingDriverQuota || !driverQuota.neverChecked {
            engine.sendGetDriverQuotaUsage(refresh: false)
        }
        resyncQuotaReadOwed = false
        var docRefs: [DesignDocRef] = []
        for ref in [selectedDesignDocRef, pendingAsyncViewerRef].compactMap({ $0 }) where !docRefs.contains(ref) {
            docRefs.append(ref)
        }
        for ref in docRefs { engine.sendGetProductDesignDoc(ref: ref) }
    }

    /// Drop a pending resync refetch. A (re)connect re-reads most of it, and
    /// a disconnect would queue the requests for the next connection. What the
    /// connect burst does not re-read ([[sendResyncOnlyReads]]) stays owed
    /// until the next connect sends it.
    func cancelPendingResyncRefetch() {
        if resyncRefetchGate.isDirty {
            resyncOnlyReadsOwed = true
            // Decide now: handleConnected clears the in-flight flag before the
            // owed reads go out, so a refresh whose completion was evicted
            // would otherwise look like nothing was waiting on the quota.
            if isRefreshingDriverQuota || !driverQuota.neverChecked { resyncQuotaReadOwed = true }
        }
        pendingResyncRefetchTask?.cancel()
        pendingResyncRefetchTask = nil
        resyncRefetchGate.cancel()
    }
}
