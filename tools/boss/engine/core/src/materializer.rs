//! The deterministic **Materializer** — the *apply* half of auto-populate.
//!
//! [`Materializer::apply`] takes a validated [`PlannerOutput`] proposal and
//! writes the project's implementation task graph in a **single SQLite
//! transaction**: it dedups by `(name, project_id)`, creates the new tasks
//! `autostart = false` (staged), tags each with the originating
//! `planner_runs.id`, and wires the dependency edges. It is the *only* thing
//! in the auto-populate flow that writes task rows.
//!
//! See `tools/boss/docs/designs/auto-populate-project-tasks-on-design-pr-merge.md`
//! (project P783) §3 "The deterministic materializer (apply)". This module is
//! task 5 of that design.
//!
//! ## Reuses the existing write paths — never a parallel creation path
//!
//! Per the project constraint, the Materializer does **not** invent its own
//! task/edge writes. It composes the same in-transaction helpers the CLI and
//! every other caller use:
//!
//! - [`insert_task_in_tx`] / [`insert_investigation_in_tx`] — the bodies of
//!   `create_task` / `create_investigation`, so created rows inherit the
//!   product/project existence checks, ordinal allocation, and short-id
//!   allocation.
//! - [`add_dependency_edge_in_tx`] — the body of `add_dependency`, so wired
//!   edges inherit the same-product check, `would_create_cycle` gate, the
//!   engine auto-block, and `INSERT OR IGNORE` duplicate-edge dedup.
//!
//! Because all of that runs inside one transaction the Materializer opens and
//! commits itself, **any error rolls the whole thing back — no partial graph
//! is ever created** (design §3 step 5).
//!
//! ## Defense in depth: validate the graph before any insert
//!
//! The validation layer (task 6, [`boss_engine_planner_validation`]) runs before
//! apply on the trigger path. But `apply` is a public entry point (operator
//! commands, replanning, tests) and must be self-contained, so it re-checks
//! handle integrity and acyclicity *before* opening the write transaction
//! ([`check_graph`]). A cyclic or handle-broken proposal is rejected with
//! nothing written. `add_dependency_edge_in_tx`'s own `would_create_cycle` is
//! then the second line of defense at edge-insert time.
//!
//! ## Idempotent / re-apply safe
//!
//! Re-applying the same proposal is additive, never destructive: existing
//! tasks (by `(name, project_id)`) are skipped but still resolve their handle
//! to the existing id so edges wire, and duplicate edges are `INSERT OR
//! IGNORE` no-ops. This is what makes replanning against an updated doc safe
//! (design §2 "Reusability" #3).

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};

use boss_protocol::{
    ApplyResult, CreateInvestigationInput, CreateTaskInput, PlannerOutput, ProposedEdge, ProposedTask, TaskKind,
};

use crate::work::{
    WorkDb, add_dependency_edge_in_tx, insert_investigation_in_tx, insert_task_in_tx, now_string, query_project,
};
use crate::work_dependencies::{RELATION_BLOCKS, RELATION_MERGE_ORDER, insert_edge, query_edge};

/// `created_via` stamp for tasks materialized from a planner proposal, so the
/// row's provenance is distinguishable from `cli` / `mac_app` / `bossctl`
/// creations (design §3 step 3).
const CREATED_VIA_ENGINE_AUTO: &str = "engine_auto";

/// The deterministic Materializer. A zero-sized entry point so callers write
/// the `Materializer::apply(..)` shape the design names; it holds no state.
pub struct Materializer;

