use super::*;
use crate::spawn_flow::WorkerSpawner;

// Tests for `ServerState::retire_pane` / `ServerState::list_hosted_pane_statuses` —
// the break-glass leftover-viewer path. Occupancy is resolved from live-state,
// the worker registry, and `work_runs.agent_id`; `ListHostedPanes` only
// describes which slots still have a Ghostty viewer. `list_hosted_pane_statuses`
// powers `bossctl agents list --all`; `retire_pane` is the operator verb
// that acts on a slot.

#[tokio::test]
async fn retire_pane_refuses_when_live_run_tracked_in_slot() {
    // Safety check: a slot the engine's own LiveWorkerStateRegistry
    // still considers live (non-terminal) is NOT a husk. Retiring it
    // would tear down a pane the engine thinks is doing work — the
    // caller must go through `agents stop` instead.
    let (server_state, _dir) = test_server_state();
    server_state
        .live_worker_states
        .register_spawn(3, "run-live", "claude-opus-4-7", 0, None);

    let result = server_state.retire_pane(3).await;
    match result {
        Err(RetirePaneError::LiveRunTracked { slot_id, run_id }) => {
            assert_eq!(slot_id, 3);
            assert_eq!(run_id, "run-live");
        }
        other => panic!("expected LiveRunTracked, got {other:?}"),
    }

    // The refusal must not have touched the live-state entry.
    assert!(
        server_state.live_worker_states.get(3).is_some(),
        "a refused retire must leave the live-tracked slot untouched"
    );
}

#[test]
fn retire_pane_error_message_points_at_agents_stop() {
    // The whole point of the safety check is to redirect the operator
    // to the right verb — pin the message text so a future refactor
    // can't silently drop the pointer.
    let err = RetirePaneError::LiveRunTracked {
        slot_id: 3,
        run_id: "run-live".to_owned(),
    };
    let message = err.to_string();
    assert!(
        message.contains("agents stop"),
        "message should point at `agents stop`: {message}"
    );
    assert!(
        message.contains("run-live"),
        "message should name the tracked run: {message}"
    );
}

#[tokio::test]
async fn retire_pane_succeeds_for_husk_slot_with_no_app_session() {
    // No app session registered (headless/test engine): retire_pane
    // must still succeed — there's nothing to round-trip to, and the
    // engine-side cleanup (which is what this call chiefly guarantees
    // for a genuine husk) is unconditional.
    let (server_state, _dir) = test_server_state();

    let result = server_state.retire_pane(4).await;
    assert!(result.is_ok(), "expected Ok, got {result:?}");
}

#[tokio::test]
async fn retire_pane_sends_slot_keyed_detach_request_with_no_run_id_resolution() {
    // The defining property of retire_pane vs release_worker_pane: it
    // never resolves through worker_registry (there is no run id for
    // a husk) — it goes straight to the app with the slot id the
    // caller supplied.
    let (server_state, _dir) = test_server_state();
    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let retire = tokio::spawn(async move { server_clone.retire_pane(7).await });

    // Occupancy is resolved from durable identity, not ListHostedPanes.
    // Slot 7 has none, so Guard 3 is inert and retirement proceeds as a husk.
    let envelope = sink.next().await.expect("an EngineRequest event should be enqueued");
    let (request_id, request) = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, request } => (request_id, request),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    match request {
        EngineToAppRequest::DetachWorkerPane(input) => {
            assert_eq!(input.slot_id, 7);
        }
        other => panic!("expected DetachWorkerPane, got {other:?}"),
    }

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::DetachWorkerPane {
                result: Ok(crate::protocol::DetachWorkerPaneResult {}),
            },
        )
        .await;

    let result = retire.await.expect("retire task");
    assert!(result.is_ok(), "expected Ok, got {result:?}");
}

#[tokio::test]
async fn list_hosted_pane_statuses_returns_empty_when_no_app_session_registered() {
    // Best-effort query: with no app session there is nothing to
    // diff, so this must not be a hard error.
    let (server_state, _dir) = test_server_state();
    let panes = server_state.list_hosted_pane_statuses().await.expect("expected Ok");
    assert!(panes.is_empty());
}

#[tokio::test]
async fn list_hosted_pane_statuses_filters_out_slots_the_engine_still_tracks_live() {
    // The app reports two hosted slots: one the engine still has a
    // live (non-terminal) run for — not a husk, must be filtered —
    // and one the engine has no live entry for at all — a genuine
    // husk, must be reported.
    let (server_state, _dir) = test_server_state();
    server_state
        .live_worker_states
        .register_spawn(2, "run-live", "claude-opus-4-7", 0, None);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let list = tokio::spawn(async move { server_clone.list_hosted_pane_statuses().await });

    let envelope = sink.next().await.expect("an EngineRequest event should be enqueued");
    let request_id = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, request } => {
            assert!(
                matches!(request, EngineToAppRequest::ListHostedPanes(_)),
                "expected ListHostedPanes, got {request:?}"
            );
            request_id
        }
        other => panic!("expected EngineRequest, got {other:?}"),
    };

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::ListHostedPanes {
                result: Ok(crate::protocol::ListHostedPanesResult {
                    panes: vec![
                        crate::protocol::HostedPaneEntry {
                            slot_id: 2,
                            run_id: "run-live".to_owned(),
                            summary: None,
                            task_title: None,
                        },
                        crate::protocol::HostedPaneEntry {
                            slot_id: 6,
                            run_id: "run-husk".to_owned(),
                            summary: Some("fixing the fencer scraper".to_owned()),
                            task_title: None,
                        },
                    ],
                }),
            },
        )
        .await;

    let panes = husk_subset(list.await.expect("list task").expect("expected Ok"));
    assert_eq!(
        panes.len(),
        1,
        "only the non-live slot should be reported as a husk: {panes:?}"
    );
    assert_eq!(panes[0].slot_id, 6);
    assert_eq!(panes[0].run_id, "run-husk");
}

