//! `ExecutionCoordinator` construction, configuration, and pool-selection
//! accessors. Part of the `coordinator` module split; the struct itself and
//! the shared types live in [`super`].
use super::*;

/// Check out a leased cube workspace to the head commit of a PR, so a reviewer
/// worker can read full source at the PR head rather than working from a stale
/// or arbitrary baseline.
///
/// Steps:
/// 1. Fetch the current head OID from GitHub via `gh pr view`.
/// 2. `jj git fetch` — pull the remote refs into the local jj store.
/// 3. `jj new <sha>` — position the working copy on a fresh empty child of the
///    PR head. (`jj new`, not `jj edit`: a pushed PR head is immutable, so
///    `jj edit` fails deterministically; the empty child's tree equals the
///    head's, so the read-only reviewer still sees the PR-head files.)
///
/// Returns the head SHA on success. Any subprocess failure is returned as an
/// `Err` so the dispatcher can record a start failure and retry.
///
/// The caller is responsible for releasing the workspace on error.
impl ExecutionCoordinator {
    /// Convenience constructor for tests and simple callers. Wraps the
    /// provided `cube_client` and `execution_runner` in a
    /// `LocalHostAdapter` and calls [`Self::with_publisher`].
    pub fn new(
        work_db: Arc<WorkDb>,
        worker_pool: WorkerPool,
        cube_client: Arc<dyn CubeClient>,
        execution_runner: Arc<dyn ExecutionRunner>,
    ) -> Self {
        let host_adapter = Arc::new(LocalHostAdapter::new(cube_client, execution_runner));
        Self::with_host_adapter_and_publisher(work_db, worker_pool, host_adapter, Arc::new(NoopExecutionPublisher))
    }

    /// Constructor that accepts a publisher alongside the cube/runner
    /// primitives. Wraps them in `LocalHostAdapter` and delegates to
    /// [`Self::with_host_adapter_and_publisher`].
    pub fn with_publisher(
        work_db: Arc<WorkDb>,
        worker_pool: WorkerPool,
        cube_client: Arc<dyn CubeClient>,
        execution_runner: Arc<dyn ExecutionRunner>,
        publisher: Arc<dyn ExecutionPublisher>,
    ) -> Self {
        let host_adapter = Arc::new(LocalHostAdapter::new(cube_client, execution_runner));
        Self::with_host_adapter_and_publisher(work_db, worker_pool, host_adapter, publisher)
    }

    /// Primary constructor for Phase 3+. Callers that need to dispatch
    /// to a non-local host (e.g. `SshHostAdapter`) build the adapter
    /// themselves and pass it here directly.
    pub fn with_host_adapter_and_publisher(
        work_db: Arc<WorkDb>,
        worker_pool: WorkerPool,
        host_adapter: Arc<dyn HostAdapter>,
        publisher: Arc<dyn ExecutionPublisher>,
    ) -> Self {
        // Build a local registry for tests that never call `set_metrics`.
        // Pre-register the lease counter handles so `.inc()` never panics
        // on "counter not registered" in a test context.
        let local_metrics = Arc::new(Registry::new());
        register_metrics(&local_metrics);
        crate::dispatch_metrics::register_metrics(&local_metrics);
        let host_adapter_provider: Arc<dyn HostAdapterProvider> =
            Arc::new(LocalHostAdapterProvider::new(Arc::clone(&host_adapter)));
        Self {
            work_db,
            worker_pool,
            automation_pool: WorkerPool::new_automation(MAX_AUTOMATION_POOL_SIZE),
            review_pool: WorkerPool::new_review(DEFAULT_REVIEW_POOL_SIZE),
            host_adapter,
            host_adapter_provider,
            publisher,
            dispatch_events: Arc::new(NoopDispatchEventSink),
            inflight_dispatches: InflightDispatches::new(),
            dispatch_slots: Arc::new(Semaphore::new(MAX_INFLIGHT_DISPATCHES)),
            scheduling_active: AtomicBool::new(false),
            scheduling_pending: AtomicBool::new(false),
            event_bus: Arc::new(EventBus::new()),
            enable_dispatch_ready_bus: false,
            repo_cold_probe_seen: Mutex::new(HashSet::new()),
            pre_start_retry_delays: PRE_START_RETRY_DELAYS.to_vec(),
            merge_order_stagger_secs: 0,
            metrics: local_metrics,
            execution_started_hook: Arc::new(NoopExecutionStartedHook),
            automation_preemptor: Arc::new(NoopAutomationPreemptor),
            dispatch_paused: AtomicBool::new(false),
            dispatch_paused_since_epoch_s: AtomicU64::new(0),
            dispatch_pause_exempts_reviews: AtomicBool::new(false),
            dispatch_paused_reason: std::sync::Mutex::new(None),
            dispatch_pause_bypass_execution_ids: std::sync::Mutex::new(HashSet::new()),
            dispatch_preflight_block_reason: std::sync::Mutex::new(None),
            automation_paused: AtomicBool::new(false),
            automation_paused_since_epoch_s: AtomicU64::new(0),
            automation_paused_reason: std::sync::Mutex::new(None),
            live_worker_states: None,
            refused_workspaces: Mutex::new(HashMap::new()),
            max_concurrent_interactive_workers: AtomicUsize::new(MAX_CONCURRENT_INTERACTIVE_WORKERS),
        }
    }

