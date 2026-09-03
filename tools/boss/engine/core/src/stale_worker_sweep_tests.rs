use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use boss_protocol::{WorkItemBinding, WorkerEvent};

use super::*;
use crate::coordinator::ExecutionCoordinator;
use crate::dispatch_events::RecordingDispatchEventSink;
use crate::driver::ProgressFidelity;
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::test_support::*;
use crate::work::ExecutionStatus;

// A staleness threshold whose cutoff lands in the *future*, so any
// `last_event_at` stamped "now" by `apply_event` compares as stale.
// This lets us exercise the staleness branch deterministically
// without a way to backdate the in-memory `last_event_at`.
const ALWAYS_STALE: i64 = -120;
// A threshold whose cutoff is an hour in the past, so a just-stamped
// event is comfortably fresh.
const NEVER_STALE: i64 = 3_600;

// ─── classify_semantic_staleness ────────────────────────────────────────

// A tool call in flight is healthy however old the checkpoint is — a
// long foreground `bazel build` reports no intervening event, and
// reaping that would break real work.
#[test]
fn in_flight_tool_is_healthy_however_old_the_checkpoint() {
    let now = 10_000;
    assert_eq!(
        classify_semantic_staleness(
            SemanticToolCondition::InFlight,
            Some(&iso8601_utc(0)),
            &iso8601_utc(0),
            ProgressFidelity::Rich,
            now,
            1_000,
        ),
        SemanticStaleness::Healthy,
    );
}

// A checkpoint inside the threshold is healthy regardless of tool
// condition (idle or unknown).
#[test]
fn fresh_checkpoint_is_healthy_regardless_of_tool_condition() {
    let now = 10_000;
    for tool_condition in [SemanticToolCondition::Idle, SemanticToolCondition::Unknown] {
        assert_eq!(
            classify_semantic_staleness(
                tool_condition,
                Some(&iso8601_utc(9_500)),
                &iso8601_utc(0),
                ProgressFidelity::Rich,
                now,
                1_000,
            ),
            SemanticStaleness::Healthy,
            "tool_condition={tool_condition:?}",
        );
    }
}

// The core regression this rewrite exists for: a `Rich` driver whose
// tool condition is durably idle and whose checkpoint predates the
// threshold is stale — and this holds regardless of what tmux terminal
// signals would have said (`classify_semantic_staleness` never takes
// `window_activity`/`pane_current_command` as parameters at all, so an
// attached, continuously repainting TUI cannot mask this verdict the
// way it masked the old `classify_worker_liveness`).
#[test]
fn idle_stale_checkpoint_on_a_rich_driver_is_stale() {
    let now = 10_000;
    assert_eq!(
        classify_semantic_staleness(
            SemanticToolCondition::Idle,
            Some(&iso8601_utc(8_000)),
            &iso8601_utc(0),
            ProgressFidelity::Rich,
            now,
            1_000,
        ),
        SemanticStaleness::Stale {
            progress_at: iso8601_utc(8_000),
        },
    );
}

// A durably-unknown tool condition never reaches `Stale`, even once
// stale — there is no evidence a tool is *not* in flight, so this is
// degraded evidence, not a health verdict either way.
#[test]
fn stale_but_unknown_tool_condition_is_degraded_evidence() {
    let now = 10_000;
    assert_eq!(
        classify_semantic_staleness(
            SemanticToolCondition::Unknown,
            Some(&iso8601_utc(8_000)),
            &iso8601_utc(0),
            ProgressFidelity::Rich,
            now,
            1_000,
        ),
        SemanticStaleness::DegradedEvidence(DegradedEvidenceReason::ToolConditionUnknown),
    );
}

// No checkpoint at all (no driver event ever recorded) falls back to
// the execution's own `started_at` as the clock — fresh start is
// healthy, an old start with still no evidence is degraded.
#[test]
fn no_checkpoint_uses_started_at_as_the_fallback_clock() {
    let now = 10_000;
    assert_eq!(
        classify_semantic_staleness(
            SemanticToolCondition::Unknown,
            None,
            &iso8601_utc(9_500),
            ProgressFidelity::Rich,
            now,
            1_000,
        ),
        SemanticStaleness::Healthy,
        "a recently-started run with no checkpoint yet is still within grace",
    );
    assert_eq!(
        classify_semantic_staleness(
            SemanticToolCondition::Unknown,
            None,
            &iso8601_utc(0),
            ProgressFidelity::Rich,
            now,
            1_000,
        ),
        SemanticStaleness::DegradedEvidence(DegradedEvidenceReason::ToolConditionUnknown),
    );
}

// A driver below `Rich` fidelity is degraded evidence even when the
// tool condition durably reads idle and the checkpoint is stale — the
// local-dispatch fidelity requirement takes priority over an otherwise
// clean `Stale` read, since cadence judgement is not valid below `Rich`.
#[test]
fn fidelity_below_rich_is_degraded_evidence_even_when_idle_and_stale() {
    let now = 10_000;
    for fidelity in [ProgressFidelity::Coarse, ProgressFidelity::Minimal] {
        assert_eq!(
            classify_semantic_staleness(
                SemanticToolCondition::Idle,
                Some(&iso8601_utc(8_000)),
                &iso8601_utc(0),
                fidelity,
                now,
                1_000,
            ),
            SemanticStaleness::DegradedEvidence(DegradedEvidenceReason::FidelityBelowRich),
            "fidelity={fidelity:?}",
        );
    }
}

/// Records every `reap_worker` call and, at reap time, snapshots
/// whether the execution's pool slot is still claimed. The production
/// reaper (`ServerState::release_worker_pane`) kills the OS process
/// tree and frees the slot; this stub only records, leaving the
/// sweep's own `release_worker_and_kick` to free the slot — so the
/// "still claimed at reap time" snapshot proves the reap ran BEFORE
/// the slot/lease was released (defect 2's ordering requirement).
struct RecordingReaper {
    coordinator: Arc<ExecutionCoordinator>,
    reaped: StdMutex<Vec<(String, bool)>>,
}

impl RecordingReaper {
    fn new(coordinator: Arc<ExecutionCoordinator>) -> Self {
        Self {
            coordinator,
            reaped: StdMutex::new(Vec::new()),
        }
    }

