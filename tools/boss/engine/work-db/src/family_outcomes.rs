//! The family outcome ledger: what happened to every PR family, from its first
//! launch to its merge, and the query `boss cost experiment` reads it with.
//!
//! One row per PR family (the chain root work item and everything under it),
//! opened by the family's first launched execution and never by a later one:
//!
//! | Column          | Written by                                                              |
//! | --------------- | ----------------------------------------------------------------------- |
//! | `opened_at`     | the first launch, epoch seconds                                         |
//! | `first_driver`  | the first launch's driver                                               |
//! | `baseline_head` | the PR head already published when the family first launched            |
//! | `first_head`    | the first head published after that                                     |
//! | `first_ci`      | the first terminal CI result on `first_head`: `success` or `fail`       |
//! | `merged_at`     | the merge, from the merge poller or a worker that finds its PR merged   |
//!
//! It is the Sol comparison's ledger (`sol_pr_outcomes`) opened for every family
//! instead of the Sol-eligible ones. It is a second table, not a shared one,
//! because the two ledgers answer different questions about the same family: the
//! Sol ledger re-baselines at each Sol-eligible execution, so its first CI is
//! judged on the first head after *Sol* enrolled, which can be a later revision;
//! this one is judged on the first head after the family's first launch, which is
//! what a family-level experiment compares. A shared row could hold only one of
//! the two, and `boss cost sol` must keep reading what it always read.
//!
//! A family that launched before this ledger existed has no row and is never
//! tracked: its first launch is gone, and opening a row at a later execution
//! would call a mid-life family "new".

use std::collections::HashMap;

use super::*;
use crate::cost_report_db::{COST_RECORD_COLUMNS, map_cost_run_record};
use boss_engine_storage_values::{CostRunRecord, FamilyOutcomeRecord};

pub(crate) const MIGRATION_ID: &str = "20261127_2200_pr_family_outcomes";

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pr_family_outcomes (
            work_item_id  TEXT PRIMARY KEY,
            opened_at     TEXT NOT NULL,
            first_driver  TEXT,
            baseline_head TEXT,
            first_head    TEXT,
            first_ci      TEXT,
            merged_at     TEXT
         )",
    )?;
    Ok(())
}

/// A launched execution, as the launch record itself defines one: a driver or a
/// model was recorded. `e` is the `work_executions` alias.
const LAUNCHED: &str = "(e.driver IS NOT NULL OR e.model IS NOT NULL)";

/// Keep a launched execution (alias `e`) that started at or before the family's
/// merge. `o` is the `pr_family_outcomes` alias. Open families have no cutoff.
const BEFORE_MERGE: &str =
    "(o.merged_at IS NULL OR CAST(COALESCE(e.started_at, e.created_at) AS INTEGER) <= CAST(o.merged_at AS INTEGER))";

/// Open `execution`'s family in the ledger, if this is the family's first launch.
///
/// Called in the transaction that records the execution's first launch, so the
/// row is open the moment the work is known to have started. A no-op when the
/// family already has a row, when another execution of the family has already
/// launched (the family predates the ledger), and when the family's root is not
/// a task (an answer agent's comment, an automation).
pub(crate) fn open_at_first_launch(
    conn: &Connection,
    execution: &WorkExecution,
    driver: &str,
    now: &str,
) -> Result<()> {
    let root = chain_helpers::experiment_family_root(conn, &execution.kind, &execution.work_item_id)?;
    let open: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pr_family_outcomes WHERE work_item_id = ?1)",
        [&root],
        |row| row.get(0),
    )?;
    if open {
        return Ok(());
    }
    // `pr_head_sha` is NULL before a PR is published, which is the usual case at
    // a family's first launch; a row for the root says the task exists at all.
    let Some(head): Option<Option<String>> = conn
        .query_row("SELECT pr_head_sha FROM tasks WHERE id = ?1", [&root], |row| row.get(0))
        .optional()?
    else {
        return Ok(());
    };
    if family_has_earlier_launch(conn, &root, &execution.id)? {
        return Ok(());
    }
    conn.execute(
        "INSERT OR IGNORE INTO pr_family_outcomes (work_item_id, opened_at, first_driver, baseline_head)
         VALUES (?1, ?2, ?3, ?4)",
        params![root, now, driver, head],
    )?;
    Ok(())
}

/// Idempotently stamp the family's merge time; a no-op for a family with no row.
/// Called wherever the Sol ledger's merge is: the merge poller, and the worker
/// completion that finds its PR already merged.
pub(crate) fn record_merge(conn: &Connection, work_item_id: &str, now: &str) -> Result<()> {
    ledger_record_merge(conn, "pr_family_outcomes", work_item_id, now)
}

