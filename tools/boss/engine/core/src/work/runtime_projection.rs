use super::dispatch_helpers::dispatch_retry_at_for_execution;
use super::*;

/// A freshly computed runtime plus what the board runtime cache needs to know
/// about how far it may be reused.
pub(crate) struct ComputedRuntime {
    pub runtime: TaskRuntime,
    /// False when the projection consulted another work item's state (see
    /// [`queued_wait::DispatchWait::reads_other_items`]); the item's own epoch
    /// does not version that, so the result must be recomputed every time.
    pub cacheable: bool,
    /// Epoch seconds from which the result changes without any write: the
    /// moment a future `dispatch_not_before` elapses. `None` when the result
    /// is not time-dependent.
    pub expires_at: Option<i64>,
}

/// The one implementation of the per-item runtime projection. `now` is read
/// once by the caller and used for every time comparison so the result and its
/// `expires_at` agree. The inputs it reads, and what versions each one for the
/// board runtime cache (`board_runtime_epoch`, schema 49):
/// - the item's `work_executions` rows: any insert, update or delete;
/// - the item's current and previous `work_runs` rows: insert, delete, or an
///   update of `status`, `execution_id`, `created_at` or `finished_at`;
/// - dispatch deadlines and relative display times against `now`: `expires_at`;
/// - dependency gating and chain-hold blockers: not versioned, `cacheable`
///   is false.
///
/// A new input must be added to one of those four mechanisms, or the cache
/// will serve it stale; `every_runtime_input_write_invalidates_exactly_its_item`
/// in `work/tests/board_runtime_cache_tests.rs` is the net that catches a
/// missed one, so a new runtime input needs a step added there.
pub(crate) fn compute_task_runtime(
    conn: &Connection,
    work_item_id: &str,
    queries: &mut u64,
    now: i64,
) -> Result<ComputedRuntime> {
    let latest = query_latest_execution_for_work_item(conn, work_item_id)?;
    *queries += 1;
    // `current_execution_id` (the operator-facing label for
    // `TaskRuntime.execution_id`) and the kanban card must follow the
    // execution a worker is actually attached to. A re-dispatch storm
    // leaves a newer *terminal* execution (the stalled duplicate)
    // shadowing the live run — keying off the plain latest row then
    // detaches the card from the live worker (R693 showed up idle under
    // "No Project" while La Forge was actively working it). When the
    // latest row is not itself live, prefer a live (running /
    // waiting_human) execution. Steady state — the latest row IS the
    // live run — skips the extra lookup.
    let latest_is_live = latest.as_ref().map(|e| e.status.is_live()).unwrap_or(false);
    let latest_is_abandoned = latest
        .as_ref()
        .map(|e| e.status == ExecutionStatus::Abandoned)
        .unwrap_or(false);
    let execution = if latest_is_live {
        latest
    } else {
        *queries += 1;
        match query_live_execution_for_work_item(conn, work_item_id)? {
            Some(live) => Some(live),
            // No live execution. If the latest row is an `abandoned`
            // redundant/superseded duplicate it never did the work and must
            // not shadow a genuinely-worked sibling — a successfully
            // completed revision would otherwise surface as `abandoned`
            // (broken) in `agents status`. Prefer the most recent
            // non-abandoned execution; keep `latest` only when every
            // execution for the item is abandoned. Steady state (latest is a
            // normal terminal / ready row) skips this extra lookup.
            None if latest_is_abandoned => {
                *queries += 1;
                query_last_non_abandoned_execution_for_work_item(conn, work_item_id)?.or(latest)
            }
            None => latest,
        }
    };
    let wait = super::queued_wait::for_execution(conn, execution.as_ref(), now)?;
    let mut display_expires_at = None;
    let queued_display =
        super::queued_display::for_execution(conn, execution.as_ref(), &wait, now, &mut display_expires_at)?;
    *queries += u64::from(queued_display.is_some());
    let (execution_status, run_status, execution_id, current_run_id, dispatch_retry_at, dispatch_wait_since) =
        if let Some(execution) = execution {
            *queries += 1;
            let latest_run = query_latest_run(conn, &execution.id)?;
            let (run_status, run_id) = match latest_run {
                Some((id, status)) => (Some(status), Some(id)),
                None => (None, None),
            };
            let dispatch_retry_at = dispatch_retry_at_for_execution(&execution, now);
            let dispatch_wait_since = if wait.reason.is_some()
                && wait.reason == execution.dispatch_wait_reason
                && execution.status == ExecutionStatus::Ready
            {
                execution.dispatch_wait_since.clone()
            } else {
                None
            };
            (
                Some(execution.status),
                run_status,
                Some(execution.id),
                run_id,
                dispatch_retry_at,
                dispatch_wait_since,
            )
        } else {
            (None, None, None, None, None, None)
        };
    // Both time-dependent fields are set only while their deadline is still in
    // the future at `now`, so the earlier of them is when the result changes.
    let expires_at = [&dispatch_retry_at, &wait.not_before]
        .into_iter()
        .filter_map(|deadline| deadline.as_deref()?.parse::<i64>().ok())
        .chain(display_expires_at)
        .min();
    let runtime = TaskRuntime::builder()
        .work_item_id(work_item_id)
        .maybe_execution_status(execution_status)
        .maybe_run_status(run_status)
        .maybe_execution_id(execution_id)
        .maybe_current_run_id(current_run_id)
        .maybe_dispatch_retry_at(dispatch_retry_at)
        .maybe_dispatch_wait_reason(wait.reason)
        .maybe_dispatch_wait_since(dispatch_wait_since)
        .maybe_dispatch_not_before(wait.not_before)
        .maybe_dispatch_wait_blocker(wait.blocker)
        .maybe_queued_display(queued_display)
        .build();
    Ok(ComputedRuntime {
        runtime,
        cacheable: !wait.reads_other_items,
        expires_at,
    })
}