    /// `(execution_id, slot_still_claimed_at_reap)` for each reap.
    fn reaped(&self) -> Vec<(String, bool)> {
        self.reaped.lock().unwrap().clone()
    }
}

struct StaticTerminalInspector(TerminalLiveness);

#[async_trait]
impl WorkerTerminalInspector for StaticTerminalInspector {
    async fn inspect(&self, _execution_id: &str) -> Result<Option<TerminalLiveness>> {
        Ok(Some(self.0.clone()))
    }
}

/// A terminal inspector that always fails the probe — models a tmux
/// command erroring (e.g. the server is unreachable). The sweep must
/// treat this as "unknown", never as "dead".
struct FailingTerminalInspector;

#[async_trait]
impl WorkerTerminalInspector for FailingTerminalInspector {
    async fn inspect(&self, _execution_id: &str) -> Result<Option<TerminalLiveness>> {
        Err(anyhow::anyhow!("tmux probe failed"))
    }
}

#[async_trait]
impl StaleWorkerReaper for RecordingReaper {
    async fn reap_worker(&self, execution_id: &str) {
        let still_claimed = self
            .coordinator
            .worker_pool()
            .claimed_execution_ids()
            .await
            .contains(execution_id);
        self.reaped
            .lock()
            .unwrap()
            .push((execution_id.to_owned(), still_claimed));
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────

fn register_slot(live_states: &LiveWorkerStateRegistry, slot_id: u8, execution_id: &str, work_item_id: &str) {
    live_states.register_spawn(
        slot_id,
        execution_id,
        "claude-opus-4-7",
        std::process::id() as i32,
        Some(WorkItemBinding {
            work_item_id: work_item_id.to_owned(),
            work_item_name: "test chore".to_owned(),
            execution_id: execution_id.to_owned(),
        }),
    );
}

/// Drive a slot to `Working` with NO tool in flight (a balanced
/// PreToolUse/PostToolUse pair). `last_event_at` is stamped "now".
fn drive_to_working_idle(live_states: &LiveWorkerStateRegistry, slot_id: u8) {
    live_states.apply_event(
        slot_id,
        &WorkerEvent::PreToolUse {
            session_id: "s".to_owned(),
            tool_name: "Bash".to_owned(),
            tool_input: serde_json::json!({}),
        },
    );
    live_states.apply_event(
        slot_id,
        &WorkerEvent::PostToolUse {
            session_id: "s".to_owned(),
            tool_name: "Bash".to_owned(),
            tool_input: serde_json::json!({}),
            tool_response: serde_json::json!({}),
        },
    );
}

/// Drive a slot to `Working` WITH a tool in flight (PreToolUse only,
/// no balancing PostToolUse) — models a long foreground bazel build.
fn drive_to_working_tool_in_flight(live_states: &LiveWorkerStateRegistry, slot_id: u8) {
    live_states.apply_event(
        slot_id,
        &WorkerEvent::PreToolUse {
            session_id: "s".to_owned(),
            tool_name: "Bash".to_owned(),
            tool_input: serde_json::json!({}),
        },
    );
}

/// A live tmux pane at an arbitrary `#{window_activity}` reading — a
/// value the classifier carries as a diagnostic only and never consults
/// for a health verdict.
fn live_terminal(window_activity_epoch_secs: i64) -> TerminalLiveness {
    TerminalLiveness::Alive {
        session_name: "boss-worker-test".to_owned(),
        window_activity_epoch_secs,
        pane_current_command: None,
    }
}

// ─── tests ───────────────────────────────────────────────────────────────

/// The core invariant: a `working`, tool-idle slot whose last hook is
/// older than the threshold has its execution orphaned, its pool slot
/// released, and a `stale_worker_reconcile` event emitted.
#[tokio::test]
async fn stale_idle_worker_is_reaped() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_old_execution(&db, &work_item_id);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    drive_to_working_idle(&live_states, 1);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let claimed_before = coordinator.worker_pool().claimed_execution_ids().await;
    assert!(claimed_before.contains(&execution_id));

    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &HoldRegistry::new(),
        ALWAYS_STALE,
    )
    .await;

    assert_eq!(outcome.reaped, 1, "stale idle worker must be reaped");

    let exec = db.get_execution(&execution_id).unwrap();
    assert_eq!(exec.status, ExecutionStatus::Orphaned);

    let claimed_after = coordinator.worker_pool().claimed_execution_ids().await;
    assert!(
        !claimed_after.contains(&execution_id),
        "pool slot must be released after reap",
    );

    // Defect 2: the worker's process tree must be reaped, and the reap
    // must run BEFORE the pool slot / cube lease is released. The
    // recording reaper snapshots the slot as still-claimed at reap
    // time, which pins the reap-before-release ordering.
    assert_eq!(
        reaper.reaped(),
        vec![(execution_id.clone(), true)],
        "reconcile must reap the process tree before releasing the slot/lease",
    );

    let events = sink.events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].stage, "stale_worker_reconcile");
    assert_eq!(events[0].outcome, "ok");
    assert_eq!(events[0].work_item_id.as_deref(), Some(work_item_id.as_str()));

    let item = db.get_work_item(&work_item_id).unwrap();
    let desc = match &item {
        boss_protocol::WorkItem::Chore(t) | boss_protocol::WorkItem::Task(t) => t.description.clone(),
        _ => panic!("expected chore"),
    };
    assert!(desc.contains("[engine-reconcile]"), "got: {desc:?}");
}