    /// Seed the bounded `merge_order` dispatch-stagger window (seconds).
    /// `app.rs` calls this with the (already-clamped)
    /// [`crate::config::WorkConfig::merge_order_stagger_secs`]; `0` disables
    /// the stagger. Tests set it directly to exercise the deferral.
    pub fn set_merge_order_stagger_secs(&mut self, secs: u64) {
        self.merge_order_stagger_secs = secs;
    }

    /// Override the automation pool. `app.rs` calls this with a pool sized
    /// from `BOSS_AUTOMATION_POOL_SIZE`; tests may supply a smaller pool.
    pub fn set_automation_pool(&mut self, pool: WorkerPool) {
        self.automation_pool = pool;
    }

    /// The local-host adapter. `app.rs` reads this to seed the production
    /// [`crate::host_adapter::SshHostAdapterProvider`] (which returns it
    /// verbatim for `host_id = "local"`).
    pub fn host_adapter(&self) -> Arc<dyn HostAdapter> {
        Arc::clone(&self.host_adapter)
    }

    /// Install the host-adapter provider used to build per-host adapters
    /// in the dispatch loop. `app.rs` wires the SSH-capable provider so
    /// the coordinator can route to registered remote hosts; tests inject
    /// recording/fake providers to assert routing.
    pub fn set_host_adapter_provider(&mut self, provider: Arc<dyn HostAdapterProvider>) {
        self.host_adapter_provider = provider;
    }

    /// Read the tail of a run's transcript that lives on host `host_id`.
    ///
    /// Returns `Ok(None)` for `host_id = "local"` — the transcript is on
    /// the engine's own filesystem, so the caller reads the recorded
    /// path directly. For a remote host, resolves the host + adapter and
    /// pulls the last `max_bytes` of `path` over SSH (the design's Q7
    /// readback, done on demand rather than via a streaming socket).
    /// `app.rs`'s `TailRunTranscript` handler routes remote runs through
    /// here so `bossctl agents transcript` / the transcript viewer work
    /// identically against a remote worker.
    pub async fn read_remote_transcript_tail(
        &self,
        host_id: &str,
        path: &str,
        max_bytes: u64,
    ) -> Result<Option<String>> {
        if host_id == "local" {
            return Ok(None);
        }
        let host = self
            .work_db
            .get_host(host_id)?
            .ok_or_else(|| anyhow!("unknown host '{host_id}' for remote transcript read"))?;
        let adapter = self.host_adapter_provider.adapter_for(&host).await?;
        adapter.read_transcript_tail_bytes(path, max_bytes).await
    }

    /// Re-establish reverse events forwards for every detached remote run
    /// after an engine restart. Thin binding of the coordinator's
    /// `work_db` + host-adapter provider to
    /// [`crate::remote_reattach::reattach_remote_runs`]; `app.rs` calls
    /// this once at startup so a remote worker that outlived the previous
    /// engine has its hook stream (and eventual completion) routed back.
    pub async fn reattach_remote_runs(&self, engine_events_socket: &str) -> crate::remote_reattach::ReattachSummary {
        crate::remote_reattach::reattach_remote_runs(
            &self.work_db,
            self.host_adapter_provider.as_ref(),
            engine_events_socket,
        )
        .await
    }

