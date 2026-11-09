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
//! - `progression_candidate_base` is every clause but 4 plus
//!   `candidate_ex_dependency`. Clause 4 does not treat that flag as a
//!   prerequisite's candidacy: a prerequisite held only by its own clause 4 is
//!   not a candidate and does not hold its dependent.
//! - `progression_prereq_task_edges` expands each `blocks` edge to task
//!   prerequisites (a project becomes its open members, excluding the
//!   dependent). `progression_real_candidates` walks that DAG bottom-up so
//!   each item's clause 4 sees its prerequisites' real candidacy. The result
//!   is a non-correlated set tested with `IN`.
//! - `progression_chain_roots`, `progression_family_keys`,
//!   `progression_live_families`, `progression_live_execution_tasks`,
//!   `progression_live_tasks` and `progression_ladder_families` are the
//!   helpers that give clause 2 its "item, its revisions and its PR chain"
//!   reach. They are non-correlated sets tested with `IN`, so the recursive
//!   chain walk runs once per statement, not once per candidate row (the sweep
//!   evaluates the view inside an `IMMEDIATE` transaction that holds the
//!   writer lock). The same is true of the clause-4 candidacy walk.
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
//! 2. **No live or pending work** (`queued | ready | waiting_dependency |
//!    claimed | running | waiting_human`) on the item, on its revisions, or on
//!    any item sharing its PR, and no in-flight `ci_remediations` /
//!    `conflict_resolutions` attempt on that family whose revision is missing or
//!    still open. `waiting_dependency` counts everywhere in the family, as
//!    design 2.1 specifies. A dead revision head that a later revision is
//!    serialized behind (S13) is therefore not nominated while that gate row
//!    exists; the engine-side fix that stops gating behind a dead head owns that
//!    shape. An in-flight ladder attempt is named separately in
//!    [`ProgressionClauses::no_inflight_ladder`] so diagnose can say which half
//!    of the clause failed.
//! 3. **Not waiting on capacity.** No queued execution of the item carries a
//!    slot, cap, workspace or pause wait reason. Waiting for bandwidth is not
//!    stuck.
//! 4. **Dependency clear.** A `blocked: dependency` item is progressed through
//!    its prerequisite, not itself, when that prerequisite is live, parked, or
//!    itself a progression candidate (every clause, including the prerequisite's
//!    own clause 4). A project prerequisite counts when it is parked or any
//!    open member task other than the dependent is live, parked, or such a
//!    candidate. The dependent is excluded from every prerequisite-side test, so
//!    its own candidacy, its own live executions (including a progression run
//!    the sweep just minted) and a `blocks` edge onto its own project cannot
//!    hold it. A stuck prerequisite that grace, cooldown, the create budget or
//!    the failure cap keeps from being nominated is not a candidate and does
//!    not hold the dependent; nor does an idle, unparked one, a human-held one,
//!    a dangling edge, or a middle item held only by *its* prerequisite. The
//!    stuck report reads the same value as `dependency_held`.
//! 5. **Stuck long enough.** In one of the stuck states the enumeration marks as
//!    progression-eligible ([`stuck_kind_sql`]) continuously for at least
//!    `progression.stuck_after`, measured from the task status ledger exactly as
//!    the stuck report ages it; or the grace was waived since the item entered
//!    that state (`tasks.progression_grace_waived_at`).
//! 6. **Cooldown and budget.** No progression run within `progression.cooldown`
//!    (four times that after a `no_changes_needed` run), the last
//!    [`INFRASTRUCTURE_FAILURE_CAP`] runs were not all infrastructure failures,
//!    and the item's create budget (`progression.max_creates_per_item_window`
//!    revisions-plus-precursors in the rolling window) is not exhausted. A run
//!    that is already in flight still sees the same cap at the verbs (exhaustion
//!    there is a refusal so the agent can ask); an item that arrives at the
//!    sweep already over budget is not nominated again.
//!
//! ## Keeping the views current
//!
//! [`views_sql`] interpolates the same Rust helpers the stuck report uses
//! (`stuck_kind_sql`, `human_held_sql`, the wait-reason constants, the
//! infrastructure-failure cap, the cooldown multiplier, [`parked_task_sql`]).
//! The named migration creates the views once; [`WorkDb::init`] then recreates
//! them on every open after migrations so an existing `state.db` always
//! evaluates the predicate this binary ships, not the text frozen at first
//! migrate. A later edit to those helpers is therefore a behaviour change on
//! existing databases without a new migration. Tests that drop the views to
//! simulate an older schema get them back on reopen.
//!
//! ## Limits
//!
//! `progression.stuck_after`, `progression.cooldown`, and the create-budget
//! pair are read from the `metadata` table (the engine publishes them with
//! [`WorkDb::publish_progression_limits`]); an absent key falls back to the
//! design's placeholder, so the views are usable by a process that never ran the
//! engine, such as `bossctl` against a copied database.
use std::collections::HashSet;

