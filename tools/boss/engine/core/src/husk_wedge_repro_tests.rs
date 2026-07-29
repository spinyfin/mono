//! End-to-end reproduction of the mass-transient-API wedge, and proof that
//! the fix clears it.
//!
//! The wedge, as observed live: a burst of HTTP 529 Overloaded errors stalled
//! eleven interactive/review workers at once. `transient_recovery` did the
//! right thing for each — orphan the execution, queue a resume that prefers
//! the same dirty workspace — but freed each slot by clearing *engine
//! bookkeeping only*, leaving the app hosting a live pane in every one. From
//! then on:
//!
//! 1. the pool advertised eleven free slots the app would reject;
//! 2. each resume dispatch hit `SpawnWorkerPane` → `SlotBusy` ("requested
//!    slot already hosts a pane (engine/app slot desync, not capacity)");
//! 3. the only thing that frees such a pane — [`crate::husk_pane_sweep`] —
//!    saw eleven confirmed husks in one pass, tripped its mass-retirement
//!    circuit breaker, and retired **nothing**;
//! 4. because the candidates stayed confirmed, the breaker re-tripped every
//!    60s indefinitely, and (before this change) said so only in the engine
//!    log and in pull-only dispatch events.
//!
//! Nothing in that loop was self-clearing. The documented recovery was a
//! human running `bossctl agents retire-pane <slot>` once per slot.
//!
//! These tests drive both halves against a [`FakeApp`] that models the one
//! app-side invariant that matters — a slot hosts at most one pane, and
//! spawning into an occupied slot is rejected `SlotBusy`:
//!
//! - [`bookkeeping_only_release_wedges_the_pool_and_escalates`] reproduces the
//!   wedge with a releaser that behaves as the pre-fix `release_slot` did, and
//!   asserts the wedge is self-sustaining, that redispatch is impossible, and
//!   that the breaker's refusal now reaches the operator.
//! - [`pane_teardown_returns_the_pool_to_health_with_no_operator_action`] runs
//!   the identical scenario through the shipped path and asserts the pool
//!   comes back on its own: no hosted panes, no husks, breaker never trips, no
//!   escalation, and every resume execution can be spawned.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use boss_protocol::{HostedPaneEntry, RequestExecutionInput, WorkItemBinding};
use tempfile::TempDir;

use super::*;
use crate::completion::{PaneReleaseOutcome, WorkerPaneReleaser};
use crate::coordinator::{ExecutionCoordinator, slot_id_from_worker_id, worker_id_for_slot};
use crate::dead_pid_sweep::PidStatus;
use crate::dispatch_events::RecordingDispatchEventSink;
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::transient_error::RecoveryPolicy;
use crate::transient_recovery::{NoopWorkerNudger, RecoveryContext};
use crate::work::{ATTENTION_KIND_HUSK_BREAKER_TRIPPED, ExecutionStatus, WorkDb};

/// Workers stalled simultaneously. Matches the observed incident (at least
/// eleven distinct slots across the interactive and review pools) and is well
/// clear of [`MAX_RETIREMENTS_PER_PASS`].
const STALLED_WORKERS: usize = 11;

/// Verbatim error text from the incident's dispatch events.
const OVERLOADED_529: &str =
    "API Error: 529 Overloaded. This is a server-side issue, usually temporary — try again in a moment.";

/// The app's rejection when asked to spawn into a slot it already hosts a
/// pane in — `EngineToAppError::SlotBusy` in the real protocol.
#[derive(Debug, PartialEq, Eq)]
struct SlotBusy {
    slot_id: u8,
    hosting_run_id: String,
}

/// Minimal model of the macOS app's pane hosting: which slot hosts which
/// run's pane. Deliberately knows nothing about the engine's pool or
/// live-worker registry — that separation is the whole subject of the bug.
#[derive(Default)]
struct FakeApp {
    hosted: StdMutex<HashMap<u8, String>>,
}

impl FakeApp {
    fn host(&self, slot_id: u8, run_id: &str) {
        self.hosted.lock().unwrap().insert(slot_id, run_id.to_owned());
    }

    /// What `SpawnWorkerPane` does: refuse if the slot already hosts a pane.
    /// This is the rejection the wedged resume dispatches kept hitting.
    fn spawn_pane(&self, slot_id: u8, run_id: &str) -> Result<(), SlotBusy> {
        let mut hosted = self.hosted.lock().unwrap();
        if let Some(hosting_run_id) = hosted.get(&slot_id) {
            return Err(SlotBusy {
                slot_id,
                hosting_run_id: hosting_run_id.clone(),
            });
        }
        hosted.insert(slot_id, run_id.to_owned());
        Ok(())
    }