impl Materializer {
    /// Apply a validated [`PlannerOutput`] to `project_id` in one transaction.
    ///
    /// Every task created in this run is tagged with `planner_run_id` (the
    /// `planner_runs.id` that owns the populate) so the undo path can later
    /// delete exactly this batch. Tasks are created `autostart = false`
    /// (staged) — they exist and are graph-wired but the dispatcher will not
    /// promote them until an operator releases them.
    ///
    /// Returns an [`ApplyResult`] describing what was created, what was
    /// deduped, and how many new edges were inserted. On **any** error the
    /// transaction is rolled back and no rows are written (no partial graph).
    ///
    /// # Errors
    ///
    /// - The proposal's graph is invalid: a duplicate task handle, an edge
    ///   referencing an unknown handle, or a dependency cycle. Rejected
    ///   before any DB write.
    /// - The project does not exist, or a reused write path fails (e.g. the
    ///   edge cycle-gate fires).
    pub fn apply(db: &WorkDb, project_id: &str, planner_run_id: &str, output: &PlannerOutput) -> Result<ApplyResult> {
        // 1 + 2. Handle integrity + topo-sort/cycle reject, before any insert.
        check_graph(output)?;

        let mut conn = db.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();

        // Resolve the product from the project (also validates it exists).
        let project = query_project(&tx, project_id)?
            .with_context(|| format!("materializer: project not found: {project_id}"))?;
        let product_id = project.product_id;

        // Existing non-deleted task names in the project → id. Drives the
        // `(name, project_id)` dedup; a skipped task still resolves its handle
        // to the existing id so edges pointing at it wire correctly.
        let mut name_to_id = existing_task_names(&tx, project_id)?;

        // handle → task id (whether freshly created or deduped to an existing
        // row). Every handle lands here so every edge endpoint resolves.
        let mut handle_to_id: HashMap<String, String> = HashMap::with_capacity(output.tasks.len());
        let mut created: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();

        for task in &output.tasks {
            let name_key = task.name.trim().to_owned();
            if let Some(existing_id) = name_to_id.get(&name_key) {
                // Dedup: a non-deleted task with this name already exists in
                // the project (operator pre-seed, prior populate, or an
                // earlier task in this same proposal).
                handle_to_id.insert(task.handle.clone(), existing_id.clone());
                skipped.push(task.name.clone());
                continue;
            }

            let new_id = insert_proposed_task(&tx, &product_id, project_id, task)?;
            // Tag the new row with the owning planner run, in the same
            // transaction that created it — so the batch is atomically
            // taggable-or-absent.
            tx.execute(
                "UPDATE tasks SET planner_run_id = ?2 WHERE id = ?1",
                params![new_id, planner_run_id],
            )?;

            name_to_id.insert(name_key, new_id.clone());
            handle_to_id.insert(task.handle.clone(), new_id.clone());
            created.push(new_id);
        }

        // Wire edges through the shared write path: same-product check +
        // `would_create_cycle` gate + `INSERT OR IGNORE` dedup.
        let mut edges_created = 0usize;
        for edge in &output.edges {
            let dependent_id = handle_to_id
                .get(&edge.dependent)
                .with_context(|| format!("materializer: unresolved edge dependent handle: {}", edge.dependent))?;
            let prerequisite_id = handle_to_id.get(&edge.prerequisite).with_context(|| {
                format!(
                    "materializer: unresolved edge prerequisite handle: {}",
                    edge.prerequisite
                )
            })?;

            // Count only genuinely-new insertions so re-apply reports 0.
            let already = query_edge(&tx, dependent_id, prerequisite_id, RELATION_BLOCKS)?.is_some();
            add_dependency_edge_in_tx(&tx, dependent_id, prerequisite_id, RELATION_BLOCKS, &now)
                .with_context(|| format!("materializer: wiring edge {} -> {}", edge.dependent, edge.prerequisite))?;
            if !already {
                edges_created += 1;
            }
        }

        // Wire soft `merge_order` edges from the proposal's file-overlap hints.
        // These are **non-blocking** (dispatch gating is `blocks`-only, see
        // `dep_helpers::compute_gated_work_item_ids`) and pair two otherwise-
        // parallel siblings so the later PR forward-ports preservingly at merge
        // time (merge-conflict-reduction design, Layer 3 / direction 2). The
        // pairing is undirected; we store a canonical direction
        // (`task_a` = prerequisite/"first", `task_b` = dependent/"later") and
        // dedup a hint whose pairing already exists in either direction.
        let mut merge_order_edges_created = 0usize;
        for hint in &output.merge_order_hints {
            // Hints are **soft**: a malformed one (a handle that doesn't
            // resolve — possible on the public `apply` entry point, which does
            // not run the upstream hint-handle validation) must never abort a
            // valid task graph. Skip it with a warning instead of failing the
            // whole populate the way an unresolved `blocks` edge handle does.
            let (Some(a_id), Some(b_id)) = (handle_to_id.get(&hint.task_a), handle_to_id.get(&hint.task_b)) else {
                tracing::warn!(
                    task_a = %hint.task_a,
                    task_b = %hint.task_b,
                    "materializer: skipping merge_order hint with an unresolved handle",
                );
                continue;
            };
            // A hint whose two handles deduped to the same existing task is a
            // no-op — a task never sequences against itself.
            if a_id == b_id {
                continue;
            }
            // Dedup the undirected pairing: an existing edge either way
            // satisfies it, so re-applying a proposal that lists the same pair
            // (even with the handles swapped) stays a no-op.
            let already = query_edge(&tx, b_id, a_id, RELATION_MERGE_ORDER)?.is_some()
                || query_edge(&tx, a_id, b_id, RELATION_MERGE_ORDER)?.is_some();
            if already {
                continue;
            }
            // Non-blocking insert: bypass `add_dependency_edge_in_tx`, whose
            // auto-block and `would_create_cycle` gate are `blocks`-specific. A
            // merge_order edge must never block a dependent or be rejected by
            // the blocks-graph cycle check.
            insert_edge(&tx, b_id, a_id, RELATION_MERGE_ORDER, &now).with_context(|| {
                format!(
                    "materializer: wiring merge_order edge {} <-> {}",
                    hint.task_a, hint.task_b
                )
            })?;
            merge_order_edges_created += 1;
        }

        tx.commit()?;
        Ok(ApplyResult {
            created,
            skipped,
            edges_created,
            merge_order_edges_created,
        })
    }
}

