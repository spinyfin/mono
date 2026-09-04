//! Structured, file-backed log of every step in the dispatch
//! pipeline — `RequestExecution` ↦ pane bound to slot — so a silent
//! failure between any two stages can be diagnosed after the fact
//! without re-deriving state.
//!
//! The pipeline is described in detail in
//! [`engine-dispatch-instrumentation.md`]. This module is the
//! minimum production sink that the coordinator and spawn flow can
//! emit into today; downstream phases of that design (CLI verbs,
//! stage-stalled detector, topic broadcast) layer on top.
//!
//! Files live under the existing Boss state root so they survive
//! engine restarts and never share fate with `events.sock` (the
//! engine's *other* stream, which is itself one of the failure
//! modes operators may be diagnosing):
//!
//! ```text
//! boss-state-root/
//!   dispatch-events/
//!     current.jsonl                  # live source-of-truth flat stream
//!     current.jsonl.<unix_seconds>   # rotated segments, oldest first
//!   executions/<execution-id>/
//!     dispatch.jsonl           # mirror of just this execution's lines
//! ```
//!
//! Writers are best-effort: a write that fails to land on disk logs
//! once via `tracing::warn!` and is dropped. Dispatch is never
//! blocked on event emission.
//!
//! `current.jsonl` rotates once it crosses [`JsonlFileSink`]'s size
//! threshold (`DEFAULT_CURRENT_MAX_BYTES`, overridable via
//! `BOSS_DISPATCH_EVENTS_MAX_BYTES`); rotated segments beyond
//! `DEFAULT_CURRENT_MAX_FILES` (`BOSS_DISPATCH_EVENTS_MAX_FILES`) are pruned
//! oldest-first. The per-execution mirrors are not rotated. Readers that
//! need the full flat stream (`boss_dispatch_reader::read_current`,
//! `TimelineIndex`) span rotated segments plus the live file via
//! `boss_log_files::segments_with_live` / `rotated_segments`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use boss_engine_jsonl_append::JsonlAppender;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Synthetic execution id for dispatch events that describe engine boot rather
/// than a single durable execution.
pub const ENGINE_BOOT_EXECUTION_ID: &str = "engine-boot";

