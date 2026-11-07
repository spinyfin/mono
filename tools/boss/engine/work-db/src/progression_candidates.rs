//! Progression candidates: the single SQL definition of "this item is stuck,
//! nothing is working it, and an administrative agent should be spent on it".
//!
//! Design: `tools/boss/docs/designs/task-persistence-parking-and-autonomous-progression.md`
//! section 2.1. The sweep that mints progression executions, the stuck report's
//! progression buckets, and `bossctl dispatch diagnose` all read these views, so
//! they cannot disagree about what a candidate is.
//!
//! ## The views
//!
//! - `progression_candidate_clauses` has one row per open, non-deleted task and
//!   one `0/1` column per clause of section 2.1, plus the facts a reader needs
//!   to say *why* an item is or is not a candidate. `bossctl dispatch diagnose`
//!   reads this view.
//! - `progression_candidates` keeps the rows where every clause holds. The sweep
//!   reads this view.
//! - `progression_chain_roots`, `progression_family_keys` and
//!   `progression_live_families` are the helpers that give clause 2 its "item,
//!   its revisions and its PR chain" reach.
//! - `progression_run_state` is the cheap per-item subset the stuck report
//!   reads: the item's own progression runs, nothing about the rest of the graph.
//!
//! ## The six clauses
//!
//! 1. **Eligible item.** Not parked (itself, its project, or its revision chain
//!    root, via [`crate::parking::parked_task_sql`]) and not a human hold:
//!    `human_driven`, `deferred`, an open operator question,
//!    `blocked: awaiting_operator_answer`, an operator-refused `worker_failed`
//!    and the other holds the stuck report counts as waiting on a human. Done,
//!    archived and soft-deleted rows are not rows of the view at all.
//! 2. **No live or pending work** (`queued | ready | claimed | running |
//!    waiting_human`) on the item, on its revisions, or on any item sharing its
//!    PR. `waiting_dependency` deliberately does not count, matching
//!    `stuck_activity::related_work_is_live`: it is a gate, not evidence that
//!    anyone is working the PR, and counting it would let the dead revision at
//!    the head of a chain hide behind the later revisions queued behind it.
//! 3. **Not waiting on capacity.** No queued execution of the item carries a
//!    slot, cap, workspace or pause wait reason. Waiting for bandwidth is not
//!    stuck.
//! 4. **Dependency clear.** A `blocked: dependency` item is progressed through
//!    its prerequisite, not itself, unless every open prerequisite is held by a
//!    human or the edge dangles. The same notion as the stuck report's
//!    `waiting_on_dependency` and `parked_upstream` buckets.
//! 5. **Stuck long enough.** In one of the stuck states the enumeration marks as
//!    progression-eligible ([`stuck_kind_sql`]) continuously for at least
//!    `progression.stuck_after`, measured from the task status ledger exactly as
//!    the stuck report ages it; or the grace was waived since the item entered
//!    that state (`tasks.progression_grace_waived_at`).
//! 6. **Cooldown and budget.** No progression run within `progression.cooldown`
//!    (four times that after a `no_changes_needed` run), and the last
//!    [`INFRASTRUCTURE_FAILURE_CAP`] runs were not all infrastructure failures.
//!    The per-item create budget (`progression.max_creates_per_item_window`) is
//!    not an input here: it limits what a run may do, enforced by the verbs that
//!    consume it, and exhausting it makes the agent ask rather than makes the
//!    item un-nominatable.
//!
//! ## Limits
//!
//! `progression.stuck_after` and `progression.cooldown` are read from the
//! `metadata` table (the engine publishes them with
//! [`WorkDb::publish_progression_limits`]); an absent key falls back to the
//! design's placeholder, so the views are usable by a process that never ran the
//! engine, such as `bossctl` against a copied database.
use super::*;
use crate::parking::parked_task_sql;
use boss_engine_storage_values::dispatch_wait::{
    DISPATCH_WAIT_REASON_AUTOMATION_PAUSED, DISPATCH_WAIT_REASON_DISPATCH_PAUSED, DISPATCH_WAIT_REASON_FOCUS_MODE,
    DISPATCH_WAIT_REASON_POOL_EXHAUSTED, WAITING_FOR_WORKSPACE_REASON,
};
use boss_engine_storage_values::progression::{
    CREATED_VIA_PROGRESSION_SWEEP, DEFAULT_COOLDOWN_SECS, DEFAULT_STUCK_AFTER_SECS, INFRASTRUCTURE_FAILURE_CAP,
    METADATA_KEY_COOLDOWN_SECS, METADATA_KEY_STUCK_AFTER_SECS, NO_ACTION_COOLDOWN_MULTIPLIER, ProgressionSettings,
};

