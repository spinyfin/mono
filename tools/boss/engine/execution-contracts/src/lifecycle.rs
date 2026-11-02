//! Lifecycle services the dispatch coordinator consumes through an injected
//! interface, plus the plain value types those operations share.
//!
//! Production wiring lives in engine core (`lifecycle_adapter`); this crate
//! holds only the contract so a later coordinator extraction does not pull
//! engine-lifecycle modules along with it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use boss_dispatch_events::DispatchEventSink;
use boss_engine_metrics_registry::Registry;
use boss_engine_storage_values::Host;
use boss_protocol::{ExecutionKind, LiveWorkerState, SpawnFailureStreak, WorkExecution};

use crate::{ExecutionPublisher, HostAdapter, HostAdapterProvider};

/// Why the run ended. Proven causes may release without an occupancy scan;
/// inferred death may not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalCause {
    /// Worker finished its turn and the completion path tore the pane down.
    ProvenCompleted,
    /// Pre-start failure: lease was acquired, no worker ever started.
    ProvenNeverStarted,
    /// Explicit stop; the pane and worker process have been confirmed torn down.
    ProvenStopped,
    /// Abandoned, orphaned, idle reap, or liveness timeout — death inferred.
    Inferred,
}

impl TerminalCause {
    pub fn label(self) -> &'static str {
        match self {
            Self::ProvenCompleted => "proven_completed",
            Self::ProvenNeverStarted => "proven_never_started",
            Self::ProvenStopped => "proven_stopped",
            Self::Inferred => "inferred",
        }
    }
}

/// Whether a live worker pane still maps to this execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneStatus {
    Gone,
    Present,
}

/// Whether the recovery bookmark is confirmed written, when the run had work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookmarkStatus {
    NotRequired,
    Confirmed,
    Unconfirmed(String),
}

/// Why lease-release policy kept the lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    BookmarkUnconfirmed,
    PaneStillPresent,
    ProcessInWorkspace,
    OccupancyUnconfirmed,
}

impl KeepReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::BookmarkUnconfirmed => "bookmark_unconfirmed",
            Self::PaneStillPresent => "pane_still_present",
            Self::ProcessInWorkspace => "process_in_workspace",
            Self::OccupancyUnconfirmed => "occupancy_unconfirmed",
        }
    }
}

/// Policy result: release now, or keep the lease (cube TTL remains the backstop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseReleaseDecision {
    Release,
    Keep(KeepReason),
}

/// Outcome of attempting a cube release after policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseReleaseOutcome {
    Released,
    NoLease,
    Kept { reason: KeepReason },
    CubeFailed { error: String },
}

/// How far dispatch got before a pre-start failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreStartStage {
    /// The execution's recovery bookmark has not been written. The workspace
    /// holds only what cube placed there, so there is no worker work to lose.
    BeforeRecoveryBookmark,
    /// Bookmark creation was attempted or has run; its pointers must be
    /// confirmed before the workspace is returned.
    AtOrAfterRecoveryBookmark,
}

/// Why a driver-owned workspace is being torn down.
///
/// Every variant maps to exactly one call site family. Adding a call site
/// means adding a variant — a call site that cannot name itself is a call
/// site nobody can attribute later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeardownReason {
    /// `coordinator::run_execution` — `ExecutionRunner::run_execution`
    /// returned `Err` before any pane existed.
    SpawnFailed,
    /// `coordinator::run_execution` — the run was cancelled while its spawn
    /// was still in flight; the runner already reaped the just-spawned pane.
    CancelledDuringSpawn,
    /// `coordinator::record_run_completion` — the run reached a wait state
    /// that releases the workspace.
    RunCompleted,
    /// `completion::finish_worker_teardown`; the payload is that function's
    /// own `path` label (`"no_op"`, `"pr_review"`, `"idle_park"`, …), so the
    /// completion family stays distinguishable from the sweeps without
    /// flattening which completion path it was.
    Completion(&'static str),
    /// `completion::force_release` — `bossctl agents stop`, cascade cancel,
    /// and every other explicit operator-driven teardown.
    ForceRelease,
    /// `ServerState::reap_run` — an operator reaped this run by hand.
    ManualReap,
    /// Engine-startup app-crash reconciliation.
    AppCrashReconcile,
    /// `host_reconcile` — the run's host went offline (disabled, drained, or
    /// removed from the registry).
    HostDrainReconcile,
    /// `terminal_work_sweep` — a live pane outlived its terminal work item.
    TerminalWorkReconcile,
    /// `dead_pid_sweep` — the worker's tracked pid is gone.
    DeadPidReconcile,
    /// `execution_liveness` — the pane died or never attached
    /// (dead-pane / lost-workspace reconcile).
    PaneLivenessReconcile,
    /// `transient_recovery` — the sweep orphaned the run before resuming it.
    TransientRecoveryReap,
    /// `cube_lease_heartbeat` — the lease failed to refresh often enough to
    /// auto-reap the execution.
    CubeLeaseAutoReap,
    /// `remote_lease_reconcile` — a remote worker process was provably gone.
    RemoteLeaseReconcile,
    /// `stale_worker_sweep` — the worker is alive but wedged past threshold.
    StaleWorkerReconcile,
    /// `spawn_ack_sweep` — driver-start verification found a pane with a
    /// shell but no driver-originated signal within the grace window.
    DriverStartTimeout,
}