    /// Run one cross-host remote-lease reconcile pass and kick the
    /// scheduler if anything was reaped (a cleared remote zombie unblocks
    /// the redundant-spawn guard for its work item). Thin binding of the
    /// coordinator's `work_db` + host-adapter provider + dispatch-event
    /// sink to [`crate::remote_lease_reconcile::reconcile_remote_leases`];
    /// the periodic sweep in `app.rs` drives it.
    pub async fn reconcile_remote_leases_once(
        self: &Arc<Self>,
    ) -> crate::remote_lease_reconcile::RemoteLeaseReconcileOutcome {
        let outcome = crate::remote_lease_reconcile::reconcile_remote_leases(
            &self.work_db,
            self.host_adapter_provider.as_ref(),
            self.dispatch_events.as_ref(),
        )
        .await;
        if outcome.reaped > 0 {
            self.kick();
        }
        outcome
    }

    /// Return a clone of the automation worker pool handle. Used by
    /// `app.rs` to expose the pool's live state to the Agents-tab UI.
    pub fn automation_worker_pool(&self) -> WorkerPool {
        self.automation_pool.clone()
    }

    /// Override the review pool. `app.rs` calls this with a pool sized
    /// from `BOSS_REVIEW_POOL_SIZE`; tests may supply a smaller pool.
    pub fn set_review_pool(&mut self, pool: WorkerPool) {
        self.review_pool = pool;
    }

    /// Return a clone of the review worker pool handle. Used by `app.rs`
    /// to expose the pool's live state to the Agents-tab UI and by the
    /// pool-claim reconciler to sweep leaked review claims.
    pub fn review_worker_pool(&self) -> WorkerPool {
        self.review_pool.clone()
    }

    /// Return the union of execution ids currently claimed across ALL
    /// worker pools (main, automation, and review).
    ///
    /// The orphan-active sweep uses this as its liveness oracle so that
    /// executions claimed in the review or automation pools are correctly
    /// treated as live — not abandoned and re-dispatched.  Using only
    /// `worker_pool().claimed_execution_ids()` (the main pool) would miss
    /// review-pool claims and cause the sweep to abandon live reviewer
    /// executions ~90 s after they start.
    pub async fn all_claimed_execution_ids(&self) -> std::collections::HashSet<String> {
        let mut claimed = self.worker_pool.claimed_execution_ids().await;
        claimed.extend(self.automation_pool.claimed_execution_ids().await);
        claimed.extend(self.review_pool.claimed_execution_ids().await);
        claimed
    }

    /// Re-claim `worker_id` for `execution_id` in whichever pool owns that
    /// slot — the union-of-pools counterpart to
    /// [`Self::all_claimed_execution_ids`], used by the re-adoption path.
    ///
    /// A slot id names a physical pane workspace slot, and any of the three
    /// pools may be the one holding it (an automation run that spilled into
    /// the interactive pool, a reviewer in the review pool). Restoring the
    /// claim in only the main pool would leave a re-adopted reviewer still
    /// reading as unclaimed to `all_claimed_execution_ids` — the exact
    /// main-pool-only blind spot that made the orphan sweep abandon live
    /// reviewers before the union fix.
    ///
    /// Returns `true` as soon as some pool holds the claim for
    /// `execution_id`; `false` when no pool has that slot free (or it belongs
    /// to a different execution), which the caller treats as "the row's status
    /// is the only re-dispatch protection".
    pub async fn reclaim_slot(&self, worker_id: &str, execution_id: &str) -> bool {
        for pool in [&self.worker_pool, &self.automation_pool, &self.review_pool] {
            if pool.reclaim_slot(worker_id, execution_id).await {
                return true;
            }
        }
        false
    }

    /// Wire the execution-started hook. Production installs the
    /// `WorkerCompletionHandler` here so it can snapshot the bound
    /// chore PR's head SHA into `work_executions.pr_head_before`
    /// when an execution transitions to `running`.
    pub fn set_execution_started_hook(&mut self, hook: Arc<dyn ExecutionStartedHook>) {
        self.execution_started_hook = hook;
    }

