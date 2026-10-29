//! Database adapter for the incremental hub model. Journal and card reads share
//! one snapshot. Invalidation IDs never establish completeness; committed input
//! changes do, including writes by another connection before event publication.
use super::*;
use crate::population_timing::PopulationTrace;
use boss_board_projection::{Change, MAX_AFFECTED, MAX_CARDS, Projection};
use std::collections::BTreeSet;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refresh {
    Snapshot(&'static str),
    Incremental(usize),
}

pub(crate) struct CachedProjection {
    model: Projection,
    validity: HashMap<String, (bool, Option<i64>)>,
    pub(crate) retained_bytes: usize,
}

impl CachedProjection {
    pub(crate) fn tree(&self) -> &WorkTree {
        self.model.tree()
    }
}

const MAX_RETAINED_BYTES: usize = 32 * 1024 * 1024;

impl WorkDb {
    pub(crate) fn refresh_board_projection(
        &self,
        product: &str,
        projection: &mut Option<CachedProjection>,
        trace: &mut PopulationTrace,
        caches: &board_caches::BoardCaches,
        now: i64,
    ) -> Result<(WorkTree, Refresh)> {
        let mut connection = self.connect()?;
        let conn = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let (first, revision): (u64, u64) = conn.query_row(
            "SELECT COALESCE(MIN(sequence), 0), COALESCE(MAX(sequence), 0) FROM board_changes",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let reason = match projection.as_mut() {
            None => "cold",
            Some(model) if model.tree().product.id != product => "product",
            Some(model) if model.model.revision() > revision || model.model.revision() + 1 < first => "journal_gap",
            Some(model) => match incremental(&conn, model, revision, now)? {
                Ok(count) => {
                    let tree = model.tree().clone();
                    trace.set_items(tree.tasks.len() + tree.chores.len());
                    if !retainable(&tree, model.retained_bytes) {
                        *projection = None;
                    }
                    return Ok((tree, Refresh::Incremental(count)));
                }
                Err(reason) => reason,
            },
        };
        // Exactly one complete read, with no retry loop or partial publication.
        // It can run for minutes on a large board, so report its progress.
        trace.begin_cold_build();
        let mut validity = HashMap::new();
        let tree = Self::assemble_work_tree_in(&conn, product, trace, Some(caches), now, Some(&mut validity))?;
        // Bound retained state by both cardinality and encoded size. Oversized
        // products still work through the existing snapshot path.
        // Three bytes cover the changing comma counts in tasks, chores and
        // runtimes. Updating each record's size plus one then stays a
        // conservative bound (within six bytes) even across empty lanes.
        let retained_bytes = serde_json::to_vec(&tree)?.len() + 3;
        *projection = if retainable(&tree, retained_bytes) {
            Some(CachedProjection {
                model: Projection::snapshot(revision, tree.clone()),
                validity,
                retained_bytes,
            })
        } else {
            None
        };
        Ok((tree, Refresh::Snapshot(reason)))
    }
}

fn retainable(tree: &WorkTree, bytes: usize) -> bool {
    tree.tasks.len() + tree.chores.len() <= MAX_CARDS && bytes <= MAX_RETAINED_BYTES
}

fn record_bytes(record: &impl serde::Serialize) -> Result<usize> {
    Ok(serde_json::to_vec(record)?.len() + 1)
}

fn incremental(
    conn: &Connection,
    cached: &mut CachedProjection,
    revision: u64,
    now: i64,
) -> Result<std::result::Result<usize, &'static str>> {
    let model = &mut cached.model;
    let mut stmt = conn.prepare("SELECT item_id FROM board_changes WHERE sequence > ?1 ORDER BY sequence")?;
    let changes: Vec<Option<String>> = collect_rows(stmt.query_map([model.revision()], |row| row.get(0))?)?;
    if changes.iter().any(Option::is_none) {
        return Ok(Err("broad_input"));
    }
    let mut seeds: BTreeSet<_> = changes.into_iter().flatten().collect();
    // Cross-item reads and elapsed deadlines need refresh without a write.
    for (id, (cacheable, expires_at)) in &cached.validity {
        if !cacheable || expires_at.is_some_and(|at| at <= now) {
            seeds.insert(id.clone());
        }
    }
    // Union the old and new graphs to a fixed point, with a hard bound on
    // both recursive SQL output and in-memory expansion (cycles included).
    let affected = loop {
        let Ok(old) = model.affected(seeds.clone()) else {
            return Ok(Err("affected_limit"));
        };
        let mut stmt = conn.prepare(
            "WITH RECURSIVE affected(id) AS (
                 SELECT value FROM json_each(?1)
                 UNION SELECT t.parent_task_id FROM tasks t JOIN affected a ON t.id = a.id WHERE t.parent_task_id IS NOT NULL
                 UNION SELECT t.id FROM tasks t JOIN affected a ON t.parent_task_id = a.id
                 LIMIT ?2
             ) SELECT id FROM affected")?;
        let current: BTreeSet<String> = collect_rows(
            stmt.query_map(params![serde_json::to_string(&old)?, MAX_AFFECTED + 1], |row| {
                row.get(0)
            })?,
        )?
        .into_iter()
        .collect();
        if current.len() > MAX_AFFECTED {
            return Ok(Err("affected_limit"));
        }
        if current == seeds {
            break current;
        }
        seeds = current;
    };
    let product = &model.tree().product.id;
    let patch = board_patch::read_cards(conn, product, &affected, &[], now)?;
    let dependencies = collect_product_dependencies(conn, product)?;
    let mut removed_bytes = serde_json::to_vec(&model.tree().dependencies)?.len();
    for task in model.tree().tasks.iter().chain(&model.tree().chores) {
        if affected.contains(&task.id) {
            removed_bytes += record_bytes(task)?;
        }
    }
    for runtime in &model.tree().task_runtimes {
        if affected.contains(&runtime.work_item_id) {
            removed_bytes += record_bytes(runtime)?;
        }
    }
    let mut added_bytes = serde_json::to_vec(&dependencies)?.len();
    for entry in &patch.items {
        let task = match &entry.item {
            WorkItem::Task(task) | WorkItem::Chore(task) => task,
            _ => unreachable!("board patch contains only cards"),
        };
        added_bytes += record_bytes(task)? + record_bytes(&entry.runtime)?;
    }
    let count = patch.items.len();
    let (tasks, runtimes) = patch
        .items
        .into_iter()
        .map(|entry| {
            let task = match entry.item {
                WorkItem::Task(task) | WorkItem::Chore(task) => task,
                _ => unreachable!("board patch contains only cards"),
            };
            (task, entry.runtime)
        })
        .unzip();
    cached.validity.retain(|id, _| !affected.contains(id));
    cached.validity.extend(patch.validity);
    model
        .apply(Change {
            base: model.revision(),
            revision,
            affected,
            tasks,
            runtimes,
        })
        .expect("bounded change against the locked projection");
    model.set_dependencies(dependencies);
    cached.retained_bytes = cached.retained_bytes - removed_bytes + added_bytes;
    Ok(Ok(count))
}
