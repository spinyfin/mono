//! Periodic reconciler that drains executions off hosts that have gone
//! offline — the fix for the 2026-07-03 anaplian incident.
//!
//! Disabling a host (operator `bossctl hosts disable`, the dispatch-health
//! circuit breaker in [`WorkDb::record_host_dispatch_failure`], or an
//! `eager_push_wrapper_rpc` failure) removes it only from *future*
//! [`crate::host_scheduling::select_host`] picks. **Nothing reconciles the
//! executions already routed to it.** That night, when the operator
//! disabled anaplian, its in-flight `pr_review` executions stayed stuck:
//! their cube-lease heartbeats errored forever with no re-route, and queued
//! work behind them waited until the coordinator manually reaped each
//! phantom one by one. Only brand-new dispatches landed locally.
//!
//! This sweep closes that gap. Each pass:
//!
//! 1. Queries every non-terminal execution whose latest `work_runs.host_id`
//!    is a host that is now **offline** — disabled or removed from the
//!    registry (`local` runs are excluded; they are judged by the
//!    local-filesystem sweeps). This is `queued`/`leased`/`run_started`/
//!    `heartbeat-failing` uniformly: the trigger is the *host* being gone,
//!    not the execution's stage.
//! 2. Terminalizes each via [`WorkDb::mark_execution_orphaned`] — the exact
//!    same terminal path as `bossctl agents reap`, so the redundant-spawn
//!    guard's `is_live()` check stops treating the row as live.
//! 3. Best-effort force-releases the cube lease (very likely already a
//!    no-op — the host is unreachable — so failure is benign).
//! 4. Emits a [`Stage::HostDrainReconcile`] dispatch event so the drain is
//!    visible in `bossctl dispatch tail`.
//! 5. Kicks the coordinator so the existing orphan-active sweep
//!    ([`crate::orphan_sweep`]) re-dispatches the freed work item to a
//!    still-eligible host (e.g. `local`) — `select_host` already excludes
//!    the disabled host, so the re-route is automatic.
//!
//! Runs every [`DEFAULT_INTERVAL`] and fires immediately on boot, so a host
//! disabled while the engine was down is drained at startup without waiting
//! for the first interval.

use std::sync::Arc;
use std::time::Duration;

use boss_protocol::ExecutionKind;

use crate::coordinator::{CubeClient, ExecutionCoordinator};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::work::{HostBoundExecution, WorkDb};

/// How often the host-reconcile sweep runs. Matches the other steady-state
/// reconcilers (orphan / pool-claim / lost-workspace, all 60s) so a host
/// disable is drained within a bounded window without a tight busy-loop.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// Counts from one pass; logged at `info` when non-zero.
#[derive(Debug, Default)]
pub struct HostReconcileOutcome {
    /// Executions terminalized this pass because their host went offline.
    pub reaped: usize,
    /// Executions whose orphan-mark failed (already terminal via a racing
    /// sweep, or a DB error) — informational, not fatal.
    pub reap_skipped: usize,
    /// Best-effort cube lease releases that errored (host unreachable is
    /// the expected case).
    pub release_failed: usize,
}

impl crate::sweep_loop::SweepOutcome for HostReconcileOutcome {
    fn has_activity(&self) -> bool {
        self.reaped > 0 || self.reap_skipped > 0
    }

    fn log(&self) {
        tracing::info!(
            reaped = self.reaped,
            reap_skipped = self.reap_skipped,
            release_failed = self.release_failed,
            "host reconcile: pass complete",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn so a host disabled while the engine was down
/// is drained at boot.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    cube_client: Arc<dyn CubeClient>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let cube_client = Arc::clone(&cube_client);
        let coordinator = Arc::clone(&coordinator);
        let dispatch_events = Arc::clone(&dispatch_events);
        async move {
            let outcome = run_one_pass(work_db.as_ref(), cube_client.as_ref(), dispatch_events.as_ref()).await;
            if outcome.reaped > 0 {
                // A drain frees the work item's redundant-spawn blocker;
                // kick the scheduler so the orphan-active sweep's fresh
                // `ready` execution dispatches to a still-eligible host
                // immediately instead of waiting for an opportunistic kick.
                coordinator.kick();
            }
            outcome
        }
    })
}