/// Record only the first head published after the family's first launch, and
/// that head's first terminal CI result. A later head or a CI rerun cannot turn
/// a failed first attempt into a success, and the head that existed at the first
/// launch never counts. Two primary-key updates for a family with no row.
pub(crate) fn observe_ci(conn: &Connection, item: &str, head: Option<&str>, state: &str) -> Result<()> {
    ledger_observe_ci(conn, "pr_family_outcomes", item, head, state)
}

/// Stamp merge on `table` (`pr_family_outcomes` or `sol_pr_outcomes`).
pub(crate) fn ledger_record_merge(conn: &Connection, table: &'static str, work_item_id: &str, now: &str) -> Result<()> {
    debug_assert_outcome_ledger(table);
    let root = chain_helpers::chain_root(conn, work_item_id)?;
    conn.execute(
        &format!("UPDATE {table} SET merged_at = COALESCE(merged_at, ?2) WHERE work_item_id = ?1"),
        params![root, now],
    )?;
    Ok(())
}

/// Record the first post-baseline head and that head's first terminal CI on `table`.
pub(crate) fn ledger_observe_ci(
    conn: &Connection,
    table: &'static str,
    item: &str,
    head: Option<&str>,
    state: &str,
) -> Result<()> {
    debug_assert_outcome_ledger(table);
    let Some(head) = head else { return Ok(()) };
    let root = chain_helpers::chain_root(conn, item)?;
    conn.execute(
        &format!(
            "UPDATE {table}
             SET first_head = COALESCE(first_head, CASE WHEN ?2 IS NOT baseline_head THEN ?2 END)
             WHERE work_item_id = ?1 AND first_ci IS NULL"
        ),
        params![root, head],
    )?;
    conn.execute(
        &format!(
            "UPDATE {table} SET first_ci = ?3
             WHERE work_item_id = ?1 AND first_ci IS NULL AND first_head = ?2 AND ?3 IN ('success', 'fail')"
        ),
        params![root, head, state],
    )?;
    Ok(())
}

fn debug_assert_outcome_ledger(table: &str) {
    debug_assert!(
        matches!(table, "pr_family_outcomes" | "sol_pr_outcomes"),
        "outcome ledger table must be a known identifier, got {table:?}"
    );
}

/// The review-family walk down from the owners `owners_sql` selects: the
/// `roots(task_id, cycle_root_id)` relation of every family member, tombstoned
/// revisions included. `owners_sql` is the body of an `IN (...)` list of root ids.
pub(crate) fn family_cte(owners_sql: &str) -> String {
    format!(
        "{}, roots AS (SELECT DISTINCT id AS task_id, root AS cycle_root_id FROM family)",
        review_badge::review_family_cte(owners_sql, review_badge::ReviewKey::Owner)
    )
}

/// `family_cte` over the families named by the JSON array bound as `?1`.
fn family_cte_of_bound_roots() -> String {
    family_cte("SELECT value FROM json_each(?1)")
}

struct LedgerFamily {
    root: String,
    first_driver: Option<String>,
    first_ci: Option<bool>,
    merged: bool,
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Every PR family in the ledger as outcome facts, oldest first.
    ///
    /// With `experiment`, only the families in which some launched execution
    /// recorded that experiment, each carrying the arm every launched execution
    /// recorded for it (`FamilyOutcomeRecord::launch_arms`); the arms are read,
    /// not judged, so mixed and stopped families come back for the builder to
    /// count. Without one, every family, with no arms.
    ///
    /// Only families whose root has a PR and has not been deleted are read, as
    /// the Sol comparison reads them. Everything comes from one read
    /// transaction, so the figures describe a single moment.
    pub fn family_outcome_records(&self, experiment: Option<&str>) -> Result<Vec<FamilyOutcomeRecord>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        let records = read_family_outcomes(&tx, experiment)?;
        tx.commit()?;
        Ok(records)
    }
}

