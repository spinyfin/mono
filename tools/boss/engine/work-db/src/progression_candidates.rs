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
//! - `progression_candidate_base` evaluates every clause except dependency
//!   health. `progression_prereq_task_edges` expands open task and project
//!   prerequisites; `progression_dependency_waiting` walks those edges
//!   transitively to detect cycles and identifies ordinary unfinished waits.
//!   Neither depends on a prerequisite's progression candidacy or age.
//! - `progression_chain_roots`, `progression_family_keys`,
//!   `progression_live_families` and `progression_ladder_families` give
//!   clause 2 its "item, its revisions and its PR chain" reach. They are
//!   non-correlated sets tested with `IN`, so the recursive walks run once
//!   per statement, not once per candidate row under the sweep's writer lock.
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
//! 2. **No progressing work** (`queued | ready | claimed | running |
//!    waiting_human`) on the item, its revisions, or any item sharing its PR.
//!    Dependency-waiting executions hold a family only while a prerequisite
//!    family is progressing, transitively. An idle gate cannot hide a failed
//!    revision forever. CI/conflict attempts with a revision hold only while
//!    that revision's family is progressing, and never hold their own revision
//!    as a ladder candidate. Attempts without a revision still protect the
//!    engine's mechanical work and revision-creation window. An in-flight
//!    ladder attempt is named separately in
//!    [`ProgressionClauses::no_inflight_ladder`] so diagnose can say which half
//!    of the clause failed.
//! 3. **Not waiting on capacity.** No queued execution of the item carries a
//!    slot, cap, workspace or pause wait reason. Waiting for bandwidth is not
//!    stuck.
//! 4. **Dependency clear.** An item waiting on unfinished prerequisites is
//!    progressed through those prerequisites, however long the chain or wait.
//!    A stuck root is considered through its own stuck state, even when its
//!    grace, cooldown, budget or a human hold currently prevents nomination.
//!    Dependency progression repairs missing, deleted or archived prerequisites,
//!    cycles (including a dependency on the item's own project), and stale
//!    blocks after all prerequisites finish or disappear. These are diagnosed
//!    at the broken edge's dependent or the cycle's members; downstream items
//!    wait for that repair. An open project holds dependents while parked or
//!    while it has open members. The stuck report reads the same value as
//!    `dependency_held`.
//! 5. **Stuck long enough.** In one of the stuck states the enumeration marks as
//!    progression-eligible ([`stuck_kind_sql`]) continuously for at least
//!    `progression.stuck_after`, measured from the task status ledger exactly as
//!    the stuck report ages it; or the grace was waived since the item entered
//!    that state (`tasks.progression_grace_waived_at`).
//! 6. **Cooldown and budget.** No progression run within `progression.cooldown`
//!    (four times that after a `no_changes_needed` run), the last
//!    [`INFRASTRUCTURE_FAILURE_CAP`] runs since the owner's last "keep trying"
//!    were not all infrastructure failures (the engine asks the owner about such
//!    an item: `progression_ladder`),
//!    and the item's create budget (`progression.max_creates_per_item_window`
//!    revisions-plus-precursors in the rolling window) is not exhausted. A run
//!    that is already in flight still sees the same cap at the verbs (exhaustion
//!    there is a refusal so the agent can ask); an item that arrives at the
//!    sweep already over budget is not nominated again.
//!
//!    A run whose agent never started ([`launch_failed_sql`]) starts the
//!    separate launch backoff: 2 minutes, doubling per consecutive failure up
//!    to 30 minutes, reset when an agent starts. It does not start the calibrated
//!    cooldown or count toward the infrastructure-failure cap. The
//!    cap is the engine admitting it cannot unstick *this item*; a failed
//!    launch says nothing about the item, and every item given the same
//!    driver fails the same way while that driver's launch is broken. Such an
//!    item is retried each time its launch backoff ends, and
//!    [`WorkDb::progression_launch_backoff`] reports the wait so it is never
//!    silent.
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
//! [`WorkDb::configure_progression_calibration`]); an absent key falls back to the
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
    METADATA_KEY_MAX_CREATES_PER_ITEM_WINDOW, METADATA_KEY_MAX_CREATES_WINDOW_SECS, METADATA_KEY_STUCK_AFTER_SECS,
    NO_ACTION_COOLDOWN_MULTIPLIER,
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
         DROP VIEW IF EXISTS progression_dependency_waiting;
         DROP VIEW IF EXISTS progression_dependency_cycles;
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

