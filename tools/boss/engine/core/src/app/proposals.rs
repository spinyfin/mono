//! `FrontendRequest` handlers — the mediated worker→engine proposal API.
//!
//! Two verbs: `SubmitProposal` (write) and `ListProposals` (read). Both are
//! attributed the same way and both refuse the same way, so the attribution
//! step lives in one place ([`attribute_caller`]) rather than being spelled
//! twice with a chance of drifting.
//!
//! ## Attribution is derived, never declared
//!
//! The engine works out *which execution is proposing* from the socket
//! peer's pid, walked up the process tree to a registered worker run
//! ([`crate::worker_registry`]). The caller's `run_id` — its own
//! `BOSS_RUN_ID` — is a **cross-check, not a credential**: if it disagrees
//! with what the peer resolved to, the call is refused. So a worker cannot
//! file a proposal against another run's work item by passing a different
//! id, and a command copy-pasted between two worker panes fails loudly
//! instead of misattributing.
//!
//! Attribution **fails closed**. A connection with no local peer pid (a
//! remote SSH worker, per design §"Non-goals") or a peer whose ancestry
//! holds no registered worker run is refused with a typed error rather than
//! admitted on trust. The design's open question — "fail closed for writes,
//! open for reads… or strictly closed?" — resolves to strictly closed here,
//! for the reason it gives: a worker that cannot be attributed still has the
//! `[blocked]` bootstrap marker, so closing the door costs it nothing it
//! cannot route around, while opening it would let one worker read another's
//! work item.
//!
//! ## What this module does not do
//!
//! The apply pipeline itself is not here: `WorkDb::submit_worker_proposal`
//! (`crate::work::proposals`) runs it, inside the same transaction as the
//! insert, before this handler ever sees the returned row — see
//! `crate::work::proposal_apply`. Tier enforcement now exists: worker-classified
//! connections are gated by `worker_verb_decision` before dispatch when
//! `worker_rpc_tier` is on, and `SubmitProposal`/`ListProposals` are on the
//! worker allowlist. Their own peer-pid attribution is what additionally
//! confines a worker to its own work item, independently of the flag.
//!
//! Design: `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`
//! §"Transport and authn" / §"CLI surface".

use super::*;

use boss_engine_proposal_validation::{derive_idempotency_key, validate_caller_idempotency_key, validate_payload};
use boss_protocol::{ProposalErrorCode, ProposalSubmissionError};

use crate::work::{SubmitWorkerProposalInput, SubmitWorkerProposalOutcome};

/// The execution a proposal call was attributed to, plus the work item it
/// is thereby scoped to.
///
/// `pub(super)`: [`attribute_caller`] is reused by `app::context` for
/// `GetWorkerContext`, which is attributed identically and refuses the same
/// way (see that module's doc comment).
pub(super) struct AttributedCaller {
    pub(super) execution_id: String,
    pub(super) work_item_id: String,
}

/// Resolve the calling connection to a specific execution.
///
/// The chain is: local peer pid → registered worker run (== execution id) →
/// cross-check against the caller's `BOSS_RUN_ID` → execution row → work
/// item. Any break in that chain is a typed refusal naming which link
/// failed, because the remediations differ: a remote worker cannot fix
/// anything, a mismatched env var can be corrected, and a pruned execution
/// means the run is over.
pub(super) fn attribute_caller(
    server_state: &ServerState,
    work_db: &WorkDb,
    peer_pid: Option<libc::pid_t>,
    claimed_run_id: &str,
) -> std::result::Result<AttributedCaller, ProposalSubmissionError> {
    let Some(peer_pid) = peer_pid else {
        return Err(ProposalSubmissionError::new(
            ProposalErrorCode::NoLocalPeer,
            "this connection has no local socket peer, so the engine cannot verify which \
             execution is proposing. The proposal API is scoped to local workers in v1; a \
             remote (SSH) worker must use the `[blocked]` marker instead.",
        ));
    };

    let Some(resolved_run_id) = server_state.worker_registry.lookup_with_ancestor_walk(peer_pid) else {
        return Err(ProposalSubmissionError::new(
            ProposalErrorCode::AttributionUnresolved,
            format!(
                "no registered worker run was found in the process ancestry of peer pid \
                 {peer_pid}, so this call cannot be attributed to an execution. Proposals are \
                 accepted only from a live worker session."
            ),
        ));
    };

    // The cross-check. `BOSS_RUN_ID` never *grants* anything — it only has
    // the power to make a call fail — so a worker cannot reach another run's
    // work item by supplying its id.
    if claimed_run_id != resolved_run_id {
        return Err(ProposalSubmissionError::new(
            ProposalErrorCode::AttributionMismatch,
            format!(
                "BOSS_RUN_ID is `{claimed_run_id}` but this connection resolves to run \
                 `{resolved_run_id}`. Proposals are attributed from the socket peer, not from \
                 the supplied id — check that the command is running in its own worker session \
                 and that BOSS_RUN_ID matches it."
            ),
        ));
    }

    match work_db.work_item_for_execution(&resolved_run_id) {
        Ok(Some(work_item_id)) => Ok(AttributedCaller {
            execution_id: resolved_run_id,
            work_item_id,
        }),
        // The registry still holds a pid for an execution the DB no longer
        // has — a pruned row, or a stale entry for a run that ended. Not the
        // caller's fault and not fixable by it, so it gets its own code
        // rather than being folded into the attribution failures above.
        Ok(None) => Err(ProposalSubmissionError::new(
            ProposalErrorCode::UnknownExecution,
            format!(
                "this connection resolves to run `{resolved_run_id}`, but no such execution \
                 exists — the run may have been pruned."
            ),
        )),
        Err(err) => {
            tracing::warn!(
                run_id = %resolved_run_id,
                ?err,
                "proposal attribution: failed to read the execution row",
            );
            Err(ProposalSubmissionError::new(
                ProposalErrorCode::Internal,
                format!("failed to read execution `{resolved_run_id}`: {err}"),
            ))
        }
    }
}

