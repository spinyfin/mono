//! Reap-eligibility, failure-class, and liveness-attention tests for
//! [`crate::spawn_ack_sweep`]. Nested under `spawn_ack_sweep::tests` so the
//! parent file stays under the line-count cap.

use std::sync::Arc;

use super::*;
use crate::dispatch_events::RecordingDispatchEventSink;
use crate::live_worker_state::DriverSignalKind;
use crate::spawn_health::SpawnHealthTracker;
use crate::transcript_liveness::{TranscriptLiveness, TranscriptSource};
use crate::work::{ExecutionStatus, WorkDb};
use boss_protocol::WorkerActivity;

fn present_liveness() -> TranscriptLiveness {
    TranscriptLiveness::Present {
        source: TranscriptSource::Rollout,
        path: std::path::PathBuf::from("/tmp/rollout.jsonl"),
        age_secs: 3,
        bytes: 100,
        discovery_would_attach: true,
        detail: "exists (100 bytes, last written 3s ago)".to_owned(),
    }
}

fn absent_liveness() -> TranscriptLiveness {
    TranscriptLiveness::Absent {
        checked: vec!["probe stub".to_owned()],
    }
}

// ─── the liveness veto (2026-09-13) ──────────────────────────────────────

/// A `session_meta` first line in the shape Codex writes, with `cwd`
/// pointing wherever the test wants correlation to land.
fn session_meta_line(session_id: &str, cwd: &std::path::Path) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "timestamp": "2026-09-13T21:41:42.000Z",
            "type": "session_meta",
            "payload": {
                "id": session_id,
                "timestamp": "2026-09-13T21:41:42.000Z",
                "cwd": cwd.display().to_string(),
                "originator": "codex_cli_rs",
                "cli_version": "0.0.0-test",
            }
        })
    )
}

/// The durable shape a Codex spawn leaves behind: a run row and an
/// `Armed` ingress checkpoint pointing at `root` with the given
/// baseline. Returns the workspace the ingress correlates against.
fn arm_file_ingress(
    db: &WorkDb,
    execution_id: &str,
    root: &std::path::Path,
    workspace: &std::path::Path,
    baseline: Vec<std::path::PathBuf>,
) {
    use crate::agent_jsonl_progress::{IngressCheckpoint, IngressCheckpointStore};
    let checkpoint = IngressCheckpoint::Armed {
        ingress: crate::driver::AgentJsonlFileIngress {
            directory: root.to_path_buf(),
            filename_prefix: "rollout-".to_owned(),
            filename_suffix: ".jsonl".to_owned(),
            workspace_path: workspace.to_path_buf(),
        },
        baseline,
    };
    db.store_ingress_checkpoint(execution_id, &checkpoint)
        .expect("the run row exists, so the checkpoint can be stored");
}

/// The incident, reproduced: a pane with a live shell, no driver
/// signal for longer than the window, and a rollout on disk that the
/// progress ingress never attached. The reap must be refused, the
/// transcript recorded as the run's driver-start proof, and nothing
/// torn down. Run with a rollout that correlates and with one discovery
/// would reject (the 43-second-margin case): the driver wrote both, so
/// both are proof of life.
#[tokio::test]
async fn a_transcript_on_disk_vetoes_the_driver_start_reap() {
    for correlates in [true, false] {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_spawned_execution(&db, &work_item_id, 92697);
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        arm_file_ingress(&db, &execution_id, &root, &workspace, Vec::new());
        let cwd = if correlates {
            workspace.clone()
        } else {
            temp.path().to_path_buf()
        };
        let rollout = root.join("rollout-2026-09-13T21-41-42-sess-1.jsonl");
        std::fs::write(&rollout, session_meta_line("sess-1", &cwd)).unwrap();

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_live_shell(&live_states, 1, &execution_id, &work_item_id, 92697, false);
        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let cube = RecordingCube::default();
        let (outcome, sink) = run_pass(&db, &live_states, &coordinator, &cube).await;

        assert_eq!(
            outcome.driver_start_reaped, 0,
            "correlates={correlates}: a worker with a transcript on disk must not be reaped",
        );
        assert_eq!(outcome.vetoed, 1, "correlates={correlates}: the veto must be counted");
        assert_ne!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Orphaned,
            "correlates={correlates}: the execution must not be orphaned",
        );
        assert!(
            coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(&execution_id),
            "correlates={correlates}: the slot must stay claimed",
        );
        assert!(
            cube.released_lease_ids().is_empty(),
            "correlates={correlates}: the cube lease must not be released",
        );
        assert!(
            sink.events().await.is_empty(),
            "correlates={correlates}: no reap event may be emitted",
        );
        assert!(
            db.list_attention_items(&execution_id).unwrap().is_empty(),
            "correlates={correlates}: no attention item may be raised",
        );
        assert!(
            live_states.driver_signal_at(1).is_some(),
            "correlates={correlates}: the transcript must be recorded as driver-start proof",
        );

        // The proof is permanent: a second pass finds nothing to examine.
        let (again, _) = run_pass(&db, &live_states, &coordinator, &cube).await;
        assert_eq!(
            again.driver_start_reaped + again.vetoed + again.liveness_undeterminable,
            0
        );
    }
}

