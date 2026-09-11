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
use boss_protocol::{ProposalErrorCode, ProposalKind, ProposalSubmissionError};

use crate::metrics::Registry;
use crate::work::{SubmitWorkerProposalInput, SubmitWorkerProposalOutcome};

// `SubmitProposal` counter families (design §"Seam migration map" /
// implementation task 13: "proposal counters registered in the metrics
// framework: submissions by kind, validation failures, rate-limit hits,
// fallback hits per seam"). The fourth family (fallback hits per seam)
// is declared per-seam as each seam migration lands (see
// `completion.rs`'s `worker_proposals.fallback_hit.*` counters); these
// three cover every `SubmitProposal` call regardless of which seam it
// belongs to.
crate::register_counter!(
    PROPOSAL_SUBMITTED_ATTENTION,
    "worker_proposals.submitted.attention",
    "SubmitProposal accepted a proposal of kind `attention`.",
);
crate::register_counter!(
    PROPOSAL_SUBMITTED_EFFORT_ESCALATION,
    "worker_proposals.submitted.effort_escalation",
    "SubmitProposal accepted a proposal of kind `effort_escalation`.",
);
crate::register_counter!(
    PROPOSAL_SUBMITTED_BLOCKED,
    "worker_proposals.submitted.blocked",
    "SubmitProposal accepted a proposal of kind `blocked`.",
);
crate::register_counter!(
    PROPOSAL_SUBMITTED_DEFERRED_SCOPE,
    "worker_proposals.submitted.deferred_scope",
    "SubmitProposal accepted a proposal of kind `deferred_scope`.",
);
crate::register_counter!(
    PROPOSAL_SUBMITTED_FOLLOWUP_TASK,
    "worker_proposals.submitted.followup_task",
    "SubmitProposal accepted a proposal of kind `followup_task`.",
);
crate::register_counter!(
    PROPOSAL_SUBMITTED_AUTOMATION_OUTCOME,
    "worker_proposals.submitted.automation_outcome",
    "SubmitProposal accepted a proposal of kind `automation_outcome`.",
);
crate::register_counter!(
    PROPOSAL_SUBMITTED_PR_CREATED,
    "worker_proposals.submitted.pr_created",
    "SubmitProposal accepted a proposal of kind `pr_created`.",
);
crate::register_counter!(
    PROPOSAL_SUBMITTED_REVIEW_REPORT,
    "worker_proposals.submitted.review_report",
    "SubmitProposal accepted a proposal of kind `review_report`.",
);
crate::register_counter!(
    PROPOSAL_SUBMITTED_REVIEW_VERDICT,
    "worker_proposals.submitted.review_verdict",
    "SubmitProposal accepted a proposal of kind `review_verdict`.",
);
crate::register_counter!(
    PROPOSAL_SUBMITTED_RUN_DONE,
    "worker_proposals.submitted.run_done",
    "SubmitProposal accepted a proposal of kind `run_done` — a worker declared its run finished.",
);
crate::register_counter!(
    PROPOSAL_VALIDATION_FAILED,
    "worker_proposals.validation_failed",
    "SubmitProposal rejected a submission for ProposalErrorCode::ValidationFailed (payload schema).",
);
crate::register_counter!(
    PROPOSAL_RATE_LIMITED,
    "worker_proposals.rate_limited",
    "SubmitProposal rejected a submission for ProposalErrorCode::RateLimited (per-execution cap exhausted).",
);

/// Register every `SubmitProposal` counter handle with `registry`. Called
/// from [`crate::metrics_init::init_all`] at engine startup.
pub fn register_metrics(registry: &Registry) {
    registry.register_counter(&PROPOSAL_SUBMITTED_ATTENTION);
    registry.register_counter(&PROPOSAL_SUBMITTED_EFFORT_ESCALATION);
    registry.register_counter(&PROPOSAL_SUBMITTED_BLOCKED);
    registry.register_counter(&PROPOSAL_SUBMITTED_DEFERRED_SCOPE);
    registry.register_counter(&PROPOSAL_SUBMITTED_FOLLOWUP_TASK);
    registry.register_counter(&PROPOSAL_SUBMITTED_AUTOMATION_OUTCOME);
    registry.register_counter(&PROPOSAL_SUBMITTED_PR_CREATED);
    registry.register_counter(&PROPOSAL_SUBMITTED_REVIEW_REPORT);
    registry.register_counter(&PROPOSAL_SUBMITTED_REVIEW_VERDICT);
    registry.register_counter(&PROPOSAL_SUBMITTED_RUN_DONE);
    registry.register_counter(&PROPOSAL_VALIDATION_FAILED);
    registry.register_counter(&PROPOSAL_RATE_LIMITED);
}

