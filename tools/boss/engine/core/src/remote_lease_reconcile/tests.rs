use super::*;
use crate::dispatch_events::RecordingDispatchEventSink;
use crate::host_registry::Host;
use crate::test_support::*;
use crate::work::{CreateChoreInput, WorkDb};
use anyhow::{Result, bail};
use async_trait::async_trait;
use boss_protocol::{ExecutionStatus, FinishExecutionRunInput, RequestExecutionInput};
use std::sync::Arc;
use std::sync::Mutex;

/// Canned liveness verdict for the stub adapter's probe.
#[derive(Clone, Copy)]
enum Probe {
    Alive,
    Dead,
    /// The probe round-trip itself failed (host down).
    Error,
}

/// Records `force_release_lease` / probe calls and returns a canned
/// liveness verdict. Every other method is unused by the reconcile path.
#[derive(bon::Builder)]
#[builder(on(String, into))]
struct StubAdapter {
    host_id: String,
    probe: Probe,
    force_released: Mutex<Vec<(String, Option<String>)>>,
    /// How many times `probe_remote_worker_alive` was called.
    probe_calls: Mutex<usize>,
    /// Canned `worker.log` tail. `Ok(None)` models an adapter with no
    /// such log; `Err` models a failed round-trip.
    worker_log: Mutex<Option<String>>,
    worker_log_fails: bool,
    cancel_on_log: Mutex<Option<(Arc<WorkDb>, String)>>,
    /// Workspace paths `read_worker_log_tail` was asked about, so a
    /// test can prove the read happened BEFORE the lease was released.
    worker_log_reads: Mutex<Vec<String>>,
}

crate::stub_host_adapter! { StubAdapter {
    fn host_id(&self) -> &str {
        &self.host_id
    }
    async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()> {
        self.force_released
            .lock()
            .unwrap()
            .push((lease_id.to_owned(), reason.map(str::to_owned)));
        Ok(())
    }
    async fn probe_remote_worker_alive(&self, _remote_pid: i64) -> Result<Option<bool>> {
        *self.probe_calls.lock().unwrap() += 1;
        match self.probe {
            Probe::Alive => Ok(Some(true)),
            Probe::Dead => Ok(Some(false)),
            Probe::Error => bail!("ssh probe transport failure"),
        }
    }
    async fn read_worker_log_tail(&self, workspace_path: &str, _max_bytes: u64) -> Result<Option<String>> {
        if let Some((db, id)) = self.cancel_on_log.lock().unwrap().take() {
            db.cancel_running_execution(&id).unwrap();
        }
        self.worker_log_reads.lock().unwrap().push(workspace_path.to_owned());
        if self.worker_log_fails {
            bail!("ssh transport failure reading worker.log");
        }
        Ok(self.worker_log.lock().unwrap().clone())
    }
} }

struct StubProvider {
    adapter: Arc<StubAdapter>,
}

#[async_trait]
impl HostAdapterProvider for StubProvider {
    async fn adapter_for(&self, _host: &Host) -> Result<Arc<dyn HostAdapter>> {
        Ok(self.adapter.clone() as Arc<dyn HostAdapter>)
    }
}

fn create_chore(db: &WorkDb) -> String {
    let product = create_test_product_with_repo(db, "p", Some("https://github.com/test/repo")).id;
    db.create_chore(CreateChoreInput::builder().product_id(product).name("c").build())
        .unwrap()
        .id
}

