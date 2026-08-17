//! `FrontendRequest` handlers — executions, runs, and transcripts.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

use crate::coordinator::DispatchAdmission;

/// Byte cap for an over-SSH remote transcript tail pull. The
/// `TailRunTranscript` RPC requests a line count, not a byte count, so
/// we pull a generous suffix and split it to the requested lines; 256 KiB
/// comfortably covers the few-hundred-line tails the viewer requests
/// while keeping the SSH round-trip cheap. Matches the bound
/// `transient_recovery` uses for its local tail read.
const REMOTE_TRANSCRIPT_TAIL_BYTES: u64 = 256 * 1024;

pub(super) async fn handle_create_execution(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateExecution { input } = req else {
        unreachable!()
    };
    match work_db.create_execution(input) {
        Ok(execution) => {
            send_response(&sink, &request_id, FrontendEvent::ExecutionCreated { execution });
        }
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
        }
    }
}

pub(super) async fn handle_request_execution(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RequestExecution { mut input } = req else {
        unreachable!()
    };
    input.work_item_id = match server_state.resolve_work_item_id(&input.work_item_id).await {
        Ok(id) => id,
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    };
    if input.bypass_dispatch_pause {
        // Pause-only forced dispatch (`bossctl work start --force`, or a
        // confirmed app-drag going through `MoveWorkItemOnBoard` instead
        // of this RPC directly). Distinct from `force` (pool growth,
        // below): this never grows a pool, never skips the interactive
        // cap, and is refused outright when any non-overridable
        // constraint blocks it — see
        // `docs/designs/operator-forced-dispatch-while-dispatch-is-paused.md`.
        let coordinator = server_state.execution_coordinator.clone();
        let live_states = server_state.live_worker_states.clone();
        match coordinator.dispatch_with_pause_bypass(input, live_states).await {
            Ok(execution) => {
                send_response(&sink, &request_id, FrontendEvent::ExecutionRequested { execution });
            }
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
            }
        }
        return;
    }
    {
        // Live-worker awareness: when the work item already has
        // a non-terminal execution, the engine reuses it only
        // when the slot registry actually still has a live
        // worker for that run id. Without this check, a chore
        // whose previous worker died with the app gets stuck
        // — the kanban drag fires RequestExecution, the engine
        // says "still running," coordinator polls for `ready`
        // and sees nothing, no new spawn ever happens.
        //
        // `force = true` is the `bossctl agents launch`
        // entry point: same DB row creation, but we hand the
        // ready execution straight to
        // `ExecutionCoordinator::force_dispatch` instead of
        // kicking the auto-dispatcher. force_dispatch grows
        // the execution's home pool (main, automation, or
        // review) by one slot (bounded by that pool's own
        // hard cap) when every configured slot is busy, so
        // the launch verb skips the cap-deferral the normal
        // request path would otherwise hit.
        //
        // It is admitted as `OperatorForced`, which overrides an
        // active dispatch pause: a human naming one execution by
        // hand is deliberately overriding their own pause (and
        // may be probing a breaker pause on purpose). That bypass
        // is declared in `bossctl dispatch state` so the pause's
        // stated scope stays honest. The non-force path below
        // just queues and kicks — the row waits behind the pause
        // and drains on resume.
        let force = input.force;
        let live_states = server_state.live_worker_states.clone();
        let result = work_db.request_execution_with_live_check(input, |run_id| live_states.is_run_live(run_id));
        match result {
            Ok(execution) => {
                if force {
                    // If the request landed on an existing
                    // non-terminal execution (idempotent path
                    // when a live worker already runs the
                    // item), just refresh the row and skip
                    // force-dispatch — there's no second
                    // worker to spawn.
                    if execution.status == ExecutionStatus::Ready {
                        let coordinator = server_state.execution_coordinator.clone();
                        let execution_id = execution.id.clone();
                        match coordinator
                            .force_dispatch(&execution_id, DispatchAdmission::OperatorForced)
                            .await
                        {
                            Ok(_worker_id) => {}
                            Err(err) => {
                                send_work_error(&sink, &request_id, &err);
                                return;
                            }
                        }
                    }
                    // Re-read the execution after force_dispatch
                    // so the response carries the row's now-
                    // running status (and worker / lease ids).
                    let refreshed = match work_db.get_execution(&execution.id) {
                        Ok(execution) => execution,
                        Err(_) => execution,
                    };
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::ExecutionRequested { execution: refreshed },
                    );
                } else {
                    // Log every queued request so an operator can pair
                    // a `bossctl work start` call with the engine-side
                    // outcome even when the scheduler races the row
                    // (the kick-noop/lost-wakeup class of bug). The
                    // structured `spawn_attempt` line lands in
                    // `run_scheduler` once it picks the row up; this
                    // line bookends the request itself.
                    tracing::info!(
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        execution_status = %execution.status,
                        "RequestExecution accepted -> kicking scheduler"
                    );
                    server_state.execution_coordinator.kick();
                    send_response(&sink, &request_id, FrontendEvent::ExecutionRequested { execution });
                }
            }
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
            }
        }
    }
}