pub(super) const GRACE_COLUMN_MIGRATION_ID: &str = "20261106_1200_progression_grace_waived_at";
pub(super) const EXECUTION_PROVENANCE_MIGRATION_ID: &str = "20261106_1205_execution_created_via";
pub(super) const VIEWS_MIGRATION_ID: &str = "20261106_1210_progression_candidate_views";

/// Add `tasks.progression_grace_waived_at`: epoch seconds, set when a producer
/// waives the `progression.stuck_after` grace for an item (a worker-declared
/// blocker, an un-park) and cleared when the sweep consumes it.
pub(super) fn migrate_grace_column(conn: &Connection) -> Result<()> {
    if !crate::schema_init::table_has_column(conn, "tasks", "progression_grace_waived_at")? {
        conn.execute_batch("ALTER TABLE tasks ADD COLUMN progression_grace_waived_at TEXT")?;
    }
    Ok(())
}

/// Add `work_executions.created_via`: which producer minted the execution. Only
/// the `progression_sweep` sets it today; a NULL value means an execution minted
/// by the ordinary reconcile and request paths.
pub(super) fn migrate_execution_provenance(conn: &Connection) -> Result<()> {
    if !crate::schema_init::table_has_column(conn, "work_executions", "created_via")? {
        conn.execute_batch("ALTER TABLE work_executions ADD COLUMN created_via TEXT")?;
    }
    Ok(())
}

/// (Re)create the candidate views. Idempotent, and written as drop-then-create
/// so a later migration can re-run it to ship a changed definition.
pub(super) fn migrate_views(conn: &Connection) -> Result<()> {
    conn.execute_batch(&views_sql())?;
    Ok(())
}

/// Every view this module defines, dropped dependents first.
const DROP_VIEWS: &str = "DROP VIEW IF EXISTS progression_candidates;
         DROP VIEW IF EXISTS progression_candidate_clauses;
         DROP VIEW IF EXISTS progression_run_state;
         DROP VIEW IF EXISTS progression_live_families;
         DROP VIEW IF EXISTS progression_family_keys;
         DROP VIEW IF EXISTS progression_chain_roots;";

/// Drop the views. The views read the task status ledger, and SQLite re-parses
/// every view when an `ALTER TABLE` runs, so a test that simulates an older
/// database by dropping the ledger drops these first; reopening the database
/// replays the migration that recreates them.
#[cfg(test)]
pub(crate) fn drop_views(conn: &Connection) -> Result<()> {
    conn.execute_batch(DROP_VIEWS)?;
    Ok(())
}

const NOW: &str = "CAST(strftime('%s','now') AS INTEGER)";

/// Execution statuses that mean someone or something is working the item, or is
/// about to. `waiting_dependency` is excluded on purpose; see the module docs.
const LIVE_OR_PENDING: &str = "('queued','ready','claimed','running','waiting_human')";

/// Execution statuses of a progression run that died rather than reported.
const INFRASTRUCTURE_FAILED: &str = "('failed','orphaned','abandoned')";

fn limit_sql(key: &str, default: i64) -> String {
    format!("COALESCE((SELECT CAST(value AS INTEGER) FROM metadata WHERE key = '{key}'), {default})")
}

/// Open attention rows of the given kinds on the item or any of its executions.
fn open_attention_sql(a: &str, kind_predicate: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM work_attention_items wa WHERE wa.status = 'open' AND ({kind_predicate})
                 AND (wa.work_item_id = {a}.id
                      OR wa.execution_id IN (SELECT wx.id FROM work_executions wx WHERE wx.work_item_id = {a}.id)))"
    )
}