// ─── 2026-07-26 regression: terminal bookkeeping is not proof of death ──────
//
// Six live workers received a synchronized `SessionEnd { reason: "other" }`
// burst inside 250ms while their `claude` processes kept running. That flipped
// each live-state entry to `Terminated`; `list_hosted_pane_statuses`
// classified those terminal entries as husks, and `retire_pane` re-read the
// same wrong bookkeeping and agreed. Five workers were SIGTERMed mid-work,
// three of them inside a foreground `bazel` build.
//
// Both the classifier and the retire guard now take a second opinion from the
// OS and the worker's own hook stream before acting.

/// Drive `slot_id` into the exact state the victims were in: a live worker
/// with an unbalanced `PreToolUse` (a long foreground build) that then
/// received a spurious `SessionEnd`. `shell_pid` is this test process, so
/// `kill(pid, 0)` genuinely reports it alive.
fn drive_spurious_session_end_mid_tool(server_state: &ServerState, slot_id: u8, run_id: &str) {
    server_state
        .live_worker_states
        .register_spawn(slot_id, run_id, "claude-opus-4-7", std::process::id() as i32, None);
    server_state.live_worker_states.apply_event(
        slot_id,
        &crate::protocol::WorkerEvent::PreToolUse {
            session_id: "s".to_owned(),
            tool_name: "Bash".to_owned(),
            tool_input: serde_json::Value::Null,
        },
    );
    server_state.live_worker_states.apply_event(
        slot_id,
        &crate::protocol::WorkerEvent::SessionEnd {
            session_id: "s".to_owned(),
            reason: "other".to_owned(),
        },
    );
}

#[tokio::test]
async fn retire_pane_refuses_when_a_terminal_entry_still_has_a_live_worker_process() {
    let (server_state, _dir) = test_server_state();
    drive_spurious_session_end_mid_tool(&server_state, 3, "run-victim");

    // Precondition: the engine's bookkeeping really does say "terminated" —
    // the old `LiveRunTracked` guard would have waved this straight through.
    let state = server_state.live_worker_states.get(3).expect("entry");
    assert!(
        state.activity.is_terminal(),
        "precondition: bookkeeping must say terminal"
    );

    match server_state.retire_pane(3).await {
        Err(RetirePaneError::LiveProcessCorroborated {
            slot_id,
            run_id,
            evidence,
        }) => {
            assert_eq!(slot_id, 3);
            assert_eq!(run_id, "run-victim");
            assert!(
                evidence.contains("Bash"),
                "evidence should name the in-flight tool: {evidence}"
            );
        }
        other => panic!("expected LiveProcessCorroborated, got {other:?}"),
    }

    // A refused retire must leave the slot completely untouched.
    assert!(
        server_state.live_worker_states.get(3).is_some(),
        "a refused retire must not clear the live-state entry"
    );
}

#[tokio::test]
async fn retire_pane_still_retires_a_terminal_slot_with_no_live_process() {
    // The sweep's reason for existing must survive: a terminal entry with no
    // shell pid to corroborate (the classic husk left by a release RPC that
    // never landed) is still reclaimed.
    let (server_state, _dir) = test_server_state();
    server_state
        .live_worker_states
        .register_spawn(4, "run-husk", "claude-opus-4-7", 0, None);
    server_state.live_worker_states.apply_event(
        4,
        &crate::protocol::WorkerEvent::SessionEnd {
            session_id: "s".to_owned(),
            reason: "exit".to_owned(),
        },
    );

    let result = server_state.retire_pane(4).await;
    assert!(result.is_ok(), "a genuine husk must still be retired: {result:?}");
    assert!(
        server_state.live_worker_states.get(4).is_none(),
        "retiring a genuine husk clears its slot"
    );
}

#[tokio::test]
async fn list_hosted_pane_statuses_does_not_flag_a_terminal_slot_whose_worker_is_alive() {
    // The classifier half. Slot 2 is the incident shape (terminal entry, live
    // process); slot 6 is a genuine husk the engine has no entry for at all.
    // Only slot 6 may be reported as a husk — a live worker must never even
    // be flagged, because `agents list --all` and any future caller inherit
    // this classification.
    let (server_state, _dir) = test_server_state();
    drive_spurious_session_end_mid_tool(&server_state, 2, "run-victim");

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let list = tokio::spawn(async move { server_clone.list_hosted_pane_statuses().await });

    let envelope = sink.next().await.expect("an EngineRequest event should be enqueued");
    let request_id = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id,
        other => panic!("expected EngineRequest, got {other:?}"),
    };

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::ListHostedPanes {
                result: Ok(crate::protocol::ListHostedPanesResult {
                    panes: vec![
                        crate::protocol::HostedPaneEntry {
                            slot_id: 2,
                            run_id: "run-victim".to_owned(),
                            summary: None,
                            task_title: None,
                        },
                        crate::protocol::HostedPaneEntry {
                            slot_id: 6,
                            run_id: "run-husk".to_owned(),
                            summary: None,
                            task_title: None,
                        },
                    ],
                }),
            },
        )
        .await;

    let panes = husk_subset(list.await.expect("list task").expect("expected Ok"));
    assert_eq!(
        panes.iter().map(|pane| pane.slot_id).collect::<Vec<_>>(),
        vec![6],
        "a terminal slot with a live worker process must not be classified as a husk: {panes:?}"
    );
}

