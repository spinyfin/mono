//! Storage and query helpers for `work_item_dependencies`.
//!
//! Edges are pure (`dependent_id`, `prerequisite_id`, `relation`)
//! triples; the table sits alongside `tasks` and `projects` and is
//! managed by `WorkDb` (see `work.rs`). This module provides the
//! lower-level SQL helpers; higher-level concerns (status mechanics,
//! dispatcher gating) live in the modules that call them.
//!
//! The functions here all take a `rusqlite::Connection` reference so
//! callers can compose them inside an in-flight transaction (cycle
//! check + insert, edge cleanup on prereq delete, etc.).

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use boss_protocol::WorkItemDependency;

/// The edge type that gates dispatch. Storage permits other values
/// for forward compatibility (`relates-to`, `duplicates`, …) but the
/// dispatcher only reads `blocks` rows: [`gating_prereqs_for`] and
/// `compute_gated_work_item_ids` filter strictly on this relation.
pub const RELATION_BLOCKS: &str = "blocks";

/// A **non-blocking** edge linking two otherwise-parallel siblings that
/// the Planner flagged as likely to co-edit the same files (the soft
/// `merge_order_hints` from a [`boss_protocol::PlannerOutput`]). Unlike
/// [`RELATION_BLOCKS`] it **never gates dispatch** — dispatch gating keys
/// strictly on `blocks` (see `dep_helpers::compute_gated_work_item_ids`
/// and [`gating_prereqs_for`], both of which pass `Some(RELATION_BLOCKS)`).
/// Its purpose is merge sequencing: the later PR of the pair forward-ports
/// preservingly (see the merge-conflict-reduction design, Layer 3 /
/// direction 2, and incident-002 postmortem P5).
pub const RELATION_MERGE_ORDER: &str = "merge_order";

/// Result of `insert_edge` — `Inserted` if a new row was added,
/// `AlreadyExists` if the call was an idempotent re-add (Q6: `add`
/// on an existing edge is a no-op success).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeInsertOutcome {
    Inserted,
    AlreadyExists,
}