/// One step of the dispatch pipeline. Stage values are stable strings
/// so external tooling (`jq`, future bossctl verbs) can pin against
/// them. Spelled provisionally for now — the schema in
/// `engine-dispatch-instrumentation.md` may subsume these names when
/// the full design ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// `UpdateWorkItem` observed a `tasks.status` transition that
    /// would normally trigger auto-dispatch (drag-to-Doing path
    /// from #345). Fires whether or not the dispatch attempt
    /// actually ran — the `details.did_dispatch` flag distinguishes
    /// the two cases. Before this stage existed, a status flip that
    /// fell through the `work_item_needs_dispatch` gate produced no
    /// event at all and the symptom presented as "I dragged it and
    /// nothing happened."
    StatusTransition,
    /// Scheduler picked the execution off the ready queue and is
    /// about to attempt to claim a worker.
    RequestRecorded,
    /// Worker pool returned a free slot (or skipped because every
    /// slot was busy).
    WorkerClaimed,
    /// The dispatch picked the host this execution will run on (and
    /// built its host adapter). Emitted between `worker_claimed` and
    /// `cube_repo_ensured` to close a silent gap: before this stage
    /// existed, the work-item resolution, host pick, and adapter build
    /// that happen right after a claim produced NO event, so when any
    /// of them failed — `no_eligible_host`, `host_adapter_unavailable`,
    /// or an unresolved work item — the per-execution timeline went
    /// silent after `worker_claimed` and the stall watchdog reaped the
    /// execution ~30s later mislabelling `stalled_stage="worker_claimed"`
    /// (see the automation-pool stall, 2026-06-03). `outcome=ok` carries
    /// the chosen `host_id` in `details`; `outcome=error` carries a
    /// `reason` (`work_item_unresolved` / `no_eligible_host` /
    /// `host_adapter_unavailable`) so a diagnose verb names the real
    /// blocker instead of pointing at the claim.
    HostSelected,
    /// Engine is about to call `cube repo ensure`. Emitted *before* the
    /// subprocess (same rationale as `cube_workspace_lease_attempted`):
    /// `cube repo ensure` on a cold/large repo can run for tens of
    /// seconds, and if it exceeds the `worker_claimed` stall threshold
    /// before returning, the watchdog would otherwise blame the claim.
    /// With this marker the stall is attributed to the repo-ensure
    /// subprocess. `details` carries the origin URL and the timeout.
    CubeRepoEnsureAttempted,
    /// `cube repo ensure` returned a repo handle. Always `outcome=ok` —
    /// a failed or timed-out attempt emits [`Stage::CubeRepoEnsureFailed`]
    /// instead (see that variant for why the split exists).
    CubeRepoEnsured,
    /// `cube repo ensure` failed or timed out. Split out from
    /// `cube_repo_ensured` (which used to carry `outcome=error` for this
    /// case) because a success-shaped stage name with an error attached
    /// reads as a passing stage to anyone `jq`-filtering or skimming
    /// `dispatch.jsonl` — exactly the "Waiting for a slot" incident this
    /// fixes: the ssh `command not found: cube` failure was recorded
    /// under `cube_repo_ensured`, so nothing in the timeline *looked*
    /// like a failure. `error_message` carries the verbatim failure
    /// (ssh/cube error text, or the timeout message).
    CubeRepoEnsureFailed,
    /// Engine is about to call `cube workspace lease`. Emitted *before*
    /// the subprocess invocation so an operator can see what the
    /// engine intended to do (preferred workspace id, fallback
    /// policy) even if the cube call itself hangs and never returns.
    /// The motivating incident hit this exact gap — the engine had
    /// claimed a worker, made the cube call, and then sat silent for
    /// ~46 seconds with no event between `worker_claimed` and the
    /// next stage. Adding an explicit "attempted" record means
    /// `bossctl dispatch diagnose` can show "lease was attempted with
    /// these inputs but the subprocess never came back."
    CubeWorkspaceLeaseAttempted,
    /// `cube workspace lease` returned a lease.
    CubeWorkspaceLeased,
    /// `cube workspace lease` failed (cube returned an error, the
    /// engine timed out the subprocess, or any other reason the
    /// preceding `cube_workspace_lease_attempted` did not progress to
    /// `cube_workspace_leased`). The `error_message` field carries
    /// the verbatim cube stderr / timeout message so a diagnose verb
    /// can render the reason without going back to tracing logs.
    /// Distinct from `cube_workspace_leased` with `outcome=error` so
    /// readers don't have to disambiguate by outcome.
    CubeWorkspaceLeaseFailed,
    /// `cube workspace goto` succeeded — `@` is now positioned as an editable
    /// child commit atop the PR branch head. Emitted for dispatch paths that
    /// target an existing PR (RevisionImplementation, PrReview, etc.).
    CubeWorkspacePositioned,
    /// `cube workspace goto` failed. The `error_message` field carries the
    /// verbatim cube stderr so `bossctl dispatch diagnose` can render the
    /// root cause without a log dive. Positioning failure aborts dispatch.
    CubeWorkspacePositioningFailed,
    /// `cube change create` returned a change handle.
    CubeChangeCreated,
    /// `start_execution_run` committed and `tasks.status` flipped
    /// to `active`.
    RunStarted,
    /// `SpawnWorkerPane` returned ok / error. This is the stage
    /// whose silent failure motivated the structured stream:
    /// before this fix landed, a spawn failure marked the run
    /// `failed` and released the lease without surfacing anything
    /// to the user.
    PaneSpawned,
    /// An execution that had already entered the dispatch pipeline was
    /// cancelled by an engine lifecycle transition. `details.reason` names
    /// the transition and `details.prior_status` records the status that was
    /// terminalized. The merge poller emits this when a revision's parent PR
    /// merges mid-run, so the execution timeline does not silently stop at
    /// `pane_spawned` even though the durable row says `cancelled`.
    ExecutionCancelled,
    /// The dispatch aborted between `run_started` and any pane existing:
    /// `ExecutionRunner::run_execution` returned `Err` — a prompt-composition
    /// failure, a driver provision/permission-config failure, an
    /// undeliverable `SpawnWorkerPane` RPC, anything upstream of the app
    /// actually being asked for a pane.
    ///
    /// Emitted the INSTANT the abort is observed, ahead of driver teardown
    /// and ahead of the (unbounded) `cube workspace release` that follows —
    /// which is the entire reason it exists separately from
    /// [`Stage::PaneSpawned`] with `outcome=error`. That event is emitted
    /// only inside the success branch of `finish_execution_run`, which sits
    /// behind an unbounded cube release and is skipped entirely if a cancel
    /// lands first and rejects the terminalizing write. Without this stage,
    /// that double-fault leaves a per-execution timeline that simply STOPS at
    /// `run_started` with the spawn error discarded unlogged. This stage is
    /// the terminal marker that survives it: `error_message` carries the full
    /// `{err:#}` chain, and `details` carries `run_id` and `slot_id`.
    ///
    /// A healthy dispatch never emits it — `pane_spawned` is still the
    /// success terminal, and `pane_spawned/error` still follows this one when
    /// the tail of the abort path completes normally (it carries what only
    /// becomes knowable later: `released_workspace` and the `slot_busy`
    /// occupant). Readers keying off `pane_spawned` — `bossctl doctor`'s
    /// SIG-B/SIG-F/SIG-H signatures — are unaffected.
    SpawnFailed,
    /// The completion handler finished tearing down an execution it had
    /// just terminalized — pane released, driver state torn down, cube
    /// lease released (or its release timed out and was abandoned to cube's
    /// TTL). Emitted from the one teardown choke point every completion
    /// route funnels through (`completion::teardown::finish_worker_teardown`),
    /// so it fires for a PR-producing completion, a declared no-op
    /// completion, a pr_review / automation / answer-agent finalize, an
    /// idle park, and a driver-reported terminal error alike; `details.path`
    /// names which. It is always emitted AFTER the terminalizing DB write.
    ///
    /// This is the dispatch timeline's explicit end-of-run marker. Dispatch
    /// itself ends at `pane_spawned` (or `tmux_adopt` after an engine
    /// restart), but post-dispatch observations keep being appended to the
    /// same per-execution timeline for as long as the run lives; without a
    /// record of the run actually ending, `bossctl dispatch diagnose` can
    /// only infer completion from the absence of further events. `details`
    /// carries `path`, `pane_outcome`, `released_lease`, `cube_timed_out`,
    /// and the per-step teardown timings.
    ExecutionFinalized,
    /// A non-terminal stage exceeded its per-stage stalled-threshold
    /// without progressing to the next stage. Fires periodically
    /// from the engine's stage-stalled detector; surfaces via
    /// `bossctl dispatch ghost-active --include-stalled`. Does NOT
    /// auto-remediate — the operator decides whether to retry,
    /// reap, or wait.
    StageStalled,
    /// The periodic orphan-active sweep found a work item in `active`
    /// status with no live execution and inserted a fresh `ready`
    /// execution to drive it back into the dispatch pipeline. Distinct
    /// from `status_transition` (which fires on kanban drags) so
    /// `bossctl dispatch tail` can filter orphan-sweep redispatches
    /// separately from human-initiated ones.
    OrphanActiveRedispatch,
    /// The periodic pr_review dead-review recovery sweep
    /// (`pr_review_recovery`) found a `pr_review` execution that reached a
    /// terminal state without ever finalizing (host failure, cube-lease
    /// reap, crash) and re-enqueued a fresh `pr_review` execution against
    /// the same PR. `details` carries the dead execution's id and status.
    /// Distinct from `orphan_active_redispatch`: that sweep explicitly
    /// excludes these items (it has no notion of `pr_review` as a kind and
    /// would wrongly redispatch an implementer) so this stage's presence
    /// is the sole signal that a review was auto-recovered.
    PrReviewDeadRecovery,
    /// The periodic dead-PID sweep found a claimed worker slot whose
    /// backing OS process is gone (ESRCH from `kill(pid, 0)`). The
    /// execution has been marked `orphaned`, the pool slot released,
    /// and the work item will be redispatched by the orphan sweep on
    /// the next tick. Distinct from `orphan_active_redispatch` so
    /// operators can distinguish "slot claimed but PID dead" from
    /// "slot not claimed at all."
    DeadPidReconcile,
    /// The periodic pane-death sweep (`boss_engine::dead_pane_sweep`) found a
    /// still-`running`/`waiting_human` local execution whose worker pane is
    /// provably gone — its durable shell pid (persisted from the app's
    /// `UpdateWorkerShellPid`) reports `ESRCH` from `kill(pid, 0)`. Unlike
    /// `dead_pid_reconcile`, which reads the in-memory live-worker registry
    /// (empty after any engine restart), this probes the DB-persisted pid, so
    /// it catches a pane that died *with its host app* across a relaunch —
    /// the 2026-07-04 wedge where a triage worker ran normally, an app/engine
    /// relaunch killed its pane mid-run, and the row then sat `waiting_human`
    /// with a still-green cube lease and no pane forever, permanently blocking
    /// the redundant-spawn guard. The execution is marked `orphaned`
    /// (workspace/lease preserved for resume redispatch) and the work item is
    /// redispatched on the next tick. The `details` object carries the dead
    /// `shell_pid` and `prior_status`.
    PaneDeathReconcile,
    /// A dispatch *trigger* loop (orphan-active sweep, startup
    /// reconcile, worker-release rescan, kanban drag) evaluated whether
    /// a work item needs a fresh dispatch. Emitted UPSTREAM of
    /// `request_recorded` — `request_recorded` only ever fires once the
    /// scheduler has already decided to dispatch, so the decision that
    /// *produced* the request was previously invisible. The `details`
    /// object carries the loop name, the predicate it keyed off, and —
    /// critically — the live execution the loop found (or failed to
    /// account for) so a re-dispatch storm can be traced back to the
    /// loop that re-fired despite a healthy live run. See
    /// `task_18b347260cd7da80_e` (the R693 re-dispatch storm).
    DispatchDecision,
    /// The transient-recovery sweep detected a worker that stalled or
    /// died with a *transient* Claude API error as the last entry in
    /// its transcript and auto-resumed it on the same workspace. The
    /// `details` object carries `attempt`, `max_attempts`, the error
    /// `class`, and a clipped `error` string so `bossctl dispatch tail`
    /// shows "recovering, attempt 2/3" without a log dive.
    TransientRecovery,
    /// The transient-recovery sweep stopped retrying a worker and
    /// raised a `WorkAttentionItem` instead — either the error was
    /// non-retryable (permanent / unrecognised) or the retry cap was
    /// reached. The `details` object carries the escalation `reason`.
    TransientRecoveryExhausted,
    /// The transient-recovery sweep sent a runtime nudge to a live idle
    /// worker rather than tearing it down. The worker's `claude` process
    /// is still alive at its REPL and can receive input; a nudge is
    /// cheaper than orphan+respawn. If the nudge does not clear the error
    /// by the next sweep the sweep falls back to the normal
    /// orphan+respawn path.
    TransientRecoveryNudge,
    /// The periodic stale-worker sweep found a slot whose `claude`
    /// process is still alive but has emitted no hook event for longer
    /// than the staleness threshold while `activity=working` with no
    /// tool in flight — the wedged-dependency hang (e.g. a backgrounded
    /// bazel build the worker is idling on that never completes). The
    /// execution has been marked `orphaned`, the pool slot released, and
    /// the work item will be redispatched by the orphan sweep on the
    /// next tick. Distinct from `dead_pid_reconcile` (PID gone) because
    /// here the process is *alive but parked* — `kill(pid, 0)` would
    /// report it healthy.
    StaleWorkerReconcile,
    /// The periodic pool-claim reconciler found a worker-pool slot still
    /// claimed by an execution that is terminal in the DB and has no live
    /// worker pane backing it, and released the claim. This is the
    /// backstop for the leak that wedged the automation pool: every other
    /// slot-releasing path (completion's `release_worker_pane`, the
    /// dead-pid / stale-worker / transient-recovery sweeps) keys off a
    /// live `LiveWorkerStateRegistry` entry, so a claim whose backing
    /// execution terminated WITHOUT a live pane (mid-spawn cancel,
    /// `finalize_pr_transition` DB error, a teardown that dropped the
    /// run→slot mapping but not the pool claim) was released by nothing
    /// and outlived its execution forever. The `details` object carries
    /// the leaked `worker_id`, the terminal `execution_status`, and the
    /// `pool` name so a leak is diagnosable from `bossctl dispatch tail`
    /// without grepping engine logs. Distinct from `dead_pid_reconcile`
    /// (slot has a live-state entry whose PID is gone) — here the slot
    /// has NO live-state entry at all.
    PoolClaimReconcile,
    /// The periodic terminal-work reconciler found a LIVE worker pane whose
    /// bound work item (or its execution) is already terminal — the
    /// O'Brien zombie: a worker that sat alive in `waiting_for_input` for
    /// days after its task went `done` and its PR merged, holding a slot
    /// long after its work was finished. Every other reconciler skips this
    /// case: the dead-pid sweep needs a dead PID *and* a non-terminal
    /// status, the stale-worker sweep only inspects `working` slots, the
    /// transient-recovery sweep recovers unfinished work (never checks
    /// work-item terminality), and the pool-claim sweep deliberately leaves
    /// live-backed claims to the completion path. When that completion
    /// teardown never lands (laptop closed, API call wedged), the worker is
    /// stranded. The `details` object carries the reap `reason`
    /// (`work_item_terminal` / `execution_terminal`), the `slot_id`, and the
    /// terminal `execution_status` so the strand is diagnosable from
    /// `bossctl dispatch tail`. Reaping uses the same idempotent,
    /// run-id-keyed `release_worker_pane` teardown the completion path uses,
    /// so a slot recycled to a different execution between snapshot and reap
    /// is a no-op — an active worker is never reaped.
    TerminalWorkReconcile,
    /// The periodic cube-lease heartbeat sweep tried to refresh the TTL on a
    /// live worker's cube workspace lease and the `cube workspace heartbeat`
    /// call failed (e.g. the lease was reclaimed out from under us, or cube
    /// errored). Emitted ONLY on failure — routine successful heartbeats are
    /// logged, not evented, because every live worker is refreshed every
    /// interval and per-success events would bloat the stream. A run of these
    /// for one lease means cube and the engine have desynced for that
    /// workspace; see `boss_engine::cube_lease_heartbeat`.
    CubeLeaseHeartbeat,
    /// The lost-workspace reconciler finalized a non-terminal execution
    /// (`running` / `waiting_human` / …) whose recorded local cube
    /// workspace directory no longer exists on disk. That directory is a
    /// live worker's cwd for the lifetime of its pane; if it has vanished
    /// (e.g. the 2026-06-14 cube workspace-root migration relocated the
    /// pool out from under running triage panes), the worker is gone and
    /// the row must not keep counting as "live" — otherwise the
    /// redundant-spawn guard blocks every future spawn for that work item.
    /// The `details` object carries `prior_status`, the missing
    /// `workspace_path`, and the reap `reason` so the strand is diagnosable
    /// from `bossctl dispatch tail`; see `boss_engine::lost_workspace_sweep`.
    LostWorkspaceReconcile,
    /// The cube-lease heartbeat sweep gave up refreshing a lease after
    /// `boss_engine::cube_lease_heartbeat::AUTO_REAP_AFTER_CONSECUTIVE_FAILURES`
    /// consecutive failures and auto-reaped the execution through the same
    /// terminal path as `bossctl agents reap` (`mark_execution_orphaned`).
    /// Before this existed, a lease cube no longer tracked (workspace
    /// directory still present, so `lost_workspace_reconcile` never fired)
    /// produced only an endless stream of `cube_lease_heartbeat` warnings
    /// while the row stayed `waiting_human`/`running` forever, permanently
    /// blocking the redundant-spawn guard for that work item (2026-07-03
    /// incident). The `details` object carries `consecutive_failures` so an
    /// operator can see how long the lease had been failing before the
    /// auto-reap fired.
    CubeLeaseAutoReap,
    /// The periodic remote-lease reconciler (`boss_engine::remote_lease_reconcile`)
    /// found a still-`waiting_human`/`running` execution on a remote SSH host
    /// whose worker process was provably gone (a `kill -0` over the host's
    /// `ControlMaster` reported no such process), reaped the execution through
    /// the terminal `mark_execution_orphaned` path, and force-released its cube
    /// lease on the remote so the stranded workspace (and its multi-GB clone)
    /// is reclaimed. This is the cross-host analogue of `lost_workspace_reconcile`
    /// (a local `.exists()`/pid probe is meaningless for a remote worker). The
    /// `details` object carries `prior_status`, the `remote_pid`, and the
    /// reap `reason` so the strand is diagnosable from `bossctl dispatch tail`.
    RemoteLeaseReconcile,
    /// The host-reconcile sweep (`boss_engine::host_reconcile`) terminalized a
    /// non-terminal execution whose latest run was bound to a host that is
    /// now offline — disabled by the operator (`bossctl hosts disable`),
    /// auto-disabled by the dispatch-health circuit breaker, or removed
    /// from the registry. Before this existed, disabling a host removed it
    /// only from *future* `select_host` picks; anything already routed to
    /// it stayed stuck (queued / leased / run-started / heartbeat-erroring)
    /// with no re-route, and an operator had to reap each phantom by hand
    /// (2026-07-03 anaplian incident). The execution is marked `orphaned`
    /// (same terminal path as `bossctl agents reap`) and its cube lease
    /// best-effort released; the existing orphan-active sweep then
    /// re-dispatches the work item to a still-eligible host. The `details`
    /// object carries the offline `host_id` and the reap `reason`
    /// (`host_disabled` / `host_removed`) so the re-route is diagnosable
    /// from `bossctl dispatch tail`.
    HostDrainReconcile,
    /// The periodic spawn-ack sweep (`boss_engine::spawn_ack_sweep`) found a
    /// slot stuck in `Spawning` that never reported a shell pid AND never
    /// received a single hook event, past the grace window — proof no
    /// worker process ever came up at all, not merely one blocked on the
    /// interactive directory-trust prompt (which `mark_stalled_spawns`
    /// still handles, and which always has a pid). The execution has been
    /// marked `orphaned`, the app's pane torn down, the pool slot
    /// released, and the work item will be redispatched by the orphan
    /// sweep on the next tick. This is the fix for the 2026-07-03/04
    /// false-live incident, where such a slot instead sat at
    /// `activity=waiting_for_input, shell_pid=0` forever, requiring a
    /// human to notice and manually reap it. Distinct from
    /// `dead_pid_reconcile` (a pid WAS observed, then the process died)
    /// and `stale_worker_reconcile` (a pid is alive but wedged after
    /// reaching `working`) — here no pid was ever observed at all. The
    /// `details` object carries `shell_pid` (always `0`) and the
    /// `threshold_secs` grace window that elapsed.
    SpawnAckTimeout,
    /// A worker pane came up, but no **driver-originated** signal — a hook
    /// event or a `transcript_path` — ever arrived, so the driver binary
    /// itself never executed. The execution is marked `orphaned`, the pane
    /// torn down, the pool slot released, the cube workspace lease
    /// force-released, and an attention item raised.
    ///
    /// Distinct from [`Stage::SpawnAckTimeout`] in exactly the way that
    /// matters: that stage fires only when NOTHING reported in (`shell_pid`
    /// is always `0`), whereas here a shell pid is typically alive and
    /// healthy-looking. It is the login shell hosting the pane, not the
    /// driver. That distinction is the 2026-07-30 incident: the pid made the
    /// slot pass `spawn_ack_sweep` (which skipped `shell_pid > 0`),
    /// `mark_stalled_spawns` (which skips drivers without
    /// `Capability::AwaitingInputSignal`), and `dead_pid_sweep` (whose
    /// `kill(pid, 0)` found the shell alive), so the slot and its cube lease
    /// were held indefinitely with no attention item.
    ///
    /// The `details` object carries `slot_id`, the (possibly live)
    /// `shell_pid`, the `threshold_secs` window that elapsed, `silent_secs`,
    /// and the `activity` the slot was advertising.
    DriverStartTimeout,
    /// The periodic dispatch-failure-recovery sweep found a work item the
    /// engine had bounced to Backlog after a pre-spawn dispatch failure
    /// exhausted its immediate retries (`bounce_dispatch_failed_to_backlog`
    /// — `autostart` cleared, `dispatch_failed_reason` set) and re-enqueued
    /// it after a cooldown. This is the pre-spawn-failure sibling of
    /// `orphan_active_redispatch` / `cube_lease_auto_reap`, which only
    /// self-heal failures *after* `run_started`; before this stage existed
    /// a pre-spawn failure that exhausted its retries stayed parked until
    /// a human ran `bossctl work start` (2026-07-03 incident: sat
    /// undispatched 45+ minutes with free slots available). See
    /// `boss_engine::dispatch_failure_recovery_sweep`.
    DispatchFailureRecoveryRedispatch,
    /// The app proactively reported — via `ReportWorkerSpawnFailed` — that a
    /// worker pane's shell never came up because the libghostty surface
    /// failed to create (typically `ghostty_surface_new` returning NULL when
    /// there is no active display after sleep/wake). Unlike
    /// [`Stage::SpawnAckTimeout`], which the periodic sweep infers from 60s
    /// of total silence, this fires the instant the app tells us, so the
    /// execution is reaped and the slot freed in seconds rather than after
    /// the grace window. The reap path is identical (orphan → pane teardown →
    /// slot release), and both feed the same spawn-capability circuit breaker
    /// (see [`Stage::SpawnCapabilityUnhealthy`]). The `details` object carries
    /// the app-supplied `reason` and the `slot_id`.
    SpawnNack,
    /// The app reported a worker pane died (`WorkerPaneDied`) for a slot that
    /// had never shown any proof of life — no shell pid, no hook event, still
    /// advertising `Spawning`. The pane did not *die*; it never came up, so
    /// the execution is reaped through the never-started-spawn path (orphan →
    /// driver teardown → pane teardown → slot release → cube lease release)
    /// and feeds the same spawn-capability circuit breaker as
    /// [`Stage::SpawnNack`] and [`Stage::SpawnAckTimeout`].
    ///
    /// Distinct from [`Stage::DeadPidReconcile`] / [`Stage::PaneDeathReconcile`],
    /// which handle a pane that died *after* hosting a live worker. That
    /// distinction is the 2026-07 no-active-display incident: a surface that
    /// `ghostty_surface_new` refused to create was reported as a pane death,
    /// so it took the death path — which does not feed the cross-work-item
    /// breaker — and the diagnostic `ReportWorkerSpawnFailed` NACK that
    /// followed found the slot already released and was dropped as stale.
    /// 818 executions across 79 work items churned because no single work
    /// item reached its own churn threshold and the one aggregator that
    /// would have caught it was never fed.
    ///
    /// The `details` object carries `slot_id`, `shell_pid` (always `0`), and
    /// the app-supplied `detail` describing what it observed.
    PaneDeathBeforeStart,
    /// The app-spawn-capability circuit breaker tripped: too many worker-pane
    /// spawns failed across DIFFERENT work items within a short window
    /// (`ReportWorkerSpawnFailed` NACKs and/or `spawn_ack_timeout` reaps),
    /// proving the app session's spawn path — not any one work item — is
    /// broken. This is the fix for the 2026-07-05 post-wake wedge, where
    /// every pane spawn silently produced no shell for 1.5+ hours and the
    /// per-work-item churn guard could not catch it because the failures
    /// were spread across many items. The engine always raises a single
    /// `app_spawn_capability_unhealthy` attention item as ONE loud signal,
    /// instead of independently churning each work item into its own churn
    /// guard — but whether it also **pauses dispatch** depends on
    /// `boss_engine::config::WorkConfig::enable_spawn_capability_breaker`
    /// (`BOSS_ENABLE_SPAWN_CAPABILITY_BREAKER`, ON by default — see
    /// `boss_engine::config::DEFAULT_ENABLE_SPAWN_CAPABILITY_BREAKER` for the
    /// 2026-07-15 incident that briefly defaulted it off and why it is safe
    /// to default on again). When enabled, dispatch stays paused
    /// until either the half-open recovery probe or a fresh app session
    /// registering auto-resumes it (see
    /// [`Stage::SpawnCapabilityRecovered`] and
    /// `boss_engine::spawn_health::maybe_admit_recovery_probe`); when disabled,
    /// this event fires as observability only. The `details` object carries
    /// `distinct_work_items`, `window_secs`, `breaker_enabled`, and
    /// `dispatch_paused` (mirrors `breaker_enabled`).
    SpawnCapabilityUnhealthy,
    /// Dispatch auto-resumed after Breaker-origin evidence that the app's
    /// spawn path recovered — either the half-open recovery probe's canary
    /// (see `boss_engine::spawn_health::maybe_admit_recovery_probe`) reported a
    /// real shell pid, or a fresh app session registered (an app relaunch,
    /// the operator's natural recovery action). Never fired for an
    /// operator-originated pause, which stays manual-resume-only. The
    /// `details` object carries the human-readable `reason`; the event's
    /// `execution_id` is the canary's id when the probe succeeded, or the
    /// sentinel `"engine"` for a fresh-session recovery with no specific
    /// execution behind it.
    SpawnCapabilityRecovered,
    /// The periodic husk-pane sweep (`boss_engine::husk_pane_sweep`) found a
    /// Boss-owned tmux session whose durable spawn token has no DB row,
    /// confirmed it across two consecutive passes, and destroyed the session.
    /// The same server-wide inventory first re-enters non-terminal sessions
    /// into adoption and routes terminal rows through `worker_readoption`, so
    /// only a truly unknown token reaches this reap event. This is the
    /// general backstop for a session that survives after engine bookkeeping
    /// has been cleared. Tmux is the physical inventory authority; two-pass
    /// confirmation protects against a fresh durable write racing the sweep.
    /// The `details` object carries the retired `tmux_session_name`.
    HuskPaneReconcile,
    /// The execution-liveness reconciler finalized a non-terminal LOCAL
    /// execution whose worker pane never reported a shell pid and has been
    /// running past the pane-attach deadline (a spawn that stalled before
    /// `pane_spawned`) — see `boss_engine::lost_workspace_sweep`. This is the gap
    /// `lost_workspace_reconcile` (workspace dir still on disk),
    /// `dead_pane_sweep` (only ever probes a pid that was actually reported),
    /// and `cube_lease_auto_reap` (the engine's own DB-fallback heartbeat kept
    /// the lease alive, so it never failed) all miss — the 2026-07-03 zombies
    /// that survived the earlier dead-pane-sweep fix. A recorded pid that is now dead is a
    /// separate signal owned exclusively by `dead_pane_sweep`, which emits
    /// `Stage::PaneDeathReconcile` instead. The `details` object carries
    /// `reason` (`pane_never_attached`), `prior_status`, `age_in_status_secs`,
    /// and the observed `shell_pid` (always absent) so a recurrence is
    /// attributable from `bossctl dispatch tail` in one read; see
    /// `boss_engine::execution_liveness::classify_pane_liveness`.
    ExecutionLivenessReconcile,
    /// Dispatch (and, when `details.reviews_held` is `true`, `pr_review`)
    /// was paused — either an operator toggled `bossctl dispatch pause` /
    /// the app's pause switch, or the spawn-capability circuit breaker
    /// tripped in enabled mode (see [`Stage::SpawnCapabilityUnhealthy`]).
    /// This is the durable audit record `bossctl dispatch state --history`
    /// reads: before this existed, an operator pause left only a one-line
    /// `tracing::info!`, and a breaker pause's full evidence (which
    /// executions/work items/slots failed to spawn, over what window,
    /// against which threshold) lived only in the `spawn_capability_unhealthy`
    /// event's `details` — nothing correlated "dispatch is paused right now"
    /// back to *why*, retrievable after the fact (2026-07-15 incident: the
    /// only surviving explanation of a ~40-minute fleet-wide pause was the
    /// one-line `bossctl dispatch state` reason string). The `details`
    /// object always carries `origin` (`"operator"` / `"breaker"`), `actor`
    /// (`"operator"` / `"breaker"`), `paused_since_epoch_s`, `reviews_held`,
    /// and `scope` (the pause targets, e.g. `["dispatch", "reviews"]`); a
    /// breaker-origin pause additionally carries `trigger` — the rule that
    /// fired (`threshold`, `window_secs`, `distinct_work_items`) and the
    /// concrete `triggering_events` (execution id, work item id, slot id,
    /// shell pid, timestamp) that tripped it.
    DispatchPaused,
    /// Dispatch resumed after a [`Stage::DispatchPaused`] — either an
    /// operator toggled dispatch back on, or the spawn-capability breaker's
    /// half-open recovery probe / a fresh app session cleared a
    /// Breaker-origin pause (see
    /// `boss_engine::spawn_health::resume_dispatch_after_breaker_recovery`). The
    /// `details` object carries `origin`, `actor` (`"operator"` for a human
    /// toggle, `"automatic"` for breaker auto-recovery), `resumed_at_epoch_s`,
    /// `pause_duration_secs` (how long the episode this closes actually
    /// lasted), and a human-readable `reason`.
    DispatchResumed,
    /// A `RequestExecution { bypass_dispatch_pause: true, .. }` request was
    /// actually admitted through an active, overridable (operator-origin)
    /// global dispatch pause — see
    /// `docs/designs/operator-forced-dispatch-while-dispatch-is-paused.md`.
    /// Fires only once the row is claimed past the pause gate: a bypassed
    /// row that then loses to the interactive concurrency cap or another
    /// admission constraint emits [`Stage::DispatchPauseOverrideRefused`]
    /// instead, not this stage. This stage's presence means the row was
    /// claimed for dispatch, NOT that a worker has actually spawned yet —
    /// the emit site deliberately does not await the spawn tail (cube
    /// repo/workspace setup, which can take its own multi-retry budget);
    /// whether that tail then succeeds or fails is owned by the engine's
    /// ordinary retry/backoff machinery, exactly as for a claim an
    /// unpaused drain pass made. The `details` object carries
    /// `entry_point` (`"cli"` / `"app_drag"`), and the overridden pause's
    /// `origin`, `reason`, and `paused_since_epoch_s`.
    DispatchPauseOverride,
    /// A `RequestExecution { bypass_dispatch_pause: true, .. }` request was
    /// refused — either the pause itself was not overridable (breaker
    /// origin), the confirmed pause generation was stale, or a
    /// non-overridable constraint (interactive concurrency cap, unmet
    /// dependency, chain-hold, ineligible status) still blocked it after
    /// the pause was set aside. Never claims an override occurred — no
    /// worker spawned, and any `ready` row this request would otherwise
    /// have left behind is removed. The `details` object carries
    /// `entry_point`, the blocking `reason` string, and — when the refusal
    /// followed a stale confirmation — the pause state actually observed.
    DispatchPauseOverrideRefused,
    /// A dispatch was refused at the spawn chokepoint because the active
    /// pause holds it. Emitted by
    /// `ExecutionCoordinator::schedule_execution`'s admission gate — the
    /// single check every worker spawn passes through, whichever entry
    /// point queued the row. The ordinary case is silent (the ready-queue
    /// drain short-circuits before ever claiming a slot), so this event
    /// firing means a pause landed *between* a slot claim and the spawn,
    /// or a caller reached the chokepoint without honouring the pause. In
    /// either case the claimed worker is handed straight back; nothing is
    /// leased and nothing spawns.
    ///
    /// The `details` object carries `origin` (`"operator"` / `"breaker"`),
    /// `admission` (which entry point asked — `"queued"`,
    /// `"operator_forced"`, `"breaker_recovery_probe"`,
    /// `"pause_bypass_override"`), `reviews_held`, `targets_review_pool`,
    /// and the pause `reason`. Always [`Outcome::Skipped`]: a held dispatch
    /// is the pause working, not a failure.
    DispatchHeldByPause,
    /// The spawn-capability breaker's half-open recovery probe admitted one
    /// canary execution through a Breaker-origin pause (see
    /// `boss_engine::spawn_health::maybe_admit_recovery_probe`). This is the
    /// breaker's only route out of the latch — normal dispatch stays fully
    /// held while paused, so without a canary no execution could ever run to
    /// prove the app's spawn path recovered.
    ///
    /// It exists as its own stage because the bypass was previously
    /// invisible: a canary produced a complete `worker_claimed` →
    /// `cube_workspace_leased` → `pane_spawned` sequence in
    /// `bossctl dispatch tail`, minutes into a pause, with nothing
    /// distinguishing it from a pause that simply was not being honoured.
    /// Reading the tail during the 2026-08-10 incident, that is exactly how
    /// it presented.
    ///
    /// `pr_review` executions are never eligible as canaries — a dead canary
    /// records another terminal execution against its work item, and for a
    /// review that feeds the `pr_review_recovery` churn guard until the item
    /// parks with an unreviewed open PR. The `details` object carries
    /// `ready_candidates` (how many rows were eligible), `skipped_reviews`
    /// (how many ready review rows were passed over), and
    /// `consecutive_failures` (the probe backoff generation).
    BreakerRecoveryProbeAdmitted,
    /// A ready mainline item preempted an in-progress spilled automation
    /// run to obtain an interactive slot (see
    /// `boss_engine::dispatch_spillover`). Fires only when every Bridge Crew
    /// and Lower Decks slot is occupied; never for a review or automation
    /// item, and never against a mainline or review victim.
    ///
    /// Emitted TWICE per preemption — once bound to the victim execution
    /// and once to the preempting execution — so `bossctl dispatch
    /// diagnose` tells the story from either side without a cross-stream
    /// join. The victim's copy carries `preempting_execution_id` /
    /// `preempting_work_item_id`; the preemptor's carries
    /// `preempted_execution_id` / `preempted_work_item_id`. Both carry
    /// `reason` and the `requeued_as` execution id, so a reader of the
    /// victim's timeline can follow the automation work to the fresh
    /// execution that will redispatch it rather than concluding it was
    /// dropped.
    ///
    /// `outcome=ok` means the victim was torn down and its work requeued.
    /// `outcome=skipped` means preemption was considered and declined
    /// (`reason` names why: `no_eligible_victim`, `victim_mid_spawn`, …) —
    /// the mainline item simply waits for the next drain, exactly as it
    /// does on ordinary pool exhaustion. `outcome=error` means teardown
    /// or requeue failed.
    /// Post-lease recovery of a crashed worker's in-flight work, emitted
    /// once per resume dispatch that had something to recover.
    ///
    /// `outcome=ok` with `details.source="cube_in_place"` is the good path:
    /// cube re-leased the dead worker's own workspace with its dirty working
    /// copy intact, so nothing was replayed. `details.source="patch"` means
    /// cube could not recover and the saved patch was applied — `details`
    /// then carries the restored file count and line counts, in human terms,
    /// excluding Boss's own bookkeeping.
    ///
    /// `outcome=error` means the patch did NOT apply. That is deliberately a
    /// loud, separate record rather than a silent fall-through: a worker
    /// starting on a tree it believes was recovered is worse than one that
    /// knows it must start over.
    WorkspaceRecovery,
    AutomationPreempted,
    /// The engine's abandoned-branch-PR sweep found a terminated execution
    /// whose engine-supplied branch was pushed to the remote with no PR
    /// ever opened for it. `outcome=ok` with `details.action="bound"`
    /// means a PR already existed and was bound to the work item;
    /// `details.action="created"` means the sweep opened the PR itself.
    /// `outcome=error` means an auto-create attempt failed (transient or
    /// permanent — `details.transient` distinguishes them); `outcome=skipped`
    /// means the branch has nothing to open a PR for (never pushed, or no
    /// commits ahead of the default branch — `details.action="nothing_to_create"`).
    AbandonedBranchPrRecovery,
    /// A worker the engine had already terminalized proved itself alive, and
    /// the engine put it back under tracking instead of leaving it stranded.
    ///
    /// This is the *re-adoption* half of the convergence rule "a live worker
    /// for an execution the engine believes is dead must either be re-adopted
    /// or reaped." Before it existed, the engine had only the reaping half:
    /// `husk_pane_reconcile` could kill such a pane, and it correctly refuses
    /// to when the process is demonstrably alive — so the one case where the
    /// ENGINE was wrong (not the worker) had no resolution at all. The run
    /// stayed alive, untracked, invisible to `bossctl agents list`, unstoppable
    /// by `bossctl agents stop`, and — because its pool claim was gone — its
    /// work item stayed eligible for re-dispatch, which is how one chore
    /// accumulated three concurrent workers on 2026-07-28.
    ///
    /// `details` carries `trigger` (which signal proved liveness:
    /// `hook_after_terminal` or `redispatch_guard`), `prior_status` (the
    /// terminal status being reversed), `shell_pid` when a durable pid backed
    /// the decision, and `slot_id` when the app's hosted-pane list let the
    /// engine restore the live-state slot mapping too.
    LiveWorkerReadopted,
    /// A re-dispatch was declined because the row's previous worker process is
    /// still running — the durable-pid guard in `boss_engine::orphan_sweep`.
    ///
    /// Distinct from `dispatch_decision` with `live_execution_claimed=false`:
    /// that event records what the engine's own bookkeeping believed, which in
    /// this exact failure is wrong. This one records what the OS said. Its
    /// presence means a duplicate worker was prevented; `details` carries the
    /// `blocking_execution_id`, its `blocking_execution_status` (normally a
    /// TERMINAL one — that is the whole point), and the probed `shell_pid`.
    RedispatchBlockedLiveProcess,
    /// The re-dispatch guard in `boss_engine::orphan_sweep` probed a work
    /// item's previous worker process and DECLINED to block the redispatch —
    /// the converse of `redispatch_blocked_live_process`, which fires only
    /// when the guard blocks. Before this event existed the guard was
    /// silent on its decline path, so diagnosing a false redispatch (the
    /// probe said `Gone` when the worker was actually still alive) required
    /// manually cross-referencing this sweep's trace lines against a
    /// different sweep's (`dead_pid_reconcile`) 45ms apart — this event
    /// makes that self-diagnosing from `bossctl dispatch diagnose` alone.
    ///
    /// Fires only when the guard actually had a recorded pid to evaluate
    /// (`crate::durable_liveness::probe_work_item_worker` returned `Some`);
    /// a work item with no prior recorded worker process at all has nothing
    /// to decline, so it emits nothing here. `details` carries
    /// `blocking_execution_id`, `blocking_execution_status`, `probe_result`
    /// (`process_alive` / `process_gone` / `process_unknown` — always a
    /// non-alive verdict here, since an alive one blocks instead),
    /// `shell_pid` (the probed pid, when one exists), `last_event_at` and
    /// `last_event_age_secs` (the live-worker registry's corroboration
    /// signal for this execution, when available), and
    /// `corroborated_alive` (whether corroboration flipped an initially-Gone
    /// probe to Alive, in which case the guard actually blocked instead —
    /// see `redispatch_blocked_live_process`; this event never fires in
    /// that case).
    RedispatchGuardDeclined,
    /// The boot-time tmux adoption pass matched a live tmux session's
    /// authoritative `BOSS_SPAWN_TOKEN` against a non-terminal `work_runs`
    /// row and rebuilt the derived bookkeeping an engine restart always
    /// empties: the pool slot claim, the `WorkerRegistry` pid/slot map, the
    /// `LiveWorkerState` entry, and the live-status summarizer task.
    ///
    /// Distinct from `live_worker_readopted`: that event reverses a
    /// TERMINAL execution status the engine now believes was a wrong guess.
    /// This one never touches the execution's status at all — the row was
    /// correct the whole time, the engine's in-memory belief about it was
    /// just empty. `details` carries `slot_id`, `shell_pid`,
    /// `tmux_session_name`, and `repaired_intent` (`true` when the run's
    /// `tmux_spawn_state` was still `intended` — a crash between `tmux
    /// new-session` and its confirmation write — and this pass durably
    /// confirmed it before rebuilding the live state).
    TmuxAdopt,
    /// The boot-time tmux adoption pass found a live session with an
    /// unsupported `BOSS_SESSION_SCHEMA` and refused to adopt it, reaping it
    /// instead — a version-skew guard, not a contradiction
    /// [`crate::worker_readoption`] resolves. Fires from either of the two
    /// places the schema guard runs: a session whose token matched a
    /// non-terminal `work_runs` row (about to go to `adopt_one`), or one
    /// whose token resolved to an already-terminal execution (about to be
    /// handed off to [`crate::worker_readoption`]) — `details` distinguishes
    /// the two only implicitly, via whatever the execution's own status
    /// says. The session's `BOSS_SESSION_SCHEMA` was missing, unparseable,
    /// or newer than this engine's own contract
    /// (`tools/boss/engine/core/src/spawn_flow.rs`'s `TMUX_SESSION_SCHEMA`),
    /// meaning the session was written by a build this engine cannot safely
    /// assume compatibility with — the session's command line, environment,
    /// and injected settings could all differ from what this engine would
    /// have written. Refusing-then-reaping (rather than refusing and leaving
    /// the session alive) prevents two live workers ever sharing one cube
    /// workspace. `details` carries `session_name`, `reason`,
    /// `schema_guard_failure` (`missing` / `unparseable` / `too_new`) plus
    /// the raw and supported schema values where applicable, and `reaped`
    /// (`true`/`false`) — whether the `kill-session` this refusal depends on
    /// actually succeeded. Outcome is [`Outcome::Ok`] when `reaped` is
    /// `true` and [`Outcome::Error`] when the kill itself failed, since the
    /// session may still be running in that case. The execution row itself
    /// is always left untouched by this event; whether anything picks the
    /// work back up depends on whether the row was already terminal — a
    /// non-terminal row is left for the normal dead-worker reconcilers to
    /// redispatch, a terminal row has nothing left to redispatch.
    TmuxRefuseSkew,
    /// The tmux inventory found a Boss-tokened session that has no durable
    /// run row. The husk sweep will independently confirm it before reaping.
    /// Not scoped to any `work_runs` row — the only identifier we have is
    /// the session's spawn token, and using that as `execution_id` would
    /// mint a new JsonlFileSink mirror directory under
    /// `executions/<token>/` on every leak. `execution_id` is therefore
    /// the same constant sentinel `"engine-boot"` that
    /// [`Stage::TmuxAdoptionOwnerConflict`] uses: one stable synthetic
    /// mirror (`executions/engine-boot/`) shared with other boot-scoped
    /// tmux events. The spawn token lives in `details.spawn_token`
    /// alongside `tmux_session_name` and `reason`. Always
    /// [`Outcome::Ok`] — detection is the success of this stage; reaping
    /// is a later husk-sweep concern.
    TmuxLeakDetected,
    /// A session name exists but its live spawn token differs from the
    /// durable identity. The engine refused to touch that session.
    TmuxTokenMismatch,
    /// The boot-time tmux adoption pass (`boss-engine`'s
    /// `tmux_adoption::claim_or_detect_conflicting_owner`) refused to run at
    /// all because the server-scoped `@boss_engine_owner` tmux option was
    /// stamped by a different, still-live engine process — double-adoption
    /// would risk two engines controlling the same worker sessions. Not
    /// scoped to any one execution (the whole pass is skipped before any
    /// session is even enumerated), so `execution_id` is the constant
    /// sentinel `"engine-boot"` rather than a real `work_runs` row — one
    /// stable synthetic mirror directory (`executions/engine-boot/`) shared
    /// across every boot that hits this conflict, not one per boot/pid, so
    /// forensic tooling that reads mirrors by execution id sees a single
    /// well-known non-execution directory instead of an unbounded set of
    /// unknown ones. `details` carries `other_pid` and `this_pid` (the pid
    /// that would otherwise have gone into the id). Always
    /// [`Outcome::Error`] — a skipped adoption pass on a host with
    /// tmux-hosted workers is itself an operational problem, not a routine
    /// outcome.
    TmuxAdoptionOwnerConflict,
    /// Startup recovery re-issued the pane spawn for a `running` execution
    /// whose cube lease was re-adopted across an engine restart but whose
    /// worker pane was never registered (the previous process died between
    /// `run_started` / driver resolution and `spawn_requested`). Complements
    /// `cube-lease heartbeat: re-adopted live lease at startup`: that path
    /// keeps the workspace, this path re-drives the pane. `outcome=ok` means
    /// the spawn was handed back to the runner; `outcome=error` means a
    /// required pane-presence oracle could not be asked (must not be treated
    /// as either present or absent) or the respawn itself failed;
    /// `outcome=skipped` means a pane was already present. `details` carries
    /// `reason` and, when relevant, the oracle diagnostic.
    StartupPaneRespawn,
}

