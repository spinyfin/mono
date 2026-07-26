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

use crate::merge_poller::PrLifecycleState;

/// Engine-wide bound on `boss pr status --refresh` probes: at most
/// [`Self::MAX_PER_WINDOW`] live `gh pr view` calls per
/// [`Self::WINDOW_SECS`]-second **fixed** window, shared across every
/// worker in the pool, PLUS a [`Self::PER_EXECUTION_COOLDOWN_SECS`] minimum
/// spacing between two refreshes from the same execution. This is what
/// stops a refresh flag from becoming a second polling loop layered on top
/// of the merge poller's own: the per-execution cooldown stops any single
/// worker from burning the whole engine-wide budget by looping `--refresh`
/// itself, and the window cap bounds the fleet even if every worker plays
/// nicely. When either limit is hit, [`handle_get_pr_status`] returns the
/// stored snapshot with `refresh_throttled: true` rather than blocking or
/// erroring — see that function's doc comment.
#[derive(Default)]
pub(super) struct PrStatusRefreshBudget {
    window: StdMutex<RefreshWindow>,
}

#[derive(Default)]
struct RefreshWindow {
    window_start_secs: i64,
    count_in_window: u32,
    /// Last refresh timestamp per execution id, for the per-execution
    /// cooldown. Entries are never pruned: each is a few bytes and the
    /// number of executions an engine ever sees is bounded, so a background
    /// sweep isn't worth the complexity.
    last_refresh_by_execution: HashMap<String, i64>,
}

impl PrStatusRefreshBudget {
    /// Bucket width for the engine-wide cap. This is a **fixed** (tumbling)
    /// window, not a sliding one: `now_secs` is bucketed into
    /// `WINDOW_SECS`-second buckets and the counter resets at each bucket
    /// boundary, so a burst straddling a boundary can in principle land up
    /// to `2 * MAX_PER_WINDOW` probes within a short span. That looseness is
    /// accepted for a soft, best-effort cap — the per-execution cooldown is
    /// what actually bounds any single caller's contribution to a burst.
    const WINDOW_SECS: i64 = 60;
    /// `pub(super)`: exercised directly by `app::tests::pr_status`'s budget
    /// exhaustion test, so the test can burn exactly the real limit instead
    /// of a magic number that would silently drift from the real one.
    pub(super) const MAX_PER_WINDOW: u32 = 20;
    /// Minimum spacing, in seconds, between two `--refresh` probes issued by
    /// the SAME execution. `pub(super)` for the same reason as
    /// `MAX_PER_WINDOW`.
    pub(super) const PER_EXECUTION_COOLDOWN_SECS: i64 = 10;

    /// Try to claim one refresh slot for `execution_id` at `now_secs` (Unix
    /// epoch seconds). Checks the per-execution cooldown first (cheap, and
    /// it must not itself consume a window slot), then the engine-wide
    /// window. `false` means the caller must fall back to the stored
    /// snapshot.
    fn try_acquire(&self, execution_id: &str, now_secs: i64) -> bool {
        let mut window = self.window.lock().expect("pr status refresh budget mutex poisoned");
        if let Some(&last) = window.last_refresh_by_execution.get(execution_id)
            && now_secs - last < Self::PER_EXECUTION_COOLDOWN_SECS
        {
            return false;
        }
        if now_secs - window.window_start_secs >= Self::WINDOW_SECS {
            window.window_start_secs = now_secs;
            window.count_in_window = 0;
        }
        if window.count_in_window < Self::MAX_PER_WINDOW {
            window.count_in_window += 1;
            window
                .last_refresh_by_execution
                .insert(execution_id.to_owned(), now_secs);
            true
        } else {
            false
        }
    }
}

/// The effective bound PR for the calling execution, and the task id whose
/// `tasks` row carries the merge-poller-written PR-state columns
/// (`pr_mergeable_state`, `pr_merge_state_status`, `pr_head_sha`,
/// `pr_status_observed_at`) for it.
///
/// For an ordinary task/chore, the task's own bound `pr_url` answers both
/// questions. A `revision_implementation` execution's task never owns a PR
/// by design — `task.pr_url` is always NULL for `kind = 'revision'` — so
/// the bound PR, and the row the merge poller actually polls, belong to the
/// chain root instead. Mirrors the fallback chain already used by
/// `completion::execution_started` / `completion::metadata_gate`:
/// `task_bound_pr_url` -> `execution.pr_url` -> chain-root `pr_url`.
/// Without this, every revision worker (the largest population of `boss pr
/// status`/`boss pr body` callers) reads/writes a row that never has PR
/// state on it.
struct EffectivePr {
    status_task_id: String,
    pr_url: Option<String>,
}