#[tokio::test]
async fn stuck_tmux_worker_raises_attention_without_reaping() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);
    let execution_id = create_old_execution(&db, &work_item_id);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    drive_to_working_idle(&live_states, 1);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let inspector = StaticTerminalInspector(live_terminal(0));
    let hold_registry = HoldRegistry::new();
    let outcome = run_one_pass_with_terminal(
        db.as_ref(),
        &live_states,
        Some(&inspector),
        coordinator.clone(),
        sink.as_ref(),
        StaleWorkerSweepControls {
            reaper: reaper.as_ref(),
            hold_registry: &hold_registry,
            cube_client: &NoopCube,
        },
        ALWAYS_STALE,
    )
    .await;

    assert_eq!(outcome.genuinely_stuck, 1);
    assert_eq!(outcome.reaped, 0);
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
    assert!(reaper.reaped().is_empty());
    let attention = db
        .list_attention_items_for_work_item(&work_item_id)
        .unwrap()
        .into_iter()
        .find(|item| item.kind == STALE_WORKER_ATTENTION_KIND)
        .expect("stuck worker must raise an attention item");
    assert!(
        attention
            .body_markdown
            .contains("tmux -S /state/boss/tmux.sock attach-session -t 'boss-worker-test'")
    );
}

// The core end-to-end regression this rewrite exists for: an attached,
// continuously repainting TUI (huge/fresh `#{window_activity}`) must
// NOT rescue a semantically stale worker — window_activity is a
// diagnostic only and is never consulted for the health verdict. Before
// this rewrite, `output_is_recent` in `classify_worker_liveness` made
// this exact scenario `AliveAndWorking`, which is the inertness the
// design doc's investigation identified.
#[tokio::test]
async fn fresh_window_activity_does_not_rescue_a_semantically_stale_worker() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);
    let execution_id = create_old_execution(&db, &work_item_id);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    drive_to_working_idle(&live_states, 1);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    // A spinner that has advanced `window_activity` to the maximum
    // possible reading — as fresh as terminal evidence can look.
    let inspector = StaticTerminalInspector(live_terminal(i64::MAX / 2));
    let hold_registry = HoldRegistry::new();
    let outcome = run_one_pass_with_terminal(
        db.as_ref(),
        &live_states,
        Some(&inspector),
        coordinator.clone(),
        sink.as_ref(),
        StaleWorkerSweepControls {
            reaper: reaper.as_ref(),
            hold_registry: &hold_registry,
            cube_client: &NoopCube,
        },
        ALWAYS_STALE,
    )
    .await;

    assert_eq!(
        outcome.genuinely_stuck, 1,
        "fresh window_activity must not mask a stale semantic-progress checkpoint",
    );
    assert_eq!(outcome.alive_and_working, 0);
    assert_eq!(outcome.reaped, 0);
    assert!(reaper.reaped().is_empty());
}

/// The tmux-reported dead-pane arm reaps through
/// `reap_observed_worker_death` with the `pane_dead_status` folded into
/// the reason string, and resolves any previously-open `stale_worker`
/// attention for the work item — the ProducerReconciles contract
/// `attention_lifecycle.rs` declares for this kind.
#[tokio::test]
async fn dead_tmux_pane_is_reaped_and_resolves_open_attention() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);
    let execution_id = create_old_execution(&db, &work_item_id);
    db.start_execution_run(&execution_id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    drive_to_working_idle(&live_states, 1);

    // A prior pass raised a stale_worker attention (e.g. the worker was
    // genuinely stuck before its pane died). It must be resolved once
    // the pane is confirmed dead, or it is stranded open forever.
    db.upsert_external_tracker_attention(
        &work_item_id,
        STALE_WORKER_ATTENTION_KIND,
        "Worker appears stuck; inspection required",
        "prior body",
    )
    .unwrap();

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let inspector = StaticTerminalInspector(TerminalLiveness::Dead {
        session_name: "boss-worker-test".to_owned(),
        evidence: DeadPaneEvidence::PaneExited {
            pane_dead_status: Some("143".to_owned()),
        },
    });
    let hold_registry = HoldRegistry::new();
    let outcome = run_one_pass_with_terminal(
        db.as_ref(),
        &live_states,
        Some(&inspector),
        coordinator.clone(),
        sink.as_ref(),
        StaleWorkerSweepControls {
            reaper: reaper.as_ref(),
            hold_registry: &hold_registry,
            cube_client: &AlwaysSucceedsCube,
        },
        ALWAYS_STALE,
    )
    .await;

    assert_eq!(outcome.dead, 1);
    assert_eq!(outcome.reaped, 1);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Orphaned
    );
    let run = db.list_runs(&execution_id).unwrap().pop().expect("orphaned run");
    let run_reason = run.result_summary.or(run.error_text).unwrap_or_default();
    assert!(
        run_reason.contains("pane_dead_status=143"),
        "reap reason must record tmux pane_dead_status, got {run_reason:?}"
    );
    // `reap_observed_worker_death` reconciles through the dead-pid-sweep
    // teardown path, not the injected `StaleWorkerReaper` — that
    // reaper backs only the cadence-fallback OS-process kill.
    assert!(reaper.reaped().is_empty());

    let open_items: Vec<_> = db
        .list_attention_items_for_work_item(&work_item_id)
        .unwrap()
        .into_iter()
        .filter(|item| item.kind == STALE_WORKER_ATTENTION_KIND && item.status == "open")
        .collect();
    assert!(
        open_items.is_empty(),
        "a dead pane must resolve any open stale_worker attention for the work item: {open_items:?}",
    );
}

/// A tmux probe failure must never be treated as death, and never
/// jumps straight to `genuinely_stuck` — without a confirmed-live
/// session Boss cannot name an exact identity to act on. Once the
/// underlying semantic evidence itself reads non-healthy, it raises the
/// dedicated probe-unavailable attention (recording that cadence
/// result) instead — never destructive, but no longer silent either.
#[tokio::test]
async fn terminal_probe_failure_raises_probe_unavailable_attention_without_reaping() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);
    let execution_id = create_old_execution(&db, &work_item_id);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    drive_to_working_idle(&live_states, 1);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let inspector = FailingTerminalInspector;
    let hold_registry = HoldRegistry::new();
    let outcome = run_one_pass_with_terminal(
        db.as_ref(),
        &live_states,
        Some(&inspector),
        coordinator.clone(),
        sink.as_ref(),
        StaleWorkerSweepControls {
            reaper: reaper.as_ref(),
            hold_registry: &hold_registry,
            cube_client: &NoopCube,
        },
        ALWAYS_STALE,
    )
    .await;

    assert_eq!(outcome.terminal_probe_failed, 1);
    assert_eq!(outcome.reaped, 0);
    assert_eq!(outcome.dead, 0);
    assert_eq!(
        outcome.genuinely_stuck, 0,
        "no confirmed-live identity ⇒ never genuinely_stuck"
    );
    assert_eq!(outcome.alive_and_working, 0);
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
    assert!(reaper.reaped().is_empty());

    let events = sink.events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].stage, "stale_worker_reconcile");
    assert_eq!(events[0].outcome, "skipped");

    let attention = db
        .list_attention_items_for_work_item(&work_item_id)
        .unwrap()
        .into_iter()
        .find(|item| item.kind == STALE_WORKER_ATTENTION_KIND)
        .expect("a probe-unavailable attention must be raised");
    assert!(attention.body_markdown.contains("tmux identity probe failed"));
}