/// How a dispatch record participates in an execution timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineStageClass {
    /// A dispatch step which replaces the tracked stage and starts its clock.
    Pipeline,
    /// A record made only after dispatch has handed the execution to a pane.
    PostDispatch,
    /// Bookkeeping about a timeline that must not replace its tracked stage.
    Observation,
}

impl Stage {
    /// Every stable stage name, for exhaustive schema-contract tests.
    pub const ALL: [Stage; 62] = [
        Stage::StatusTransition,
        Stage::RequestRecorded,
        Stage::WorkerClaimed,
        Stage::HostSelected,
        Stage::CubeRepoEnsureAttempted,
        Stage::CubeRepoEnsured,
        Stage::CubeRepoEnsureFailed,
        Stage::CubeWorkspaceLeaseAttempted,
        Stage::CubeWorkspaceLeased,
        Stage::CubeWorkspaceLeaseFailed,
        Stage::CubeWorkspacePositioned,
        Stage::CubeWorkspacePositioningFailed,
        Stage::CubeChangeCreated,
        Stage::RunStarted,
        Stage::PaneSpawned,
        Stage::ExecutionCancelled,
        Stage::SpawnFailed,
        Stage::ExecutionFinalized,
        Stage::StageStalled,
        Stage::OrphanActiveRedispatch,
        Stage::PrReviewDeadRecovery,
        Stage::DeadPidReconcile,
        Stage::PaneDeathReconcile,
        Stage::DispatchDecision,
        Stage::TransientRecovery,
        Stage::TransientRecoveryExhausted,
        Stage::TransientRecoveryNudge,
        Stage::StaleWorkerReconcile,
        Stage::PoolClaimReconcile,
        Stage::TerminalWorkReconcile,
        Stage::CubeLeaseHeartbeat,
        Stage::LostWorkspaceReconcile,
        Stage::CubeLeaseAutoReap,
        Stage::RemoteLeaseReconcile,
        Stage::HostDrainReconcile,
        Stage::SpawnAckTimeout,
        Stage::DriverStartTimeout,
        Stage::DispatchFailureRecoveryRedispatch,
        Stage::SpawnNack,
        Stage::PaneDeathBeforeStart,
        Stage::SpawnCapabilityUnhealthy,
        Stage::SpawnCapabilityRecovered,
        Stage::HuskPaneReconcile,
        Stage::ExecutionLivenessReconcile,
        Stage::DispatchPaused,
        Stage::DispatchResumed,
        Stage::DispatchPauseOverride,
        Stage::DispatchPauseOverrideRefused,
        Stage::DispatchHeldByPause,
        Stage::BreakerRecoveryProbeAdmitted,
        Stage::WorkspaceRecovery,
        Stage::AutomationPreempted,
        Stage::AbandonedBranchPrRecovery,
        Stage::LiveWorkerReadopted,
        Stage::RedispatchBlockedLiveProcess,
        Stage::RedispatchGuardDeclined,
        Stage::TmuxAdopt,
        Stage::TmuxRefuseSkew,
        Stage::TmuxLeakDetected,
        Stage::TmuxTokenMismatch,
        Stage::TmuxAdoptionOwnerConflict,
        Stage::StartupPaneRespawn,
    ];