use super::*;
use crate::parking::parked_task_sql;
use boss_engine_storage_values::dispatch_wait::{
    DISPATCH_WAIT_REASON_AUTOMATION_PAUSED, DISPATCH_WAIT_REASON_DISPATCH_PAUSED, DISPATCH_WAIT_REASON_FOCUS_MODE,
    DISPATCH_WAIT_REASON_POOL_EXHAUSTED, WAITING_FOR_WORKSPACE_REASON,
};
use boss_engine_storage_values::progression::{
    CREATED_VIA_PROGRESSION_SWEEP, DEFAULT_COOLDOWN_SECS, DEFAULT_MAX_CREATES_PER_ITEM_WINDOW,
    DEFAULT_MAX_CREATES_WINDOW_SECS, DEFAULT_STUCK_AFTER_SECS, INFRASTRUCTURE_FAILURE_CAP, METADATA_KEY_COOLDOWN_SECS,
    METADATA_KEY_MAX_CREATES_PER_ITEM_WINDOW, METADATA_KEY_MAX_CREATES_WINDOW_SECS, METADATA_KEY_RUN_TIMEOUT_SECS,
    METADATA_KEY_STUCK_AFTER_SECS, NO_ACTION_COOLDOWN_MULTIPLIER, ProgressionSettings,
};

pub(super) const GRACE_COLUMN_MIGRATION_ID: &str = "20261106_1200_progression_grace_waived_at";
pub(super) const EXECUTION_PROVENANCE_MIGRATION_ID: &str = "20261106_1205_execution_created_via";
pub(super) const VIEWS_MIGRATION_ID: &str = "20261106_1210_progression_candidate_views";

/// Add `work_executions.created_via`: which producer minted the execution. Only
/// the `progression_sweep` sets it today; a NULL value means an execution minted
/// by the ordinary reconcile and request paths.
pub(super) fn migrate_execution_provenance(conn: &Connection) -> Result<()> {
    if !crate::schema_init::table_has_column(conn, "work_executions", "created_via")? {
        conn.execute_batch("ALTER TABLE work_executions ADD COLUMN created_via TEXT")?;
    }
    Ok(())
}

/// Apply function of the shipped `20261106_1210` registry entry. The id stays so
/// databases that recorded it keep a consistent history, but the entry creates
/// nothing: the views are not schema owned by the migration sequence. If it
/// created them, they would exist while every later migration runs, and a
/// migration that rebuilds a table they read would fail with "error in view
/// progression_...". [`WorkDb::init`] creates them after the runner instead.
pub(super) fn migrate_views_noop(_conn: &Connection) -> Result<()> {
    Ok(())
}

/// (Re)create the candidate views. Idempotent drop-then-create.
/// [`WorkDb::init`] calls this after migrations on every open so a database
/// evaluates the predicate this binary ships (the helpers interpolated into
/// [`views_sql`] are live Rust, not frozen SQL).
pub(super) fn migrate_views(conn: &Connection) -> Result<()> {
    conn.execute_batch(&views_sql())?;
    Ok(())
}

/// Every view this module defines, dropped dependents first.
const DROP_VIEWS: &str = "DROP VIEW IF EXISTS progression_candidates;
         DROP VIEW IF EXISTS progression_candidate_clauses;
         DROP VIEW IF EXISTS progression_real_candidates;
         DROP VIEW IF EXISTS progression_candidate_base;
         DROP VIEW IF EXISTS progression_run_state;
         DROP VIEW IF EXISTS progression_live_tasks;
         DROP VIEW IF EXISTS progression_live_execution_tasks;
         DROP VIEW IF EXISTS progression_ladder_families;
         DROP VIEW IF EXISTS progression_live_families;
         DROP VIEW IF EXISTS progression_family_keys;
         DROP VIEW IF EXISTS progression_chain_roots;
         DROP VIEW IF EXISTS progression_prereq_task_edges;";

/// Drop the views. SQLite re-parses every view when an `ALTER TABLE` runs, so
/// [`WorkDb::init`] drops them before the migration runner and recreates them
/// from [`views_sql`] afterwards; a test that simulates an older database by
/// dropping a table they read drops them first too.
pub(crate) fn drop_views(conn: &Connection) -> Result<()> {
    conn.execute_batch(DROP_VIEWS)?;
    Ok(())
}