    /// Wire the automation-preemption teardown. Production installs the
    /// `WorkerCompletionHandler` here so a starved mainline item can
    /// reclaim an interactive slot from a spilled automation run through
    /// the same pane-reap + lease-release path `bossctl agents stop`
    /// uses. Left unset (the [`NoopAutomationPreemptor`] default),
    /// preemption is disabled and mainline simply waits for a slot.
    pub fn set_automation_preemptor(&mut self, preemptor: Arc<dyn AutomationPreemptor>) {
        self.automation_preemptor = preemptor;
    }

    /// Wire the engine-global metrics registry into this coordinator.
    /// `app.rs` calls this once after `init_all` has registered the
    /// lease counter handles. Tests that omit this call use a pre-seeded
    /// local registry (created in `with_publisher`) so counter increments
    /// never panic.
    pub fn set_metrics(&mut self, metrics: Arc<Registry>) {
        self.metrics = metrics;
    }

    /// Seed the `kick()`-routes-through-the-bus kill-switch. `app.rs` calls
    /// this once at construction with `cfg.work.enable_dispatch_ready_bus`
    /// (default off). A boot-time setting, not a live toggle: nothing flips
    /// this after the coordinator is wrapped in its `Arc`. See
    /// [`Self::enable_dispatch_ready_bus`] and
    /// [`Self::spawn_dispatch_ready_subscriber`].
    pub fn set_enable_dispatch_ready_bus(&mut self, enabled: bool) {
        self.enable_dispatch_ready_bus = enabled;
    }

    /// Wire the engine-wide event bus into this coordinator. Per the design
    /// doc ("One engine process, one bus" —
    /// `engine-event-bus-event-driven-reconcilers-via-an-in-process-message-queue.md`),
    /// there must be exactly one `EventBus` per engine process so every
    /// producer and subscriber — this coordinator's `kick()`/
    /// `spawn_dispatch_ready_subscriber`, and any future producer wired
    /// through `crate::event_publish::commit_and_publish` — reaches the same
    /// fan-out. `app.rs` calls this once at construction with the single
    /// `Arc<EventBus>` it also hands to `ServerState`. Left unset, the
    /// coordinator keeps its private `EventBus::new()` default, which is
    /// correct for tests and any caller that never wires a shared bus but
    /// would silently strand events from a second producer in production.
    pub fn set_event_bus(&mut self, bus: Arc<EventBus>) {
        self.event_bus = bus;
    }

    /// The coordinator's `EventBus`, per the design doc's "one engine
    /// process, one bus" invariant. A future producer living outside the
    /// coordinator (e.g. a `crate::event_publish::commit_and_publish` call
    /// site in another module) reaches the same bus `kick()` and
    /// `spawn_dispatch_ready_subscriber` use through
    /// `server_state.execution_coordinator.event_bus()`, rather than
    /// constructing an unreachable bus of its own. `serve_with_merge_probe`
    /// also uses this to log the subscriber count once the dispatch-ready
    /// subscriber has attached, as a boot-time sanity check that the
    /// injected bus in [`Self::set_event_bus`] is actually the one wired up.
    pub(crate) fn event_bus(&self) -> &Arc<EventBus> {
        &self.event_bus
    }

    /// Wire the engine's live per-slot worker registry so the dispatch
    /// loop can run the lease-time occupancy guard (defect 3). `app.rs`
    /// calls this once with the shared registry; tests that want to
    /// exercise the guard install a registry, and those that don't leave
    /// it unset (the guard then fails open, preserving legacy behaviour).
    pub fn set_live_worker_states(&mut self, live: Arc<crate::live_worker_state::LiveWorkerStateRegistry>) {
        self.live_worker_states = Some(live);
    }

    /// Override the pre-start retry delay schedule. Pass an empty vec
    /// to disable retries entirely (immediate permanent failure); pass
    /// short durations in tests to avoid real sleeps.
    pub fn with_pre_start_retry_delays(mut self, delays: Vec<Duration>) -> Self {
        self.pre_start_retry_delays = delays;
        self
    }