    /// Classify this stage's effect on an execution timeline.
    pub fn timeline_class(self) -> TimelineStageClass {
        match self {
            // These events describe an execution but are not progression in
            // its dispatch pipeline. In particular, a heartbeat must not
            // hide a dispatch that is stuck before pane spawn, and the
            // victim-side preemption record must not reopen a finished run.
            Stage::DispatchDecision | Stage::AutomationPreempted | Stage::CubeLeaseHeartbeat => {
                TimelineStageClass::Observation
            }
            stage if stage.is_post_dispatch() => TimelineStageClass::PostDispatch,
            _ => TimelineStageClass::Pipeline,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Stage::StatusTransition => "status_transition",
            Stage::RequestRecorded => "request_recorded",
            Stage::WorkerClaimed => "worker_claimed",
            Stage::HostSelected => "host_selected",
            Stage::CubeRepoEnsureAttempted => "cube_repo_ensure_attempted",
            Stage::CubeRepoEnsured => "cube_repo_ensured",
            Stage::CubeRepoEnsureFailed => "cube_repo_ensure_failed",
            Stage::CubeWorkspaceLeaseAttempted => "cube_workspace_lease_attempted",
            Stage::CubeWorkspaceLeased => "cube_workspace_leased",
            Stage::CubeWorkspaceLeaseFailed => "cube_workspace_lease_failed",
            Stage::CubeWorkspacePositioned => "cube_workspace_positioned",
            Stage::CubeWorkspacePositioningFailed => "cube_workspace_positioning_failed",
            Stage::CubeChangeCreated => "cube_change_created",
            Stage::RunStarted => "run_started",
            Stage::PaneSpawned => "pane_spawned",
            Stage::ExecutionCancelled => "execution_cancelled",
            Stage::SpawnFailed => "spawn_failed",
            Stage::ExecutionFinalized => "execution_finalized",
            Stage::StageStalled => "stage_stalled",
            Stage::OrphanActiveRedispatch => "orphan_active_redispatch",
            Stage::PrReviewDeadRecovery => "pr_review_dead_recovery",
            Stage::DeadPidReconcile => "dead_pid_reconcile",
            Stage::PaneDeathReconcile => "pane_death_reconcile",
            Stage::DispatchDecision => "dispatch_decision",
            Stage::TransientRecovery => "transient_recovery",
            Stage::TransientRecoveryExhausted => "transient_recovery_exhausted",
            Stage::TransientRecoveryNudge => "transient_recovery_nudge",
            Stage::StaleWorkerReconcile => "stale_worker_reconcile",
            Stage::PoolClaimReconcile => "pool_claim_reconcile",
            Stage::TerminalWorkReconcile => "terminal_work_reconcile",
            Stage::CubeLeaseHeartbeat => "cube_lease_heartbeat",
            Stage::LostWorkspaceReconcile => "lost_workspace_reconcile",
            Stage::CubeLeaseAutoReap => "cube_lease_auto_reap",
            Stage::RemoteLeaseReconcile => "remote_lease_reconcile",
            Stage::HostDrainReconcile => "host_drain_reconcile",
            Stage::SpawnAckTimeout => "spawn_ack_timeout",
            Stage::DriverStartTimeout => "driver_start_timeout",
            Stage::DispatchFailureRecoveryRedispatch => "dispatch_failure_recovery_redispatch",
            Stage::SpawnNack => "spawn_nack",
            Stage::PaneDeathBeforeStart => "pane_death_before_start",
            Stage::SpawnCapabilityUnhealthy => "spawn_capability_unhealthy",
            Stage::SpawnCapabilityRecovered => "spawn_capability_recovered",
            Stage::HuskPaneReconcile => "husk_pane_reconcile",
            Stage::ExecutionLivenessReconcile => "execution_liveness_reconcile",
            Stage::DispatchPaused => "dispatch_paused",
            Stage::DispatchResumed => "dispatch_resumed",
            Stage::DispatchPauseOverride => "dispatch_pause_override",
            Stage::DispatchPauseOverrideRefused => "dispatch_pause_override_refused",
            Stage::DispatchHeldByPause => "dispatch_held_by_pause",
            Stage::BreakerRecoveryProbeAdmitted => "breaker_recovery_probe_admitted",
            Stage::AutomationPreempted => "automation_preempted",
            Stage::WorkspaceRecovery => "workspace_recovery",
            Stage::AbandonedBranchPrRecovery => "abandoned_branch_pr_recovery",
            Stage::LiveWorkerReadopted => "live_worker_readopted",
            Stage::RedispatchBlockedLiveProcess => "redispatch_blocked_live_process",
            Stage::RedispatchGuardDeclined => "redispatch_guard_declined",
            Stage::TmuxAdopt => "tmux_adopt",
            Stage::TmuxRefuseSkew => "tmux_refuse_skew",
            Stage::TmuxLeakDetected => "tmux_leak_detected",
            Stage::TmuxTokenMismatch => "tmux_token_mismatch",
            Stage::TmuxAdoptionOwnerConflict => "tmux_adoption_owner_conflict",
            Stage::StartupPaneRespawn => "startup_pane_respawn",
        }
    }