/// A slot classified `Healthy` (semantic-progress checkpoint's tool
/// condition is `in_flight`, so it is healthy however stale the
/// threshold) resolves any previously-open `stale_worker` attention —
/// the recovery half of the ProducerReconciles contract. Uses a
/// deliberately ancient `window_activity` reading to prove the
/// terminal signal plays no part in the verdict either way.
#[tokio::test]
async fn alive_and_working_resolves_previously_open_attention() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);
    let execution_id = create_old_execution(&db, &work_item_id);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    drive_to_working_tool_in_flight(&live_states, 1);

    db.upsert_external_tracker_attention(
        &work_item_id,
        STALE_WORKER_ATTENTION_KIND,
        "Worker appears stuck; inspection required",
        "prior body",
    )
    .unwrap();

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let inspector = StaticTerminalInspector(live_terminal(0));
    let hold_registry = HoldRegistry::new();
    let outcome = run_one_pass_with_terminal(
        db.as_ref(),
        &live_states,
        Some(&inspector),
        coordinator.clone(),
        sink.as_ref(),
        StaleWorkerSweepControls {
            reaper: reaper.as_ref(),
            hold_registry: &hold_registry,
            cube_client: &NoopCube,
        },
        ALWAYS_STALE,
    )
    .await;

    assert_eq!(outcome.alive_and_working, 1);
    assert_eq!(outcome.reaped, 0);
    assert!(reaper.reaped().is_empty());

    let open_items: Vec<_> = db
        .list_attention_items_for_work_item(&work_item_id)
        .unwrap()
        .into_iter()
        .filter(|item| item.kind == STALE_WORKER_ATTENTION_KIND && item.status == "open")
        .collect();
    assert!(
        open_items.is_empty(),
        "a recovered worker must resolve its open stale_worker attention: {open_items:?}",
    );
}

/// A slot whose execution has already reached a terminal DB status is
/// skipped before the inspector (the production inspector cannot return
/// `Some` for a terminal execution — `tmux_run_for_execution` excludes
/// them) and any open `stale_worker` attention is resolved.
#[tokio::test]
async fn terminal_execution_is_skipped_and_resolves_open_attention() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);
    let execution_id = create_old_execution(&db, &work_item_id);
    db.mark_execution_orphaned(&execution_id, "test: already settled")
        .unwrap();

    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    drive_to_working_idle(&live_states, 1);

    db.upsert_external_tracker_attention(
        &work_item_id,
        STALE_WORKER_ATTENTION_KIND,
        "Worker appears stuck; inspection required",
        "prior body",
    )
    .unwrap();

    let coordinator = make_coordinator(db.clone(), 1);
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let hold_registry = HoldRegistry::new();
    // No inspector: the production `tmux_run_for_execution` predicate
    // returns `None` for a terminal execution, so the sweep used to fall
    // through to cadence and `continue` without resolving attention.
    let outcome = run_one_pass_with_terminal(
        db.as_ref(),
        &live_states,
        None,
        coordinator.clone(),
        sink.as_ref(),
        StaleWorkerSweepControls {
            reaper: reaper.as_ref(),
            hold_registry: &hold_registry,
            cube_client: &NoopCube,
        },
        ALWAYS_STALE,
    )
    .await;

    assert_eq!(outcome.reaped, 0);
    assert_eq!(outcome.dead, 0);
    assert_eq!(outcome.genuinely_stuck, 0);
    assert_eq!(outcome.alive_and_working, 0);
    assert!(reaper.reaped().is_empty());
    assert!(sink.events().await.is_empty());

    let open_items: Vec<_> = db
        .list_attention_items_for_work_item(&work_item_id)
        .unwrap()
        .into_iter()
        .filter(|item| item.kind == STALE_WORKER_ATTENTION_KIND && item.status == "open")
        .collect();
    assert!(
        open_items.is_empty(),
        "a terminal execution must resolve its open stale_worker attention: {open_items:?}",
    );
}

/// A `working` slot whose last hook is *recent* (within the
/// threshold) is left alone — the common healthy case.
#[tokio::test]
async fn fresh_worker_is_not_reaped() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_old_execution(&db, &work_item_id);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    drive_to_working_idle(&live_states, 1);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &HoldRegistry::new(),
        NEVER_STALE,
    )
    .await;

    assert_eq!(outcome.reaped, 0, "fresh worker must not be reaped");
    assert_eq!(outcome.fresh_skipped, 1);
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
}

/// A `working` slot WITH a tool in flight (e.g. a long foreground
/// bazel build) is never reaped even past the threshold — this is the
/// critical false-positive guard.
#[tokio::test]
async fn worker_with_tool_in_flight_is_not_reaped() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_old_execution(&db, &work_item_id);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    drive_to_working_tool_in_flight(&live_states, 1);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &HoldRegistry::new(),
        ALWAYS_STALE,
    )
    .await;

    assert_eq!(
        outcome.reaped, 0,
        "a tool in flight (long foreground build) must never be reaped",
    );
    assert_eq!(outcome.tool_in_flight_skipped, 1);
    assert!(sink.events().await.is_empty());
}

/// A slot that is still `Spawning` (no working transition yet) is not
/// a candidate.
#[tokio::test]
async fn non_working_activity_is_skipped() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_old_execution(&db, &work_item_id);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    // Left at Spawning — no events applied.

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &HoldRegistry::new(),
        ALWAYS_STALE,
    )
    .await;

    assert_eq!(outcome.reaped, 0);
    assert_eq!(outcome.not_working_skipped, 1);
}

