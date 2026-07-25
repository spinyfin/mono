//! `FrontendRequest::GetPrStatus` / `GetPrBody` handlers — `boss pr status`
//! / `boss pr body`, the bounded read-only PR-state verbs over state Boss
//! already stores, so a worker does not need `gh pr view` / `gh pr list
//! --head` just to answer "what's my own PR's mergeable/CI/body state."
//!
//! Attribution is identical to [`super::context`] / [`super::proposals`]:
//! reused via [`attribute_caller`], never re-implemented, so the three
//! verbs cannot drift on what "attribution failed" means.
//!
//! `GetPrStatus` is read-only against the merge poller's already-stored
//! snapshot by default; `refresh: true` allows exactly one bounded,
//! rate-limited on-demand probe per call — see [`PrStatusRefreshBudget`].
//! `GetPrBody` never talks to GitHub at all: it returns the execution-start
//! snapshot [`WorkDb::get_execution_pr_body_before`] already captured.
//!
//! Design: `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`
//! §"Read-only model access and the exposure boundary".

use super::context::own_task;
use super::proposals::{attribute_caller, send_rejection};
use super::*;

use boss_protocol::{PrBodyView, PrStatusView, ProposalErrorCode, ProposalSubmissionError};

/// Engine-wide bound on `boss pr status --refresh` probes: at most
/// [`Self::MAX_PER_WINDOW`] live `gh pr view` calls per
/// [`Self::WINDOW_SECS`]-second sliding window, shared across every worker
/// in the pool. This is what stops a refresh flag from becoming a second
/// polling loop layered on top of the merge poller's own — a worker fleet
/// that all pass `--refresh` still cannot out-poll this cap. When the
/// budget is exhausted, [`handle_get_pr_status`] returns the stored
/// snapshot with `refresh_throttled: true` rather than blocking or
/// erroring — see that function's doc comment.
#[derive(Default)]
pub(super) struct PrStatusRefreshBudget {
    window: StdMutex<RefreshWindow>,
}

#[derive(Default)]
struct RefreshWindow {
    window_start_secs: i64,
    count_in_window: u32,
}

impl PrStatusRefreshBudget {
    const WINDOW_SECS: i64 = 60;
    /// `pub(super)`: exercised directly by `app::tests::pr_status`'s budget
    /// exhaustion test, so the test can burn exactly the real limit instead
    /// of a magic number that would silently drift from the real one.
    pub(super) const MAX_PER_WINDOW: u32 = 20;

    /// Try to claim one refresh slot for `now_secs` (Unix epoch seconds).
    /// Rolls the window forward and resets the count when `now_secs` has
    /// moved past the current window; otherwise increments and admits
    /// while under [`Self::MAX_PER_WINDOW`]. `false` means the caller
    /// must fall back to the stored snapshot.
    fn try_acquire(&self, now_secs: i64) -> bool {
        let mut window = self.window.lock().expect("pr status refresh budget mutex poisoned");
        if now_secs - window.window_start_secs >= Self::WINDOW_SECS {
            window.window_start_secs = now_secs;
            window.count_in_window = 0;
        }
        if window.count_in_window < Self::MAX_PER_WINDOW {
            window.count_in_window += 1;
            true
        } else {
            false
        }
    }
}

/// Normalize GitHub's raw `mergeable` (`"MERGEABLE"` / `"CONFLICTING"` /
/// `"UNKNOWN"`, case-insensitive) into Boss's stored vocabulary — the same
/// three values [`crate::merge_poller::mergeable_state_str`] writes from
/// the merge poller's own probe, so a `--refresh` response and a stored
/// snapshot response are never spelled differently for the same state.
fn normalize_mergeable(raw: &str) -> &'static str {
    match raw.to_ascii_uppercase().as_str() {
        "MERGEABLE" => "mergeable",
        "CONFLICTING" => "conflicting",
        _ => "unknown",
    }
}