/// The stuck report's "waiting on a human" holds, in SQL. Mirrors
/// `stuck_report::human_hold`; the test module checks the two against the same
/// fixtures.
fn human_held_sql(a: &str) -> String {
    // COALESCE: a NULL `blocked_reason` makes several terms NULL, and a NULL hold
    // must read as "not held", not poison the clause that negates it.
    format!(
        "COALESCE({a}.deferred = 1 OR {a}.human_driven = 1
          OR {a}.blocked_reason IN ('awaiting_operator_answer', 'deletion_signoff')
          OR EXISTS (SELECT 1 FROM operator_questions oq WHERE oq.work_item_id = {a}.id AND oq.status = 'open')
          OR EXISTS (SELECT 1 FROM work_executions hx WHERE hx.work_item_id = {a}.id AND hx.status = 'waiting_human')
          OR ({a}.blocked_reason = 'worker_failed' AND COALESCE(
                (SELECT hb.basis FROM task_status_audit hb
                 WHERE hb.work_item_id = {a}.id AND hb.property = 'blocked_reason'
                 ORDER BY hb.id DESC LIMIT 1), '') = 'operator_no')
          OR ({a}.status = 'blocked' AND {a}.blocked_reason IS NULL AND COALESCE(
                (SELECT sa.actor FROM task_status_audit sa
                 WHERE sa.work_item_id = {a}.id AND sa.property = 'status'
                 ORDER BY sa.id DESC LIMIT 1), {a}.last_status_actor) <> 'engine')
          OR ({a}.status = 'todo' AND {a}.autostart = 0
              AND {a}.dispatch_failed_reason IS NULL AND {a}.blocked_reason IS NULL
              AND COALESCE((SELECT aa.actor FROM task_status_audit aa
                            WHERE aa.work_item_id = {a}.id AND aa.property = 'autostart'
                            ORDER BY aa.id DESC LIMIT 1), {a}.last_status_actor) <> 'engine'
              AND NOT EXISTS (SELECT 1 FROM work_executions qx
                              WHERE qx.work_item_id = {a}.id AND qx.status IN ('queued','ready'))), 0)"
    )
}

/// The stuck state an item is in, as the `S<n>_...` code the stuck report uses
/// (its `classification` prefix; `root` is the alias of the item's chain-root
/// task, joined by the caller), or NULL when the item is in no state a
/// progression agent addresses: a human hold (`S8`, `S12`, `S21`), a stalled or
/// paused queue (`S20`, `S23`), the merge queue (`S17`), a PR that is behind but
/// mergeable (`S18`), or simply healthy. The branches follow the order of
/// `stuck_report::classification` so the two never classify one item two ways.
fn stuck_kind_sql(a: &str, root: &str) -> String {
    let chain_root_ci = format!("{root}.ci_required_state");
    let trunk_queue = open_attention_sql(a, "wa.kind LIKE 'trunk_queue_%'");
    let abandoned = open_attention_sql(a, "wa.kind = 'abandoned_branch_no_pr'");
    let recovery = open_attention_sql(
        a,
        "wa.kind IN ('worker_recovery_permanent_error', 'worker_recovery_exhausted')",
    );
    let review_churn = open_attention_sql(a, "wa.kind = 'churn_guard_parked'");
    format!(
        "CASE
           WHEN {a}.dispatch_failed_reason IS NOT NULL THEN
               CASE {a}.dispatch_failed_reason
                   WHEN 'churn_guard' THEN 'S2_churn_guard'
                   WHEN 'deliberate_park' THEN 'S3_deliberate_park'
                   ELSE 'S1_dispatch_failure' END
           WHEN {a}.blocked_reason IS NOT NULL THEN
               CASE
                   WHEN {a}.blocked_reason IN ('worker_failed', 'spawning_attempt_retired_unsuccessfully')
                        AND {a}.kind = 'revision' AND {chain_root_ci} = 'success' THEN 'S13_revision_chain'
                   WHEN {a}.blocked_reason = 'worker_failed' THEN 'S7_worker_failed'
                   WHEN {a}.blocked_reason = 'dependency' THEN 'S4_dependency'
                   WHEN {a}.blocked_reason = 'execution_recovery_failed' THEN 'S9_execution_recovery'
                   WHEN {a}.blocked_reason = 'spawning_attempt_retired_unsuccessfully' THEN 'S10_retired_attempt'
                   WHEN {a}.blocked_reason = 'merge_conflict' THEN 'S5_merge_conflict'
                   WHEN {a}.blocked_reason IN ('ci_failure', 'ci_failure_exhausted') THEN 'S6_ci_failure'
                   WHEN {a}.blocked_reason = 'pr_binding_unresolved' THEN 'S11_pr_binding'
                   ELSE NULL
               END
           WHEN {a}.deferred = 1 OR ({a}.status = 'todo' AND {a}.autostart = 0) THEN NULL
           WHEN {trunk_queue} THEN NULL
           WHEN {a}.pr_url IS NULL AND {abandoned} THEN 'S22_abandoned_branch'
           WHEN {recovery} THEN 'S14_worker_recovery'
           WHEN {a}.status = 'in_review' AND {a}.ci_required_state = 'fail' THEN 'S6_ci_failure'
           WHEN {a}.status = 'in_review' AND {review_churn} THEN 'S16_review_churn'
           ELSE NULL
         END"
    )
}