    fn hosted_slots(&self) -> Vec<u8> {
        let mut slots: Vec<u8> = self.hosted.lock().unwrap().keys().copied().collect();
        slots.sort_unstable();
        slots
    }

    fn hosted_panes(&self) -> Vec<HostedPaneEntry> {
        let hosted = self.hosted.lock().unwrap();
        let mut panes: Vec<HostedPaneEntry> = hosted
            .iter()
            .map(|(slot_id, run_id)| HostedPaneEntry {
                slot_id: *slot_id,
                run_id: run_id.clone(),
                summary: None,
                task_title: Some("wedged chore".to_owned()),
            })
            .collect();
        panes.sort_by_key(|pane| pane.slot_id);
        panes
    }
}

/// [`HuskPaneSweepSource`] over a [`FakeApp`] plus the engine's live registry,
/// performing the same diff [`crate::app::ServerState::list_husk_panes`]
/// performs — including the liveness-corroboration second opinion for a
/// terminal entry, with the PID probe pinned `Alive` so the test never depends
/// on real process ids.
struct FakeAppSweepSource {
    app: Arc<FakeApp>,
    live_states: Arc<LiveWorkerStateRegistry>,
    now_epoch_secs: i64,
}

#[async_trait]
impl HuskPaneSweepSource for FakeAppSweepSource {
    async fn list_husk_candidates(&self) -> Option<Vec<HostedPaneEntry>> {
        let live_by_slot: HashMap<u8, boss_protocol::LiveWorkerState> = self
            .live_states
            .snapshot()
            .into_iter()
            .map(|state| (state.slot_id, state))
            .collect();
        Some(
            self.app
                .hosted_panes()
                .into_iter()
                .filter(|pane| match live_by_slot.get(&pane.slot_id) {
                    Some(state) if !state.activity.is_terminal() => false,
                    Some(state) if state.run_id == pane.run_id => {
                        live_process_evidence_with(state, &PidStatus::Alive, self.now_epoch_secs).is_none()
                    }
                    _ => true,
                })
                .collect(),
        )
    }

    async fn retire_husk(&self, slot_id: u8) {
        self.app.hosted.lock().unwrap().remove(&slot_id);
    }
}

/// The shipped teardown, modelled on `ServerState::release_worker_pane`: ask
/// the app to destroy the pane, then release the pool claim, then drop the
/// live-state entry. Pane first — that ordering is what keeps the pool from
/// advertising a slot the app will reject.
struct AppPaneReleaser {
    app: Arc<FakeApp>,
    live_states: Arc<LiveWorkerStateRegistry>,
    coordinator: Arc<ExecutionCoordinator>,
}

#[async_trait]
impl WorkerPaneReleaser for AppPaneReleaser {
    async fn release_pane(&self, run_id: &str) -> PaneReleaseOutcome {
        let Some(state) = self
            .live_states
            .snapshot()
            .into_iter()
            .find(|state| state.run_id == run_id)
        else {
            return PaneReleaseOutcome::NoLiveWorker;
        };
        self.app.hosted.lock().unwrap().remove(&state.slot_id);
        let worker_id = worker_id_for_slot(state.slot_id);
        self.coordinator.release_worker_and_kick(&worker_id, None).await;
        self.live_states.release_slot(state.slot_id);
        PaneReleaseOutcome::Reaped
    }
}

/// The PRE-FIX behaviour of `transient_recovery::release_slot`: clear the
/// engine's own bookkeeping for the slot and leave the app hosting the pane.
/// Reports `Reaped` because the old code path had no notion of a pane at all —
/// which is exactly how the desync went unnoticed.
struct BookkeepingOnlyReleaser {
    live_states: Arc<LiveWorkerStateRegistry>,
    coordinator: Arc<ExecutionCoordinator>,
}

#[async_trait]
impl WorkerPaneReleaser for BookkeepingOnlyReleaser {
    async fn release_pane(&self, run_id: &str) -> PaneReleaseOutcome {
        let Some(state) = self
            .live_states
            .snapshot()
            .into_iter()
            .find(|state| state.run_id == run_id)
        else {
            return PaneReleaseOutcome::NoLiveWorker;
        };
        self.live_states.release_slot(state.slot_id);
        let worker_id = worker_id_for_slot(state.slot_id);
        self.coordinator.release_worker_and_kick(&worker_id, None).await;
        PaneReleaseOutcome::Reaped
    }
}

