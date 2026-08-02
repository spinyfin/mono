use super::*;

// Tests for `ServerState::retire_pane` / `ServerState::list_husk_panes` —
// the break-glass "husk pane" verb. A husk is a pane the app still hosts
// a session in that the engine has NO live-tracked run for (crash,
// terminal-fail path bug, spawn-ack timeout); neither `stop_run` nor
// `reap_run` can reach it since both resolve through a run id the
// engine no longer maps to a slot.

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
async fn retire_pane_sends_slot_keyed_release_request_with_no_run_id_resolution() {
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

    // With no live-state entry for the slot, the durable-liveness guard runs
    // first: it asks the app which run occupies slot 7 so it can probe that
    // run's recorded pid. Answering "nothing hosted" leaves the guard inert
    // and the retirement proceeds, which is the path this test is about.
    let probe = sink.next().await.expect("an EngineRequest event should be enqueued");
    let probe_id = match probe.payload {
        FrontendEvent::EngineRequest { request_id, request } => {
            assert!(
                matches!(request, EngineToAppRequest::ListHostedPanes(_)),
                "expected the liveness probe first, got {request:?}"
            );
            request_id
        }
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    server_state
        .deliver_app_response(
            "session-app",
            &probe_id,
            EngineToAppResponse::ListHostedPanes {
                result: Ok(crate::protocol::ListHostedPanesResult { panes: vec![] }),
            },
        )
        .await;

    let envelope = sink.next().await.expect("an EngineRequest event should be enqueued");
    let (request_id, request) = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, request } => (request_id, request),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    match request {
        EngineToAppRequest::ReleaseWorkerPane(input) => {
            assert_eq!(input.slot_id, 7);
        }
        other => panic!("expected ReleaseWorkerPane, got {other:?}"),
    }

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::ReleaseWorkerPane {
                result: Ok(crate::protocol::ReleaseWorkerPaneResult {}),
            },
        )
        .await;

    let result = retire.await.expect("retire task");
    assert!(result.is_ok(), "expected Ok, got {result:?}");
}

#[tokio::test]
async fn list_husk_panes_returns_empty_when_no_app_session_registered() {
    // Best-effort query: with no app session there is nothing to
    // diff, so this must not be a hard error.
    let (server_state, _dir) = test_server_state();
    let panes = server_state.list_husk_panes().await.expect("expected Ok");
    assert!(panes.is_empty());
}