/// When the item's current stuck state began, read from the task status ledger
/// the way the stuck report ages it: a failure from its own latest observation
/// (an unrelated edit must not make a week-old blocker look new), anything else
/// from the latest ledger event, and `updated_at` for a row the ledger has never
/// seen.
fn stuck_since_sql(a: &str) -> String {
    let age_property = format!(
        "CASE WHEN {a}.dispatch_failed_reason IS NOT NULL THEN 'dispatch_failed_reason'
              WHEN {a}.blocked_reason IS NOT NULL THEN 'blocked_reason' END"
    );
    format!(
        "COALESCE(
           (SELECT CAST(la.changed_at AS INTEGER) FROM task_status_audit la
            WHERE la.work_item_id = {a}.id AND (({age_property}) IS NULL OR la.property = ({age_property}))
            ORDER BY la.id DESC LIMIT 1),
           NULLIF(CAST({a}.updated_at AS INTEGER), 0),
           {NOW})"
    )
}

/// Clause 3: no queued execution of the item is waiting on slots, the cap, a
/// workspace, or a pause.
fn not_capacity_wait_sql(a: &str) -> String {
    let reasons = [
        WAITING_FOR_WORKSPACE_REASON,
        DISPATCH_WAIT_REASON_POOL_EXHAUSTED,
        DISPATCH_WAIT_REASON_DISPATCH_PAUSED,
        DISPATCH_WAIT_REASON_AUTOMATION_PAUSED,
        DISPATCH_WAIT_REASON_FOCUS_MODE,
    ]
    .iter()
    .map(|reason| format!("'{}'", reason.replace('\'', "''")))
    .collect::<Vec<_>>()
    .join(", ");
    format!(
        "NOT EXISTS (SELECT 1 FROM work_executions cx
                     WHERE cx.work_item_id = {a}.id AND cx.status IN ('queued','ready')
                       AND (cx.dispatch_wait_reason IN ({reasons})
                            OR cx.dispatch_wait_reason LIKE 'Waiting for an available worker (%'))"
    )
}

/// Clause 2: nothing live or pending anywhere in the item's family. `family` is
/// the alias of the item's `progression_family_keys` row, joined by the caller:
/// the family test is a join and a one-time `IN` set, not a correlated lookup,
/// because the chain walk behind it is a recursive query that must be evaluated
/// once per statement rather than once per candidate row.
fn no_live_work_sql(family: &str) -> String {
    format!("NOT COALESCE({family}.family_key IN (SELECT family_key FROM progression_live_families), 0)")
}

/// Clause 4: a dependency-blocked item is a candidate only when none of its open
/// prerequisites is being progressed or is the owner's decision to make.
fn dependency_clear_sql(a: &str) -> String {
    let parked = parked_task_sql("p");
    let held = human_held_sql("p");
    format!(
        "({a}.blocked_reason IS NOT 'dependency' OR NOT EXISTS (
            SELECT 1 FROM work_item_dependencies d
            WHERE d.dependent_id = {a}.id AND d.relation = 'blocks' AND (
                EXISTS (SELECT 1 FROM projects pj
                        WHERE pj.id = d.prerequisite_id AND pj.status NOT IN ('done', 'archived'))
                OR EXISTS (SELECT 1 FROM tasks p
                           WHERE p.id = d.prerequisite_id AND p.deleted_at IS NULL
                             AND p.status NOT IN ('done', 'archived')
                             AND ({parked}
                                  OR EXISTS (SELECT 1 FROM work_executions px
                                             WHERE px.work_item_id = p.id AND px.status IN {LIVE_OR_PENDING})
                                  OR NOT {held})))))"
    )
}

/// Whether the item is inside the cooldown after a progression run. The cooldown
/// is measured from the end of the run (its creation while it has not ended), a
/// run that was cancelled before it started does not start one, and a run that
/// judged the item not stuck (`no_changes_needed`) earns the longer cooldown.
fn in_cooldown_sql(a: &str, cooldown: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM work_executions px
                 WHERE px.work_item_id = {a}.id AND px.kind = 'progression' AND px.status <> 'cancelled'
                   AND COALESCE(CAST(px.finished_at AS INTEGER), CAST(px.created_at AS INTEGER))
                       + ({cooldown}) * (CASE WHEN px.run_done_outcome = 'no_changes_needed'
                                              THEN {NO_ACTION_COOLDOWN_MULTIPLIER} ELSE 1 END) > {NOW})"
    )
}