    /// Override the interactive-pool concurrency cap ceiling (default
    /// [`MAX_CONCURRENT_INTERACTIVE_WORKERS`]). Tests that exercise
    /// automation spillover/preemption at pool sizes at or near
    /// [`WORKER_PAGE_SIZE`] raise this so the cap doesn't hold mainline rows
    /// before they reach the spillover/preemption path under test.
    pub fn with_max_concurrent_interactive_workers(self, max: usize) -> Self {
        self.max_concurrent_interactive_workers.store(max, Ordering::Release);
        self
    }

    /// Current interactive-pool concurrency cap.
    pub fn max_concurrent_interactive_workers(&self) -> usize {
        self.max_concurrent_interactive_workers.load(Ordering::Acquire)
    }

    /// Live-set the interactive-pool concurrency cap. `0` is rejected
    /// outright with `Err` — it would wedge all mainline dispatch with no
    /// error surface — and anything above the live main-pool capacity
    /// (`self.worker_pool.capacity_sync()`, the same number
    /// `handle_worker_pool_summary` reports and the value
    /// `config.worker_pool_size`/`BOSS_WORKER_POOL_SIZE` produces at
    /// construction) is clamped down to it, since there are no backing slots
    /// or panes above that on this instance. Clamping against the live pool
    /// rather than the compile-time [`MAX_WORKER_POOL_SIZE`] ceiling avoids
    /// accepting a cap the configured pool can never fill, which would
    /// otherwise reproduce the same capacity-vs-cap contradiction a
    /// shrunk-and-capped pool is meant to avoid. Reported via
    /// `Ok(SetConcurrencyCapOutcome::clamped_from)` so the caller can still
    /// surface a clear message instead of pretending the request was
    /// honored verbatim.
    ///
    /// The caller is responsible for persisting the new value to `state.db`
    /// (see `handle_set_dispatch_concurrency` in `app/engine_meta.rs`) so it
    /// survives an engine restart, and for calling [`Self::kick`] afterward
    /// when raising the cap — a bare store here does not itself wake
    /// `drain_ready_queue`, so a raised cap would otherwise sit unused until
    /// the next naturally-triggered drain pass.
    pub fn set_max_concurrent_interactive_workers(&self, requested: usize) -> Result<SetConcurrencyCapOutcome, String> {
        if requested == 0 {
            return Err(
                "interactive concurrency cap must be at least 1 (0 would wedge all mainline dispatch)".to_string(),
            );
        }
        let ceiling = self.worker_pool.capacity_sync().min(MAX_WORKER_POOL_SIZE);
        let applied = requested.min(ceiling);
        self.max_concurrent_interactive_workers
            .store(applied, Ordering::Release);
        Ok(SetConcurrencyCapOutcome {
            applied,
            clamped_from: (applied != requested).then_some(requested),
        })
    }

    /// Install a dispatch-event sink. The production engine threads
    /// in a `JsonlFileSink` writing under the Boss state root; tests
    /// pass a `RecordingDispatchEventSink` to assert on the stage
    /// timeline.
    pub fn set_dispatch_events(&mut self, sink: Arc<dyn DispatchEventSink>) {
        self.dispatch_events = sink;
    }

    /// Builder-style equivalent for callers that construct the
    /// coordinator inside an `Arc::new(...)` chain.
    pub fn with_dispatch_events(mut self, sink: Arc<dyn DispatchEventSink>) -> Self {
        self.dispatch_events = sink;
        self
    }

    pub fn worker_pool(&self) -> WorkerPool {
        self.worker_pool.clone()
    }

    /// Pause global dispatch. The scheduler drain stops claiming worker
    /// slots for new executions from the main and automation pools;
    /// already-running executions are unaffected. `origin` determines
    /// whether `pr_review` executions are exempt from the pause — see
    /// [`DispatchPauseOrigin`]. `reason` is a required, validated non-empty
    /// [`PauseReason`] rather than a bare `String` — there is no overload
    /// that lets a caller pause dispatch without one, which is the point:
    /// dispatch must never be found paused with no record of who paused it
    /// or why. See [`Self::resume_dispatch`] to clear the pause.
    ///
    /// The caller is responsible for persisting the new state (including
    /// `origin`, via [`DispatchPauseOrigin::as_metadata_str`], and `reason`)
    /// to `state.db` so it survives an engine restart — see the
    /// `handle_set_dispatch_paused` handler in `app/engine_meta.rs`.
    pub fn pause_dispatch(&self, paused_since_epoch_s: u64, origin: DispatchPauseOrigin, reason: PauseReason) {
        self.dispatch_paused.store(true, Ordering::Release);
        self.dispatch_paused_since_epoch_s
            .store(paused_since_epoch_s, Ordering::Release);
        self.dispatch_pause_exempts_reviews
            .store(origin == DispatchPauseOrigin::Operator, Ordering::Release);
        *self.dispatch_paused_reason.lock().unwrap() = Some(reason.into());
    }