impl TeardownReason {
    /// Stable snake_case family label for the `reason` trace field. Pair with
    /// [`Self::detail`], which carries the completion path for
    /// [`Self::Completion`] and is empty for every other variant — keeping
    /// the two apart is what stops a completion path label (a free-form
    /// string) from colliding with a reason name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SpawnFailed => "spawn_failed",
            Self::CancelledDuringSpawn => "cancelled_during_spawn",
            Self::RunCompleted => "run_completed",
            Self::Completion(_) => "completion",
            Self::ForceRelease => "force_release",
            Self::ManualReap => "manual_reap",
            Self::AppCrashReconcile => "app_crash_reconcile",
            Self::HostDrainReconcile => "host_drain_reconcile",
            Self::TerminalWorkReconcile => "terminal_work_reconcile",
            Self::DeadPidReconcile => "dead_pid_reconcile",
            Self::PaneLivenessReconcile => "pane_liveness_reconcile",
            Self::TransientRecoveryReap => "transient_recovery_reap",
            Self::CubeLeaseAutoReap => "cube_lease_auto_reap",
            Self::RemoteLeaseReconcile => "remote_lease_reconcile",
            Self::StaleWorkerReconcile => "stale_worker_reconcile",
            Self::DriverStartTimeout => "driver_start_timeout",
        }
    }

    /// Sub-label for the `reason_detail` trace field: the completion path for
    /// [`Self::Completion`], empty otherwise.
    pub fn detail(self) -> &'static str {
        match self {
            Self::Completion(path) => path,
            _ => "",
        }
    }
}

/// Result of probing whether a pid is still alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidStatus {
    Alive,
    Dead,
    PermissionDenied,
    Unknown,
}

/// Outcome of tearing down the tmux session hosting a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneReleaseOutcome {
    /// A mapped worker slot was found: its pane was torn down and its OS
    /// process tree signalled (SIGTERM, escalating to SIGKILL). The
    /// workspace is no longer occupied, so the caller may free the lease.
    Reaped,
    /// No slot was mapped for the run. Either the worker already released
    /// (idempotent second call) or — the case this distinction exists for
    /// — it is still mid-spawn with no pid yet, so nothing could be
    /// reaped. The caller MUST NOT free the cube lease on this outcome.
    NoLiveWorker,
}

/// Tears down the tmux session hosting `run_id` after verifying its token
/// and detaches its app viewer. Implementations must be idempotent: a duplicate
/// call after the slot has been released is a no-op, not an error.
#[async_trait]
pub trait WorkerPaneReleaser: Send + Sync {
    async fn release_pane(&self, run_id: &str) -> PaneReleaseOutcome;

    /// Remove a retained live entry's roster name before its persona becomes reusable.
    async fn forget_persona_name(&self, _run_id: &str) {}

    /// The owning host has positively confirmed this remote worker is gone.
    async fn release_proven_dead_remote_pane(&self, run_id: &str) -> PaneReleaseOutcome {
        self.release_pane(run_id).await
    }
}

/// `WorkerPaneReleaser` that does nothing — used when no app session
/// release is wired (tests, headless runs). Reports `Reaped` so the
/// lease-release path is unchanged for setups without a pane subsystem.
#[derive(Debug, Default)]
pub struct NoopWorkerPaneReleaser;

#[async_trait]
impl WorkerPaneReleaser for NoopWorkerPaneReleaser {
    async fn release_pane(&self, _run_id: &str) -> PaneReleaseOutcome {
        PaneReleaseOutcome::Reaped
    }
}