fn resolve_effective_pr(work_db: &WorkDb, task: &Task, execution: &crate::work::WorkExecution) -> EffectivePr {
    if let Some(url) = crate::runner::task_bound_pr_url(task) {
        return EffectivePr {
            status_task_id: task.id.clone(),
            pr_url: Some(url.to_owned()),
        };
    }
    if execution.kind == ExecutionKind::RevisionImplementation {
        let pr_url = execution
            .pr_url
            .clone()
            .filter(|u| !u.is_empty())
            .or_else(|| work_db.get_revision_chain_root_pr_url(&task.id));
        if pr_url.is_some() {
            let status_task_id = work_db
                .get_revision_chain_root_task_id(&task.id)
                .unwrap_or_else(|| task.id.clone());
            return EffectivePr { status_task_id, pr_url };
        }
    }
    EffectivePr {
        status_task_id: task.id.clone(),
        pr_url: None,
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

    let task = match own_task(&work_db, &caller.work_item_id) {
        Ok(task) => task,
        Err(err) => {
            tracing::error!(work_item_id = %caller.work_item_id, ?err, "get_pr_status failed to read task");
            return send_rejection(
                &sink,
                &request_id,
                ProposalSubmissionError::new(ProposalErrorCode::Internal, format!("failed to read pr status: {err}")),
            );
        }
    };
    let execution = match work_db.get_execution(&caller.execution_id) {
        Ok(execution) => execution,
        Err(err) => {
            tracing::error!(execution_id = %caller.execution_id, ?err, "get_pr_status failed to read execution");
            return send_rejection(
                &sink,
                &request_id,
                ProposalSubmissionError::new(ProposalErrorCode::Internal, format!("failed to read pr status: {err}")),
            );
        }
    };
    let effective = resolve_effective_pr(&work_db, &task, &execution);

    let snapshot = match work_db.get_pr_status_snapshot(&effective.status_task_id) {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => {
            tracing::error!(
                work_item_id = %caller.work_item_id,
                status_task_id = %effective.status_task_id,
                "get_pr_status failed: resolved status task has no row",
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
    // The resolved PR URL is authoritative over the status-task row's own
    // `pr_url` column — normally identical, but the effective resolution
    // can know about a bound PR the row hasn't caught up to yet (e.g. right
    // after a revision's `execution.pr_url` was stamped, before the chain
    // root row itself was touched).
    let pr_url_for_display = effective.pr_url.clone().or_else(|| snapshot.pr_url.clone());

    // Nothing to refresh: no PR yet, or the caller didn't ask.
    let Some(pr_url) = effective.pr_url.clone().filter(|_| refresh) else {
        return send_response(
            &sink,
            &request_id,
            FrontendEvent::PrStatusResult {
                status: PrStatusView::builder()
                    .maybe_pr_url(pr_url_for_display)
                    .maybe_mergeable(snapshot.mergeable)
                    .maybe_merge_state_status(snapshot.merge_state_status)
                    .maybe_head_sha(snapshot.head_sha)
                    .maybe_observed_at(snapshot.observed_at)
                    .build(),
            },
        );
    };

    let now_secs = boss_engine_utils::epoch_time::now_epoch_secs();
    if !server_state
        .pr_status_refresh_budget
        .try_acquire(&caller.execution_id, now_secs)
    {
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

    // Only a genuinely `Open` probe carries mergeability worth persisting —
    // mirrors `merge_poller::sweep::update_pr_poll_state`'s own `Open`
    // guard. A merged/closed/404 probe must not clobber the stored
    // `pr_mergeable_state`/`pr_merge_state_status`/`pr_head_sha` with
    // "unknown"/NULL: the merge poller (which keeps probing terminal PRs
    // long enough to finalize them) is responsible for reconciling that
    // transition, not this bounded refresh path.
    let PrLifecycleState::Open(open) = &probe.state else {
        tracing::debug!(
            work_item_id = %caller.work_item_id,
            pr_url = %pr_url,
            "get_pr_status: refresh probe observed a non-open PR; returning stored snapshot without persisting",
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
    };

    let mergeable = crate::merge_poller::mergeable_state_str(open.mergeability);
    let probe_merge_state_status =
        (!probe.raw_merge_state_status.is_empty()).then_some(probe.raw_merge_state_status.as_str());
    let probe_head_sha = probe.head_ref_oid.as_deref();
    use crate::work::now_string;
    let observed_at = now_string();
    if let Err(err) = work_db.set_pr_status_observation(
        &effective.status_task_id,
        mergeable,
        probe_merge_state_status,
        probe_head_sha,
        &observed_at,
    ) {
        tracing::warn!(
            work_item_id = %caller.work_item_id,
            pr_url = %pr_url,
            ?err,
            "get_pr_status: failed to persist refresh observation (response still reflects the live probe)",
        );
    }

    // Mirror the write path's COALESCE: a probe response missing a field
    // must not report it as null to the caller when a known-good stored
    // value exists — otherwise a `--refresh` caller sees a blanked field
    // that a later plain `boss pr status` (reading the coalesced row back)
    // would not.
    let merge_state_status = probe_merge_state_status.or(snapshot.merge_state_status.as_deref());
    let head_sha = probe_head_sha.or(snapshot.head_sha.as_deref());

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
    let execution = match work_db.get_execution(&caller.execution_id) {
        Ok(execution) => execution,
        Err(err) => {
            tracing::error!(execution_id = %caller.execution_id, ?err, "get_pr_body failed to read execution");
            return send_rejection(
                &sink,
                &request_id,
                ProposalSubmissionError::new(ProposalErrorCode::Internal, format!("failed to read pr body: {err}")),
            );
        }
    };
    let effective = resolve_effective_pr(&work_db, &task, &execution);

    let title = match work_db.get_execution_pr_title_before(&caller.execution_id) {
        Ok(title) => title,
        Err(err) => {
            tracing::error!(
                execution_id = %caller.execution_id,
                ?err,
                "get_pr_body failed to read execution title snapshot",
            );
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
            body: PrBodyView::builder()
                .maybe_pr_url(effective.pr_url)
                .maybe_title(title)
                .maybe_body(body)
                .build(),
        },
    );
}
