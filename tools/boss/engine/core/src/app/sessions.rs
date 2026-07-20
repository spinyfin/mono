//! `FrontendRequest` handlers — app/boss session registration, engine responses, shutdown.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_register_app_session(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        session_id,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::RegisterAppSession = req else {
        unreachable!()
    };
    {
        // Trust the peer if any of:
        //   (a) it matches the declared app pid exactly. The
        //       engine reads `BOSS_APP_PID` at startup; the
        //       macOS app sets this before spawning the engine
        //       (necessary because `bazel run` daemonizes,
        //       which severs the engine's process tree from
        //       the app and breaks ancestor-walk auth).
        //   (b) the peer pid appears in the engine's ancestor
        //       chain (covers direct-launch scenarios like
        //       `swift run` where no daemonizing wrapper
        //       exists).
        //   (c) APP RESTART against a surviving engine: the
        //       trusted app pid belongs to a now-dead process
        //       and a fresh app instance is connecting. The
        //       engine correctly stays up on a same-version
        //       relaunch, so the relaunched app must be able to
        //       re-attach its session — otherwise the stale pid
        //       rejects `RegisterAppSession` forever, no
        //       `app_session` is registered, and every
        //       engine→app RPC (`SpawnWorkerPane`, reveal) dies
        //       silently. This is the mirror of T351 (engine
        //       restart re-attaching surviving panes): there the
        //       app survives and the engine restarts; here the
        //       engine survives and the app restarts. We require
        //       the old pid to be genuinely dead so a second
        //       live app can't hijack the trust root from the
        //       real one.
        let engine_pid = std::process::id() as libc::pid_t;
        let current_app_pid = server_state.current_app_pid();
        let trust_ok = register_app_session_trust_ok(current_app_pid, peer_pid, engine_pid);
        if !trust_ok {
            tracing::warn!(
                peer_pid = ?peer_pid,
                engine_pid,
                expected_app_pid = ?current_app_pid,
                "register_app_session rejected: peer pid neither matches BOSS_APP_PID nor is an engine ancestor",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "register_app_session: peer pid does not match app_pid".to_owned(),
                },
            );
            return;
        }
        // Re-pin the trust root to the (re)connecting app when it
        // differs from the stale pid. Keeps RPC authorization
        // (`SpawnWorkerPane`, BossOnly/AppOrBoss tiers) following
        // the live app across restarts. Only when a real trust
        // root was configured — test mode (`None`) stays
        // permissive so unit tests aren't pinned to a live pid.
        if let (Some(prior), Some(observed)) = (current_app_pid, peer_pid)
            && prior != observed
        {
            server_state.set_app_pid(observed);
            tracing::info!(
                prior_app_pid = prior,
                new_app_pid = observed,
                "app session re-attached: trust root re-pinned to relaunched app",
            );
            // The relaunched app killed every worker shell that was a
            // child of the prior (now-dead) app process, but their engine
            // slot bindings, pool claims, and DB execution rows survive.
            // Reconcile them now via the dead-PID probe: waiting for the
            // periodic dead-PID sweep would leave the slots bound and the
            // work items stuck "active" for up to a full sweep interval,
            // exactly the 2026-07-03 relaunch-orphan desync. Spawned
            // detached so the app's RegisterAppSession round-trip is not
            // blocked on the DB lookups + coordinator kicks the reconcile
            // performs. Only genuinely-dead PIDs are reaped, so a worker
            // that somehow outlived the relaunch is left untouched.
            let work_db = server_state.work_db.clone();
            let live_worker_states = server_state.live_worker_states.clone();
            let execution_coordinator = server_state.execution_coordinator.clone();
            let dispatch_events = server_state.dispatch_events.clone();
            tokio::spawn(async move {
                crate::dead_pid_sweep::reconcile_orphans_on_reattach(
                    work_db,
                    live_worker_states,
                    execution_coordinator,
                    dispatch_events,
                    prior,
                    observed,
                )
                .await;
            });
        }
        server_state
            .register_app_session(session_id.clone(), sink.clone())
            .await;
        tracing::info!(session_id = %session_id, "app session registered");
        send_response(&sink, &request_id, FrontendEvent::AppSessionRegistered);
        // A fresh app session is the operator's natural recovery action
        // (e.g. relaunching the app after waking the display) — clear the
        // spawn-capability breaker's failure window and any half-open probe
        // state left over from before, and auto-resume dispatch if it's
        // currently Breaker-paused. Never touches an operator pause:
        // `resume_dispatch_after_breaker_recovery` no-ops unless the
        // current pause is Breaker-origin.
        server_state.spawn_health.record_success();
        server_state.spawn_health.reset_probe();
        if crate::spawn_health::resume_dispatch_after_breaker_recovery(
            &server_state.work_db,
            &server_state.execution_coordinator,
            server_state.dispatch_events.as_ref(),
            None,
            "fresh app session registered",
        )
        .await
        {
            server_state.execution_coordinator.kick();
            server_state.broadcast_engine_health().await;
        }
        // Push pool sizes immediately after registration so the app's
        // WorkersWorkspaceModel can configure its slot ranges before the
        // engine dispatches any SpawnWorkerPane. This is the single source
        // of truth: the engine's runtime pool config drives the app's
        // capacity check, so they can never be independently out of sync.
        send_push(
            &sink,
            FrontendEvent::EnginePoolConfig {
                worker_slots: server_state.worker_pool_size,
                automation_slots: server_state.automation_pool_size,
                review_slots: server_state.review_pool_size,
                coordinator_model: server_state.coordinator_model.clone(),
            },
        );
    }
}