/// Increment the `worker_proposals.submitted.<kind>` counter for `kind`.
fn record_proposal_submitted(metrics: &Registry, kind: ProposalKind) {
    match kind {
        ProposalKind::Attention => PROPOSAL_SUBMITTED_ATTENTION.inc(metrics),
        ProposalKind::EffortEscalation => PROPOSAL_SUBMITTED_EFFORT_ESCALATION.inc(metrics),
        ProposalKind::Blocked => PROPOSAL_SUBMITTED_BLOCKED.inc(metrics),
        ProposalKind::DeferredScope => PROPOSAL_SUBMITTED_DEFERRED_SCOPE.inc(metrics),
        ProposalKind::FollowupTask => PROPOSAL_SUBMITTED_FOLLOWUP_TASK.inc(metrics),
        ProposalKind::AutomationOutcome => PROPOSAL_SUBMITTED_AUTOMATION_OUTCOME.inc(metrics),
        ProposalKind::PrCreated => PROPOSAL_SUBMITTED_PR_CREATED.inc(metrics),
        ProposalKind::ReviewReport => PROPOSAL_SUBMITTED_REVIEW_REPORT.inc(metrics),
        ProposalKind::ReviewVerdict => PROPOSAL_SUBMITTED_REVIEW_VERDICT.inc(metrics),
        ProposalKind::RunDone => PROPOSAL_SUBMITTED_RUN_DONE.inc(metrics),
    }
}

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
            PROPOSAL_VALIDATION_FAILED.inc(&server_state.metrics);
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
            review_batch_quorum_outcome,
        })) => {
            record_proposal_submitted(&server_state.metrics, kind);
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
            // This report/verdict acceptance dispatched the supervisor and
            // inserted its `Ready` execution inside the same transaction
            // that just committed, but that path has no publisher of its
            // own to kick the scheduler with — mirror
            // `finalize_passes.rs`'s member-failure dispatch, which does,
            // so the supervisor execution does not sit `Ready` until the
            // scheduler's own heartbeat happens to re-kick it.
            if review_batch_quorum_outcome == Some(crate::work::ReviewBatchQuorumOutcome::SupervisorDispatched) {
                server_state.publisher.kick_scheduler();
            }
            // A review report's or review verdict's acceptance is itself the
            // completion signal for its batch member (leaf or consolidating
            // supervisor alike): both mark the member `reported` synchronously
            // in the same submission transaction that already committed. Do
            // not wait for another, driver-specific turn boundary — that
            // boundary may never arrive after the worker has delivered its
            // report/verdict. The batch finalizer owns both the execution
            // terminal row and pane/lease teardown.
            //
            // No `!already_submitted` guard: the finalizer is idempotent (it
            // no-ops on an execution that is already terminal), so a replay —
            // including one that lands after a crash between apply and the
            // teardown below — must still reach it rather than silently
            // skipping the only path that releases the pane.
            let finalize_reporting_member = match kind {
                ProposalKind::ReviewReport => proposal.state == boss_protocol::ProposalState::Applied,
                // `ReviewVerdict` applies asynchronously (GitHub probes,
                // possible remediation creation), so its proposal is still
                // `Proposed` right after submission; it only reaches
                // `Applied` once the reconciler in the `tokio::spawn` below
                // (or a prior pass, on replay) finishes. Either state means
                // the member itself was already accepted as `reported`.
                ProposalKind::ReviewVerdict => matches!(
                    proposal.state,
                    boss_protocol::ProposalState::Proposed | boss_protocol::ProposalState::Applied
                ),
                _ => false,
            };
            // A `run_done` declaration's acceptance is itself the completion
            // signal for the WHOLE execution — not just a batch member, the
            // way a review report/verdict is — mirroring the block above for
            // exactly the reason its comment gives: do not wait for another,
            // driver-specific turn boundary that may never arrive after the
            // worker has delivered its declaration. Evidence checks stay in
            // place, but move off this path entirely (never a network call
            // here — see `completion::run_done_declaration`'s module doc).
            //
            // No `!already_submitted` guard, for the same reason
            // `finalize_reporting_member` has none: the finalize is
            // idempotent (every underlying write re-checks liveness), so a
            // replay — including one landing after a crash between apply and
            // the finalize call below — must still reach it rather than
            // silently leaving the slot held.
            let finalize_run_done_declaration =
                kind == ProposalKind::RunDone && proposal.state == boss_protocol::ProposalState::Applied;
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
            let apply_after_submit = !already_submitted
                && kind == ProposalKind::ReviewVerdict
                && proposal.state == boss_protocol::ProposalState::Proposed;
            let apply_proposal_id = apply_after_submit.then(|| proposal.id.clone());
            let response_delivery = super::handler_helpers::send_response_awaiting_delivery(
                &sink,
                &request_id,
                FrontendEvent::ProposalSubmitted {
                    proposal,
                    already_submitted,
                },
            );
            // Deliver the RPC acknowledgement before initiating teardown. The production
            // pane releaser reaps the reporting member's whole worker process
            // tree, which can include the very `boss propose` client still
            // waiting on the response above — tearing down first can kill
            // that client before it ever observes the ack it is blocked on.
            if finalize_reporting_member {
                match tokio::time::timeout(std::time::Duration::from_secs(10), response_delivery).await {
                    Ok(Ok(true)) => match server_state
                        .completion_handler
                        .finalize_accepted_review_batch_member(&caller.execution_id)
                        .await
                    {
                        Some(crate::completion::StopOutcome::ReviewPassCompleted { .. })
                        | Some(crate::completion::StopOutcome::AlreadyTerminal) => {}
                        Some(outcome) => tracing::error!(
                            execution_id = %caller.execution_id,
                            ?outcome,
                            "accepted review report/verdict did not cleanly finalize its batch member",
                        ),
                        None => tracing::error!(
                            execution_id = %caller.execution_id,
                            "accepted review report/verdict has no live batch member to finalize",
                        ),
                    },
                    Ok(Ok(false)) | Ok(Err(_)) => tracing::error!(
                        execution_id = %caller.execution_id,
                        "accepted review report/verdict response was not delivered; teardown skipped, \
                         the reported-plus-live sweep will raise an attention item",
                    ),
                    Err(_) => tracing::error!(
                        execution_id = %caller.execution_id,
                        "timed out waiting for accepted review report/verdict response delivery; teardown skipped, \
                         the reported-plus-live sweep will raise an attention item",
                    ),
                }
            } else if finalize_run_done_declaration {
                match serde_json::from_str::<boss_protocol::RunDoneProposalPayload>(&validated.canonical_json) {
                    Ok(run_done_payload) => {
                        match tokio::time::timeout(std::time::Duration::from_secs(10), response_delivery).await {
                            Ok(Ok(true)) => {
                                let stop_outcome = server_state
                                    .completion_handler
                                    .finalize_declared_run_done(&caller.execution_id, run_done_payload.outcome)
                                    .await;
                                tracing::info!(
                                    execution_id = %caller.execution_id,
                                    outcome = %run_done_payload.outcome,
                                    ?stop_outcome,
                                    "run_done proposal accepted: finalized synchronously at submit",
                                );
                            }
                            Ok(Ok(false)) | Ok(Err(_)) => tracing::error!(
                                execution_id = %caller.execution_id,
                                "accepted run_done proposal response was not delivered; finalize skipped — \
                                 the execution stays live for a later Stop or the merge poller to recover",
                            ),
                            Err(_) => tracing::error!(
                                execution_id = %caller.execution_id,
                                "timed out waiting for accepted run_done proposal response delivery; finalize \
                                 skipped — the execution stays live for a later Stop or the merge poller to \
                                 recover",
                            ),
                        }
                    }
                    Err(err) => tracing::error!(
                        execution_id = %caller.execution_id,
                        ?err,
                        "run_done proposal applied but its own canonical payload_json did not deserialize; \
                         finalize skipped — this should be unreachable since the payload already validated \
                         at submission",
                    ),
                }
            }
            // Apply after the worker has the `proposed` ack so GitHub probes
            // and remediation creation cannot stall the submission socket.
            // The periodic sweep is the crash-recovery path for the same work.
            if let Some(proposal_id) = apply_proposal_id {
                let work_db = Arc::clone(&work_db);
                let publisher = server_state.publisher.clone();
                tokio::spawn(async move {
                    let created = tokio::task::spawn_blocking(move || {
                        work_db.apply_review_verdict_proposal(&proposal_id, &crate::work::GhPrStateChecker)
                    })
                    .await;
                    match created {
                        Ok(Ok(Some(_))) => publisher.kick_scheduler(),
                        Ok(Ok(None)) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(?error, "submit_proposal: review-verdict apply failed; sweep will retry",)
                        }
                        Err(error) => tracing::warn!(
                            ?error,
                            "submit_proposal: review-verdict apply task joined with error; sweep will retry",
                        ),
                    }
                });
            }
        }
        Ok(Err(refusal)) => {
            if refusal.code == ProposalErrorCode::RateLimited {
                PROPOSAL_RATE_LIMITED.inc(&server_state.metrics);
            }
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