/// `pub(super)`: reused by `app::context` to render an attribution failure
/// the same way `SubmitProposal`/`ListProposals` do.
pub(super) fn send_rejection(sink: &SessionSink, request_id: &str, error: ProposalSubmissionError) {
    send_response(sink, request_id, FrontendEvent::ProposalRejected { error });
}

pub(super) async fn handle_submit_proposal(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::SubmitProposal {
        run_id,
        kind,
        payload,
        idempotency_key,
    } = req
    else {
        unreachable!()
    };

    let caller = match attribute_caller(&server_state, &work_db, peer_pid, &run_id) {
        Ok(caller) => caller,
        Err(error) => {
            tracing::warn!(
                run_id = %run_id,
                peer_pid = ?peer_pid,
                code = %error.code,
                "submit_proposal rejected: attribution failed",
            );
            return send_rejection(&sink, &request_id, error);
        }
    };

    // Attribution first, then payload. A caller that cannot be attributed
    // has nothing to fix in its payload, and the derived idempotency key
    // needs the execution id anyway. A submission that is wrong on both
    // counts therefore costs two round trips — acceptable, since an
    // attribution failure means the session itself is misconfigured, which
    // is both rarer and more urgent than a typo'd field.
    let validated = match validate_payload(kind, &payload) {
        Ok(validated) => validated,
        Err(field_errors) => {
            let error = ProposalSubmissionError::validation(field_errors);
            tracing::debug!(
                execution_id = %caller.execution_id,
                kind = %kind,
                fields = ?error.field_errors.iter().map(|e| e.field.as_str()).collect::<Vec<_>>(),
                "submit_proposal rejected: payload validation failed",
            );
            return send_rejection(&sink, &request_id, error);
        }
    };

    // A caller that supplied no key gets the same key the CLI would have
    // derived, so an ad-hoc submission is replay-safe too. Blank is treated
    // as absent: an unset shell variable expands to an empty string, and
    // storing that would make every keyless submission from the run collide.
    let idempotency_key = idempotency_key
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty());

    let idempotency_key = match idempotency_key {
        Some(key) => match validate_caller_idempotency_key(&key) {
            Ok(()) => key,
            Err(field_error) => {
                let error = ProposalSubmissionError::validation(vec![field_error]);
                tracing::debug!(
                    execution_id = %caller.execution_id,
                    kind = %kind,
                    "submit_proposal rejected: idempotency_key invalid",
                );
                return send_rejection(&sink, &request_id, error);
            }
        },
        None => derive_idempotency_key(&caller.execution_id, kind, &validated.canonical_json),
    };

    let outcome = work_db.submit_worker_proposal(SubmitWorkerProposalInput {
        execution_id: &caller.execution_id,
        work_item_id: &caller.work_item_id,
        kind,
        payload_json: &validated.canonical_json,
        idempotency_key: &idempotency_key,
    });

    match outcome {
        Ok(Ok(SubmitWorkerProposalOutcome {
            proposal,
            already_submitted,
            staged_followup,
        })) => {
            tracing::info!(
                proposal_id = %proposal.id,
                execution_id = %caller.execution_id,
                work_item_id = %caller.work_item_id,
                kind = %kind,
                already_submitted,
                "worker proposal submitted",
            );
            // A freshly staged `followup_task` member is not yet visible
            // anywhere else — publish the same `AttentionCreated` event every
            // other attention-creating path publishes
            // (`app/attentions.rs:381`, `completion.rs`, `populator.rs`), so
            // the Notifications window renders the card live instead of
            // waiting for an unrelated refresh. Design: "no gated kind is
            // invisible while pending".
            if let Some((attention, group)) = staged_followup {
                let product_id = group.product_id.clone();
                server_state
                    .publisher
                    .publish_frontend_event_on_product(
                        &product_id,
                        FrontendEvent::AttentionCreated { attention, group },
                    )
                    .await;
            }
            // Mirror completion.rs's legacy marker-detector paths
            // (`file_worker_signal_attention` / `record_deferred_scope_item`):
            // both publish `AttentionItemCreated` on the work item's product
            // right after writing the row, which is what the macOS app's
            // deferred-scope badge and Notifications window key their
            // live-update off of. A fresh (not replayed) auto-applied
            // `attention`/`effort_escalation`/`blocked`/`deferred_scope`
            // proposal produces the exact same row through a different
            // write path, so it must publish the exact same event.
            if !already_submitted
                && proposal.state == boss_protocol::ProposalState::Applied
                && let Some(applied_ref) = proposal.applied_ref.as_deref()
                && applied_ref.starts_with("attn_")
            {
                match work_db.get_attention_item(applied_ref) {
                    Ok(item) => match work_db.get_work_item(&caller.work_item_id) {
                        Ok(work_item) => {
                            server_state
                                .publisher
                                .publish_frontend_event_on_product(
                                    work_item.product_id(),
                                    FrontendEvent::AttentionItemCreated { item },
                                )
                                .await;
                        }
                        Err(err) => {
                            tracing::warn!(
                                work_item_id = %caller.work_item_id,
                                ?err,
                                "submit_proposal: applied attention item created, but failed to read \
                                 its work item to publish AttentionItemCreated (non-fatal)",
                            );
                        }
                    },
                    Err(err) => {
                        tracing::warn!(
                            applied_ref,
                            ?err,
                            "submit_proposal: failed to read the just-applied attention item to publish \
                             AttentionItemCreated (non-fatal)",
                        );
                    }
                }
            }
            send_response(
                &sink,
                &request_id,
                FrontendEvent::ProposalSubmitted {
                    proposal,
                    already_submitted,
                },
            );
        }
        Ok(Err(refusal)) => {
            tracing::warn!(
                execution_id = %caller.execution_id,
                kind = %kind,
                "submit_proposal rejected: {}",
                refusal.message,
            );
            send_rejection(&sink, &request_id, refusal);
        }
        Err(err) => {
            tracing::error!(
                execution_id = %caller.execution_id,
                kind = %kind,
                ?err,
                "submit_proposal failed to persist",
            );
            send_rejection(
                &sink,
                &request_id,
                ProposalSubmissionError::new(
                    ProposalErrorCode::Internal,
                    format!("failed to persist the proposal: {err}"),
                ),
            );
        }
    }
}

pub(super) async fn handle_list_proposals(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::ListProposals { run_id, kind, state } = req else {
        unreachable!()
    };

    let caller = match attribute_caller(&server_state, &work_db, peer_pid, &run_id) {
        Ok(caller) => caller,
        Err(error) => {
            tracing::warn!(
                run_id = %run_id,
                peer_pid = ?peer_pid,
                code = %error.code,
                "list_proposals rejected: attribution failed",
            );
            return send_rejection(&sink, &request_id, error);
        }
    };

    match work_db.list_worker_proposals_for_work_item(&caller.work_item_id, kind, state) {
        Ok(proposals) => send_response(
            &sink,
            &request_id,
            FrontendEvent::ProposalsList {
                work_item_id: caller.work_item_id,
                proposals,
            },
        ),
        Err(err) => {
            tracing::error!(
                work_item_id = %caller.work_item_id,
                ?err,
                "list_proposals failed to read",
            );
            send_rejection(
                &sink,
                &request_id,
                ProposalSubmissionError::new(ProposalErrorCode::Internal, format!("failed to list proposals: {err}")),
            );
        }
    }
}