#[tokio::test]
async fn list_hosted_pane_statuses_classifies_occupancy_not_the_viewers_claimed_run() {
    // Occupancy comes from live-state / durable identity, not the viewer's
    // claimed run_id. Slot 5 is occupied by `run-new` (terminal bookkeeping,
    // live process) even if the Ghostty viewer still labels itself `run-old`.
    let (server_state, _dir) = test_server_state();
    drive_spurious_session_end_mid_tool(&server_state, 5, "run-new");

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let list = tokio::spawn(async move { server_clone.list_hosted_pane_statuses().await });

    let envelope = sink.next().await.expect("an EngineRequest event should be enqueued");
    let request_id = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id,
        other => panic!("expected EngineRequest, got {other:?}"),
    };

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::ListHostedPanes {
                result: Ok(crate::protocol::ListHostedPanesResult {
                    panes: vec![crate::protocol::HostedPaneEntry {
                        slot_id: 5,
                        run_id: "run-old".to_owned(),
                        summary: None,
                        task_title: None,
                    }],
                }),
            },
        )
        .await;

    let statuses = list.await.expect("list task").expect("expected Ok");
    assert!(
        husk_subset(statuses.clone()).is_empty(),
        "durable occupancy is run-new with a live process, so the slot is not a husk: {statuses:?}"
    );
    assert_eq!(statuses[0].run_id, "run-new");
    assert!(matches!(
        statuses[0].state,
        crate::protocol::HostedPaneState::LiveProcessNoRegistry { .. }
    ));
}

// ─── 2026-07-28 regression: no live-state entry is not proof of death either ──
//
// The 2026-07-26 fix taught the classifier to distrust a TERMINAL live-state
// entry. It could not help when there is no entry at all — which is the state
// every wrongly-terminalized worker ends up in, because `release_worker_pane`
// drops the entry unconditionally on its way out. Those six workers were alive,
// untracked, and (had the mass-retirement breaker not declined) one sweep pass
// away from being SIGTERMed.
//
// The classifier now falls back to durable state — `work_runs.shell_pid` plus
// the execution's status — for exactly the slots its in-memory corroboration
// cannot reach.

fn husk_subset(statuses: Vec<crate::protocol::HostedPaneStatus>) -> Vec<crate::protocol::HostedPaneStatus> {
    statuses
        .into_iter()
        .filter(|status| matches!(status.state, crate::protocol::HostedPaneState::Husk))
        .collect()
}

/// Drive the app's `ListHostedPanes` round-trip for `panes` and return the
/// classifier's husk subset. Factors out the request/response dance the
/// hosted-pane classification tests above all repeat.
async fn husk_panes_for(
    server_state: &Arc<ServerState>,
    sink: &Arc<SessionSink>,
    panes: Vec<crate::protocol::HostedPaneEntry>,
) -> Vec<crate::protocol::HostedPaneStatus> {
    husk_subset(all_pane_statuses_for(server_state, sink, panes).await)
}

/// Same round-trip as [`husk_panes_for`], returning every classified pane.
async fn all_pane_statuses_for(
    server_state: &Arc<ServerState>,
    sink: &Arc<SessionSink>,
    panes: Vec<crate::protocol::HostedPaneEntry>,
) -> Vec<crate::protocol::HostedPaneStatus> {
    let server_clone = server_state.clone();
    let list = tokio::spawn(async move { server_clone.list_hosted_pane_statuses().await });

    let envelope = sink.next().await.expect("an EngineRequest event should be enqueued");
    let request_id = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id,
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::ListHostedPanes {
                result: Ok(crate::protocol::ListHostedPanesResult { panes }),
            },
        )
        .await;
    list.await.expect("list task").expect("expected Ok")
}

fn hosted(slot_id: u8, run_id: &str) -> crate::protocol::HostedPaneEntry {
    crate::protocol::HostedPaneEntry {
        slot_id,
        run_id: run_id.to_owned(),
        summary: None,
        task_title: None,
    }
}

#[tokio::test]
async fn list_hosted_pane_statuses_spares_an_untracked_slot_whose_durable_process_is_alive() {
    use crate::test_support::*;

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    // Our own pid: `kill(pid, 0)` genuinely reports it alive.
    let execution_id = create_spawned_execution(db, &work_item_id, i64::from(std::process::id()));
    db.mark_execution_orphaned(&execution_id, "spawn-ack timeout; presumed dead")
        .unwrap();

    // The engine tracks NOTHING for this slot — the terminal path cleared it.
    assert!(server_state.live_worker_states.get(4).is_none());

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    // Slot 1 matches `create_spawned_execution`'s durable `worker-1`.
    let panes = husk_panes_for(&server_state, &sink, vec![hosted(1, &execution_id)]).await;
    assert!(
        panes.is_empty(),
        "a slot the engine forgot, whose execution was orphaned by INFERENCE and whose recorded \
         process is alive, is a re-adoption candidate — not a husk to SIGTERM: {panes:?}"
    );
}

#[tokio::test]
async fn list_hosted_pane_statuses_still_retires_an_untracked_slot_whose_process_is_gone() {
    use crate::test_support::*;

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    // A pid that cannot exist: `kill(pid, 0)` returns ESRCH.
    let execution_id = create_spawned_execution(db, &work_item_id, 4_194_303);
    db.mark_execution_orphaned(&execution_id, "worker died").unwrap();

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let panes = husk_panes_for(&server_state, &sink, vec![hosted(1, &execution_id)]).await;
    assert_eq!(
        panes.iter().map(|pane| pane.slot_id).collect::<Vec<_>>(),
        vec![1],
        "the guard must not disable the sweep: a dead process is still a husk",
    );
}

#[tokio::test]
async fn list_hosted_pane_statuses_still_retires_a_lingering_shell_under_a_cancelled_run() {
    use crate::test_support::*;

    // The shape the durable guard must NOT protect, and the reason it keys on
    // the terminal status as well as the pid: a genuine husk keeps its shell
    // alive after `claude` exits inside it. Its execution was cancelled — a
    // decided outcome, not an inference — so the pane is stray and must be
    // reclaimed.
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    let execution_id = create_spawned_execution(db, &work_item_id, i64::from(std::process::id()));
    db.cancel_execution(&execution_id).unwrap();

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let panes = husk_panes_for(&server_state, &sink, vec![hosted(1, &execution_id)]).await;
    assert_eq!(
        panes.iter().map(|pane| pane.slot_id).collect::<Vec<_>>(),
        vec![1],
        "a lingering shell under a DECIDED terminal status is a husk even though its pid is alive",
    );
}