const NOW: &str = "CAST(strftime('%s','now') AS INTEGER)";

/// Execution statuses that mean someone or something is working the item, or is
/// about to (design 2.1 clause 2), including a `waiting_dependency` gate.
const LIVE_OR_PENDING: &str = "('queued','ready','waiting_dependency','claimed','running','waiting_human')";

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

/// The "waiting on a human" holds, in SQL. This is the one definition: the stuck
/// report reads clause 1 from the clause view rather than keeping a Rust copy.
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

/// Clause 2, execution half: nothing live or pending anywhere in the item's
/// family, `waiting_dependency` included (design 2.1). `family` is the alias of
/// the item's `progression_family_keys` row, joined by the caller. The family
/// test is a one-time `IN` set, not a correlated lookup, because the chain walk
/// behind it is a recursive query that must be evaluated once per statement
/// rather than once per candidate row.
fn no_live_work_sql(family: &str) -> String {
    format!("NOT COALESCE({family}.family_key IN (SELECT family_key FROM progression_live_families), 0)")
}

/// Clause 2, ladder half: no in-flight `ci_remediations` or
/// `conflict_resolutions` attempt on the family whose revision is missing or
/// still open. An attempt that has already finished, or whose revision is
/// `done`/`archived`, does not exclude the item even if a live execution is
/// also absent — that is the window the generic live-work check cannot see.
/// Like clause 2's execution half this is a one-time `IN` set over
/// `progression_ladder_families`.
fn no_inflight_ladder_sql(family: &str) -> String {
    format!("NOT COALESCE({family}.family_key IN (SELECT family_key FROM progression_ladder_families), 0)")
}

/// The view behind [`no_inflight_ladder_sql`]: families with an attempt in
/// either ladder table that is `pending` or `running` and whose revision is
/// missing or still open.
fn ladder_families_select() -> String {
    let attempt = |table: &str| {
        format!(
            "SELECT rk.family_key AS family_key
             FROM {table} r
             JOIN progression_family_keys rk ON rk.task_id = r.work_item_id
             LEFT JOIN tasks rt ON rt.id = r.revision_task_id AND rt.deleted_at IS NULL
             WHERE r.status IN ('pending', 'running')
               AND (r.revision_task_id IS NULL
                    OR (rt.id IS NOT NULL AND rt.status NOT IN ('done', 'archived')))"
        )
    };
    format!(
        "{} UNION {}",
        attempt("ci_remediations"),
        attempt("conflict_resolutions")
    )
}

/// True when `task` holds dependents of `excluded`: it is parked, something in
/// its PR family other than `excluded` is live or pending, or it is a real
/// progression candidate (clause 4 resolved through the DAG).
fn task_holds_dependent_sql(task: &str, excluded: &str) -> String {
    let parked = parked_task_sql(task);
    format!(
        "({parked}
          OR EXISTS (
              SELECT 1 FROM progression_family_keys pk
              JOIN progression_family_keys other ON other.family_key = pk.family_key
              WHERE pk.task_id = {task}.id
                AND other.task_id <> {excluded}.id
                AND other.task_id IN (SELECT work_item_id FROM progression_live_execution_tasks))
          OR {task}.id IN (SELECT work_item_id FROM progression_real_candidates))"
    )
}

/// Clause 4, the raw test: some open prerequisite of the item is live, parked,
/// or itself a progression candidate, so progress goes through it rather than
/// through the item (design 2.1). It does not look at the item's own
/// `blocked_reason`; [`dependency_clear_sql`] applies that guard, and the stuck
/// report reads this value as `dependency_held`.
///
/// - A **task** prerequisite holds when it is parked, when anything in its PR
///   family *other than the dependent* is live or pending, or when it is a
///   real candidate (`progression_real_candidates`, which includes the
///   prerequisite's own clause 4). A stuck prerequisite that grace, cooldown,
///   the create budget or the infrastructure-failure cap keeps from being
///   nominated is *not* a candidate, so it does not hold the dependent. A
///   middle item held only by *its* prerequisite is not a candidate either.
/// - A **project** prerequisite holds when it is parked, or when any open
///   member task *other than the dependent* is live (independently of the
///   dependent), parked, or such a candidate. An open project whose members
///   are all idle and not stuck holds nothing: the dependent is then the
///   candidate and can drop the edge or ask.
///
/// The dependent is excluded from every prerequisite-side test: its own
/// candidacy, its own live executions, and a `blocks` edge onto its own
/// project cannot hold it. A human-held prerequisite, an idle one and a
/// dangling edge hold nothing.
fn dependency_held_sql(a: &str) -> String {
    let member_holds = task_holds_dependent_sql("m", a);
    let prereq_holds = task_holds_dependent_sql("p", a);
    format!(
        "EXISTS (
            SELECT 1 FROM work_item_dependencies d
            WHERE d.dependent_id = {a}.id AND d.relation = 'blocks' AND (
                EXISTS (SELECT 1 FROM projects pj
                        WHERE pj.id = d.prerequisite_id AND pj.status NOT IN ('done', 'archived')
                          AND (pj.parked_at IS NOT NULL
                               OR EXISTS (SELECT 1 FROM tasks m
                                          WHERE m.project_id = pj.id AND m.deleted_at IS NULL
                                            AND m.status NOT IN ('done', 'archived')
                                            AND m.id <> {a}.id
                                            AND {member_holds})))
                OR EXISTS (SELECT 1 FROM tasks p
                           WHERE p.id = d.prerequisite_id AND p.deleted_at IS NULL
                             AND p.status NOT IN ('done', 'archived')
                             AND p.id <> {a}.id
                             AND {prereq_holds})))"
    )
}

