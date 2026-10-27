use super::board_runtime_cache::{BoardRuntimeCache, CachedRuntime};
use super::dispatch_helpers::compute_task_runtime;
use super::*;

/// [`collect_task_runtimes`] backed by the engine-wide [`BoardRuntimeCache`].
/// Epochs are read in the same snapshot as the fallback computation, so a hit
/// is exactly the value the uncached path would compute from that snapshot;
/// misses recompute execution/run fields, while holds refresh on cache hits.
/// The returned statement count includes both projections and the epoch query.
pub(crate) fn collect_task_runtimes_cached(
    conn: &Connection,
    product_id: &str,
    tasks: &[Task],
    chores: &[Task],
    cache: &BoardRuntimeCache,
    now: i64,
) -> Result<(Vec<TaskRuntime>, u64)> {
    let chains = super::queued_wait::OrdinalChains::from_tasks(tasks.iter().chain(chores));
    let epochs = query_runtime_epochs(conn, product_id)?;
    let mut queries = 1u64;
    let items: Vec<(&str, i64)> = tasks
        .iter()
        .chain(chores)
        .map(|task| (task.id.as_str(), epochs.get(&task.id).copied().unwrap_or(0)))
        .collect();
    let lookup = cache.lookup(product_id, &items, now);
    let mut runtimes = Vec::with_capacity(items.len());
    let mut fresh = Vec::new();
    for (((id, epoch), hit), task) in items.iter().zip(lookup.hits).zip(tasks.iter().chain(chores)) {
        if let Some(mut runtime) = hit {
            // Hold reasons consult task, project and repository state outside
            // execution epochs. Refresh that projection while retaining the
            // epoch-validated execution/run fields.
            if runtime
                .execution_status
                .as_ref()
                .is_none_or(ExecutionStatus::is_terminal)
            {
                runtime.dispatch_hold_reason = if task.status == TaskStatus::Todo && task.autostart {
                    let execution = super::db_diagnostics::count_queries(&mut queries, || {
                        runtime
                            .execution_id
                            .as_deref()
                            .map(|id| query_execution(conn, id))
                            .transpose()
                    })?
                    .flatten();
                    dispatch_hold_reason_for(conn, Some(task), execution.as_ref(), &mut queries)?
                } else {
                    None
                };
            }
            runtimes.push(runtime);
            continue;
        }
        let computed = compute_task_runtime(conn, id, Some(task), Some(&chains), &mut queries, now)?;
        if computed.cacheable {
            fresh.push((
                (*id).to_owned(),
                CachedRuntime {
                    epoch: *epoch,
                    expires_at: computed.expires_at,
                    runtime: computed.runtime.clone(),
                },
            ));
        }
        runtimes.push(computed.runtime);
    }
    cache.complete(product_id, lookup.pass, fresh);
    Ok((runtimes, queries))
}

/// Current `board_runtime_epoch` for every item of `product_id` that has one;
/// an item with no row has never had a runtime input written (epoch 0).
fn query_runtime_epochs(conn: &Connection, product_id: &str) -> Result<HashMap<String, i64>> {
    let mut stmt = conn.prepare(
        "SELECT e.work_item_id, e.epoch
         FROM board_runtime_epoch e
         JOIN tasks t ON t.id = e.work_item_id
         WHERE t.product_id = ?1",
    )?;
    let rows = stmt.query_map([product_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    collect_rows(rows).map(|rows| rows.into_iter().collect())
}