/// Per-host context provided to [`select_host`]. The caller builds
/// these from the `hosts` table, capability rows, and the live
/// active-run counter the coordinator already maintains.
#[derive(Debug, Clone)]
pub struct HostSlot {
    pub host: Host,
    /// All capabilities (auto + user) for this host. Compared
    /// string-equality against the required set.
    pub capabilities: BTreeSet<String>,
    /// Workers currently running on this host. Subtracted from
    /// `pool_size` to compute free slots.
    pub active_runs: i64,
    /// Whether a previous run of this execution's PR branch landed
    /// here. Used for the branch-affinity tiebreaker.
    pub had_prior_run_on_branch: bool,
}

impl HostSlot {
    fn free_slots(&self) -> i64 {
        self.host.pool_size.saturating_sub(self.active_runs).max(0)
    }
}

/// Chore-level inputs for host selection.
#[derive(Debug, Clone, Default)]
pub struct ChoreRequirements {
    /// Union of product / project / chore required capabilities.
    pub required_capabilities: BTreeSet<String>,
    /// `work_executions.pinned_host_id`; narrows placement to this host and
    /// intentionally bypasses capability matching.
    pub pinned_host_id: Option<String>,
    /// A one-dispatch `--host` request. Unlike [`Self::pinned_host_id`], this
    /// is an admission constraint and therefore does not bypass capabilities.
    pub requested_host_id: Option<String>,
}

/// Reasons a host can be ineligible. Surfaced as part of the
/// no-eligible-host attention item; structured so the macOS app /
/// bossctl can render a useful per-host explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IneligibilityReason {
    /// Operator turned the host off.
    Disabled,
    /// Pool full (`active_runs >= pool_size`).
    NoFreeSlots,
    /// Capability filter rejected the host.
    MissingCapabilities(Vec<String>),
    /// A driver-constrained row cannot establish whether this host has the
    /// requested driver because discovery has not run yet. This is distinct
    /// from a completed probe that found no matching binary.
    DriverProbeNotRun,
    /// A durable `pinned_host_id` OR a request-scoped `requested_host_id`
    /// is set on the execution and this host is not that host. When both
    /// are set the request-scoped host wins outright — a `--host` request
    /// silently overrides a durable pin for that one dispatch.
    NotSelectedHost,
}

/// Per-host eligibility report from [`select_host`].
#[derive(Debug, Clone)]
pub struct Eligibility {
    pub host_id: String,
    pub eligible: bool,
    pub reasons: Vec<IneligibilityReason>,
    pub free_slots: i64,
    pub had_prior_run_on_branch: bool,
}

/// Compute per-host eligibility and return the picked host id along
/// with the full eligibility list. Returns `Ok(None)` for the picked
/// host when no candidate survives — the coordinator surfaces a
/// `decision_required` attention item with the eligibility list as
/// the body. The caller is responsible for filtering on
/// `pinned_host_id` semantics: when set, only that host can win.
pub fn select_host(requirements: &ChoreRequirements, slots: &[HostSlot]) -> (Option<String>, Vec<Eligibility>) {
    let mut report = Vec::with_capacity(slots.len());
    let mut candidates: Vec<&HostSlot> = Vec::new();

    for slot in slots {
        let mut reasons = Vec::new();
        if !slot.host.enabled {
            reasons.push(IneligibilityReason::Disabled);
        }
        if slot.free_slots() <= 0 {
            reasons.push(IneligibilityReason::NoFreeSlots);
        }
        let selected_pin = requirements
            .requested_host_id
            .as_ref()
            .or(requirements.pinned_host_id.as_ref());
        if let Some(pin) = selected_pin
            && &slot.host.id != pin
        {
            reasons.push(IneligibilityReason::NotSelectedHost);
        }
        let missing: Vec<String> = requirements
            .required_capabilities
            .iter()
            .filter(|cap| !slot.capabilities.contains(*cap))
            .cloned()
            .collect();
        // The durable pin escape hatch intentionally bypasses capabilities;
        // a request-scoped pin is stricter and must meet them.
        if !missing.is_empty() && (requirements.requested_host_id.is_some() || requirements.pinned_host_id.is_none()) {
            let driver_probe_missing = missing.iter().any(|cap| cap.starts_with("driver="))
                && !slot.capabilities.contains(DRIVERS_PROBED_CAPABILITY);
            if driver_probe_missing {
                reasons.push(IneligibilityReason::DriverProbeNotRun);
                let non_driver_missing: Vec<String> =
                    missing.into_iter().filter(|cap| !cap.starts_with("driver=")).collect();
                if !non_driver_missing.is_empty() {
                    reasons.push(IneligibilityReason::MissingCapabilities(non_driver_missing));
                }
            } else {
                reasons.push(IneligibilityReason::MissingCapabilities(missing));
            }
        }
        let eligible = reasons.is_empty();
        report.push(Eligibility {
            host_id: slot.host.id.clone(),
            eligible,
            reasons,
            free_slots: slot.free_slots(),
            had_prior_run_on_branch: slot.had_prior_run_on_branch,
        });
        if eligible {
            candidates.push(slot);
        }
    }

    // Rank candidates. Branch affinity wins outright; among hosts that
    // tie on branch affinity, prefer more free slots; ties on free
    // slots fall through to lexicographic host id (stable, testable).
    let picked = candidates
        .into_iter()
        .min_by(|a, b| {
            // Branch affinity: a host with a prior run sorts *before*
            // one without (so we want false > true under min).
            let aff = b.had_prior_run_on_branch.cmp(&a.had_prior_run_on_branch);
            if aff != std::cmp::Ordering::Equal {
                return aff;
            }
            // More free slots wins (higher slots sorts before).
            let slots = b.free_slots().cmp(&a.free_slots());
            if slots != std::cmp::Ordering::Equal {
                return slots;
            }
            a.host.id.cmp(&b.host.id)
        })
        .map(|s| s.host.id.clone());

    (picked, report)
}

