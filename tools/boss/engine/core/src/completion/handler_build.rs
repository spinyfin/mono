//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

impl WorkerCompletionHandler {
    pub fn new(
        work_db: Arc<WorkDb>,
        pr_detector: Arc<dyn PrDetector>,
        cube_client: Arc<dyn CubeClient>,
        publisher: Arc<dyn ExecutionPublisher>,
        pane_releaser: Arc<dyn WorkerPaneReleaser>,
        probe_queuer: Arc<dyn ProbeQueuer>,
    ) -> Self {
        // Build a local registry for tests that never call `with_metrics`.
        // Pre-register the PR-capture handles so `.inc()` never panics on
        // "counter not registered" in a test context.
        let local_metrics = Arc::new(Registry::new());
        register_metrics(&local_metrics);
        Self {
            work_db,
            pr_detector,
            cube_client,
            publisher,
            pane_releaser,
            probe_queuer,
            staged_pr_urls: Arc::new(crate::pr_url_capture::StagedPrUrlCache::new()),
            staged_revision_pushes: Arc::new(crate::pr_url_capture::StagedRevisionPushCache::new()),
            feature_flags: Arc::new(crate::feature_flags::FeatureFlagsStore::new(std::path::PathBuf::new())),
            branch_verifier: Arc::new(CommandBranchVerifier::new()),
            metrics: local_metrics,
            merge_probe: Arc::new(NoopMergeProbe),
            conflict_unknown_backoff: conflict_stop_gate::DEFAULT_UNKNOWN_RETRY_BACKOFF,
            nudge_breaker: Arc::new(NudgeBreaker::new()),
            max_unproductive_nudges: DEFAULT_MAX_UNPRODUCTIVE_NUDGES,
            build_wait_tracker: Arc::new(BuildWaitTracker::new()),
            build_wait_horizon_secs: DEFAULT_BUILD_WAIT_HORIZON_SECS,
            background_activity_probe: Arc::new(crate::background_children::NoopBackgroundActivityProbe),
            background_children_tracker: Arc::new(BuildWaitTracker::new()),
            background_children_horizon_secs: crate::background_children::DEFAULT_BACKGROUND_CHILDREN_HORIZON_SECS,
            hold_registry: Arc::new(crate::hold_registry::HoldRegistry::new()),
            max_review_cycles: crate::config::DEFAULT_MAX_REVIEW_CYCLES,
            min_review_changed_lines: crate::config::DEFAULT_MIN_REVIEW_CHANGED_LINES,
            enable_revision_triggered_reviews: false,
            pr_state_checker: Arc::new(crate::work::GhPrStateChecker),
            structured_output_dir: crate::structured_output::default_dir(),
            now_fn: Arc::new(std::time::Instant::now),
        }
    }