/// A stale-looking `working` slot whose execution started within the
/// grace window is skipped, guarding against racing a fresh dispatch.
#[tokio::test]
async fn recent_started_at_is_skipped() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_execution_started_now(&db, &work_item_id);

    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    drive_to_working_idle(&live_states, 1);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &HoldRegistry::new(),
        ALWAYS_STALE,
    )
    .await;

    assert_eq!(outcome.reaped, 0, "grace period must prevent reaping fresh dispatches");
    assert_eq!(outcome.grace_skipped, 1);
}

/// Regression test (a) for the false-positive cancel: slot reuse.
///
/// Slot 1 was recycled — its live-state `run_id` now points at the
/// CURRENT execution, but its `last_event_at` still carries a PRIOR
/// run's (much older) timestamp, the recycled-slot attribution
/// artifact from the incident. The current execution started AFTER
/// that stale timestamp. Even at an always-stale threshold, the
/// reconciler must NOT reap: a hook timestamp predating the
/// execution's own `started_at` cannot be one of its events, so
/// staleness is un-evaluable and the (healthy) worker is left alone.
/// Without the event-attribution guard this slot would be reaped,
/// false-cancelling a live worker.
#[tokio::test]
async fn slot_reuse_stale_prior_event_is_not_reaped() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    // The CURRENT execution started 5 minutes ago (clears the grace
    // window) — `create_old_execution` stamps started_at to now-300.
    let execution_id = create_old_execution(&db, &work_item_id);
    let started_epoch = db
        .get_execution(&execution_id)
        .unwrap()
        .started_at
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap();

    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    drive_to_working_idle(&live_states, 1);
    // The recycled slot carries a PRIOR run's last-event timestamp,
    // an hour before THIS execution even started — the exact
    // mis-attribution the incident hit (last_event_at "03:43:55Z"
    // predating the 06:24Z dispatch).
    live_states.set_last_event_at_for_test(1, iso8601_utc(started_epoch - 3_600));

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &HoldRegistry::new(),
        ALWAYS_STALE,
    )
    .await;

    assert_eq!(
        outcome.reaped, 0,
        "a worker whose only 'staleness' is a recycled-slot prior-run timestamp must not be reaped",
    );
    assert_eq!(outcome.pre_start_event_skipped, 1);
    // Execution untouched, slot still claimed, no reap, no event.
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
    assert!(
        coordinator
            .worker_pool()
            .claimed_execution_ids()
            .await
            .contains(&execution_id),
        "the healthy worker's slot must remain claimed",
    );
    assert!(
        reaper.reaped().is_empty(),
        "no process reap may fire for a healthy worker"
    );
    assert!(sink.events().await.is_empty());
}

/// Regression test (b): a legitimate reconcile-cancel must reap the
/// worker's process tree BEFORE the slot/lease is freed. The
/// recording reaper captures the pool slot as still-claimed at reap
/// time; combined with the slot being released by the end of the
/// pass, that pins the reap-before-release ordering the incident
/// required (lease freed while the process lived is what produced the
/// shared-workspace catastrophe).
#[tokio::test]
async fn reconcile_reaps_process_before_releasing_slot() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_old_execution(&db, &work_item_id);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    drive_to_working_idle(&live_states, 1);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &HoldRegistry::new(),
        ALWAYS_STALE,
    )
    .await;

    assert_eq!(outcome.reaped, 1);
    // Exactly one reap, for this execution, observed while the slot
    // was STILL claimed → reap ran before the slot/lease release.
    assert_eq!(reaper.reaped(), vec![(execution_id.clone(), true)]);
    // …and by the end of the pass the slot is released.
    assert!(
        !coordinator
            .worker_pool()
            .claimed_execution_ids()
            .await
            .contains(&execution_id),
        "slot must be released after the reap",
    );
}

/// Coarse-fidelity slots are exempt from the cadence fallback only.
/// Terminal evidence still classifies them; this test has no inspector.
#[tokio::test]
async fn coarse_fidelity_slot_is_exempt_from_cadence_staleness_without_terminal_evidence() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_old_execution(&db, &work_item_id);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    live_states.set_progress_fidelity(1, ProgressFidelity::Coarse);
    drive_to_working_idle(&live_states, 1);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &HoldRegistry::new(),
        ALWAYS_STALE,
    )
    .await;

    assert_eq!(outcome.reaped, 0, "a Coarse-fidelity slot must never be cadence-reaped");
    assert_eq!(outcome.fidelity_exempt_skipped, 1);
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
    assert!(reaper.reaped().is_empty());
    assert!(sink.events().await.is_empty());
}

/// Minimal fidelity receives the same cadence-only exemption.
#[tokio::test]
async fn minimal_fidelity_slot_is_exempt_from_cadence_staleness_without_terminal_evidence() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_old_execution(&db, &work_item_id);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    live_states.set_progress_fidelity(1, ProgressFidelity::Minimal);
    drive_to_working_idle(&live_states, 1);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &HoldRegistry::new(),
        ALWAYS_STALE,
    )
    .await;

    assert_eq!(
        outcome.reaped, 0,
        "a Minimal-fidelity slot must never be cadence-reaped"
    );
    assert_eq!(outcome.fidelity_exempt_skipped, 1);
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
    assert!(reaper.reaped().is_empty());
    assert!(sink.events().await.is_empty());
}

/// A slot with no declared fidelity (the default, and every existing
/// call site's behaviour) is treated as `Rich` and reaped exactly like
/// today — this is the "Claude's sweep behaviour must be unchanged"
/// requirement, pinned as a test.
#[tokio::test]
async fn unset_fidelity_defaults_to_rich_and_reaps_like_today() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_old_execution(&db, &work_item_id);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    // No set_progress_fidelity call — this is the untouched default.
    drive_to_working_idle(&live_states, 1);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &HoldRegistry::new(),
        ALWAYS_STALE,
    )
    .await;

    assert_eq!(
        outcome.reaped, 1,
        "default (Rich) fidelity must reap exactly like before this change"
    );
    assert_eq!(
        outcome.fidelity_exempt_skipped, 0,
        "a Rich/default slot is judged, not exempted"
    );
}