/// Capability tag for an installed agent driver: `driver=<slug>`.
pub fn driver_capability(slug: &str) -> String {
    format!("driver={slug}")
}

/// Marker emitted whenever the driver probe section has run, even when no
/// driver binary was found. Distinguishes "never probed for drivers" from
/// "probed and found none".
pub const DRIVERS_PROBED_CAPABILITY: &str = "drivers-probed=true";

/// Tally from one remote-run reattach pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReattachSummary {
    /// Runs whose reverse events forward was re-established.
    pub reattached: usize,
    /// Runs whose host could not be resolved or whose adapter / forward
    /// re-establishment failed. Logged per-run; the run stays in the DB
    /// and a later pass (or manual `hosts probe`) can retry.
    pub failed: usize,
    /// Runs whose `host_id` no longer exists in the `hosts` table (host
    /// removed since the run started). Counted separately from `failed`
    /// because it is not an error the engine can recover from.
    pub host_missing: usize,
}

impl ReattachSummary {
    /// True when the pass touched at least one detached remote run.
    pub fn had_candidates(&self) -> bool {
        self.reattached + self.failed + self.host_missing > 0
    }
}

/// Counts from one remote-lease reconcile pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RemoteLeaseReconcileOutcome {
    /// Remote runs whose worker was provably dead → reaped + lease released.
    pub reaped: usize,
    /// Remote runs confirmed alive (left running).
    pub alive: usize,
    /// Remote runs we could not adjudicate — no recorded `remote_pid`, an
    /// inconclusive probe (host unreachable), or a host that has since been
    /// removed / whose adapter could not be built. Left ALONE.
    pub skipped: usize,
}

/// The worker posture shared by prompt rendering and engine-owned permission installers.
///
/// New kinds should document their read/write access contract in a comment
/// so reviewers can verify the deny rules match the stated posture.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkerKind {
    /// Normal implementation worker (task, chore, revision, etc.). Has
    /// write access to its leased workspace; can push branches and open
    /// PRs (subject to kind-specific guards such as the revision PR guard).
    #[default]
    Standard,
    /// Read-only reviewer worker (design §9). Reads the PR diff and workspace
    /// files; MUST NOT mutate files, push commits, or interact with GitHub
    /// write endpoints. The engine reviewer deny rules are the
    /// primary enforcement layer for this mandate.
    Reviewer,
    /// Automation triage worker. Investigates the repo and
    /// emits a single decision marker (`automation: task <id>` /
    /// `automation: skip — …`), optionally running one
    /// `boss task create --automation`. It MUST NOT do the work itself: no
    /// file edits, commits, pushes, or PRs — and crucially there is **no PR
    /// deliverable**, so it must not receive the [`WorkerKind::Standard`]
    /// "a PR is the deliverable / print the PR URL as your last line"
    /// CLAUDE.md, which otherwise overrides the marker contract and leaves
    /// the run ending without a decision marker. The engine triage deny rules
    /// enforce the no-work posture; `boss task create`
    /// is intentionally left allowed.
    Triage,
    /// Read-only "mini-coordinator" answer agent (see
    /// `comment-triggered-document-revisions.md`). Answers one reviewer
    /// question in a doc-comment thread. Unlike every other kind it is
    /// enforced with an **allowlist, not a blocklist**: it is spawned in
    /// deny-by-default `dontAsk` permission mode with an explicit
    /// allowlist (read-only tools and the single thread-reply command), plus
    /// comprehensive engine answer-agent deny rules. It reads anything the
    /// coordinator can see and reads code in a
    /// read-only checkout, but MUST NOT edit, push, open/modify a PR, mutate
    /// task/comment/cube state, or take any action other than posting its one
    /// reply. See `answer_agent` for the worker-facing surface.
    AnswerAgent,
    /// PR review-guide generator on the fixed Codex Astra profile. Reads its pinned, read-only workspace and
    /// the embedded source packet and submits Markdown with
    /// `"$BOSS_BIN" propose review-guide --body`. A shared PreToolUse allowlist
    /// allows only source reads and submission; Claude additionally uses dontAsk.
    ReviewGuide,
}