/// One stalled worker: its work item, its (about to be orphaned) execution,
/// and the slot whose pane the app is hosting.
struct StalledWorker {
    work_item_id: String,
    execution_id: String,
    slot_id: u8,
}

/// Stand up `STALLED_WORKERS` workers that have each just been killed
/// mid-chore by a 529: a `running` execution past the recovery grace window, a
/// transcript whose last entry is the API error, a claimed pool slot, an
/// `Idle` live-state entry, and an app-hosted pane.
async fn stall_workers(
    dir: &TempDir,
    db: &Arc<WorkDb>,
    live_states: &Arc<LiveWorkerStateRegistry>,
    coordinator: &Arc<ExecutionCoordinator>,
    app: &FakeApp,
) -> Vec<StalledWorker> {
    let product_id = crate::test_support::create_product(db);
    let mut workers = Vec::new();
    for i in 0..STALLED_WORKERS {
        let work_item_id = crate::test_support::create_active_chore(db, &product_id, &format!("chore {i}"));
        let transcript = dir.path().join(format!("transcript-{i}.jsonl"));
        let mut file = std::fs::File::create(&transcript).unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"working"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","isApiErrorMessage":true,"message":{{"role":"assistant","content":[{{"type":"text","text":{error}}}]}}}}"#,
            error = serde_json::to_string(OVERLOADED_529).unwrap(),
        )
        .unwrap();
        drop(file);

        let workspace_id = format!("mono-agent-{i:03}");
        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .preferred_workspace_id(workspace_id.clone())
                    .build(),
            )
            .unwrap();
        db.start_execution_run(
            &execution.id,
            &format!("worker-{i}"),
            "repo-1",
            &format!("lease-{i}"),
            &workspace_id,
            &format!("/tmp/{workspace_id}"),
        )
        .unwrap();
        db.set_run_transcript_path_if_unset(&execution.id, &transcript.to_string_lossy())
            .unwrap();
        db.force_started_at_for_test(
            &execution.id,
            boss_engine_utils::epoch_time::now_epoch_secs().saturating_sub(600),
        )
        .unwrap();

        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&execution.id, None)
            .await
            .expect("pool must have a slot for every stalled worker");
        let slot_id = slot_id_from_worker_id(&worker_id).unwrap();
        live_states.register_spawn(
            slot_id,
            &execution.id,
            "claude-opus-4-7",
            10_000 + i as i32,
            Some(WorkItemBinding {
                work_item_id: work_item_id.clone(),
                work_item_name: format!("chore {i}"),
                execution_id: execution.id.clone(),
            }),
        );
        // The 529 ends the turn, so the events socket reports the worker as
        // Idle — "looks done" while actually wedged mid-chore.
        live_states.apply_event(
            slot_id,
            &boss_protocol::WorkerEvent::Stop {
                session_id: format!("s{i}"),
                stop_hook_active: false,
                stop_reason: boss_protocol::StopReason::Completed,
            },
        );
        app.host(slot_id, &execution.id);

        workers.push(StalledWorker {
            work_item_id,
            execution_id: execution.id,
            slot_id,
        });
    }
    workers
}

/// Run one transient-recovery pass with the given pane releaser.
async fn recovery_pass(
    db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    coordinator: &Arc<ExecutionCoordinator>,
    pane_releaser: &dyn WorkerPaneReleaser,
) -> crate::transient_recovery::TransientRecoveryOutcome {
    let sink = RecordingDispatchEventSink::new();
    let policy = RecoveryPolicy::default();
    let cx = RecoveryContext {
        work_db: db,
        live_states,
        coordinator: coordinator.clone(),
        dispatch_events: &sink,
        policy: &policy,
        nudger: &NoopWorkerNudger,
        pane_releaser,
    };
    // `NoopWorkerNudger` always fails, so every worker takes the
    // orphan+respawn path in a single pass — the same place the wedged run
    // arrives at on its second sweep after an unsuccessful nudge.
    crate::transient_recovery::run_one_pass(
        &cx,
        &mut HashSet::new(),
        boss_engine_utils::epoch_time::now_epoch_secs(),
    )
    .await
}