/// Liveness that cannot be established is not absence. A checkpoint
/// whose root cannot be verified must leave the slot alone and say so,
/// rather than reaping on an unreadable answer.
#[tokio::test]
async fn undeterminable_liveness_does_not_reap() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_spawned_execution(&db, &work_item_id, 4242);
    let temp = tempfile::TempDir::new().unwrap();
    let missing_root = temp.path().join("never-created");
    arm_file_ingress(&db, &execution_id, &missing_root, temp.path(), Vec::new());

    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot_with_live_shell(&live_states, 1, &execution_id, &work_item_id, 4242, false);
    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let cube = RecordingCube::default();
    let (outcome, sink) = run_pass(&db, &live_states, &coordinator, &cube).await;

    assert_eq!(outcome.driver_start_reaped, 0);
    assert_eq!(outcome.liveness_undeterminable, 1);
    assert_ne!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Orphaned
    );
    assert!(sink.events().await.is_empty());
    assert!(
        live_states.driver_signal_at(1).is_none(),
        "an undeterminable answer is not proof either way",
    );

    // A liveness answer that stays undeterminable pass after pass must
    // not be held silently forever: once the consecutive count crosses
    // the threshold, a distinct attention item is raised exactly once.
    for pass in 2..UNDETERMINABLE_LIVENESS_ATTENTION_THRESHOLD {
        let (outcome, _) = run_pass(&db, &live_states, &coordinator, &cube).await;
        assert_eq!(outcome.liveness_undeterminable, 1, "pass {pass}");
        assert!(
            db.list_attention_items(&execution_id).unwrap().is_empty(),
            "pass {pass}: no attention item before the threshold is crossed",
        );
    }
    let (outcome, _) = run_pass(&db, &live_states, &coordinator, &cube).await;
    assert_eq!(outcome.liveness_undeterminable, 1, "the threshold-crossing pass");
    let attentions = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        attentions.len(),
        1,
        "exactly one attention item once the threshold is crossed"
    );
    assert_eq!(attentions[0].kind, LIVENESS_UNDETERMINABLE_ATTENTION_KIND);

    // Further passes must not raise a second one.
    let (outcome, _) = run_pass(&db, &live_states, &coordinator, &cube).await;
    assert_eq!(outcome.liveness_undeterminable, 1);
    assert_eq!(
        db.list_attention_items(&execution_id).unwrap().len(),
        1,
        "the attention item must not be raised again on subsequent passes",
    );
}

/// A confirmed absence still reaps — the breaker must keep firing on
/// genuinely dead spawns — and the record says what was checked and
/// which failure class fired, not that the driver "never started".
#[tokio::test]
async fn absent_transcript_reaps_and_the_record_states_what_was_checked() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_spawned_execution(&db, &work_item_id, 4242);
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    // A rollout from before the spawn is baselined away and must not
    // count as this run's transcript.
    let stale = root.join("rollout-old-sess-0.jsonl");
    std::fs::write(&stale, session_meta_line("sess-0", temp.path())).unwrap();
    let stale = std::fs::canonicalize(&stale).unwrap();
    arm_file_ingress(&db, &execution_id, &root, temp.path(), vec![stale]);

    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot_with_live_shell(&live_states, 1, &execution_id, &work_item_id, 4242, false);
    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let spawn_health = SpawnHealthTracker::new();
    let cube = RecordingCube::default();
    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &spawn_health,
        &cube,
        SPAWN_ACK_GRACE_SECS,
        DRIVER_START_GRACE_SECS,
    )
    .await;

    assert_eq!(outcome.driver_start_reaped, 1, "a confirmed absence must still reap");
    assert_eq!(cube.released_lease_ids(), vec!["lease-1".to_owned()]);
    assert_eq!(outcome.vetoed, 0);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Orphaned
    );

    let attentions = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(attentions.len(), 1);
    let body = &attentions[0].body_markdown;
    assert!(
        body.contains("no rollout file newer than the pre-spawn baseline"),
        "the attention body must state the liveness probe's finding; got: {body}",
    );
    assert!(
        body.contains("1 pre-existing file(s) excluded"),
        "the probe must report the baselined file it ignored; got: {body}",
    );
    assert!(
        !body.contains("The driver binary never started") && !body.contains("driver binary never ran"),
        "the body must not assert the inference that the driver never started; got: {body}",
    );
    assert!(
        body.contains("Either the driver never started, or it started and its signal never reached"),
        "the body must name both explanations for the missing signal; got: {body}",
    );
    assert!(
        attentions[0].title.contains("no driver signal was observed"),
        "got: {}",
        attentions[0].title,
    );

    let events = sink.events().await;
    let reap = events
        .iter()
        .find(|e| e.stage == "driver_start_timeout")
        .expect("the reap event");
    assert_eq!(
        reap.details["failure_class"],
        serde_json::json!("shell_without_driver_signal")
    );
    assert!(
        reap.details["liveness_probe"]
            .as_str()
            .unwrap()
            .contains("no transcript exists"),
        "got: {}",
        reap.details["liveness_probe"],
    );

    let evidence = spawn_health.evidence_in_window(boss_engine_utils::epoch_time::now_epoch_secs());
    assert_eq!(evidence.len(), 1);
    assert_eq!(
        evidence[0].class,
        crate::spawn_health::SpawnFailureClass::ShellWithoutDriverSignal,
        "pass 2 must feed the breaker as its own failure class",
    );
    assert_eq!(evidence[0].cause, "driver_start_timeout");
    assert!(
        evidence[0].observed.contains("liveness probe:"),
        "got: {}",
        evidence[0].observed
    );
}

/// The liveness veto does NOT protect the app-reported causes: an
/// `AppNack` is the app itself positively reporting the pane failed to
/// spawn, which a transcript's mere existence does not contradict.
/// Before this fix the veto applied uniformly to every cause, leaving a
/// `pid<=0`/`Spawning` slot behind that no other sweep could reclaim
/// (see the module doc and `ReapCause::vetoable`'s doc) — the reap must
/// proceed here, and the transcript must NOT be recorded as a permanent
/// driver signal.
#[tokio::test]
async fn app_nack_is_not_vetoed_by_a_transcript_on_disk() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_spawned_execution(&db, &work_item_id, 0);
    let execution = db.get_execution(&execution_id).unwrap();
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    arm_file_ingress(&db, &execution_id, &root, &workspace, Vec::new());
    std::fs::write(
        root.join("rollout-2026-09-13T21-41-42-sess-1.jsonl"),
        session_meta_line("sess-1", &workspace),
    )
    .unwrap();

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let spawn_health = SpawnHealthTracker::new();
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let live_states = LiveWorkerStateRegistry::new();
    register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);
    let cube = RecordingCube::default();
    let ctx = SpawnReapCtx::builder()
        .work_db(db.as_ref())
        .live_states(&live_states)
        .coordinator(coordinator.clone())
        .dispatch_events(sink.as_ref())
        .reaper(reaper.as_ref())
        .spawn_health(&spawn_health)
        .cube_client(&cube)
        .build();
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let outcome = reap_never_started_spawn(&ctx, &execution, 1, 0, ReapCause::AppNack { reason: "late" }, now).await;

    assert_eq!(
        outcome,
        ReapOutcome::Reaped,
        "an app-reported NACK must reap despite a transcript on disk"
    );
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Orphaned,
    );
    assert_eq!(reaper.reaped().len(), 1, "the pane must still be torn down");
    assert_eq!(sink.events().await.len(), 1);
    assert!(
        live_states.driver_signal_at(1).is_none(),
        "an app-reported cause must never record a permanent driver signal from the veto probe",
    );
    assert!(
        !coordinator
            .worker_pool()
            .claimed_execution_ids()
            .await
            .contains(&execution_id),
        "the slot must be released — reachable by redispatch, not stuck the way a vetoed slot is",
    );
}