/// Whether progression execution `x` failed before its driver started. The
/// durable marker includes slot collisions without counting them in the
/// driver's pre-start streak. Session identities are transient teardown state.
/// Keep the start-time and pre-start-count fallbacks for older executions.
pub(crate) fn launch_failed_sql(x: &str) -> String {
    format!(
        "({x}.status = 'failed' AND ({x}.launch_failed = 1
          OR {x}.pre_start_failure_count > 0 OR {x}.started_at IS NULL))"
    )
}

/// The progression runs of item `a` that count as attempts, as a `WHERE`
/// fragment over execution alias `x`: not cancelled, and not withdrawn by the
/// engine before it ever started.
fn attempt_sql(x: &str, a: &str) -> String {
    format!(
        "{x}.work_item_id = {a}.id AND {x}.kind = 'progression' AND {x}.status <> 'cancelled'
         AND NOT ({x}.status = 'abandoned' AND {x}.started_at IS NULL)"
    )
}

pub(crate) fn limit_sql(key: &str, default: i64) -> String {
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
    let not_positioning_held = crate::dispatch_helpers::not_positioning_held_sql(a);
    // COALESCE: a NULL `blocked_reason` makes several terms NULL, and a NULL hold
    // must read as "not held", not poison the clause that negates it.
    // Positioning refusals use the shared predicate so an open attention item
    // holds a review-phase row the same way `blocked_reason` holds a rewritten
    // todo/active row.
    format!(
        "COALESCE({a}.deferred = 1 OR {a}.human_driven = 1
          OR {a}.blocked_reason IN ('awaiting_operator_answer', 'question_dismissed', 'deletion_signoff')
          OR NOT {not_positioning_held}
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
    let not_positioning_held = crate::dispatch_helpers::not_positioning_held_sql(a);
    let trunk_queue = open_attention_sql(a, "wa.kind LIKE 'trunk_queue_%'");
    let abandoned = open_attention_sql(a, "wa.kind = 'abandoned_branch_no_pr'");
    let recovery = open_attention_sql(
        a,
        "wa.kind IN ('worker_recovery_permanent_error', 'worker_recovery_exhausted')",
    );
    let review_churn = open_attention_sql(a, "wa.kind = 'churn_guard_parked'");
    format!(
        "CASE
           WHEN {a}.id IN (SELECT work_item_id FROM progression_dependency_cycles) THEN 'S4_dependency_cycle'
           WHEN {a}.dispatch_failed_reason IS NOT NULL THEN
               CASE {a}.dispatch_failed_reason
                   WHEN 'churn_guard' THEN 'S2_churn_guard'
                   WHEN 'deliberate_park' THEN 'S3_deliberate_park'
                   ELSE 'S1_dispatch_failure' END
           WHEN NOT {not_positioning_held} THEN NULL
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
            WHERE la.work_item_id = {a}.id
              AND (la.property = ({age_property}) OR (({age_property}) IS NULL AND la.property <> 'deferred'))
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
/// family, including dependency waits backed by progressing work. `family` is the alias of
/// the item's `progression_family_keys` row, joined by the caller. The family
/// test is a one-time `IN` set, not a correlated lookup, because the chain walk
/// behind it is a recursive query that must be evaluated once per statement
/// rather than once per candidate row.
fn no_live_work_sql(family: &str) -> String {
    format!("NOT COALESCE({family}.family_key IN (SELECT family_key FROM progression_live_families), 0)")
}

/// Clause 2, ladder half: no in-flight `ci_remediations` or
/// `conflict_resolutions` attempt doing work. Never let an attempt's own
/// revision exclude itself; the execution half still protects live workers.
fn no_inflight_ladder_sql(item: &str) -> String {
    format!("{item}.id NOT IN (SELECT task_id FROM progression_ladder_families)")
}

/// The view behind [`no_inflight_ladder_sql`]: families with an attempt in
/// either ladder table that is `pending` or `running`. An open revision alone
/// is not progress: it must have progressing work in its family. A NULL
/// revision still protects mechanical attempts and the revision-creation window.
/// Expand to affected family members here, excluding the attempt's own revision,
/// so the recursive family/liveness sets stay uncorrelated with candidate rows.
fn ladder_families_select() -> String {
    let attempt = |table: &str| {
        format!(
            "SELECT rk.family_key AS family_key, candidate.task_id AS task_id
             FROM {table} r
             JOIN progression_family_keys rk ON rk.task_id = r.work_item_id
             JOIN progression_family_keys candidate ON candidate.family_key = rk.family_key
             LEFT JOIN tasks rt ON rt.id = r.revision_task_id AND rt.deleted_at IS NULL
             WHERE r.status IN ('pending', 'running')
               AND r.revision_task_id IS NOT candidate.task_id
               AND (r.revision_task_id IS NULL
                    OR (rt.id IS NOT NULL AND rt.status NOT IN ('done', 'archived')
                        AND rt.id IN (SELECT k.task_id FROM progression_family_keys k
                                      WHERE k.family_key IN (SELECT family_key FROM progression_live_families))))"
        )
    };
    format!(
        "{} UNION {}",
        attempt("ci_remediations"),
        attempt("conflict_resolutions")
    )
}

/// Clause 4 uses graph health rather than progression candidacy. A failed or
/// throttled prerequisite still owns its recovery; its dependents keep waiting.
fn dependency_held_sql(a: &str) -> String {
    format!("{a}.id IN (SELECT work_item_id FROM progression_dependency_waiting)")
}

/// Open `blocks` edges expanded to task prerequisites. Include the dependent
/// among project members: depending on one's own project is a cycle. Parked
/// projects hold dependents directly, without walking their members.
fn prereq_task_edges_sql() -> String {
    let live = crate::live_chain_writer_sql("p");
    let writer = crate::revision_sequencing::has_writer_sql("p.id");
    let conflict_in_flight = crate::revision_sequencing::conflict_fix_in_flight_sql("p.id");
    let conflict_prefix = boss_protocol::CREATED_VIA_MERGE_CONFLICT_PREFIX;
    let recovery_prefix = boss_protocol::CREATED_VIA_CONFLICT_RECOVERY_PREFIX;
    let recovery_start = recovery_prefix.len() + 1;
    format!(
        "SELECT d.dependent_id AS dependent_id, d.prerequisite_id AS prerequisite_task_id
             FROM work_item_dependencies d
             JOIN tasks p ON p.id = d.prerequisite_id
             LEFT JOIN tasks t ON t.id = d.dependent_id
             LEFT JOIN progression_chain_roots tc ON tc.task_id = t.id
             LEFT JOIN progression_chain_roots pc ON pc.task_id = p.id
             -- A recovery inherits the conflict revision's ordering identity.
             LEFT JOIN tasks source ON source.id = CASE
                 WHEN t.created_via LIKE '{recovery_prefix}%'
                 THEN substr(t.created_via, {recovery_start}) ELSE t.id END
             LEFT JOIN progression_chain_roots sc ON sc.task_id = source.id
             WHERE d.relation = 'blocks' AND p.deleted_at IS NULL
               AND CASE
                 -- Conflict writer exclusion precedes status satisfaction,
                 -- matching gating_prereqs_for.
                 WHEN t.kind = 'revision' AND source.kind = 'revision'
                      AND source.created_via LIKE '{conflict_prefix}%'
                      AND sc.root_id = pc.root_id
                 THEN CASE WHEN p.id > source.id THEN {conflict_in_flight} ELSE {writer} END
                 WHEN p.status IN ('done', 'archived') THEN 0
                 WHEN t.kind = 'revision' AND tc.root_id = pc.root_id
                 THEN NOT (p.status = 'in_review' OR (p.kind = 'revision' AND NOT {live}))
                 ELSE 1
               END
             UNION
             SELECT d.dependent_id, m.id
             FROM work_item_dependencies d
             JOIN projects pj ON pj.id = d.prerequisite_id
               AND pj.status NOT IN ('done', 'archived') AND pj.parked_at IS NULL
             JOIN tasks m ON m.project_id = pj.id AND m.deleted_at IS NULL
               AND m.status NOT IN ('done', 'archived')
             WHERE d.relation = 'blocks'"
    )
}

/// Tasks waiting on unfinished work, independent of how that work is dispatched.
/// The transitive closure uses UNION (not UNION ALL), so even corrupt cycles
/// terminate. Only cycle members are nominated; other dependents wait for the
/// repair upstream. Missing/archived/deleted edges belong to their direct
/// dependent. A done edge is satisfied, so it is only a stale block once no
/// unfinished prerequisite remains.
fn dependency_waiting_view_sql() -> &'static str {
    "CREATE VIEW progression_dependency_waiting AS
     WITH RECURSIVE reach(dependent_id, prerequisite_id) AS (
         SELECT dependent_id, prerequisite_task_id FROM progression_prereq_task_edges
         UNION
         SELECT r.dependent_id, e.prerequisite_task_id
         FROM reach r
         JOIN progression_prereq_task_edges e ON e.dependent_id = r.prerequisite_id
     ),
     waiting(work_item_id) AS (
         SELECT dependent_id FROM progression_prereq_task_edges
         UNION
         SELECT d.dependent_id FROM work_item_dependencies d
         JOIN projects p ON p.id = d.prerequisite_id
         WHERE d.relation = 'blocks' AND p.parked_at IS NOT NULL
           AND p.status NOT IN ('done', 'archived')
     ),
     broken(work_item_id) AS (
         SELECT d.dependent_id FROM work_item_dependencies d
         LEFT JOIN tasks t ON t.id = d.prerequisite_id
         LEFT JOIN projects p ON p.id = d.prerequisite_id
         WHERE d.relation = 'blocks'
           AND ((t.id IS NULL AND p.id IS NULL)
                OR t.deleted_at IS NOT NULL OR t.status = 'archived' OR p.status = 'archived')
         UNION
         SELECT dependent_id FROM reach WHERE dependent_id = prerequisite_id
         UNION
         SELECT work_item_id FROM progression_dependency_cycles
     )
     SELECT work_item_id FROM waiting
     EXCEPT
     SELECT work_item_id FROM broken"
}

/// Clause 6, create-budget half: revisions and precursors a progression run
/// minted for this item inside the rolling window, identified by
/// `created_via LIKE 'progression:%'` (design 2.4).
fn create_budget_exhausted_sql(a: &str, max_creates: &str, window: &str) -> String {
    format!("({}) >= ({max_creates})", create_budget_used_sql(a, window))
}

/// SQL expressions for the per-item create cap and its rolling window, read from
/// metadata with the published defaults.
pub(crate) fn create_budget_limit_sqls() -> (String, String) {
    (
        limit_sql(
            METADATA_KEY_MAX_CREATES_PER_ITEM_WINDOW,
            i64::from(DEFAULT_MAX_CREATES_PER_ITEM_WINDOW),
        ),
        limit_sql(METADATA_KEY_MAX_CREATES_WINDOW_SECS, DEFAULT_MAX_CREATES_WINDOW_SECS),
    )
}

/// Count of revisions and precursors a progression run minted for `{a}` inside
/// the `window` (seconds) SQL expression. Shared by the candidate clause and the
/// diagnosis packet so the budget the agent reads is the one the sweep enforces.
pub(crate) fn create_budget_used_sql(a: &str, window: &str) -> String {
    // Two indexed arms (children by parent, precursors by dependent) rather than
    // one scan of every task with an OR across both relations.
    format!(
        "SELECT COUNT(*) FROM (
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
                AND CAST(created.created_at AS INTEGER) > {NOW} - ({window}))"
    )
}

/// Whether the item is inside the cooldown after a progression run. The cooldown
/// is measured from the end of the run (its creation while it has not ended), a
/// run that was cancelled before it started does not start one, a run the engine
/// withdrew before it ever started (`abandoned` with no `started_at`) is not an
/// attempt at all. Failed launches use their own bounded backoff instead. A
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
    let launch_failed = launch_failed_sql("px");
    format!(
        "EXISTS (SELECT 1 FROM work_executions px
                 WHERE px.work_item_id = {a}.id AND px.kind = 'progression' AND {status_filter}
                   AND NOT {launch_failed}
                   AND COALESCE(CAST(px.finished_at AS INTEGER), CAST(px.created_at AS INTEGER))
                       + ({cooldown}) * (CASE WHEN px.run_done_outcome = 'no_changes_needed'
                                              THEN {NO_ACTION_COOLDOWN_MULTIPLIER} ELSE 1 END) > {NOW})"
    )
}