/// Whether the item's last [`INFRASTRUCTURE_FAILURE_CAP`] progression runs all
/// ended in infrastructure failure, so the sweep stops nominating it.
fn infrastructure_failing_sql(a: &str) -> String {
    (0..INFRASTRUCTURE_FAILURE_CAP)
        .map(|offset| {
            format!(
                "COALESCE((SELECT fx.status FROM work_executions fx
                           WHERE fx.work_item_id = {a}.id AND fx.kind = 'progression' AND fx.status <> 'cancelled'
                           ORDER BY fx.created_at DESC, fx.id DESC LIMIT 1 OFFSET {offset})
                          IN {INFRASTRUCTURE_FAILED}, 0)"
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// The progression-run facts about one item: whether a run is live or pending,
/// whether the item is inside the cooldown after a run, and whether the last
/// runs all died. They depend only on the item's own progression executions, so
/// the stuck report reads them from their own cheap view.
fn run_state_select(cooldown: &str) -> String {
    let in_cooldown = in_cooldown_sql("t", cooldown);
    let infrastructure_failing = infrastructure_failing_sql("t");
    format!(
        "EXISTS (SELECT 1 FROM work_executions lx
                 WHERE lx.work_item_id = t.id AND lx.kind = 'progression'
                   AND lx.status IN {LIVE_OR_PENDING}) AS progression_live,
         {in_cooldown} AS in_cooldown,
         ({infrastructure_failing}) AS infrastructure_failing"
    )
}

fn views_sql() -> String {
    let stuck_after = limit_sql(METADATA_KEY_STUCK_AFTER_SECS, DEFAULT_STUCK_AFTER_SECS);
    let cooldown = limit_sql(METADATA_KEY_COOLDOWN_SECS, DEFAULT_COOLDOWN_SECS);
    let parked = parked_task_sql("t");
    let human_held = human_held_sql("t");
    let stuck_kind = stuck_kind_sql("t", "root");
    let stuck_since = stuck_since_sql("t");
    let no_live_work = no_live_work_sql("fk");
    let not_capacity_wait = not_capacity_wait_sql("t");
    let dependency_clear = dependency_clear_sql("t");
    let run_state = run_state_select(&cooldown);
    format!(
        "{DROP_VIEWS}

         -- Every task with the root of its revision chain. A revision whose parent
         -- is missing roots at itself, as `chain_root` does.
         CREATE VIEW progression_chain_roots AS
         WITH RECURSIVE chain(task_id, root_id) AS (
             SELECT id, id FROM tasks
             WHERE kind <> 'revision' OR parent_task_id IS NULL
                OR parent_task_id NOT IN (SELECT id FROM tasks)
             UNION
             SELECT t.id, c.root_id FROM tasks t JOIN chain c ON t.parent_task_id = c.task_id
             WHERE t.kind = 'revision'
         )
         SELECT task_id, root_id FROM chain;

         -- Tasks that share a PR: a chain shares its root's PR, and unrelated
         -- items bound to the same PR share it too.
         CREATE VIEW progression_family_keys AS
         SELECT c.task_id AS task_id,
                c.root_id AS root_id,
                COALESCE(NULLIF(r.pr_url, ''), NULLIF(m.pr_url, ''), c.root_id) AS family_key
         FROM progression_chain_roots c
         JOIN tasks r ON r.id = c.root_id
         JOIN tasks m ON m.id = c.task_id;

         -- The families with live or pending work in them.
         CREATE VIEW progression_live_families AS
         SELECT DISTINCT k.family_key AS family_key
         FROM progression_family_keys k
         JOIN work_executions e ON e.work_item_id = k.task_id
         WHERE e.status IN {LIVE_OR_PENDING};

         CREATE VIEW progression_run_state AS
         SELECT t.id AS work_item_id, {run_state}
         FROM tasks t
         WHERE t.deleted_at IS NULL AND t.status NOT IN ('done', 'archived');

         CREATE VIEW progression_candidate_clauses AS
         SELECT s.work_item_id AS work_item_id,
                s.stuck_kind AS stuck_kind,
                s.stuck_since AS stuck_since,
                (s.grace_stamp IS NOT NULL AND CAST(s.grace_stamp AS INTEGER) >= s.stuck_since) AS grace_waived,
                s.c1_eligible AS c1_eligible,
                s.c2_no_live_work AS c2_no_live_work,
                s.c3_not_capacity_wait AS c3_not_capacity_wait,
                s.c4_dependency_clear AS c4_dependency_clear,
                (s.stuck_kind IS NOT NULL
                 AND ((s.grace_stamp IS NOT NULL AND CAST(s.grace_stamp AS INTEGER) >= s.stuck_since)
                      OR {NOW} - s.stuck_since >= {stuck_after})) AS c5_stuck_long_enough,
                (NOT s.in_cooldown AND NOT s.infrastructure_failing) AS c6_cooldown_and_budget_clear,
                s.progression_live AS progression_live,
                s.in_cooldown AS in_cooldown,
                s.infrastructure_failing AS infrastructure_failing
         FROM (
             SELECT t.id AS work_item_id,
                    {stuck_kind} AS stuck_kind,
                    {stuck_since} AS stuck_since,
                    t.progression_grace_waived_at AS grace_stamp,
                    (NOT {parked} AND NOT {human_held}) AS c1_eligible,
                    {no_live_work} AS c2_no_live_work,
                    {not_capacity_wait} AS c3_not_capacity_wait,
                    {dependency_clear} AS c4_dependency_clear,
                    {run_state}
             FROM tasks t
             LEFT JOIN progression_family_keys fk ON fk.task_id = t.id
             LEFT JOIN tasks root ON root.id = fk.root_id
             WHERE t.deleted_at IS NULL AND t.status NOT IN ('done', 'archived')
         ) s;

         CREATE VIEW progression_candidates AS
         SELECT work_item_id, stuck_kind, stuck_since, grace_waived
         FROM progression_candidate_clauses
         WHERE c1_eligible AND c5_stuck_long_enough AND c6_cooldown_and_budget_clear
           AND c3_not_capacity_wait AND c4_dependency_clear AND c2_no_live_work;"
    )
}

/// One row of `progression_candidates`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressionCandidate {
    pub work_item_id: String,
    /// The `S<n>_...` stuck-state code, the prefix of the stuck report's
    /// classification.
    pub stuck_kind: String,
    /// Epoch seconds the item's current stuck state began.
    pub stuck_since: i64,
    /// The item was nominated before `progression.stuck_after` because a
    /// producer waived the grace.
    pub grace_waived: bool,
}