/// `blocks` edges expanded to task prerequisites. A project prerequisite
/// becomes each of its open members except the dependent, so a task that
/// depends on its own project is not held by itself. Parked projects are
/// omitted here: they hold dependents directly, not through members.
fn prereq_task_edges_sql() -> &'static str {
    "SELECT d.dependent_id AS dependent_id, d.prerequisite_id AS prerequisite_task_id
             FROM work_item_dependencies d
             JOIN tasks p ON p.id = d.prerequisite_id
             WHERE d.relation = 'blocks'
               AND p.deleted_at IS NULL AND p.status NOT IN ('done', 'archived')
             UNION
             SELECT d.dependent_id, m.id
             FROM work_item_dependencies d
             JOIN projects pj ON pj.id = d.prerequisite_id
               AND pj.status NOT IN ('done', 'archived') AND pj.parked_at IS NULL
             JOIN tasks m ON m.project_id = pj.id AND m.deleted_at IS NULL
               AND m.status NOT IN ('done', 'archived') AND m.id <> d.dependent_id
             WHERE d.relation = 'blocks'"
}

/// Real progression candidates: `candidate_ex_dependency` and clause 4, with
/// each prerequisite's own clause 4 resolved bottom-up over the acyclic
/// `blocks` DAG. Live and parked items are holding too, so a live or parked
/// prerequisite keeps its dependent out of this set. The walk is one
/// statement: a topological pass that accumulates holding ids, then an `IN`
/// set for clause 4.
fn real_candidates_view_sql() -> String {
    let parked = parked_task_sql("t");
    format!(
        "CREATE VIEW progression_real_candidates AS
         WITH RECURSIVE
         paths(id, d) AS (
             SELECT t.id, 0
             FROM tasks t
             WHERE t.deleted_at IS NULL AND t.status NOT IN ('done', 'archived')
               AND NOT EXISTS (
                   SELECT 1 FROM progression_prereq_task_edges e WHERE e.dependent_id = t.id)
             UNION
             SELECT e.dependent_id, paths.d + 1
             FROM paths
             JOIN progression_prereq_task_edges e ON e.prerequisite_task_id = paths.id
             WHERE paths.d < (SELECT COUNT(*) FROM tasks)
         ),
         topo AS (
             SELECT t.id AS id,
                    ROW_NUMBER() OVER (
                        ORDER BY COALESCE(p.max_d, 1000000), t.id
                    ) - 1 AS ord
             FROM tasks t
             LEFT JOIN (SELECT id, MAX(d) AS max_d FROM paths GROUP BY id) p ON p.id = t.id
             WHERE t.deleted_at IS NULL AND t.status NOT IN ('done', 'archived')
         ),
         walk(ord, holding) AS (
             SELECT -1, ''
             UNION ALL
             SELECT tord.ord,
                    CASE
                      WHEN t.id IN (SELECT task_id FROM progression_live_tasks)
                           OR {parked}
                           OR (COALESCE(b.candidate_ex_dependency, 0)
                               AND (t.blocked_reason IS NOT 'dependency'
                                    OR (NOT EXISTS (
                                            SELECT 1 FROM work_item_dependencies d
                                            JOIN projects pj ON pj.id = d.prerequisite_id
                                            WHERE d.dependent_id = t.id AND d.relation = 'blocks'
                                              AND pj.parked_at IS NOT NULL
                                              AND pj.status NOT IN ('done', 'archived'))
                                        AND NOT EXISTS (
                                            SELECT 1 FROM progression_prereq_task_edges e
                                            WHERE e.dependent_id = t.id
                                              AND INSTR(',' || walk.holding || ',',
                                                        ',' || e.prerequisite_task_id || ',') > 0))))
                      THEN walk.holding || ',' || t.id
                      ELSE walk.holding
                    END
             FROM walk
             JOIN topo tord ON tord.ord = walk.ord + 1
             JOIN tasks t ON t.id = tord.id
             LEFT JOIN progression_candidate_base b ON b.work_item_id = t.id
         )
         SELECT b.work_item_id AS work_item_id
         FROM progression_candidate_base b
         JOIN walk w ON w.ord = (SELECT MAX(ord) FROM topo)
         WHERE b.candidate_ex_dependency
           AND INSTR(',' || w.holding || ',', ',' || b.work_item_id || ',') > 0"
    )
}