pub(super) async fn handle_register_boss_session(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RegisterBossSession { shell_pid } = req else {
        unreachable!()
    };
    {
        // Only the registered app session may install the
        // Boss trust root.
        let app_session_id = server_state
            .app_session
            .lock()
            .await
            .as_ref()
            .map(|h| h.session_id.clone());
        if app_session_id.as_deref() != Some(session_id.as_str()) {
            tracing::warn!(
                session_id = %session_id,
                "register_boss_session rejected: caller is not the app session",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "register_boss_session: only the app session may install the Boss trust root".to_owned(),
                },
            );
            return;
        }
        server_state.set_boss_pid(shell_pid as libc::pid_t);
        tracing::info!(boss_pid = shell_pid, "boss session registered as second trust root",);
        send_response(&sink, &request_id, FrontendEvent::BossSessionRegistered);
    }
}

/// Handle the app reporting the real shell pid for a worker pane.
///
/// The app returns `shell_pid = 0` from `SpawnWorkerPane` because the
/// libghostty surface is created asynchronously by SwiftUI after the RPC
/// returns. Once the surface attaches and the shell pid is available, the
/// app sends this message so the engine can wire process tracking.
///
/// Registers the pid in both `WorkerRegistry` (for ancestor-walk correlation
/// on hook events) and `LiveWorkerStateRegistry` (for dead-pid sweep and
/// `bossctl agents stop` reaping). Fire-and-forget: the app does not wait
/// for a response.
pub(super) async fn handle_update_worker_shell_pid(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state, peer_pid, ..
    } = ctx;
    let FrontendRequest::UpdateWorkerShellPid { run_id, shell_pid } = req else {
        unreachable!()
    };
    if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
        tracing::warn!(
            peer_pid = ?peer_pid,
            run_id = %run_id,
            "update_worker_shell_pid rejected: caller not in app/Boss subtree",
        );
        return;
    }
    if shell_pid <= 0 {
        tracing::warn!(
            run_id = %run_id,
            shell_pid,
            "update_worker_shell_pid: received non-positive pid; ignoring",
        );
        return;
    }
    // A real shell pid is proof the app's spawn path is working again — reset
    // the spawn-capability breaker so its failure window doesn't carry stale
    // pre-recovery failures into the next outage.
    server_state.spawn_health.record_success();
    // If this run was the half-open recovery probe's canary (see
    // `maybe_admit_recovery_probe`), this is proof the breaker's trip has
    // resolved — auto-resume dispatch. Never auto-resumes an operator pause:
    // `resume_dispatch_after_breaker_recovery` no-ops unless the current
    // pause is Breaker-origin.
    if server_state.spawn_health.record_probe_success(&run_id)
        && crate::spawn_health::resume_dispatch_after_breaker_recovery(
            &server_state.work_db,
            &server_state.execution_coordinator,
            server_state.dispatch_events.as_ref(),
            Some(&run_id),
            "recovery probe reported a real shell pid",
        )
        .await
    {
        server_state.execution_coordinator.kick();
        server_state.broadcast_engine_health().await;
    }
    // Persist the pid to the DB FIRST, keyed by run_id (the execution id).
    // The `work_runs` row always exists by now (inserted synchronously at
    // dispatch, before the pane was spawned), so unlike the in-memory slot
    // registration below this write can never lose to the concurrent-spawn
    // race — even when `update_shell_pid` reports "no live slot found", the
    // durable pid is recorded. This is the restart-robust signal
    // `dead_pane_sweep` probes to detect a pane that died with its host app.
    match server_state
        .work_db
        .set_run_shell_pid_for_execution(&run_id, shell_pid as i64)
    {
        Ok(true) => {}
        Ok(false) => tracing::debug!(
            run_id = %run_id,
            shell_pid,
            "update_worker_shell_pid: no work_runs row for run_id yet; durable pid not stored this pass",
        ),
        Err(err) => tracing::warn!(
            run_id = %run_id,
            shell_pid,
            ?err,
            "update_worker_shell_pid: failed to persist durable shell pid (pane-liveness may be blind after restart)",
        ),
    }
    // Update the pid→run_id registry so hook-event ancestor walk works.
    server_state.worker_registry.register(shell_pid, run_id.clone());
    // Update the live-state registry so dead-pid sweep and bossctl reaping
    // can signal the process when needed. A miss here (the concurrent-spawn
    // race where the app's pid push outran the engine's `register_spawn`, or a
    // late/duplicate report after the slot was released) only affects the
    // in-memory live registry — the durable pid persisted above is the
    // authoritative signal `dead_pane_sweep` reads, and it is never lost — so
    // the miss is logged for observability but is no longer a data-loss event.
    match server_state.live_worker_states.update_shell_pid(&run_id, shell_pid) {
        Some(slot_id) => {
            tracing::info!(
                run_id = %run_id,
                slot_id,
                shell_pid,
                "update_worker_shell_pid: registered real shell pid for worker pane",
            );
            server_state.broadcast_live_worker_states().await;
        }
        None => {
            tracing::warn!(
                run_id = %run_id,
                shell_pid,
                "update_worker_shell_pid: no live slot found for run_id (already released?); \
                 durable pid recorded, in-memory live-state not updated this pass",
            );
        }
    }
}