/// A stale-looking `working` slot whose execution an operator has
/// explicitly held (`bossctl agents hold`) is never reaped, even past
/// the threshold — the auto-reap sweep must respect a hold.
#[tokio::test]
async fn held_worker_is_not_reaped() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);

    let execution_id = create_old_execution(&db, &work_item_id);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    drive_to_working_idle(&live_states, 1);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let hold_registry = HoldRegistry::new();
    hold_registry.hold(&execution_id, Some("debugging by hand".to_owned()), 0);

    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &hold_registry,
        ALWAYS_STALE,
    )
    .await;

    assert_eq!(outcome.reaped, 0, "a held worker must never be reaped");
    assert_eq!(outcome.held_skipped, 1);
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
    assert!(
        coordinator
            .worker_pool()
            .claimed_execution_ids()
            .await
            .contains(&execution_id),
        "a held worker's slot must remain claimed",
    );
    assert!(reaper.reaped().is_empty(), "no process reap may fire for a held worker");
}

/// An inferred Dead verdict (session absent / token mismatch) must not
/// reap when durable_liveness still sees the recorded worker pid alive.
#[tokio::test]
async fn inferred_dead_skips_reap_when_durable_pid_is_alive() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let db = Arc::new(db);
    let execution_id = create_old_execution(&db, &work_item_id);
    db.start_execution_run(&execution_id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    assert!(
        db.set_run_shell_pid_for_execution(&execution_id, i64::from(std::process::id()))
            .unwrap()
    );

    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    drive_to_working_idle(&live_states, 1);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let inspector = StaticTerminalInspector(TerminalLiveness::Dead {
        session_name: "boss-worker-test".to_owned(),
        evidence: DeadPaneEvidence::SessionAbsent,
    });
    let hold_registry = HoldRegistry::new();
    let outcome = run_one_pass_with_terminal(
        db.as_ref(),
        &live_states,
        Some(&inspector),
        coordinator.clone(),
        sink.as_ref(),
        StaleWorkerSweepControls {
            reaper: reaper.as_ref(),
            hold_registry: &hold_registry,
            cube_client: &NoopCube,
        },
        ALWAYS_STALE,
    )
    .await;

    assert_eq!(outcome.dead_uncorroborated, 1);
    assert_eq!(outcome.dead, 0);
    assert_eq!(outcome.reaped, 0);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Running
    );
    assert!(reaper.reaped().is_empty());
}

/// An example `#{pane_current_command}` value carried through
/// [`TerminalLiveness::Alive`] as a diagnostic only — never compared
/// against anything by the classifier. A live Boss coordinator pane
/// reported `#{pane_current_command}` as `2.1.235` (observed
/// 2026-08-19) — a version string, not a binary name. Worker panes are
/// spawned as `<login-shell> -l -i -c '. .boss/<script>'`, so this field
/// is the sourced script's current child (`claude` when the descriptor
/// is on PATH as itself; otherwise `node`, the shell, or a version
/// string like the coordinator observation).
const CLASSIFIER_DRIVER_BINARY: &str = "claude";

struct ScriptedTmux {
    sessions: Vec<String>,
    tokens: std::collections::HashMap<String, String>,
    fields: std::collections::HashMap<(String, String), String>,
    calls: StdMutex<Vec<Vec<String>>>,
}

impl ScriptedTmux {
    fn new() -> Self {
        Self {
            sessions: Vec::new(),
            tokens: std::collections::HashMap::new(),
            fields: std::collections::HashMap::new(),
            calls: StdMutex::new(Vec::new()),
        }
    }

    fn with_session(mut self, name: &str, token: &str) -> Self {
        self.sessions.push(name.to_owned());
        self.tokens.insert(name.to_owned(), token.to_owned());
        self
    }