/// Same as above for the other app-reported cause: a pane the app
/// reports as dead-before-start must be reaped even with a transcript
/// on disk.
#[tokio::test]
async fn pane_died_before_start_is_not_vetoed_by_a_transcript_on_disk() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_spawned_execution(&db, &work_item_id, 0);
    let execution = db.get_execution(&execution_id).unwrap();
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    arm_file_ingress(&db, &execution_id, &root, &workspace, Vec::new());
    std::fs::write(
        root.join("rollout-2026-09-13T21-41-42-sess-1.jsonl"),
        session_meta_line("sess-1", &workspace),
    )
    .unwrap();

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let spawn_health = SpawnHealthTracker::new();
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let live_states = LiveWorkerStateRegistry::new();
    register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);
    let cube = RecordingCube::default();
    let ctx = SpawnReapCtx::builder()
        .work_db(db.as_ref())
        .live_states(&live_states)
        .coordinator(coordinator.clone())
        .dispatch_events(sink.as_ref())
        .reaper(reaper.as_ref())
        .spawn_health(&spawn_health)
        .cube_client(&cube)
        .build();
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let outcome = reap_never_started_spawn(
        &ctx,
        &execution,
        1,
        0,
        ReapCause::PaneDiedBeforeStart {
            detail: "surface failed to attach",
        },
        now,
    )
    .await;

    assert_eq!(outcome, ReapOutcome::Reaped);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Orphaned,
    );
    assert!(live_states.driver_signal_at(1).is_none());
}

/// The recorded-transcript-path source must not vouch for a run that
/// merely reused a run row an earlier incarnation already stamped a
/// transcript path onto: a file last written before this execution's
/// `started_at` must not veto a vetoable cause.
#[tokio::test]
async fn a_transcript_path_recorded_before_this_spawn_does_not_veto() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_spawned_execution(&db, &work_item_id, 0);
    let temp = tempfile::TempDir::new().unwrap();
    let transcript = temp.path().join("earlier-incarnation.jsonl");
    std::fs::write(&transcript, "{}\n").unwrap();
    db.set_run_transcript_path_if_unset(&execution_id, transcript.to_str().unwrap())
        .unwrap();

    // Force `started_at` to AFTER the transcript file's mtime, simulating
    // a later incarnation of the same execution row reusing a run whose
    // `transcript_path` a prior, unrelated spawn already recorded.
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    db.force_started_at_for_test(&execution_id, now + 1000).unwrap();
    let execution = db.get_execution(&execution_id).unwrap();

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let spawn_health = SpawnHealthTracker::new();
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let live_states = LiveWorkerStateRegistry::new();
    register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);
    let cube = RecordingCube::default();
    let ctx = SpawnReapCtx::builder()
        .work_db(db.as_ref())
        .live_states(&live_states)
        .coordinator(coordinator.clone())
        .dispatch_events(sink.as_ref())
        .reaper(reaper.as_ref())
        .spawn_health(&spawn_health)
        .cube_client(&cube)
        .build();
    let outcome = reap_never_started_spawn(
        &ctx,
        &execution,
        1,
        0,
        ReapCause::SpawnAckTimeout { grace_secs: 60 },
        now + 2000,
    )
    .await;

    assert_eq!(
        outcome,
        ReapOutcome::Reaped,
        "a transcript path predating this run's spawn must not veto the reap"
    );
    assert!(
        live_states.driver_signal_at(1).is_none(),
        "a stale recorded transcript path must not be recorded as this run's driver signal",
    );
}

/// No false positives: a worker whose driver DID start
/// is never touched, however long it then runs without further events —
/// the driver-start signal is first-write-wins and permanent.
#[tokio::test]
async fn a_driver_that_signalled_is_never_reaped() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_spawned_execution(&db, &work_item_id, 777);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot_with_live_shell(&live_states, 1, &execution_id, &work_item_id, 777, false);

    // The driver reported in exactly once, long ago.
    assert_eq!(
        live_states.record_driver_signal(&execution_id, crate::live_worker_state::DriverSignalKind::HookEvent),
        Some(1),
    );

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let cube = RecordingCube::default();
    let (outcome, sink) = run_pass(&db, &live_states, &coordinator, &cube).await;

    assert_eq!(outcome.driver_start_reaped, 0, "a started driver must never be reaped");
    assert_eq!(outcome.reaped, 0);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Running,
        "the execution must be left exactly as it was",
    );
    assert!(
        coordinator
            .worker_pool()
            .claimed_execution_ids()
            .await
            .contains(&execution_id),
        "the slot must NOT be released out from under a working worker",
    );
    assert!(
        cube.released_lease_ids().is_empty(),
        "the cube lease must NOT be released out from under a working worker",
    );
    assert!(db.list_attention_items(&execution_id).unwrap().is_empty());
    assert!(sink.events().await.iter().all(|e| e.stage != "driver_start_timeout"));
}

