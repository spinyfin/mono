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
            scheduling_active: AtomicBool::new(false),
            scheduling_pending: AtomicBool::new(false),
            repo_cold_probe_seen: Mutex::new(HashSet::new()),
            pre_start_retry_delays: PRE_START_RETRY_DELAYS.to_vec(),
            merge_order_stagger_secs: 0,
            metrics: local_metrics,
            execution_started_hook: Arc::new(NoopExecutionStartedHook),
            automation_preemptor: Arc::new(NoopAutomationPreemptor),
            dispatch_paused: AtomicBool::new(false),
            dispatch_paused_since_epoch_s: AtomicU64::new(0),
            dispatch_pause_exempts_reviews: AtomicBool::new(false),
            automation_paused: AtomicBool::new(false),
            automation_paused_since_epoch_s: AtomicU64::new(0),
            live_worker_states: None,
            refused_workspaces: Mutex::new(HashMap::new()),
            max_concurrent_interactive_workers: MAX_CONCURRENT_INTERACTIVE_WORKERS,
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
    /// [`WORKER_PAGE_SIZE`] raise this so the unrelated, temporary cap
    /// doesn't hold mainline rows before they ever reach the spillover/
    /// preemption path under test.
    pub fn with_max_concurrent_interactive_workers(mut self, max: usize) -> Self {
        self.max_concurrent_interactive_workers = max;
        self
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

    /// Pause or resume global dispatch. When `paused = true` the scheduler
    /// drain stops claiming worker slots for new executions from the main and
    /// automation pools; already-running executions are unaffected. `origin`
    /// determines whether `pr_review` executions are exempt from the pause —
    /// see [`DispatchPauseOrigin`] — and is ignored when resuming. Pass
    /// `paused_since_epoch_s = 0` when resuming (it is ignored).
    ///
    /// The caller is responsible for persisting the new state (including
    /// `origin`, via [`DispatchPauseOrigin::as_metadata_str`]) to `state.db`
    /// so it survives an engine restart — see the `handle_set_dispatch_paused`
    /// handler in `app/engine_meta.rs`.
    pub fn set_dispatch_paused(&self, paused: bool, paused_since_epoch_s: u64, origin: DispatchPauseOrigin) {
        self.dispatch_paused.store(paused, Ordering::Release);
        self.dispatch_paused_since_epoch_s
            .store(if paused { paused_since_epoch_s } else { 0 }, Ordering::Release);
        if paused {
            self.dispatch_pause_exempts_reviews
                .store(origin == DispatchPauseOrigin::Operator, Ordering::Release);
        }
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

    /// Pause or resume automation-originated activity — independent of
    /// [`Self::set_dispatch_paused`]. When `paused = true`,
    /// `drain_ready_queue` stops claiming worker slots for executions bound
    /// for the automation pool, and the triage-fire seam
    /// (`EngineTriageDispatcher::fire`) refuses to start a new pass; an
    /// already-claimed automation worker is unaffected. Pass
    /// `paused_since_epoch_s = 0` when resuming (it is ignored).
    ///
    /// The caller is responsible for persisting the new state to `state.db`
    /// so it survives an engine restart — see `handle_set_automation_paused`
    /// in `app/engine_meta.rs`.
    pub fn set_automation_paused(&self, paused: bool, paused_since_epoch_s: u64) {
        self.automation_paused.store(paused, Ordering::Release);
        self.automation_paused_since_epoch_s
            .store(if paused { paused_since_epoch_s } else { 0 }, Ordering::Release);
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