/// One row of `progression_candidate_clauses`: every clause of the predicate for
/// one open item, and the facts behind the buckets that name progression.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ProgressionClauses {
    pub work_item_id: String,
    /// The stuck state, or `None` when the item is in none a progression agent
    /// addresses.
    pub stuck_kind: Option<String>,
    pub stuck_since: i64,
    pub grace_waived: bool,
    /// Clause 1: not parked and not a human hold.
    pub eligible_item: bool,
    /// Clause 2: nothing live or pending on the item, its revisions or its PR.
    pub no_live_work: bool,
    /// Clause 3: not waiting on capacity.
    pub not_capacity_wait: bool,
    /// Clause 4: no prerequisite is carrying the item.
    pub dependency_clear: bool,
    /// Clause 5: in a stuck state for long enough, or the grace was waived.
    pub stuck_long_enough: bool,
    /// Clause 6: outside the cooldown and under the failure budget.
    pub cooldown_and_budget_clear: bool,
    /// A progression execution of the item is live or pending.
    pub progression_live: bool,
    /// Inside the cooldown after a progression run.
    pub in_cooldown: bool,
    /// The last progression runs all ended in infrastructure failure.
    pub infrastructure_failing: bool,
}

impl ProgressionClauses {
    /// Every clause holds: the item is a progression candidate.
    pub fn is_candidate(&self) -> bool {
        self.failing_clauses().is_empty()
    }