/// Insert one proposed task through the appropriate reused write path and
/// return its new id. `project_task` goes through [`insert_task_in_tx`];
/// `investigation` through [`insert_investigation_in_tx`] (the only two kinds
/// the [`PlannerOutput`] contract permits).
///
/// `autostart = false` stages the task; `force_duplicate = true` is set
/// deliberately — the Materializer has *already* performed the authoritative
/// `(name, project_id)` dedup, so the product-scoped 60-second recent-
/// duplicate heuristic (which would otherwise abort the whole populate on an
/// unrelated same-named task elsewhere in the product) must not derail it.
/// The `[effort-classification]` audit line is already appended to
/// `task.description` by the Planner, so it is passed through verbatim.
fn insert_proposed_task(conn: &Connection, product_id: &str, project_id: &str, task: &ProposedTask) -> Result<String> {
    let created = match task.kind {
        TaskKind::Investigation => insert_investigation_in_tx(
            conn,
            CreateInvestigationInput::builder()
                .product_id(product_id)
                .project_id(project_id)
                .name(task.name.clone())
                .description(task.description.clone())
                .effort_level(task.effort)
                .autostart(false)
                .force_duplicate(true)
                .created_via(CREATED_VIA_ENGINE_AUTO)
                .build(),
        )?,
        // `project_task` is the default and the only other kind the contract
        // permits; anything else would have been rejected upstream.
        _ => insert_task_in_tx(
            conn,
            CreateTaskInput::builder()
                .product_id(product_id)
                .project_id(project_id)
                .name(task.name.clone())
                .description(task.description.clone())
                .effort_level(task.effort)
                .autostart(false)
                .force_duplicate(true)
                .created_via(CREATED_VIA_ENGINE_AUTO)
                .build(),
        )?,
    };
    Ok(created.id)
}