/// Re-adoption then sweep: a durable driver signal from before the
/// engine restart must leave the worker entirely alone.
///
/// `readopt_live_worker` restores the durable semantic-progress
/// checkpoint after reconstructing the slot. That checkpoint came from
/// a driver-originated event before restart, so it proves this run is
/// not a never-started driver even though this registration itself was
/// triggered by a shell-pid probe.
#[tokio::test]
async fn a_readopted_worker_with_durable_driver_proof_is_not_reaped() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_spawned_execution(&db, &work_item_id, 92697);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    // Exactly what `readopt_live_worker` does when its durable-pid probe
    // cannot produce a positive pid.
    live_states.register_readoption(
        1,
        execution_id.as_str(),
        "grok-4.6",
        0,
        Some(WorkItemBinding {
            work_item_id: work_item_id.clone(),
            work_item_name: "test chore".to_owned(),
            execution_id: execution_id.clone(),
        }),
        false,
        crate::live_worker_state::LiveSpawnRouting::none(),
        crate::live_worker_state::ReadoptionEvidence::LiveShellPid,
    );
    // Age the re-registration past every window under test.
    live_states.set_spawn_time_for_test(
        1,
        boss_engine_utils::epoch_time::now_epoch_secs() - (DRIVER_START_GRACE_SECS + 60),
    );
    live_states.seed_semantic_progress(
        1,
        &SemanticProgressCheckpoint {
            progress_at: "2026-09-02T12:00:00Z".to_owned(),
            tool_condition: SemanticToolCondition::Unknown,
        },
    );
    assert!(
        live_states.driver_signal_at(1).is_some(),
        "a durable checkpoint restores proof that the driver signalled before restart",
    );

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let cube = RecordingCube::default();
    let (outcome, sink) = run_pass(&db, &live_states, &coordinator, &cube).await;

    assert_eq!(
        outcome.driver_start_reaped, 0,
        "a re-adopted worker with durable driver proof must not be reaped",
    );
    assert_eq!(outcome.reaped, 0);
    assert_eq!(
        outcome.skipped.readopted, 1,
        "the readopted skip must be counted, not silently dropped from the accounting",
    );
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Running,
        "the re-adopted execution must be left exactly as re-adoption restored it",
    );
    assert!(
        coordinator
            .worker_pool()
            .claimed_execution_ids()
            .await
            .contains(&execution_id),
        "the slot must NOT be released out from under a re-adopted worker",
    );
    assert!(
        cube.released_lease_ids().is_empty(),
        "the cube lease must NOT be force-released out from under a re-adopted worker",
    );
    assert!(db.list_attention_items(&execution_id).unwrap().is_empty());
    assert!(sink.events().await.iter().all(|e| e.stage != "driver_start_timeout"));
}

/// A driver still inside its grace window is left alone, so a merely-slow
/// start is never reaped.
#[tokio::test]
async fn driver_start_verification_respects_its_grace_window() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_spawned_execution(&db, &work_item_id, 888);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot_with_live_shell(&live_states, 1, &execution_id, &work_item_id, 888, false);
    // Spawned well inside the window: no driver signal yet, but too early
    // to conclude anything.
    live_states.set_spawn_time_for_test(1, boss_engine_utils::epoch_time::now_epoch_secs() - 5);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let cube = RecordingCube::default();
    let (outcome, _sink) = run_pass(&db, &live_states, &coordinator, &cube).await;

    assert_eq!(outcome.driver_start_reaped, 0, "a fresh spawn must be given its window");
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Running,
    );
}

/// Pass 1's proof-of-life test is now the driver signal, not
/// `last_event_at`. A zero-pid slot carrying only a synthesized
/// `last_event_at` must still be reaped rather than skipped.
#[tokio::test]
async fn pass_one_no_longer_treats_a_synthesized_timestamp_as_proof_of_life() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_old_execution(&db, &work_item_id);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);
    // An engine-written timestamp with no driver behind it.
    live_states.set_last_event_at_for_test(1, "2026-07-30T05:47:45Z");
    assert!(live_states.driver_signal_at(1).is_none());

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let cube = RecordingCube::default();
    let (outcome, _sink) = run_pass(&db, &live_states, &coordinator, &cube).await;

    assert_eq!(
        outcome.reaped, 1,
        "only a driver-originated signal may suppress the spawn-ack reap",
    );
    assert_eq!(outcome.skipped.has_driver_signal, 0);
}

/// A hook that lands while the liveness probe is parked must veto the
/// reap: no orphan, no teardown, no breaker failure.
#[tokio::test]
async fn a_hook_during_the_probe_await_vetoes_the_reap() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_old_execution(&db, &work_item_id);
    let execution = db.get_execution(&execution_id).unwrap();
    let live_states = LiveWorkerStateRegistry::new();
    register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let spawn_health = SpawnHealthTracker::new();
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let cube = RecordingCube::default();
    let ctx = SpawnReapCtx::builder()
        .work_db(db.as_ref())
        .live_states(&live_states)
        .coordinator(coordinator.clone())
        .dispatch_events(sink.as_ref())
        .reaper(reaper.as_ref())
        .spawn_health(&spawn_health)
        .cube_client(&cube)
        .build();

    let hold = super::super::probe_hold::ProbeHold::new();
    super::super::probe_hold::arm(&execution_id, Arc::clone(&hold));
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let signal_id = execution_id.clone();
    let outcome = tokio::select! {
        outcome = reap_never_started_spawn(
            &ctx,
            &execution,
            1,
            0,
            ReapCause::SpawnAckTimeout { grace_secs: 60 },
            now,
        ) => outcome,
        _ = async {
            tokio::task::spawn_blocking({
                let hold = Arc::clone(&hold);
                move || hold.wait_for_entry()
            })
            .await
            .unwrap();
            assert_eq!(
                live_states.record_driver_signal(&signal_id, DriverSignalKind::HookEvent),
                Some(1),
                "the hook must be accepted before the reap commits",
            );
            hold.release();
            std::future::pending::<()>().await
        } => unreachable!(),
    };
    super::super::probe_hold::disarm(&execution_id);

    assert_eq!(outcome, ReapOutcome::Vetoed);
    assert_ne!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Orphaned
    );
    assert!(reaper.reaped().is_empty(), "the pane must not be torn down");
    assert!(cube.released_lease_ids().is_empty());
    assert!(
        spawn_health
            .evidence_in_window(boss_engine_utils::epoch_time::now_epoch_secs())
            .is_empty(),
        "a vetoed reap must not feed the breaker",
    );
    assert!(
        coordinator
            .worker_pool()
            .claimed_execution_ids()
            .await
            .contains(&execution_id),
        "the slot must stay claimed",
    );
}