    /// Point structured-output reads at `dir` instead of the process-wide
    /// [`crate::structured_output::default_dir`]. Tests use this to seed the
    /// per-execution artifact in a tempdir; production leaves the default.
    pub fn with_structured_output_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.structured_output_dir = dir;
        self
    }

    /// Wire an externally-owned [`StagedRevisionPushCache`] into this handler.
    /// Called by `app.rs` so the PostToolUse dispatcher and on_stop_inner share
    /// the same cache instance.
    pub fn with_staged_revision_pushes(mut self, cache: Arc<crate::pr_url_capture::StagedRevisionPushCache>) -> Self {
        self.staged_revision_pushes = cache;
        self
    }

    /// Wire an externally-owned [`NudgeBreaker`] into this handler.
    /// `app.rs` does not need to call this — each handler owns its own
    /// breaker. Tests use it to share / inspect breaker state.
    pub fn with_nudge_breaker(mut self, breaker: Arc<NudgeBreaker>) -> Self {
        self.nudge_breaker = breaker;
        self
    }

    /// Override the consecutive-unproductive-nudge cap. Tests set this
    /// low to trip the breaker deterministically; production uses the
    /// default.
    pub fn with_max_unproductive_nudges(mut self, max: u32) -> Self {
        self.max_unproductive_nudges = max;
        self
    }

    /// Wire an externally-owned [`BuildWaitTracker`] into this handler.
    /// Tests use it to share / inspect tracker state.
    pub fn with_build_wait_tracker(mut self, tracker: Arc<BuildWaitTracker>) -> Self {
        self.build_wait_tracker = tracker;
        self
    }

    /// Override the build-wait suppression horizon. Tests set this low (or
    /// to `0`) to exercise the post-expiry fallback to the normal
    /// nudge/park flow deterministically; production uses the default.
    pub fn with_build_wait_horizon_secs(mut self, horizon_secs: i64) -> Self {
        self.build_wait_horizon_secs = horizon_secs;
        self
    }

    /// Wire the real [`crate::background_children::BackgroundActivityProbe`]
    /// into this handler. `app.rs` wires in
    /// [`crate::background_children::RegistryBackgroundActivityProbe`]; tests
    /// inject a fixed-count stub to exercise the suppression path
    /// deterministically without a real process tree.
    pub fn with_background_activity_probe(
        mut self,
        probe: Arc<dyn crate::background_children::BackgroundActivityProbe>,
    ) -> Self {
        self.background_activity_probe = probe;
        self
    }

    /// Wire an externally-owned background-children tracker into this
    /// handler. Tests use it to share / inspect tracker state.
    pub fn with_background_children_tracker(mut self, tracker: Arc<BuildWaitTracker>) -> Self {
        self.background_children_tracker = tracker;
        self
    }

    /// Override the background-children suppression horizon. Tests set this
    /// low (or to `0`) to exercise the post-expiry fallback to the normal
    /// nudge/park flow deterministically; production uses the default.
    pub fn with_background_children_horizon_secs(mut self, horizon_secs: i64) -> Self {
        self.background_children_horizon_secs = horizon_secs;
        self
    }

    /// Wire an externally-owned [`crate::hold_registry::HoldRegistry`] into
    /// this handler. `app.rs` shares one instance between this handler,
    /// the RPC handlers that set/clear holds, and
    /// [`crate::stale_worker_sweep::run_one_pass`].
    pub fn with_hold_registry(mut self, registry: Arc<crate::hold_registry::HoldRegistry>) -> Self {
        self.hold_registry = registry;
        self
    }

    /// Override the automated-reviewer cycle cap.
    /// Production wires in `WorkConfig.max_review_cycles` via `app.rs`;
    /// tests that need to exercise the cycle-bound path set it low.
    pub fn with_max_review_cycles(mut self, max: usize) -> Self {
        self.max_review_cycles = max;
        self
    }

    /// Turn revision-triggered reviewer passes on (2026-07-01 experiment).
    /// Production wires in `WorkConfig.enable_revision_triggered_reviews`
    /// (default `true`) via `app.rs`; tests that exercise the new behaviour
    /// opt in explicitly since the bare handler defaults to `false`.
    pub fn with_enable_revision_triggered_reviews(mut self, enabled: bool) -> Self {
        self.enable_revision_triggered_reviews = enabled;
        self
    }

    /// Override the trivial-diff skip threshold.
    /// Production wires in `WorkConfig.min_review_changed_lines` via `app.rs`;
    /// tests that exercise the trivial-diff path set it to a small value.
    pub fn with_min_review_changed_lines(mut self, min: u64) -> Self {
        self.min_review_changed_lines = min;
        self
    }

    /// Override the PR state checker used by `finalize_pr_review_pass` when
    /// creating a revision. Tests inject
    /// `FakePrStateChecker::always(Open)` to avoid live `gh` calls.
    #[cfg(test)]
    pub(super) fn with_pr_state_checker(mut self, checker: Arc<dyn crate::work::PrStateChecker>) -> Self {
        self.pr_state_checker = checker;
        self
    }

    /// Wire the engine-global metrics registry into this handler. `app.rs`
    /// calls this once after `init_all` has registered the PR-capture
    /// counter handles. Tests that omit this call use a pre-seeded local
    /// registry (created in `new`) so counter increments never panic.
    pub fn with_metrics(mut self, metrics: Arc<Registry>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Wire an externally-owned [`StagedPrUrlCache`] into this
    /// handler so the events-socket dispatcher and the on-Stop
    /// resolver share the same map. `app.rs` calls this once after
    /// construction; tests that want to exercise the staged-URL
    /// path can call it with their own cache. Tests that don't
    /// invoke it get the default empty cache from `new` and follow
    /// the legacy detector path — preserving the pre-change
    /// behaviour without a signature break.
    pub fn with_staged_pr_urls(mut self, cache: Arc<crate::pr_url_capture::StagedPrUrlCache>) -> Self {
        self.staged_pr_urls = cache;
        self
    }

    /// Wire an externally-owned [`BranchVerifier`] into this handler
    /// for Layer-2 staged-URL branch validation. `app.rs` does not need
    /// to call this — the default `CommandBranchVerifier` is correct for
    /// production. Tests that exercise the staged-URL path must call
    /// this with a stub to avoid live `gh pr view` calls.
    pub fn with_branch_verifier(mut self, verifier: Arc<dyn BranchVerifier>) -> Self {
        self.branch_verifier = verifier;
        self
    }

    /// Wire an externally-owned [`FeatureFlagsStore`] into this
    /// handler so engine-wide flag toggles are observed by the
    /// completion path. `app.rs` calls this once at startup with the
    /// store loaded from `~/Library/Application Support/Boss/feature-flags.toml`.
    /// Tests that don't invoke it get the default store (every flag
    /// at its registry default), preserving the pre-change behaviour.
    pub fn with_feature_flags(mut self, flags: Arc<crate::feature_flags::FeatureFlagsStore>) -> Self {
        self.feature_flags = flags;
        self
    }

    /// Whether the rung-1 engine-direct mechanical rebase is enabled on the
    /// conflict-watch path. Read by the merge poller's conflict sweep to
    /// decide whether to hand `on_conflict_detected` a live `CubeClient` for
    /// the escalation ladder. Default OFF (see the flag registry).
    pub fn mechanical_rebase_enabled(&self) -> bool {
        self.feature_flags.is_enabled("conflict_ladder_mechanical_rebase")
    }

    /// Whether the speculative conflict-prediction sweep is enabled (Layer 4).
    /// Read by the merge poller's periodic loop to decide whether to
    /// run [`crate::speculative_conflict::run_speculative_pass`] alongside
    /// the normal full sweep. Default OFF (see the flag registry).
    pub fn speculative_conflict_prediction_enabled(&self) -> bool {
        self.feature_flags.is_enabled("speculative_conflict_prediction")
    }

    /// Whether the stacked-PR auto-structuring sweep is enabled (Layer 4).
    /// Read by the merge poller's periodic loop to decide whether to
    /// run [`crate::stacked_pr_structuring::run_stacking_pass`] alongside the
    /// normal full sweep. Default OFF (see the flag registry).
    pub fn stacked_pr_auto_structuring_enabled(&self) -> bool {
        self.feature_flags.is_enabled("stacked_pr_auto_structuring")
    }

    /// Wire the shared [`MergeProbe`] for the on-transition CI pre-fetch.
    /// `app.rs` passes the same [`CommandMergeProbe`] used by the merge
    /// poller so both paths share probe logic. Tests that do not need the
    /// CI-fetch path can omit this call and rely on the default
    /// [`NoopMergeProbe`].
    pub fn with_merge_probe(mut self, probe: Arc<dyn MergeProbe>) -> Self {
        self.merge_probe = probe;
        self
    }

    /// Shrink the conflict stop gate's `mergeable=UNKNOWN` re-probe
    /// backoff. Tests pass [`std::time::Duration::ZERO`] so exercising the
    /// indeterminate path costs no wall clock; production keeps the
    /// default.
    #[cfg(test)]
    pub(super) fn with_conflict_unknown_backoff(mut self, backoff: std::time::Duration) -> Self {
        self.conflict_unknown_backoff = backoff;
        self
    }

    /// Override the clock the auto-nudge debounce guard
    /// ([`crate::nudge_breaker::MIN_RENUDGE_INTERVAL`]) reads. See the
    /// `now_fn` field doc. `app.rs` does not need to call this — the
    /// default real wall clock is correct for production.
    #[cfg(test)]
    pub(super) fn with_now_fn(mut self, now_fn: Arc<dyn Fn() -> std::time::Instant + Send + Sync>) -> Self {
        self.now_fn = now_fn;
        self
    }
}