/// Build the `(trimmed name → id)` map of existing non-deleted tasks in the
/// project. Keying on the trimmed name matches the engine's duplicate
/// semantics (`check_recent_duplicate` also trims).
fn existing_task_names(conn: &Connection, project_id: &str) -> Result<HashMap<String, String>> {
    let mut stmt = conn.prepare(
        "SELECT id, name FROM tasks
         WHERE project_id = ?1 AND deleted_at IS NULL",
    )?;
    let rows = stmt.query_map(params![project_id], |row| {
        Ok((row.get::<_, String>(1)?, row.get::<_, String>(0)?))
    })?;
    let mut map: HashMap<String, String> = HashMap::new();
    for row in rows {
        let (name, id) = row?;
        map.entry(name.trim().to_owned()).or_insert(id);
    }
    Ok(map)
}

/// Pure graph validation: reject duplicate handles, edges referencing unknown
/// handles, and cycles — before any DB write. Mirrors the checks in
/// [`boss_engine_planner_validation`] but returns a plain `Result<()>` (apply's
/// caller has already mapped the richer `ValidationResult` to an audit
/// outcome; here the checks are self-contained defense in depth).
fn check_graph(output: &PlannerOutput) -> Result<()> {
    // Every handle must be unique.
    let mut handles: HashSet<&str> = HashSet::with_capacity(output.tasks.len());
    for task in &output.tasks {
        if !handles.insert(task.handle.as_str()) {
            bail!("materializer: duplicate task handle: {}", task.handle);
        }
    }
    // Every edge endpoint must name a known handle.
    for edge in &output.edges {
        if !handles.contains(edge.dependent.as_str()) {
            bail!("materializer: edge references unknown handle: {}", edge.dependent);
        }
        if !handles.contains(edge.prerequisite.as_str()) {
            bail!("materializer: edge references unknown handle: {}", edge.prerequisite);
        }
    }
    // The edge set must form a DAG.
    if let Some(cycle) = find_cycle(&handles, &output.edges) {
        bail!(
            "materializer: dependency graph contains a cycle involving: {}",
            cycle.join(", ")
        );
    }
    Ok(())
}

