use super::board_runtime_cache::{BoardRuntimeCache, CachedRuntime};
use super::db_diagnostics::StatementCounter;
use super::dispatch_helpers::{RuntimeCacheCost, RuntimePassCost, RuntimePhaseCost, compute_task_runtime};
use super::queued_wait::ReviewNaming;
use super::*;
use std::time::Instant;

/// [`collect_task_runtimes`] backed by the engine-wide [`BoardRuntimeCache`].
/// Epochs are read in the same snapshot as the fallback computation, so a hit
/// is exactly the value the uncached path would compute from that snapshot;
/// only misses (changed, expired, never seen, or consulting other items) run
/// execution point queries.
///
/// Task-derived wait annotation runs on hits too, because no epoch versions
/// the task row it reads. It names every card with nothing pending in one
/// set-based pass, so its statements do not grow with the board: a warm pass
/// over a settled board costs the epoch read plus that fixed handful.
///
/// The returned cost splits the pass by phase. Statements are counted as the
/// segment always has: the epoch read, the execution point queries each miss
/// reports, and every statement of the annotation, which one SQLite trace
/// hook counts for the whole pass rather than one per card.
pub(crate) fn collect_task_runtimes_cached(
    conn: &Connection,
    product_id: &str,
    tasks: &[Task],
    chores: &[Task],
    cache: &BoardRuntimeCache,
    now: i64,
    mut validity: Option<&mut HashMap<String, (bool, Option<i64>)>>,
) -> Result<(Vec<TaskRuntime>, RuntimePassCost)> {
    let started = Instant::now();
    let epochs = query_runtime_epochs(conn, product_id)?;
    let items: Vec<(&str, i64)> = tasks
        .iter()
        .chain(chores)
        .map(|task| (task.id.as_str(), epochs.get(&task.id).copied().unwrap_or(0)))
        .collect();
    let lookup = cache.lookup(product_id, &items, now);
    let hits = lookup.hits.iter().filter(|hit| hit.is_some()).count();
    let cache_cost = RuntimeCacheCost {
        hits,
        misses: items.len() - hits,
        statements: 1,
        millis: started.elapsed().as_secs_f64() * 1000.0,
    };

    let mut runtimes = Vec::with_capacity(items.len());
    let mut fresh = Vec::new();
    let mut computed = RuntimePhaseCost {
        cards: cache_cost.misses,
        ..RuntimePhaseCost::default()
    };
    for (((id, epoch), hit), expires_at) in items.iter().zip(lookup.hits).zip(lookup.expiries) {
        let (runtime, cacheable, expires_at) = match hit {
            Some(runtime) => (runtime, true, expires_at),
            None => {
                let miss = Instant::now();
                let result = compute_task_runtime(conn, id, &mut computed.statements, now, ReviewNaming::Deferred)?;
                computed.millis += miss.elapsed().as_secs_f64() * 1000.0;
                if result.cacheable {
                    fresh.push((
                        (*id).to_owned(),
                        CachedRuntime {
                            epoch: *epoch,
                            expires_at: result.expires_at,
                            runtime: result.runtime.clone(),
                        },
                    ));
                }
                (result.runtime, result.cacheable, result.expires_at)
            }
        };
        if let Some(validity) = validity.as_mut() {
            validity.insert((*id).to_owned(), (cacheable, expires_at));
        }
        runtimes.push(runtime);
    }

    // Applied to hits as well as misses: the cache holds the
    // execution-derived runtime, and why a card with no execution is
    // queued comes from its task row, which no epoch versions.
    let annotation = {
        let counter = StatementCounter::install(conn);
        super::queued_wait::annotate_unscheduled_many(
            conn,
            tasks.iter().chain(chores).zip(runtimes.iter_mut()),
            Some(&counter),
        )?
    };
    cache.complete(product_id, lookup.pass, fresh);
    let cost = RuntimePassCost {
        statements: cache_cost.statements + computed.statements + annotation.statements(),
        cache: Some(cache_cost),
        computed,
        annotation,
    };
    Ok((runtimes, cost))
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