/// Handle the app reporting that a worker pane died — either its
/// libghostty surface never attached (`ghostty_surface_new` returned
/// NULL) or its child process exited with no app-side restart handler
/// for it (only the Boss pane restarts itself).
///
/// Reaps the backing execution immediately via
/// [`crate::dead_pid_sweep::reap_reported_pane_death`] instead of
/// waiting for the next periodic dead-PID sweep pass (up to 60s later)
/// or an app restart. Fire-and-forget: the app does not wait for a
/// response.
pub(super) async fn handle_worker_pane_died(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state, peer_pid, ..
    } = ctx;
    let FrontendRequest::WorkerPaneDied { run_id } = req else {
        unreachable!()
    };
    if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
        tracing::warn!(
            peer_pid = ?peer_pid,
            run_id = %run_id,
            "worker_pane_died rejected: caller not in app/Boss subtree",
        );
        return;
    }
    let reaped = crate::dead_pid_sweep::reap_reported_pane_death(
        server_state.work_db.as_ref(),
        server_state.live_worker_states.as_ref(),
        server_state.execution_coordinator.clone(),
        server_state.dispatch_events.as_ref(),
        &run_id,
        "app reported the worker pane died (surface failed to attach or child process exited)",
    )
    .await;
    if reaped {
        tracing::info!(run_id = %run_id, "worker_pane_died: execution reaped immediately");
    }
}

/// App reports that it can once again host worker panes after a
/// sleep/wake cycle (`GhosttyRuntime` confirmed an active display via
/// `NSWorkspace.didWakeNotification` / `screensDidWakeNotification`).
/// Kicks the scheduler immediately so anything stranded by the sleep —
/// an execution orphaned via `WorkerPaneDied`, or a `ready` row that
/// never got a slot while the app couldn't host a surface — redispatches
/// right away instead of waiting for the next periodic sweep or the
/// scheduler heartbeat.
pub(super) async fn handle_spawn_capability_restored(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state, peer_pid, ..
    } = ctx;
    let FrontendRequest::SpawnCapabilityRestored = req else {
        unreachable!()
    };
    if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
        tracing::warn!(
            peer_pid = ?peer_pid,
            "spawn_capability_restored rejected: caller not in app/Boss subtree",
        );
        return;
    }
    tracing::info!("spawn_capability_restored: kicking scheduler");
    server_state.execution_coordinator.kick();
}