    /// The clauses that do not hold, as `(number, plain-words reason)`.
    pub fn failing_clauses(&self) -> Vec<(u8, &'static str)> {
        [
            (
                1,
                self.eligible_item,
                "parked, or waiting on a human (a question, a refusal, or a decision only the owner can make)",
            ),
            (
                2,
                self.no_live_work,
                "work is already live or pending on the item, its revisions, or its PR",
            ),
            (
                3,
                self.not_capacity_wait,
                "waiting for a worker slot, the cap, or a pause",
            ),
            (
                4,
                self.dependency_clear,
                "waiting on a prerequisite that is being worked or is the owner's decision",
            ),
            (
                5,
                self.stuck_long_enough,
                "not in a stuck state a progression agent addresses, or not stuck for long enough yet",
            ),
            (
                6,
                self.cooldown_and_budget_clear,
                "inside the cooldown after a progression run, or progression failed twice in a row",
            ),
        ]
        .into_iter()
        .filter(|(_, holds, _)| !holds)
        .map(|(number, _, reason)| (number, reason))
        .collect()
    }
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Publish the two limits the candidate views read, so every reader of the
    /// views (the sweep, the stuck report, `bossctl dispatch diagnose` with the
    /// engine down) applies the numbers this engine was started with.
    pub fn publish_progression_limits(&self, settings: &ProgressionSettings) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (key, value) in [
            (METADATA_KEY_STUCK_AFTER_SECS, settings.stuck_after_secs),
            (METADATA_KEY_COOLDOWN_SECS, settings.cooldown_secs),
        ] {
            tx.execute(
                "INSERT INTO metadata (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The progression candidates, oldest-stuck first. `limit` bounds how many
    /// are returned; `None` returns them all.
    pub fn list_progression_candidates(&self, limit: Option<usize>) -> Result<Vec<ProgressionCandidate>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT work_item_id, stuck_kind, stuck_since, grace_waived FROM progression_candidates
             ORDER BY stuck_since ASC, work_item_id ASC LIMIT ?1",
        )?;
        let limit = limit.map_or(-1, |n| i64::try_from(n).unwrap_or(i64::MAX));
        let rows = stmt.query_map([limit], read_candidate)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every clause of the predicate for one item, or `None` when the item is
    /// not an open, non-deleted task (finished, deleted, a project, or unknown).
    pub fn progression_clauses(&self, work_item_id: &str) -> Result<Option<ProgressionClauses>> {
        let conn = self.connect()?;
        read_clauses(&conn, work_item_id)
    }

    /// Mint a `ready` progression execution for `work_item_id`, bound to the
    /// stuck item itself, if the item is *still* a candidate. The check and the
    /// insert share one transaction, so an item that a worker, a revision or an
    /// operator picked up since it was listed is not minted a second writer.
    /// Consumes the item's grace-waiver stamp. Returns `None` when the item is
    /// no longer a candidate.
    ///
    /// Re-evaluating the predicate for one item costs about as much as listing
    /// every candidate, so a caller with many items uses
    /// [`Self::mint_progression_executions`] instead.
    pub fn mint_progression_execution(&self, work_item_id: &str) -> Result<Option<WorkExecution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let still_candidate: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM progression_candidates WHERE work_item_id = ?1)",
            [work_item_id],
            |row| row.get(0),
        )?;
        if !still_candidate {
            return Ok(None);
        }
        let execution = insert_progression_execution(&tx, work_item_id)?;
        tx.commit()?;
        Ok(Some(execution))
    }

    /// Mint a `ready` progression execution for every current candidate, oldest
    /// stuck first, in one transaction. The candidates are read inside the
    /// transaction that inserts the executions, so none can be picked up by
    /// other work between being listed and being minted, and the write lock is
    /// held for one evaluation of the predicate rather than one per item. An
    /// item whose execution cannot be inserted (for example its repository
    /// cannot be resolved) is skipped and reported; it does not stop the rest.
    pub fn mint_progression_executions(&self) -> Result<ProgressionMint> {
        let mut conn = self.connect()?;
        let mut tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidates = {
            let mut stmt = tx.prepare(
                "SELECT work_item_id, stuck_kind, stuck_since, grace_waived FROM progression_candidates
                 ORDER BY stuck_since ASC, work_item_id ASC",
            )?;
            let rows = stmt.query_map([], read_candidate)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut mint = ProgressionMint::default();
        for candidate in candidates {
            let savepoint = tx.savepoint()?;
            match insert_progression_execution(&savepoint, &candidate.work_item_id) {
                Ok(execution) => {
                    savepoint.commit()?;
                    mint.minted.push((candidate, execution));
                }
                Err(error) => {
                    // Dropping the savepoint rolls this item back.
                    drop(savepoint);
                    mint.failed.push((candidate, format!("{error:#}")));
                }
            }
        }
        tx.commit()?;
        Ok(mint)
    }

    /// Waive the `progression.stuck_after` grace for `work_item_id` once: the
    /// sweep nominates it as soon as it is otherwise a candidate, and consumes
    /// the stamp when it does. Used when an owner un-parks an item. Stamps with
    /// the database clock so the stamp can never read as older than the ledger
    /// row written in the same statement.
    pub fn waive_progression_grace(&self, work_item_id: &str) -> Result<()> {
        let conn = self.connect()?;
        waive_grace(&conn, work_item_id)?;
        Ok(())
    }
}