    /// Parse a wire stage name (`DispatchEvent::stage`) back into a
    /// [`Stage`]. `None` for a name this build does not know — a record
    /// written by a newer engine, or a corrupt line. Readers must treat
    /// `None` as "not classifiable", never as any particular phase.
    pub fn from_wire(stage: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(stage.to_owned())).ok()
    }

    /// Whether this stage is a **post-dispatch observation** rather than a
    /// step of the dispatch pipeline.
    ///
    /// The dispatch pipeline is the ordered handoff `request_recorded` →
    /// `worker_claimed` → … → `pane_spawned`: every stage in it is one the
    /// run can be *stuck in*, and the stage-stalled detector flags a
    /// timeline whose last event is such a stage and has not progressed.
    /// Everything else recorded against an execution id is an observation
    /// about a run that is already past that handoff — a live pane was
    /// found or adopted, a sweep reaped or terminalized the execution, the
    /// completion handler finalized it. Those events are appended to the
    /// same per-execution timeline, but the run is not "stuck in" them and
    /// nothing in the pipeline will ever follow them under the same
    /// execution id, so they must never re-open a timeline that dispatch
    /// already closed. The 2026-09-04 incident is what happens when one
    /// does: an engine restart appended `tmux_adopt` to two runs whose
    /// timelines had ended at `pane_spawned`, the detector treated
    /// `tmux_adopt` as a pipeline stage that had not progressed, and both
    /// runs were reported `stage_stalled` at `tmux_adopt` two minutes
    /// later — a hundred seconds after they had completed cleanly.
    ///
    /// The match is deliberately exhaustive: adding a stage forces its
    /// author to say which side it is on. When in doubt, answer `false` —
    /// a pipeline stage misfiled here would silence a real stall, whereas
    /// an observation misfiled as a pipeline stage only produces the false
    /// stall this classification exists to prevent, which is loud.
    pub fn is_post_dispatch(self) -> bool {
        match self {
            // ---- Dispatch pipeline: a run can be stuck in these. --------
            Stage::StatusTransition
            | Stage::RequestRecorded
            | Stage::WorkerClaimed
            | Stage::HostSelected
            | Stage::CubeRepoEnsureAttempted
            | Stage::CubeRepoEnsured
            | Stage::CubeRepoEnsureFailed
            | Stage::CubeWorkspaceLeaseAttempted
            | Stage::CubeWorkspaceLeased
            | Stage::CubeWorkspaceLeaseFailed
            | Stage::CubeWorkspacePositioned
            | Stage::CubeWorkspacePositioningFailed
            | Stage::CubeChangeCreated
            | Stage::RunStarted
            | Stage::PaneSpawned
            | Stage::SpawnFailed
            // Re-entry points: a fresh dispatch starts (or is re-driven)
            // from these, so what follows is pipeline progress.
            | Stage::OrphanActiveRedispatch
            | Stage::PrReviewDeadRecovery
            | Stage::DispatchFailureRecoveryRedispatch
            | Stage::BreakerRecoveryProbeAdmitted
            | Stage::StartupPaneRespawn
            | Stage::WorkspaceRecovery
            // Pause / breaker bookkeeping around a dispatch that is being
            // held; the held run is still waiting on the pipeline.
            | Stage::DispatchPaused
            | Stage::DispatchResumed
            | Stage::DispatchPauseOverride
            | Stage::DispatchPauseOverrideRefused
            | Stage::DispatchHeldByPause
            | Stage::SpawnCapabilityUnhealthy
            | Stage::SpawnCapabilityRecovered
            // These are folded as `TimelineStageClass::Observation` before
            // terminality is considered, so they are neither pipeline nor
            // post-dispatch events themselves.
            | Stage::DispatchDecision
            | Stage::AutomationPreempted
            | Stage::CubeLeaseHeartbeat
            // Not a stage at all — the detector's own flag, folded
            // separately by every reader before terminality is asked.
            | Stage::StageStalled => false,

            // ---- Post-dispatch: a live pane exists or was adopted. -------
            Stage::TmuxAdopt
            | Stage::LiveWorkerReadopted
            | Stage::TransientRecoveryNudge
            | Stage::RedispatchBlockedLiveProcess
            // ---- Post-dispatch: the run ended (completion handler). ------
            | Stage::ExecutionFinalized
            | Stage::ExecutionCancelled
            // ---- Post-dispatch: a sweep reaped / terminalized the run;
            // any redispatch is a fresh execution id. -------------------
            | Stage::DeadPidReconcile
            | Stage::PaneDeathReconcile
            | Stage::StaleWorkerReconcile
            | Stage::PoolClaimReconcile
            | Stage::TerminalWorkReconcile
            | Stage::LostWorkspaceReconcile
            | Stage::CubeLeaseAutoReap
            | Stage::RemoteLeaseReconcile
            | Stage::HostDrainReconcile
            | Stage::ExecutionLivenessReconcile
            | Stage::SpawnAckTimeout
            | Stage::DriverStartTimeout
            | Stage::SpawnNack
            | Stage::PaneDeathBeforeStart
            | Stage::TransientRecovery
            | Stage::TransientRecoveryExhausted
            | Stage::AbandonedBranchPrRecovery
            | Stage::RedispatchGuardDeclined
            // ---- Post-dispatch: tmux inventory findings about sessions
            // that already exist (keyed to a spawn token, the
            // `engine-boot` sentinel, or a run that had a pane). -------
            | Stage::HuskPaneReconcile
            | Stage::TmuxRefuseSkew
            | Stage::TmuxLeakDetected
            | Stage::TmuxTokenMismatch
            | Stage::TmuxAdoptionOwnerConflict => true,
        }
    }
}