/// Whether the item's last [`INFRASTRUCTURE_FAILURE_CAP`] progression runs all
/// ended in infrastructure failure, so the sweep stops nominating it (and the
/// engine asks the owner: `progression_ladder`). Runs from before the owner's
/// last "keep trying" answer (`progression_ladder.reset_at`) do not count, and
/// neither does a run that failed to launch ([`launch_failed_sql`]): its agent
/// never looked at the item, so it is no evidence about the item.
fn infrastructure_failing_sql(a: &str) -> String {
    let attempt = attempt_sql("fx", a);
    let launch_failed = launch_failed_sql("fx");
    (0..INFRASTRUCTURE_FAILURE_CAP)
        .map(|offset| {
            format!(
                "COALESCE((SELECT fx.status FROM work_executions fx
                           WHERE {attempt} AND NOT {launch_failed}
                             AND (CAST(fx.created_at AS INTEGER) >
                                  COALESCE((SELECT l.reset_at FROM progression_ladder l
                                            WHERE l.work_item_id = {a}.id), 0)
                                  OR (CAST(fx.created_at AS INTEGER) =
                                      COALESCE((SELECT l.reset_at FROM progression_ladder l
                                                WHERE l.work_item_id = {a}.id), 0)
                                      AND fx.rowid >
                                      COALESCE((SELECT l.reset_rowid FROM progression_ladder l
                                                WHERE l.work_item_id = {a}.id), 0)))
                           ORDER BY fx.created_at DESC, fx.id DESC LIMIT 1 OFFSET {offset})
                          IN {INFRASTRUCTURE_FAILED}, 0)"
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// When the latest failed launch's retry backoff ends; NULL outside backoff.
/// Only the five most recent attempts are needed to reach the 30-minute cap.
/// Started cancellations reset the streak too; withdrawn unstarted attempts
/// neither reset nor increase it. Execution history makes this survive restarts.
fn launch_backoff_until_sql(a: &str) -> String {
    let attempt = format!(
        "bx.work_item_id = {a}.id AND bx.kind = 'progression'
         AND (({}) OR bx.started_at IS NOT NULL)",
        attempt_sql("bx", a)
    );
    let launch_failed = launch_failed_sql("bx");
    let history_attempt = attempt_sql("hx", a);
    let history_failed = launch_failed_sql("hx");
    let streak = format!(
        "(SELECT COALESCE(MIN(CASE WHEN NOT failed THEN position - 1 END), COUNT(*))
          FROM (SELECT failed, ROW_NUMBER() OVER (ORDER BY created_at DESC, id DESC) AS position
                FROM (SELECT {history_failed} AS failed, hx.created_at, hx.id
                      FROM work_executions hx
                      WHERE hx.work_item_id = {a}.id AND hx.kind = 'progression'
                        AND (({history_attempt}) OR hx.started_at IS NOT NULL)
                      ORDER BY hx.created_at DESC, hx.id DESC LIMIT 5)))"
    );
    let until = format!(
        "COALESCE(CAST(bx.finished_at AS INTEGER), CAST(bx.created_at AS INTEGER))
         + MIN(120 << (({streak}) - 1), 1800)"
    );
    format!(
        "(SELECT CASE WHEN {launch_failed} AND {until} > {NOW} THEN {until} END
          FROM work_executions bx WHERE {attempt}
          ORDER BY bx.created_at DESC, bx.id DESC LIMIT 1)"
    )
}