    /// Resume global dispatch. Clears the pause flag, the paused-since
    /// timestamp, and — critically — the stored reason, so a later pause
    /// starts from a clean slate rather than silently inheriting whatever
    /// reason the previous episode carried.
    ///
    /// The caller is responsible for persisting the new state to
    /// `state.db` — see `handle_set_dispatch_paused` in `app/engine_meta.rs`.
    pub fn resume_dispatch(&self) {
        self.dispatch_paused.store(false, Ordering::Release);
        self.dispatch_paused_since_epoch_s.store(0, Ordering::Release);
        *self.dispatch_paused_reason.lock().unwrap() = None;
    }

    /// `true` when dispatch is globally paused.
    pub fn is_dispatch_paused(&self) -> bool {
        self.dispatch_paused.load(Ordering::Acquire)
    }

    /// `true` when the current pause (if any) exempts `pr_review` executions
    /// from `drain_ready_queue`'s pause gate. Meaningless when
    /// [`Self::is_dispatch_paused`] is `false`.
    pub fn dispatch_pause_exempts_reviews(&self) -> bool {
        self.dispatch_pause_exempts_reviews.load(Ordering::Acquire)
    }

    /// The epoch-seconds timestamp at which dispatch was last paused, or
    /// `None` when not currently paused.
    pub fn dispatch_paused_since_epoch_s(&self) -> Option<u64> {
        let v = self.dispatch_paused_since_epoch_s.load(Ordering::Acquire);
        if v == 0 { None } else { Some(v) }
    }

    /// Why dispatch is currently paused, or `None` when not paused. See
    /// [`Self::pause_dispatch`] / [`Self::resume_dispatch`].
    pub fn dispatch_paused_reason(&self) -> Option<String> {
        self.dispatch_paused_reason.lock().unwrap().clone()
    }

    /// Block all local dispatch until the required startup capability is
    /// available. This deliberately does not share the resumable dispatch
    /// pause state: breaker recovery and an operator resume must not lift a
    /// failed runtime preflight.
    pub fn set_dispatch_preflight_block(&self, reason: Option<String>) {
        *self.dispatch_preflight_block_reason.lock().unwrap() = reason;
    }

    /// The reason local dispatch is blocked by startup preflight, if any.
    pub fn dispatch_preflight_block_reason(&self) -> Option<String> {
        self.dispatch_preflight_block_reason.lock().unwrap().clone()
    }

    /// Pause automation-originated activity — independent of
    /// [`Self::pause_dispatch`]. `drain_ready_queue` stops claiming worker
    /// slots for executions bound for the automation pool, and the
    /// triage-fire seam (`EngineTriageDispatcher::fire`) refuses to start a
    /// new pass; an already-claimed automation worker is unaffected.
    /// `reason` is a required, validated non-empty [`PauseReason`] for the
    /// same anonymity-prevention reason as [`Self::pause_dispatch`]. See
    /// [`Self::resume_automation`] to clear the pause.
    ///
    /// The caller is responsible for persisting the new state (including
    /// `reason`) to `state.db` so it survives an engine restart — see
    /// `handle_set_automation_paused` in `app/engine_meta.rs`.
    pub fn pause_automation(&self, paused_since_epoch_s: u64, reason: PauseReason) {
        self.automation_paused.store(true, Ordering::Release);
        self.automation_paused_since_epoch_s
            .store(paused_since_epoch_s, Ordering::Release);
        *self.automation_paused_reason.lock().unwrap() = Some(reason.into());
    }