impl WorkerKind {
    /// The permission mode that MUST be forced at spawn (`--permission-mode
    /// <mode>`) for this kind, overriding the model-derived default, if any.
    ///
    /// `Some("dontAsk")` for the capability-restricted answer agent so its
    /// deny-by-default allowlist is authoritative and cannot be silently
    /// downgraded to `auto` (allow-by-default) or `--dangerously-skip-permissions`
    /// (bypasses settings entirely). `None` lets the model-derived default
    /// apply.
    ///
    /// Deriving BOTH the settings posture (via [`worker_kind_for_execution`])
    /// and the forced CLI mode from the same exhaustive `WorkerKind` closes the
    /// divergence footgun where a restricted kind gets a `dontAsk` settings
    /// file but a downgradable CLI flag: the match here is exhaustive, so a new
    /// restricted kind must decide its mode rather than inheriting the
    /// downgradable default.
    pub fn forced_permission_mode(&self) -> Option<&'static str> {
        match self {
            WorkerKind::AnswerAgent | WorkerKind::ReviewGuide => Some("dontAsk"),
            WorkerKind::Standard | WorkerKind::Reviewer | WorkerKind::Triage => None,
        }
    }
}

impl WorkerKind {
    /// Short display label for alerts and bossctl output.
    pub fn label(&self) -> &'static str {
        match self {
            WorkerKind::Standard => "standard",
            WorkerKind::Reviewer => "reviewer",
            WorkerKind::Triage => "triage",
            WorkerKind::AnswerAgent => "answer-agent",
            WorkerKind::ReviewGuide => "review-guide",
        }
    }
}

/// Map a dispatched [`ExecutionKind`] to the [`WorkerKind`] whose permission
/// posture it must run under. The single source of truth shared by the local
/// spawn path (`PaneSpawnRunner`) and the remote spawn path
/// (`host_adapter`) so the two can never diverge — a new execution
/// kind that needs a restricted surface is wired here once.
///
/// Security-critical, so the match is **exhaustive** (no `_` arm): adding a new
/// [`ExecutionKind`] forces a compile error here, making every new kind decide
/// its permission posture explicitly rather than silently inheriting
/// [`WorkerKind::Standard`] (full write/push access).
pub fn worker_kind_for_execution(kind: &ExecutionKind) -> WorkerKind {
    match kind {
        ExecutionKind::PrReview => WorkerKind::Reviewer,
        ExecutionKind::AutomationTriage => WorkerKind::Triage,
        ExecutionKind::AnswerAgent => WorkerKind::AnswerAgent,
        ExecutionKind::PrReviewGuide => WorkerKind::ReviewGuide,
        ExecutionKind::ChoreImplementation
        | ExecutionKind::CiRemediation
        | ExecutionKind::ConflictResolution
        | ExecutionKind::InvestigationImplementation
        | ExecutionKind::ProductDesign
        | ExecutionKind::ProjectDesign
        | ExecutionKind::RevisionImplementation
        | ExecutionKind::TaskImplementation => WorkerKind::Standard,
    }
}

/// Consecutive pre-start failures, with no success in between, at which a
/// (driver, worker kind) combination raises its alert.
pub const PRE_START_FAILURE_STREAK_THRESHOLD: u32 = 2;

/// Driver label used when no driver slug resolves for an execution.
pub const UNRESOLVED_DRIVER_LABEL: &str = "unknown";

/// The combination a streak is tracked for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct StreakKey {
    pub driver: String,
    pub worker_kind: WorkerKind,
}

impl StreakKey {
    pub fn new(driver: impl Into<String>, worker_kind: WorkerKind) -> Self {
        Self {
            driver: driver.into(),
            worker_kind,
        }
    }