/// Clause 6, create-budget half: revisions and precursors a progression run
/// minted for this item inside the rolling window, identified by
/// `created_via LIKE 'progression:%'` (design 2.4).
fn create_budget_exhausted_sql(a: &str, max_creates: &str, window: &str) -> String {
    // Two indexed arms (children by parent, precursors by dependent) rather than
    // one scan of every task with an OR across both relations.
    format!(
        "(SELECT COUNT(*) FROM (
              SELECT created.id FROM tasks created
              WHERE created.parent_task_id = {a}.id
                AND created.created_via LIKE 'progression:%'
                AND created.deleted_at IS NULL
                AND CAST(created.created_at AS INTEGER) > {NOW} - ({window})
              UNION
              SELECT created.id FROM work_item_dependencies d
              JOIN tasks created ON created.id = d.prerequisite_id
              WHERE d.dependent_id = {a}.id
                AND created.created_via LIKE 'progression:%'
                AND created.deleted_at IS NULL
                AND CAST(created.created_at AS INTEGER) > {NOW} - ({window})))
         >= ({max_creates})"
    )
}

/// Whether the item is inside the cooldown after a progression run. The cooldown
/// is measured from the end of the run (its creation while it has not ended), a
/// run that was cancelled before it started does not start one, a run the engine
/// withdrew before it ever started (`abandoned` with no `started_at`) is not an
/// attempt at all, while a never-started `failed` or `orphaned` run still counts
/// (a spawn that fails before start is a genuine infrastructure failure, and not
/// counting it would let the sweep re-nominate the item every tick), and a
/// run that judged the item not stuck (`no_changes_needed`) earns the longer cooldown. `completed_only` restricts
/// the runs to ones that reached `completed`: the cooldown the stuck report
/// treats as "being handled" (design 3.2), as opposed to the broader one that
/// only throttles nomination.
fn in_cooldown_sql(a: &str, cooldown: &str, completed_only: bool) -> String {
    let status_filter = if completed_only {
        "px.status = 'completed'"
    } else {
        "px.status <> 'cancelled' AND NOT (px.status = 'abandoned' AND px.started_at IS NULL)"
    };
    format!(
        "EXISTS (SELECT 1 FROM work_executions px
                 WHERE px.work_item_id = {a}.id AND px.kind = 'progression' AND {status_filter}
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
                             AND NOT (fx.status = 'abandoned' AND fx.started_at IS NULL)
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
    let in_cooldown = in_cooldown_sql("t", cooldown, false);
    let in_completed_cooldown = in_cooldown_sql("t", cooldown, true);
    let infrastructure_failing = infrastructure_failing_sql("t");
    format!(
        "EXISTS (SELECT 1 FROM work_executions lx
                 WHERE lx.work_item_id = t.id AND lx.kind = 'progression'
                   AND lx.status IN {LIVE_OR_PENDING}) AS progression_live,
         {in_cooldown} AS in_cooldown,
         {in_completed_cooldown} AS in_completed_cooldown,
         ({infrastructure_failing}) AS infrastructure_failing"
    )
}