    fn with_field(mut self, session: &str, format: &str, value: &str) -> Self {
        self.fields
            .insert((session.to_owned(), format.to_owned()), value.to_owned());
        self
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

fn ok_tmux(stdout: impl Into<String>) -> boss_tmux::CommandOutput {
    boss_tmux::CommandOutput {
        success: true,
        code: Some(0),
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

#[async_trait]
impl boss_tmux::CommandRunner for ScriptedTmux {
    async fn run(
        &self,
        _program: &std::path::Path,
        args: &[std::ffi::OsString],
        _cwd: Option<&std::path::Path>,
    ) -> std::io::Result<boss_tmux::CommandOutput> {
        let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        self.calls.lock().unwrap().push(args.clone());
        match args.get(2).map(String::as_str) {
            Some("list-sessions") => {
                let stdout = self
                    .sessions
                    .iter()
                    .map(|name| format!("{name}\t\n"))
                    .collect::<String>();
                Ok(ok_tmux(stdout))
            }
            Some("show-environment") => {
                let session = &args[4];
                let var = args[5].as_str();
                match self.tokens.get(session) {
                    Some(value) if var == boss_tmux::TMUX_SPAWN_TOKEN_ENV => Ok(ok_tmux(format!("{var}={value}\n"))),
                    _ => Ok(boss_tmux::CommandOutput {
                        success: false,
                        code: Some(1),
                        stdout: String::new(),
                        stderr: format!("unknown variable: {var}"),
                    }),
                }
            }
            Some("display-message") => {
                let session = &args[5];
                let format = &args[6];
                match self.fields.get(&(session.clone(), format.clone())) {
                    Some(value) => Ok(ok_tmux(format!("{value}\n"))),
                    None => Ok(ok_tmux("\n")),
                }
            }
            other => panic!("unexpected tmux command in inspector test: {other:?} (full args={args:?})"),
        }
    }
}

fn stamp_tmux_run(db: &WorkDb, execution_id: &str, session: &str, token: &str) {
    db.start_execution_run(execution_id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    assert!(
        db.record_tmux_spawn_intent_for_execution(execution_id, "boss", session, token)
            .unwrap()
    );
    assert!(
        db.record_tmux_session_created_for_execution(execution_id, token, 4242)
            .unwrap()
    );
}

async fn inspect_with(
    db: WorkDb,
    execution_id: &str,
    server: ScriptedTmux,
) -> (Result<Option<TerminalLiveness>>, Vec<Vec<String>>) {
    let server = std::sync::Arc::new(server);
    let tmux = Tmux::with_runner_and_socket(
        "/opt/homebrew/bin/tmux",
        std::sync::Arc::clone(&server) as std::sync::Arc<dyn boss_tmux::CommandRunner>,
        boss_tmux::TEST_SOCKET_PATH,
    )
    .unwrap();
    let inspector = TmuxWorkerTerminalInspector::new(std::sync::Arc::new(db), tmux, None);
    let verdict = inspector.inspect(execution_id).await;
    (verdict, server.calls())
}

#[tokio::test]
async fn inspector_live_pane_reports_diagnostics_only() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_old_execution(&db, &work_item_id);
    stamp_tmux_run(&db, &execution_id, "boss-worker-1", "tok-1");

    let (verdict, calls) = inspect_with(
        db,
        &execution_id,
        ScriptedTmux::new()
            .with_session("boss-worker-1", "tok-1")
            .with_field("boss-worker-1", "#{pane_dead}", "0")
            .with_field("boss-worker-1", "#{window_activity}", "9000")
            .with_field("boss-worker-1", "#{pane_current_command}", CLASSIFIER_DRIVER_BINARY),
    )
    .await;

    // `window_activity_epoch_secs`/`pane_current_command` are carried
    // through as diagnostics only — the inspector never resolves the
    // run's driver or compares against it (that comparison used to gate
    // `agent_is_foreground`/`foreground_command_mismatch`, both removed).
    assert_eq!(
        verdict.unwrap(),
        Some(TerminalLiveness::Alive {
            session_name: "boss-worker-1".to_owned(),
            window_activity_epoch_secs: 9000,
            pane_current_command: Some(CLASSIFIER_DRIVER_BINARY.to_owned()),
        })
    );
    assert_eq!(
        calls,
        vec![
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "list-sessions".to_owned(),
                "-F".to_owned(),
                "#{session_name}\t#{@boss_spawn_token}".to_owned(),
            ],
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "show-environment".to_owned(),
                "-t".to_owned(),
                "boss-worker-1".to_owned(),
                "BOSS_SPAWN_TOKEN".to_owned(),
            ],
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "display-message".to_owned(),
                "-p".to_owned(),
                "-t".to_owned(),
                "boss-worker-1".to_owned(),
                "#{pane_dead}".to_owned(),
            ],
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "display-message".to_owned(),
                "-p".to_owned(),
                "-t".to_owned(),
                "boss-worker-1".to_owned(),
                "#{window_activity}".to_owned(),
            ],
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "display-message".to_owned(),
                "-p".to_owned(),
                "-t".to_owned(),
                "boss-worker-1".to_owned(),
                "#{pane_current_command}".to_owned(),
            ],
        ]
    );
}

#[tokio::test]
async fn inspector_missing_session_is_dead() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_old_execution(&db, &work_item_id);
    stamp_tmux_run(&db, &execution_id, "boss-worker-1", "tok-1");

    let (verdict, calls) = inspect_with(db, &execution_id, ScriptedTmux::new()).await;
    assert_eq!(
        verdict.unwrap(),
        Some(TerminalLiveness::Dead {
            session_name: "boss-worker-1".to_owned(),
            evidence: DeadPaneEvidence::SessionAbsent,
        })
    );
    assert_eq!(
        calls,
        vec![vec![
            "-S".to_owned(),
            boss_tmux::TEST_SOCKET_PATH.to_owned(),
            "list-sessions".to_owned(),
            "-F".to_owned(),
            "#{session_name}\t#{@boss_spawn_token}".to_owned(),
        ]]
    );
}

#[tokio::test]
async fn inspector_token_mismatch_is_dead() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_old_execution(&db, &work_item_id);
    stamp_tmux_run(&db, &execution_id, "boss-worker-1", "tok-1");

    let (verdict, calls) = inspect_with(
        db,
        &execution_id,
        ScriptedTmux::new().with_session("boss-worker-1", "other-token"),
    )
    .await;
    assert_eq!(
        verdict.unwrap(),
        Some(TerminalLiveness::Dead {
            session_name: "boss-worker-1".to_owned(),
            evidence: DeadPaneEvidence::SpawnTokenMismatch,
        })
    );
    assert_eq!(
        calls,
        vec![
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "list-sessions".to_owned(),
                "-F".to_owned(),
                "#{session_name}\t#{@boss_spawn_token}".to_owned(),
            ],
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "show-environment".to_owned(),
                "-t".to_owned(),
                "boss-worker-1".to_owned(),
                "BOSS_SPAWN_TOKEN".to_owned(),
            ],
        ]
    );
}

#[tokio::test]
async fn inspector_pane_dead_carries_status() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_old_execution(&db, &work_item_id);
    stamp_tmux_run(&db, &execution_id, "boss-worker-1", "tok-1");

    let (verdict, calls) = inspect_with(
        db,
        &execution_id,
        ScriptedTmux::new()
            .with_session("boss-worker-1", "tok-1")
            .with_field("boss-worker-1", "#{pane_dead}", "1")
            .with_field("boss-worker-1", "#{pane_dead_status}", "143"),
    )
    .await;
    assert_eq!(
        verdict.unwrap(),
        Some(TerminalLiveness::Dead {
            session_name: "boss-worker-1".to_owned(),
            evidence: DeadPaneEvidence::PaneExited {
                pane_dead_status: Some("143".to_owned()),
            },
        })
    );
    assert_eq!(
        calls,
        vec![
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "list-sessions".to_owned(),
                "-F".to_owned(),
                "#{session_name}\t#{@boss_spawn_token}".to_owned(),
            ],
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "show-environment".to_owned(),
                "-t".to_owned(),
                "boss-worker-1".to_owned(),
                "BOSS_SPAWN_TOKEN".to_owned(),
            ],
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "display-message".to_owned(),
                "-p".to_owned(),
                "-t".to_owned(),
                "boss-worker-1".to_owned(),
                "#{pane_dead}".to_owned(),
            ],
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "display-message".to_owned(),
                "-p".to_owned(),
                "-t".to_owned(),
                "boss-worker-1".to_owned(),
                "#{pane_dead_status}".to_owned(),
            ],
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "display-message".to_owned(),
                "-p".to_owned(),
                "-t".to_owned(),
                "boss-worker-1".to_owned(),
                "#{window_activity}".to_owned(),
            ],
        ]
    );
}