    /// Key for an execution of `kind` that resolved to `driver`
    /// ([`UNRESOLVED_DRIVER_LABEL`] when nothing resolved).
    pub fn for_execution(driver: Option<&str>, kind: &ExecutionKind) -> Self {
        let driver = driver
            .map(str::trim)
            .filter(|slug| !slug.is_empty())
            .unwrap_or(UNRESOLVED_DRIVER_LABEL);
        Self::new(driver, worker_kind_for_execution(kind))
    }
}

/// What recording one failure did to its combination's alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureEffect {
    /// Counted, but the streak is still below the threshold: no alert.
    BelowThreshold,
    /// This failure took the streak to the threshold: the alert is new.
    Raised,
    /// An alert was already active and now carries this failure.
    Updated,
}

#[derive(Debug, Clone)]
struct Streak {
    consecutive_failures: u32,
    first_failure_epoch_s: i64,
    latest_failure_epoch_s: i64,
    latest_error: String,
    latest_execution_id: String,
}

/// Per-(driver, worker kind) consecutive pre-start failure counter. One per
/// engine, owned by the coordinator.
///
/// A `std::sync::Mutex` guards the small map and is never held across an
/// `.await`.
#[derive(Debug)]
pub struct PreStartStreakTracker {
    streaks: Mutex<BTreeMap<StreakKey, Streak>>,
    /// Generation counter bumped whenever the set of *active alerts* or
    /// their contents change (raise, in-place update, resolve), so the
    /// health broadcaster can push a fresh report without polling.
    alerts_changed: tokio::sync::watch::Sender<u64>,
}

impl Default for PreStartStreakTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PreStartStreakTracker {
    pub fn new() -> Self {
        Self {
            streaks: Mutex::new(BTreeMap::new()),
            alerts_changed: tokio::sync::watch::channel(0).0,
        }
    }

    /// Record one pre-start spawn failure for `key`. Never drops or
    /// coalesces an occurrence: every call increments the count and
    /// replaces the latest error, so an active alert always shows the
    /// current count and the most recent cause.
    pub fn record_failure(
        &self,
        key: StreakKey,
        execution_id: &str,
        error: &str,
        now_epoch_secs: i64,
    ) -> FailureEffect {
        let effect = {
            let mut streaks = self.streaks.lock().expect("pre-start streak lock poisoned");
            let streak = streaks.entry(key).or_insert_with(|| Streak {
                consecutive_failures: 0,
                first_failure_epoch_s: now_epoch_secs,
                latest_failure_epoch_s: now_epoch_secs,
                latest_error: String::new(),
                latest_execution_id: String::new(),
            });
            streak.consecutive_failures = streak.consecutive_failures.saturating_add(1);
            streak.latest_failure_epoch_s = now_epoch_secs;
            streak.latest_error = error.to_owned();
            streak.latest_execution_id = execution_id.to_owned();
            match streak.consecutive_failures {
                n if n < PRE_START_FAILURE_STREAK_THRESHOLD => FailureEffect::BelowThreshold,
                PRE_START_FAILURE_STREAK_THRESHOLD => FailureEffect::Raised,
                _ => FailureEffect::Updated,
            }
        };
        if effect != FailureEffect::BelowThreshold {
            self.notify_alerts_changed();
        }
        effect
    }

    /// Record one successful spawn for `key`, ending its streak. Returns the
    /// alert this resolved, if one was active — `None` when the combination
    /// had no failures or was still below the threshold.
    pub fn record_success(&self, key: &StreakKey) -> Option<SpawnFailureStreak> {
        let removed = self
            .streaks
            .lock()
            .expect("pre-start streak lock poisoned")
            .remove_entry(key);
        let resolved = removed
            .filter(|(_, streak)| streak.consecutive_failures >= PRE_START_FAILURE_STREAK_THRESHOLD)
            .map(|(key, streak)| to_wire(&key, &streak));
        if resolved.is_some() {
            self.notify_alerts_changed();
        }
        resolved
    }

    /// Every combination currently at or past the threshold, ordered by
    /// (driver, worker kind) so the report is stable between reads.
    pub fn active_alerts(&self) -> Vec<SpawnFailureStreak> {
        self.streaks
            .lock()
            .expect("pre-start streak lock poisoned")
            .iter()
            .filter(|(_, streak)| streak.consecutive_failures >= PRE_START_FAILURE_STREAK_THRESHOLD)
            .map(|(key, streak)| to_wire(key, streak))
            .collect()
    }