/// Three-valued outcome rather than a boolean so a stage that was
/// reached but decided to skip (e.g., worker pool exhausted) is
/// distinguishable from a stage that errored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Error,
    Skipped,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Error => "error",
            Outcome::Skipped => "skipped",
        }
    }
}

/// One line in the dispatch event stream. The wire shape is
/// deliberately wide — readers don't need to know about every field
/// and a writer that doesn't yet have a value emits `null` rather
/// than dropping the key.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct DispatchEvent {
    pub ts_epoch_ms: u128,
    pub stage: String,
    pub outcome: String,
    pub execution_id: String,
    pub work_item_id: Option<String>,
    pub worker_id: Option<String>,
    pub cube_repo_id: Option<String>,
    pub cube_lease_id: Option<String>,
    pub cube_workspace_id: Option<String>,
    /// Flat string copy of `format!("{err:#}")` for failure events.
    /// Skip when the outcome is `ok` / `skipped`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Full shell-quoted argv string of the cube subprocess invocation,
    /// e.g. `cube workspace lease ci-infra --task "fix the bug"`.
    /// Copy-pastes into a terminal to reproduce the failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cube_command: Option<String>,
    /// Absolute working directory passed to the cube subprocess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cube_cwd: Option<String>,
    /// Per-stage open object; readers `jq` into this when they care.
    #[serde(default)]
    #[builder(default)]
    pub details: serde_json::Value,
}

impl DispatchEvent {
    pub fn new(stage: Stage, outcome: Outcome, execution_id: impl Into<String>) -> Self {
        let ts_epoch_ms = boss_engine_utils::epoch_time::now_epoch_ms();
        Self {
            ts_epoch_ms,
            stage: stage.as_str().to_owned(),
            outcome: outcome.as_str().to_owned(),
            execution_id: execution_id.into(),
            work_item_id: None,
            worker_id: None,
            cube_repo_id: None,
            cube_lease_id: None,
            cube_workspace_id: None,
            error_message: None,
            cube_command: None,
            cube_cwd: None,
            details: serde_json::Value::Null,
        }
    }

    pub fn with_work_item(mut self, work_item_id: impl Into<String>) -> Self {
        self.work_item_id = Some(work_item_id.into());
        self
    }

    pub fn with_worker(mut self, worker_id: impl Into<String>) -> Self {
        self.worker_id = Some(worker_id.into());
        self
    }

    pub fn with_cube_repo(mut self, repo_id: impl Into<String>) -> Self {
        self.cube_repo_id = Some(repo_id.into());
        self
    }

    pub fn with_cube_lease(mut self, lease_id: impl Into<String>) -> Self {
        self.cube_lease_id = Some(lease_id.into());
        self
    }

    pub fn with_cube_workspace(mut self, workspace_id: impl Into<String>) -> Self {
        self.cube_workspace_id = Some(workspace_id.into());
        self
    }

    pub fn with_error(mut self, error: &anyhow::Error) -> Self {
        self.error_message = Some(format!("{error:#}"));
        self
    }

    /// Attach `cube_command` and `cube_cwd` from a `(command, cwd)` pair.
    /// Accepts `Option` so callers can pass the result of
    /// `CubeClient::command_repr` directly without an extra `if let`.
    pub fn with_cube_invocation(mut self, info: Option<(String, String)>) -> Self {
        if let Some((cmd, cwd)) = info {
            self.cube_command = Some(cmd);
            self.cube_cwd = Some(cwd);
        }
        self
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }
}

#[async_trait]
pub trait DispatchEventSink: Send + Sync {
    async fn emit(&self, event: DispatchEvent);
}

/// Default sink for tests and any caller that doesn't want the
/// structured stream. Production wiring should use
/// [`JsonlFileSink`] under the Boss state root.
#[derive(Default, Debug, Clone)]
pub struct NoopDispatchEventSink;

#[async_trait]
impl DispatchEventSink for NoopDispatchEventSink {
    async fn emit(&self, _event: DispatchEvent) {}
}

/// Test double: records every event in memory so assertions can
/// inspect the stage timeline without scanning a tracing log.
#[derive(Default, Debug, Clone)]
pub struct RecordingDispatchEventSink {
    events: Arc<Mutex<Vec<DispatchEvent>>>,
}

impl RecordingDispatchEventSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn events(&self) -> Vec<DispatchEvent> {
        self.events.lock().await.clone()
    }

    pub async fn events_for(&self, execution_id: &str) -> Vec<DispatchEvent> {
        self.events
            .lock()
            .await
            .iter()
            .filter(|event| event.execution_id == execution_id)
            .cloned()
            .collect()
    }
}

#[async_trait]
impl DispatchEventSink for RecordingDispatchEventSink {
    async fn emit(&self, event: DispatchEvent) {
        self.events.lock().await.push(event);
    }
}

/// Env var overriding [`DEFAULT_CURRENT_MAX_BYTES`].
pub const CURRENT_MAX_BYTES_ENV: &str = "BOSS_DISPATCH_EVENTS_MAX_BYTES";
/// Env var overriding [`DEFAULT_CURRENT_MAX_FILES`].
pub const CURRENT_MAX_FILES_ENV: &str = "BOSS_DISPATCH_EVENTS_MAX_FILES";

/// Default maximum size of `current.jsonl` before it is rotated: 100 MiB.
/// Matches `engine-trace.jsonl`'s threshold (`trace_rotation::DEFAULT_TRACE_MAX_BYTES`)
/// — `current.jsonl` is the forensic surface of last resort when the engine
/// is wedged, so retention is generous rather than minimal. At the observed
/// growth rate of ~1.5 MB/day (116 MB accumulated over 78 days with no
/// rotation at all), a 100 MiB segment holds roughly two months of history
/// before rotating.
pub const DEFAULT_CURRENT_MAX_BYTES: u64 = 100 * 1024 * 1024;
/// Default number of rotated `current.jsonl` segments to keep, matching
/// `engine-trace.jsonl`'s retention. Five 100 MiB segments is up to ~500 MiB
/// / roughly a year of history at the observed growth rate — generous on
/// purpose for a forensic log, not a tight budget.
pub const DEFAULT_CURRENT_MAX_FILES: usize = 5;

/// Process-wide lock serializing the stat+rename+prune sequence in
/// [`JsonlFileSink::maybe_rotate_current`]. Living at crate scope (rather than
/// as an instance field), like `boss_engine_jsonl_append::APPEND_LOCK`, is
/// what makes the "two concurrent emits crossing the threshold at once can't
/// both act on a stale size" guarantee true regardless of how many
/// `JsonlFileSink` values a caller constructs against the same or different
/// roots — see that method's doc for the retention-slot bug this closes.
static ROTATION_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Production sink: appends each event as one JSON line to
/// `<root>/dispatch-events/current.jsonl` and mirrors it into
/// `<root>/executions/<execution_id>/dispatch.jsonl` so a
/// single-execution diagnose verb doesn't need to scan the full
/// stream. Both writes are best-effort; failures log via `tracing`
/// and are dropped. Appends go through [`JsonlAppender`], which
/// serializes concurrent writers so two events emitted from different
/// tasks can never interleave into one corrupt line — see that
/// crate's docs for why a body-then-newline two-write sequence (the
/// bug this sink used to have) is unsafe under concurrency.
///
/// `current.jsonl` rotates once it crosses `max_bytes`, using the same
/// `<base>.<unix_seconds>` on-disk scheme as `engine-trace.jsonl`
/// (`boss_log_files::segments`), so `bossctl` and the engine's
/// `TimelineIndex` read rotated history through the same helper that
/// already understands the format. The per-execution mirrors are NOT
/// rotated here — they are bounded per execution already and rotating
/// them would fragment the single-execution diagnose view for no benefit.
#[derive(Debug, Clone)]
pub struct JsonlFileSink {
    root: PathBuf,
    appender: Arc<JsonlAppender>,
    max_bytes: u64,
    max_files: usize,
}

impl JsonlFileSink {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let max_bytes = boss_engine_utils::env_parse::env_parsed_or(CURRENT_MAX_BYTES_ENV, DEFAULT_CURRENT_MAX_BYTES);
        let max_files = boss_engine_utils::env_parse::env_parsed_or(CURRENT_MAX_FILES_ENV, DEFAULT_CURRENT_MAX_FILES);
        Self {
            root: root.into(),
            appender: Arc::new(JsonlAppender::new()),
            max_bytes,
            max_files,
        }
    }

    fn current_path(&self) -> PathBuf {
        self.root.join("dispatch-events").join("current.jsonl")
    }

    fn execution_path(&self, execution_id: &str) -> PathBuf {
        self.root.join("executions").join(execution_id).join("dispatch.jsonl")
    }

    /// Rotate `current.jsonl` if it has crossed `max_bytes`, then prune old
    /// segments beyond `max_files`. Called after every successful append so
    /// growth is caught promptly without a separate background sweep.
    ///
    /// Best-effort: a `stat`/`rename` failure logs via `tracing` and is
    /// otherwise ignored — rotation must never turn a dropped write into a
    /// dropped event. The stat+rename+prune sequence runs under
    /// `ROTATION_LOCK`, so two concurrent emits that both observe the file
    /// over threshold no longer race: the loser re-stats after the winner
    /// has already rotated and recreated the live file, sees it back under
    /// threshold, and does nothing — it does NOT rotate a near-empty file
    /// and burn a retention slot the way an unguarded re-check would.
    ///
    /// After a successful rename, the live file is immediately recreated so
    /// `current.jsonl` is never observably absent to a concurrent reader such
    /// as `TimelineIndex::refresh`. That recreate opens with `create(true)`
    /// and `append(true)` rather than truncating: `emit`'s append path holds
    /// only the crate-wide `boss_engine_jsonl_append::APPEND_LOCK`, not
    /// `ROTATION_LOCK`, so an emit can land between the rename and the
    /// recreate and write its line into a freshly-created `current.jsonl`
    /// before this method gets to it — a truncating recreate would silently
    /// erase that line. Opening non-truncating preserves it; the recreated
    /// file is empty only in the common case where no such write raced in.
    ///
    /// This `stat`s the file on every emit rather than tracking size with an
    /// in-memory byte counter (as `trace_rotation` does). Deliberate: at
    /// `DEFAULT_CURRENT_MAX_BYTES`'s ~two-month rotation cadence the extra
    /// syscall per event is immaterial, and a `stat` is immune to counter
    /// drift across process restarts — and needs no bookkeeping: an
    /// in-memory counter would have to be re-seeded from the file length on
    /// every startup to avoid drift, which is strictly more state than this.
    async fn maybe_rotate_current(&self, current_path: &Path) {
        let _guard = ROTATION_LOCK.lock().await;

        let len = match tokio::fs::metadata(current_path).await {
            Ok(meta) => meta.len(),
            Err(_) => return,
        };
        if len < self.max_bytes {
            return;
        }

        let rotated = boss_log_files::next_rotated_path(current_path);
        if let Err(err) = tokio::fs::rename(current_path, &rotated).await {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    ?err,
                    path = %current_path.display(),
                    "failed to rotate dispatch-events current.jsonl"
                );
            }
            return;
        }

        if let Err(err) = recreate_live_file(current_path).await {
            tracing::warn!(
                ?err,
                path = %current_path.display(),
                "failed to recreate dispatch-events current.jsonl after rotation"
            );
        }

        let to_prune = boss_log_files::rotated_segments(current_path);
        if to_prune.len() <= self.max_files {
            return;
        }
        for path in &to_prune[..to_prune.len() - self.max_files] {
            if let Err(err) = tokio::fs::remove_file(path).await {
                tracing::warn!(?err, path = %path.display(), "failed to prune old dispatch-events segment");
            }
        }
    }
}