/// Pass 1 must skip a pid that arrived during the probe, not orphan the
/// worker that just reported a shell.
#[tokio::test]
async fn a_pid_during_the_probe_await_skips_a_spawn_ack_reap() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_old_execution(&db, &work_item_id);
    let execution = db.get_execution(&execution_id).unwrap();
    let live_states = LiveWorkerStateRegistry::new();
    register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let spawn_health = SpawnHealthTracker::new();
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let cube = RecordingCube::default();
    let ctx = SpawnReapCtx::builder()
        .work_db(db.as_ref())
        .live_states(&live_states)
        .coordinator(coordinator.clone())
        .dispatch_events(sink.as_ref())
        .reaper(reaper.as_ref())
        .spawn_health(&spawn_health)
        .cube_client(&cube)
        .build();

    let hold = super::super::probe_hold::ProbeHold::new();
    super::super::probe_hold::arm(&execution_id, Arc::clone(&hold));
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let signal_id = execution_id.clone();
    let outcome = tokio::select! {
        outcome = reap_never_started_spawn(
            &ctx,
            &execution,
            1,
            0,
            ReapCause::SpawnAckTimeout { grace_secs: 60 },
            now,
        ) => outcome,
        _ = async {
            tokio::task::spawn_blocking({
                let hold = Arc::clone(&hold);
                move || hold.wait_for_entry()
            })
            .await
            .unwrap();
            live_states.update_shell_pid(&signal_id, 4242);
            hold.release();
            std::future::pending::<()>().await
        } => unreachable!(),
    };
    super::super::probe_hold::disarm(&execution_id);

    assert_eq!(outcome, ReapOutcome::Skipped);
    assert_ne!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Orphaned
    );
    assert!(reaper.reaped().is_empty());
    assert!(
        spawn_health
            .evidence_in_window(boss_engine_utils::epoch_time::now_epoch_secs())
            .is_empty()
    );
}

/// App-reported never-started eligibility is the four-term predicate
/// (not Readopted, pid<=0, no last_event_at, still Spawning). A pid that
/// arrives during the probe must skip the reap.
#[tokio::test]
async fn a_pid_during_the_probe_await_skips_an_app_nack_reap() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_old_execution(&db, &work_item_id);
    let execution = db.get_execution(&execution_id).unwrap();
    let live_states = LiveWorkerStateRegistry::new();
    register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let spawn_health = SpawnHealthTracker::new();
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let cube = RecordingCube::default();
    let ctx = SpawnReapCtx::builder()
        .work_db(db.as_ref())
        .live_states(&live_states)
        .coordinator(coordinator.clone())
        .dispatch_events(sink.as_ref())
        .reaper(reaper.as_ref())
        .spawn_health(&spawn_health)
        .cube_client(&cube)
        .build();

    let hold = super::super::probe_hold::ProbeHold::new();
    super::super::probe_hold::arm(&execution_id, Arc::clone(&hold));
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let signal_id = execution_id.clone();
    let outcome = tokio::select! {
        outcome = reap_never_started_spawn(
            &ctx,
            &execution,
            1,
            0,
            ReapCause::AppNack { reason: "late" },
            now,
        ) => outcome,
        _ = async {
            tokio::task::spawn_blocking({
                let hold = Arc::clone(&hold);
                move || hold.wait_for_entry()
            })
            .await
            .unwrap();
            live_states.update_shell_pid(&signal_id, 4242);
            hold.release();
            std::future::pending::<()>().await
        } => unreachable!(),
    };
    super::super::probe_hold::disarm(&execution_id);

    assert_eq!(outcome, ReapOutcome::Skipped);
    assert_ne!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Orphaned
    );
}

/// Re-registering the slot to a different execution while the probe is
/// held must skip the original reap as SlotGone.
#[tokio::test]
async fn a_reregistered_slot_during_the_probe_await_skips_the_reap() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_old_execution(&db, &work_item_id);
    let other_work_item_id = create_active_chore(&db, &product_id, "other chore");
    let other_id = create_old_execution(&db, &other_work_item_id);
    assert_ne!(
        execution_id, other_id,
        "the replacement registration must be a different execution"
    );
    let execution = db.get_execution(&execution_id).unwrap();
    let live_states = LiveWorkerStateRegistry::new();
    register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let spawn_health = SpawnHealthTracker::new();
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let cube = RecordingCube::default();
    let ctx = SpawnReapCtx::builder()
        .work_db(db.as_ref())
        .live_states(&live_states)
        .coordinator(coordinator.clone())
        .dispatch_events(sink.as_ref())
        .reaper(reaper.as_ref())
        .spawn_health(&spawn_health)
        .cube_client(&cube)
        .build();

    let hold = super::super::probe_hold::ProbeHold::new();
    super::super::probe_hold::arm(&execution_id, Arc::clone(&hold));
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let other = other_id.clone();
    let work_item = other_work_item_id.clone();
    let outcome = tokio::select! {
        outcome = reap_never_started_spawn(
            &ctx,
            &execution,
            1,
            0,
            ReapCause::SpawnAckTimeout { grace_secs: 60 },
            now,
        ) => outcome,
        _ = async {
            tokio::task::spawn_blocking({
                let hold = Arc::clone(&hold);
                move || hold.wait_for_entry()
            })
            .await
            .unwrap();
            register_slot_zero_pid(&live_states, 1, &other, &work_item);
            hold.release();
            std::future::pending::<()>().await
        } => unreachable!(),
    };
    super::super::probe_hold::disarm(&execution_id);

    assert_eq!(outcome, ReapOutcome::Skipped);
    assert_ne!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Orphaned
    );
}

/// After a committed never-started reap, a later hook must not be
/// recorded as driver proof for the registration the sweep is orphaning.
#[tokio::test]
async fn a_committed_reap_refuses_a_later_driver_signal() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_old_execution(&db, &work_item_id);
    let execution = db.get_execution(&execution_id).unwrap();
    let live_states = LiveWorkerStateRegistry::new();
    register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let spawn_health = SpawnHealthTracker::new();
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let cube = RecordingCube::default();
    let ctx = SpawnReapCtx::builder()
        .work_db(db.as_ref())
        .live_states(&live_states)
        .coordinator(coordinator.clone())
        .dispatch_events(sink.as_ref())
        .reaper(reaper.as_ref())
        .spawn_health(&spawn_health)
        .cube_client(&cube)
        .build();
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let outcome = reap_never_started_spawn(
        &ctx,
        &execution,
        1,
        0,
        ReapCause::SpawnAckTimeout { grace_secs: 60 },
        now,
    )
    .await;
    assert_eq!(outcome, ReapOutcome::Reaped);
    assert_eq!(
        live_states.record_driver_signal(&execution_id, DriverSignalKind::HookEvent),
        None,
        "a committed reap must refuse a later hook for this registration",
    );
}