/// Start a remote run for `work_item_id` on `host_id`, stamp its
/// `remote_pid`, and return the execution id.
///
/// Deliberately reproduces the FULL production sequence, including the
/// `finish_execution_run` the dispatch path performs within
/// milliseconds of a successful spawn (`run_status = "completed"` while
/// the execution parks live on `running` — see
/// `coordinator::record_run_completion`). The original fixture stopped
/// at `start_execution_run_on_host`, leaving the run row `active` in a
/// way it never is in production; that is precisely why every test here
/// passed while the sweep selected nothing at all on a real engine.
fn start_remote_run(db: &WorkDb, work_item_id: &str, host_id: &str, lease_id: &str, remote_pid: Option<i64>) -> String {
    let execution = db
        .request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
        .unwrap();
    let (_execution, run) = db
        .start_execution_run_on_host(
            &execution.id,
            "worker-1",
            "repo-1",
            lease_id,
            "mono-agent-004",
            "/remote/mono-agent-004",
            host_id,
        )
        .unwrap();
    if let Some(pid) = remote_pid {
        db.set_run_remote_pid_for_execution(&execution.id, pid).unwrap();
    }
    // The dispatch action completes; the worker keeps running.
    db.finish_execution_run(
        FinishExecutionRunInput::builder()
            .execution_id(&execution.id)
            .run_id(&run.id)
            .execution_status(ExecutionStatus::Running)
            .run_status("completed")
            .clear_workspace_lease(false)
            .build(),
    )
    .unwrap();
    execution.id
}

fn provider(host_id: &str, probe: Probe) -> (Arc<StubAdapter>, StubProvider) {
    provider_with_log(host_id, probe, Some(String::new()), false)
}

fn provider_with_log(
    host_id: &str,
    probe: Probe,
    worker_log: Option<String>,
    worker_log_fails: bool,
) -> (Arc<StubAdapter>, StubProvider) {
    let adapter = Arc::new(
        StubAdapter::builder()
            .host_id(host_id)
            .probe(probe)
            .force_released(Mutex::new(Vec::new()))
            .probe_calls(Mutex::new(0))
            .worker_log(Mutex::new(worker_log))
            .worker_log_fails(worker_log_fails)
            .cancel_on_log(Mutex::new(None))
            .worker_log_reads(Mutex::new(Vec::new()))
            .build(),
    );
    let provider = StubProvider {
        adapter: adapter.clone(),
    };
    (adapter, provider)
}

#[tokio::test]
async fn reaps_dead_remote_worker_and_force_releases_its_lease() {
    let (_d, db) = open_db_arc();
    let chore = create_chore(&db);
    db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();
    let exec_id = start_remote_run(&db, &chore, "anaplian", "lease-XYZ", Some(4242));
    let historical_name = db.persona_display_name(&exec_id).unwrap().unwrap();

    let (adapter, provider) = provider("anaplian", Probe::Dead);
    let sink = RecordingDispatchEventSink::new();
    let outcome = reconcile_remote_leases(&db, &provider, &sink, None).await;

    assert_eq!(outcome.reaped, 1);
    assert_eq!(outcome.alive, 0);
    assert_eq!(outcome.skipped, 0);

    // Execution is now terminal (orphaned) → no longer blocks the guard.
    let after = db.get_execution(&exec_id).unwrap();
    assert_eq!(after.status, ExecutionStatus::Orphaned);
    let next_chore = create_chore(&db);
    let next = start_remote_run(&db, &next_chore, "anaplian", "lease-next", Some(4243));
    assert_eq!(db.persona_display_name(&next).unwrap(), Some(historical_name.clone()));
    assert_eq!(db.persona_display_name(&exec_id).unwrap(), Some(historical_name));

    // The leaked lease was force-released on the REMOTE adapter.
    // Clone out of the guard so no MutexGuard is held across the await below.
    let released = adapter.force_released.lock().unwrap().clone();
    assert_eq!(released.len(), 1, "the dead worker's lease must be force-released");
    assert_eq!(released[0].0, "lease-XYZ");

    // A reconcile event was emitted carrying the diagnostic detail.
    let events = sink.events_for(&exec_id).await;
    let ev = events
        .iter()
        .find(|e| e.stage == "remote_lease_reconcile")
        .expect("remote_lease_reconcile event missing");
    assert_eq!(ev.details.get("remote_pid").and_then(|v| v.as_i64()), Some(4242));
    assert_eq!(ev.details.get("host_id").and_then(|v| v.as_str()), Some("anaplian"));
}