/// Ensures `path` exists without truncating it. Used to recreate the live
/// file immediately after rotation's rename — non-truncating because an
/// `emit` can land between that rename and this recreate (it holds only
/// `boss_engine_jsonl_append::APPEND_LOCK`, not `ROTATION_LOCK`) and write its
/// line into a freshly-created file before this call runs; a truncating
/// create (e.g. `tokio::fs::File::create`) would silently erase that line.
async fn recreate_live_file(path: &Path) -> std::io::Result<()> {
    tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    Ok(())
}

#[async_trait]
impl DispatchEventSink for JsonlFileSink {
    async fn emit(&self, event: DispatchEvent) {
        let current_path = self.current_path();
        let execution_path = self.execution_path(&event.execution_id);

        let mut results = self
            .appender
            .append_to_all(&[current_path.as_path(), execution_path.as_path()], &event)
            .await
            .into_iter();
        let current_result = results.next().expect("append_to_all returns one result per input path");
        let execution_result = results.next().expect("append_to_all returns one result per input path");

        if let Err(err) = current_result {
            tracing::warn!(
                ?err,
                path = %current_path.display(),
                stage = %event.stage,
                execution_id = %event.execution_id,
                "failed to append dispatch event to current.jsonl; dropping"
            );
        } else {
            self.maybe_rotate_current(&current_path).await;
        }

        if let Err(err) = execution_result {
            tracing::warn!(
                ?err,
                path = %execution_path.display(),
                stage = %event.stage,
                execution_id = %event.execution_id,
                "failed to append dispatch event to per-execution mirror; dropping"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn jsonl_file_sink_appends_to_current_and_mirror() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());

        let event_a = DispatchEvent::new(Stage::CubeWorkspaceLeased, Outcome::Ok, "exec-a")
            .with_work_item("task-a")
            .with_cube_lease("lease-1");
        sink.emit(event_a).await;

        let event_b = DispatchEvent::new(Stage::PaneSpawned, Outcome::Error, "exec-a")
            .with_error(&anyhow::anyhow!("app refused spawn"));
        sink.emit(event_b).await;

        let current = fs::read_to_string(dir.path().join("dispatch-events/current.jsonl")).unwrap();
        let lines: Vec<&str> = current.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("cube_workspace_leased"));
        assert!(lines[1].contains("pane_spawned"));
        assert!(lines[1].contains("app refused spawn"));

        let mirror = fs::read_to_string(dir.path().join("executions/exec-a/dispatch.jsonl")).unwrap();
        assert_eq!(mirror.lines().count(), 2);
    }

    fn sink_with_rotation(root: impl Into<PathBuf>, max_bytes: u64, max_files: usize) -> JsonlFileSink {
        JsonlFileSink {
            root: root.into(),
            appender: Arc::new(JsonlAppender::new()),
            max_bytes,
            max_files,
        }
    }