/// A failed orphan write after the fence commits must release the fence so
/// a later pass can still reap and a recovering hook can still prove the
/// driver alive.
#[tokio::test]
async fn a_failed_orphan_write_releases_the_reap_fence() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_old_execution(&db, &work_item_id);
    let execution = db.get_execution(&execution_id).unwrap();
    let live_states = LiveWorkerStateRegistry::new();
    register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let spawn_health = SpawnHealthTracker::new();
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let cube = RecordingCube::default();
    let ctx = SpawnReapCtx::builder()
        .work_db(db.as_ref())
        .live_states(&live_states)
        .coordinator(coordinator.clone())
        .dispatch_events(sink.as_ref())
        .reaper(reaper.as_ref())
        .spawn_health(&spawn_health)
        .cube_client(&cube)
        .build();

    let hold = super::super::probe_hold::ProbeHold::new();
    super::super::probe_hold::arm(&execution_id, Arc::clone(&hold));
    super::super::probe_hold::arm_orphan_write_failure(&execution_id);
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let outcome = tokio::select! {
        outcome = reap_never_started_spawn(
            &ctx,
            &execution,
            1,
            0,
            ReapCause::SpawnAckTimeout { grace_secs: 60 },
            now,
        ) => outcome,
        _ = async {
            tokio::task::spawn_blocking({
                let hold = Arc::clone(&hold);
                move || hold.wait_for_entry()
            })
            .await
            .unwrap();
            hold.release();
            std::future::pending::<()>().await
        } => unreachable!(),
    };
    super::super::probe_hold::disarm(&execution_id);

    assert_eq!(outcome, ReapOutcome::Skipped);
    assert_ne!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Orphaned
    );
    assert_eq!(
        live_states.record_driver_signal(&execution_id, DriverSignalKind::HookEvent),
        Some(1),
        "the fence must be released so a recovering hook is still accepted",
    );

    // The signal above would skip a later pass-1 reap. Clear it by
    // re-registering so the retry can actually orphan.
    register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);
    let later = reap_never_started_spawn(
        &ctx,
        &db.get_execution(&execution_id).unwrap(),
        1,
        0,
        ReapCause::SpawnAckTimeout { grace_secs: 60 },
        now,
    )
    .await;
    assert_eq!(later, ReapOutcome::Reaped);
}

#[test]
fn failure_class_follows_observed_pid_and_probe() {
    let timeout = ReapCause::DriverStartTimeout {
        grace_secs: 300,
        silent_secs: 400,
        activity: "spawning",
    };
    let absent = absent_liveness();
    let present = present_liveness();
    assert_eq!(
        timeout.failure_class(0, &absent),
        crate::spawn_health::SpawnFailureClass::NoShell
    );
    assert_eq!(
        timeout.failure_class(4242, &absent),
        crate::spawn_health::SpawnFailureClass::ShellWithoutDriverSignal
    );
    assert_eq!(
        ReapCause::AppNack { reason: "late" }.failure_class(0, &present),
        crate::spawn_health::SpawnFailureClass::ShellWithoutDriverSignal
    );
    assert_eq!(
        ReapCause::PaneDiedBeforeStart {
            detail: "surface failed"
        }
        .failure_class(0, &absent),
        crate::spawn_health::SpawnFailureClass::NoShell
    );
}

#[test]
fn pane_death_narrative_names_a_present_transcript() {
    let present = present_liveness();
    let (reason, audit, _) = reap_narrative(
        &ReapCause::PaneDiedBeforeStart {
            detail: "surface failed to attach",
        },
        "exec-1",
        &present,
        0,
    );
    assert!(
        reason.contains("a transcript on disk shows the driver had run"),
        "got: {reason}"
    );
    assert!(!reason.contains("no worker process ever existed"), "got: {reason}");
    assert!(audit.contains("the pane died after start"), "got: {audit}");
}

#[test]
fn app_nack_narrative_does_not_claim_no_shell_when_a_transcript_is_present() {
    let present = present_liveness();
    let (reason, _, _) = reap_narrative(&ReapCause::AppNack { reason: "late" }, "exec-1", &present, 0);
    assert!(reason.contains("after the driver had started"), "got: {reason}");
    assert!(!reason.contains("(no shell)"), "got: {reason}");
}

/// An app NACK with a transcript on disk must still reap, and the
/// recorded class / orphan reason must follow the probe, not claim no
/// worker ever existed.
#[tokio::test]
async fn app_nack_with_a_transcript_records_the_probe_not_no_shell() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_spawned_execution(&db, &work_item_id, 0);
    let execution = db.get_execution(&execution_id).unwrap();
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    arm_file_ingress(&db, &execution_id, &root, &workspace, Vec::new());
    std::fs::write(
        root.join("rollout-2026-09-13T21-41-42-sess-1.jsonl"),
        session_meta_line("sess-1", &workspace),
    )
    .unwrap();

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let spawn_health = SpawnHealthTracker::new();
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let live_states = LiveWorkerStateRegistry::new();
    register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);
    let cube = RecordingCube::default();
    let ctx = SpawnReapCtx::builder()
        .work_db(db.as_ref())
        .live_states(&live_states)
        .coordinator(coordinator.clone())
        .dispatch_events(sink.as_ref())
        .reaper(reaper.as_ref())
        .spawn_health(&spawn_health)
        .cube_client(&cube)
        .build();
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let outcome = reap_never_started_spawn(&ctx, &execution, 1, 0, ReapCause::AppNack { reason: "late" }, now).await;

    assert_eq!(outcome, ReapOutcome::Reaped);
    let events = sink.events().await;
    assert_eq!(
        events[0].details["failure_class"],
        serde_json::json!("shell_without_driver_signal")
    );
    let observed = events[0].details["liveness_probe"].as_str().unwrap();
    assert!(observed.contains("transcript present"), "got: {observed}");
    let evidence = spawn_health.evidence_in_window(now);
    assert_eq!(evidence.len(), 1);
    assert_eq!(
        evidence[0].class,
        crate::spawn_health::SpawnFailureClass::ShellWithoutDriverSignal
    );
    assert!(
        evidence[0].observed.contains("after the driver had started"),
        "got: {}",
        evidence[0].observed
    );
    assert!(
        !evidence[0].observed.contains("no worker process ever existed"),
        "got: {}",
        evidence[0].observed
    );
}