/// Handle the app proactively reporting that a worker pane's shell never came
/// up — the `ReportWorkerSpawnFailed` NACK (see the wire-type docs).
///
/// This is the fast-fail path for the post-wake false-live spawn. The spawn
/// RPC was already answered `Ok(shell_pid: 0)` synchronously (the surface is
/// created asynchronously), so without this the engine would only learn the
/// shell never started after the 60s [`crate::spawn_ack_sweep`] grace window.
/// Here we reap the execution the instant the app tells us — the identical
/// teardown the sweep performs (orphan → pane release → slot release), routed
/// through the shared [`crate::spawn_ack_sweep::reap_never_started_spawn`] —
/// and feed the same spawn-capability circuit breaker, so a systemic outage is
/// caught in seconds instead of churning for hours. Fire-and-forget; no
/// response.
pub(super) async fn handle_report_worker_spawn_failed(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state, peer_pid, ..
    } = ctx;
    let FrontendRequest::ReportWorkerSpawnFailed { run_id, reason } = req else {
        unreachable!()
    };
    if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
        tracing::warn!(
            peer_pid = ?peer_pid,
            run_id = %run_id,
            "report_worker_spawn_failed rejected: caller not in app/Boss subtree",
        );
        return;
    }

    // Find the live slot this run was spawning into. A NACK for a run with no
    // live slot (already reaped by the 60s sweep, or released) — or one that
    // has since shown proof of life (a pid reported, or a hook event, or it
    // already progressed past `Spawning`) — is stale. Skip it so we never
    // double-reap or tear down a pane that actually came up.
    let Some(state) = server_state
        .live_worker_states
        .snapshot()
        .into_iter()
        .find(|s| s.run_id == run_id)
    else {
        tracing::info!(
            run_id = %run_id,
            reason = %reason,
            "report_worker_spawn_failed: no live slot for run (already reaped/released?); ignoring stale NACK",
        );
        return;
    };
    if state.shell_pid > 0 || state.last_event_at.is_some() || state.activity != boss_protocol::WorkerActivity::Spawning
    {
        tracing::info!(
            run_id = %run_id,
            slot_id = state.slot_id,
            shell_pid = state.shell_pid,
            activity = ?state.activity,
            "report_worker_spawn_failed: slot already showed proof of life or progressed; ignoring stale NACK",
        );
        return;
    }

    let execution = match server_state.work_db.get_execution(&run_id) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(
                run_id = %run_id,
                ?err,
                "report_worker_spawn_failed: failed to look up execution; ignoring",
            );
            return;
        }
    };
    if execution.status.is_terminal() {
        tracing::debug!(
            run_id = %run_id,
            "report_worker_spawn_failed: execution already terminal; ignoring",
        );
        return;
    }

    tracing::warn!(
        run_id = %run_id,
        slot_id = state.slot_id,
        reason = %reason,
        "app reported worker-pane spawn failure (no shell); reaping execution immediately",
    );

    let now_epoch_secs = boss_engine_utils::epoch_time::now_epoch_secs();
    let reap_ctx = crate::spawn_ack_sweep::SpawnReapCtx {
        work_db: server_state.work_db.as_ref(),
        coordinator: server_state.execution_coordinator.clone(),
        dispatch_events: server_state.dispatch_events.as_ref(),
        reaper: server_state.as_ref(),
        spawn_health: server_state.spawn_health.as_ref(),
    };
    crate::spawn_ack_sweep::reap_never_started_spawn(
        &reap_ctx,
        &execution,
        state.slot_id,
        state.shell_pid,
        crate::spawn_ack_sweep::ReapCause::AppNack { reason: &reason },
        now_epoch_secs,
    )
    .await;
}

pub(super) async fn handle_engine_response(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        session_id,
        ..
    } = ctx;
    let FrontendRequest::EngineResponse {
        request_id: response_request_id,
        response,
    } = req
    else {
        unreachable!()
    };
    {
        server_state
            .deliver_app_response(&session_id, &response_request_id, response)
            .await;
    }
}