/// Read-only: `EvaluateDispatchAdmission`. See
/// `ExecutionCoordinator::evaluate_dispatch_admission` — the same
/// function `handle_request_execution`'s `bypass_dispatch_pause` path
/// consults, so this preview can never promise something the mutating
/// request doesn't also honor.
pub(super) async fn handle_evaluate_dispatch_admission(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::EvaluateDispatchAdmission { work_item_id } = req else {
        unreachable!()
    };
    let work_item_id = match server_state.resolve_work_item_id(&work_item_id).await {
        Ok(id) => id,
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    };
    match server_state
        .execution_coordinator
        .evaluate_dispatch_admission(&work_item_id)
        .await
    {
        Ok(admission) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::DispatchAdmissionEvaluated { admission },
            );
        }
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
        }
    }
}

pub(super) async fn handle_list_executions(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListExecutions {
        work_item_id,
        include_revision_chain,
    } = req
    else {
        unreachable!()
    };
    let work_item_id = match work_item_id {
        Some(id) => match server_state.resolve_work_item_id(&id).await {
            Ok(id) => Some(id),
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
                return;
            }
        },
        None => None,
    };
    {
        let result = if include_revision_chain {
            match work_item_id.as_deref() {
                Some(id) => work_db.list_executions_for_chain(id),
                None => work_db.list_executions(None),
            }
        } else {
            work_db.list_executions(work_item_id.as_deref())
        };
        match result {
            Ok(executions) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::ExecutionsList {
                        work_item_id,
                        executions,
                    },
                );
            }
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
            }
        }
    }
}

pub(super) async fn handle_get_task_runtime(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetTaskRuntime { work_item_id } = req else {
        unreachable!()
    };
    let work_item_id = match server_state.resolve_work_item_id(&work_item_id).await {
        Ok(id) => id,
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    };
    {
        match work_db.get_task_runtime(&work_item_id) {
            Ok(runtime) => {
                send_response(&sink, &request_id, FrontendEvent::TaskRuntimeResult { runtime });
            }
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
            }
        }
    }
}

pub(super) async fn handle_get_execution(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetExecution { id } = req else {
        unreachable!()
    };
    match work_db.get_execution(&id) {
        Ok(execution) => {
            send_response(&sink, &request_id, FrontendEvent::ExecutionResult { execution });
        }
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
        }
    }
}

pub(super) async fn handle_create_run(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateRun { input } = req else {
        unreachable!()
    };
    match work_db.create_run(input) {
        Ok(run) => {
            send_response(&sink, &request_id, FrontendEvent::RunCreated { run });
        }
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
        }
    }
}

pub(super) async fn handle_list_runs(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListRuns { execution_id } = req else {
        unreachable!()
    };
    match work_db.list_runs(&execution_id) {
        Ok(runs) => {
            send_response(&sink, &request_id, FrontendEvent::RunsList { execution_id, runs });
        }
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
        }
    }
}