#[tokio::test]
async fn pane_died_before_start_with_a_transcript_does_not_deny_the_driver_ran() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_spawned_execution(&db, &work_item_id, 0);
    let execution = db.get_execution(&execution_id).unwrap();
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    arm_file_ingress(&db, &execution_id, &root, &workspace, Vec::new());
    std::fs::write(
        root.join("rollout-2026-09-13T21-41-42-sess-1.jsonl"),
        session_meta_line("sess-1", &workspace),
    )
    .unwrap();

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let spawn_health = SpawnHealthTracker::new();
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let live_states = LiveWorkerStateRegistry::new();
    register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);
    let cube = RecordingCube::default();
    let ctx = SpawnReapCtx::builder()
        .work_db(db.as_ref())
        .live_states(&live_states)
        .coordinator(coordinator.clone())
        .dispatch_events(sink.as_ref())
        .reaper(reaper.as_ref())
        .spawn_health(&spawn_health)
        .cube_client(&cube)
        .build();
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let outcome = reap_never_started_spawn(
        &ctx,
        &execution,
        1,
        0,
        ReapCause::PaneDiedBeforeStart {
            detail: "surface failed to attach",
        },
        now,
    )
    .await;

    assert_eq!(outcome, ReapOutcome::Reaped);
    let evidence = spawn_health.evidence_in_window(now);
    assert_eq!(
        evidence[0].class,
        crate::spawn_health::SpawnFailureClass::ShellWithoutDriverSignal
    );
    assert!(
        evidence[0]
            .observed
            .contains("a transcript on disk shows the driver had run"),
        "got: {}",
        evidence[0].observed
    );
    assert!(
        !evidence[0].observed.contains("no worker process ever existed"),
        "got: {}",
        evidence[0].observed
    );
}

/// A pid-less slot promoted out of `Spawning` is pass 2's: the recorded
/// class must be `NoShell`, not "a pane and shell came up".
#[tokio::test]
async fn zero_pid_driver_start_timeout_is_classed_no_shell() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_spawned_execution(&db, &work_item_id, 0);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    // Pass 1 skips re-adopted slots; pass 2 still verifies driver start and
    // can reap a pid-less registration. `mark_stalled_spawns` will not
    // promote a pid-less slot out of Spawning.
    live_states.register_readoption(
        1,
        &execution_id,
        "grok-4.6",
        0,
        Some(boss_protocol::WorkItemBinding {
            work_item_id: work_item_id.clone(),
            work_item_name: "test chore".to_owned(),
            execution_id: execution_id.clone(),
        }),
        false,
        crate::live_worker_state::LiveSpawnRouting::none(),
        crate::live_worker_state::ReadoptionEvidence::LiveShellPid,
    );
    live_states.set_spawn_time_for_test(
        1,
        boss_engine_utils::epoch_time::now_epoch_secs() - (DRIVER_START_GRACE_SECS + 60),
    );

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let spawn_health = SpawnHealthTracker::new();
    let cube = RecordingCube::default();
    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &spawn_health,
        &cube,
        SPAWN_ACK_GRACE_SECS,
        DRIVER_START_GRACE_SECS,
    )
    .await;

    assert_eq!(outcome.driver_start_reaped, 1);
    assert_eq!(outcome.reaped, 0);
    let events = sink.events().await;
    let reap = events
        .iter()
        .find(|e| e.stage == "driver_start_timeout")
        .expect("pass 2 reap");
    assert_eq!(reap.details["failure_class"], serde_json::json!("no_shell"));
    let evidence = spawn_health.evidence_in_window(boss_engine_utils::epoch_time::now_epoch_secs());
    assert_eq!(evidence[0].class, crate::spawn_health::SpawnFailureClass::NoShell);
    let attentions = db.list_attention_items(&execution_id).unwrap();
    assert!(
        attentions[0].body_markdown.contains("no shell pid was ever reported"),
        "got: {}",
        attentions[0].body_markdown
    );
    assert!(
        !attentions[0].body_markdown.contains("and came up, but"),
        "got: {}",
        attentions[0].body_markdown
    );
}

/// One Undeterminable execution must be probed once per sweep, not twice.
#[tokio::test]
async fn undeterminable_liveness_is_counted_once_per_sweep() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_spawned_execution(&db, &work_item_id, 0);
    let temp = tempfile::TempDir::new().unwrap();
    let missing_root = temp.path().join("never-created");
    arm_file_ingress(&db, &execution_id, &missing_root, temp.path(), Vec::new());

    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);
    live_states.set_spawn_time_for_test(
        1,
        boss_engine_utils::epoch_time::now_epoch_secs() - (DRIVER_START_GRACE_SECS + 60),
    );
    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let spawn_health = SpawnHealthTracker::new();
    let cube = RecordingCube::default();
    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &spawn_health,
        &cube,
        SPAWN_ACK_GRACE_SECS,
        DRIVER_START_GRACE_SECS,
    )
    .await;

    assert_eq!(
        outcome.liveness_undeterminable, 1,
        "pass 2 must not re-probe an execution pass 1 already examined"
    );
}

