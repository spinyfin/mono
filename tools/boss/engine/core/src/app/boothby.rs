//! `FrontendRequest` handlers — Boothby state, pass history, manual run, and
//! mode control (`boothby.md`, design task 3).
//!
//! Split out of `app.rs`, following the `automations.rs` precedent. See
//! [`super::Dispatch`] for the per-request context every handler receives.

use super::*;

/// `RunBoothbyPass` / `SetBoothbyMode` mutate Boothby's control state, so —
/// like the pane-control and probe RPCs — they require an app/Boss-descended
/// caller. `GetBoothbyState` / `ListBoothbyPasses` are read-only and
/// ungated, matching the automations listing RPCs.
fn require_app_or_boss(server_state: &ServerState, peer_pid: Option<libc::pid_t>) -> bool {
    server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid)
}

fn boothby_state_event(server_state: &ServerState, work_db: &WorkDb) -> Result<FrontendEvent, anyhow::Error> {
    let mode = server_state
        .settings
        .get_text(crate::boothby_scheduler::SETTING_MODE)
        .unwrap_or_else(|| crate::boothby_scheduler::BOOTHBY_MODE_PROPOSE.to_owned());
    let open_pass = work_db.get_open_boothby_pass()?;
    let last_pass = work_db.last_finished_boothby_pass()?;
    Ok(FrontendEvent::BoothbyState {
        mode,
        open_pass,
        last_pass,
    })
}

pub(super) async fn handle_get_boothby_state(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetBoothbyState = req else {
        unreachable!()
    };
    match boothby_state_event(&server_state, &work_db) {
        Ok(event) => send_response(&sink, &request_id, event),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_list_boothby_passes(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListBoothbyPasses { limit } = req else {
        unreachable!()
    };
    match work_db.list_boothby_passes(limit.unwrap_or(50)) {
        Ok(passes) => send_response(&sink, &request_id, FrontendEvent::BoothbyPassesList { passes }),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}

pub(super) async fn handle_run_boothby_pass(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::RunBoothbyPass = req else {
        unreachable!()
    };
    if !require_app_or_boss(&server_state, peer_pid) {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: "RunBoothbyPass requires app or Boss authority".to_owned(),
            },
        );
        return;
    }

    // Manual fire (`boss boothby run` / the tab's "Run now" button): bypasses
    // the schedule and `boothby.min_pass_gap_secs` — this is explicit human
    // intent — but opening still refuses a second concurrent pass via
    // `open_boothby_pass`'s single-flight check. Per `BoothbyPassStarted`'s
    // documented contract, this handler replies as soon as the pass opens
    // rather than waiting for it to finish — the run/finish remainder runs
    // in a spawned task, and its terminal state reaches clients via the
    // `boothby.activity` topic push `run_and_finish_opened_pass` performs.
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let pass_timeout = server_state
        .settings
        .get_text_i64(crate::boothby_scheduler::SETTING_PASS_TIMEOUT_SECS)
        .unwrap_or(900);
    let opened = crate::boothby_scheduler::open_and_announce_pass(
        &work_db,
        server_state.as_ref(),
        boss_protocol::BOOTHBY_TRIGGER_MANUAL,
        now,
    )
    .await;
    match opened {
        Some(pass) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::BoothbyPassStarted { pass: pass.clone() },
            );
            tokio::spawn(async move {
                crate::boothby_scheduler::run_and_finish_opened_pass(
                    &work_db,
                    &crate::boothby_scheduler::NothingToDoPassRunner,
                    server_state.as_ref(),
                    pass,
                    pass_timeout,
                )
                .await;
            });
        }
        None => send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: "a boothby pass is already in flight".to_owned(),
            },
        ),
    }
}

pub(super) async fn handle_set_boothby_mode(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::SetBoothbyMode { mode } = req else {
        unreachable!()
    };
    if !require_app_or_boss(&server_state, peer_pid) {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: "SetBoothbyMode requires app or Boss authority".to_owned(),
            },
        );
        return;
    }
    if !crate::boothby_scheduler::is_valid_boothby_mode(&mode) {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: format!("invalid boothby mode {mode:?}; expected off, propose, or auto"),
            },
        );
        return;
    }

    if let Err(err) = server_state
        .settings
        .set_text(crate::boothby_scheduler::SETTING_MODE, mode)
    {
        send_work_error(&sink, &request_id, &err);
        return;
    }
    // Wake the scheduler immediately so flipping off `off` doesn't wait out
    // whatever sleep the loop is currently in.
    server_state.boothby_kick.notify_one();

    match boothby_state_event(&server_state, &work_db) {
        Ok(event) => send_response(&sink, &request_id, event),
        Err(err) => send_work_error(&sink, &request_id, &err),
    }
}