pub(super) async fn handle_get_run(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetRun { id } = req else {
        unreachable!()
    };
    {
        // Try the run_* namespace first, then fall back to the
        // exec_* namespace. Callers such as `bossctl agents
        // status` pass whatever id they have in hand — often an
        // execution id (exec_*) — but `get_run` joins against
        // `work_runs.id` (run_*), so the lookup silently fails
        // with "unknown run". `list_runs(exec_id)` finds the
        // run via `work_runs.execution_id` and returns the most
        // recent one (the active or last-completed run for that
        // execution).
        let result = work_db
            .get_run(&id)
            .ok()
            .or_else(|| work_db.list_runs(&id).ok().and_then(|mut runs| runs.pop()));
        match result {
            Some(run) => {
                send_response(&sink, &request_id, FrontendEvent::RunResult { run });
            }
            None => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("unknown run: {id}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_probe_run(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::ProbeRun { run_id, text, urgent } = req else {
        unreachable!()
    };
    {
        // `bossctl probe` is a coordinator-essential verb (the
        // coordinator contract names probing as the right tool
        // for low-confidence handoffs). The earlier BossOnly
        // gate rejected calls from worker (slot) panes, since
        // BossOnly explicitly excludes callers descending from
        // a registered worker shell pid. Same reasoning as the
        // `stop_run` fix in PR #218: downgrade to AppOrBoss so
        // any caller descending from the app or the Boss
        // session is accepted, including worker panes.
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                run_id = %run_id,
                "probe_run rejected: caller not in app/Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "probe_run requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        // Decide *before* minting a probe id whether this probe can actually
        // be delivered, and refuse if not. A queue-accepted-then-silently-
        // dropped probe is indistinguishable to the caller from one that is
        // about to arrive, so the caller cannot tell a healthy worker from an
        // unreachable one. Deciding up front makes the refusal visible in the
        // response rather than only in a `debug!` line inside the engine.
        let expected_delivery = match probe_delivery_expectation(&server_state, &run_id) {
            Ok(expectation) => expectation,
            Err(reason) => {
                tracing::warn!(run_id = %run_id, urgent, %reason, "probe refused: undeliverable");
                send_response(&sink, &request_id, FrontendEvent::ProbeRefused { run_id, reason });
                return;
            }
        };
        let probe_id = server_state.queue_probe_resolving_worker_signal(run_id.clone(), text, urgent);
        // `probe_delivery_expectation` above confirmed a slot mapping existed,
        // but that check and this insert are not atomic with
        // `release_worker_pane`'s teardown drain: if the run's pane was
        // released in between, the drain already ran and found nothing (this
        // probe was not queued yet), and the insert above then landed against
        // a run nothing will ever revisit. Re-check right after inserting and
        // settle immediately rather than leaving it reading `queued` forever.
        //
        // Reuse `abandon_pending_probes_for_terminated_run` (rather than
        // hand-copying its drain-and-log body) so this call site can still
        // see whether the probe it just minted is among the ones settled
        // `Abandoned` — deciding before answering, per the doc above, rather
        // than answering `ProbeQueued` for a probe already known dead.
        let just_abandoned = if server_state.worker_registry.slot_for_run(&run_id).is_none() {
            let ids = server_state.abandon_pending_probes_for_terminated_run(
                &run_id,
                "run's pane was released between the delivery-expectation check and the probe \
                 being queued",
            );
            ids.contains(&probe_id)
        } else {
            false
        };
        if just_abandoned {
            let reason = "run's pane was released between the delivery-expectation check and the probe \
                           being queued; the probe cannot be delivered"
                .to_owned();
            tracing::warn!(run_id = %run_id, probe_id = %probe_id, %reason, "probe refused: abandoned before response");
            send_response(&sink, &request_id, FrontendEvent::ProbeRefused { run_id, reason });
            return;
        }
        tracing::info!(
            run_id = %run_id,
            probe_id = %probe_id,
            urgent,
            expected_delivery = expected_delivery.as_str(),
            "probe queued",
        );
        // A human/coordinator probe on this run IS the documented ack
        // gesture for a worker-declared escalation/blocker (e.g.
        // `bossctl probe <agent> "[effort-escalation-ack] …"`) — but
        // acceptance is not delivery. Resolving the open worker-signal
        // attention items here, at accept time, would lift the completion
        // handler's "produce a PR" auto-nudge suppression even when this
        // probe subsequently settles `Abandoned`/`Orphaned` and the
        // acknowledgement text never reaches the worker — the exact
        // asymmetry [`crate::app::probes::PROBE_UNDELIVERED_ATTENTION_KIND`]
        // exists to stop repeating. The probe was tagged atomically inside
        // `queue_probe_resolving_worker_signal` above, before it became
        // visible to any dispatcher: the resolution fires from
        // [`ServerState::set_probe_lifecycle_detail`] the moment this
        // probe's own delivery state actually confirms the worker got it,
        // and never fires at all if it doesn't.
        // Deliver the probe right now if the worker's pane will take a
        // write — parked (no boundary is coming on its own) or mid-turn on
        // a driver that buffers pane input (the composer takes it while the
        // worker works). Only a pane that cannot be written to at all leaves
        // the probe queued for a later `PostToolUse`/`Stop` retry.
        let server_for_now = server_state.clone();
        let run_id_for_now = run_id.clone();
        let probe_id_for_now = probe_id.clone();
        tokio::spawn(async move {
            let outcome = dispatch_probe_now(&server_for_now, &run_id_for_now).await;
            // This runs detached, after the response has already gone back to
            // the caller, so its verdict is otherwise invisible. Log it — and
            // say so loudly when the engine promised `Immediate` and the
            // immediate path then declined to deliver, which is the engine
            // breaking a commitment it made in the response it already sent.
            // `Orphaned` still counts as the commitment honoured: it is recorded
            // only after a successful `SendToPane` write, when the pane's pid
            // liveness check failed after the fact (see
            // `ProbeDeliveryState::is_undeliverable`'s doc) — the pane got the
            // bytes, and a later `Replied` can still correct the record.
            // `Dropped`/`Abandoned` are the only states where nothing was ever
            // written, so only those break an `Immediate` commitment.
            let promised_now = expected_delivery == ProbeDeliveryExpectation::Immediate;
            let broke_commitment = promised_now
                && !matches!(
                    outcome,
                    ProbeDispatchOutcome::Dispatched(state)
                        if !matches!(state, ProbeDeliveryState::Dropped | ProbeDeliveryState::Abandoned)
                );
            if broke_commitment {
                tracing::warn!(
                    run_id = %run_id_for_now,
                    probe_id = %probe_id_for_now,
                    outcome = outcome.as_str(),
                    "probe was accepted with an `immediate` delivery commitment but settled \
                     undeliverable without ever reaching the pane",
                );
            } else {
                tracing::debug!(
                    run_id = %run_id_for_now,
                    probe_id = %probe_id_for_now,
                    outcome = outcome.as_str(),
                    expected_delivery = expected_delivery.as_str(),
                    "immediate probe dispatch attempt settled",
                );
            }
        });
        send_response(
            &sink,
            &request_id,
            FrontendEvent::ProbeQueued {
                run_id,
                probe_id,
                urgent,
                expected_delivery: Some(expected_delivery),
            },
        );
    }
}

/// Decide where a probe for `run_id` would be delivered, or why it cannot be.
///
/// `Ok(expectation)` is a commitment: the engine believes it can deliver at
/// that boundary. `Err(reason)` means the probe must be refused rather than
/// queued — the caller has no way to distinguish a silently-undeliverable
/// probe from one that is about to arrive, so accepting it is worse than
/// saying no.
///
/// Refusals are limited to conditions where delivery is genuinely impossible:
///
/// * no slot mapping, or no live state for the mapped slot — there is no pane
///   to write into;
/// * a terminal worker (`Terminated`/`Errored`) — it will never read anything
///   again.
///
/// The `urgent` flag is deliberately **not** an input. It is queue priority,
/// not transport: where a probe lands is a function of the worker's pane
/// posture alone, so `--urgent` can no longer promise — or fail to promise —
/// a boundary of its own. That is why an urgent probe against a driver which
/// rejects mid-turn input is no longer refused: it waits for the same `Stop`
/// its non-urgent sibling waits for, and the returned expectation says so
/// rather than the call failing.
///
/// A `Spawning` worker is not refused either: it has a pane and will reach a
/// boundary, so the probe legitimately waits.
fn probe_delivery_expectation(server_state: &ServerState, run_id: &str) -> Result<ProbeDeliveryExpectation, String> {
    let Some(slot_id) = server_state.worker_registry.slot_for_run(run_id) else {
        return Err(format!(
            "run {run_id} has no live worker pane (no slot mapping); there is nothing to inject into"
        ));
    };
    let Some(activity) = server_state.pane_typed_input_activity(slot_id) else {
        return Err(format!(
            "run {run_id} is mapped to slot {slot_id} but the engine has no live worker state for it; \
             cannot confirm there is a pane to inject into"
        ));
    };
    if activity.is_terminal() {
        return Err(format!(
            "run {run_id} (slot {slot_id}) is {}; a probe would never be read",
            activity.as_str(),
        ));
    }
    let posture = server_state.pane_input_posture_for_run(run_id, slot_id);
    match posture {
        // Parked at its prompt, or mid-turn on a driver that buffers pane
        // input: either way `dispatch_probe_now` writes during this call.
        PaneInputPosture::Parked | PaneInputPosture::MidTurnBuffered => Ok(ProbeDeliveryExpectation::Immediate),
        // No write may be issued right now. `Spawning` is the temporary case:
        // the worker has a pane and its driver buffers mid-turn input, so the
        // first `PostToolUse` of its first turn delivers.
        PaneInputPosture::Refused
            if activity == boss_protocol::WorkerActivity::Spawning
                && server_state.run_mid_turn_pane_input(run_id).buffers() =>
        {
            Ok(ProbeDeliveryExpectation::NextToolBoundary)
        }
        // Everything else that is still live waits for a turn boundary: a
        // driver whose foreground process is not known to read mid-turn stdin
        // has no earlier opportunity, and neither does a spawning worker on
        // one. This is the trait default (`Rejects`), so it covers every
        // driver that has not measured its mid-turn behaviour.
        PaneInputPosture::Refused => Ok(ProbeDeliveryExpectation::NextTurnBoundary),
    }
}

pub(super) async fn handle_probe_status(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::ProbeStatus { probe_id } = req else {
        unreachable!()
    };
    {
        // Same tier as `ProbeRun`: whoever may issue a probe may read what
        // became of it, including from a worker pane.
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "probe_status requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        match server_state.probe_record(&probe_id) {
            Some(record) => send_response(
                &sink,
                &request_id,
                FrontendEvent::ProbeStatusResult {
                    run_id: record.run_id,
                    probe_id,
                    state: record.state,
                    urgent: record.urgent,
                    detail: record.detail,
                },
            ),
            // Probe ids are minted per engine process and never persisted, so
            // "unknown" legitimately covers both a typo and an id from before
            // the last engine restart. Say so rather than reporting a state.
            None => send_work_error(
                &sink,
                &request_id,
                format!(
                    "unknown probe id: {probe_id} (probe ids are per-engine-process and are not \
                     retained across an engine restart)"
                ),
            ),
        }
    }
}

pub(super) async fn handle_stop_run(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::StopRun { run_id } = req else {
        unreachable!()
    };
    {
        // `bossctl agents stop` is the coordinator superset's
        // imperative kill switch, and the human invokes it
        // from wherever they happen to be — including the
        // boss pane, the macOS app shell, or *inside a worker
        // pane* (e.g. tab over to slot 1, type `bossctl
        // agents stop <id>`). The earlier BossOnly gate
        // rejected the worker-pane case because callers
        // descending from a registered worker shell pid are
        // explicitly excluded from BossOnly. Downgrade to
        // AppOrBoss to match `cancel_execution`: any caller
        // descending from the app or the Boss session is
        // accepted, which covers worker panes too.
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                run_id = %run_id,
                "stop_run rejected: caller not in app/Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "stop_run requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        tracing::info!(run_id = %run_id, "stop_run requested");
        let handler = server_state.completion_handler.clone();
        let run_id_for_release = run_id.clone();
        tokio::spawn(async move {
            // Use `force_stop_execution` instead of plain
            // `force_release`: this additionally cancels the
            // execution row and demotes the task from `active`
            // back to `todo` so the orphan sweep and
            // `reconcile_active_dispatch` cannot immediately
            // re-dispatch the work item the moment the worker
            // pool slot is freed.
            handler.force_stop_execution(&run_id_for_release).await;
        });
        send_response(&sink, &request_id, FrontendEvent::RunStopped { run_id });
    }
}

/// Place an explicit operator hold on a live run, exempting it from the
/// idle-park ([`crate::completion::WorkerCompletionHandler::nudge_or_park`])
/// and auto-reap ([`crate::stale_worker_sweep::run_one_pass`]) sweeps
/// until released ([`handle_release_hold_run`]) or the run ends. Backs
/// `bossctl agents hold`. Errors if `run_id` is not a currently-live run —
/// a hold on a run that has already ended (or never existed) would sit in
/// the registry forever with nothing left to protect.
pub(super) async fn handle_hold_run(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::HoldRun { run_id, reason } = req else {
        unreachable!()
    };
    if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
        tracing::warn!(
            peer_pid = ?peer_pid,
            run_id = %run_id,
            "hold_run rejected: caller not in app/Boss subtree",
        );
        send_response(
            &sink,
            &request_id,
            FrontendEvent::Error {
                message: "hold_run requires app or Boss authority".to_owned(),
            },
        );
        return;
    }
    if !server_state.live_worker_states.is_run_live(&run_id) {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::Error {
                message: format!("hold_run: no live run matches `{run_id}`"),
            },
        );
        return;
    }
    let now_epoch_secs = boss_engine_utils::epoch_time::now_epoch_secs();
    tracing::info!(run_id = %run_id, reason = reason.as_deref().unwrap_or("(none given)"), "hold_run requested");
    server_state.hold_registry.hold(&run_id, reason.clone(), now_epoch_secs);
    server_state.live_worker_states.set_held(&run_id, true);
    send_response(&sink, &request_id, FrontendEvent::RunHeld { run_id, reason });
}