#[tokio::test]
async fn undeterminable_attention_clears_when_a_later_probe_finds_the_transcript() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_spawned_execution(&db, &work_item_id, 4242);
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let missing = temp.path().join("never-created");
    arm_file_ingress(&db, &execution_id, &missing, &workspace, Vec::new());

    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    live_states.register_spawn_with_capabilities(
        1,
        &execution_id,
        "grok-4.6",
        4242,
        Some(boss_protocol::WorkItemBinding {
            work_item_id: work_item_id.clone(),
            work_item_name: "test chore".to_owned(),
            execution_id: execution_id.clone(),
        }),
        false,
        crate::live_worker_state::LiveSpawnRouting::none(),
    );
    live_states.set_spawn_time_for_test(
        1,
        boss_engine_utils::epoch_time::now_epoch_secs() - (DRIVER_START_GRACE_SECS + 60),
    );
    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let cube = RecordingCube::default();

    async fn pass(
        db: &Arc<WorkDb>,
        live_states: &LiveWorkerStateRegistry,
        coordinator: &Arc<crate::coordinator::ExecutionCoordinator>,
        cube: &RecordingCube,
    ) -> SpawnAckSweepOutcome {
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let spawn_health = SpawnHealthTracker::new();
        run_one_pass(
            db.as_ref(),
            live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &spawn_health,
            cube,
            SPAWN_ACK_GRACE_SECS,
            DRIVER_START_GRACE_SECS,
        )
        .await
    }

    for _ in 0..UNDETERMINABLE_LIVENESS_ATTENTION_THRESHOLD {
        let outcome = pass(&db, &live_states, &coordinator, &cube).await;
        assert_eq!(outcome.liveness_undeterminable, 1);
    }
    let attentions = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(attentions[0].kind, LIVENESS_UNDETERMINABLE_ATTENTION_KIND);
    assert_eq!(attentions[0].status, "open");

    std::fs::create_dir_all(&root).unwrap();
    arm_file_ingress(&db, &execution_id, &root, &workspace, Vec::new());
    std::fs::write(
        root.join("rollout-2026-09-13T21-41-42-sess-1.jsonl"),
        session_meta_line("sess-1", &workspace),
    )
    .unwrap();

    let outcome = pass(&db, &live_states, &coordinator, &cube).await;
    assert_eq!(outcome.vetoed, 1);
    let attentions = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(attentions[0].status, "resolved");
}

#[tokio::test]
async fn undeterminable_attention_clears_when_the_execution_goes_terminal() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_spawned_execution(&db, &work_item_id, 0);
    let temp = tempfile::TempDir::new().unwrap();
    arm_file_ingress(
        &db,
        &execution_id,
        &temp.path().join("never-created"),
        temp.path(),
        Vec::new(),
    );

    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);
    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let cube = RecordingCube::default();

    for _ in 0..UNDETERMINABLE_LIVENESS_ATTENTION_THRESHOLD {
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let spawn_health = SpawnHealthTracker::new();
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &spawn_health,
            &cube,
            SPAWN_ACK_GRACE_SECS,
            DRIVER_START_GRACE_SECS,
        )
        .await;
        assert_eq!(outcome.liveness_undeterminable, 1);
    }
    assert_eq!(db.list_attention_items(&execution_id).unwrap()[0].status, "open");

    db.mark_execution_orphaned(&execution_id, "test: terminated").unwrap();

    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let spawn_health = SpawnHealthTracker::new();
    let _ = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &spawn_health,
        &cube,
        SPAWN_ACK_GRACE_SECS,
        DRIVER_START_GRACE_SECS,
    )
    .await;

    assert_eq!(db.list_attention_items(&execution_id).unwrap()[0].status, "resolved");
}

/// A worker that recovers into Working (driver signal + activity change)
/// is skipped by the candidate loops; reconciliation must still clear the
/// undeterminable item.
#[tokio::test]
async fn undeterminable_attention_clears_when_the_worker_recovers_into_working() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_spawned_execution(&db, &work_item_id, 4242);
    let temp = tempfile::TempDir::new().unwrap();
    arm_file_ingress(
        &db,
        &execution_id,
        &temp.path().join("never-created"),
        temp.path(),
        Vec::new(),
    );

    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot_with_live_shell(&live_states, 1, &execution_id, &work_item_id, 4242, false);
    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let cube = RecordingCube::default();

    for _ in 0..UNDETERMINABLE_LIVENESS_ATTENTION_THRESHOLD {
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let spawn_health = SpawnHealthTracker::new();
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &spawn_health,
            &cube,
            SPAWN_ACK_GRACE_SECS,
            DRIVER_START_GRACE_SECS,
        )
        .await;
        assert_eq!(outcome.liveness_undeterminable, 1);
    }
    assert_eq!(db.list_attention_items(&execution_id).unwrap()[0].status, "open");

    assert_eq!(
        live_states.record_driver_signal(&execution_id, DriverSignalKind::HookEvent),
        Some(1),
    );
    live_states.set_activity_for_test(1, WorkerActivity::Working);

    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let spawn_health = SpawnHealthTracker::new();
    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &spawn_health,
        &cube,
        SPAWN_ACK_GRACE_SECS,
        DRIVER_START_GRACE_SECS,
    )
    .await;
    assert_eq!(outcome.liveness_undeterminable, 0);
    assert_eq!(outcome.skipped.not_spawning, 1);
    assert_eq!(db.list_attention_items(&execution_id).unwrap()[0].status, "resolved");
}

/// Terminal completion followed by live-slot teardown must still clear
/// the undeterminable item — the candidate loops no longer see the slot.
#[tokio::test]
async fn undeterminable_attention_clears_after_terminal_status_and_slot_removal() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_spawned_execution(&db, &work_item_id, 0);
    let temp = tempfile::TempDir::new().unwrap();
    arm_file_ingress(
        &db,
        &execution_id,
        &temp.path().join("never-created"),
        temp.path(),
        Vec::new(),
    );

    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);
    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let cube = RecordingCube::default();

    for _ in 0..UNDETERMINABLE_LIVENESS_ATTENTION_THRESHOLD {
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let spawn_health = SpawnHealthTracker::new();
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &spawn_health,
            &cube,
            SPAWN_ACK_GRACE_SECS,
            DRIVER_START_GRACE_SECS,
        )
        .await;
        assert_eq!(outcome.liveness_undeterminable, 1);
    }
    assert_eq!(db.list_attention_items(&execution_id).unwrap()[0].status, "open");

    db.mark_execution_orphaned(&execution_id, "test: terminated").unwrap();
    live_states.release_slot(1);

    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let spawn_health = SpawnHealthTracker::new();
    let _ = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &spawn_health,
        &cube,
        SPAWN_ACK_GRACE_SECS,
        DRIVER_START_GRACE_SECS,
    )
    .await;

    assert_eq!(db.list_attention_items(&execution_id).unwrap()[0].status, "resolved");
}