pub(super) async fn handle_shutdown(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::Shutdown { token } = req else {
        unreachable!()
    };
    {
        // The token written to disk at startup is the auth
        // credential — there is no pid-based tier check on
        // purpose. The whole point of the token gate (issue
        // #705) is that "same user / same machine" doesn't
        // separate the legitimate caller (macOS app, boss CLI)
        // from the accidental caller (a `bazel test` that
        // resolved the production socket). The bazel sandbox
        // already denies access to `~/Library/Application
        // Support/`, so a test that lands here without the
        // file in scope will fail with `token_missing` rather
        // than killing a 9-hour-old engine.
        let outcome = match server_state.control_token.as_deref() {
            None => {
                // In-process serve() without a control token —
                // shouldn't happen for any process that has a
                // dialable frontend socket, but the dispatcher
                // is the wrong place to assume that. Reject
                // explicitly rather than panic.
                "token_missing"
            }
            Some(expected) => {
                if constant_time_eq(expected.as_bytes(), token.as_bytes()) {
                    "accepted"
                } else {
                    "token_mismatch"
                }
            }
        };
        crate::audit::record_shutdown_rpc(outcome, peer_pid);
        if outcome == "accepted" {
            tracing::info!(
                peer_pid = ?peer_pid,
                "shutdown rpc: token accepted — graceful exit pending",
            );
            send_response(&sink, &request_id, FrontendEvent::ShutdownAccepted);
            // Defer the actual notify so the writer task has a
            // chance to drain the ShutdownAccepted frame into
            // the kernel socket buffer before the accept loop
            // breaks. 50 ms is well under the shutdown_workers
            // grace window and well over the time it takes the
            // dispatcher to enqueue + the writer task to flush.
            let trigger = server_state.shutdown_trigger.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                trigger.notify_one();
            });
        } else {
            tracing::warn!(
                peer_pid = ?peer_pid,
                outcome,
                "shutdown rpc: rejected",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::ShutdownRejected {
                    reason: outcome.to_owned(),
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{make_session_sink, test_server_state};
    use super::*;
    use crate::test_support::{create_active_chore, create_product};

    /// Pins the `EnginePoolConfig.coordinator_model` push (computed in
    /// `handle_register_app_session` above) to `ServerState::coordinator_model`
    /// — sourced from `WorkConfig::coordinator_model`
    /// (`BOSS_COORDINATOR_MODEL`, default `"opus"`) — rather than the worker
    /// effort→model table. Per the 2026-07-20 model-economy directive the two
    /// are deliberately decoupled: a change to the worker dispatch table must
    /// never silently change what model the coordinator launches on.
    #[test]
    fn coordinator_model_defaults_to_opus_independent_of_effort_table() {
        let (server_state, _temp) = test_server_state();
        assert_eq!(server_state.coordinator_model, "opus");
    }

    fn dispatch_ctx(server_state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
        Dispatch::builder()
            .server_state(server_state.clone())
            .work_db(server_state.work_db.clone())
            .sink(sink.clone())
            .session_id("s1")
            .request_id("req-1")
            .recv_instant(std::time::Instant::now())
            .decode_ms(0.0)
            .build()
    }

    fn create_ready_execution(server_state: &Arc<ServerState>) -> String {
        let product_id = create_product(&server_state.work_db);
        let work_item_id = create_active_chore(&server_state.work_db, &product_id, "test chore");
        server_state
            .work_db
            .request_execution(
                boss_protocol::RequestExecutionInput::builder()
                    .work_item_id(work_item_id)
                    .build(),
            )
            .unwrap()
            .id
    }

    async fn call_nack(server_state: &Arc<ServerState>, run_id: &str) {
        let sink = make_session_sink();
        let ctx = dispatch_ctx(server_state, &sink);
        handle_report_worker_spawn_failed(
            ctx,
            FrontendRequest::ReportWorkerSpawnFailed {
                run_id: run_id.to_owned(),
                reason: "test-nack".to_owned(),
            },
        )
        .await;
    }

    /// Guard 1: no live slot at all for `run_id` (already reaped/released,
    /// or the app raced a NACK for a run the engine never registered) — the
    /// NACK must be a pure no-op, never touching the execution.
    #[tokio::test]
    async fn nack_ignored_when_no_live_slot() {
        let (server_state, _dir) = test_server_state();
        let execution_id = create_ready_execution(&server_state);

        call_nack(&server_state, &execution_id).await;

        assert_eq!(
            server_state.work_db.get_execution(&execution_id).unwrap().status,
            boss_protocol::ExecutionStatus::Ready,
            "no live slot must leave the execution untouched",
        );
    }

    /// Guard 2a: the slot already reported a real shell pid — proof the
    /// pane actually came up. Reaping here would tear down a live worker.
    #[tokio::test]
    async fn nack_ignored_when_shell_pid_already_reported() {
        let (server_state, _dir) = test_server_state();
        let execution_id = create_ready_execution(&server_state);
        server_state
            .live_worker_states
            .register_spawn(1, &execution_id, "claude-opus-4-7", 4242, None);

        call_nack(&server_state, &execution_id).await;

        assert_eq!(
            server_state.work_db.get_execution(&execution_id).unwrap().status,
            boss_protocol::ExecutionStatus::Ready,
            "a slot with a reported pid must not be reaped",
        );
        assert!(
            server_state.live_worker_states.get(1).is_some(),
            "the live slot must not be torn down",
        );
    }

    /// Guard 2b: the slot has seen a hook event (proof of life) even though
    /// it hasn't reported a pid or left `Spawning` yet.
    #[tokio::test]
    async fn nack_ignored_when_hook_event_already_seen() {
        let (server_state, _dir) = test_server_state();
        let execution_id = create_ready_execution(&server_state);
        server_state
            .live_worker_states
            .register_spawn(1, &execution_id, "claude-opus-4-7", 0, None);
        // Resume source is proof of life without flipping activity away
        // from Spawning, isolating this guard from guard 2c below.
        server_state.live_worker_states.apply_event(
            1,
            &boss_protocol::WorkerEvent::SessionStart {
                session_id: "s".to_owned(),
                source: boss_protocol::SessionStartSource::Resume,
            },
        );

        call_nack(&server_state, &execution_id).await;

        assert_eq!(
            server_state.work_db.get_execution(&execution_id).unwrap().status,
            boss_protocol::ExecutionStatus::Ready,
            "a slot with any hook event must not be reaped",
        );
        assert!(server_state.live_worker_states.get(1).is_some());
    }

    /// Guard 2c: the slot's activity already progressed past `Spawning`.
    #[tokio::test]
    async fn nack_ignored_when_activity_past_spawning() {
        let (server_state, _dir) = test_server_state();
        let execution_id = create_ready_execution(&server_state);
        server_state
            .live_worker_states
            .register_spawn(1, &execution_id, "claude-opus-4-7", 0, None);
        server_state.live_worker_states.apply_event(
            1,
            &boss_protocol::WorkerEvent::SessionStart {
                session_id: "s".to_owned(),
                source: boss_protocol::SessionStartSource::Startup,
            },
        );
        assert_ne!(
            server_state.live_worker_states.get(1).unwrap().activity,
            boss_protocol::WorkerActivity::Spawning,
            "precondition: Startup source must move activity off Spawning",
        );

        call_nack(&server_state, &execution_id).await;

        assert_eq!(
            server_state.work_db.get_execution(&execution_id).unwrap().status,
            boss_protocol::ExecutionStatus::Ready,
            "a slot that progressed past Spawning must not be reaped",
        );
        assert!(server_state.live_worker_states.get(1).is_some());
    }

    /// Guard 3: the execution is already terminal (e.g. a duplicate or
    /// very-late NACK arriving after some other path already finished the
    /// execution) — must never re-reap a terminal execution.
    #[tokio::test]
    async fn nack_ignored_when_execution_already_terminal() {
        let (server_state, _dir) = test_server_state();
        let execution_id = create_ready_execution(&server_state);
        server_state
            .live_worker_states
            .register_spawn(1, &execution_id, "claude-opus-4-7", 0, None);
        let work_item_id = server_state.work_db.get_execution(&execution_id).unwrap().work_item_id;
        server_state
            .work_db
            .force_execution_status_for_test(&work_item_id, boss_protocol::ExecutionStatus::Completed)
            .unwrap();

        call_nack(&server_state, &execution_id).await;

        assert_eq!(
            server_state.work_db.get_execution(&execution_id).unwrap().status,
            boss_protocol::ExecutionStatus::Completed,
            "an already-terminal execution must not be reaped again",
        );
        assert!(
            server_state.live_worker_states.get(1).is_some(),
            "the live slot must not be torn down for a stale NACK",
        );
    }
}