/// Release a previously-placed hold on `run_id`, restoring the normal
/// idle-park/auto-reap sweep behavior. Idempotent — releasing a run with
/// no hold in place still replies `RunHoldReleased`. Backs `bossctl
/// agents release-hold`.
pub(super) async fn handle_release_hold_run(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::ReleaseHoldRun { run_id } = req else {
        unreachable!()
    };
    if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
        tracing::warn!(
            peer_pid = ?peer_pid,
            run_id = %run_id,
            "release_hold_run rejected: caller not in app/Boss subtree",
        );
        send_response(
            &sink,
            &request_id,
            FrontendEvent::Error {
                message: "release_hold_run requires app or Boss authority".to_owned(),
            },
        );
        return;
    }
    tracing::info!(run_id = %run_id, "release_hold_run requested");
    server_state.hold_registry.release(&run_id);
    server_state.live_worker_states.set_held(&run_id, false);
    send_response(&sink, &request_id, FrontendEvent::RunHoldReleased { run_id });
}

pub(super) async fn handle_cancel_execution(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::CancelExecution {
        execution_id,
        reason,
        queued_only,
    } = req
    else {
        unreachable!()
    };
    {
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                execution_id = %execution_id,
                "cancel_execution rejected: caller not in app/Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "cancel_execution requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        // Snapshot from_status before the write so the audit event can
        // record what the operator cancelled (the returned row is already
        // `cancelled`).
        let from_status = server_state
            .work_db
            .get_execution(&execution_id)
            .ok()
            .map(|e| e.status.as_str().to_owned());
        let opts = crate::work::CancelExecutionOpts {
            reason: reason.clone(),
            queued_only,
        };
        match server_state.work_db.cancel_execution_with(&execution_id, opts) {
            Ok(execution) => {
                tracing::info!(
                    execution_id = %execution_id,
                    queued_only,
                    reason = reason.as_deref().unwrap_or("explicit cancel"),
                    "cancel_execution: marked cancelled",
                );
                // Durable operator-facing audit trail: status `cancelled`
                // already distinguishes deliberate cancel from `orphaned`
                // / `abandoned`; the reason + from_status make the
                // decision reconstructible months later from
                // engine-audit.log alone.
                crate::audit::record_event(
                    "execution_cancelled",
                    &serde_json::json!({
                        "execution_id": &execution.id,
                        "work_item_id": &execution.work_item_id,
                        "from_status": from_status,
                        "reason": reason.as_deref().unwrap_or("explicit cancel"),
                        "queued_only": queued_only,
                        "peer_pid": peer_pid,
                    }),
                );
                // Pane releases are keyed by run_id (the slot
                // registry's key), not by execution_id — so
                // walk the execution's still-active runs and
                // release each. Idempotent on the registry side.
                // Never-started (queued_only) rows typically have no
                // active run and no lease; force_release is still
                // harmless and keeps this path uniform with work cancel.
                let active_runs = server_state
                    .work_db
                    .active_run_ids_for_execution(&execution_id)
                    .unwrap_or_default();
                let handler = server_state.completion_handler.clone();
                let exec_for_release = execution_id.clone();
                tokio::spawn(async move {
                    for run_id in active_runs {
                        handler.force_release(&run_id).await;
                    }
                    // Final pass keyed by execution_id so the
                    // cube workspace lease (which is recorded
                    // on the execution row) is released even
                    // when the execution had no active run.
                    handler.force_release(&exec_for_release).await;
                });
                send_response(&sink, &request_id, FrontendEvent::ExecutionCancelled { execution });
            }
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
            }
        }
    }
}