/// Run a single host-reconcile pass: drain every non-terminal execution
/// bound to an offline host. Returns a summary; callers may log it.
pub async fn run_one_pass(
    work_db: &WorkDb,
    cube_client: &dyn CubeClient,
    dispatch_events: &dyn DispatchEventSink,
) -> HostReconcileOutcome {
    let mut outcome = HostReconcileOutcome::default();

    let bound = match work_db.list_nonterminal_executions_on_offline_hosts() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(?err, "host reconcile: failed to list bound executions; skipping pass");
            return outcome;
        }
    };

    for entry in bound {
        drain_execution(work_db, cube_client, dispatch_events, &entry, &mut outcome).await;
    }

    outcome
}

/// Terminalize one execution off an offline host, release its lease, and
/// emit the reconcile event. Idempotent against a race with any other
/// reconciler: if the row is already terminal by the time we get here,
/// `mark_execution_orphaned` errors and we count it as `reap_skipped`.
async fn drain_execution(
    work_db: &WorkDb,
    cube_client: &dyn CubeClient,
    dispatch_events: &dyn DispatchEventSink,
    entry: &HostBoundExecution,
    outcome: &mut HostReconcileOutcome,
) {
    let execution = &entry.execution;
    let host_id = entry.host_id.as_str();

    // Label the reason from the host's current state: absent row = removed,
    // present-but-disabled = disabled. A lookup failure defaults to the
    // generic "offline" wording rather than blocking the drain.
    let reason_kind = match work_db.get_host(host_id) {
        Ok(None) => "host_removed",
        Ok(Some(_)) => "host_disabled",
        Err(err) => {
            tracing::warn!(
                execution_id = %execution.id,
                host_id,
                error = %format!("{err:#}"),
                "host reconcile: failed to read host state for reason label; proceeding with drain",
            );
            "host_offline"
        }
    };
    let reason = format!(
        "host-reconcile: host `{host_id}` is offline ({reason_kind}); draining execution off it \
         and re-routing its work item (same terminal path as `bossctl agents reap`)"
    );

    if let Err(err) = work_db.mark_execution_orphaned(&execution.id, &reason) {
        // A concurrent sweep/guard/hook may have finalized this execution
        // between our snapshot and now — that's success from our
        // perspective (the phantom is gone), just not our credit.
        outcome.reap_skipped += 1;
        tracing::debug!(
            execution_id = %execution.id,
            host_id,
            error = %format!("{err:#}"),
            "host reconcile: mark_execution_orphaned did not apply (already terminal or DB error)",
        );
        return;
    }
    outcome.reaped += 1;

    // Preserve the automation-triage open-task-recovery bookkeeping the
    // other reap paths do, so a triage that produced a task before its host
    // was disabled is recorded honestly rather than as a silent drop.
    if execution.kind == ExecutionKind::AutomationTriage {
        crate::execution_liveness::finalize_dead_automation_triage_run(
            work_db,
            execution,
            &format!("its host `{host_id}` was disabled/removed while the triage was in flight"),
        );
    }

    // A drained `pr_review` execution held its task in the Doing column
    // (P992 `PendingReview`: `active` + `pr_url`). Re-routing it as a fresh
    // *implementation* would be wrong — the PR already exists — so advance
    // the task to `in_review` via the same idempotent, single-live-worker-
    // guarded helper the stalled-reviewer fallback uses. That lands the card
    // in the human Review lane (and takes it out of orphan-active
    // redispatch's candidate set) rather than stranding it in Doing.
    // Re-firing the review itself is out of scope here (a separate
    // review-pipeline-resilience chore owns that); this only ensures the
    // drained reviewer's task is not left stuck.
    if execution.kind == ExecutionKind::PrReview {
        match work_db.advance_pending_review_task_to_in_review(&execution.work_item_id) {
            Ok(true) => tracing::info!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                "host reconcile: drained pr_review's task advanced to in_review (reviewer fallback)",
            ),
            Ok(false) => {} // Already past `active`, or a live implementation worker holds it.
            Err(err) => tracing::warn!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                error = %format!("{err:#}"),
                "host reconcile: failed to advance drained pr_review's task to in_review",
            ),
        }
    }

    // Best-effort lease release. The host is offline, so the cube call very
    // likely fails or is a no-op; either way the lease will TTL-expire.
    if let Some(lease_id) = execution.cube_lease_id.as_deref()
        && let Err(err) = cube_client
            .force_release_lease(lease_id, Some("host-reconcile drain: host offline"))
            .await
    {
        outcome.release_failed += 1;
        tracing::debug!(
            execution_id = %execution.id,
            lease_id,
            host_id,
            error = %format!("{err:#}"),
            "host reconcile: best-effort lease release failed (host offline; lease will TTL-expire)",
        );
    }

    let mut event = DispatchEvent::new(Stage::HostDrainReconcile, Outcome::Ok, &execution.id)
        .with_work_item(&execution.work_item_id)
        .with_details(serde_json::json!({
            "host_id": host_id,
            "reason": reason_kind,
            "prior_status": execution.status,
            "kind": execution.kind.as_str(),
            "run_id": entry.run_id,
        }));
    if let Some(lease_id) = execution.cube_lease_id.as_deref() {
        event = event.with_cube_lease(lease_id);
    }
    dispatch_events.emit(event).await;

    tracing::warn!(
        execution_id = %execution.id,
        work_item_id = %execution.work_item_id,
        host_id,
        reason = reason_kind,
        "host reconcile: drained execution off offline host; work item will be re-routed",
    );
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use boss_protocol::{CreateExecutionInput, ExecutionStatus, RequestExecutionInput, WorkExecution};
    use tempfile::TempDir;

    use super::*;
    use crate::coordinator::{
        CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
        ExecutionCoordinator, WorkerPool,
    };
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::host_scheduling::{self, ChoreRequirements, HostSlot};
    use crate::runner::{ExecutionRunner, RunOutcome};
    use crate::test_support::create_test_product_with_repo;
    use crate::work::{CreateChoreInput, WorkDb};

    /// No-op execution runner: the real re-route tests only need
    /// `orphan_sweep::run_one_pass` to produce a fresh `ready` execution;
    /// nothing actually drives it to completion.
    struct NoopRunner;

    #[async_trait]
    impl ExecutionRunner for NoopRunner {
        async fn run_execution(
            &self,
            _worker_id: &str,
            _execution: &WorkExecution,
            _work_item: &crate::work::WorkItem,
            _workspace_path: &Path,
            _cube_change_id: Option<&str>,
        ) -> Result<RunOutcome> {
            unimplemented!("host reconcile e2e re-route test doesn't run executions")
        }
    }

    // ─── cube stub ────────────────────────────────────────────────────────────

    /// Records `force_release_lease` calls; every other method is
    /// unreachable in the host-reconcile path.
    #[derive(Default)]
    struct FakeCube {
        force_releases: Mutex<Vec<String>>,
        release_fail: bool,
    }

    impl FakeCube {
        fn force_release_calls(&self) -> Vec<String> {
            self.force_releases.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CubeClient for FakeCube {
        async fn ensure_repo(&self, _: &str) -> Result<CubeRepoHandle> {
            unimplemented!()
        }
        async fn lease_workspace(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: bool,
            _: &[&str],
        ) -> Result<CubeWorkspaceLease> {
            unimplemented!()
        }
        async fn create_change(&self, _: &Path, _: &str) -> Result<CubeChangeHandle> {
            unimplemented!()
        }
        async fn release_workspace(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn workspace_status(&self, _: &Path) -> Result<CubeWorkspaceStatus> {
            unimplemented!()
        }
        async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
            unimplemented!()
        }
        async fn force_release_lease(&self, lease_id: &str, _: Option<&str>) -> Result<()> {
            self.force_releases.lock().unwrap().push(lease_id.to_owned());
            if self.release_fail {
                return Err(anyhow!("simulated force-release failure (host offline)"));
            }
            Ok(())
        }
        async fn goto_workspace(&self, _: &Path, _: u64) -> Result<()> {
            unimplemented!()
        }
        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            unimplemented!()
        }
        async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
            Ok(vec![])
        }
    }

    // ─── helpers ─────────────────────────────────────────────────────────────

    fn open_db() -> (TempDir, WorkDb) {
        let dir = TempDir::new().unwrap();
        let db = WorkDb::open(dir.path().join("state.db")).unwrap();
        (dir, db)
    }

    fn create_product(db: &WorkDb) -> String {
        create_test_product_with_repo(db, "test-product", Some("https://github.com/test/repo")).id
    }

    fn create_chore(db: &WorkDb, product_id: &str, name: &str) -> String {
        db.create_chore(CreateChoreInput::builder().product_id(product_id).name(name).build())
            .unwrap()
            .id
    }

    fn ready_execution(db: &WorkDb, work_item_id: &str) -> String {
        db.request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
            .unwrap()
            .id
    }

    /// Create a `running` execution whose run is attributed to `host_id`
    /// (models an in-flight worker on that host).
    fn running_execution_on_host(db: &WorkDb, work_item_id: &str, lease_id: &str, host_id: &str) -> String {
        let execution_id = ready_execution(db, work_item_id);
        db.start_execution_run_on_host(
            &execution_id,
            "agent-1",
            "repo",
            lease_id,
            "mono-agent-001",
            "/tmp/mono-agent-001",
            host_id,
        )
        .unwrap();
        execution_id
    }

    fn add_remote_host(db: &WorkDb, id: &str) {
        db.add_host(id, &format!("user@{id}"), 4, &[]).unwrap();
    }

    fn task_status(db: &WorkDb, task_id: &str) -> String {
        db.connect()
            .unwrap()
            .query_row("SELECT status FROM tasks WHERE id = ?1", [task_id], |r| r.get(0))
            .unwrap()
    }

    /// Create a `running` `pr_review` execution on `host_id` and put its
    /// task into the P992 PendingReview hold (`active` + `pr_url`), the state
    /// an independent reviewer runs against.
    fn running_pr_review_on_host(db: &WorkDb, work_item_id: &str, lease_id: &str, host_id: &str) -> String {
        let execution_id = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(work_item_id)
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url("https://github.com/test/repo")
                    .build(),
            )
            .unwrap()
            .id;
        db.start_execution_run_on_host(
            &execution_id,
            "reviewer-1",
            "repo",
            lease_id,
            "mono-agent-002",
            "/tmp/mono-agent-002",
            host_id,
        )
        .unwrap();
        // Stamp the task's pr_url so it reads as PendingReview (Doing hold).
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET pr_url = 'https://github.com/test/repo/pull/1' WHERE id = ?1",
                [work_item_id],
            )
            .unwrap();
        execution_id
    }

    /// Build the `HostSlot`s the coordinator would hand `select_host`, from
    /// the live registry state — the routing side of the re-dispatch.
    fn host_slots(db: &WorkDb) -> Vec<HostSlot> {
        let active = db.active_runs_per_host().unwrap();
        db.list_hosts()
            .unwrap()
            .into_iter()
            .map(|host| {
                let active_runs = if host.id == "local" {
                    0
                } else {
                    *active.get(&host.id).unwrap_or(&0)
                };
                HostSlot {
                    host,
                    capabilities: BTreeSet::new(),
                    active_runs,
                    had_prior_run_on_branch: false,
                }
            })
            .collect()
    }

    // ─── list query ──────────────────────────────────────────────────────────

    #[test]
    fn list_query_selects_only_offline_host_bindings() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        add_remote_host(&db, "anaplian");
        add_remote_host(&db, "zakalwe");

        // In-flight on the soon-to-be-disabled host.
        let doomed_wi = create_chore(&db, &product, "doomed");
        let doomed = running_execution_on_host(&db, &doomed_wi, "lease-doomed", "anaplian");
        // In-flight on a host that stays enabled — must be untouched.
        let healthy_wi = create_chore(&db, &product, "healthy-remote");
        let healthy = running_execution_on_host(&db, &healthy_wi, "lease-healthy", "zakalwe");
        // In-flight locally — never selected by this query.
        let local_wi = create_chore(&db, &product, "local");
        running_execution_on_host(&db, &local_wi, "lease-local", "local");
        // Terminal on the disabled host — already settled, must be excluded.
        let done_wi = create_chore(&db, &product, "already-done");
        let done = running_execution_on_host(&db, &done_wi, "lease-done", "anaplian");
        db.mark_execution_orphaned(&done, "pre-existing terminal").unwrap();

        db.set_host_enabled("anaplian", false).unwrap();

        let rows = db.list_nonterminal_executions_on_offline_hosts().unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.execution.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![doomed.as_str()],
            "only the non-terminal anaplian binding qualifies"
        );
        assert_eq!(rows[0].host_id, "anaplian");
        assert!(
            !ids.contains(&healthy.as_str()),
            "an enabled host's execution is not drained"
        );
    }

    #[test]
    fn list_query_treats_removed_host_as_offline() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        add_remote_host(&db, "anaplian");
        let wi = create_chore(&db, &product, "orphan-of-removed-host");
        let exec = running_execution_on_host(&db, &wi, "lease-x", "anaplian");

        // Remove the host row entirely (operator `hosts remove`).
        db.remove_host("anaplian").unwrap();

        let rows = db.list_nonterminal_executions_on_offline_hosts().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].execution.id, exec);
        assert_eq!(rows[0].host_id, "anaplian");
    }

    #[test]
    fn execution_bound_host_offline_distinguishes_states() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        add_remote_host(&db, "anaplian");

        let on_disabled = running_execution_on_host(&db, &create_chore(&db, &product, "a"), "l1", "anaplian");
        let on_local = running_execution_on_host(&db, &create_chore(&db, &product, "b"), "l2", "local");
        let no_run = ready_execution(&db, &create_chore(&db, &product, "c"));

        // Enabled host → not offline.
        assert!(!db.execution_bound_host_offline(&on_disabled).unwrap());
        db.set_host_enabled("anaplian", false).unwrap();
        // Disabled host → offline.
        assert!(db.execution_bound_host_offline(&on_disabled).unwrap());
        // Local run is never offline.
        assert!(!db.execution_bound_host_offline(&on_local).unwrap());
        // No run yet → nothing bound → not offline.
        assert!(!db.execution_bound_host_offline(&no_run).unwrap());
    }

    // ─── the sweep ───────────────────────────────────────────────────────────

    /// The core drain: two non-terminal executions bound to a disabled host
    /// are both orphaned, their leases best-effort released, and a
    /// `host_drain_reconcile` event is emitted for each.
    #[tokio::test]
    async fn run_one_pass_drains_disabled_host_bindings() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        add_remote_host(&db, "anaplian");

        let inflight_wi = create_chore(&db, &product, "in-flight");
        let inflight = running_execution_on_host(&db, &inflight_wi, "lease-inflight", "anaplian");
        let hb_wi = create_chore(&db, &product, "heartbeat-failing");
        let hb = running_execution_on_host(&db, &hb_wi, "lease-hb", "anaplian");

        db.set_host_enabled("anaplian", false).unwrap();

        let cube = FakeCube::default();
        let sink = RecordingDispatchEventSink::new();
        let outcome = run_one_pass(&db, &cube, &sink).await;

        assert_eq!(outcome.reaped, 2, "both bound executions are drained");
        assert_eq!(db.get_execution(&inflight).unwrap().status, ExecutionStatus::Orphaned);
        assert_eq!(db.get_execution(&hb).unwrap().status, ExecutionStatus::Orphaned);

        let mut released = cube.force_release_calls();
        released.sort();
        assert_eq!(released, vec!["lease-hb".to_owned(), "lease-inflight".to_owned()]);

        let events = sink.events().await;
        let drains: Vec<_> = events.iter().filter(|e| e.stage == "host_drain_reconcile").collect();
        assert_eq!(drains.len(), 2, "one drain event per execution");
        assert!(drains.iter().all(|e| e.outcome == "ok"));
        assert!(drains.iter().all(|e| e.details["host_id"] == "anaplian"));
        assert!(drains.iter().all(|e| e.details["reason"] == "host_disabled"));
    }

    /// An enabled host's in-flight execution must NOT be drained — the sweep
    /// only acts on offline hosts.
    #[tokio::test]
    async fn run_one_pass_leaves_enabled_host_alone() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        add_remote_host(&db, "zakalwe");
        let wi = create_chore(&db, &product, "healthy");
        let exec = running_execution_on_host(&db, &wi, "lease-ok", "zakalwe");

        let cube = FakeCube::default();
        let sink = RecordingDispatchEventSink::new();
        let outcome = run_one_pass(&db, &cube, &sink).await;

        assert_eq!(outcome.reaped, 0);
        assert_eq!(db.get_execution(&exec).unwrap().status, ExecutionStatus::Running);
        assert!(cube.force_release_calls().is_empty());
    }

    /// A best-effort lease release that fails (the host is unreachable) does
    /// not block the drain — the execution is still terminalized.
    #[tokio::test]
    async fn drain_survives_lease_release_failure() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        add_remote_host(&db, "anaplian");
        let wi = create_chore(&db, &product, "unreachable");
        let exec = running_execution_on_host(&db, &wi, "lease-gone", "anaplian");
        db.set_host_enabled("anaplian", false).unwrap();

        let cube = FakeCube {
            release_fail: true,
            ..Default::default()
        };
        let sink = RecordingDispatchEventSink::new();
        let outcome = run_one_pass(&db, &cube, &sink).await;

        assert_eq!(outcome.reaped, 1);
        assert_eq!(outcome.release_failed, 1);
        assert_eq!(db.get_execution(&exec).unwrap().status, ExecutionStatus::Orphaned);
    }

    // ─── end-to-end re-route ────────────────────────────────────────────────

    /// The verification the incident demands: disabling a host with an
    /// in-flight execution drains it, the freed work item becomes an
    /// orphan-active redispatch candidate, a fresh `ready` execution is
    /// created for it, and host selection routes that fresh execution to the
    /// still-enabled `local` host — all without operator intervention.
    #[tokio::test]
    async fn disabling_host_reroutes_stuck_execution_to_local() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        add_remote_host(&db, "anaplian");

        let wi = create_chore(&db, &product, "t2213-review");
        let stuck = running_execution_on_host(&db, &wi, "lease-stuck", "anaplian");
        // start_execution_run flipped the task to `active` (Doing column).
        assert!(db.get_execution(&stuck).unwrap().status == ExecutionStatus::Running);

        // Operator disables the broken host.
        db.set_host_enabled("anaplian", false).unwrap();

        // The reconcile sweep drains it.
        let cube = FakeCube::default();
        let sink = RecordingDispatchEventSink::new();
        let outcome = run_one_pass(&db, &cube, &sink).await;
        assert_eq!(outcome.reaped, 1);
        assert_eq!(db.get_execution(&stuck).unwrap().status, ExecutionStatus::Orphaned);

        // Back-date the task's `updated_at` by a second so the min-age-0
        // query below deterministically sees it as strictly in the past —
        // without this, a fast test run can land the drain and the query
        // in the same wall-clock second, and `updated_at < now` (strict)
        // flakes depending on which side of the second boundary each lands
        // on.
        let one_second_ago = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(1) as i64;
        db.force_updated_at_for_test(&wi, one_second_ago).unwrap();

        // The freed work item is now an orphan-active redispatch candidate
        // (min-age 0 for the test — the sweep uses ORPHAN_MIN_AGE_SECS).
        let candidates = db.list_orphan_active_candidates(0).unwrap();
        assert!(
            candidates.contains(&wi),
            "drained work item must be eligible for orphan-active redispatch; got {candidates:?}"
        );

        // Re-dispatch it via the exact primitive the orphan sweep uses.
        let fresh = db
            .request_execution_with_live_check(
                RequestExecutionInput::builder().work_item_id(wi.clone()).build(),
                |_| false,
            )
            .unwrap();
        assert_eq!(
            fresh.status,
            ExecutionStatus::Ready,
            "a fresh ready execution is created"
        );
        assert_ne!(
            fresh.id, stuck,
            "the re-dispatch is a new execution, not the drained one"
        );

        // Host selection for that fresh execution routes to `local`: anaplian
        // is disabled, so `select_host` excludes it.
        let (picked, _report) = host_scheduling::select_host(&ChoreRequirements::default(), &host_slots(&db));
        assert_eq!(
            picked.as_deref(),
            Some("local"),
            "re-dispatch must land on the still-enabled host"
        );
    }

    /// True end-to-end re-route: after `host_reconcile::run_one_pass` drains
    /// the stuck execution, actually drive `orphan_sweep::run_one_pass` (the
    /// real redispatch machinery, churn guard and running-pr_review guard
    /// included) rather than manually replaying its primitives, and assert
    /// it produces a fresh `ready` execution for the freed work item.
    #[tokio::test]
    async fn disabling_host_reroutes_via_real_orphan_sweep_pass() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        add_remote_host(&db, "anaplian");

        let wi = create_chore(&db, &product, "t2213-e2e-reroute");
        let stuck = running_execution_on_host(&db, &wi, "lease-stuck-e2e", "anaplian");
        assert_eq!(db.get_execution(&stuck).unwrap().status, ExecutionStatus::Running);

        // Age the item past ORPHAN_MIN_AGE_SECS so the real sweep's DB query
        // (not a min-age-0 test bypass) picks it up as a candidate.
        let old_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(crate::orphan_sweep::ORPHAN_MIN_AGE_SECS as u64 + 60) as i64;
        db.force_updated_at_for_test(&wi, old_epoch).unwrap();

        db.set_host_enabled("anaplian", false).unwrap();

        // host_reconcile drains the stuck execution off the disabled host.
        let cube = FakeCube::default();
        let sink = RecordingDispatchEventSink::new();
        let outcome = run_one_pass(&db, &cube, &sink).await;
        assert_eq!(outcome.reaped, 1);
        assert_eq!(db.get_execution(&stuck).unwrap().status, ExecutionStatus::Orphaned);

        // Now drive the actual orphan-active sweep, not a hand-rolled replay
        // of its primitives.
        let db = Arc::new(db);
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            Arc::new(FakeCube::default()),
            Arc::new(NoopRunner),
        ));
        let sweep_sink = RecordingDispatchEventSink::new();
        let sweep_outcome = crate::orphan_sweep::run_one_pass(db.as_ref(), coordinator, &sweep_sink).await;

        assert_eq!(
            sweep_outcome.redispatched, 1,
            "the real orphan sweep must redispatch the drained work item"
        );
        let executions = db.list_executions(Some(&wi)).unwrap();
        let fresh = executions
            .iter()
            .find(|e| e.status == ExecutionStatus::Ready)
            .expect("a fresh ready execution must exist after the real sweep pass");
        assert_ne!(
            fresh.id, stuck,
            "the re-dispatch is a new execution, not the drained one"
        );
    }

    /// A work item that already churned (repeated terminal executions in the
    /// trailing window, e.g. from prior phantom reaps during the same
    /// incident) can have the drain itself push it over
    /// `ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD`. This is intentional: a
    /// chronically-failing item should escalate to a human rather than keep
    /// auto-redispatching, even when the most recent failure was "host went
    /// offline" rather than the item's own fault. Document the interaction
    /// with a test so a future change to either guard notices if it breaks.
    #[tokio::test]
    async fn drain_can_trip_churn_guard_for_already_churning_item() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        add_remote_host(&db, "anaplian");

        let wi = create_chore(&db, &product, "t2213-already-churning");
        let stuck = running_execution_on_host(&db, &wi, "lease-churn", "anaplian");

        let old_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(crate::orphan_sweep::ORPHAN_MIN_AGE_SECS as u64 + 60) as i64;
        db.force_updated_at_for_test(&wi, old_epoch).unwrap();

        // Simulate prior phantom reaps during the incident: enough recent
        // terminal executions to be one away from tripping the guard.
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for i in 0..(crate::work::ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD - 1) {
            db.insert_terminal_execution_for_test(&wi, "orphaned", now_epoch - i)
                .unwrap();
        }

        db.set_host_enabled("anaplian", false).unwrap();

        // The drain itself is the item's threshold-th terminal execution in
        // the window.
        let cube = FakeCube::default();
        let sink = RecordingDispatchEventSink::new();
        let outcome = run_one_pass(&db, &cube, &sink).await;
        assert_eq!(outcome.reaped, 1);
        assert_eq!(db.get_execution(&stuck).unwrap().status, ExecutionStatus::Orphaned);

        let db = Arc::new(db);
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            Arc::new(FakeCube::default()),
            Arc::new(NoopRunner),
        ));
        let sweep_sink = RecordingDispatchEventSink::new();
        let sweep_outcome = crate::orphan_sweep::run_one_pass(db.as_ref(), coordinator, &sweep_sink).await;

        assert_eq!(
            sweep_outcome.churn_skipped, 1,
            "an item that already churned during the incident is left for a human, by design, \
             even though the most recent failure was a host-offline drain rather than its own fault"
        );
        assert_eq!(sweep_outcome.redispatched, 0);
    }

    /// A drained `pr_review` execution (the incident's actual casualty class)
    /// is terminalized to stop the heartbeat error-spam, and its task is
    /// advanced to `in_review` rather than left `active` — so the
    /// orphan-active sweep does NOT spuriously re-dispatch a fresh
    /// *implementation* worker on a task whose PR already exists.
    #[tokio::test]
    async fn draining_pr_review_advances_task_to_in_review_not_reimplementation() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        add_remote_host(&db, "anaplian");

        let wi = create_chore(&db, &product, "t2213-socket-write");
        let reviewer = running_pr_review_on_host(&db, &wi, "lease-review", "anaplian");
        // The task is in the PendingReview hold: active + pr_url.
        assert_eq!(task_status(&db, &wi), "active");

        db.set_host_enabled("anaplian", false).unwrap();

        let cube = FakeCube::default();
        let sink = RecordingDispatchEventSink::new();
        let outcome = run_one_pass(&db, &cube, &sink).await;

        assert_eq!(outcome.reaped, 1);
        assert_eq!(
            db.get_execution(&reviewer).unwrap().status,
            ExecutionStatus::Orphaned,
            "the stuck reviewer is terminalized (stops the heartbeat error-spam)"
        );
        assert_eq!(
            task_status(&db, &wi),
            "in_review",
            "the reviewer's task lands in Review, not stuck in Doing awaiting re-implementation"
        );
        // With the task no longer `active`, it is NOT an orphan-active
        // redispatch candidate — no spurious implementation worker.
        assert!(
            !db.list_orphan_active_candidates(0).unwrap().contains(&wi),
            "a pending-review task must not be re-dispatched as an implementation"
        );
    }
}