/// The break-glass verb must inherit the same durable guard the classifier
/// got — but where the classifier's evidence corroborates a still-running
/// process for an execution the engine tracks nothing about, `retire_pane`
/// no longer dead-ends in a refusal that points the operator at a second
/// command. This is precisely the shape `bossctl agents stop` already
/// reaps via durable state (`release_worker_pane`'s durable fallback), so
/// `retire_pane` now performs that same teardown and completes the
/// retirement — the verb the operator reached for handles the case
/// instead of a two-verb trial-and-error dance (2026-08-01).
#[tokio::test]
async fn retire_pane_reaps_an_untracked_slot_whose_durable_process_is_alive() {
    use crate::test_support::*;

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");

    // A REAL child process in its own process group — never the test
    // process's own pid — so the teardown this test exercises can
    // actually signal it without touching the test runner itself.
    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    let execution_id = create_spawned_execution(db, &work_item_id, i64::from(pid));
    super::tmux_stub::install_teardown(&server_state, &execution_id, i64::from(child.id()));
    corroborate_slot_tmux_adopted(&server_state, &execution_id, 1);
    db.mark_execution_orphaned(&execution_id, "presumed dead").unwrap();

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    // Slot 1: `create_spawned_execution`'s durable run row always records
    // worker id `worker-1`, and occupancy is resolved from that id — no
    // ListHostedPanes round-trip.
    let retire = tokio::spawn(async move { server_clone.retire_pane(1).await });

    // The durable teardown proceeds because occupancy is worker-1 and the
    // worker pool confirms nothing else claims that slot. Then the
    // slot-keyed viewer detach.
    let release = sink
        .next()
        .await
        .expect("a DetachWorkerPane request should be enqueued");
    match release.payload {
        FrontendEvent::EngineRequest { request_id, request } => {
            assert!(
                matches!(
                    request,
                    EngineToAppRequest::DetachWorkerPane(crate::protocol::DetachWorkerPaneInput { slot_id: 1, .. })
                ),
                "expected DetachWorkerPane for slot 1, got {request:?}"
            );
            server_state
                .deliver_app_response(
                    "session-app",
                    &request_id,
                    EngineToAppResponse::DetachWorkerPane {
                        result: Ok(crate::protocol::DetachWorkerPaneResult {}),
                    },
                )
                .await;
        }
        other => panic!("expected EngineRequest, got {other:?}"),
    }

    let result = retire.await.expect("retire task");
    assert!(result.is_ok(), "expected retirement to succeed, got {result:?}");

    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join wait task")
        .expect("wait on child");
    assert!(
        !status.success(),
        "the untracked worker's process tree must actually go down",
    );
}

/// Regression for a slot handed to a NEWER run between when execution A's
/// pool claim leaked and when A's own untracked teardown finally reaches
/// `detach_untracked_worker_viewer`. `hosted_pane_slot_for_run` derives the
/// slot from A's durable `work_runs.agent_id` — the slot A was once given,
/// not necessarily the slot it holds now — so if that slot (`worker-1`) has
/// since been reclaimed by a live execution B, tearing it down unconditionally
/// would detach B's viewer, free B's pool claim and drop B's live-state entry
/// out from under it, even though B is still running. The fix must refuse to
/// touch the slot when the worker pool disagrees that A still owns it.
#[tokio::test]
async fn retire_pane_does_not_clobber_a_slot_reclaimed_by_a_newer_run() {
    use crate::test_support::*;

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");

    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    // `create_spawned_execution` always records `worker-1` as the durable
    // worker id, so this run's derived slot is slot 1 regardless of which
    // slot actually hosts it today.
    let execution_id = create_spawned_execution(db, &work_item_id, i64::from(pid));
    super::tmux_stub::install_teardown(&server_state, &execution_id, i64::from(child.id()));
    corroborate_slot_tmux_adopted(&server_state, &execution_id, 1);
    db.mark_execution_orphaned(&execution_id, "presumed dead").unwrap();

    // Slot `worker-1` has since been claimed by a DIFFERENT, live execution —
    // the scenario the derived slot id cannot see, because it only reads the
    // retiring run's own historical record.
    let other_execution_id = "run-newer-occupant";
    assert!(
        server_state
            .execution_coordinator
            .reclaim_slot("worker-1", other_execution_id)
            .await,
        "the newer run must be able to claim the slot the retiring run once held",
    );

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let retire = tokio::spawn(async move { server_clone.retire_pane(1).await });

    // Occupancy is resolved from durable identity (worker-1), not the app.
    // No `DetachWorkerPane` must follow: the derived slot is owned by a
    // different live execution, so `detach_untracked_worker_viewer` must
    // refuse to act on it.
    let no_further_request = tokio::time::timeout(std::time::Duration::from_millis(200), sink.next()).await;
    assert!(
        no_further_request.is_err(),
        "expected no further app request, but got {no_further_request:?}",
    );

    let result = retire.await.expect("retire task");
    assert!(result.is_ok(), "expected retirement to succeed, got {result:?}");

    // The newer run's pool claim must survive untouched.
    let claims = server_state.execution_coordinator.worker_pool().claims().await;
    assert!(
        claims
            .iter()
            .any(|claim| claim.worker_id == "worker-1" && claim.execution_id == other_execution_id),
        "expected the newer run's pool claim to survive, got {claims:?}",
    );

    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join wait task")
        .expect("wait on child");
    assert!(
        !status.success(),
        "the retiring run's own untracked process tree must still go down",
    );
}