pub(super) async fn handle_reap_run(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::ReapRun { run_id } = req else {
        unreachable!()
    };
    {
        // `bossctl agents reap` is the manual escape hatch for
        // orphans the engine startup probe missed (e.g. the
        // cube lease was still within its TTL on relaunch, so
        // the probe said "Live" even though the libghostty
        // pane is gone). Gate it `BossOnly`: this is a state
        // mutation that should not be reachable from a worker
        // pane subtree.
        if !server_state.authorize_rpc(RpcTier::BossOnly, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                run_id = %run_id,
                "reap_run rejected: caller not in Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "reap_run requires Boss authority".to_owned(),
                },
            );
            return;
        }
        let reason = "manual reap via bossctl agents reap";
        match server_state.work_db.mark_execution_orphaned(&run_id, reason) {
            Ok(execution) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    cube_workspace_id = ?execution.cube_workspace_id,
                    "reap_run: marked execution orphaned (workspace preserved)",
                );
                // Manual-reap termination path: tear down any
                // driver-owned state outside the workspace.
                crate::driver_teardown::teardown_driver_workspace(
                    &server_state.work_db,
                    &execution.id,
                    execution.workspace_path.as_deref().map(std::path::Path::new),
                    crate::driver_teardown::TeardownReason::ManualReap,
                )
                .await;
                // The execution row is now terminal, but marking it so
                // does nothing to the worker-pool claim or the
                // `LiveWorkerStateRegistry` entry a live pane may still
                // hold — unlike every other teardown path (`agents
                // stop`, completion, dead-pid/stale-worker sweeps),
                // which all route through `release_worker_pane`. Without
                // this, the claim leaks forever: `pool_claim_sweep`'s
                // live-state cross-check treats a still-present live
                // entry as "owned by a live pane's teardown path" and
                // skips it, so the one backstop designed to catch
                // leaked claims never fires for a reaped run. Route
                // through the same release path everything else uses —
                // it only tears down the pane/pool/live-state and never
                // touches the cube lease, so the workspace stays
                // preserved for re-lease exactly as documented above.
                // Backgrounded (mirrors `handle_stop_run`) so the app
                // round-trip and process-tree signal don't add latency
                // to the RPC response.
                let run_id_for_release = run_id.clone();
                tokio::spawn(async move {
                    server_state.release_worker_pane(&run_id_for_release).await;
                });
                send_response(&sink, &request_id, FrontendEvent::RunReaped { run_id, execution });
            }
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
            }
        }
    }
}

