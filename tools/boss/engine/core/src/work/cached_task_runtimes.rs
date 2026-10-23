use super::board_runtime_cache::{BoardRuntimeCache, CachedRuntime};
use super::dispatch_helpers::compute_task_runtime;
use super::*;

/// [`collect_task_runtimes`] backed by the engine-wide [`BoardRuntimeCache`].
/// Epochs are read in the same snapshot as the fallback computation, so a hit
/// is exactly the value the uncached path would compute from that snapshot;
/// only misses (changed, expired, never seen, or consulting other items) run
/// the point queries. The returned statement count covers the misses plus the
/// one epoch query, which keeps the population trace honest about what ran.
pub(crate) fn collect_task_runtimes_cached(
    conn: &Connection,
    product_id: &str,
    tasks: &[Task],
    chores: &[Task],
    cache: &BoardRuntimeCache,
    now: i64,
) -> Result<(Vec<TaskRuntime>, u64)> {
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
        // Hold reasons consult task, project, and repository state outside the
        // execution epoch. Recompute dispatch-pending terminal/absent runtimes.
        let hit = hit.filter(|runtime| {
            task.status != TaskStatus::Todo
                || !task.autostart
                || runtime
                    .execution_status
                    .as_ref()
                    .is_some_and(|status| !status.is_terminal())
        });
        if let Some(runtime) = hit {
            runtimes.push(runtime);
            continue;
        }
        let computed = compute_task_runtime(conn, id, Some(task), &mut queries, now)?;
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