/// The same clobbering shape on a `review-N` worker id. Occupancy of
/// review slots is resolved through `slot_id_from_worker_id`, but the
/// ownership guard must look at the review pool — the main pool has no
/// `review-1` claim, so a main-pool-only check would always skip the
/// guard and DetachWorkerPane / stop_slot a newer reviewer.
#[tokio::test]
async fn retire_pane_does_not_clobber_a_review_slot_reclaimed_by_a_newer_run() {
    use crate::test_support::*;

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");

    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    let execution_id = create_old_execution(db, &work_item_id);
    let (_exec, run) = db
        .start_execution_run(&execution_id, "review-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    assert!(
        db.set_run_shell_pid_for_execution(&execution_id, i64::from(pid))
            .unwrap(),
        "the run row must exist before a shell pid can be recorded against it",
    );
    finish_run_worker_pane_alive(db, &execution_id, &run.id, Some("Spawned worker pane on review-1."));
    super::tmux_stub::install_teardown(&server_state, &execution_id, i64::from(child.id()));
    let slot_id = crate::coordinator::slot_id_from_worker_id("review-1").expect("review-1 maps to a slot");
    corroborate_slot_tmux_adopted(&server_state, &execution_id, slot_id);
    db.mark_execution_orphaned(&execution_id, "presumed dead").unwrap();

    let other_execution_id = "run-newer-reviewer";
    assert!(
        server_state
            .execution_coordinator
            .reclaim_slot("review-1", other_execution_id)
            .await,
        "the newer reviewer must be able to claim review-1",
    );

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let retire = tokio::spawn(async move { server_clone.retire_pane(slot_id).await });

    let no_further_request = tokio::time::timeout(std::time::Duration::from_millis(200), sink.next()).await;
    assert!(
        no_further_request.is_err(),
        "expected no DetachWorkerPane for a review slot claimed by a newer run, got {no_further_request:?}",
    );

    let result = retire.await.expect("retire task");
    assert!(result.is_ok(), "expected retirement to succeed, got {result:?}");

    let holder = server_state.execution_coordinator.claim_holder("review-1").await;
    assert_eq!(
        holder.as_deref(),
        Some(other_execution_id),
        "expected the newer reviewer's pool claim to survive, got {holder:?}"
    );

    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join wait task")
        .expect("wait on child");
    assert!(
        !status.success(),
        "the retiring run's own untracked process tree must still go down",
    );
}

/// `ListHostedPanes` describes the viewer. Occupancy of slot 4 is not
/// the viewer's claimed run_id: that run lives on worker-1 (slot 1).
#[tokio::test]
async fn list_hosted_pane_statuses_does_not_treat_the_viewers_run_id_as_occupancy() {
    use crate::test_support::*;

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    let execution_id = create_spawned_execution(db, &work_item_id, i64::from(std::process::id()));
    db.mark_execution_orphaned(&execution_id, "presumed dead").unwrap();

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let panes = husk_panes_for(&server_state, &sink, vec![hosted(4, &execution_id)]).await;
    assert_eq!(
        panes.iter().map(|pane| pane.slot_id).collect::<Vec<_>>(),
        vec![4],
        "a viewer in a slot with no durable occupancy is a husk even if it claims a live orphaned run: {panes:?}"
    );
}

/// `detach_untracked_worker_viewer` must skip slot-scoped effects when
/// live-state names a different run, even if the pool slot is free.
#[tokio::test]
async fn detach_untracked_viewer_does_not_clobber_a_slot_held_only_in_live_state() {
    use crate::test_support::*;

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");

    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    let execution_id = create_spawned_execution(db, &work_item_id, i64::from(pid));
    super::tmux_stub::install_teardown(&server_state, &execution_id, i64::from(child.id()));
    db.mark_execution_orphaned(&execution_id, "presumed dead").unwrap();

    let other_execution_id = "run-live-state-occupant";
    server_state.live_worker_states.register_spawn(
        1,
        other_execution_id.to_owned(),
        "claude-opus-4-7",
        std::process::id() as i32,
        None,
    );

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let outcome = server_state.release_worker_pane(&execution_id).await;
    assert_eq!(outcome, PaneReleaseOutcome::Reaped);

    let no_detach = tokio::time::timeout(std::time::Duration::from_millis(200), sink.next()).await;
    assert!(
        no_detach.is_err(),
        "expected no DetachWorkerPane when live-state names a different run, got {no_detach:?}",
    );

    let state = server_state
        .live_worker_states
        .get(1)
        .expect("the other run's live-state entry must survive");
    assert_eq!(state.run_id, other_execution_id);

    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join wait task")
        .expect("wait on child");
    assert!(
        !status.success(),
        "the retiring run's own untracked process tree must still go down",
    );
}

/// A `running` execution durably occupying a slot with no live-state entry is
/// a worker the engine lost track of: the list must not call it a husk, and
/// retire must refuse. Both paths share one predicate.
#[tokio::test]
async fn a_running_durable_occupant_with_no_live_state_is_not_a_husk_and_retire_refuses() {
    use crate::test_support::*;

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    // `create_spawned_execution` leaves the execution `running` on worker-1.
    let execution_id = create_spawned_execution(db, &work_item_id, 4_194_303);
    assert!(server_state.live_worker_states.get(1).is_none());

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;
    let panes = all_pane_statuses_for(&server_state, &sink, vec![hosted(1, &execution_id)]).await;
    assert_eq!(panes.len(), 1);
    assert!(
        matches!(
            panes[0].state,
            crate::protocol::HostedPaneState::LiveProcessNoRegistry { .. }
        ),
        "a running durable occupant must not be listed as a husk: {:?}",
        panes[0].state,
    );

    match server_state.retire_pane(1).await {
        Err(RetirePaneError::LiveRunTracked { slot_id, run_id }) => {
            assert_eq!(slot_id, 1);
            assert_eq!(run_id, execution_id);
        }
        other => panic!("expected LiveRunTracked, got {other:?}"),
    }
}