pub(super) async fn handle_tail_run_transcript(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::TailRunTranscript { run_id, lines } = req else {
        unreachable!()
    };
    {
        // `bossctl agents transcript` is a documented
        // coordinator verb. The earlier strict subtree-only
        // AppOrBoss check rejected the live coordinator when
        // it ran from a shell that descended from neither the
        // app nor the registered Boss session — see the
        // `authorize_rpc` AppOrBoss docstring for the
        // worker-exclusion fallback that fixed it. We still
        // reject worker descendants so one worker can't
        // tail another worker's transcript.
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                run_id = %run_id,
                "tail_run_transcript rejected: caller is a worker descendant",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "tail_run_transcript requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        match resolve_transcript_for_tail(&server_state, &run_id) {
            TranscriptResolution::Found { transcript_path } => {
                // The recorded `transcript_path` is interpreted on the
                // host that produced it. For a local run it is a path on
                // the engine's own filesystem; for a remote run it lives
                // on the remote host and must be pulled over SSH. Resolve
                // the run's host (the id may be a `run_*` or `exec_*`
                // namespace, so try both keys) and route accordingly so
                // `bossctl agents transcript` / the transcript viewer work
                // identically against a remote worker.
                let host = server_state
                    .work_db
                    .run_host(&run_id)
                    .ok()
                    .flatten()
                    .or_else(|| {
                        server_state
                            .work_db
                            .latest_run_host_for_execution(&run_id)
                            .ok()
                            .flatten()
                    })
                    .unwrap_or_else(|| "local".to_owned());
                let read_result: Result<(Vec<String>, bool), String> = if host == "local" {
                    read_transcript_tail(&transcript_path, lines)
                        .await
                        .map_err(|err| format!("transcript read failed for {transcript_path}: {err}"))
                } else {
                    match server_state
                        .execution_coordinator
                        .read_remote_transcript_tail(&host, &transcript_path, REMOTE_TRANSCRIPT_TAIL_BYTES)
                        .await
                    {
                        Ok(Some(content)) => Ok(tail_lines_from_content(&content, lines)),
                        // A remote host reporting `None` means "treat as
                        // local"; fall back to the local read rather than
                        // surface a spurious error.
                        Ok(None) => read_transcript_tail(&transcript_path, lines)
                            .await
                            .map_err(|err| format!("transcript read failed for {transcript_path}: {err}")),
                        Err(err) => Err(format!(
                            "remote transcript read failed for {transcript_path} on host {host}: {err:#}"
                        )),
                    }
                };
                match read_result {
                    Ok((lines_out, truncated)) => {
                        // `run_id` may be either namespace (see the host
                        // resolution above). A `run_*` id can never resolve
                        // as an execution id, so trying it first against
                        // `resolve_execution_driver_slug` always misses and
                        // logs a misleading "no driver slug resolves for
                        // this execution" before falling back — check the
                        // `run_*` shape first (mirroring the host
                        // resolution's own namespace-aware order above) and
                        // only try the id directly as an `exec_*` id
                        // otherwise. The client needs this to normalize
                        // `lines` through the producing driver's own dialect
                        // before rendering — see the `driver` field's doc
                        // comment on `RunTranscriptTail`.
                        let driver_slug = if let Ok(run) = server_state.work_db.get_run(&run_id) {
                            crate::driver_transcript::resolve_execution_driver_slug(
                                &server_state.work_db,
                                &run.execution_id,
                            )
                        } else {
                            crate::driver_transcript::resolve_execution_driver_slug(&server_state.work_db, &run_id)
                        };
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::RunTranscriptTail {
                                run_id,
                                transcript_path,
                                lines: lines_out,
                                truncated,
                                driver: driver_slug,
                            },
                        );
                    }
                    Err(message) => {
                        send_response(&sink, &request_id, FrontendEvent::WorkError { message });
                    }
                }
            }
            TranscriptResolution::Buffering => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!(
                            "{TRANSCRIPT_NOT_YET_AVAILABLE_PREFIX}{run_id}: engine has not yet received a hook event carrying transcript_path (retry in a few seconds)"
                        ),
                    },
                );
            }
            TranscriptResolution::NeverDispatched { execution_status } => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!(
                            "execution {run_id} was {execution_status} before dispatch — \
                             no worker ever ran for this execution so no transcript was recorded. \
                             This typically happens when a conflict-resolution or CI-fix revision \
                             attempt retires (resolves) before the scheduler dispatches the \
                             associated execution. Check the parent task's execution transcript instead."
                        ),
                    },
                );
            }
            TranscriptResolution::KnownNoTranscript => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("run {run_id} has no transcript path recorded"),
                    },
                );
            }
            TranscriptResolution::Unknown => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("unknown run: {run_id}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_execution_transcript(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::ExecutionTranscript { execution_id } = req else {
        unreachable!()
    };
    {
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "execution_transcript requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        let execution = match work_db.get_execution(&execution_id) {
            Ok(exec) => exec,
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
                return;
            }
        };
        let is_live = execution.finished_at.is_none() && execution.status.is_live();
        let transcript_path = match work_db.transcript_path_for_execution(&execution_id) {
            Ok(Some(path)) => path,
            Ok(None) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::ExecutionTranscriptUnavailable {
                        execution_id,
                        reason: "no transcript path recorded for this execution".to_owned(),
                    },
                );
                return;
            }
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("transcript lookup failed for {execution_id}: {err}"),
                    },
                );
                return;
            }
        };
        match tokio::fs::read_to_string(&transcript_path).await {
            Ok(content) => {
                // Same driver-aware path as Stop-boundary marker scans: a Codex
                // rollout is unreadable by the Claude dialect parser, so a raw
                // parse here would show the viewer an empty transcript for any
                // non-Claude run even when the worker spoke.
                let events = crate::driver_transcript::parse_execution_transcript(&work_db, &execution_id, &content);
                let segments = crate::transcript_markdown::events_to_segments(&events, &Default::default());
                let wire_segments: Vec<boss_protocol::TranscriptSegment> =
                    segments.into_iter().map(segment_to_wire).collect();
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::ExecutionTranscriptResult {
                        execution_id,
                        segments: wire_segments,
                        is_live,
                        complete: !is_live,
                    },
                );
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::ExecutionTranscriptUnavailable {
                        execution_id,
                        reason: format!("transcript file not found: {transcript_path}"),
                    },
                );
            }
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("transcript read failed for {transcript_path}: {err}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_list_engine_attempts(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListEngineAttempts {
        kinds,
        product_id,
        status,
        work_item_id,
        limit,
    } = req
    else {
        unreachable!()
    };
    let work_item_id = match work_item_id {
        Some(id) => match server_state.resolve_work_item_id(&id).await {
            Ok(id) => Some(id),
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
                return;
            }
        },
        None => None,
    };
    {
        match work_db.list_engine_attempts(&kinds, product_id.as_deref(), &status, work_item_id.as_deref(), limit) {
            Ok(attempts) => send_response(&sink, &request_id, FrontendEvent::EngineAttemptsList { attempts }),
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}
