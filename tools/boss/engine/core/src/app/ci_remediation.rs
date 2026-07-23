//! `FrontendRequest` handlers — CI remediation attempts and budget.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_classify_ci_remediation(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ClassifyCiRemediation {
        attempt_id,
        triage_class,
    } = req
    else {
        unreachable!()
    };
    {
        // Worker-facing marker: stamp `triage_class` on a
        // `ci_remediations` row. Pure metadata column, no
        // authority gate — a forged attempt id has no row to
        // clobber.
        match work_db.set_ci_remediation_triage_class(&attempt_id, &triage_class) {
            Ok(Some(attempt)) => send_response(&sink, &request_id, FrontendEvent::CiRemediationClassified { attempt }),
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("ci_remediation attempt {attempt_id:?} is unknown",),
                },
            ),
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}

pub(super) async fn handle_mark_ci_remediation_failed(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::MarkCiRemediationFailed { attempt_id, reason } = req else {
        unreachable!()
    };
    {
        match work_db.mark_ci_remediation_failed(&attempt_id, &reason) {
            Ok(Some(attempt)) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    work_item_id = %attempt.work_item_id,
                    pr_url = %attempt.pr_url,
                    %reason,
                    "mark_ci_remediation_failed: attempt flipped to failed",
                );
                server_state
                    .publisher
                    .publish_frontend_event_on_product(
                        &attempt.product_id,
                        FrontendEvent::CiRemediationFailed {
                            product_id: attempt.product_id.clone(),
                            work_item_id: attempt.work_item_id.clone(),
                            attempt_id: attempt.id.clone(),
                            pr_url: attempt.pr_url.clone(),
                            failure_reason: reason.clone(),
                        },
                    )
                    .await;
                send_response(&sink, &request_id, FrontendEvent::CiRemediationMarkedFailed { attempt });
            }
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("ci_remediation attempt {attempt_id:?} is unknown or already terminal",),
                },
            ),
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}

pub(super) async fn handle_mark_ci_remediation_retriggered(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::MarkCiRemediationRetriggered { attempt_id, new_id } = req else {
        unreachable!()
    };
    {
        // The retrigger marker is the worker's "this is flaky/infra, I
        // re-ran the failing job, there is nothing to push" verdict. The
        // engine flips the attempt to the terminal `retriggered` status and
        // stamps the `ci_flaky_retriggered` signal on the parent. That
        // signal (a) surfaces a flake tag on the task card and (b) tells the
        // completion path to park the worker — awaiting the CI retry / a
        // human decision — instead of re-probing it for a diff that will
        // never exist (the stuck-loop bug). The merge-poller still observes
        // the re-run's outcome on its next sweep and clears the signal when
        // CI goes green.
        match work_db.mark_ci_remediation_retriggered(&attempt_id) {
            Ok(Some(attempt)) => {
                tracing::info!(
                    attempt_id = %attempt.id,
                    work_item_id = %attempt.work_item_id,
                    new_id = %new_id,
                    "mark_ci_remediation_retriggered: flaky/infra verdict recorded; parent parked awaiting CI retry",
                );
                server_state
                    .publisher
                    .publish_frontend_event_on_product(
                        &attempt.product_id,
                        FrontendEvent::CiRemediationFlakyRetriggered {
                            product_id: attempt.product_id.clone(),
                            work_item_id: attempt.work_item_id.clone(),
                            attempt_id: attempt.id.clone(),
                            pr_url: attempt.pr_url.clone(),
                            new_run_id: new_id.clone(),
                        },
                    )
                    .await;
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::CiRemediationRetriggered { attempt, new_id },
                );
            }
            // Already terminal (idempotent re-marker) or unknown id.
            // Distinguish the two so the worker's receipt is honest: echo
            // the existing row on a duplicate, error on a forged id.
            Ok(None) => match work_db.get_ci_remediation(&attempt_id) {
                Ok(Some(attempt)) => {
                    tracing::info!(
                        attempt_id = %attempt.id,
                        status = %attempt.status,
                        new_id = %new_id,
                        "mark_ci_remediation_retriggered: attempt already terminal; echoing receipt",
                    );
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::CiRemediationRetriggered { attempt, new_id },
                    );
                }
                Ok(None) => send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("ci_remediation attempt {attempt_id:?} is unknown"),
                    },
                ),
                Err(err) => send_work_error(&sink, &request_id, &err),
            },
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}