/// An occupant parked in `waiting_review` has no worker by design: the list
/// calls it a husk and retire detaches it instead of pointing at `agents stop`.
#[tokio::test]
async fn a_waiting_review_durable_occupant_is_a_husk_and_retire_detaches() {
    use crate::test_support::*;

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    let execution_id = create_spawned_execution(db, &work_item_id, 4_194_303);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'waiting_review' WHERE id = ?1",
            rusqlite::params![&execution_id],
        )
        .unwrap();

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;
    let husks = husk_panes_for(&server_state, &sink, vec![hosted(1, &execution_id)]).await;
    assert_eq!(husks.iter().map(|pane| pane.slot_id).collect::<Vec<_>>(), vec![1]);

    let server_clone = server_state.clone();
    let retire = tokio::spawn(async move { server_clone.retire_pane(1).await });
    let request = sink.next().await.expect("a DetachWorkerPane request");
    match request.payload {
        FrontendEvent::EngineRequest { request_id, request } => {
            assert!(
                matches!(request, EngineToAppRequest::DetachWorkerPane(_)),
                "expected DetachWorkerPane, got {request:?}"
            );
            server_state
                .deliver_app_response(
                    "session-app",
                    &request_id,
                    EngineToAppResponse::DetachWorkerPane {
                        result: Ok(crate::protocol::DetachWorkerPaneResult {}),
                    },
                )
                .await;
        }
        other => panic!("expected EngineRequest, got {other:?}"),
    }
    let result = retire.await.expect("retire task");
    assert!(result.is_ok(), "expected retirement to succeed, got {result:?}");
}

/// An orphaned execution whose newest run moved to another slot must not be
/// attributed to its old slot, so retiring the old slot never reaps the
/// worker now running in the new one.
#[tokio::test]
async fn retire_pane_does_not_reap_an_execution_that_resumed_onto_another_slot() {
    use crate::test_support::*;

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    let mut child = spawn_group_leader_sleeper();
    let execution_id = create_spawned_execution(db, &work_item_id, 4_194_303);
    // Resume onto worker-2: a newer run row on a different slot.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET created_at = '1000000000' WHERE execution_id = ?1",
            rusqlite::params![&execution_id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'ready' WHERE id = ?1",
            rusqlite::params![&execution_id],
        )
        .unwrap();
    db.start_execution_run(&execution_id, "worker-2", "repo-1", "lease-2", "ws-2", "/tmp/ws-2")
        .unwrap();
    assert!(
        db.set_run_shell_pid_for_execution(&execution_id, i64::from(child.id()))
            .unwrap()
    );
    db.mark_execution_orphaned(&execution_id, "presumed dead").unwrap();

    assert_eq!(server_state.hosted_pane_run_for_slot(1).await, SlotOccupancy::Absent);
    assert_eq!(
        server_state.hosted_pane_run_for_slot(2).await,
        SlotOccupancy::Occupied(execution_id.clone())
    );

    let result = server_state.retire_pane(1).await;
    assert!(result.is_ok(), "expected Ok, got {result:?}");
    assert!(
        child.try_wait().expect("poll child").is_none(),
        "retiring the old slot must not reap the worker that resumed on another slot",
    );
    child.kill().ok();
    child.wait().ok();
}

/// Rewrite `install_teardown`'s unparseable session name to this slot's
/// spawn shape and script an adopted live-tmux corroboration on the
/// pane-delivery override (independent of teardown's `tmux_override`).
fn corroborate_slot_tmux_adopted(server_state: &ServerState, execution_id: &str, slot_id: u8) {
    let session = format!("boss-{slot_id}-occupancy");
    let token = format!("token-{execution_id}");
    server_state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET tmux_session_name = ?1 WHERE execution_id = ?2",
            rusqlite::params![&session, execution_id],
        )
        .unwrap();
    let replies = adopted_tmux_replies(&session, &token);
    let (tmux, _) = super::tmux_stub::fake_tmux(replies);
    *server_state.pane_delivery_tmux_override.write().unwrap() = Some(tmux);
}

fn adopted_tmux_replies(session: &str, token: &str) -> Vec<boss_tmux::CommandOutput> {
    use super::tmux_stub::ok;
    vec![
        ok(&format!("{session}\t\n")),
        ok(&format!("BOSS_SPAWN_TOKEN={token}\n")),
        ok("0"),
        ok("1776528000"),
        ok("claude"),
    ]
}

async fn assign_replacement(server: &ServerState) {
    assert!(
        server
            .execution_coordinator
            .reclaim_slot("worker-1", "replacement")
            .await
    );
    server
        .live_worker_states
        .register_spawn(1, "replacement", "claude-opus-4-7", 0, None);
    server.start_live_status_slot(1, "replacement", std::sync::Arc::new(crate::driver::ClaudeDriver));
}

async fn assert_replacement_survives(server: &ServerState) {
    assert_eq!(
        server.execution_coordinator.claim_holder("worker-1").await.as_deref(),
        Some("replacement")
    );
    assert_eq!(server.live_worker_states.get(1).unwrap().run_id, "replacement");
    assert!(server.live_status_manager.has_slot(1));
    server.live_status_manager.stop_slot_for_run(1, "retired-run");
    assert!(server.live_status_manager.has_slot(1));
}

#[tokio::test]
async fn run_scoped_cleanup_removes_only_the_matching_occupant() {
    let (server, _dir) = test_server_state();
    assign_replacement(&server).await;
    assert_eq!(server.live_worker_states.release_slot_for_run("retired-run"), None);
    assert_replacement_survives(&server).await;
    assert_eq!(server.live_worker_states.release_slot_for_run("replacement"), Some(1));
    assert!(server.live_worker_states.get(1).is_none());
    server.live_status_manager.stop_slot_for_run(1, "replacement");
    assert!(!server.live_status_manager.has_slot(1));
}