    /// Subscribe to alert changes. The receiver starts already-seen, so
    /// `changed()` first resolves on the next raise / update / resolve.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.alerts_changed.subscribe()
    }

    fn notify_alerts_changed(&self) {
        self.alerts_changed.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
    }
}

fn to_wire(key: &StreakKey, streak: &Streak) -> SpawnFailureStreak {
    SpawnFailureStreak::builder()
        .driver(key.driver.clone())
        .worker_kind(key.worker_kind.label())
        .consecutive_failures(streak.consecutive_failures)
        .first_failure_epoch_s(streak.first_failure_epoch_s)
        .latest_failure_epoch_s(streak.latest_failure_epoch_s)
        .latest_error(streak.latest_error.clone())
        .latest_execution_id(streak.latest_execution_id.clone())
        .build()
}

/// Narrow live-worker surface the coordinator (and the pane-death /
/// orphan-sweep corroboration paths that read the coordinator's registry)
/// need, without naming the engine-core registry type.
pub trait LiveWorkerLiveness: Send + Sync {
    fn is_run_live(&self, run_id: &str) -> bool;
    fn snapshot(&self) -> Vec<LiveWorkerState>;
    fn release_slot_for_run(&self, run_id: &str) -> Option<u8>;
    fn release_persona_for_run(&self, run_id: &str);
    fn last_event_at_for_run(&self, run_id: &str) -> Option<String>;
    fn current_tool_for_run(&self, run_id: &str) -> Option<String>;
}

/// Engine lifecycle operations consumed by the dispatch coordinator (lease
/// release, driver teardown, bookmark recovery, liveness reconcile, remote
/// reattach). The core lifecycle adapter implements it by delegating to the
/// owning lifecycle modules.
#[async_trait]
pub trait LifecycleServices: Send + Sync {
    /// Proven-death cube release used by the coordinator's spawn/completion
    /// paths: empty occupancy, DB bookmark gate on `adapter`, then
    /// `release_workspace_with_reset`.
    async fn apply_proven_workspace_release(
        &self,
        execution: &WorkExecution,
        lease_id: &str,
        cause: TerminalCause,
        adapter: &dyn HostAdapter,
    ) -> LeaseReleaseOutcome;

    async fn release_lease_after_proven_never_started(
        &self,
        execution: &WorkExecution,
        lease_id: &str,
        adapter: &dyn HostAdapter,
        stage: PreStartStage,
    ) -> LeaseReleaseOutcome;

    async fn release_refused_workspace(
        &self,
        execution: &WorkExecution,
        lease_id: &str,
        adapter: &dyn HostAdapter,
    ) -> LeaseReleaseOutcome;

    async fn release_lease_after_inferred_death(
        &self,
        execution: &WorkExecution,
        pane: PaneStatus,
        adapter: &dyn HostAdapter,
        reason: &str,
    ) -> LeaseReleaseOutcome;

    async fn teardown_driver_workspace(
        &self,
        execution_id: &str,
        workspace_path: Option<&Path>,
        reason: TeardownReason,
    );

    fn report_bookmark_failure(&self, execution: &WorkExecution, reason: &str);

    fn block_unrecoverable_bookmark(&self, execution: &WorkExecution, err: &anyhow::Error) -> anyhow::Result<bool>;

    async fn reconcile_if_execution_dead(
        &self,
        execution: &WorkExecution,
        dispatch_events: &dyn DispatchEventSink,
    ) -> bool;

    async fn reconcile_if_pane_dead(
        &self,
        execution: &WorkExecution,
        dispatch_events: &dyn DispatchEventSink,
        now_epoch_secs: i64,
        live_states: Option<&dyn LiveWorkerLiveness>,
    ) -> bool;

    fn probe_pid(&self, pid: i32) -> PidStatus;

    async fn reattach_remote_runs(
        &self,
        provider: &dyn HostAdapterProvider,
        engine_events_socket: &str,
    ) -> ReattachSummary;

    async fn reconcile_remote_leases(
        &self,
        provider: &dyn HostAdapterProvider,
        dispatch_events: &dyn DispatchEventSink,
        pane_releaser: Option<&dyn WorkerPaneReleaser>,
    ) -> RemoteLeaseReconcileOutcome;

    fn register_answer_agent_metrics(&self, registry: &Registry);

    fn record_answer_agent_started(&self, registry: &Registry, queue_wait_ms: u64);

    fn record_conflict_class_counter(&self, registry: &Registry, product_id: &str, conflict_class: &str);

    async fn notify_review_guide_changed(
        &self,
        publisher: &Arc<dyn ExecutionPublisher>,
        root_task_id: &str,
        reason: &str,
    );