pub(super) async fn handle_mark_ci_remediation_succeeded_via_rebase(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::MarkCiRemediationSucceededViaRebase { attempt_id } = req else {
        unreachable!()
    };
    {
        // T2764 postmortem (PR spinyfin/mono#2023): a worker's "rebase
        // fixed it" claim used to be honored purely on say-so — the
        // engine flipped the attempt to succeeded, refunded budget, and
        // published `CiRemediationSucceeded` before CI had even started
        // on the pushed head. This verb now shares the exact
        // verify-at-call-time gate `boss engine ci mark-noop` uses: the
        // engine independently re-probes LIVE CI for the PR's CURRENT
        // head SHA and only honors the claim when every required check
        // is verified passing on that exact SHA. A premature or false
        // claim is rejected with the live status — no state change, no
        // event.

        // 1. Resolve the attempt. A forged id simply has no row.
        let attempt = match work_db.get_ci_remediation(&attempt_id) {
            Ok(Some(a)) => a,
            Ok(None) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("ci_remediation attempt {attempt_id:?} is unknown"),
                    },
                );
                return;
            }
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
                return;
            }
        };

        // 2. Idempotency: already succeeded — echo a HONORED receipt.
        //    The budget was refunded (if any) on the first, verified
        //    call; a repeat must not double-refund.
        if attempt.status == "succeeded" {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::CiRemediationSucceededViaRebase {
                    attempt,
                    budget_refunded: false,
                },
            );
            return;
        }
        // A terminal-but-not-succeeded attempt is not the worker's live,
        // actionable attempt. Reject with guidance rather than silently
        // touching an unrelated row.
        if attempt.status != "pending" && attempt.status != "running" {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!(
                        "ci_remediation attempt {attempt_id:?} is already terminal ({status}); nothing to validate.",
                        status = attempt.status,
                    ),
                },
            );
            return;
        }

        // 3. A queue-side failure (merge-queue rebounce or a Trunk queue
        //    eviction) is NOT validated by the PR's head-branch CI (which is
        //    always green in both cases — the failure was on a synthetic/
        //    ephemeral commit, not the PR head; see runner.rs's revision
        //    fragment). Honouring a rebase claim off a green head-branch
        //    probe would be a bypass: it would mark the attempt resolved on
        //    evidence that cannot speak to the failure — the same guard
        //    `mark_ci_remediation_noop` enforces. Reject; keep the row
        //    actionable.
        if crate::ci_watch::is_queue_side_failure_kind(attempt.failure_kind.as_deref()) {
            tracing::warn!(
                attempt_id = %attempt.id,
                work_item_id = %attempt.work_item_id,
                failure_kind = ?attempt.failure_kind,
                "mark_ci_remediation_succeeded_via_rebase: rejected — queue-side-failure attempt not validatable via head-branch CI",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::CiRemediationSucceededViaRebaseRejected {
                    attempt_id: attempt.id.clone(),
                    work_item_id: attempt.work_item_id.clone(),
                    pr_url: attempt.pr_url.clone(),
                    status: "This attempt's failure lives on a synthetic/ephemeral commit, not the PR head, so \
                             head-branch CI going green proves nothing about it. If post-rebase CI is green, do \
                             not retry this verb — push the fix and get the PR resubmitted to its queue (Trunk \
                             or GitHub's merge queue as applicable) and stop; the poller retires the attempt when \
                             the queue outcome is observed. If CI is still red, fix the semantic conflict and push."
                        .to_owned(),
                    live_sha: None,
                },
            );
            return;
        }

        // 4. THE VALIDATION GATE. Independently re-probe LIVE CI for the
        //    PR's CURRENT head SHA — the same `gh pr view …
        //    statusCheckRollup` source the merge-poller uses. We never
        //    trust the worker's assertion or a cached status. A probe
        //    failure means we cannot verify → reject (do not honor).
        let probe = match server_state.merge_probe.probe(&attempt.pr_url).await {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    pr_url = %attempt.pr_url,
                    ?err,
                    "mark_ci_remediation_succeeded_via_rebase: rejected — live CI probe failed; cannot verify green",
                );
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::CiRemediationSucceededViaRebaseRejected {
                        attempt_id: attempt.id.clone(),
                        work_item_id: attempt.work_item_id.clone(),
                        pr_url: attempt.pr_url.clone(),
                        status: format!("could not verify CI status (live probe failed): {err}"),
                        live_sha: None,
                    },
                );
                return;
            }
        };

        match crate::ci_watch::classify_noop_validation(&probe) {
            crate::ci_watch::NoopValidation::Green { head_sha } => {
                // Snapshot taken before the write so `budget_refunded`
                // reflects whether this attempt actually consumed a slot
                // (only fix-kind attempts with `consumes_budget = 1` get
                // a counter decrement).
                let budget_refunded = attempt.consumes_budget != 0;
                match work_db.mark_ci_remediation_succeeded_via_rebase(&attempt_id, head_sha.as_deref()) {
                    Ok(Some(updated)) => {
                        tracing::info!(
                            attempt_id = %updated.id,
                            work_item_id = %updated.work_item_id,
                            budget_refunded,
                            verified_sha = ?head_sha,
                            "mark_ci_remediation_succeeded_via_rebase: VERIFIED GREEN — rebase-only success recorded",
                        );
                        server_state
                            .publisher
                            .publish_frontend_event_on_product(
                                &updated.product_id,
                                FrontendEvent::CiRemediationSucceeded {
                                    product_id: updated.product_id.clone(),
                                    work_item_id: updated.work_item_id.clone(),
                                    attempt_id: updated.id.clone(),
                                    pr_url: updated.pr_url.clone(),
                                },
                            )
                            .await;
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::CiRemediationSucceededViaRebase {
                                attempt: updated,
                                budget_refunded,
                            },
                        );
                    }
                    // Raced to terminal between the lookup and the write
                    // (another path retired it — could be `mark-failed`,
                    // a merge-poller retire, or a duplicate winning call).
                    // Only echo a HONORED receipt if the row actually
                    // landed on `succeeded`; any other terminal status is
                    // a real rejection, not a success, so the CLI must
                    // exit non-zero rather than print a false receipt.
                    Ok(None) => match work_db.get_ci_remediation(&attempt_id) {
                        Ok(Some(current)) if current.status == "succeeded" => send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::CiRemediationSucceededViaRebase {
                                attempt: current,
                                budget_refunded: false,
                            },
                        ),
                        Ok(Some(current)) => send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: format!(
                                    "ci_remediation attempt {attempt_id:?} is already terminal ({status}); \
                                     nothing to validate.",
                                    status = current.status,
                                ),
                            },
                        ),
                        Ok(None) => send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: format!("ci_remediation attempt {attempt_id:?} vanished mid-retire"),
                            },
                        ),
                        Err(err) => send_work_error(&sink, &request_id, &err),
                    },
                    Err(err) => send_work_error(&sink, &request_id, &err),
                }
            }
            crate::ci_watch::NoopValidation::Rejected { head_sha, status } => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    work_item_id = %attempt.work_item_id,
                    pr_url = %attempt.pr_url,
                    live_sha = ?head_sha,
                    %status,
                    "mark_ci_remediation_succeeded_via_rebase: REJECTED — CI not verified green; row stays actionable",
                );
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::CiRemediationSucceededViaRebaseRejected {
                        attempt_id: attempt.id.clone(),
                        work_item_id: attempt.work_item_id.clone(),
                        pr_url: attempt.pr_url.clone(),
                        status,
                        live_sha: head_sha,
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_mark_ci_remediation_noop(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::MarkCiRemediationNoop {
        attempt_id,
        observed_sha,
        reason,
    } = req
    else {
        unreachable!()
    };
    {
        // The validated "there is no CI to fix" terminal signal. Unlike
        // the sibling `Mark*` verbs, the engine does NOT take the
        // worker's word for it: it independently re-probes LIVE CI for
        // the PR's CURRENT head SHA and only honors a verified-green
        // claim. A red/pending probe (or a head that moved) is rejected
        // with a receipt, leaving the row actionable so the worker
        // cannot escape a real failure.

        // 1. Resolve the attempt. A forged id simply has no row.
        let attempt = match work_db.get_ci_remediation(&attempt_id) {
            Ok(Some(a)) => a,
            Ok(None) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("ci_remediation attempt {attempt_id:?} is unknown"),
                    },
                );
                return;
            }
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
                return;
            }
        };

        // 2. Idempotency: an attempt already retired green echoes a
        //    HONORED receipt — the loop is already closed (e.g. the
        //    merge-poller's `on_ci_resolved` beat us to it, or this is a
        //    duplicate call).
        if attempt.status == "succeeded" {
            let validated_sha = attempt.head_sha_after.clone();
            send_response(
                &sink,
                &request_id,
                FrontendEvent::CiRemediationNoopValidated {
                    attempt,
                    validated_sha,
                    observed_sha,
                },
            );
            return;
        }
        // A terminal-but-not-succeeded attempt is not the worker's live,
        // actionable attempt. Reject with guidance rather than silently
        // touching an unrelated row.
        if attempt.status != "pending" && attempt.status != "running" {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!(
                        "ci_remediation attempt {attempt_id:?} is already terminal ({status}); nothing to validate. \
                         If CI is still failing on a fresh attempt, use `boss engine ci retry`.",
                        status = attempt.status,
                    ),
                },
            );
            return;
        }

        // 3. A queue-side failure (merge-queue rebounce or a Trunk queue
        //    eviction) is NOT validated by the PR's head-branch CI (which is
        //    always green in both cases — the failure was on a synthetic/
        //    ephemeral commit). Honouring a noop off a green head-branch
        //    probe would be a bypass: it would mark the attempt resolved on
        //    evidence that cannot speak to the failure. Reject; keep the row
        //    actionable.
        if crate::ci_watch::is_queue_side_failure_kind(attempt.failure_kind.as_deref()) {
            tracing::warn!(
                attempt_id = %attempt.id,
                work_item_id = %attempt.work_item_id,
                failure_kind = ?attempt.failure_kind,
                "mark_ci_remediation_noop: rejected — queue-side-failure attempt not validatable via head-branch CI",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::CiRemediationNoopRejected {
                    attempt_id: attempt.id.clone(),
                    work_item_id: attempt.work_item_id.clone(),
                    pr_url: attempt.pr_url.clone(),
                    status: "This attempt's failure lives on a synthetic/ephemeral commit, not the PR head, so \
                             head-branch CI cannot validate it. Fix the failure and get the PR resubmitted to its \
                             queue, or use `boss engine ci mark-failed`."
                        .to_owned(),
                    live_sha: None,
                    observed_sha,
                },
            );
            return;
        }

        // 4. THE VALIDATION GATE. Independently re-probe LIVE CI for the
        //    PR's CURRENT head SHA — the same `gh pr view …
        //    statusCheckRollup` source the merge-poller uses. We never
        //    trust the worker's assertion or a cached status. A probe
        //    failure means we cannot verify → reject (do not honor).
        let probe = match server_state.merge_probe.probe(&attempt.pr_url).await {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    pr_url = %attempt.pr_url,
                    ?err,
                    "mark_ci_remediation_noop: rejected — live CI probe failed; cannot verify green",
                );
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::CiRemediationNoopRejected {
                        attempt_id: attempt.id.clone(),
                        work_item_id: attempt.work_item_id.clone(),
                        pr_url: attempt.pr_url.clone(),
                        status: format!("could not verify CI status (live probe failed): {err}"),
                        live_sha: None,
                        observed_sha,
                    },
                );
                return;
            }
        };

        match crate::ci_watch::classify_noop_validation(&probe) {
            crate::ci_watch::NoopValidation::Green { head_sha } => {
                match work_db.mark_ci_remediation_validated_green(&attempt.id, head_sha.as_deref()) {
                    Ok(Some(succeeded)) => {
                        tracing::info!(
                            attempt_id = %succeeded.id,
                            work_item_id = %succeeded.work_item_id,
                            pr_url = %succeeded.pr_url,
                            validated_sha = ?head_sha,
                            observed_sha = ?observed_sha,
                            reason = %reason.as_deref().unwrap_or("already_green"),
                            "mark_ci_remediation_noop: VALIDATED GREEN — attempt retired, parent unblocked",
                        );
                        server_state
                            .publisher
                            .publish_frontend_event_on_product(
                                &succeeded.product_id,
                                FrontendEvent::CiRemediationSucceeded {
                                    product_id: succeeded.product_id.clone(),
                                    work_item_id: succeeded.work_item_id.clone(),
                                    attempt_id: succeeded.id.clone(),
                                    pr_url: succeeded.pr_url.clone(),
                                },
                            )
                            .await;
                        server_state
                            .publisher
                            .publish_work_item_changed(
                                &succeeded.product_id,
                                &succeeded.work_item_id,
                                "ci_validated_green_noop",
                            )
                            .await;
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::CiRemediationNoopValidated {
                                attempt: succeeded,
                                validated_sha: head_sha,
                                observed_sha,
                            },
                        );
                    }
                    // Raced to terminal between the lookup and the write
                    // (another path retired it — could be `mark-failed`, a
                    // merge-poller retire, or a duplicate winning call).
                    // Only echo a HONORED receipt if the row actually
                    // landed on `succeeded`; any other terminal status is a
                    // real rejection, not a success, so the CLI must exit
                    // non-zero rather than print a false receipt.
                    Ok(None) => match work_db.get_ci_remediation(&attempt.id) {
                        Ok(Some(current)) if current.status == "succeeded" => {
                            let validated_sha = current.head_sha_after.clone();
                            send_response(
                                &sink,
                                &request_id,
                                FrontendEvent::CiRemediationNoopValidated {
                                    attempt: current,
                                    validated_sha,
                                    observed_sha,
                                },
                            );
                        }
                        Ok(Some(current)) => send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: format!(
                                    "ci_remediation attempt {:?} is already terminal ({status}); \
                                     nothing to validate.",
                                    attempt.id,
                                    status = current.status,
                                ),
                            },
                        ),
                        Ok(None) => send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: format!("ci_remediation attempt {:?} vanished mid-retire", attempt.id),
                            },
                        ),
                        Err(err) => send_work_error(&sink, &request_id, &err),
                    },
                    Err(err) => send_work_error(&sink, &request_id, &err),
                }
            }
            crate::ci_watch::NoopValidation::Rejected { head_sha, status } => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    work_item_id = %attempt.work_item_id,
                    pr_url = %attempt.pr_url,
                    live_sha = ?head_sha,
                    observed_sha = ?observed_sha,
                    %status,
                    "mark_ci_remediation_noop: REJECTED — CI not verified green; row stays actionable",
                );
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::CiRemediationNoopRejected {
                        attempt_id: attempt.id.clone(),
                        work_item_id: attempt.work_item_id.clone(),
                        pr_url: attempt.pr_url.clone(),
                        status,
                        live_sha: head_sha,
                        observed_sha,
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_list_ci_remediations(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListCiRemediations {
        product_id,
        status,
        work_item_id,
        limit,
    } = req
    else {
        unreachable!()
    };
    {
        // Read-only listing surface for `boss engine ci list`
        // (design Phase 11 #35). Mirror of
        // `ListConflictResolutions`.
        match work_db.list_ci_remediations(product_id.as_deref(), &status, work_item_id.as_deref(), limit) {
            Ok(attempts) => send_response(&sink, &request_id, FrontendEvent::CiRemediationsList { attempts }),
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}

pub(super) async fn handle_get_ci_remediation(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetCiRemediation { attempt_id } = req else {
        unreachable!()
    };
    {
        match work_db.get_ci_remediation(&attempt_id) {
            Ok(Some(attempt)) => send_response(&sink, &request_id, FrontendEvent::CiRemediation { attempt }),
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("ci_remediation attempt {attempt_id:?} is unknown",),
                },
            ),
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}

pub(super) async fn handle_retry_ci_remediation(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RetryCiRemediation { selector } = req else {
        unreachable!()
    };
    {
        // The CLI accepts either a `ci_remediations` attempt id
        // or a work-item id (design Q11 "When invoked on an
        // attempt id, the engine resolves the attempt to its
        // work_item_id and acts on the parent."). Resolve the
        // selector before invoking the engine path so the
        // error messages stay grounded in what the caller
        // typed.
        let resolved: Result<Option<String>, anyhow::Error> = if selector.starts_with("cir_") {
            work_db
                .get_ci_remediation(&selector)
                .map(|opt| opt.map(|a| a.work_item_id))
        } else {
            Ok(Some(selector.clone()))
        };
        match resolved {
            Ok(Some(work_item_id)) => match work_db.retry_ci_remediation_for_work_item(&work_item_id) {
                Ok(Some((budget, was_exhausted))) => {
                    tracing::warn!(
                        %work_item_id,
                        was_exhausted,
                        "retry_ci_remediation: budget reset, parent unblocked={was_exhausted}",
                    );
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::CiRemediationRetryDone {
                            work_item_id,
                            budget,
                            was_exhausted,
                        },
                    );
                }
                Ok(None) => send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("work item {work_item_id:?} is unknown",),
                    },
                ),
                Err(err) => send_work_error(&sink, &request_id, &err),
            },
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("ci_remediation attempt {selector:?} is unknown",),
                },
            ),
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}