fn views_sql() -> String {
    let stuck_after = limit_sql(METADATA_KEY_STUCK_AFTER_SECS, DEFAULT_STUCK_AFTER_SECS);
    let cooldown = limit_sql(METADATA_KEY_COOLDOWN_SECS, DEFAULT_COOLDOWN_SECS);
    let max_creates = limit_sql(
        METADATA_KEY_MAX_CREATES_PER_ITEM_WINDOW,
        i64::from(DEFAULT_MAX_CREATES_PER_ITEM_WINDOW),
    );
    let create_window = limit_sql(METADATA_KEY_MAX_CREATES_WINDOW_SECS, DEFAULT_MAX_CREATES_WINDOW_SECS);
    let parked = parked_task_sql("t");
    let human_held = human_held_sql("t");
    let stuck_kind = stuck_kind_sql("t", "root");
    let stuck_since = stuck_since_sql("t");
    let no_live_work = no_live_work_sql("fk");
    let no_inflight_ladder = no_inflight_ladder_sql("fk");
    let not_capacity_wait = not_capacity_wait_sql("t");
    let dependency_held = dependency_held_sql("t");
    let ladder_families = ladder_families_select();
    let c5 = format!(
        "(s.stuck_kind IS NOT NULL
                 AND ((s.grace_stamp IS NOT NULL AND CAST(s.grace_stamp AS INTEGER) >= s.stuck_since)
                      OR {NOW} - s.stuck_since >= {stuck_after}))"
    );
    let c6 = "(NOT s.in_cooldown AND NOT s.infrastructure_failing AND NOT s.create_budget_exhausted)";
    let create_budget_exhausted = create_budget_exhausted_sql("t", &max_creates, &create_window);
    let run_state = run_state_select(&cooldown);
    let prereq_edges = prereq_task_edges_sql();
    let real_candidates = real_candidates_view_sql();
    format!(
        "{DROP_VIEWS}

         -- `blocks` edges expanded to task ids (project members except the
         -- dependent). The real-candidate walk is over this DAG.
         CREATE VIEW progression_prereq_task_edges AS
         {prereq_edges};

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

         -- The families with live or pending work in them, `waiting_dependency`
         -- gates included (design 2.1 clause 2).
         CREATE VIEW progression_live_families AS
         SELECT DISTINCT k.family_key AS family_key
         FROM progression_family_keys k
         JOIN work_executions e ON e.work_item_id = k.task_id
         WHERE e.status IN {LIVE_OR_PENDING};

         -- The families with an in-flight CI-fix or conflict-resolution attempt.
         CREATE VIEW progression_ladder_families AS
         {ladder_families};

         -- Tasks that themselves carry a live or pending execution. Clause 4
         -- uses this (not the family expansion) so a dependent's own run cannot
         -- make a same-family prerequisite look live.
         CREATE VIEW progression_live_execution_tasks AS
         SELECT DISTINCT work_item_id AS work_item_id
         FROM work_executions
         WHERE status IN {LIVE_OR_PENDING};

         -- Every task that belongs to a family with live or pending work.
         CREATE VIEW progression_live_tasks AS
         SELECT k.task_id AS task_id
         FROM progression_family_keys k
         WHERE k.family_key IN (SELECT family_key FROM progression_live_families);

         CREATE VIEW progression_run_state AS
         SELECT t.id AS work_item_id, {run_state}
         FROM tasks t
         WHERE t.deleted_at IS NULL AND t.status NOT IN ('done', 'archived');

         -- Every clause except clause 4, and whether the item would be a candidate
         -- without it. The real-candidate walk uses this as the per-item input;
         -- clause 4 then applies each prerequisite's own clause 4.
         CREATE VIEW progression_candidate_base AS
         SELECT s.work_item_id AS work_item_id,
                s.stuck_kind AS stuck_kind,
                s.stuck_since AS stuck_since,
                (s.grace_stamp IS NOT NULL AND CAST(s.grace_stamp AS INTEGER) >= s.stuck_since) AS grace_waived,
                s.c1_eligible AS c1_eligible,
                s.c2_no_live_work AS c2_no_live_work,
                s.c2_no_inflight_ladder AS c2_no_inflight_ladder,
                s.c3_not_capacity_wait AS c3_not_capacity_wait,
                {c5} AS c5_stuck_long_enough,
                {c6} AS c6_cooldown_and_budget_clear,
                s.progression_live AS progression_live,
                s.in_cooldown AS in_cooldown,
                s.infrastructure_failing AS infrastructure_failing,
                (s.c1_eligible AND s.c2_no_live_work AND s.c2_no_inflight_ladder AND s.c3_not_capacity_wait
                 AND {c5} AND {c6}) AS candidate_ex_dependency
         FROM (
             SELECT t.id AS work_item_id,
                    {stuck_kind} AS stuck_kind,
                    {stuck_since} AS stuck_since,
                    t.progression_grace_waived_at AS grace_stamp,
                    (NOT {parked} AND NOT {human_held}) AS c1_eligible,
                    {no_live_work} AS c2_no_live_work,
                    {no_inflight_ladder} AS c2_no_inflight_ladder,
                    {not_capacity_wait} AS c3_not_capacity_wait,
                    {create_budget_exhausted} AS create_budget_exhausted,
                    {run_state}
             FROM tasks t
             LEFT JOIN progression_family_keys fk ON fk.task_id = t.id
             LEFT JOIN tasks root ON root.id = fk.root_id
             WHERE t.deleted_at IS NULL AND t.status NOT IN ('done', 'archived')
         ) s;

         {real_candidates};

         CREATE VIEW progression_candidate_clauses AS
         SELECT b.work_item_id AS work_item_id,
                b.stuck_kind AS stuck_kind,
                b.stuck_since AS stuck_since,
                b.grace_waived AS grace_waived,
                b.c1_eligible AS c1_eligible,
                b.c2_no_live_work AS c2_no_live_work,
                b.c2_no_inflight_ladder AS c2_no_inflight_ladder,
                b.c3_not_capacity_wait AS c3_not_capacity_wait,
                (t.blocked_reason IS NOT 'dependency' OR NOT x.dependency_held) AS c4_dependency_clear,
                b.c5_stuck_long_enough AS c5_stuck_long_enough,
                b.c6_cooldown_and_budget_clear AS c6_cooldown_and_budget_clear,
                b.progression_live AS progression_live,
                b.in_cooldown AS in_cooldown,
                b.infrastructure_failing AS infrastructure_failing,
                x.dependency_held AS dependency_held
         FROM progression_candidate_base b
         JOIN tasks t ON t.id = b.work_item_id
         JOIN (SELECT t.id AS work_item_id, {dependency_held} AS dependency_held
               FROM tasks t WHERE t.deleted_at IS NULL AND t.status NOT IN ('done', 'archived')) x
              ON x.work_item_id = b.work_item_id;

         CREATE VIEW progression_candidates AS
         SELECT work_item_id, stuck_kind, stuck_since, grace_waived
         FROM progression_candidate_clauses
         WHERE c1_eligible AND c5_stuck_long_enough AND c6_cooldown_and_budget_clear
           AND c3_not_capacity_wait AND c4_dependency_clear
           AND c2_no_live_work AND c2_no_inflight_ladder;"
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
    /// Clause 2, execution half: nothing live or pending on the item, its
    /// revisions or its PR.
    pub no_live_work: bool,
    /// Clause 2, ladder half: no in-flight CI-fix or conflict-resolution
    /// attempt on the family whose revision is missing or still open.
    #[builder(default = true)]
    pub no_inflight_ladder: bool,
    /// Clause 3: not waiting on capacity.
    pub not_capacity_wait: bool,
    /// Clause 4: no prerequisite is live, parked, or a progression candidate.
    pub dependency_clear: bool,
    /// Clause 5: in a stuck state for long enough, or the grace was waived.
    pub stuck_long_enough: bool,
    /// Clause 6: outside the cooldown, under the failure budget, and under the
    /// per-item create budget.
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
                self.no_live_work && self.no_inflight_ladder,
                if self.no_live_work {
                    "a CI-fix or conflict-resolution attempt is in flight on the item, its revisions, or its PR"
                } else {
                    "work is already live or pending on the item, its revisions, or its PR"
                },
            ),
            (
                3,
                self.not_capacity_wait,
                "waiting for a worker slot, the cap, or a pause",
            ),
            (
                4,
                self.dependency_clear,
                "waiting on a prerequisite that is live, parked, or itself a progression candidate",
            ),
            (
                5,
                self.stuck_long_enough,
                "not in a stuck state a progression agent addresses, or not stuck for long enough yet",
            ),
            (
                6,
                self.cooldown_and_budget_clear,
                "inside the cooldown after a progression run, progression failed twice in a row, or the create budget is exhausted",
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
            (
                METADATA_KEY_MAX_CREATES_PER_ITEM_WINDOW,
                i64::from(settings.max_creates_per_item_window),
            ),
            (METADATA_KEY_MAX_CREATES_WINDOW_SECS, settings.max_creates_window_secs),
            // Not read by the candidate views: the stale-worker sweep applies it
            // to a live progression run.
            (
                METADATA_KEY_RUN_TIMEOUT_SECS,
                i64::try_from(settings.run_timeout_secs).unwrap_or(i64::MAX),
            ),
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
    /// insert share one transaction, so an item that other work picked up since
    /// it was listed is not minted a second writer. Consumes the item's
    /// grace-waiver stamp. Returns `None` when the item is no longer a
    /// candidate.
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
    /// held for one evaluation of the predicate rather than one per item. A
    /// freshly minted `ready` run is live work on that family: later candidates
    /// that share its `family_key` are skipped so one pass never gives two
    /// writers to the same PR family. An item whose execution cannot be inserted
    /// (for example its repository cannot be resolved) is skipped and reported;
    /// it does not stop the rest.
    pub fn mint_progression_executions(&self) -> Result<ProgressionMint> {
        let mut conn = self.connect()?;
        let mut tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidates = {
            let mut stmt = tx.prepare(
                "SELECT c.work_item_id, c.stuck_kind, c.stuck_since, c.grace_waived,
                        COALESCE(fk.family_key, c.work_item_id)
                 FROM progression_candidates c
                 LEFT JOIN progression_family_keys fk ON fk.task_id = c.work_item_id
                 ORDER BY c.stuck_since ASC, c.work_item_id ASC",
            )?;
            let rows = stmt.query_map([], |row| Ok((read_candidate(row)?, row.get::<_, String>(4)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut mint = ProgressionMint::default();
        let mut minted_families = HashSet::new();
        for (candidate, family_key) in candidates {
            if minted_families.contains(&family_key) {
                continue;
            }
            let savepoint = tx.savepoint()?;
            match insert_progression_execution(&savepoint, &candidate.work_item_id) {
                Ok(execution) => {
                    savepoint.commit()?;
                    minted_families.insert(family_key);
                    mint.minted.push((candidate, execution));
                }
                Err(error) => {
                    // Dropping the savepoint rolls this item back. The family is
                    // still free, so a later member can be tried.
                    drop(savepoint);
                    mint.failed.push((candidate, format!("{error:#}")));
                }
            }
        }
        tx.commit()?;
        Ok(mint)
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

/// A progression run's footprint on one item; see `progression_run_state`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RunState {
    /// A progression execution of the item is live or pending.
    pub live: bool,
    /// The item is inside the cooldown after a progression run.
    pub in_cooldown: bool,
    /// The item is inside the cooldown after a progression run that completed;
    /// a run that died leaves this false (it only throttles nomination).
    pub in_completed_cooldown: bool,
    /// The last progression runs all ended in infrastructure failure.
    pub infrastructure_failing: bool,
}

/// The run state of every open item that has any progression footprint.
pub(crate) fn run_states(conn: &Connection) -> Result<std::collections::HashMap<String, RunState>> {
    let mut stmt = conn.prepare(
        "SELECT work_item_id, progression_live, in_cooldown, in_completed_cooldown, infrastructure_failing
         FROM progression_run_state
         WHERE progression_live OR in_cooldown OR in_completed_cooldown OR infrastructure_failing",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            RunState {
                live: row.get(1)?,
                in_cooldown: row.get(2)?,
                in_completed_cooldown: row.get(3)?,
                infrastructure_failing: row.get(4)?,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Two clause facts the stuck report shares with the sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SharedClauses {
    /// Clause 1: the item is neither parked nor waiting on a human.
    pub eligible: bool,
    /// An open prerequisite is live, parked, or a progression candidate
    /// (clause 4's raw test, without the item's own `blocked_reason` guard).
    pub dependency_held: bool,
}

/// [`SharedClauses`] for every open item, read in one pass over the clause view.
/// The stuck report consults them per item; reading them once replaces a
/// re-evaluation of the whole candidate graph per item, and means the report and
/// the sweep share one definition of a human hold. An item absent from the map is
/// not an open task.
pub(crate) fn shared_clauses(conn: &Connection) -> Result<std::collections::HashMap<String, SharedClauses>> {
    let mut stmt =
        conn.prepare("SELECT work_item_id, c1_eligible, dependency_held FROM progression_candidate_clauses")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            SharedClauses {
                eligible: row.get(1)?,
                dependency_held: row.get(2)?,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn read_clauses(conn: &Connection, work_item_id: &str) -> Result<Option<ProgressionClauses>> {
    Ok(conn
        .query_row(
            "SELECT work_item_id, stuck_kind, stuck_since, grace_waived, c1_eligible, c2_no_live_work,
                    c2_no_inflight_ladder, c3_not_capacity_wait, c4_dependency_clear, c5_stuck_long_enough,
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
                    .no_inflight_ladder(row.get(6)?)
                    .not_capacity_wait(row.get(7)?)
                    .dependency_clear(row.get(8)?)
                    .stuck_long_enough(row.get(9)?)
                    .cooldown_and_budget_clear(row.get(10)?)
                    .progression_live(row.get(11)?)
                    .in_cooldown(row.get(12)?)
                    .infrastructure_failing(row.get(13)?)
                    .build())
            },
        )
        .optional()?)
}