/// The progression-run facts about one item: whether a run is live or pending,
/// whether the item is inside the cooldown after a run, whether the last
/// runs all died, and until when progression is backing off after a failed
/// launch. They depend only on the item's own progression executions, so
/// the stuck report reads them from their own cheap view.
fn run_state_select(cooldown: &str) -> String {
    let in_cooldown = in_cooldown_sql("t", cooldown, false);
    let in_completed_cooldown = in_cooldown_sql("t", cooldown, true);
    let infrastructure_failing = infrastructure_failing_sql("t");
    let launch_backoff_until = launch_backoff_until_sql("t");
    format!(
        "EXISTS (SELECT 1 FROM work_executions lx
                 WHERE lx.work_item_id = t.id AND lx.kind = 'progression'
                   AND lx.status IN {LIVE_OR_PENDING}) AS progression_live,
         {in_cooldown} AS in_cooldown,
         {in_completed_cooldown} AS in_completed_cooldown,
         ({infrastructure_failing}) AS infrastructure_failing,
         {launch_backoff_until} AS launch_backoff_until"
    )
}

fn views_sql() -> String {
    let stuck_after = limit_sql(METADATA_KEY_STUCK_AFTER_SECS, DEFAULT_STUCK_AFTER_SECS);
    let cooldown = limit_sql(METADATA_KEY_COOLDOWN_SECS, DEFAULT_COOLDOWN_SECS);
    let (max_creates, create_window) = create_budget_limit_sqls();
    let parked = parked_task_sql("t");
    let human_held = human_held_sql("t");
    let stuck_kind = stuck_kind_sql("t", "root");
    let stuck_since = stuck_since_sql("t");
    let no_live_work = no_live_work_sql("fk");
    let no_inflight_ladder = no_inflight_ladder_sql("t");
    let not_capacity_wait = not_capacity_wait_sql("t");
    let dependency_held = dependency_held_sql("t");
    let ladder_families = ladder_families_select();
    let c5 = format!(
        "(s.stuck_kind IS NOT NULL
                 AND ((s.grace_stamp IS NOT NULL AND CAST(s.grace_stamp AS INTEGER) >= s.stuck_since)
                      OR {NOW} - s.stuck_since >= {stuck_after}))"
    );
    let c6 = "(NOT s.in_cooldown AND s.launch_backoff_until IS NULL AND NOT s.infrastructure_failing AND NOT s.create_budget_exhausted)";
    let create_budget_exhausted = create_budget_exhausted_sql("t", &max_creates, &create_window);
    let run_state = run_state_select(&cooldown);
    let prereq_edges = prereq_task_edges_sql();
    let dependency_waiting = dependency_waiting_view_sql();
    let dependency_cycles = crate::work_dependencies::cycle::members_sql();
    format!(
        "{DROP_VIEWS}

         CREATE VIEW progression_dependency_cycles AS {dependency_cycles};

         -- Open `blocks` edges, including project members for cycle detection.
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

         -- Follow waiting gates only back to actual progressing work. UNION
         -- terminates cycles; a cycle of idle waits cannot keep itself live.
         CREATE VIEW progression_live_families AS
         WITH RECURSIVE live(family_key) AS (
             SELECT k.family_key FROM progression_family_keys k
             JOIN work_executions e ON e.work_item_id = k.task_id
             WHERE e.status IN ('queued','ready','claimed','running','waiting_human')
                OR (e.kind = 'progression' AND e.status = 'waiting_dependency')
             UNION
             SELECT dk.family_key FROM live l
             JOIN progression_family_keys pk ON pk.family_key = l.family_key
             JOIN progression_prereq_task_edges p ON p.prerequisite_task_id = pk.task_id
             JOIN work_executions e ON e.work_item_id = p.dependent_id
               AND e.status = 'waiting_dependency'
             JOIN progression_family_keys dk ON dk.task_id = p.dependent_id
             WHERE dk.family_key NOT IN (
                 SELECT ck.family_key FROM progression_family_keys ck
                 JOIN progression_dependency_cycles c ON c.work_item_id = ck.task_id)
         )
         SELECT family_key FROM live;

         -- The families with an in-flight CI-fix or conflict-resolution attempt.
         CREATE VIEW progression_ladder_families AS
         {ladder_families};

         CREATE VIEW progression_run_state AS
         SELECT t.id AS work_item_id, {run_state}
         FROM tasks t
         WHERE t.deleted_at IS NULL AND t.status NOT IN ('done', 'archived');

         -- Every clause except clause 4. Dependency health is independent of
         -- the prerequisite's eligibility, grace, cooldown and budgets.
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
                s.launch_backoff_until AS launch_backoff_until
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

         {dependency_waiting};

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
                b.launch_backoff_until AS launch_backoff_until,
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
    /// attempt doing work on the family, excluding the candidate's own attempt.
    #[builder(default = true)]
    pub no_inflight_ladder: bool,
    /// Clause 3: not waiting on capacity.
    pub not_capacity_wait: bool,
    /// Clause 4: not an ordinary wait on unfinished prerequisite work.
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
                "waiting on unfinished prerequisite work; progress or repair the prerequisite first",
            ),
            (
                5,
                self.stuck_long_enough,
                "not in a stuck state a progression agent addresses, or not stuck for long enough yet",
            ),
            (
                6,
                self.cooldown_and_budget_clear,
                "inside the cooldown or launch backoff after a progression run, progression failed twice in a row, or the create budget is exhausted",
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
    /// Whether progression is backing off after failed launches, and until
    /// when. `None` when no item is waiting on one.
    ///
    /// An item counts when its latest progression run failed to launch
    /// ([`launch_failed_sql`]), the launch backoff from that run is still running,
    /// and every other clause that does not depend on time or on the run
    /// history holds, so the backoff is the one thing between the item and
    /// its next run. An item that stopped being stuck, was parked, or picked
    /// up live work in the meantime is not waiting on progression at all.
    pub fn progression_launch_backoff(&self) -> Result<Option<ProgressionLaunchBackoff>> {
        let conn = self.connect()?;
        // The run-state view is cheap; the clause view is the expensive one,
        // so it is only read when some item is inside such a backoff.
        let any: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM progression_run_state WHERE launch_backoff_until IS NOT NULL)",
            [],
            |row| row.get(0),
        )?;
        if !any {
            return Ok(None);
        }
        let attempt = attempt_sql("x", "c");
        let mut stmt = conn.prepare(&format!(
            "SELECT c.launch_backoff_until, x.id,
                    COALESCE(CAST(x.finished_at AS INTEGER), CAST(x.created_at AS INTEGER)),
                    COALESCE(x.last_error, '')
             FROM (SELECT work_item_id AS id, launch_backoff_until FROM progression_candidate_clauses
                   WHERE launch_backoff_until IS NOT NULL AND NOT progression_live AND stuck_kind IS NOT NULL
                     AND c1_eligible AND c2_no_live_work AND c2_no_inflight_ladder
                     AND c3_not_capacity_wait AND c4_dependency_clear) c
             JOIN work_executions x ON x.id = (SELECT x.id FROM work_executions x WHERE {attempt}
                                               ORDER BY x.created_at DESC, x.id DESC LIMIT 1)
             ORDER BY 3 DESC, x.id DESC"
        ))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let Some((_, latest_execution_id, latest_failure_epoch_s, latest_error)) = rows.first().cloned() else {
            return Ok(None);
        };
        Ok(Some(ProgressionLaunchBackoff {
            waiting_items: rows.len(),
            next_attempt_epoch_s: rows.iter().map(|row| row.0).min().unwrap_or_default(),
            latest_execution_id,
            latest_failure_epoch_s,
            latest_error,
        }))
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
    /// writers to the same PR family. Before any run is minted the escalation
    /// ladder (`progression_ladder`) is asked about every candidate: one whose
    /// attempts have run out is asked about, not given another run, and is
    /// reported in [`ProgressionMint::asked`]. An item whose execution cannot be inserted
    /// (for example its repository cannot be resolved) is skipped and reported;
    /// it does not stop the rest.
    pub fn mint_progression_executions(&self) -> Result<ProgressionMint> {
        let mut conn = self.connect()?;
        let mut tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut pending = PendingEvents::new();
        // Before the candidates are read: an item whose question is withdrawn
        // here is a candidate again in this same pass.
        let retraction = crate::progression_ladder::retract_unwarranted_questions(&mut tx, &mut pending)?;
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
        let mut mint = ProgressionMint {
            retracted: retraction.retracted,
            ladder_failed: retraction.failed,
            ..Default::default()
        };
        // The ladder first: an item whose attempts have run out is asked about
        // (and parked on the question) instead of being given another run.
        let pass = crate::progression_ladder::run_pass(&mut tx, &mut pending, candidates)?;
        mint.asked = pass.asked;
        mint.ladder_failed.extend(pass.failed);
        let mut minted_families = HashSet::new();
        for (candidate, family_key) in pass.mintable {
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
        commit_and_publish(tx, pending, self.event_bus())?;
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
    /// Items the escalation ladder asked the owner about instead of minting a
    /// run for (`progression_ladder`).
    pub asked: Vec<crate::progression_ladder::LadderQuestion>,
    /// Items the ladder meant to ask about and could not (the question could
    /// not be filed), or whose unwarranted question it could not withdraw,
    /// with the reason. They are not minted a run either: that
    /// would be the attempt the ladder ruled out. The next pass tries again.
    pub ladder_failed: Vec<(String, String)>,
    /// Items whose infrastructure-failure question the engine withdrew because
    /// the runs it counted never launched (`progression_ladder`). They are
    /// candidates again, in this pass when their cooldown allows.
    pub retracted: Vec<String>,
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

/// Progression backing off after its agents failed to launch: the stuck items
/// that would be given a run now but for the backoff a failed launch started.
/// Nothing is asked of the owner and nothing is given up: each item is retried
/// when its launch backoff ends. See [`WorkDb::progression_launch_backoff`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressionLaunchBackoff {
    /// Items waiting out that launch backoff.
    pub waiting_items: usize,
    /// Epoch seconds at which the first of those launch backoffs ends, which is when
    /// the next launch is attempted.
    pub next_attempt_epoch_s: i64,
    /// The most recent failed launch among those items.
    pub latest_execution_id: String,
    /// Epoch seconds at which it failed.
    pub latest_failure_epoch_s: i64,
    /// Why it failed, as recorded on the execution.
    pub latest_error: String,
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

/// Clause facts the stuck report shares with the sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SharedClauses {
    /// Clause 1: the item is neither parked nor waiting on a human.
    pub eligible: bool,
    /// An open prerequisite is live, parked, or a progression candidate
    /// (clause 4's raw test, without the item's own `blocked_reason` guard).
    pub dependency_held: bool,
    pub dependency_cycle: bool,
}

/// [`SharedClauses`] for every open item, read in one pass over the clause view.
/// The stuck report consults them per item; reading them once replaces a
/// re-evaluation of the whole candidate graph per item, and means the report and
/// the sweep share one definition of a human hold. An item absent from the map is
/// not an open task.
pub(crate) fn shared_clauses(conn: &Connection) -> Result<std::collections::HashMap<String, SharedClauses>> {
    let mut stmt =
        conn.prepare("SELECT work_item_id, c1_eligible, dependency_held, COALESCE(stuck_kind = 'S4_dependency_cycle', 0) FROM progression_candidate_clauses")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            SharedClauses {
                eligible: row.get(1)?,
                dependency_held: row.get(2)?,
                dependency_cycle: row.get(3)?,
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

impl WorkDb {
    /// Record a `progression` run, its host and its tmux hosting, **without
    /// touching the work item it was sent to diagnose**.
    ///
    /// [`WorkDb::start_execution_run_on_host_with_tmux_hosting`] also advances
    /// the item to `active`, consumes its `autostart` and clears its
    /// `dispatch_failed_*` stamp in the same transaction. For a progression run
    /// that would erase the very evidence of the stuck state the agent is
    /// started to inspect, so this variant writes only the execution and run
    /// rows. The run holds no cube lease: the lease, repo and workspace columns
    /// stay unset and `workspace_path` records the engine-owned scratch
    /// directory it runs in.
    pub fn start_progression_run_on_local_host(
        &self,
        execution_id: &str,
        agent_id: &str,
        scratch_path: &str,
        tmux_hosted: bool,
    ) -> Result<(WorkExecution, WorkRun)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if execution.kind.holds_cube_lease() {
            bail!(
                "execution {execution_id} is a `{}` run; only a progression run may start without a lease",
                execution.kind
            );
        }
        if !execution.status.can_begin_run() {
            bail!(
                "execution {execution_id} is not ready and cannot start a run from status `{}`",
                execution.status
            );
        }

        let now = now_string();
        let updated = tx.execute(
            "UPDATE work_executions
             SET status = 'running',
                 workspace_path = ?2,
                 host_id = 'local',
                 started_at = COALESCE(started_at, ?3),
                 finished_at = NULL,
                 last_error = NULL
             WHERE id = ?1
               AND status IN ('ready', 'claimed')",
            params![execution_id, scratch_path, now],
        )?;
        if updated == 0 {
            let current = query_execution(&tx, execution_id).require("execution", execution_id)?;
            bail!(
                "execution {execution_id} is not ready and cannot start a run from status `{}`",
                current.status
            );
        }

        let run_id = next_id("run");
        tx.execute(
            "INSERT INTO work_runs (
                id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                artifacts_path, created_at, started_at, finished_at, host_id, tmux_hosted
             ) VALUES (?1, ?2, ?3, 'active', NULL, NULL, NULL, NULL, ?4, ?4, NULL, 'local', ?5)",
            params![run_id, execution_id, agent_id, now, tmux_hosted],
        )?;

        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        let overflow = personas::allocate(&tx, &run_id)?;
        let run = query_run(&tx, &run_id)?.with_context(|| format!("missing run after insert: {run_id}"))?;
        tx.commit()?;
        self.record_persona_overflow(u64::from(overflow));
        Ok((execution, run))
    }

    /// Drive a `progression` execution to the `failed` terminal status and
    /// record `result_summary` as the failure detail on its most recent run,
    /// even when that run was already closed as `completed` by the pane spawn
    /// (the normal case for a pane-hosted worker). The ordinary pane-parked
    /// finalizer always reports `completed`, which would hide a driver error
    /// from the candidate view's infrastructure-failure cap. Never touches the
    /// work item. Idempotent: returns `Ok(None)` if the execution is already
    /// terminal.
    pub fn fail_progression_execution(
        &self,
        execution_id: &str,
        result_summary: &str,
    ) -> Result<Option<WorkExecution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if execution.status.is_terminal() || execution.kind.holds_cube_lease() {
            return Ok(None);
        }
        let now = now_string();
        let summary = normalize_optional_text(Some(result_summary.to_owned()));
        tx.execute(
            "UPDATE work_runs
             SET status = 'failed',
                 error_text = COALESCE(?2, error_text),
                 result_summary = COALESCE(?2, result_summary),
                 finished_at = COALESCE(finished_at, ?3)
             WHERE id = (SELECT id FROM work_runs WHERE execution_id = ?1
                         ORDER BY created_at DESC, id DESC LIMIT 1)",
            params![execution_id, summary, now],
        )?;
        tx.execute(
            "UPDATE work_executions SET status = 'failed', finished_at = ?2 WHERE id = ?1",
            params![execution_id, now],
        )?;
        let updated = query_execution(&tx, execution_id).require("execution", execution_id)?;
        let mut pending = PendingEvents::new();
        stage_execution_terminal(&mut pending, &tx, execution_id, &updated.work_item_id)?;
        commit_and_publish(tx, pending, &self.event_bus)?;
        Ok(Some(updated))
    }
}