pub(super) async fn handle_abandon_ci_remediation(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::AbandonCiRemediation { attempt_id, reason } = req else {
        unreachable!()
    };
    {
        match work_db.mark_ci_remediation_abandoned(&attempt_id, &reason) {
            Ok(Some(attempt)) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    work_item_id = %attempt.work_item_id,
                    pr_url = %attempt.pr_url,
                    %reason,
                    "abandon_ci_remediation: attempt flipped to abandoned",
                );
                server_state
                    .publisher
                    .publish_frontend_event_on_product(
                        &attempt.product_id,
                        FrontendEvent::CiRemediationAbandoned {
                            product_id: attempt.product_id.clone(),
                            work_item_id: attempt.work_item_id.clone(),
                            attempt_id: attempt.id.clone(),
                            pr_url: attempt.pr_url.clone(),
                            failure_reason: reason.clone(),
                        },
                    )
                    .await;
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::CiRemediationMarkedAbandoned { attempt },
                );
            }
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("ci_remediation attempt {attempt_id:?} is unknown or already terminal",),
                },
            ),
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}

pub(super) async fn handle_get_ci_budget(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetCiBudget { work_item_id } = req else {
        unreachable!()
    };
    {
        match work_db.ci_budget_snapshot(&work_item_id) {
            Ok(Some(budget)) => send_response(&sink, &request_id, FrontendEvent::CiBudget { budget }),
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("work item {work_item_id:?} is unknown"),
                },
            ),
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}

pub(super) async fn handle_set_ci_budget(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetCiBudget { work_item_id, budget } = req else {
        unreachable!()
    };
    {
        match work_db.set_ci_attempt_budget(&work_item_id, budget) {
            Ok(Some(snapshot)) => {
                send_response(&sink, &request_id, FrontendEvent::CiBudgetUpdated { budget: snapshot })
            }
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("work item {work_item_id:?} is unknown"),
                },
            ),
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}