pub(super) async fn handle_get_pr_status(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::GetPrStatus { run_id, refresh } = req else {
        unreachable!()
    };

    let caller = match attribute_caller(&server_state, &work_db, peer_pid, &run_id) {
        Ok(caller) => caller,
        Err(error) => {
            tracing::warn!(
                run_id = %run_id,
                peer_pid = ?peer_pid,
                code = %error.code,
                "get_pr_status rejected: attribution failed",
            );
            return send_rejection(&sink, &request_id, error);
        }
    };

    let snapshot = match work_db.get_pr_status_snapshot(&caller.work_item_id) {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => {
            tracing::error!(
                work_item_id = %caller.work_item_id,
                "get_pr_status failed: attributed work item has no row",
            );
            return send_rejection(
                &sink,
                &request_id,
                ProposalSubmissionError::new(ProposalErrorCode::Internal, "attributed work item could not be read"),
            );
        }
        Err(err) => {
            tracing::error!(work_item_id = %caller.work_item_id, ?err, "get_pr_status failed to read");
            return send_rejection(
                &sink,
                &request_id,
                ProposalSubmissionError::new(ProposalErrorCode::Internal, format!("failed to read pr status: {err}")),
            );
        }
    };

    // Nothing to refresh: no PR yet, or the caller didn't ask.
    let Some(pr_url) = snapshot.pr_url.clone().filter(|_| refresh) else {
        return send_response(
            &sink,
            &request_id,
            FrontendEvent::PrStatusResult {
                status: PrStatusView::builder()
                    .maybe_pr_url(snapshot.pr_url)
                    .maybe_mergeable(snapshot.mergeable)
                    .maybe_merge_state_status(snapshot.merge_state_status)
                    .maybe_head_sha(snapshot.head_sha)
                    .maybe_observed_at(snapshot.observed_at)
                    .build(),
            },
        );
    };

    let now_secs = boss_engine_utils::epoch_time::now_epoch_secs();
    if !server_state.pr_status_refresh_budget.try_acquire(now_secs) {
        tracing::debug!(
            work_item_id = %caller.work_item_id,
            pr_url = %pr_url,
            "get_pr_status: refresh budget exhausted, returning stored snapshot",
        );
        return send_response(
            &sink,
            &request_id,
            FrontendEvent::PrStatusResult {
                status: PrStatusView::builder()
                    .pr_url(pr_url)
                    .maybe_mergeable(snapshot.mergeable)
                    .maybe_merge_state_status(snapshot.merge_state_status)
                    .maybe_head_sha(snapshot.head_sha)
                    .maybe_observed_at(snapshot.observed_at)
                    .refresh_throttled(true)
                    .build(),
            },
        );
    }

    let probe = match server_state.merge_probe.probe(&pr_url).await {
        Ok(probe) => probe,
        Err(err) => {
            tracing::warn!(
                work_item_id = %caller.work_item_id,
                pr_url = %pr_url,
                ?err,
                "get_pr_status: refresh probe failed, returning stored snapshot",
            );
            return send_response(
                &sink,
                &request_id,
                FrontendEvent::PrStatusResult {
                    status: PrStatusView::builder()
                        .pr_url(pr_url)
                        .maybe_mergeable(snapshot.mergeable)
                        .maybe_merge_state_status(snapshot.merge_state_status)
                        .maybe_head_sha(snapshot.head_sha)
                        .maybe_observed_at(snapshot.observed_at)
                        .build(),
                },
            );
        }
    };

    let mergeable = normalize_mergeable(&probe.raw_mergeable);
    let merge_state_status =
        (!probe.raw_merge_state_status.is_empty()).then_some(probe.raw_merge_state_status.as_str());
    let head_sha = probe.head_ref_oid.as_deref();
    use crate::work::now_string;
    let observed_at = now_string();
    if let Err(err) = work_db.set_pr_status_observation(
        &caller.work_item_id,
        mergeable,
        merge_state_status,
        head_sha,
        &observed_at,
    ) {
        tracing::warn!(
            work_item_id = %caller.work_item_id,
            pr_url = %pr_url,
            ?err,
            "get_pr_status: failed to persist refresh observation (response still reflects the live probe)",
        );
    }

    send_response(
        &sink,
        &request_id,
        FrontendEvent::PrStatusResult {
            status: PrStatusView::builder()
                .pr_url(pr_url)
                .mergeable(mergeable)
                .maybe_merge_state_status(merge_state_status)
                .maybe_head_sha(head_sha)
                .observed_at(observed_at)
                .refreshed(true)
                .build(),
        },
    );
}

pub(super) async fn handle_get_pr_body(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::GetPrBody { run_id } = req else {
        unreachable!()
    };

    let caller = match attribute_caller(&server_state, &work_db, peer_pid, &run_id) {
        Ok(caller) => caller,
        Err(error) => {
            tracing::warn!(
                run_id = %run_id,
                peer_pid = ?peer_pid,
                code = %error.code,
                "get_pr_body rejected: attribution failed",
            );
            return send_rejection(&sink, &request_id, error);
        }
    };

    let task = match own_task(&work_db, &caller.work_item_id) {
        Ok(task) => task,
        Err(err) => {
            tracing::error!(work_item_id = %caller.work_item_id, ?err, "get_pr_body failed to read task");
            return send_rejection(
                &sink,
                &request_id,
                ProposalSubmissionError::new(ProposalErrorCode::Internal, format!("failed to read pr body: {err}")),
            );
        }
    };

    let body = match work_db.get_execution_pr_body_before(&caller.execution_id) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(
                execution_id = %caller.execution_id,
                ?err,
                "get_pr_body failed to read execution snapshot",
            );
            return send_rejection(
                &sink,
                &request_id,
                ProposalSubmissionError::new(ProposalErrorCode::Internal, format!("failed to read pr body: {err}")),
            );
        }
    };

    send_response(
        &sink,
        &request_id,
        FrontendEvent::PrBodyResult {
            body: PrBodyView::builder().maybe_pr_url(task.pr_url).maybe_body(body).build(),
        },
    );
}