/// The 2026-08-12 anaplian incident's *diagnosability* half.
///
/// Both remote workers died seconds after launch with `Failed to
/// authenticate. API Error: 401 OAuth access token has expired.` in
/// `<workspace>/.boss/worker.log`. Every engine-side surface was blank
/// and no attention item was raised, so the card sat in Doing reading
/// `active` with nothing anywhere naming the cause. The reap must carry
/// that line out with it — into the orphan reason, the dispatch event,
/// and an attention item — because the workspace (and the log) goes back
/// to cube moments later.
#[tokio::test]
async fn reap_carries_the_dead_workers_log_into_the_reason_event_and_attention() {
    let (_d, db) = open_db_arc();
    let chore = create_chore(&db);
    db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();
    let exec_id = start_remote_run(&db, &chore, "anaplian", "lease-AUTH", Some(4242));

    const OAUTH_ERROR: &str = "Failed to authenticate. API Error: 401 OAuth access token has expired.";
    let (adapter, provider) = provider_with_log("anaplian", Probe::Dead, Some(OAUTH_ERROR.to_owned()), false);
    let sink = RecordingDispatchEventSink::new();
    let outcome = reconcile_remote_leases(&db, &provider, &sink, None).await;
    assert_eq!(outcome.reaped, 1);

    // The log was read against the run's recorded workspace path...
    assert_eq!(
        adapter.worker_log_reads.lock().unwrap().clone(),
        vec!["/remote/mono-agent-004".to_owned()],
    );
    // ...and its content reached the durable orphan reason. The run row
    // was already closed by the dispatch path, so `mark_execution_orphaned`
    // records the reason on `work_runs.error_text` (its `result_summary` /
    // `finished_at` guards are already spent by then).
    assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Orphaned);
    let reason = db
        .list_runs(&exec_id)
        .unwrap()
        .into_iter()
        .filter_map(|run| run.error_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        reason.contains(OAUTH_ERROR),
        "the orphan reason must name why the worker died, got: {reason}",
    );

    // ...the dispatch event...
    let events = sink.events_for(&exec_id).await;
    let ev = events
        .iter()
        .find(|e| e.stage == "remote_lease_reconcile")
        .expect("remote_lease_reconcile event missing");
    assert_eq!(
        ev.details.get("worker_log_tail").and_then(|v| v.as_str()),
        Some(OAUTH_ERROR),
    );

    // ...and an attention item, so the failure is not silent. This is
    // the surface whose absence made the incident invisible.
    let attentions = db.list_attention_items(&exec_id).unwrap();
    let item = attentions
        .iter()
        .find(|a| a.kind == REMOTE_WORKER_DIED_ATTENTION_KIND)
        .expect("a dead remote worker must raise an attention item");
    assert!(
        item.title.contains("anaplian"),
        "title should name the host: {}",
        item.title
    );
    assert!(
        item.body_markdown.contains(OAUTH_ERROR),
        "the attention body must carry the worker's own output",
    );
}

/// An unreadable log must never masquerade as "the worker said
/// nothing" — the two lead a reader to different next steps. The reap
/// itself proceeds either way; losing the log is not a reason to leave
/// a dead worker's lease stranded.
#[tokio::test]
async fn unreadable_worker_log_still_reaps_and_says_it_was_unreadable() {
    let (_d, db) = open_db_arc();
    let chore = create_chore(&db);
    db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();
    let exec_id = start_remote_run(&db, &chore, "anaplian", "lease-NOLOG", Some(4242));

    let (adapter, provider) = provider_with_log("anaplian", Probe::Dead, None, true);
    let sink = RecordingDispatchEventSink::new();
    let outcome = reconcile_remote_leases(&db, &provider, &sink, None).await;

    assert_eq!(outcome.reaped, 1, "a failed log read must not block the reap");
    assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Orphaned);
    assert_eq!(
        adapter.force_released.lock().unwrap().len(),
        1,
        "the lease must still be released",
    );

    let attentions = db.list_attention_items(&exec_id).unwrap();
    let item = attentions
        .iter()
        .find(|a| a.kind == REMOTE_WORKER_DIED_ATTENTION_KIND)
        .expect("attention item still filed");
    assert!(
        item.body_markdown.contains("could not be read"),
        "an unreadable log must say so rather than read as empty output: {}",
        item.body_markdown,
    );
}