fn read_family_outcomes(conn: &Connection, experiment: Option<&str>) -> Result<Vec<FamilyOutcomeRecord>> {
    let mut families: Vec<LedgerFamily> = conn
        .prepare(
            "SELECT o.work_item_id, o.first_driver, o.first_ci, o.merged_at
             FROM pr_family_outcomes o JOIN tasks root ON root.id = o.work_item_id
             WHERE root.pr_url IS NOT NULL AND root.deleted_at IS NULL
             ORDER BY CAST(o.opened_at AS INTEGER), o.work_item_id",
        )?
        .query_map([], |row| {
            Ok(LedgerFamily {
                root: row.get(0)?,
                first_driver: row.get(1)?,
                first_ci: row.get::<_, Option<String>>(2)?.map(|state| state == "success"),
                merged: row.get::<_, Option<String>>(3)?.is_some(),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut launch_arms: HashMap<String, Vec<Option<String>>> = HashMap::new();
    if let Some(experiment) = experiment {
        let roots: Vec<&str> = families.iter().map(|family| family.root.as_str()).collect();
        launch_arms = read_launch_arms(conn, &roots, experiment)?;
        // A family that never recorded the experiment was never in it, so it
        // costs the rest of this read nothing.
        families.retain(|family| {
            launch_arms
                .get(&family.root)
                .is_some_and(|arms| arms.iter().any(Option::is_some))
        });
    }
    if families.is_empty() {
        return Ok(Vec::new());
    }

    let roots: Vec<&str> = families.iter().map(|family| family.root.as_str()).collect();
    let ids = query_ensure::id_array(&roots);
    let revisions = count_by_family(
        conn,
        &format!(
            "{} SELECT roots.cycle_root_id, COUNT(DISTINCT t.id)
             FROM roots
             JOIN pr_family_outcomes o ON o.work_item_id = roots.cycle_root_id
             JOIN tasks t ON t.id = roots.task_id AND t.kind = 'revision'
             WHERE EXISTS (
                 SELECT 1 FROM work_executions e
                 WHERE e.work_item_id = t.id AND {LAUNCHED} AND {BEFORE_MERGE})
             GROUP BY roots.cycle_root_id",
            family_cte_of_bound_roots()
        ),
        &ids,
    )?;
    let review_batches = count_by_family(
        conn,
        "SELECT cycle_root_id, COUNT(*) FROM pr_review_batches
         WHERE phase = 'pre_merge' AND cycle_root_id IN (SELECT value FROM json_each(?1))
         GROUP BY cycle_root_id",
        &ids,
    )?;
    let accepted = review_verdicts::INFORMATIVE_GATE_OUTCOMES
        .iter()
        .map(|outcome| format!("'{outcome}'"))
        .collect::<Vec<_>>()
        .join(",");
    let findings = count_by_family(
        conn,
        &format!(
            "{} SELECT roots.cycle_root_id, COALESCE(SUM(v.findings_count), 0)
             FROM roots
             JOIN pr_family_outcomes o ON o.work_item_id = roots.cycle_root_id
             JOIN pr_review_verdicts v ON v.work_item_id = roots.task_id
             WHERE v.gate_outcome IN ({accepted})
               AND (o.merged_at IS NULL OR CAST(v.created_at AS INTEGER) <= CAST(o.merged_at AS INTEGER))
             GROUP BY roots.cycle_root_id",
            family_cte_of_bound_roots()
        ),
        &ids,
    )?;
    // A post-merge verdict records the follow-up it minted; one tombstoned as an
    // orphan of a superseded verdict does not count.
    let followups = count_by_family(
        conn,
        "SELECT b.cycle_root_id, COUNT(DISTINCT v.revision_task_id)
         FROM pr_review_batches b
         JOIN pr_review_verdicts v ON v.batch_id = b.id
         JOIN tasks followup ON followup.id = v.revision_task_id AND followup.deleted_at IS NULL
         WHERE b.phase = 'post_merge' AND b.cycle_root_id IN (SELECT value FROM json_each(?1))
         GROUP BY b.cycle_root_id",
        &ids,
    )?;
    let mut runs = read_merged_family_runs(conn, &ids)?;

    Ok(families
        .into_iter()
        .map(|family| {
            let root = family.root;
            FamilyOutcomeRecord::builder()
                .family_root(&root)
                .maybe_first_driver(family.first_driver)
                .launch_arms(launch_arms.remove(&root).unwrap_or_default())
                .merged(family.merged)
                .maybe_first_ci(family.first_ci)
                .revisions(revisions.get(&root).copied().unwrap_or(0))
                .review_batches(review_batches.get(&root).copied().unwrap_or(0))
                .findings(findings.get(&root).copied().unwrap_or(0))
                .followups(followups.get(&root).copied().unwrap_or(0))
                .runs(runs.remove(&root).unwrap_or_default())
                .build()
        })
        .collect())
}

/// Run a `(family root, count)` query with the family roots bound as `?1`.
fn count_by_family(conn: &Connection, sql: &str, ids: &str) -> Result<HashMap<String, u64>> {
    Ok(conn
        .prepare(sql)?
        .query_map([ids], |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

/// For each of `roots`, the arm every launched execution of the family recorded
/// for `experiment` up to the family's merge: `None` for one that recorded
/// nothing for it (an unrecorded or unreadable arm set, or one without the key).
///
/// A family's executions are those of its work items and of the review guides
/// built from its PR, whose work item is a source comparison, not a task. A
/// launch after the merge is not enrolment: stopping an experiment must not
/// move a merged family into mixed.
fn read_launch_arms(
    conn: &Connection,
    roots: &[&str],
    experiment: &str,
) -> Result<HashMap<String, Vec<Option<String>>>> {
    let ids = query_ensure::id_array(roots);
    let mut arms: HashMap<String, Vec<Option<String>>> = HashMap::new();
    for join in [task_execution_join("e"), guide_execution_join("e")] {
        let mut stmt = conn.prepare(&format!(
            "{} SELECT roots.cycle_root_id, e.experiment_arms
             FROM roots
             JOIN pr_family_outcomes o ON o.work_item_id = roots.cycle_root_id
             {join}
             WHERE {LAUNCHED} AND {BEFORE_MERGE}",
            family_cte_of_bound_roots()
        ))?;
        let rows = stmt.query_map([&ids], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (root, text) = row?;
            let arm = text
                .as_deref()
                .and_then(execution_launch_config::parse_arm_set)
                .and_then(|set| set.get(experiment).cloned());
            arms.entry(root).or_default().push(arm);
        }
    }
    Ok(arms)
}

/// True when some launched execution of `root`'s family, other than
/// `execution_id`, already exists: a task execution or a review guide. Used so
/// a family that launched before this ledger existed is not opened mid-life.
fn family_has_earlier_launch(conn: &Connection, root: &str, execution_id: &str) -> Result<bool> {
    for join in [task_execution_join("e"), guide_execution_join("e")] {
        let found: bool = conn.query_row(
            &format!(
                "{} SELECT EXISTS (
                    SELECT 1 FROM roots {join}
                    WHERE roots.cycle_root_id = ?1 AND e.id != ?2 AND {LAUNCHED})",
                family_cte("?1")
            ),
            params![root, execution_id],
            |row| row.get(0),
        )?;
        if found {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Joins `roots` to the executions of its tasks, as `alias`.
fn task_execution_join(alias: &str) -> String {
    format!("JOIN work_executions {alias} ON {alias}.work_item_id = roots.task_id")
}

/// Joins `roots` to the review-guide executions of its families, as `alias`: a
/// guide's work item is a source comparison, found through its series' root task.
fn guide_execution_join(alias: &str) -> String {
    format!(
        "JOIN pr_review_guide_source_series s ON s.root_task_id = roots.task_id
         JOIN pr_review_guide_source_comparisons c ON c.series_id = s.id
         JOIN work_executions {alias} ON {alias}.work_item_id = c.id AND {alias}.kind = 'pr_review_guide'"
    )
}

/// Every run of each merged family among `ids`, up to the family's merge, by
/// family. A family that has not merged has no cost read: it is not in a
/// per-merged-family figure, and its runs are the bulk of this report's rows.
/// The review guide of a PR is part of the PR's cost, so its runs are read too.
fn read_merged_family_runs(conn: &Connection, ids: &str) -> Result<HashMap<String, Vec<CostRunRecord>>> {
    let mut runs: HashMap<String, Vec<CostRunRecord>> = HashMap::new();
    for join in [task_execution_join("we"), guide_execution_join("we")] {
        let mut stmt = conn.prepare(&format!(
            "{} SELECT {COST_RECORD_COLUMNS}, roots.cycle_root_id
             FROM roots
             JOIN pr_family_outcomes o ON o.work_item_id = roots.cycle_root_id AND o.merged_at IS NOT NULL
             {join}
             JOIN work_runs wr ON wr.execution_id = we.id
             LEFT JOIN tasks t ON t.id = we.work_item_id
             WHERE CAST(wr.created_at AS INTEGER) <= CAST(o.merged_at AS INTEGER)",
            family_cte_of_bound_roots()
        ))?;
        let rows = stmt.query_map([ids], |row| {
            // The family root is the one column after the run's own.
            let root = row.as_ref().column_count() - 1;
            Ok((map_cost_run_record(row)?, row.get::<_, String>(root)?))
        })?;
        for row in rows {
            let (run, root) = row?;
            runs.entry(root).or_default().push(run);
        }
    }
    for family in runs.values_mut() {
        family.sort_by(|a, b| (a.created_at_epoch_s, &a.run_id).cmp(&(b.created_at_epoch_s, &b.run_id)));
    }
    Ok(runs)
}

#[cfg(test)]
#[path = "family_outcomes_tests.rs"]
mod tests;