#[tokio::test]
async fn inspector_unparseable_window_activity_is_an_error() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_old_execution(&db, &work_item_id);
    stamp_tmux_run(&db, &execution_id, "boss-worker-1", "tok-1");

    let (verdict, calls) = inspect_with(
        db,
        &execution_id,
        ScriptedTmux::new()
            .with_session("boss-worker-1", "tok-1")
            .with_field("boss-worker-1", "#{pane_dead}", "0")
            .with_field("boss-worker-1", "#{window_activity}", "not-a-number"),
    )
    .await;
    assert!(
        verdict.is_err(),
        "unparseable window_activity must be Err, got {verdict:?}"
    );
    assert_eq!(
        calls,
        vec![
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "list-sessions".to_owned(),
                "-F".to_owned(),
                "#{session_name}\t#{@boss_spawn_token}".to_owned(),
            ],
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "show-environment".to_owned(),
                "-t".to_owned(),
                "boss-worker-1".to_owned(),
                "BOSS_SPAWN_TOKEN".to_owned(),
            ],
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "display-message".to_owned(),
                "-p".to_owned(),
                "-t".to_owned(),
                "boss-worker-1".to_owned(),
                "#{pane_dead}".to_owned(),
            ],
            vec![
                "-S".to_owned(),
                boss_tmux::TEST_SOCKET_PATH.to_owned(),
                "display-message".to_owned(),
                "-p".to_owned(),
                "-t".to_owned(),
                "boss-worker-1".to_owned(),
                "#{window_activity}".to_owned(),
            ],
        ]
    );
}

// A `#{pane_current_command}` that differs from the driver binary (e.g.
// Claude sourced through a login shell that hasn't exec'd it yet) is
// carried through as a plain diagnostic, not compared against anything
// or flagged — the removed veto this rewrite deletes.
#[tokio::test]
async fn inspector_reports_pane_current_command_as_diagnostic_only() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_old_execution(&db, &work_item_id);
    stamp_tmux_run(&db, &execution_id, "boss-worker-1", "tok-1");

    let (verdict, _) = inspect_with(
        db,
        &execution_id,
        ScriptedTmux::new()
            .with_session("boss-worker-1", "tok-1")
            .with_field("boss-worker-1", "#{pane_dead}", "0")
            .with_field("boss-worker-1", "#{window_activity}", "9000")
            .with_field("boss-worker-1", "#{pane_current_command}", "node"),
    )
    .await;
    assert_eq!(
        verdict.unwrap(),
        Some(TerminalLiveness::Alive {
            session_name: "boss-worker-1".to_owned(),
            window_activity_epoch_secs: 9000,
            pane_current_command: Some("node".to_owned()),
        })
    );
}

/// [`TmuxWorkerTerminalInspector::tmux_for_run`] routing: a run recorded
/// with the literal legacy label must be inspected against the `-L boss`
/// server, never the durable socket — the correctness gap a
/// legacy-adopted worker's liveness probe would otherwise hit.
#[tokio::test]
async fn inspector_routes_legacy_labeled_run_to_the_label_server() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let execution_id = create_old_execution(&db, &work_item_id);
    // Explicitly record the legacy label — mirrors what the boot drain
    // adopts a surviving `-L boss` session with.
    db.start_execution_run(&execution_id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution_id, boss_tmux::SERVER_LABEL, "boss-worker-1", "tok-1")
            .unwrap()
    );
    assert!(
        db.record_tmux_session_created_for_execution(&execution_id, "tok-1", 4242)
            .unwrap()
    );

    let legacy_server = std::sync::Arc::new(
        ScriptedTmux::new()
            .with_session("boss-worker-1", "tok-1")
            .with_field("boss-worker-1", "#{pane_dead}", "0")
            .with_field("boss-worker-1", "#{window_activity}", "9000")
            .with_field("boss-worker-1", "#{pane_current_command}", CLASSIFIER_DRIVER_BINARY),
    );
    let socket_server = std::sync::Arc::new(ScriptedTmux::new());
    let socket_tmux = Tmux::with_runner_and_socket(
        "/opt/homebrew/bin/tmux",
        std::sync::Arc::clone(&socket_server) as std::sync::Arc<dyn boss_tmux::CommandRunner>,
        boss_tmux::TEST_SOCKET_PATH,
    )
    .unwrap();
    let legacy_tmux = Tmux::for_legacy_label_server_with_runner(
        "/opt/homebrew/bin/tmux",
        std::sync::Arc::clone(&legacy_server) as std::sync::Arc<dyn boss_tmux::CommandRunner>,
    )
    .unwrap();
    let inspector = TmuxWorkerTerminalInspector::new(std::sync::Arc::new(db), socket_tmux, Some(legacy_tmux));

    let verdict = inspector.inspect(&execution_id).await.unwrap();
    assert_eq!(
        verdict,
        Some(TerminalLiveness::Alive {
            session_name: "boss-worker-1".to_owned(),
            window_activity_epoch_secs: 9000,
            pane_current_command: Some(CLASSIFIER_DRIVER_BINARY.to_owned()),
        }),
        "the run must be inspected against the legacy server, not the socket",
    );
    assert!(
        socket_server.calls().is_empty(),
        "a legacy-labeled run must never be probed against the durable socket",
    );
    assert!(
        legacy_server
            .calls()
            .iter()
            .any(|call| call[0] == "-L" && call[1] == boss_tmux::SERVER_LABEL),
        "expected at least one -L boss call, got {:?}",
        legacy_server.calls(),
    );

    let prefix = inspector.operator_prefix_for_run(&execution_id);
    assert_eq!(
        prefix,
        format!("tmux -L {}", boss_tmux::quote_for_shell(boss_tmux::SERVER_LABEL)),
        "the operator-facing prefix for a legacy-labeled run must also address -L boss",
    );
}