#[tokio::test]
async fn retire_preserves_replacement_assigned_during_tmux_probe() {
    let (server, _dir) = test_server_state();
    let run = super::tmux_stub::seed_teardown(&server);
    corroborate_slot_tmux_adopted(&server, &run, 1);
    server.work_db.mark_execution_orphaned(&run, "worker exited").unwrap();
    let (tmux, runner) = super::tmux_stub::fake_tmux(adopted_tmux_replies("boss-1-occupancy", &format!("token-{run}")));
    let pause = runner.pause_next();
    *server.pane_delivery_tmux_override.write().unwrap() = Some(tmux);
    let sink = make_session_sink();
    server.register_app_session("session-app".into(), sink.clone()).await;
    let task_server = server.clone();
    let retire = tokio::spawn(async move { task_server.retire_pane(1).await });
    tokio::time::timeout(Duration::from_secs(5), pause.entered.notified())
        .await
        .unwrap();
    assign_replacement(&server).await;
    pause.resume.notify_one();
    assert!(matches!(
        retire.await.unwrap(),
        Err(RetirePaneError::LiveRunTracked { .. })
    ));
    assert_replacement_survives(&server).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), sink.next())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn retire_preserves_replacement_assigned_during_detach_ack() {
    let (server, _dir) = test_server_state();
    let sink = make_session_sink();
    server.register_app_session("session-app".into(), sink.clone()).await;
    let task_server = server.clone();
    let retire = tokio::spawn(async move { task_server.retire_pane(1).await });
    let envelope = sink.next().await.unwrap();
    let FrontendEvent::EngineRequest {
        request_id,
        request: EngineToAppRequest::DetachWorkerPane(_),
    } = envelope.payload
    else {
        panic!("expected detach")
    };
    assign_replacement(&server).await;
    server
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::DetachWorkerPane {
                result: Ok(crate::protocol::DetachWorkerPaneResult {}),
            },
        )
        .await;
    assert!(matches!(
        retire.await.unwrap(),
        Err(RetirePaneError::LiveRunTracked { .. })
    ));
    assert_replacement_survives(&server).await;
}

#[tokio::test]
async fn missing_session_stop_clears_identity_and_allows_retirement() {
    let (server, _dir) = test_server_state();
    let run = super::tmux_stub::seed_teardown(&server);
    corroborate_slot_tmux_adopted(&server, &run, 1);
    server.work_db.mark_execution_orphaned(&run, "worker exited").unwrap();
    let (tmux, _) = super::tmux_stub::fake_tmux([super::tmux_stub::ok("")]);
    *server.pane_delivery_tmux_override.write().unwrap() = Some(tmux);
    let error = server.retire_pane(1).await.unwrap_err().to_string();
    assert!(error.contains(&format!("bossctl agents stop {run}")));
    assert!(error.contains("bossctl agents retire-pane 1"));
    let (tmux, _) = super::tmux_stub::fake_tmux([super::tmux_stub::failure("session not found")]);
    server.set_tmux_override_for_test(tmux);
    // Exercise the same completion entry point as `bossctl agents stop`.
    server.completion_handler.force_stop_execution(&run).await;
    assert!(server.work_db.tmux_identity_for_execution(&run).unwrap().is_none());
    server.retire_pane(1).await.unwrap();
}

fn install_occupancy_tmux(server_state: &ServerState, replies: Vec<boss_tmux::CommandOutput>) {
    // Three copies: hosted_pane_run_for_slot, list_hosted_pane_statuses, and
    // retire_pane each probe live tmux once.
    let first = replies.clone();
    let second = replies.clone();
    let triple = first.into_iter().chain(second).chain(replies);
    let (tmux, _) = super::tmux_stub::fake_tmux(triple);
    *server_state.pane_delivery_tmux_override.write().unwrap() = Some(tmux);
}

async fn seed_orphaned_slot1_with_identity(
    server_state: &ServerState,
    session_name: &str,
    token: &str,
) -> (String, std::process::Child) {
    use crate::test_support::*;
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    let child = spawn_group_leader_sleeper();
    let execution_id = create_spawned_execution(db, &work_item_id, i64::from(child.id()));
    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution_id, boss_tmux::SERVER_LABEL, session_name, token)
            .unwrap()
    );
    db.mark_execution_orphaned(&execution_id, "presumed dead").unwrap();
    assert!(
        server_state
            .execution_coordinator
            .reclaim_slot("worker-1", &execution_id)
            .await,
        "the occupancy tests pin a pool claim so a refused retire can prove bookkeeping is untouched",
    );
    (execution_id, child)
}

async fn assert_inconclusive_and_untouched(
    server_state: &std::sync::Arc<ServerState>,
    slot_id: u8,
    execution_id: &str,
    reason_needle: &str,
    child: &mut std::process::Child,
) {
    match server_state.hosted_pane_run_for_slot(slot_id).await {
        SlotOccupancy::Inconclusive { run_id, reason } => {
            assert_eq!(run_id.as_deref(), Some(execution_id));
            assert!(
                reason.contains(reason_needle),
                "expected reason to contain {reason_needle:?}, got {reason}"
            );
        }
        other => panic!("expected Inconclusive, got {other:?}"),
    }

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;
    let panes = all_pane_statuses_for(server_state, &sink, vec![hosted(slot_id, execution_id)]).await;
    assert_eq!(panes.len(), 1);
    assert!(
        matches!(
            &panes[0].state,
            crate::protocol::HostedPaneState::OccupancyInconclusive { .. }
        ),
        "inconclusive occupancy must not be listed as a husk: {:?}",
        panes[0].state,
    );

    match server_state.retire_pane(slot_id).await {
        Err(RetirePaneError::OccupancyInconclusive { slot_id: got, reason }) => {
            assert_eq!(got, slot_id);
            assert!(
                reason.contains(reason_needle),
                "expected reason to contain {reason_needle:?}, got {reason}"
            );
        }
        other => panic!("expected OccupancyInconclusive, got {other:?}"),
    }

    let no_detach = tokio::time::timeout(std::time::Duration::from_millis(200), sink.next()).await;
    assert!(
        no_detach.is_err(),
        "a refused retire must not detach the viewer, got {no_detach:?}",
    );
    let holder = server_state.execution_coordinator.claim_holder("worker-1").await;
    assert_eq!(
        holder.as_deref(),
        Some(execution_id),
        "refusing occupancy must leave the pool claim untouched, got {holder:?}"
    );
    assert!(
        server_state.live_worker_states.get(slot_id).is_none(),
        "refusing occupancy must not synthesize a live-state entry"
    );
    assert!(
        child.try_wait().expect("poll child").is_none(),
        "inconclusive occupancy must not reap the worker process",
    );
}