    /// Resume automation-originated activity. Clears the pause flag, the
    /// paused-since timestamp, and the stored reason — see
    /// [`Self::resume_dispatch`] for why clearing the reason matters.
    ///
    /// The caller is responsible for persisting the new state to
    /// `state.db` — see `handle_set_automation_paused` in
    /// `app/engine_meta.rs`.
    pub fn resume_automation(&self) {
        self.automation_paused.store(false, Ordering::Release);
        self.automation_paused_since_epoch_s.store(0, Ordering::Release);
        *self.automation_paused_reason.lock().unwrap() = None;
    }

    /// `true` when automation-originated activity is globally paused.
    pub fn is_automation_paused(&self) -> bool {
        self.automation_paused.load(Ordering::Acquire)
    }

    /// The epoch-seconds timestamp at which automation was last paused, or
    /// `None` when not currently paused.
    pub fn automation_paused_since_epoch_s(&self) -> Option<u64> {
        let v = self.automation_paused_since_epoch_s.load(Ordering::Acquire);
        if v == 0 { None } else { Some(v) }
    }

    /// Why automation is currently paused, or `None` when not paused. See
    /// [`Self::pause_automation`] / [`Self::resume_automation`].
    pub fn automation_paused_reason(&self) -> Option<String> {
        self.automation_paused_reason.lock().unwrap().clone()
    }

    /// The pool `execution` is **attributed** to (`"main"`,
    /// `"automation"`, or `"review"`), independent of which pool's slot it
    /// physically occupies.
    ///
    /// These two normally agree, and for most of the engine's history the
    /// worker-id prefix (`worker-` / `auto-worker-` / `review-`) was a
    /// sound proxy for both. Spillover breaks that proxy: an automation
    /// execution that spilled into Lower Decks holds an ordinary
    /// `worker-N` slot, so anything keying attribution off the prefix
    /// would silently report automation load as main-pool load. Diagnostic
    /// surfaces that answer "what kind of work is this?" must use this;
    /// code answering "which pool owns this slot?" (release routing) must
    /// keep using [`Self::pool_for_worker_id`].
    pub fn attributed_pool_label(&self, execution: &WorkExecution) -> &'static str {
        if self.execution_targets_review_pool(execution) {
            "review"
        } else if self.execution_targets_automation_pool(execution) {
            "automation"
        } else {
            "main"
        }
    }

    /// Return the pool that should handle `execution`.
    ///
    /// `pr_review` executions always route to the review pool — this is
    /// checked first so a reviewer of an automation-produced task still
    /// lands in the review pool, not the automation pool.
    /// `automation_triage` executions always route to the automation pool.
    /// Regular task executions route to the automation pool when the owning
    /// task has `source_automation_id IS NOT NULL` (it was produced by an
    /// automation). All other executions go to the main pool.
    pub(super) fn pool_for_execution<'a>(&'a self, execution: &WorkExecution) -> &'a WorkerPool {
        if self.execution_targets_review_pool(execution) {
            &self.review_pool
        } else if self.execution_targets_automation_pool(execution) {
            &self.automation_pool
        } else {
            &self.worker_pool
        }
    }

    /// `true` when `execution` must run on the dedicated review pool —
    /// i.e. it is a `pr_review` reviewer execution.
    pub(super) fn execution_targets_review_pool(&self, execution: &WorkExecution) -> bool {
        execution.kind == ExecutionKind::PrReview
    }

    pub(super) fn execution_targets_automation_pool(&self, execution: &WorkExecution) -> bool {
        if execution.kind == ExecutionKind::AutomationTriage {
            return true;
        }
        matches!(
            self.work_db.source_automation_id_for_work_item(&execution.work_item_id),
            Ok(Some(_))
        )
    }

    /// Return the pool that owns `worker_id`. Automation-pool slots carry the
    /// `"auto-worker-"` prefix and review-pool slots the `"review-"` prefix,
    /// both stamped at construction time; everything else is the main pool.
    pub(super) fn pool_for_worker_id<'a>(&'a self, worker_id: &str) -> &'a WorkerPool {
        if worker_id.starts_with(REVIEW_WORKER_ID_PREFIX) {
            &self.review_pool
        } else if worker_id.starts_with(AUTOMATION_WORKER_ID_PREFIX) {
            &self.automation_pool
        } else {
            &self.worker_pool
        }
    }
}