#[tokio::test]
async fn leaves_live_remote_worker_untouched() {
    let (_d, db) = open_db_arc();
    let chore = create_chore(&db);
    db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();
    let exec_id = start_remote_run(&db, &chore, "anaplian", "lease-LIVE", Some(4242));

    let (adapter, provider) = provider("anaplian", Probe::Alive);
    let sink = RecordingDispatchEventSink::new();
    let outcome = reconcile_remote_leases(&db, &provider, &sink, None).await;

    assert_eq!(outcome.alive, 1);
    assert_eq!(outcome.reaped, 0);
    // A live worker must never be reaped or have its lease released.
    assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Running);
    assert!(adapter.force_released.lock().unwrap().is_empty());
}

/// A remote chore that succeeds parks in `waiting_review` with its
/// lease retained and its worker already exited by design. The
/// recorded `remote_pid` stays set (only spawn writes it), so a
/// `kill -0` probe correctly reports dead — and without the
/// `is_live()` gate that would look like a zombie and orphan the
/// just-opened PR ~60s later. Sibling sweeps all refuse to reap this
/// park state; this one must too.
#[tokio::test]
async fn parked_waiting_review_remote_execution_is_not_reaped() {
    let (_d, db) = open_db_arc();
    let chore = create_chore(&db);
    db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();
    let exec_id = start_remote_run(&db, &chore, "anaplian", "lease-WR", Some(4242));
    db.force_execution_status_for_test(&chore, ExecutionStatus::WaitingReview)
        .unwrap();
    assert_eq!(
        db.get_execution(&exec_id).unwrap().status,
        ExecutionStatus::WaitingReview
    );
    assert!(
        db.get_execution(&exec_id).unwrap().cube_lease_id.is_some(),
        "waiting_review must retain its lease"
    );

    let (adapter, provider) = provider("anaplian", Probe::Dead);
    let sink = RecordingDispatchEventSink::new();
    let outcome = reconcile_remote_leases(&db, &provider, &sink, None).await;

    assert_eq!(
        outcome.reaped, 0,
        "a parked waiting_review execution must never be reaped"
    );
    assert_eq!(
        outcome.skipped, 0,
        "parked executions are filtered before the probe, not counted as probe skips"
    );
    assert_eq!(
        *adapter.probe_calls.lock().unwrap(),
        0,
        "must not SSH-probe a parked execution every reconcile pass"
    );
    assert_eq!(
        db.get_execution(&exec_id).unwrap().status,
        ExecutionStatus::WaitingReview,
        "status must be unchanged"
    );
    assert!(
        adapter.force_released.lock().unwrap().is_empty(),
        "the retained park lease must not be force-released"
    );
}

#[tokio::test]
async fn inconclusive_probe_never_reaps() {
    // A host outage (probe Err) must NOT look like proof of death —
    // otherwise every live worker on a briefly-unreachable host would
    // be mass-reaped.
    let (_d, db) = open_db_arc();
    let chore = create_chore(&db);
    db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();
    let exec_id = start_remote_run(&db, &chore, "anaplian", "lease-1", Some(4242));

    let (adapter, provider) = provider("anaplian", Probe::Error);
    let sink = RecordingDispatchEventSink::new();
    let outcome = reconcile_remote_leases(&db, &provider, &sink, None).await;

    assert_eq!(outcome.skipped, 1);
    assert_eq!(outcome.reaped, 0);
    assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Running);
    assert!(adapter.force_released.lock().unwrap().is_empty());
}