/// The resume executions the recovery pass queued, in creation order.
fn resume_executions(db: &WorkDb, workers: &[StalledWorker]) -> Vec<String> {
    workers
        .iter()
        .map(|worker| {
            let execs = db.list_executions(Some(&worker.work_item_id)).unwrap();
            let resume = execs
                .iter()
                .find(|e| e.id != worker.execution_id && e.status == ExecutionStatus::Ready)
                .expect("every orphaned execution must have queued a resume");
            assert!(
                resume.allow_dirty,
                "the resume must re-lease the dirty workspace rather than let cube wipe it",
            );
            resume.id.clone()
        })
        .collect()
}

/// Reproduces the wedge. Every assertion here describes observed production
/// behaviour before this change; none of it is hypothetical.
#[tokio::test]
async fn bookkeeping_only_release_wedges_the_pool_and_escalates() {
    let (dir, db) = crate::test_support::open_db();
    let db = Arc::new(db);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    let coordinator = crate::test_support::make_coordinator(db.clone(), STALLED_WORKERS);
    let app = Arc::new(FakeApp::default());
    let workers = stall_workers(&dir, &db, &live_states, &coordinator, &app).await;

    // ── the 529 burst is recovered, engine-side only ────────────────────────
    let releaser = BookkeepingOnlyReleaser {
        live_states: live_states.clone(),
        coordinator: coordinator.clone(),
    };
    let outcome = recovery_pass(&db, &live_states, &coordinator, &releaser).await;
    assert_eq!(outcome.resumed, STALLED_WORKERS);
    let resumes = resume_executions(&db, &workers);

    // The engine thinks every slot is free. The app disagrees.
    assert!(
        coordinator.worker_pool().claimed_execution_ids().await.is_empty(),
        "the pool believes every slot is free",
    );
    assert_eq!(
        app.hosted_slots().len(),
        STALLED_WORKERS,
        "but the app is still hosting a pane in every one of them",
    );

    // ── consequence 1: the resume executions cannot land ────────────────────
    for (worker, resume) in workers.iter().zip(&resumes) {
        let err = app
            .spawn_pane(worker.slot_id, resume)
            .expect_err("spawning into a slot the app still hosts must be rejected");
        assert_eq!(
            err,
            SlotBusy {
                slot_id: worker.slot_id,
                hosting_run_id: worker.execution_id.clone(),
            },
            "this is the observed `pane_spawned/error … engine/app slot desync` event",
        );
    }

    // ── consequence 2: the husk sweep refuses, pass after pass ──────────────
    let source = FakeAppSweepSource {
        app: app.clone(),
        live_states: live_states.clone(),
        now_epoch_secs: boss_engine_utils::epoch_time::now_epoch_secs(),
    };
    let sink = RecordingDispatchEventSink::new();
    let health = HuskRetirementBreakerHealth::new();
    let mut seen = HashSet::new();
    let mut passes = Vec::new();
    for _ in 0..5 {
        let cx = HuskSweepContext {
            source: &source,
            dispatch_events: &sink,
            work_db: &db,
            breaker_health: &health,
        };
        passes.push(run_one_pass(&cx, &mut seen).await);
    }

    assert_eq!(passes[0].pending_confirmation, STALLED_WORKERS, "first pass observes");
    for pass in &passes[1..] {
        assert_eq!(
            pass.breaker_tripped,
            Some(STALLED_WORKERS),
            "the breaker re-trips on every subsequent pass — the wedge is self-sustaining",
        );
        assert_eq!(pass.retired, 0, "and it correctly retires nothing");
    }
    assert_eq!(
        app.hosted_slots().len(),
        STALLED_WORKERS,
        "no pane is ever freed, so the pool stays wedged indefinitely",
    );

    // ── the fix for defect 1: the refusal is now loud ───────────────────────
    let status = health.snapshot();
    assert!(status.tripped, "engine-health banner must report the wedged pool");
    assert_eq!(status.confirmed, STALLED_WORKERS);
    assert_eq!(
        status.slots,
        workers.iter().map(|w| w.slot_id).collect::<Vec<_>>(),
        "the banner names every wedged slot",
    );
    for worker in &workers {
        let open = db
            .list_attention_items_for_work_item(&worker.work_item_id)
            .unwrap()
            .into_iter()
            .filter(|item| item.kind == ATTENTION_KIND_HUSK_BREAKER_TRIPPED && item.status == "open")
            .count();
        assert_eq!(
            open, 1,
            "every wedged work item carries exactly one open attention item"
        );
    }

    println!(
        "WEDGE REPRODUCED: {STALLED_WORKERS} workers stalled on 529 → {STALLED_WORKERS} panes left hosted → \
         {STALLED_WORKERS} SlotBusy rejections → breaker tripped on {} consecutive passes, 0 retired. \
         Escalation raised: health banner (slots {:?}) + {} attention items.",
        passes.len() - 1,
        status.slots,
        workers.len(),
    );
}