/// Add a `(dependent_id, prerequisite_id, relation)` edge if it does
/// not already exist. Caller is responsible for:
///
/// - validating both ids resolve to live work items in the same
///   product (see `validate_edge_endpoints`),
/// - validating the new edge would not close a cycle (see
///   `would_create_cycle`).
///
/// This function only writes the row. It runs against any
/// `Connection` (engine-owned db, in-flight transaction, fresh
/// connection in a test) so the caller picks the isolation.
pub fn insert_edge(
    conn: &Connection,
    dependent_id: &str,
    prerequisite_id: &str,
    relation: &str,
    now_epoch: &str,
) -> Result<(WorkItemDependency, EdgeInsertOutcome)> {
    if dependent_id == prerequisite_id {
        bail!("dependency edge cannot point at itself: {dependent_id}");
    }
    let rows = conn.execute(
        "INSERT OR IGNORE INTO work_item_dependencies
            (dependent_id, prerequisite_id, relation, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![dependent_id, prerequisite_id, relation, now_epoch],
    )?;
    let outcome = if rows == 1 {
        EdgeInsertOutcome::Inserted
    } else {
        EdgeInsertOutcome::AlreadyExists
    };
    let edge = query_edge(conn, dependent_id, prerequisite_id, relation)?
        .with_context(|| format!("missing edge after insert: {dependent_id} → {prerequisite_id}"))?;
    Ok((edge, outcome))
}

/// Remove the named edge, if present. Returns `true` when a row was
/// actually deleted, `false` for a no-op delete (Q6: `rm` on a
/// missing edge is a success).
pub fn delete_edge(conn: &Connection, dependent_id: &str, prerequisite_id: &str, relation: &str) -> Result<bool> {
    let rows = conn.execute(
        "DELETE FROM work_item_dependencies
         WHERE dependent_id = ?1 AND prerequisite_id = ?2 AND relation = ?3",
        params![dependent_id, prerequisite_id, relation],
    )?;
    Ok(rows > 0)
}

/// All edges that name `work_item_id` as either endpoint. Useful
/// when a row is being deleted and the engine needs to cascade
/// edge cleanup (Q10: deleted prerequisite drops edges).
pub fn list_edges_touching(conn: &Connection, work_item_id: &str) -> Result<Vec<WorkItemDependency>> {
    let mut stmt = conn.prepare(
        "SELECT dependent_id, prerequisite_id, relation, created_at
         FROM work_item_dependencies
         WHERE dependent_id = ?1 OR prerequisite_id = ?1
         ORDER BY created_at ASC, dependent_id ASC, prerequisite_id ASC",
    )?;
    let rows = stmt.query_map([work_item_id], map_edge)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Edges whose `dependent_id` is `work_item_id` — i.e. the rows that
/// gate `work_item_id`. Only `blocks` rows are returned by default;
/// pass `None` to relation to get every relation type.
pub fn prerequisites_of(
    conn: &Connection,
    work_item_id: &str,
    relation: Option<&str>,
) -> Result<Vec<WorkItemDependency>> {
    edges_for(
        conn,
        "SELECT dependent_id, prerequisite_id, relation, created_at
         FROM work_item_dependencies
         WHERE dependent_id = ?1",
        work_item_id,
        relation,
        "prerequisite_id",
    )
}

/// Edges whose `prerequisite_id` is `work_item_id` — i.e. the rows
/// that depend on `work_item_id`.
pub fn dependents_of(conn: &Connection, work_item_id: &str, relation: Option<&str>) -> Result<Vec<WorkItemDependency>> {
    edges_for(
        conn,
        "SELECT dependent_id, prerequisite_id, relation, created_at
         FROM work_item_dependencies
         WHERE prerequisite_id = ?1",
        work_item_id,
        relation,
        "dependent_id",
    )
}

fn edges_for(
    conn: &Connection,
    base_sql: &str,
    work_item_id: &str,
    relation: Option<&str>,
    order_by_id_column: &str,
) -> Result<Vec<WorkItemDependency>> {
    let mut out = Vec::new();
    if let Some(rel) = relation {
        let sql = format!("{base_sql} AND relation = ?2 ORDER BY created_at ASC, {order_by_id_column} ASC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![work_item_id, rel], map_edge)?;
        for row in rows {
            out.push(row?);
        }
    } else {
        let sql = format!("{base_sql} ORDER BY created_at ASC, {order_by_id_column} ASC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([work_item_id], map_edge)?;
        for row in rows {
            out.push(row?);
        }
    }
    Ok(out)
}

/// Look up a single edge by its primary key.
pub fn query_edge(
    conn: &Connection,
    dependent_id: &str,
    prerequisite_id: &str,
    relation: &str,
) -> Result<Option<WorkItemDependency>> {
    conn.query_row(
        "SELECT dependent_id, prerequisite_id, relation, created_at
         FROM work_item_dependencies
         WHERE dependent_id = ?1 AND prerequisite_id = ?2 AND relation = ?3",
        params![dependent_id, prerequisite_id, relation],
        map_edge,
    )
    .optional()
    .map_err(Into::into)
}

/// True if inserting a **`blocks`** `(dependent_id → prerequisite_id)`
/// edge would close a cycle. Walks the existing edge graph forward from
/// `prerequisite_id`; if `dependent_id` is reachable, the new edge
/// would form a cycle. Designed to run inside the same transaction
/// as the upcoming insert so a concurrent writer sees the proposed
/// row before adding its own.
///
/// The walk is **scoped to `relation='blocks'`** on both legs: a cycle
/// only matters for the dispatch-gating graph, which is `blocks`-only.
/// A non-blocking `merge_order` edge must never be able to manufacture a
/// false cycle that rejects a legitimate `blocks` prerequisite — e.g. a
/// `merge_order` A→B plus a later `blocks` B→A is perfectly valid and
/// must be allowed (the two relations are independent graphs).
pub fn would_create_cycle(conn: &Connection, dependent_id: &str, prerequisite_id: &str) -> Result<bool> {
    if dependent_id == prerequisite_id {
        return Ok(true);
    }
    // The proposed edge says: `prerequisite_id → dependent_id` is
    // already implied (they share a future ordering). We walk forward
    // from `prerequisite_id` (i.e. its own prerequisites and their
    // prerequisites, recursively); if we ever reach `dependent_id`,
    // the new edge would close a loop.
    let exists: Option<i64> = conn
        .query_row(
            "WITH RECURSIVE forward(id) AS (
                SELECT prerequisite_id
                FROM work_item_dependencies
                WHERE dependent_id = ?1 AND relation = 'blocks'
              UNION
                SELECT d.prerequisite_id
                FROM work_item_dependencies d
                JOIN forward f ON d.dependent_id = f.id
                WHERE d.relation = 'blocks'
            )
            SELECT 1 FROM forward WHERE id = ?2 LIMIT 1",
            params![prerequisite_id, dependent_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(exists.is_some())
}

fn map_edge(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkItemDependency> {
    Ok(WorkItemDependency {
        dependent_id: row.get(0)?,
        prerequisite_id: row.get(1)?,
        relation: row.get(2)?,
        created_at: row.get(3)?,
    })
}

/// Whether `status` counts as a "satisfied" prerequisite for the
/// dependency rule (Q4 / Q10). Tasks and chores satisfy on `done`;
/// projects also satisfy on `archived` (a wound-down project should
/// not perpetually gate downstream work). The function is on `id`
/// prefix because `tasks.kind` is not visible to callers that walk
/// the edge table.
///
/// This is the default (non-revision-aware) form. For a
/// revision-specific gate check, use
/// [`status_satisfies_for_dependent`] instead.
pub fn status_satisfies(work_item_id: &str, status: &str) -> bool {
    if work_item_id.starts_with("proj_") {
        matches!(status, "done" | "archived")
    } else {
        status == "done"
    }
}

/// Whether `prereq_status` counts as a satisfied prerequisite when
/// the *dependent* has kind `dependent_kind`.
///
/// The rule is revision-specific: a `revision` task becomes runnable
/// exactly when its prerequisite (the parent task or a preceding
/// revision) reaches `in_review` — the PR is open, work is
/// quiescent, and the revision's job is to add a commit to that PR.
/// Waiting for `done` (merged) is a hard deadlock because by the
/// time the PR merges the revision can no longer push to it.
///
/// For all non-revision dependents the standard rules apply: task
/// prereqs satisfy on `done`; project prereqs satisfy on
/// `done`/`archived`.
pub fn status_satisfies_for_dependent(prereq_id: &str, prereq_status: &str, dependent_kind: Option<&str>) -> bool {
    if dependent_kind == Some("revision") && prereq_status == "in_review" {
        return true;
    }
    status_satisfies(prereq_id, prereq_status)
}

/// Look up the `kind` column for a task row. Returns `None` for
/// project ids, unknown ids, and soft-deleted rows.
pub fn lookup_work_item_kind(conn: &Connection, work_item_id: &str) -> Result<Option<String>> {
    if !work_item_id.starts_with("task_") {
        return Ok(None);
    }
    conn.query_row(
        "SELECT kind FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
        params![work_item_id],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Look up the current status of a work item in either `tasks` or
/// `projects`. Returns `None` for unknown / soft-deleted ids; the
/// caller decides whether that's an error or a "treat as satisfied"
/// signal (a soft-deleted prereq has its edge dropped immediately,
/// so this code path should rarely see one).
pub fn lookup_work_item_status(conn: &Connection, work_item_id: &str) -> Result<Option<String>> {
    if work_item_id.starts_with("proj_") {
        return conn
            .query_row(
                "SELECT status FROM projects WHERE id = ?1",
                params![work_item_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into);
    }
    if work_item_id.starts_with("task_") {
        return conn
            .query_row(
                "SELECT status FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
                params![work_item_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into);
    }
    Ok(None)
}

/// Return the prerequisite ids that currently *gate* `work_item_id`
/// — `blocks` edges whose prereq has not reached a satisfied
/// status. Used by both the dispatcher (to demote a gated dependent
/// to `waiting_dependency`) and the auto-block / unblock path.
///
/// The satisfaction check is revision-aware: for `kind = 'revision'`
/// dependents a prerequisite also satisfies when it reaches
/// `in_review` (the PR is open and the revision can push to it).
/// For all other dependents the standard `done`/`archived` rules
/// apply.
pub fn gating_prereqs_for(conn: &Connection, work_item_id: &str) -> Result<Vec<String>> {
    let dependent_kind = lookup_work_item_kind(conn, work_item_id)?;
    let edges = prerequisites_of(conn, work_item_id, Some(RELATION_BLOCKS))?;
    let mut gating = Vec::new();
    for edge in edges {
        let status = lookup_work_item_status(conn, &edge.prerequisite_id)?;
        match status {
            Some(s) if status_satisfies_for_dependent(&edge.prerequisite_id, &s, dependent_kind.as_deref()) => {}
            _ => gating.push(edge.prerequisite_id),
        }
    }
    Ok(gating)
}

/// A `merge_order` sibling that has already merged — the "first" side of a
/// pairing whose surfaces the later PR's forward-port must preserve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOrderMergedSibling {
    /// The merged sibling's task id.
    pub task_id: String,
    /// Its PR url, if recorded — so the forward-port brief can name it.
    pub pr_url: Option<String>,
}

/// One end of a `merge_order` pairing, resolved for `work_item_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOrderSibling {
    /// The *other* work item in the pairing.
    pub sibling_id: String,
    /// `true` when `work_item_id` is the `dependent_id` of the edge — the
    /// canonical "later" side (the one the dispatch stagger delays). `false`
    /// when it is the `prerequisite_id` (the canonical "first" side).
    pub work_item_is_later: bool,
}

/// Every `merge_order` peer of `work_item_id`, in either direction. Because
/// a `merge_order` edge is a soft, undirected *pairing* (the stored
/// direction is only a canonical tiebreaker), callers that need "the other
/// task in the pair" must look both ways. Ordering is stable
/// (created_at ASC, then sibling id ASC) so callers are deterministic.
///
/// Never consulted by dispatch gating — that path is `blocks`-only. Used by
/// the merge-sequencing paths (forward-port brief stamping, dispatch
/// stagger).
pub fn merge_order_siblings(conn: &Connection, work_item_id: &str) -> Result<Vec<MergeOrderSibling>> {
    let mut out = Vec::new();
    // Edges where this item is the dependent (canonical "later") side.
    for edge in prerequisites_of(conn, work_item_id, Some(RELATION_MERGE_ORDER))? {
        out.push(MergeOrderSibling {
            sibling_id: edge.prerequisite_id,
            work_item_is_later: true,
        });
    }
    // Edges where this item is the prerequisite (canonical "first") side.
    for edge in dependents_of(conn, work_item_id, Some(RELATION_MERGE_ORDER))? {
        out.push(MergeOrderSibling {
            sibling_id: edge.dependent_id,
            work_item_is_later: false,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fresh_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE work_item_dependencies (
                dependent_id     TEXT NOT NULL,
                prerequisite_id  TEXT NOT NULL,
                relation         TEXT NOT NULL DEFAULT 'blocks',
                created_at       TEXT NOT NULL,
                PRIMARY KEY (dependent_id, prerequisite_id, relation),
                CHECK (dependent_id <> prerequisite_id)
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn insert_then_idempotent_reinsert() {
        let conn = fresh_db();
        let (edge, outcome) = insert_edge(&conn, "task_a", "task_b", RELATION_BLOCKS, "1000").unwrap();
        assert_eq!(outcome, EdgeInsertOutcome::Inserted);
        assert_eq!(edge.dependent_id, "task_a");
        assert_eq!(edge.prerequisite_id, "task_b");
        assert_eq!(edge.relation, RELATION_BLOCKS);

        let (_, outcome2) = insert_edge(&conn, "task_a", "task_b", RELATION_BLOCKS, "9999").unwrap();
        assert_eq!(outcome2, EdgeInsertOutcome::AlreadyExists);
    }

    #[test]
    fn delete_returns_false_for_missing_edge() {
        let conn = fresh_db();
        let removed = delete_edge(&conn, "task_a", "task_b", RELATION_BLOCKS).unwrap();
        assert!(!removed);
        let _ = insert_edge(&conn, "task_a", "task_b", RELATION_BLOCKS, "1").unwrap();
        let removed = delete_edge(&conn, "task_a", "task_b", RELATION_BLOCKS).unwrap();
        assert!(removed);
    }

    #[test]
    fn cycle_detection_catches_two_step_loops() {
        let conn = fresh_db();
        insert_edge(&conn, "task_a", "task_b", RELATION_BLOCKS, "1").unwrap();
        // Adding `task_b → task_a` would close the loop.
        assert!(would_create_cycle(&conn, "task_b", "task_a").unwrap());
        // But `task_b → task_c` is fine.
        assert!(!would_create_cycle(&conn, "task_b", "task_c").unwrap());
    }

    #[test]
    fn cycle_detection_catches_self_loop() {
        let conn = fresh_db();
        assert!(would_create_cycle(&conn, "task_a", "task_a").unwrap());
    }

    #[test]
    fn cycle_detection_walks_multi_hop() {
        let conn = fresh_db();
        insert_edge(&conn, "task_a", "task_b", RELATION_BLOCKS, "1").unwrap();
        insert_edge(&conn, "task_b", "task_c", RELATION_BLOCKS, "2").unwrap();
        insert_edge(&conn, "task_c", "task_d", RELATION_BLOCKS, "3").unwrap();
        // a → b → c → d. Adding d → a closes a 4-cycle.
        assert!(would_create_cycle(&conn, "task_d", "task_a").unwrap());
        // Adding e → a is fine.
        assert!(!would_create_cycle(&conn, "task_e", "task_a").unwrap());
    }

    // ── merge_order: non-blocking pairing ───────────────────────────────────

    /// A `merge_order` edge must never manufacture a false cycle that would
    /// reject a legitimate `blocks` edge in the reverse direction. `blocks`
    /// and `merge_order` are independent graphs.
    #[test]
    fn merge_order_edge_does_not_block_reverse_blocks_edge() {
        let conn = fresh_db();
        insert_edge(&conn, "task_a", "task_b", RELATION_MERGE_ORDER, "1").unwrap();
        // A `blocks` B→A must still be allowed — the merge_order A→B edge is
        // invisible to the (blocks-scoped) cycle walk.
        assert!(
            !would_create_cycle(&conn, "task_b", "task_a").unwrap(),
            "a merge_order edge must not gate a reverse blocks edge",
        );
        // And a genuine blocks cycle is still caught.
        insert_edge(&conn, "task_b", "task_a", RELATION_BLOCKS, "2").unwrap();
        assert!(would_create_cycle(&conn, "task_a", "task_b").unwrap());
    }

    /// `merge_order` edges are never returned by the blocks-only gating query.
    #[test]
    fn merge_order_edge_is_not_a_gating_prereq() {
        let conn = fresh_db();
        insert_edge(&conn, "task_later", "task_first", RELATION_MERGE_ORDER, "1").unwrap();
        // No `blocks` prerequisites → nothing gates task_later even though a
        // merge_order edge names task_first.
        let blocks_prereqs = prerequisites_of(&conn, "task_later", Some(RELATION_BLOCKS)).unwrap();
        assert!(
            blocks_prereqs.is_empty(),
            "merge_order must not appear as a blocks prereq"
        );
    }

    /// `merge_order_siblings` resolves the peer in both directions with the
    /// correct "later" flag.
    #[test]
    fn merge_order_siblings_resolves_both_directions() {
        let conn = fresh_db();
        // Canonical edge: task_first (prereq) → task_later (dependent).
        insert_edge(&conn, "task_later", "task_first", RELATION_MERGE_ORDER, "1").unwrap();

        let for_later = merge_order_siblings(&conn, "task_later").unwrap();
        assert_eq!(for_later.len(), 1);
        assert_eq!(for_later[0].sibling_id, "task_first");
        assert!(for_later[0].work_item_is_later, "the dependent side is the later side");

        let for_first = merge_order_siblings(&conn, "task_first").unwrap();
        assert_eq!(for_first.len(), 1);
        assert_eq!(for_first[0].sibling_id, "task_later");
        assert!(
            !for_first[0].work_item_is_later,
            "the prerequisite side is the first side"
        );

        // A pure blocks edge must not show up as a merge_order sibling.
        insert_edge(&conn, "task_later", "task_gate", RELATION_BLOCKS, "2").unwrap();
        assert_eq!(merge_order_siblings(&conn, "task_later").unwrap().len(), 1);
    }

    #[test]
    fn list_endpoints_match_direction() {
        let conn = fresh_db();
        insert_edge(&conn, "task_a", "task_b", RELATION_BLOCKS, "1").unwrap();
        insert_edge(&conn, "task_a", "task_c", RELATION_BLOCKS, "2").unwrap();
        insert_edge(&conn, "task_d", "task_a", RELATION_BLOCKS, "3").unwrap();

        let prereqs_of_a = prerequisites_of(&conn, "task_a", Some(RELATION_BLOCKS)).unwrap();
        assert_eq!(prereqs_of_a.len(), 2);
        assert_eq!(prereqs_of_a[0].prerequisite_id, "task_b");
        assert_eq!(prereqs_of_a[1].prerequisite_id, "task_c");

        let dependents_of_a = dependents_of(&conn, "task_a", Some(RELATION_BLOCKS)).unwrap();
        assert_eq!(dependents_of_a.len(), 1);
        assert_eq!(dependents_of_a[0].dependent_id, "task_d");
    }

    // ── status_satisfies_for_dependent / revision-gate semantics ────────────

    /// A non-revision dependent still requires `done` from a task prereq;
    /// `in_review` must NOT satisfy it.
    #[test]
    fn in_review_does_not_satisfy_non_revision_dependent() {
        assert!(
            !status_satisfies_for_dependent("task_prereq", "in_review", Some("chore")),
            "chore dependent: in_review must NOT satisfy"
        );
        assert!(
            !status_satisfies_for_dependent("task_prereq", "in_review", None),
            "no kind: in_review must NOT satisfy"
        );
        assert!(
            status_satisfies_for_dependent("task_prereq", "done", Some("chore")),
            "chore dependent: done must satisfy"
        );
    }

    /// A revision dependent must unblock when its prerequisite reaches
    /// `in_review`; it must also accept `done` (merged) as satisfying.
    #[test]
    fn in_review_satisfies_revision_dependent() {
        assert!(
            status_satisfies_for_dependent("task_prereq", "in_review", Some("revision")),
            "revision dependent: in_review must satisfy"
        );
        assert!(
            status_satisfies_for_dependent("task_prereq", "done", Some("revision")),
            "revision dependent: done must also satisfy"
        );
        assert!(
            !status_satisfies_for_dependent("task_prereq", "todo", Some("revision")),
            "revision dependent: todo must NOT satisfy"
        );
        assert!(
            !status_satisfies_for_dependent("task_prereq", "active", Some("revision")),
            "revision dependent: active must NOT satisfy"
        );
    }

    /// Projects satisfy on `done` or `archived`; the revision rule only
    /// applies to task prereqs (project `in_review` does not exist in the
    /// status machine, but guard anyway).
    #[test]
    fn project_prereq_satisfies_on_done_or_archived() {
        assert!(status_satisfies_for_dependent("proj_x", "done", Some("revision")));
        assert!(status_satisfies_for_dependent("proj_x", "archived", Some("revision")));
        assert!(!status_satisfies_for_dependent("proj_x", "active", Some("revision")));
    }
}