#[tokio::test]
async fn list_husk_panes_filters_out_slots_the_engine_still_tracks_live() {
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
    let list = tokio::spawn(async move { server_clone.list_husk_panes().await });

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

    let panes = list.await.expect("list task").expect("expected Ok");
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
// each live-state entry to `Terminated`; `list_husk_panes` filtered terminal
// entries out of its live set, so five slots classified as husks, and
// `retire_pane` re-read the same wrong bookkeeping and agreed. Five workers
// were SIGTERMed mid-work, three of them inside a foreground `bazel` build.
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
async fn list_husk_panes_does_not_flag_a_terminal_slot_whose_worker_is_alive() {
    // The classifier half. Slot 2 is the incident shape (terminal entry, live
    // process); slot 6 is a genuine husk the engine has no entry for at all.
    // Only slot 6 may be reported — a live worker must never even be flagged,
    // since being flagged is what starts the two-pass clock toward the kill.
    let (server_state, _dir) = test_server_state();
    drive_spurious_session_end_mid_tool(&server_state, 2, "run-victim");

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let list = tokio::spawn(async move { server_clone.list_husk_panes().await });

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

    let panes = list.await.expect("list task").expect("expected Ok");
    assert_eq!(
        panes.iter().map(|pane| pane.slot_id).collect::<Vec<_>>(),
        vec![6],
        "a terminal slot with a live worker process must not be classified as a husk: {panes:?}"
    );
}

#[tokio::test]
async fn list_husk_panes_flags_a_recycled_slot_even_though_its_entry_looks_alive() {
    // The `run_id` match in the classifier. The app is hosting a pane for
    // `run-old`, but the engine's entry for that slot belongs to `run-new`.
    // The slot was recycled: `run-new`'s liveness signals say nothing about
    // `run-old`'s stray pane, which is a genuine husk and must be reported.
    let (server_state, _dir) = test_server_state();
    drive_spurious_session_end_mid_tool(&server_state, 5, "run-new");

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let list = tokio::spawn(async move { server_clone.list_husk_panes().await });

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

    let panes = list.await.expect("list task").expect("expected Ok");
    assert_eq!(
        panes.iter().map(|pane| pane.run_id.as_str()).collect::<Vec<_>>(),
        vec!["run-old"],
        "a stray pane for a recycled run is still a husk: {panes:?}"
    );
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

/// Drive the app's `ListHostedPanes` round-trip for `panes` and return the
/// classifier's verdict. Factors out the request/response dance the husk tests
/// above all repeat.
async fn husk_panes_for(
    server_state: &Arc<ServerState>,
    sink: &Arc<SessionSink>,
    panes: Vec<crate::protocol::HostedPaneEntry>,
) -> Vec<crate::protocol::HostedPaneEntry> {
    let server_clone = server_state.clone();
    let list = tokio::spawn(async move { server_clone.list_husk_panes().await });

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
async fn list_husk_panes_spares_an_untracked_slot_whose_durable_process_is_alive() {
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

    let panes = husk_panes_for(&server_state, &sink, vec![hosted(4, &execution_id)]).await;
    assert!(
        panes.is_empty(),
        "a slot the engine forgot, whose execution was orphaned by INFERENCE and whose recorded \
         process is alive, is a re-adoption candidate — not a husk to SIGTERM: {panes:?}"
    );
}

#[tokio::test]
async fn list_husk_panes_still_retires_an_untracked_slot_whose_process_is_gone() {
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

    let panes = husk_panes_for(&server_state, &sink, vec![hosted(4, &execution_id)]).await;
    assert_eq!(
        panes.iter().map(|pane| pane.slot_id).collect::<Vec<_>>(),
        vec![4],
        "the guard must not disable the sweep: a dead process is still a husk",
    );
}

#[tokio::test]
async fn list_husk_panes_still_retires_a_lingering_shell_under_a_cancelled_run() {
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

    let panes = husk_panes_for(&server_state, &sink, vec![hosted(4, &execution_id)]).await;
    assert_eq!(
        panes.iter().map(|pane| pane.slot_id).collect::<Vec<_>>(),
        vec![4],
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
    db.mark_execution_orphaned(&execution_id, "presumed dead").unwrap();

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let retire = tokio::spawn(async move { server_clone.retire_pane(4).await });

    // Guard 3's own liveness probe: "what does the app host in slot 4?"
    let probe = sink.next().await.expect("an EngineRequest event should be enqueued");
    match probe.payload {
        FrontendEvent::EngineRequest { request_id, request } => {
            assert!(
                matches!(request, EngineToAppRequest::ListHostedPanes(_)),
                "expected the liveness probe, got {request:?}"
            );
            server_state
                .deliver_app_response(
                    "session-app",
                    &request_id,
                    EngineToAppResponse::ListHostedPanes {
                        result: Ok(crate::protocol::ListHostedPanesResult {
                            panes: vec![hosted(4, &execution_id)],
                        }),
                    },
                )
                .await;
        }
        other => panic!("expected EngineRequest, got {other:?}"),
    }

    // The durable teardown's own reverse lookup ("which slot hosts this
    // run?", shared with `agents stop`'s fallback) — same question, asked
    // again since the engine has no bookkeeping to read it from.
    let reverse_lookup = sink.next().await.expect("a second EngineRequest should be enqueued");
    match reverse_lookup.payload {
        FrontendEvent::EngineRequest { request_id, request } => {
            assert!(
                matches!(request, EngineToAppRequest::ListHostedPanes(_)),
                "expected the reverse hosted-pane lookup, got {request:?}"
            );
            server_state
                .deliver_app_response(
                    "session-app",
                    &request_id,
                    EngineToAppResponse::ListHostedPanes {
                        result: Ok(crate::protocol::ListHostedPanesResult {
                            panes: vec![hosted(4, &execution_id)],
                        }),
                    },
                )
                .await;
        }
        other => panic!("expected EngineRequest, got {other:?}"),
    }

    // Then the actual slot-keyed teardown request.
    let release = sink
        .next()
        .await
        .expect("a ReleaseWorkerPane request should be enqueued");
    match release.payload {
        FrontendEvent::EngineRequest { request_id, request } => {
            assert!(
                matches!(
                    request,
                    EngineToAppRequest::ReleaseWorkerPane(ReleaseWorkerPaneInput { slot_id: 4, .. })
                ),
                "expected ReleaseWorkerPane for slot 4, got {request:?}"
            );
            server_state
                .deliver_app_response(
                    "session-app",
                    &request_id,
                    EngineToAppResponse::ReleaseWorkerPane {
                        result: Ok(crate::protocol::ReleaseWorkerPaneResult {}),
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