    fn is_unconfirmed_spawn_abort(&self, err: &anyhow::Error) -> bool;

    async fn verify_deletion_tripwire(
        &self,
        repo_slug: &str,
        head_before: &str,
        base_sha: &str,
        pr_number: u64,
    ) -> Vec<String>;
}

/// Inert implementation for tests that never exercise lifecycle side effects.
/// Lease release reports `NoLease` and teardown does nothing, so it must not
/// be used in production composition.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Default)]
pub struct NoopLifecycleServices;

#[cfg(any(test, feature = "test-support"))]
#[async_trait]
impl LifecycleServices for NoopLifecycleServices {
    async fn apply_proven_workspace_release(
        &self,
        _: &WorkExecution,
        _: &str,
        _: TerminalCause,
        _: &dyn HostAdapter,
    ) -> LeaseReleaseOutcome {
        LeaseReleaseOutcome::NoLease
    }

    async fn release_lease_after_proven_never_started(
        &self,
        _: &WorkExecution,
        _: &str,
        _: &dyn HostAdapter,
        _: PreStartStage,
    ) -> LeaseReleaseOutcome {
        LeaseReleaseOutcome::NoLease
    }

    async fn release_refused_workspace(&self, _: &WorkExecution, _: &str, _: &dyn HostAdapter) -> LeaseReleaseOutcome {
        LeaseReleaseOutcome::NoLease
    }

    async fn release_lease_after_inferred_death(
        &self,
        _: &WorkExecution,
        _: PaneStatus,
        _: &dyn HostAdapter,
        _: &str,
    ) -> LeaseReleaseOutcome {
        LeaseReleaseOutcome::NoLease
    }

    async fn teardown_driver_workspace(&self, _: &str, _: Option<&Path>, _: TeardownReason) {}

    fn report_bookmark_failure(&self, _: &WorkExecution, _: &str) {}

    fn block_unrecoverable_bookmark(&self, _: &WorkExecution, _: &anyhow::Error) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn reconcile_if_execution_dead(&self, _: &WorkExecution, _: &dyn DispatchEventSink) -> bool {
        false
    }

    async fn reconcile_if_pane_dead(
        &self,
        _: &WorkExecution,
        _: &dyn DispatchEventSink,
        _: i64,
        _: Option<&dyn LiveWorkerLiveness>,
    ) -> bool {
        false
    }

    fn probe_pid(&self, _: i32) -> PidStatus {
        PidStatus::Unknown
    }

    async fn reattach_remote_runs(&self, _: &dyn HostAdapterProvider, _: &str) -> ReattachSummary {
        ReattachSummary::default()
    }

    async fn reconcile_remote_leases(
        &self,
        _: &dyn HostAdapterProvider,
        _: &dyn DispatchEventSink,
        _: Option<&dyn WorkerPaneReleaser>,
    ) -> RemoteLeaseReconcileOutcome {
        RemoteLeaseReconcileOutcome::default()
    }

    fn register_answer_agent_metrics(&self, _: &Registry) {}

    fn record_answer_agent_started(&self, _: &Registry, _: u64) {}

    fn record_conflict_class_counter(&self, _: &Registry, _: &str, _: &str) {}

    async fn notify_review_guide_changed(&self, _: &Arc<dyn ExecutionPublisher>, _: &str, _: &str) {}

    fn is_unconfirmed_spawn_abort(&self, _: &anyhow::Error) -> bool {
        false
    }

    async fn verify_deletion_tripwire(&self, _: &str, _: &str, _: &str, _: u64) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::ExecutionKind;

    #[test]
    fn driver_capability_uses_slug_tag() {
        assert_eq!(driver_capability("codex"), "driver=codex");
    }

    #[test]
    fn streak_key_maps_execution_kind_labels() {
        assert_eq!(
            StreakKey::for_execution(Some("codex"), &ExecutionKind::PrReviewGuide)
                .worker_kind
                .label(),
            "review-guide"
        );
        assert_eq!(
            StreakKey::for_execution(Some("claude"), &ExecutionKind::PrReview)
                .worker_kind
                .label(),
            "reviewer"
        );
        assert_eq!(
            StreakKey::for_execution(None, &ExecutionKind::TaskImplementation).driver,
            UNRESOLVED_DRIVER_LABEL
        );
        assert_eq!(
            StreakKey::for_execution(Some("  "), &ExecutionKind::AutomationTriage)
                .worker_kind
                .label(),
            "triage"
        );
    }
}