#[tokio::test]
async fn run_without_remote_pid_is_skipped() {
    // No pid → no positive death evidence → never reap.
    let (_d, db) = open_db_arc();
    let chore = create_chore(&db);
    db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();
    let exec_id = start_remote_run(&db, &chore, "anaplian", "lease-1", None);

    let (adapter, provider) = provider("anaplian", Probe::Dead);
    let sink = RecordingDispatchEventSink::new();
    let outcome = reconcile_remote_leases(&db, &provider, &sink, None).await;

    assert_eq!(outcome.skipped, 1);
    assert_eq!(outcome.reaped, 0);
    assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Running);
    assert!(adapter.force_released.lock().unwrap().is_empty());
}

#[tokio::test]
async fn local_runs_are_not_candidates() {
    // A local run is covered by the local sweeps and must never appear
    // here (host_id = 'local' is excluded by the candidate query).
    let (_d, db) = open_db_arc();
    let chore = create_chore(&db);
    let exec_id = start_remote_run(&db, &chore, "local", "lease-1", Some(4242));

    let (adapter, provider) = provider("anaplian", Probe::Dead);
    let sink = RecordingDispatchEventSink::new();
    let outcome = reconcile_remote_leases(&db, &provider, &sink, None).await;

    assert_eq!(outcome, RemoteLeaseReconcileOutcome::default());
    assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Running);
    assert!(adapter.force_released.lock().unwrap().is_empty());
}

#[tokio::test]
async fn terminal_remote_persona_backstop_preserves_history_and_live_leases() {
    let (_d, db) = open_db_arc();
    db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();
    let dead = start_remote_run(&db, &create_chore(&db), "anaplian", "dead", Some(1));
    let live = start_remote_run(&db, &create_chore(&db), "anaplian", "live", Some(2));
    let dead_name = db.persona_display_name(&dead).unwrap();
    let live_name = db.persona_display_name(&live).unwrap();
    db.cancel_running_execution(&dead).unwrap();
    let (_, provider) = provider("anaplian", Probe::Alive);
    reconcile_remote_leases(&db, &provider, &RecordingDispatchEventSink::new(), None).await;
    assert!(db.terminal_remote_persona_executions().unwrap().is_empty());
    let next = start_remote_run(&db, &create_chore(&db), "anaplian", "next", Some(3));
    assert_eq!(db.persona_display_name(&next).unwrap(), dead_name);
    assert_eq!(db.persona_display_name(&dead).unwrap(), dead_name);
    assert_eq!(db.persona_display_name(&live).unwrap(), live_name);
}

#[tokio::test]
async fn concurrent_terminalization_still_releases_persona() {
    let (_d, db) = open_db_arc();
    db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();
    let id = start_remote_run(&db, &create_chore(&db), "anaplian", "lease", Some(1));
    let name = db.persona_display_name(&id).unwrap();
    let (adapter, provider) = provider("anaplian", Probe::Dead);
    *adapter.cancel_on_log.lock().unwrap() = Some((db.clone(), id.clone()));
    let outcome = reconcile_remote_leases(&db, &provider, &RecordingDispatchEventSink::new(), None).await;
    assert_eq!(outcome.reaped, 1);
    assert_eq!(db.get_execution(&id).unwrap().status, ExecutionStatus::Cancelled);
    assert!(db.terminal_remote_persona_executions().unwrap().is_empty());
    let next = start_remote_run(&db, &create_chore(&db), "anaplian", "next", Some(2));
    assert_eq!(db.persona_display_name(&next).unwrap(), name);
    assert_eq!(db.persona_display_name(&id).unwrap(), name);
}
