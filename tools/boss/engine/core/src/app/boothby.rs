//! The `boothby.act` RPC — Boothby's only way to change anything.
//!
//! Thin by design. Every decision worth making lives in
//! [`crate::boothby::BoothbyExecutor`], because the executor is reachable
//! from tests and from the scheduler while an RPC handler is reachable only
//! over a socket; a rail implemented here would be a rail that nothing else
//! could exercise. This module authorizes the caller, hands the request over,
//! and puts the verdict on the wire.

use super::*;

pub(super) async fn handle_boothby_act(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::BoothbyAct { input } = req else {
        unreachable!()
    };

    // The design puts this verb on a `BoothbyOrApp` tier alongside the
    // transcript RPCs — a tier that does not exist yet, because it arrives
    // with the `boothby_pid` trust root in breakdown task 5 (session spawn +
    // privilege tier). `AppOrBoss` is the correct placeholder rather than a
    // convenient one: task 5's design has Boothby's own descendants passing
    // `AppOrBoss` anyway, so this is the tier the real caller will hold, and
    // it already excludes worker panes. It is looser in exactly one respect —
    // the coordinator can reach it, which `BoothbyOrApp` would forbid — and
    // that is inert today: no Boothby session exists to open a pass, and the
    // executor refuses every call without one.
    //
    // TODO(@brianduff,2026-12-31): tighten to `BoothbyOrApp` when the
    // Boothby session-spawn task lands the tier and the `boothby_pid` trust
    // root.
    if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
        tracing::warn!(
            peer_pid = ?peer_pid,
            verb = %input.verb,
            target_id = %input.target_id,
            "boothby_act rejected: caller not in app/Boss subtree",
        );
        send_response(
            &sink,
            &request_id,
            FrontendEvent::Error {
                message: "boothby_act requires app or Boss authority".to_owned(),
            },
        );
        return;
    }

    // Every action belongs to a pass — the journal's `pass_id` is NOT NULL,
    // and the mutation layer resolves the owning pass from the database
    // in-transaction. Resolving it here too would be a second source of
    // truth, so the executor is handed the same open pass the journal will
    // find. No open pass means nobody is running a Boothby pass, which makes
    // this call unattributable and therefore refused.
    let pass_id = match work_db.open_boothby_pass_id() {
        Ok(Some(pass_id)) => pass_id,
        Ok(None) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::BoothbyActed {
                    outcome: BoothbyActOutcome::Refused {
                        reason: "no Boothby pass is open; every action belongs to a pass".to_owned(),
                    },
                },
            );
            return;
        }
        Err(err) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("boothby_act: {err}"),
                },
            );
            return;
        }
    };

    match server_state.boothby_executor.act(&work_db, &pass_id, &input) {
        Ok(outcome) => {
            // Logged at info for every outcome, refusals included: the rails
            // firing is the interesting signal when Boothby seems idle, and a
            // refusal that left no trace anywhere would be invisible (only
            // executed actions reach the journal).
            tracing::info!(
                verb = %input.verb,
                target_id = %input.target_id,
                pass_id = %pass_id,
                outcome = ?outcome,
                "boothby: act",
            );
            send_response(&sink, &request_id, FrontendEvent::BoothbyActed { outcome });
        }
        Err(err) => {
            // Machinery broke, as opposed to a rail refusing. Distinct from
            // every outcome above, and deliberately an error on the wire so
            // Boothby cannot mistake it for a considered "no".
            tracing::error!(
                verb = %input.verb,
                target_id = %input.target_id,
                ?err,
                "boothby: act failed",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("boothby_act: {err:#}"),
                },
            );
        }
    }
}