/// A tmux identity that names a session hosted in a different slot is
/// inconclusive: list reports a non-husk, retire refuses without mutating
/// slot bookkeeping.
#[tokio::test]
async fn conflicting_slot_identity_is_inconclusive_and_retire_does_not_mutate() {
    let (server_state, _dir) = test_server_state();
    let (execution_id, mut child) = seed_orphaned_slot1_with_identity(&server_state, "boss-2-abcdef", "token-x").await;
    // A live matching session on the *wrong* name must not flip this to Occupied.
    install_occupancy_tmux(&server_state, adopted_tmux_replies("boss-2-abcdef", "token-x"));
    assert_inconclusive_and_untouched(&server_state, 1, &execution_id, "names slot 2", &mut child).await;
    child.kill().ok();
    child.wait().ok();
}

#[tokio::test]
async fn token_mismatch_is_inconclusive_and_retire_does_not_mutate() {
    use super::tmux_stub::ok;
    let (server_state, _dir) = test_server_state();
    let (execution_id, mut child) =
        seed_orphaned_slot1_with_identity(&server_state, "boss-1-abcdef", "token-ours").await;
    install_occupancy_tmux(
        &server_state,
        vec![ok("boss-1-abcdef\t\n"), ok("BOSS_SPAWN_TOKEN=token-someone-elses\n")],
    );
    assert_inconclusive_and_untouched(
        &server_state,
        1,
        &execution_id,
        "spawn token does not match",
        &mut child,
    )
    .await;
    child.kill().ok();
    child.wait().ok();
}

#[tokio::test]
async fn missing_session_is_inconclusive_and_retire_does_not_mutate() {
    use super::tmux_stub::ok;
    let (server_state, _dir) = test_server_state();
    let (execution_id, mut child) =
        seed_orphaned_slot1_with_identity(&server_state, "boss-1-abcdef", "token-ours").await;
    install_occupancy_tmux(&server_state, vec![ok("other-session\t\n")]);
    assert_inconclusive_and_untouched(&server_state, 1, &execution_id, "session is missing", &mut child).await;
    child.kill().ok();
    child.wait().ok();
}

#[tokio::test]
async fn probe_failure_is_inconclusive_and_retire_does_not_mutate() {
    use super::tmux_stub::failure;
    let (server_state, _dir) = test_server_state();
    let (execution_id, mut child) =
        seed_orphaned_slot1_with_identity(&server_state, "boss-1-abcdef", "token-ours").await;
    install_occupancy_tmux(&server_state, vec![failure("error connecting to server")]);
    assert_inconclusive_and_untouched(&server_state, 1, &execution_id, "inventory unavailable", &mut child).await;
    child.kill().ok();
    child.wait().ok();
}

/// The pid probe reads the newest local row by `created_at`. An older
/// worker-1 run with a transcript must not credit that execution to slot 1
/// when a newer worker-2 run has no transcript yet.
#[tokio::test]
async fn older_transcript_run_does_not_credit_occupancy_to_the_old_slot() {
    use crate::test_support::*;

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    let execution_id = create_spawned_execution(db, &work_item_id, 4_194_303);
    db.set_run_transcript_path_if_unset(&execution_id, "/tmp/older-worker-1.jsonl")
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET created_at = '1000000000' WHERE execution_id = ?1",
            rusqlite::params![&execution_id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'ready' WHERE id = ?1",
            rusqlite::params![&execution_id],
        )
        .unwrap();
    let (_exec, newer_run) = db
        .start_execution_run(&execution_id, "worker-2", "repo-1", "lease-2", "ws-2", "/tmp/ws-2")
        .unwrap();
    // Both runs finished: unfinished-first no longer prefers worker-2, so the
    // hook resolver's transcript tie-break would name worker-1.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET status = 'completed', finished_at = created_at WHERE id = ?1",
            rusqlite::params![&newer_run.id],
        )
        .unwrap();

    assert_eq!(
        db.latest_run_agent_id_for_execution(&execution_id).unwrap().as_deref(),
        Some("worker-1"),
        "precondition: the transcript-preferring resolver still names worker-1",
    );
    assert_eq!(
        db.latest_local_agent_id_for_execution(&execution_id)
            .unwrap()
            .as_deref(),
        Some("worker-2"),
        "the occupancy query must follow created_at, not the transcript resolver",
    );
    assert_eq!(server_state.hosted_pane_run_for_slot(1).await, SlotOccupancy::Absent);
    assert_eq!(
        server_state.hosted_pane_run_for_slot(2).await,
        SlotOccupancy::Occupied(execution_id)
    );
}

#[test]
fn slot_from_tmux_session_name_parses_the_spawn_shape() {
    assert_eq!(
        super::super::pane_ops::slot_from_tmux_session_name("boss-3-abc123"),
        Some(3)
    );
    assert_eq!(
        super::super::pane_ops::slot_from_tmux_session_name("boss-test-worker"),
        None
    );
    assert_eq!(super::super::pane_ops::slot_from_tmux_session_name("other-3-abc"), None);
}