/// What one [`WorkDb::mint_progression_executions`] pass did.
#[derive(Debug, Default)]
pub struct ProgressionMint {
    /// Candidates a `ready` progression execution was minted for, oldest stuck
    /// first.
    pub minted: Vec<(ProgressionCandidate, WorkExecution)>,
    /// Candidates the insert failed for, with the reason.
    pub failed: Vec<(ProgressionCandidate, String)>,
}

fn read_candidate(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProgressionCandidate> {
    Ok(ProgressionCandidate {
        work_item_id: row.get(0)?,
        stuck_kind: row.get(1)?,
        stuck_since: row.get(2)?,
        grace_waived: row.get(3)?,
    })
}

/// Insert the `ready` progression execution for `work_item_id` and consume the
/// item's grace-waiver stamp. The caller has established that the item is a
/// candidate, inside the transaction `conn` belongs to.
fn insert_progression_execution(conn: &Connection, work_item_id: &str) -> Result<WorkExecution> {
    let execution = insert_execution(
        conn,
        CreateExecutionInput::builder()
            .work_item_id(work_item_id)
            .kind(ExecutionKind::Progression)
            .status(ExecutionStatus::Ready)
            .build(),
    )?;
    conn.execute(
        "UPDATE work_executions SET created_via = ?2 WHERE id = ?1",
        params![execution.id, CREATED_VIA_PROGRESSION_SWEEP],
    )?;
    conn.execute(
        "UPDATE tasks SET progression_grace_waived_at = NULL WHERE id = ?1",
        [work_item_id],
    )?;
    Ok(execution)
}

/// Stamp `tasks.progression_grace_waived_at`. A stamp older than the item's
/// current stuck state is ignored by the views, so a stale one is harmless.
pub(crate) fn waive_grace(conn: &Connection, work_item_id: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE tasks SET progression_grace_waived_at = strftime('%s','now') WHERE id = ?1",
        [work_item_id],
    )
}

/// A progression run's footprint on one item; see `progression_run_state`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RunState {
    /// A progression execution of the item is live or pending.
    pub live: bool,
    /// The item is inside the cooldown after a progression run.
    pub in_cooldown: bool,
    /// The last progression runs all ended in infrastructure failure.
    pub infrastructure_failing: bool,
}

/// The run state of every open item that has any progression footprint.
pub(crate) fn run_states(conn: &Connection) -> Result<std::collections::HashMap<String, RunState>> {
    let mut stmt = conn.prepare(
        "SELECT work_item_id, progression_live, in_cooldown, infrastructure_failing
         FROM progression_run_state
         WHERE progression_live OR in_cooldown OR infrastructure_failing",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            RunState {
                live: row.get(1)?,
                in_cooldown: row.get(2)?,
                infrastructure_failing: row.get(3)?,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn read_clauses(conn: &Connection, work_item_id: &str) -> Result<Option<ProgressionClauses>> {
    Ok(conn
        .query_row(
            "SELECT work_item_id, stuck_kind, stuck_since, grace_waived, c1_eligible, c2_no_live_work,
                    c3_not_capacity_wait, c4_dependency_clear, c5_stuck_long_enough,
                    c6_cooldown_and_budget_clear, progression_live, in_cooldown, infrastructure_failing
             FROM progression_candidate_clauses WHERE work_item_id = ?1",
            [work_item_id],
            |row| {
                Ok(ProgressionClauses::builder()
                    .work_item_id(row.get::<_, String>(0)?)
                    .maybe_stuck_kind(row.get::<_, Option<String>>(1)?)
                    .stuck_since(row.get(2)?)
                    .grace_waived(row.get(3)?)
                    .eligible_item(row.get(4)?)
                    .no_live_work(row.get(5)?)
                    .not_capacity_wait(row.get(6)?)
                    .dependency_clear(row.get(7)?)
                    .stuck_long_enough(row.get(8)?)
                    .cooldown_and_budget_clear(row.get(9)?)
                    .progression_live(row.get(10)?)
                    .in_cooldown(row.get(11)?)
                    .infrastructure_failing(row.get(12)?)
                    .build())
            },
        )
        .optional()?)
}