/// Topologically sort the handle graph with Kahn's algorithm; return `None`
/// when it is acyclic, or `Some(stuck)` — the sorted set of handles that
/// never reached in-degree 0 (i.e. those on or feeding into a cycle) — when
/// it is not.
///
/// Edge direction: `dependent` depends on `prerequisite`, so `prerequisite`
/// must be emitted first. In-degree counts a node's unmet prerequisites.
fn find_cycle(handles: &HashSet<&str>, edges: &[ProposedEdge]) -> Option<Vec<String>> {
    let mut in_degree: HashMap<&str, usize> = handles.iter().map(|&h| (h, 0usize)).collect();
    let mut adj: HashMap<&str, Vec<&str>> = handles.iter().map(|&h| (h, Vec::new())).collect();
    for edge in edges {
        // prerequisite → dependent: emitting the prerequisite unblocks the dependent.
        adj.entry(edge.prerequisite.as_str())
            .or_default()
            .push(edge.dependent.as_str());
        *in_degree.entry(edge.dependent.as_str()).or_insert(0) += 1;
    }

    let mut queue: VecDeque<&str> = in_degree.iter().filter(|&(_, &d)| d == 0).map(|(&h, _)| h).collect();
    let mut emitted = 0usize;
    while let Some(node) = queue.pop_front() {
        emitted += 1;
        if let Some(neighbors) = adj.get(node) {
            for &next in neighbors {
                let d = in_degree.get_mut(next).expect("edge endpoint is a tracked handle");
                *d -= 1;
                if *d == 0 {
                    queue.push_back(next);
                }
            }
        }
    }

    if emitted == handles.len() {
        None
    } else {
        // Handles that never reached in-degree 0 are exactly those on/into a cycle.
        let mut stuck: Vec<String> = in_degree
            .iter()
            .filter(|&(_, &d)| d > 0)
            .map(|(&h, _)| h.to_owned())
            .collect();
        stuck.sort();
        Some(stuck)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use boss_protocol::{Confidence, CreateTaskInput, EffortLevel, ProposedMergeOrderHint, ProposedTask, TaskKind};

    use crate::work::{ClaimPlannerRunInput, WorkDb};

    // ---- helpers -----------------------------------------------------------

    fn open() -> WorkDb {
        WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap()
    }

    fn product_and_project(db: &WorkDb) -> (String, String) {
        let product = create_test_product_with_repo(db, "Test", Some("git@github.com:test/test.git"));
        let project = db
            .create_project(
                boss_protocol::CreateProjectInput::builder()
                    .product_id(product.id.clone())
                    .name("Alpha")
                    .goal("build it")
                    .build(),
            )
            .unwrap();
        (product.id, project.id)
    }

    fn claim(db: &WorkDb, product_id: &str, project_id: &str) -> String {
        db.claim_planner_run(ClaimPlannerRunInput {
            project_id,
            product_id,
            design_task_id: None,
            caller: "operator",
        })
        .unwrap()
        .unwrap()
        .id
    }

    fn ptask(handle: &str, name: &str, kind: TaskKind) -> ProposedTask {
        ProposedTask {
            handle: handle.to_owned(),
            name: name.to_owned(),
            description: format!(
                "Do {name}.\n\n[effort-classification] level=`small` matched-rule=`rule 5 (self-contained)` reasons=\"x\""
            ),
            kind,
            effort: EffortLevel::Small,
            ordinal: 0,
        }
    }

    fn pedge(dependent: &str, prerequisite: &str) -> ProposedEdge {
        ProposedEdge {
            dependent: dependent.to_owned(),
            prerequisite: prerequisite.to_owned(),
        }
    }

    fn output(tasks: Vec<ProposedTask>, edges: Vec<ProposedEdge>) -> PlannerOutput {
        output_with_hints(tasks, edges, vec![])
    }

    fn output_with_hints(
        tasks: Vec<ProposedTask>,
        edges: Vec<ProposedEdge>,
        merge_order_hints: Vec<ProposedMergeOrderHint>,
    ) -> PlannerOutput {
        PlannerOutput {
            tasks,
            edges,
            merge_order_hints,
            confidence: Confidence::High,
            breakdown_found: true,
            notes: String::new(),
            effort_audit: vec![],
        }
    }

    fn phint(task_a: &str, task_b: &str) -> ProposedMergeOrderHint {
        ProposedMergeOrderHint {
            task_a: task_a.to_owned(),
            task_b: task_b.to_owned(),
            reason: "shared component file".to_owned(),
        }
    }

    /// One-column scalar query against the task row for a targeted assertion.
    fn task_scalar<T: rusqlite::types::FromSql>(db: &WorkDb, id: &str, column: &str) -> T {
        let conn = db.connect().unwrap();
        conn.query_row(
            &format!("SELECT {column} FROM tasks WHERE id = ?1"),
            params![id],
            |row| row.get::<_, T>(0),
        )
        .unwrap()
    }

    fn count(db: &WorkDb, sql: &str, p: &[&dyn rusqlite::ToSql]) -> i64 {
        let conn = db.connect().unwrap();
        conn.query_row(sql, p, |row| row.get::<_, i64>(0)).unwrap()
    }

    // ---- apply: happy path -------------------------------------------------

    #[test]
    fn applies_dag_creates_tags_and_stages() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = claim(&db, &product_id, &project_id);

        let out = output(
            vec![
                ptask("schema", "Add schema", TaskKind::ProjectTask),
                ptask("engine", "Engine handler", TaskKind::ProjectTask),
            ],
            vec![pedge("engine", "schema")],
        );

        let res = Materializer::apply(&db, &project_id, &run, &out).unwrap();
        assert_eq!(res.created.len(), 2, "both tasks created");
        assert!(res.skipped.is_empty());
        assert_eq!(res.edges_created, 1);

        // Every created task is staged (autostart = false) and tagged.
        for id in &res.created {
            assert_eq!(task_scalar::<i64>(&db, id, "autostart"), 0, "staged, not dispatching");
            assert_eq!(task_scalar::<String>(&db, id, "planner_run_id"), run);
            assert_eq!(task_scalar::<String>(&db, id, "kind"), "project_task");
        }

        // The effort-classification line survives into the description.
        let desc = task_scalar::<String>(&db, &res.created[0], "description");
        assert!(desc.contains("[effort-classification]"));

        // The tag is readable via the accessor.
        let tagged = db.list_task_ids_for_planner_run(&run).unwrap();
        assert_eq!(tagged.len(), 2);

        // Exactly one `blocks` edge was wired.
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM work_item_dependencies WHERE relation = 'blocks'",
                &[]
            ),
            1
        );
    }

    // ---- apply: dedup ------------------------------------------------------

    #[test]
    fn dedups_existing_name_and_still_wires_edge_to_it() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = claim(&db, &product_id, &project_id);

        // Operator pre-seeded a task with the same name the proposal uses.
        let existing = db
            .create_task(
                CreateTaskInput::builder()
                    .product_id(product_id.clone())
                    .project_id(project_id.clone())
                    .name("Add schema")
                    .build(),
            )
            .unwrap();

        let out = output(
            vec![
                ptask("schema", "Add schema", TaskKind::ProjectTask),
                ptask("engine", "Engine handler", TaskKind::ProjectTask),
            ],
            vec![pedge("engine", "schema")],
        );

        let res = Materializer::apply(&db, &project_id, &run, &out).unwrap();
        assert_eq!(res.created.len(), 1, "only the new task is created");
        assert_eq!(res.skipped, vec!["Add schema".to_owned()], "existing name is skipped");
        assert_eq!(res.edges_created, 1);

        // The edge's prerequisite resolves to the PRE-EXISTING task id, proving
        // the deduped handle still wired the graph.
        let conn = db.connect().unwrap();
        let pre: String = conn
            .query_row(
                "SELECT prerequisite_id FROM work_item_dependencies WHERE relation = 'blocks'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pre, existing.id);

        // The pre-existing task is NOT tagged with the run (only newly created rows are).
        let tagged = db.list_task_ids_for_planner_run(&run).unwrap();
        assert_eq!(tagged, res.created);
    }

    #[test]
    fn dedups_duplicate_name_within_one_proposal() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = claim(&db, &product_id, &project_id);

        // Two distinct handles, same name — the second must dedup against the
        // first created in the same transaction.
        let out = output(
            vec![
                ptask("a", "Same name", TaskKind::ProjectTask),
                ptask("b", "Same name", TaskKind::ProjectTask),
            ],
            vec![],
        );

        let res = Materializer::apply(&db, &project_id, &run, &out).unwrap();
        assert_eq!(res.created.len(), 1);
        assert_eq!(res.skipped, vec!["Same name".to_owned()]);
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM tasks WHERE project_id = ?1 AND name = 'Same name'",
                &[&project_id]
            ),
            1,
            "no duplicate row created",
        );
    }

    // ---- apply: investigation kind ----------------------------------------

    #[test]
    fn creates_investigation_kind() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = claim(&db, &product_id, &project_id);

        let out = output(vec![ptask("audit", "Audit the thing", TaskKind::Investigation)], vec![]);
        let res = Materializer::apply(&db, &project_id, &run, &out).unwrap();
        assert_eq!(res.created.len(), 1);
        assert_eq!(task_scalar::<String>(&db, &res.created[0], "kind"), "investigation");
        assert_eq!(task_scalar::<i64>(&db, &res.created[0], "autostart"), 0);
    }

    // ---- apply: rejections leave nothing behind ---------------------------

    #[test]
    fn rejects_cycle_and_writes_nothing() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = claim(&db, &product_id, &project_id);

        let out = output(
            vec![
                ptask("a", "Task A", TaskKind::ProjectTask),
                ptask("b", "Task B", TaskKind::ProjectTask),
            ],
            vec![pedge("a", "b"), pedge("b", "a")],
        );

        let err = Materializer::apply(&db, &project_id, &run, &out).unwrap_err();
        assert!(err.to_string().contains("cycle"), "got: {err}");
        assert!(db.list_task_ids_for_planner_run(&run).unwrap().is_empty());
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM tasks WHERE project_id = ?1 AND name IN ('Task A', 'Task B')",
                &[&project_id]
            ),
            0,
            "no tasks created for a rejected proposal",
        );
    }

    #[test]
    fn rejects_self_loop() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = claim(&db, &product_id, &project_id);
        let out = output(vec![ptask("a", "Task A", TaskKind::ProjectTask)], vec![pedge("a", "a")]);
        assert!(Materializer::apply(&db, &project_id, &run, &out).is_err());
        assert!(db.list_task_ids_for_planner_run(&run).unwrap().is_empty());
    }

    #[test]
    fn rejects_unknown_edge_handle() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = claim(&db, &product_id, &project_id);
        let out = output(
            vec![ptask("a", "Task A", TaskKind::ProjectTask)],
            vec![pedge("a", "ghost")],
        );
        let err = Materializer::apply(&db, &project_id, &run, &out).unwrap_err();
        assert!(err.to_string().contains("unknown handle"), "got: {err}");
        assert!(db.list_task_ids_for_planner_run(&run).unwrap().is_empty());
    }

    #[test]
    fn rejects_duplicate_handle() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = claim(&db, &product_id, &project_id);
        let out = output(
            vec![
                ptask("dup", "Task A", TaskKind::ProjectTask),
                ptask("dup", "Task B", TaskKind::ProjectTask),
            ],
            vec![],
        );
        let err = Materializer::apply(&db, &project_id, &run, &out).unwrap_err();
        assert!(err.to_string().contains("duplicate task handle"), "got: {err}");
        assert!(db.list_task_ids_for_planner_run(&run).unwrap().is_empty());
    }

    // ---- apply: re-apply is additive + edge-dedup -------------------------

    #[test]
    fn re_apply_is_idempotent() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = claim(&db, &product_id, &project_id);

        let out = output(
            vec![
                ptask("schema", "Add schema", TaskKind::ProjectTask),
                ptask("engine", "Engine handler", TaskKind::ProjectTask),
            ],
            vec![pedge("engine", "schema")],
        );

        let first = Materializer::apply(&db, &project_id, &run, &out).unwrap();
        assert_eq!(first.created.len(), 2);
        assert_eq!(first.edges_created, 1);

        // Re-apply the identical proposal: everything dedups, no new edge.
        let second = Materializer::apply(&db, &project_id, &run, &out).unwrap();
        assert!(second.created.is_empty(), "nothing new created");
        assert_eq!(second.skipped.len(), 2, "both tasks deduped");
        assert_eq!(second.edges_created, 0, "edge already existed");

        // No duplicate rows or edges accumulated.
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM tasks WHERE project_id = ?1 AND kind = 'project_task'",
                &[&project_id]
            ),
            2,
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM work_item_dependencies WHERE relation = 'blocks'",
                &[]
            ),
            1
        );
    }

    // ---- apply: independent tasks stay unedged (parallelism preserved) ----

    #[test]
    fn fan_out_wires_only_declared_edges() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = claim(&db, &product_id, &project_id);

        // schema is a shared root; engine and cli each depend on it but not on
        // each other.
        let out = output(
            vec![
                ptask("schema", "Schema", TaskKind::ProjectTask),
                ptask("engine", "Engine", TaskKind::ProjectTask),
                ptask("cli", "CLI", TaskKind::ProjectTask),
            ],
            vec![pedge("engine", "schema"), pedge("cli", "schema")],
        );

        let res = Materializer::apply(&db, &project_id, &run, &out).unwrap();
        assert_eq!(res.created.len(), 3);
        assert_eq!(res.edges_created, 2);
        // No engine<->cli edge invented.
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM work_item_dependencies WHERE relation = 'blocks'",
                &[]
            ),
            2
        );
    }

    // ---- apply: merge_order hints → non-blocking edges --------------------

    #[test]
    fn materializes_merge_order_hint_as_non_blocking_edge() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = claim(&db, &product_id, &project_id);

        // Two parallel tasks (no blocks edge) flagged as file-overlapping.
        let out = output_with_hints(
            vec![
                ptask("compact", "Compact view", TaskKind::ProjectTask),
                ptask("detail", "Detail view", TaskKind::ProjectTask),
            ],
            vec![],
            vec![phint("compact", "detail")],
        );

        let res = Materializer::apply(&db, &project_id, &run, &out).unwrap();
        assert_eq!(res.created.len(), 2);
        assert_eq!(res.edges_created, 0, "file overlap must NOT create a blocks edge");
        assert_eq!(res.merge_order_edges_created, 1);

        // Exactly one merge_order edge, zero blocks edges.
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM work_item_dependencies WHERE relation = 'merge_order'",
                &[]
            ),
            1
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM work_item_dependencies WHERE relation = 'blocks'",
                &[]
            ),
            0
        );

        // Neither task is gated — a merge_order edge never blocks dispatch.
        for id in &res.created {
            assert_ne!(
                task_scalar::<String>(&db, id, "status"),
                "blocked",
                "a merge_order sibling must never be blocked",
            );
        }
    }

    #[test]
    fn re_apply_dedups_merge_order_edge_in_both_directions() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = claim(&db, &product_id, &project_id);

        let out = output_with_hints(
            vec![
                ptask("a", "Task A", TaskKind::ProjectTask),
                ptask("b", "Task B", TaskKind::ProjectTask),
            ],
            vec![],
            vec![phint("a", "b")],
        );
        let first = Materializer::apply(&db, &project_id, &run, &out).unwrap();
        assert_eq!(first.merge_order_edges_created, 1);

        // Re-apply the identical proposal: the pairing already exists.
        let second = Materializer::apply(&db, &project_id, &run, &out).unwrap();
        assert_eq!(second.merge_order_edges_created, 0, "same pairing must dedup");

        // Re-apply with the handles swapped: the undirected pairing still
        // exists, so no reverse-direction edge is created.
        let swapped = output_with_hints(
            vec![
                ptask("a", "Task A", TaskKind::ProjectTask),
                ptask("b", "Task B", TaskKind::ProjectTask),
            ],
            vec![],
            vec![phint("b", "a")],
        );
        let third = Materializer::apply(&db, &project_id, &run, &swapped).unwrap();
        assert_eq!(third.merge_order_edges_created, 0, "reverse pairing must dedup");

        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM work_item_dependencies WHERE relation = 'merge_order'",
                &[]
            ),
            1,
            "exactly one merge_order edge total",
        );
    }

    #[test]
    fn merge_order_hint_with_unknown_handle_is_skipped_not_fatal() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = claim(&db, &product_id, &project_id);

        // A soft hint referencing a handle that isn't among the proposal's
        // tasks must not abort the (otherwise valid) task graph.
        let out = output_with_hints(
            vec![ptask("a", "Task A", TaskKind::ProjectTask)],
            vec![],
            vec![phint("a", "ghost")],
        );
        let res = Materializer::apply(&db, &project_id, &run, &out).expect("soft hint must not fail apply");
        assert_eq!(res.created.len(), 1, "the valid task is still created");
        assert_eq!(res.merge_order_edges_created, 0, "the malformed hint is skipped");
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM work_item_dependencies WHERE relation = 'merge_order'",
                &[]
            ),
            0
        );
    }

    #[test]
    fn merge_order_hint_collapsing_to_one_task_is_skipped() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = claim(&db, &product_id, &project_id);

        // Two handles with the same name dedup to one task id; a merge_order
        // hint between them must not create a self-edge.
        let out = output_with_hints(
            vec![
                ptask("a", "Same name", TaskKind::ProjectTask),
                ptask("b", "Same name", TaskKind::ProjectTask),
            ],
            vec![],
            vec![phint("a", "b")],
        );
        let res = Materializer::apply(&db, &project_id, &run, &out).unwrap();
        assert_eq!(res.created.len(), 1, "both handles deduped to one task");
        assert_eq!(res.merge_order_edges_created, 0, "no self-pairing");
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM work_item_dependencies WHERE relation = 'merge_order'",
                &[]
            ),
            0
        );
    }
}