/// The shipped path on the identical scenario: the pool returns to health by
/// itself, with no `bossctl agents retire-pane` and nothing for the breaker to
/// decline.
#[tokio::test]
async fn pane_teardown_returns_the_pool_to_health_with_no_operator_action() {
    let (dir, db) = crate::test_support::open_db();
    let db = Arc::new(db);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    let coordinator = crate::test_support::make_coordinator(db.clone(), STALLED_WORKERS);
    let app = Arc::new(FakeApp::default());
    let workers = stall_workers(&dir, &db, &live_states, &coordinator, &app).await;
    assert_eq!(app.hosted_slots().len(), STALLED_WORKERS, "all slots start occupied");

    let releaser = AppPaneReleaser {
        app: app.clone(),
        live_states: live_states.clone(),
        coordinator: coordinator.clone(),
    };
    let outcome = recovery_pass(&db, &live_states, &coordinator, &releaser).await;
    assert_eq!(outcome.resumed, STALLED_WORKERS);
    let resumes = resume_executions(&db, &workers);

    // Both sides agree the slots are free, and the wedged `claude` processes
    // are gone rather than left running in a workspace about to be re-leased.
    assert!(
        app.hosted_slots().is_empty(),
        "every pane was torn down as part of freeing its slot",
    );
    assert!(coordinator.worker_pool().claimed_execution_ids().await.is_empty());
    assert!(live_states.snapshot().is_empty());
    for worker in &workers {
        assert_eq!(
            db.get_execution(&worker.execution_id).unwrap().status,
            ExecutionStatus::Orphaned,
        );
    }

    // The resume executions can actually be dispatched — no SlotBusy.
    for (worker, resume) in workers.iter().zip(&resumes) {
        app.spawn_pane(worker.slot_id, resume)
            .expect("a genuinely free slot accepts the resume execution's pane");
    }
    assert_eq!(
        app.hosted_slots().len(),
        STALLED_WORKERS,
        "the pool is fully back in service with the resumed work",
    );

    // Nothing for the husk sweep to see, so the breaker never trips and no
    // escalation is raised. Note the resumed panes ARE hosted here and are
    // correctly not husks: each has a live, non-terminal engine entry.
    for (worker, resume) in workers.iter().zip(&resumes) {
        live_states.register_spawn(worker.slot_id, resume, "claude-opus-4-7", 20_000, None);
    }
    let source = FakeAppSweepSource {
        app: app.clone(),
        live_states: live_states.clone(),
        now_epoch_secs: boss_engine_utils::epoch_time::now_epoch_secs(),
    };
    let sink = RecordingDispatchEventSink::new();
    let health = HuskRetirementBreakerHealth::new();
    let mut seen = HashSet::new();
    for _ in 0..3 {
        let cx = HuskSweepContext {
            source: &source,
            dispatch_events: &sink,
            work_db: &db,
            breaker_health: &health,
        };
        let pass = run_one_pass(&cx, &mut seen).await;
        assert_eq!(pass.breaker_tripped, None, "no burst, so no breaker trip");
        assert_eq!(pass.pending_confirmation, 0, "no husk candidates at all");
        assert_eq!(pass.retired, 0);
    }
    assert!(!health.snapshot().tripped, "no operator escalation is raised");
    assert!(sink.events().await.is_empty());
    for worker in &workers {
        assert!(
            db.list_attention_items_for_work_item(&worker.work_item_id)
                .unwrap()
                .iter()
                .all(|item| item.kind != ATTENTION_KIND_HUSK_BREAKER_TRIPPED),
            "no circuit-breaker attention item is filed",
        );
    }

    println!(
        "RECOVERED WITHOUT OPERATOR ACTION: {STALLED_WORKERS} workers stalled on 529 → \
         {STALLED_WORKERS} panes torn down with their slots → 0 husks → breaker never tripped → \
         all {STALLED_WORKERS} resume executions spawned successfully. No retire-pane needed.",
    );
}