    /// Once `current.jsonl` crosses `max_bytes`, the next emit rotates it to
    /// a `<unix_seconds>`-suffixed segment and starts a fresh, empty live
    /// file — the segment plus the live file together contain every emitted
    /// line, so nothing is lost across the rotation boundary.
    #[tokio::test]
    async fn rotates_current_once_threshold_crossed() {
        let dir = TempDir::new().unwrap();
        let sink = sink_with_rotation(dir.path(), 10, 5);
        let current_path = dir.path().join("dispatch-events/current.jsonl");

        sink.emit(DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-1"))
            .await;
        sink.emit(DispatchEvent::new(Stage::WorkerClaimed, Outcome::Ok, "exec-1"))
            .await;

        let backups = boss_log_files::rotated_segments(&current_path);
        assert!(
            !backups.is_empty(),
            "10-byte threshold should have triggered at least one rotation"
        );

        // Every event emitted so far must still be readable across the
        // rotated segment(s) plus whatever now lives in the fresh current
        // file — no event may be lost across a rotation boundary.
        let mut total_lines = 0;
        for backup in &backups {
            total_lines += fs::read_to_string(backup).unwrap().lines().count();
        }
        if let Ok(current) = fs::read_to_string(&current_path) {
            total_lines += current.lines().count();
        }
        assert_eq!(total_lines, 2, "no event may be lost across rotation");
    }

    /// Rotation prunes segments beyond `max_files`, keeping only the most
    /// recent ones — mirrors `trace_rotation::prune_old_rotated`.
    #[tokio::test]
    async fn prunes_old_segments_beyond_max_files() {
        let dir = TempDir::new().unwrap();
        let sink = sink_with_rotation(dir.path(), 5, 2);
        let current_path = dir.path().join("dispatch-events/current.jsonl");

        // Pre-populate 2 old rotated segments so the next rotation pushes
        // the count to 3, one over the max_files=2 limit.
        fs::create_dir_all(current_path.parent().unwrap()).unwrap();
        for ts in [1_000_u64, 1_001] {
            fs::write(boss_log_files::rotated_segment_path(&current_path, ts), b"old").unwrap();
        }

        sink.emit(DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-1"))
            .await;
        sink.emit(DispatchEvent::new(Stage::WorkerClaimed, Outcome::Ok, "exec-1"))
            .await;

        let backups = boss_log_files::rotated_segments(&current_path);
        assert_eq!(backups.len(), 2, "expected exactly 2 rotated segments after prune");
        // Each emit here crosses the 5-byte threshold on its own, so this
        // scenario rotates twice: the pre-seeded `.1000` and `.1001` segments
        // are both pushed out by the two freshly rotated segments, oldest
        // first. Assert the specific survivors rather than just a count, so
        // a prune that kept the wrong segments (e.g. newest-N-by-mtime
        // instead of by parsed suffix) would fail this test.
        assert!(
            backups.iter().all(|p| {
                let suffix = p.extension().and_then(|e| e.to_str());
                suffix != Some("1000") && suffix != Some("1001")
            }),
            "pre-seeded segments should have been pruned, got {backups:?}"
        );
    }

    /// If an `emit` writes a line into a freshly-recreated `current.jsonl`
    /// after rotation's rename but before rotation's recreate step runs (the
    /// two hold different locks, so this interleaving is reachable), that
    /// line must survive the recreate — it must not be silently truncated
    /// away, which is exactly the bug a `File::create`-based recreate had.
    #[tokio::test]
    async fn recreate_after_rotation_does_not_truncate_a_raced_write() {
        let dir = TempDir::new().unwrap();
        let current_path = dir.path().join("current.jsonl");

        // Simulate emit B: after A's rename moved the old file aside, B's
        // append_locked ran `OpenOptions::create(true).append(true)` and
        // wrote a full line into a brand-new current.jsonl.
        fs::write(&current_path, b"{\"raced\":true}\n").unwrap();

        // A now runs its recreate step.
        recreate_live_file(&current_path).await.unwrap();

        let content = fs::read_to_string(&current_path).unwrap();
        assert_eq!(
            content, "{\"raced\":true}\n",
            "recreate must not truncate a line written by a concurrent emit"
        );
    }

    /// Below the threshold, `current.jsonl` never rotates.
    #[tokio::test]
    async fn no_rotation_below_threshold() {
        let dir = TempDir::new().unwrap();
        let sink = sink_with_rotation(dir.path(), 1024 * 1024, 5);
        let current_path = dir.path().join("dispatch-events/current.jsonl");

        sink.emit(DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-1"))
            .await;

        assert!(boss_log_files::rotated_segments(&current_path).is_empty());
    }

    #[tokio::test]
    async fn recording_sink_collects_events_per_execution() {
        let sink = RecordingDispatchEventSink::new();
        sink.emit(DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-1"))
            .await;
        sink.emit(DispatchEvent::new(Stage::WorkerClaimed, Outcome::Skipped, "exec-2"))
            .await;
        sink.emit(DispatchEvent::new(Stage::PaneSpawned, Outcome::Error, "exec-1"))
            .await;

        let all = sink.events().await;
        assert_eq!(all.len(), 3);

        let only_one = sink.events_for("exec-1").await;
        assert_eq!(only_one.len(), 2);
        assert_eq!(only_one[0].stage, "request_recorded");
        assert_eq!(only_one[1].stage, "pane_spawned");
        assert_eq!(only_one[1].outcome, "error");
    }

    /// On an `Ok` event the three skip-serialized optionals must be
    /// *absent* from the JSON object (not present-as-null) so the `jq`
    /// expressions downstream tooling uses (`has("error_message")`,
    /// `.cube_command // empty`) behave. `details` still serializes —
    /// as JSON `null` — because it carries no `skip_serializing_if`.
    #[test]
    fn ok_event_omits_skip_serialized_optional_keys() {
        let event = DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-omit");
        let value = serde_json::to_value(&event).unwrap();
        let obj = value.as_object().unwrap();

        assert!(
            !obj.contains_key("error_message"),
            "error_message must be omitted on ok"
        );
        assert!(!obj.contains_key("cube_command"), "cube_command must be omitted on ok");
        assert!(!obj.contains_key("cube_cwd"), "cube_cwd must be omitted on ok");

        // details has no skip_serializing_if, so the key stays and is null.
        assert!(obj.contains_key("details"), "details key must always be present");
        assert!(obj["details"].is_null(), "details defaults to JSON null");
    }

    /// Same omission contract holds for a `Skipped` event — the keys
    /// are gated on `Option::is_none`, not on the outcome, but a reader
    /// pinning the skip behaviour on a non-error event must still see
    /// them absent.
    #[test]
    fn skipped_event_omits_skip_serialized_optional_keys() {
        let event = DispatchEvent::new(Stage::WorkerClaimed, Outcome::Skipped, "exec-skip");
        let value = serde_json::to_value(&event).unwrap();
        let obj = value.as_object().unwrap();

        assert!(!obj.contains_key("error_message"));
        assert!(!obj.contains_key("cube_command"));
        assert!(!obj.contains_key("cube_cwd"));
    }

    /// `with_cube_invocation(Some(..))` populates BOTH `cube_command`
    /// and `cube_cwd`, and both survive serialization.
    #[test]
    fn with_cube_invocation_some_sets_both_command_and_cwd() {
        let event = DispatchEvent::new(Stage::CubeWorkspaceLeaseAttempted, Outcome::Ok, "exec-inv")
            .with_cube_invocation(Some((
                "cube workspace lease ci-infra --task \"fix\"".to_owned(),
                "/work/dir".to_owned(),
            )));

        assert_eq!(
            event.cube_command.as_deref(),
            Some("cube workspace lease ci-infra --task \"fix\"")
        );
        assert_eq!(event.cube_cwd.as_deref(), Some("/work/dir"));

        let obj = serde_json::to_value(&event).unwrap();
        assert_eq!(
            obj["cube_command"],
            serde_json::json!("cube workspace lease ci-infra --task \"fix\"")
        );
        assert_eq!(obj["cube_cwd"], serde_json::json!("/work/dir"));
    }

    /// `with_cube_invocation(None)` leaves both fields untouched, so
    /// they stay `None` and are omitted from the JSON object.
    #[test]
    fn with_cube_invocation_none_leaves_both_absent() {
        let event =
            DispatchEvent::new(Stage::CubeWorkspaceLeaseAttempted, Outcome::Ok, "exec-inv").with_cube_invocation(None);

        assert!(event.cube_command.is_none());
        assert!(event.cube_cwd.is_none());

        let obj = serde_json::to_value(&event).unwrap();
        let map = obj.as_object().unwrap();
        assert!(!map.contains_key("cube_command"));
        assert!(!map.contains_key("cube_cwd"));
    }

    /// `with_error` flattens the full anyhow cause chain via `{err:#}`,
    /// so the serialized `error_message` contains the outer context
    /// *and* the root cause, joined by anyhow's `: ` separator.
    #[test]
    fn with_error_flattens_full_anyhow_cause_chain() {
        let err = anyhow::anyhow!("connection refused").context("cube workspace lease failed");
        let event = DispatchEvent::new(Stage::CubeWorkspaceLeaseFailed, Outcome::Error, "exec-err").with_error(&err);

        let obj = serde_json::to_value(&event).unwrap();
        let message = obj["error_message"].as_str().unwrap();

        assert!(
            message.contains("cube workspace lease failed"),
            "outer context missing: {message}"
        );
        assert!(message.contains("connection refused"), "root cause missing: {message}");
        // anyhow's `{:#}` joins each cause with `: `.
        assert_eq!(message, "cube workspace lease failed: connection refused");
    }

    /// The full builder chain populates every optional field and the
    /// values round-trip cleanly through serde back to a `DispatchEvent`
    /// with the expected getters.
    #[test]
    fn full_builder_chain_round_trips_through_serde() {
        let event = DispatchEvent::new(Stage::CubeWorkspaceLeased, Outcome::Ok, "exec-rt")
            .with_work_item("task-rt")
            .with_worker("worker-7")
            .with_cube_repo("repo-9")
            .with_cube_lease("lease-3")
            .with_cube_workspace("ws-2")
            .with_details(serde_json::json!({ "host_id": "host-1", "did_dispatch": true }));

        let line = serde_json::to_string(&event).unwrap();
        let restored: DispatchEvent = serde_json::from_str(&line).unwrap();

        assert_eq!(restored.stage, "cube_workspace_leased");
        assert_eq!(restored.outcome, "ok");
        assert_eq!(restored.execution_id, "exec-rt");
        assert_eq!(restored.work_item_id.as_deref(), Some("task-rt"));
        assert_eq!(restored.worker_id.as_deref(), Some("worker-7"));
        assert_eq!(restored.cube_repo_id.as_deref(), Some("repo-9"));
        assert_eq!(restored.cube_lease_id.as_deref(), Some("lease-3"));
        assert_eq!(restored.cube_workspace_id.as_deref(), Some("ws-2"));
        assert_eq!(restored.details["host_id"], serde_json::json!("host-1"));
        assert_eq!(restored.details["did_dispatch"], serde_json::json!(true));
        assert_eq!(restored.ts_epoch_ms, event.ts_epoch_ms);
    }

    /// A minimal JSON line that omits every skip-serialized optional key
    /// (and `details`) still deserializes — this is the forward/backward
    /// compat guarantee for the wire shape. The absent optionals default
    /// to `None` and `details` defaults to JSON `null`.
    #[test]
    fn deserializes_from_minimal_line_omitting_optional_keys() {
        let line = r#"{
            "ts_epoch_ms": 1700000000000,
            "stage": "request_recorded",
            "outcome": "ok",
            "execution_id": "exec-min"
        }"#;

        let event: DispatchEvent = serde_json::from_str(line).unwrap();

        assert_eq!(event.ts_epoch_ms, 1_700_000_000_000);
        assert_eq!(event.stage, "request_recorded");
        assert_eq!(event.outcome, "ok");
        assert_eq!(event.execution_id, "exec-min");
        assert!(event.work_item_id.is_none());
        assert!(event.worker_id.is_none());
        assert!(event.cube_repo_id.is_none());
        assert!(event.cube_lease_id.is_none());
        assert!(event.cube_workspace_id.is_none());
        assert!(event.error_message.is_none());
        assert!(event.cube_command.is_none());
        assert!(event.cube_cwd.is_none());
        assert!(event.details.is_null());
    }

    /// `Stage::as_str` is the on-disk stage identifier ledger consumers
    /// pin against; a silent rename would break them. Pin every variant
    /// to its exact snake_case string.
    #[test]
    fn stage_as_str_pins_exact_snake_case_identifiers() {
        assert_eq!(Stage::StatusTransition.as_str(), "status_transition");
        assert_eq!(Stage::RequestRecorded.as_str(), "request_recorded");
        assert_eq!(Stage::WorkerClaimed.as_str(), "worker_claimed");
        assert_eq!(Stage::HostSelected.as_str(), "host_selected");
        assert_eq!(Stage::CubeRepoEnsureAttempted.as_str(), "cube_repo_ensure_attempted");
        assert_eq!(Stage::CubeRepoEnsured.as_str(), "cube_repo_ensured");
        assert_eq!(Stage::CubeRepoEnsureFailed.as_str(), "cube_repo_ensure_failed");
        assert_eq!(
            Stage::CubeWorkspaceLeaseAttempted.as_str(),
            "cube_workspace_lease_attempted"
        );
        assert_eq!(Stage::CubeWorkspaceLeased.as_str(), "cube_workspace_leased");
        assert_eq!(Stage::CubeWorkspaceLeaseFailed.as_str(), "cube_workspace_lease_failed");
        assert_eq!(Stage::CubeChangeCreated.as_str(), "cube_change_created");
        assert_eq!(Stage::RunStarted.as_str(), "run_started");
        assert_eq!(Stage::PaneSpawned.as_str(), "pane_spawned");
        assert_eq!(Stage::ExecutionCancelled.as_str(), "execution_cancelled");
        assert_eq!(Stage::SpawnFailed.as_str(), "spawn_failed");
        assert_eq!(Stage::StageStalled.as_str(), "stage_stalled");
        assert_eq!(Stage::OrphanActiveRedispatch.as_str(), "orphan_active_redispatch");
        assert_eq!(Stage::PrReviewDeadRecovery.as_str(), "pr_review_dead_recovery");
        assert_eq!(Stage::DeadPidReconcile.as_str(), "dead_pid_reconcile");
        assert_eq!(Stage::PaneDeathReconcile.as_str(), "pane_death_reconcile");
        assert_eq!(Stage::DispatchDecision.as_str(), "dispatch_decision");
        assert_eq!(Stage::TransientRecovery.as_str(), "transient_recovery");
        assert_eq!(
            Stage::TransientRecoveryExhausted.as_str(),
            "transient_recovery_exhausted"
        );
        assert_eq!(Stage::TransientRecoveryNudge.as_str(), "transient_recovery_nudge");
        assert_eq!(Stage::StaleWorkerReconcile.as_str(), "stale_worker_reconcile");
        assert_eq!(Stage::PoolClaimReconcile.as_str(), "pool_claim_reconcile");
        assert_eq!(Stage::TerminalWorkReconcile.as_str(), "terminal_work_reconcile");
        assert_eq!(Stage::CubeLeaseHeartbeat.as_str(), "cube_lease_heartbeat");
        assert_eq!(Stage::LostWorkspaceReconcile.as_str(), "lost_workspace_reconcile");
        assert_eq!(Stage::CubeLeaseAutoReap.as_str(), "cube_lease_auto_reap");
        assert_eq!(Stage::RemoteLeaseReconcile.as_str(), "remote_lease_reconcile");
        assert_eq!(Stage::HostDrainReconcile.as_str(), "host_drain_reconcile");
        assert_eq!(Stage::SpawnAckTimeout.as_str(), "spawn_ack_timeout");
        assert_eq!(Stage::DriverStartTimeout.as_str(), "driver_start_timeout");
        assert_eq!(
            Stage::DispatchFailureRecoveryRedispatch.as_str(),
            "dispatch_failure_recovery_redispatch"
        );
        assert_eq!(Stage::SpawnNack.as_str(), "spawn_nack");
        assert_eq!(Stage::PaneDeathBeforeStart.as_str(), "pane_death_before_start");
        assert_eq!(Stage::SpawnCapabilityUnhealthy.as_str(), "spawn_capability_unhealthy");
        assert_eq!(Stage::SpawnCapabilityRecovered.as_str(), "spawn_capability_recovered");
        assert_eq!(Stage::HuskPaneReconcile.as_str(), "husk_pane_reconcile");
        assert_eq!(
            Stage::ExecutionLivenessReconcile.as_str(),
            "execution_liveness_reconcile"
        );
        assert_eq!(Stage::DispatchPaused.as_str(), "dispatch_paused");
        assert_eq!(Stage::DispatchResumed.as_str(), "dispatch_resumed");
        assert_eq!(Stage::AutomationPreempted.as_str(), "automation_preempted");
        assert_eq!(Stage::LiveWorkerReadopted.as_str(), "live_worker_readopted");
        assert_eq!(
            Stage::RedispatchBlockedLiveProcess.as_str(),
            "redispatch_blocked_live_process"
        );
        assert_eq!(Stage::RedispatchGuardDeclined.as_str(), "redispatch_guard_declined");
        assert_eq!(Stage::TmuxAdopt.as_str(), "tmux_adopt");
        assert_eq!(Stage::TmuxRefuseSkew.as_str(), "tmux_refuse_skew");
        assert_eq!(Stage::TmuxLeakDetected.as_str(), "tmux_leak_detected");
        assert_eq!(Stage::TmuxTokenMismatch.as_str(), "tmux_token_mismatch");
        assert_eq!(
            Stage::TmuxAdoptionOwnerConflict.as_str(),
            "tmux_adoption_owner_conflict"
        );
        assert_eq!(Stage::StartupPaneRespawn.as_str(), "startup_pane_respawn");
        assert_eq!(Stage::ExecutionFinalized.as_str(), "execution_finalized");
    }

    /// `from_wire` must invert `as_str` — the serde rename and the
    /// hand-written table are two spellings of the same contract.
    #[test]
    fn from_wire_inverts_as_str() {
        for stage in Stage::ALL {
            assert_eq!(Stage::from_wire(stage.as_str()), Some(stage), "{}", stage.as_str());
        }
        assert_eq!(Stage::from_wire("not_a_stage"), None);
        assert_eq!(Stage::from_wire(""), None);
    }

    /// The pipeline stages a run can be stuck in stay on the pipeline
    /// side; the observations that get appended to an already-dispatched
    /// run's timeline are post-dispatch. `tmux_adopt` is the one the
    /// 2026-09-04 false-stall incident turned on.
    #[test]
    fn post_dispatch_classification_pins_the_incident_stages() {
        for stage in Stage::ALL {
            assert!(matches!(
                stage.timeline_class(),
                TimelineStageClass::Pipeline | TimelineStageClass::PostDispatch | TimelineStageClass::Observation
            ));
        }
        for stage in [
            Stage::DispatchDecision,
            Stage::AutomationPreempted,
            Stage::CubeLeaseHeartbeat,
        ] {
            assert_eq!(
                stage.timeline_class(),
                TimelineStageClass::Observation,
                "{} is a timeline observation",
                stage.as_str()
            );
        }
        for stage in [Stage::TmuxAdopt, Stage::ExecutionFinalized, Stage::DeadPidReconcile] {
            assert_eq!(
                stage.timeline_class(),
                TimelineStageClass::PostDispatch,
                "{} is post-dispatch",
                stage.as_str()
            );
        }
    }

    /// `Outcome::as_str` strings are the on-disk outcome identifiers;
    /// pin them exactly. The serde `rename_all = "snake_case"`
    /// serialization must agree with `as_str`.
    #[test]
    fn outcome_as_str_pins_exact_identifiers() {
        assert_eq!(Outcome::Ok.as_str(), "ok");
        assert_eq!(Outcome::Error.as_str(), "error");
        assert_eq!(Outcome::Skipped.as_str(), "skipped");

        // serde serialization must agree with as_str so the JSON
        // `outcome` field and the in-memory identifier never diverge.
        for outcome in [Outcome::Ok, Outcome::Error, Outcome::Skipped] {
            assert_eq!(
                serde_json::to_value(outcome).unwrap(),
                serde_json::json!(outcome.as_str())
            );
        }
    }
}
