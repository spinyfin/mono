use super::*;

use std::sync::OnceLock;
use std::time::Instant;

use crate::startup_timing::SLOW_STEP_THRESHOLD;

// Why no `migrate_*` step (and neither init path) probes the local host's
// capabilities any more ("the capability-probe note"):
//
// Until 2026-09 the chain called `refresh_local_host_auto_capabilities`
// between `ensure_local_host` and the revision-task migrations. That probe
// spawns three interactive login shells (one `command -v` per registered
// driver) plus a live `gh auth status`, and it ran on every engine boot,
// inside `WorkDb::open`, before the frontend socket existed — the largest
// single attributable contributor to the app showing no data after an
// update. Schema migration must not perform host I/O, network calls, or
// subprocess execution. Discovery now runs from engine startup
// (`app::server::run_server`) after the database is open, with dispatch held
// until it completes — see `WorkDb::refresh_local_host_auto_capabilities`.

/// Per-step wall-clock ledger for one [`WorkDb::run_full_migration_chain`]
/// run. The chain replays ~150 idempotent steps on every boot of an
/// existing database, so a per-step line is logged only above
/// [`SLOW_STEP_THRESHOLD`]; the total is always logged.
struct MigrationChainTimer {
    started: Instant,
    steps: u32,
    slow_steps: u32,
}

impl MigrationChainTimer {
    fn start() -> Self {
        Self {
            started: Instant::now(),
            steps: 0,
            slow_steps: 0,
        }
    }

    /// Run one chain step under the clock. `name` is the step function's
    /// path as written at the call site; only its last segment is logged.
    fn step(&mut self, conn: &Connection, name: &str, step: fn(&Connection) -> Result<()>) -> Result<()> {
        let started = Instant::now();
        let result = step(conn);
        let elapsed = started.elapsed();
        self.steps += 1;
        if elapsed >= SLOW_STEP_THRESHOLD {
            self.slow_steps += 1;
            tracing::info!(
                step = name.rsplit("::").next().unwrap_or(name),
                elapsed_ms = elapsed.as_millis() as u64,
                ok = result.is_ok(),
                "work db: slow migration step",
            );
        }
        result
    }

    fn finish(&self) {
        tracing::info!(
            steps = self.steps,
            slow_steps = self.slow_steps,
            slow_threshold_ms = SLOW_STEP_THRESHOLD.as_millis() as u64,
            total_ms = self.started.elapsed().as_millis() as u64,
            "work db: migration chain complete",
        );
    }
}

/// `step!(timer, conn, migrate_x)` runs `migrate_x(conn)` through the chain
/// timer, naming the step after the function.
macro_rules! step {
    ($timer:expr, $conn:expr, $step:path) => {
        $timer.step($conn, stringify!($step), $step)
    };
}

impl WorkDb {
    /// Bring this database up to the current schema. A brand-new, empty
    /// database is seeded directly from [`Self::final_schema_ddl`] — the
    /// fast path, see its docs. Anything else (reopening an on-disk or
    /// shared-cache in-memory database that already went through `init()`
    /// once) replays the real incremental chain via
    /// [`Self::run_full_migration_chain`], so in-place upgrades of existing
    /// databases keep working exactly as before.
    pub(crate) fn init(&self) -> Result<()> {
        let conn = self.connect()?;
        if Self::has_any_existing_table(&conn)? {
            return Self::run_full_migration_chain(&conn);
        }
        Self::apply_final_schema_template(&conn)
    }

    /// `true` if this connection's database already has ANY user table —
    /// not just `metadata`. Gates the fast fresh-schema template path: that
    /// path replays `CREATE TABLE` statements captured verbatim from
    /// `sqlite_master.sql`, which (unlike this file's own DDL) does not
    /// retain `IF NOT EXISTS` and so errors outright if the table already
    /// exists. A caller can hand `init()` a database that already has some
    /// (but not all — e.g. a hand-seeded pre-v3 fixture missing `metadata`
    /// entirely) tables, so checking only for `metadata` is not enough:
    /// only a database with zero tables is safe to seed from the template.
    /// Everything else must replay the real incremental chain, whose
    /// `CREATE TABLE IF NOT EXISTS` / `table_has_column` guards tolerate a
    /// partially-present schema.
    fn has_any_existing_table(conn: &Connection) -> Result<bool> {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table')",
            [],
            |row| row.get(0),
        )
        .context("checking for existing tables")
    }

    /// Seed a brand-new, empty database directly from the final schema DDL
    /// instead of replaying ~80 incremental `migrate_*` calls (most of
    /// which are column-only `ALTER TABLE`/`table_has_column` probes against
    /// an empty database that never needed them) against an empty database.
    /// End state is identical to [`Self::run_full_migration_chain`] — see
    /// `full_migration_chain_produces_current_schema` in this module's
    /// tests, which exercises the real chain directly and remains the
    /// coverage of record for the migration steps themselves.
    fn apply_final_schema_template(conn: &Connection) -> Result<()> {
        for statement in Self::final_schema_ddl() {
            conn.execute_batch(statement)?;
        }
        // Not schema — this establishes required *data* (the local host row)
        // that `run_full_migration_chain` also performs unconditionally on
        // every call, fresh database or not. The local host's capability
        // *probe* is deliberately not here: schema init must not spawn
        // processes or touch the network — see the capability-probe note
        // at the top of this file.
        crate::host_registry::ensure_local_host(conn)?;
        Self::stamp_schema_version(conn)?;
        // Same data stamp `run_full_migration_chain` writes; the template
        // path never replays those migrate_* calls.
        migrate_stamp_pr_review_verdicts_since(conn)?;
        Ok(())
    }

    /// The current schema's full DDL — every `CREATE TABLE`/`CREATE INDEX`
    /// statement as it stands after every migration has run — captured once
    /// per process from a scratch in-memory database taken through the real
    /// [`Self::run_full_migration_chain`]. `sqlite_master.sql` reflects a
    /// table's *current* column set even after `ALTER TABLE ... ADD COLUMN`,
    /// so this is a complete final-state snapshot, not just the original
    /// schema-init batch.
    fn final_schema_ddl() -> &'static [String] {
        static DDL: OnceLock<Vec<String>> = OnceLock::new();
        DDL.get_or_init(|| {
            let scratch = Connection::open_in_memory().expect("open scratch db for schema template capture");
            Self::run_full_migration_chain(&scratch).expect("run full migration chain against scratch db");
            let mut stmt = scratch
                .prepare(
                    "SELECT sql FROM sqlite_master \
                     WHERE sql IS NOT NULL \
                     ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 ELSE 2 END",
                )
                .expect("prepare schema template capture query");
            stmt.query_map([], |row| row.get::<_, String>(0))
                .expect("query schema template")
                .collect::<rusqlite::Result<Vec<String>>>()
                .expect("collect schema template rows")
        })
    }

    /// Every migration this database has ever needed, applied in order
    /// against an existing connection. This is the only path for a database
    /// that isn't brand-new (reopening an on-disk or shared-cache database
    /// an earlier `init()` call already migrated) — every step here must
    /// stay idempotent against its own prior output, since `init()` can run
    /// it again on an already-current database. For a brand-new database,
    /// [`Self::apply_final_schema_template`] reaches the same end state far
    /// faster; that fast path's own template is itself captured by running
    /// this function once, in [`Self::final_schema_ddl`].
    pub(crate) fn run_full_migration_chain(conn: &Connection) -> Result<()> {
        let mut timer = MigrationChainTimer::start();
        let timer = &mut timer;
        step!(timer, conn, Self::schema_init_batch)?;
        step!(timer, conn, migrate_work_executions_v3)?;
        step!(timer, conn, migrate_tasks_autostart)?;
        step!(timer, conn, migrate_tasks_deferred)?;
        step!(timer, conn, migrate_tasks_human_driven)?;
        step!(timer, conn, migrate_tasks_design_reasoning_effort_xhigh)?;
        step!(timer, conn, migrate_tasks_completion_summary)?;
        step!(timer, conn, migrate_last_status_actor)?;
        step!(timer, conn, migrate_tasks_priority)?;
        step!(timer, conn, migrate_project_design_doc_columns)?;
        step!(timer, conn, migrate_tasks_created_via)?;
        step!(timer, conn, migrate_backfill_project_design_tasks)?;
        step!(timer, conn, migrate_tasks_repo_remote_url)?;
        step!(timer, conn, migrate_project_property_audit_table)?;
        step!(timer, conn, Self::post_v3_indexes)?;
        step!(timer, conn, migrate_timestamps_to_epoch)?;
        step!(timer, conn, migrate_tasks_blocked_reason)?;
        step!(timer, conn, migrate_products_auto_pr_maintenance_enabled)?;
        step!(timer, conn, migrate_conflict_resolutions_table)?;
        step!(timer, conn, migrate_backfill_blocked_reason_dependency)?;
        step!(timer, conn, migrate_work_attention_items_work_item_id)?;
        step!(timer, conn, migrate_work_attention_items_converted_task_id)?;
        step!(timer, conn, migrate_work_attention_items_last_raised_at)?;
        step!(timer, conn, migrate_tasks_effort_and_model_columns)?;
        step!(timer, conn, migrate_products_default_model)?;
        step!(timer, conn, migrate_task_blocked_signals_table)?;
        step!(timer, conn, migrate_ci_remediations_table)?;
        step!(timer, conn, migrate_ci_remediations_failure_kind_columns)?;
        step!(timer, conn, migrate_ci_failure_suppressions_table)?;
        step!(timer, conn, migrate_ci_inflight_observations_table)?;
        step!(timer, conn, migrate_tasks_ci_attempt_columns)?;
        step!(timer, conn, migrate_products_ci_attempt_budget)?;
        step!(timer, conn, migrate_products_dispatch_preamble)?;
        step!(timer, conn, migrate_products_design_repo)?;
        step!(timer, conn, migrate_products_docs_repo)?;
        step!(timer, conn, migrate_products_worker_branch_prefix)?;
        step!(timer, conn, migrate_work_executions_worker_branch_prefix)?;
        // The bespoke investigation-doc pointer columns are gone — the card
        // affordance now derives from `pr_url`, mirroring the design-doc model.
        // This drop is idempotent (fresh DBs never had the columns).
        step!(timer, conn, migrate_drop_tasks_investigation_doc_columns)?;
        // Per-task doc-pointer columns (doc_repo_remote_url / doc_branch /
        // doc_path) for the project-less doc-link card affordance —
        // investigations have no project, so they cannot reuse the
        // per-project `design_doc_*` columns. Detector-populated from the
        // PR's changed files, mirroring the design-doc model.
        step!(timer, conn, migrate_tasks_doc_pointer_columns)?;
        step!(timer, conn, migrate_backfill_task_blocked_signals)?;
        step!(timer, conn, migrate_effort_escalations_table)?;
        step!(timer, conn, migrate_null_redundant_task_repo_remote_urls)?;
        // Runs last so the per-product `(created_at, id)` backfill
        // sees every task/project row that earlier migrations may
        // have inserted (notably `migrate_backfill_project_design_tasks`).
        step!(timer, conn, migrate_short_id_columns)?;
        // Clears `autostart` on rows that have already been dispatched
        // so the single-shot semantics (AI #2, Incident 001) apply to
        // existing data too. Must run after `migrate_tasks_autostart`
        // so the column exists.
        step!(timer, conn, migrate_backfill_autostart_consumed)?;
        // Engine counter-metrics framework (phase 1). Independent of
        // every other table — runs last because order doesn't matter
        // for `CREATE TABLE IF NOT EXISTS`.
        step!(timer, conn, migrate_metrics_tables)?;
        step!(timer, conn, migrate_work_executions_pre_start_retry)?;
        step!(timer, conn, migrate_work_executions_pr_url)?;
        step!(timer, conn, migrate_work_executions_pr_head_before)?;
        step!(timer, conn, migrate_work_executions_pr_head_after)?;
        // Positive-evidence columns for the metadata-only CI-fix finalize
        // gate (issue #1252): the PR body snapshotted at run start plus the
        // Stop-boundary "metadata delta observed" marker.
        step!(timer, conn, migrate_work_executions_metadata_fix_columns)?;
        // PR poll state columns for CI + review indicators on Review-lane cards.
        step!(timer, conn, migrate_pr_poll_state_columns)?;
        // External tracker binding columns (products) and per-work-item
        // upstream-ref columns (tasks) plus partial indices. Design:
        // tools/boss/docs/designs/external-issue-tracker-sync-github-projects.md
        step!(timer, conn, migrate_external_tracker_columns)?;
        // Host registry tables + work_executions host columns for distributed
        // agent execution (phase 1 — schema + CLI only, no dispatch change).
        // Design: tools/boss/docs/designs/distributed-agent-execution-register-and-dispatch-to-remote-ssh-hosts.md
        step!(timer, conn, crate::host_registry::migrate_host_registry_tables)?;
        step!(timer, conn, crate::host_registry::migrate_work_executions_host_columns)?;
        // Phase 3: add host_id / cube_workspace_id / remote_pid to work_runs
        // so the macOS app (and run-failure paths) can see which host
        // a run executed on.
        step!(timer, conn, crate::host_registry::migrate_work_runs_host_columns)?;
        step!(timer, conn, crate::host_registry::migrate_work_runs_shell_pid)?;
        step!(timer, conn, migrate_work_runs_tmux_hosted)?;
        // Dispatch-time host health circuit breaker (starves-on-broken-host
        // fix): consecutive-failure counter used by
        // `record_host_dispatch_failure` / `_success` to auto-disable a
        // host that fails every dispatch instead of retrying it forever.
        step!(timer, conn, crate::host_registry::migrate_hosts_health_columns)?;
        step!(timer, conn, crate::host_registry::ensure_local_host)?;
        // The local host's capability *probe* used to run right here, inside
        // the schema chain. It no longer does — see the capability-probe
        // note at the top of this file.
        // Revision tasks (Phase 1): parent linkage column + index on tasks,
        // and soft-prefer signal on work_executions. Ships dark — the
        // `revision` kind is parseable but not yet dispatchable.
        // Design: tools/boss/docs/designs/revision-tasks.md
        step!(timer, conn, migrate_tasks_parent_task_id_column)?;
        step!(timer, conn, migrate_work_executions_prefer_is_soft)?;
        step!(timer, conn, migrate_work_executions_transient_failure_count)?;
        step!(timer, conn, migrate_work_executions_allow_dirty)?;
        // Revision card fix: update existing revision rows whose `name` was
        // set to the full description text (the original insertion behaviour).
        // The new insertion code uses only the first line; this backfill
        // aligns pre-fix rows by truncating to the first newline-terminated
        // segment using SQLite string functions. Rows whose name already
        // differs from description (e.g. manually patched via `boss task edit`)
        // are intentionally skipped.
        step!(timer, conn, migrate_revision_names_to_first_line)?;
        // Phase 1 of `unify-pr-remediation-on-revisions.md`: add the
        // `revision_task_id` reverse link to both attempt side-tables so
        // Phase 2+ can stamp the FK when a producer creates a revision.
        // Additive only — bespoke conflict/CI flows are untouched.
        step!(timer, conn, migrate_conflict_resolutions_revision_task_id)?;
        step!(timer, conn, migrate_ci_remediations_revision_task_id)?;
        // Comments in the markdown viewer (Phase 2): engine-backed comment
        // rows with W3C TextQuoteSelector anchors. Independent of every
        // other table; `CREATE TABLE IF NOT EXISTS` so order is irrelevant.
        // Design: tools/boss/docs/designs/comments-in-markdown-viewer.md
        step!(timer, conn, migrate_work_comments_table)?;
        // Comments Phase 3: magic-wand dispatch audit trail.
        step!(timer, conn, migrate_magic_wand_dispatches_table)?;
        // Comments Phase 4: PR-backed doc → Boss chore worker. Adds `chore_id`
        // to `magic_wand_dispatches` for audit linkage.
        step!(timer, conn, migrate_magic_wand_dispatches_add_chore_id)?;
        // Automations foundation (maintenance-tasks.md): `automations`,
        // `automation_runs`, `automation_short_id_sequences` tables plus
        // `tasks.source_automation_id` provenance column. Purely additive —
        // no existing rows are touched and no behaviour changes ship with
        // this migration. Everything depends on these tables existing.
        step!(timer, conn, migrate_automations_tables)?;
        step!(timer, conn, migrate_tasks_source_automation_id)?;
        // Attentions — new `attention_groups` and `attentions` tables for
        // agent-raised, human-actionable notifications (questions +
        // followups). Design: tools/boss/docs/designs/attentions.md.
        step!(timer, conn, migrate_attentions)?;
        // Editorial controls (P576, chore #1): per-product editorial_rules JSON
        // column, branch_naming snapshot on work_executions, and editorial_actions
        // audit table. Ships dark — no behaviour change until a product opts in.
        // Design: tools/boss/docs/designs/editorial-controls-for-agent-authored-prs-and-github-comments.md
        step!(timer, conn, migrate_editorial_controls_schema)?;
        // Normalise any effort_level rows stored as '' to NULL. The mapper
        // already converts '' → None at read time, but canonical DB storage
        // should use NULL (consistent with schema intent and SQL IS NULL queries).
        step!(timer, conn, migrate_tasks_empty_effort_to_null)?;
        // Behavior 8: upstream title/body drift detection. Adds
        // `external_ref_upstream_title` and `external_ref_upstream_body` to
        // `tasks` so the reconciler can tell apart operator edits from upstream
        // changes without parsing the description prose. Superseded by the
        // checksum migration below but kept for safe forward compatibility.
        step!(timer, conn, migrate_external_tracker_upstream_content)?;
        // Behavior 8 (revision): replace raw-content columns with SHA-256
        // checksums. Adds `external_ref_upstream_checksum` and
        // `external_ref_boss_checksum`; the old title/body columns remain in
        // the schema but are no longer read or written.
        step!(timer, conn, migrate_external_tracker_content_checksums)?;
        // P992 task 9: loop termination & bounds — per-PR review cycle
        // counter and last-reviewed SHA for the no-op skip gate.
        step!(timer, conn, migrate_tasks_review_cycle_columns)?;
        // P783 task 2: planner_runs audit ledger + per-project idempotency gate.
        // The UNIQUE partial index is created here (after the table) so SQLite
        // can resolve the `outcome` column. `CREATE TABLE IF NOT EXISTS` +
        // `CREATE INDEX IF NOT EXISTS` make this fully idempotent.
        // Design: tools/boss/docs/designs/auto-populate-project-tasks-on-design-pr-merge.md
        step!(timer, conn, migrate_planner_runs_table)?;
        // P1422 task B: driver data model (mix-and-match agent-driver
        // abstraction). Adds `tasks.driver` and `products.default_driver`
        // TEXT columns. NULL resolves to the engine default (`"claude"`).
        step!(timer, conn, migrate_tasks_driver_column)?;
        step!(timer, conn, migrate_products_default_driver)?;
        // Followup provenance: origin_task_short_id and origin_pr_number
        // on kind='followup' tasks (PR-review follow-ups created when the
        // reviewed PR merges before findings are addressed).
        step!(timer, conn, migrate_tasks_followup_provenance_columns)?;
        // Done-lane bucketing fix: add completed_at so the kanban can group
        // done tasks by their actual completion time instead of updated_at.
        step!(timer, conn, migrate_tasks_completed_at)?;
        // P783 task 5: tag tasks created by an auto-populate run with the
        // originating planner_runs.id, so the undo path can delete exactly
        // that batch. Purely additive nullable column; NULL for every
        // non-planner task.
        step!(timer, conn, migrate_tasks_planner_run_id)?;
        // Comment intent classification (P1a): the four intent-classifier
        // columns on `work_comments`. Purely additive, `NULL` for every
        // existing row (classifier never ran on them).
        // Design: tools/boss/docs/designs/comment-triggered-document-revisions.md
        step!(timer, conn, migrate_work_comments_intent_columns)?;
        // Comment intent handling (P3a): `answer_agent_runs` tracks each
        // ephemeral read-only answer-agent run against a question-classified
        // comment. Independent of every other table; `CREATE TABLE IF NOT
        // EXISTS` so order is irrelevant.
        // Design: tools/boss/docs/designs/comment-triggered-document-revisions.md
        step!(timer, conn, migrate_answer_agent_runs_table)?;
        step!(timer, conn, migrate_answer_agent_runs_execution_id_column)?;
        // Archival provenance: tasks.archived_reason surfaces why the
        // engine auto-archived a revision (parent PR merged/closed) so
        // `boss task show` doesn't leave the operator guessing.
        step!(timer, conn, migrate_tasks_archived_reason)?;
        step!(timer, conn, migrate_tasks_archival_provenance)?;
        step!(timer, conn, migrate_products_status_provenance)?;
        // Buckets 1&3 unification (P2a): `work_comments.revise_task_id`, the
        // soft FK a `CommentsReviseDoc` batch stamps on every comment it
        // addresses. Purely additive, `NULL` for every existing row.
        // Design: tools/boss/docs/designs/comment-triggered-document-revisions.md
        step!(timer, conn, migrate_work_comments_revise_task_id_column)?;
        // Buckets 1&3 unification (P2b) / comment intent handling (P3b):
        // `comment_thread_entries`, the shared engine-authored
        // nudge/answer/follow-up table. Purely additive, `CREATE TABLE IF NOT
        // EXISTS` so order is irrelevant.
        // Design: tools/boss/docs/designs/comment-triggered-document-revisions.md
        step!(timer, conn, migrate_comment_thread_entries_table)?;
        // Magic-wand removal (P2e): retire any `work_comments` row still
        // sitting in the now-invalid `dispatched` status. Data-only, no
        // schema change; the `magic_wand_dispatches` table itself is left
        // in place, unread, as a historical record.
        // Design: tools/boss/docs/designs/comment-triggered-document-revisions.md
        step!(timer, conn, migrate_retire_magic_wand_dispatched_comments)?;
        // Dispatch-failure surface: tasks.dispatch_failed_reason /
        // dispatch_failed_error / dispatch_failed_at, so a task that fails
        // to start (as opposed to merely waiting on a full worker pool)
        // renders an error inline on its kanban card.
        step!(timer, conn, migrate_tasks_dispatch_failure_columns)?;
        // `schema_version` is a coarse bookkeeping marker, not a per-migration
        // dispatch key: additive `CREATE TABLE IF NOT EXISTS` migrations (like
        // this one and the P1a intent columns above) ride the current marker
        // rather than bumping it. Left at '22'.
        // P1203 task 1: add score + merged_into_attention_id + linked_work_item_id
        // to `attentions` and create the `attention_merges` provenance ledger.
        // Design: tools/boss/docs/designs/notification-dedup-scoring.md §"Data model".
        step!(timer, conn, migrate_attentions_score_and_merges)?;
        // Comment intent classifier terminal-failure surface:
        // work_comments.intent_classification_failed_at /
        // intent_classification_error, so a comment whose classifier call
        // never succeeds shows a failed state instead of an indefinite
        // "classifying…" spinner. Purely additive.
        step!(timer, conn, migrate_work_comments_classification_failure_columns)?;
        // Dispatch-wait surface: work_executions.dispatch_wait_reason /
        // dispatch_wait_since, so a ready-but-undispatched execution's
        // kanban card can show the real defer reason (chain_serialized,
        // pool_exhausted) instead of a generic "Waiting for a slot".
        step!(timer, conn, migrate_work_executions_dispatch_wait)?;
        // Widen the conflict_resolutions idempotency key so the
        // stale-base re-arm path in conflict_watch can dispatch a fresh
        // attempt once a `succeeded` row's resolution has gone stale,
        // instead of colliding with that row's UNIQUE slot forever
        // (T2396 / PR #1874).
        step!(timer, conn, migrate_conflict_resolutions_widen_unique_key)?;
        // Regression fix (T1503/T1496): SHA-delta gate in recheck_for_pr must
        // only fire for revision executions after a Stop event has been
        // observed, not the moment any commit lands on the parent PR. Without
        // this guard the gate fires immediately when a *different* worker (e.g.
        // the parent chore's worker, still active) pushes to the same PR,
        // transitioning the revision to `in_review` before the revision worker
        // has done any work. `stop_seen` is set by `on_stop_inner` the first
        // time a Stop fires; the gate checks it before running the SHA delta
        // comparison.
        step!(timer, conn, migrate_work_executions_stop_seen)?;
        // `revision_stop_contributed_head`: SHA that on_stop_inner's Contributed arm
        // observed for a revision_implementation execution. recheck_for_pr uses this
        // as the T848 recovery gate: only finalize when head matches the SHA on_stop
        // previously attempted to finalize on — not on any head movement from a
        // concurrently-active parent worker.
        step!(timer, conn, migrate_work_executions_revision_stop_contributed_head)?;
        // Merge-queue sub-state: tasks.merge_queue_detail JSON blob (queue
        // position, GitHub's raw entry state, enqueued-at timestamp) for the
        // Review card's merging indicator (T2467/mono#1904).
        step!(timer, conn, migrate_tasks_merge_queue_detail_column)?;
        // Layer 0 conflict telemetry (T1 of
        // merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md):
        // conflict_resolutions.event_source / conflict_class /
        // resolved_by_rung, so producer-side conflicts (a normal
        // worker's own `cube workspace rebase` hitting
        // `REBASED_WITH_CONFLICTS`) and per-rung outcomes are captured,
        // not just in-review `conflict_watch` detections.
        step!(timer, conn, migrate_conflict_resolutions_telemetry_columns)?;
        // Durable in-flight marker for the mechanical escalation-ladder
        // rungs (0/1), which run inline in the engine with no dispatched
        // worker: `conflict_resolutions.mechanical_rung_in_flight`. Lets the
        // startup reconciler recover an attempt killed mid-rung by a restart
        // (2026-07-18 conflict-ladder restart incident) instead of leaving it
        // stranded and mistaken for a live "old-style" attempt forever.
        step!(timer, conn, migrate_conflict_resolutions_mechanical_rung_column)?;
        // One-time cleanup of orphaned `merge_queue_state = 'queued'` rows on
        // already-terminal tasks (see `mark_chore_pr_merged` and its sibling
        // terminal-transition sites, which now clear these columns going
        // forward) — snaps stale queue positions back to 1..N immediately
        // after deploy instead of leaving dead rows in `queued` state forever.
        step!(timer, conn, migrate_clear_merge_queue_state_on_terminal_tasks)?;
        // Boothby, the autonomous groundskeeper: boothby_passes /
        // _actions / _findings / _cursors. Independent of every other
        // table and additive-only (`CREATE TABLE IF NOT EXISTS`), so
        // ordering against its neighbours is irrelevant. Ships dark —
        // the tables exist but nothing writes them until the Boothby
        // agent lands; the actor-boothby capture in the mutation layer
        // is inert until a caller passes `LAST_STATUS_ACTOR_BOOTHBY`.
        step!(timer, conn, migrate_boothby_tables)?;
        // Automation dedup gate: `automation_dedup_suppressions`, the
        // append-only trace of tasks the gate refused to create because an
        // open sibling of the same automation already tracks the finding.
        // Independent of every other table and additive-only.
        step!(timer, conn, migrate_automation_dedup_suppressions_table)?;
        // `task_targets`: declared (and, later, actual) files/symbols a task
        // touches. First consumer is the automation pre-file dedup gate
        // (`WorkDb::create_automation_task`) — additive, independent of
        // every other table.
        // Design: tools/boss/docs/investigations/automation-duplicate-work-2026-07-14.md
        step!(timer, conn, migrate_task_targets_table)?;
        // Per-product merge mechanism (`direct` | `trunk_queue`), schema/contract
        // only — see trunk-merge-queue-integration-queue-backed-merges-merging-ui.md.
        step!(timer, conn, migrate_products_merge_mechanism)?;
        // `worker_proposals`: the durable ingress ledger behind the mediated
        // worker→engine proposal mechanism. Schema-only — no engine
        // behavior change until the follow-on submission/apply-pipeline
        // tasks land.
        // Design: tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md
        step!(timer, conn, migrate_worker_proposals_table)?;
        // `trunk_merge_intents`: the standing record of a merge-button click
        // submitted to Trunk's queue for a `trunk_queue` product. Additive,
        // independent of every other table.
        step!(timer, conn, migrate_trunk_merge_intents_table)?;
        // `github_merge_intents`: the GitHub-native counterpart. It records
        // a successful `gh pr merge --auto --squash` at a PR head so an empty
        // first post-submit probe cannot erase the requested merge.
        step!(timer, conn, migrate_github_merge_intents_table)?;
        // `tasks.blocked_detail`: verbatim long-form explanation of
        // `blocked_reason`, rendered as a tooltip on the kanban card's
        // blocked pill. `blocked_reason` itself stays a short, title-cased
        // label — this sibling column is where prose goes instead of
        // being crammed (and truncated) into the label.
        step!(timer, conn, migrate_tasks_blocked_detail_column)?;
        // `tasks.effort_matched_rule` / `tasks.effort_reasons`: first-class
        // effort-classification provenance (matched §Q4 rule + reasons
        // summary). Replaces free-text `[effort-classification]` tag
        // stuffing into `description`, which races autostart.
        step!(timer, conn, migrate_tasks_effort_provenance_columns)?;
        // `automation_runs.first_attempted_at`: the first-attempt timestamp
        // the scheduler's retry deadline is measured from, distinct from
        // `started_at` which the retry upsert rewrites on every attempt.
        step!(timer, conn, migrate_automation_runs_first_attempted_at_column)?;
        // `attentions.source_proposal_id`: provenance back to the
        // `worker_proposals` row a `followup_task` proposal staged a
        // followup-group member from. Additive, independent of every other
        // table. Implementation task 6 of the worker-proposal-api design.
        step!(timer, conn, migrate_attentions_source_proposal_id)?;
        // Comment intent taxonomy collapse: the classifier's retired
        // `directive`/`larger_change` split re-homed onto the single
        // `revision` intent value (nothing downstream ever branched on which
        // of the two a comment carried). Data-only, no schema change.
        step!(timer, conn, migrate_collapse_directive_larger_change_intent)?;
        // `tasks.pr_merge_state_status` / `tasks.pr_head_sha`: two fields the
        // merge poller's probe already fetches every sweep but previously
        // discarded. Backs `boss pr status` — see migration doc comment.
        step!(timer, conn, migrate_tasks_pr_status_columns)?;
        // `work_executions.pr_title_before`: PR title snapshot alongside the
        // existing `pr_body_before`. Backs `boss pr body` returning title
        // and body together.
        step!(timer, conn, migrate_work_executions_pr_title_before)?;
        // `product_decisions` + `decision_short_id_sequences`: product-scoped
        // wontfix/decided records (T-B2-decision). New table only — no
        // `tasks` column changes, so no collision with effort-provenance /
        // blocked_detail migrations above.
        step!(timer, conn, migrate_product_decisions_table)?;
        // `github_api_calls`: per-call GitHub API usage telemetry (caller
        // subsystem, API bucket, rateLimit reading). Independent of every
        // other table and additive-only. Rides the current schema marker.
        step!(timer, conn, migrate_github_api_calls_table)?;
        // `tasks.reasoning`: the capability signal (standard | investigation),
        // independent of `effort_level`'s size signal. Nullable, and NULL is
        // load-bearing — it means "never classified" and keeps the row on the
        // dispatcher's pre-existing kind-floor/effort-table path, so landing
        // this migration re-models nothing already in flight.
        step!(timer, conn, migrate_tasks_reasoning_column)?;
        // revision chains must be flat under the original non-revision
        // work item. New inserts already canonicalize in
        // `assert_parent_revisable_and_insert`; this rewrites any pre-existing
        // nested `parent_task_id` links (revision → revision) to the chain root
        // so the UI rollup and `list_revisions --parent <root>` surface the
        // full chain. Idempotent; preserves status/executions/deps/history.
        step!(timer, conn, migrate_flatten_nested_revision_parents)?;
        // `tasks.tags`: free-form ordered label strings for kanban cards.
        // JSON array text, default `[]`. Caps enforced at write.
        step!(timer, conn, migrate_tasks_tags_column)?;
        // `work_executions.driver_runtime_state`: opaque JSON from
        // AgentDriver::provision_workspace, handed back to teardown on
        // every termination path. Survives workspace release so future
        // Codex retention can operate only on a recorded root.
        step!(timer, conn, migrate_work_executions_driver_runtime_state)?;
        // `work_runs.progress_session_id`: the one current provider session
        // identity, stored in engine-owned SQLite rather than an
        // agent-writable provider home. Cleared by normal teardown.
        step!(timer, conn, migrate_work_runs_progress_session_id)?;
        // Raw provider usage on the run row. Captured on hook delivery rather
        // than finalization so orphaned executions retain their observed cost.
        step!(timer, conn, migrate_work_runs_cost_columns)?;
        // `work_runs.turn_boundary_at`: durable proof that THIS run's process
        // delivered a terminal result, so a one-turn-per-process worker's exit
        // can be told apart from a death across an engine restart.
        step!(timer, conn, migrate_work_runs_turn_boundary_at)?;
        // `work_runs.progress_ingress_checkpoint`: where a file-tailing
        // progress ingress had got to, so an engine restart re-attaches a
        // long-lived agent session's rollout at the right byte rather than
        // replaying it from zero or skipping to its end.
        step!(timer, conn, migrate_work_runs_progress_ingress_checkpoint)?;
        // `work_runs.semantic_progress_at` / `semantic_tool_condition`: last
        // driver-originated event time and tri-state tool condition, so tmux
        // re-adoption and later stale recovery can judge semantic health
        // after an engine restart without treating engine-synthesized
        // display timestamps as progress. Nullable; legacy NULL is unknown.
        step!(timer, conn, migrate_work_runs_semantic_progress)?;
        // Tmux session identity is durable per spawned run so startup
        // adoption can match a surviving session by its opaque token.
        // The spawn path does not use these columns until the tmux-hosting
        // rollout lands.
        step!(timer, conn, migrate_work_runs_tmux_columns)?;
        // `work_runs.liveness_anchor_at`: mutable liveness-age for durable
        // reconcilers, so readoption can reset the pane-attach clock without
        // stomping the immutable pane-spawn `started_at`.
        step!(timer, conn, migrate_work_runs_liveness_anchor_at)?;
        // `execution_driver_decisions`: one row per execution recording the
        // driver traffic allocation decision (driver + reason + the split it
        // was decided under). New table plus one additive column, independent
        // of every migration above.
        step!(timer, conn, migrate_execution_driver_decisions_table)?;
        // Resolved driver/model/effort values frozen only after a worker has
        // actually spawned. NULL on earlier rows means not recorded, never a
        // guessed default. Must run before the backfill below, which reads
        // `work_executions.driver` to decide which decision rows it may
        // safely rewrite.
        step!(timer, conn, migrate_work_executions_launch_config)?;
        // Correct the older decision records for execution kinds whose pool
        // overrides row/product pins. The backfill is self-idempotent.
        step!(timer, conn, migrate_backfill_pool_driver_decisions)?;
        // Fold a superseded `codex_dispatch_percentage` value into the
        // equivalent three-way split and drop the legacy key. Data-only,
        // self-idempotent — see the function doc comment.
        step!(timer, conn, migrate_driver_traffic_split_from_codex_percentage)?;
        // `pr_review_verdicts`: durable per-pass review-verdict ledger, written
        // atomically with `record_worker_pr_completion` so a `pr_review` pass
        // can never reach `completed` without a verdict row. Additive,
        // independent of every other table.
        step!(timer, conn, migrate_pr_review_verdicts_table)?;
        step!(timer, conn, migrate_stamp_pr_review_verdicts_since)?;
        // `pr_review_batches` / `pr_review_batch_members`: immutable review
        // target/profile snapshots and role-specific attempts. The tables are
        // persistence-only at this stage; dispatch still uses legacy review
        // orchestration until the batch reconciler lands.
        step!(timer, conn, migrate_pr_review_batches_tables)?;
        // Batch-verdict applier: one `pr_review_verdicts` row per batch,
        // keyed on the review-verdict proposal id so reapply is a no-op.
        step!(timer, conn, migrate_pr_review_verdicts_batch_columns)?;
        // One-time backfill: auto-resolve `pr_review_died_without_findings`
        // attentions already followed by a later completed review pass —
        // data-only, no schema change; self-idempotent.
        step!(timer, conn, migrate_backfill_resolve_stale_dead_review_attentions)?;
        // `work_executions.pr_head_baseline_absorbed`: set when on_stop_inner's
        // parent-push suppression path rewrites `pr_head_before` mid-run (a
        // head movement attributed to the concurrently-active parent worker,
        // not this revision). Once set, the SHA-delta gate's "head unchanged"
        // finding means "unchanged since the last absorbed baseline", not
        // "unchanged since the run started" — the ProvenAbsent evidence this
        // flag gates on must not be trusted the same way a never-absorbed
        // baseline is (mono#2606 revision).
        step!(timer, conn, migrate_work_executions_pr_head_baseline_absorbed)?;
        // Repair any `projects.status` corrupted by the pre-fix untyped
        // shared engine-status writer (out-of-enum values like `"todo"`),
        // then close the gap that let it happen: a `CHECK` constraint on
        // both `projects.status` and `tasks.status`. The repair MUST run
        // first — the constraint migration's table rebuild would
        // otherwise reject any still-corrupt row.
        step!(timer, conn, migrate_repair_invalid_project_status)?;
        step!(timer, conn, migrate_tasks_cancelled_status_to_archived)?;
        step!(timer, conn, migrate_projects_tasks_status_check)?;
        // Project lifecycle provenance: the current row states why its status
        // was selected, and the existing append-only property audit retains
        // every status transition (including transitions later superseded).
        step!(timer, conn, migrate_project_status_provenance)?;
        // `products.design_guidance`: kind-scoped design-directive guidance,
        // distinct from `dispatch_preamble` (every kind) and
        // `editorial_rules.instructions` (GitHub-visible surfaces only) — see
        // `migrate_products_design_guidance`'s doc comment.
        step!(timer, conn, migrate_products_design_guidance)?;
        // `work_attachments`: metadata for screenshot evidence kept for a
        // worker's own verification and for an operator inspecting a run
        // locally; bytes live content-addressed under the engine state root.
        // Independent of execution retention. Existing installations are
        // upgraded transactionally so evidence rows outlive pruned runs.
        // Design: tools/boss/docs/designs/worker-screenshot-evidence-attachments.md
        step!(timer, conn, migrate_work_attachments_table)?;
        step!(timer, conn, migrate_work_attachments_execution_retention)?;
        // Data correction: repair any `work_comments` row still reading
        // 'answered' off a failed, no-reply answer-agent run (the
        // "comment reads answered when its answer-agent run failed with no
        // reply" incident) — see the migration's own doc comment for the
        // exact repair rule. Must run after `migrate_work_comments_table`
        // and `migrate_answer_agent_runs_table`, both far earlier in this
        // list. Idempotent; a no-op on every subsequent startup.
        step!(timer, conn, migrate_correct_falsely_answered_comments_with_failed_runs)?;
        // Data correction: resolve stale pre-existing `orphan_sweep`
        // `churn_guard_parked` attention items. The current guarded sweep
        // will re-park genuinely orphaned work in the
        // `dispatch_failed_reason` representation — see the migration's own
        // doc comment and
        // `docs/designs/dispatch-halt-state-vs-attention-items.md`.
        // Idempotent; a no-op once no open items of this shape remain.
        step!(timer, conn, migrate_resolve_open_orphan_sweep_churn_guard_parked)?;
        // `work_comments.reopened_at`, stamped by reconciliation's
        // `Reopened` outcome so the sidebar can tell "never claimed" apart
        // from "claimed, then abandoned". Purely additive.
        step!(timer, conn, migrate_work_comments_reopened_at_column)?;
        // `work_executions.run_done_declared_at` / `run_done_outcome` /
        // `run_undeclared_at`: the durable record of whether a run's worker
        // declared itself finished (`boss propose done`), and of the
        // backstop having ended a run that never did. Additive columns on
        // an existing table; independent of every migration above.
        // Design: tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md
        step!(timer, conn, migrate_work_executions_run_done_columns)?;
        // `ideas` + `idea_short_id_sequences`: markdown drafts authored over
        // time and later graduated into a chore or project. Own table, own
        // `I<n>` namespace — deliberately not a `tasks` row. Additive-only
        // (`CREATE TABLE IF NOT EXISTS`); rides the current schema marker.
        step!(timer, conn, migrate_ideas_tables)?;
        step!(timer, conn, Self::stamp_schema_version)?;
        timer.finish();
        Ok(())
    }

    /// `work_executions_ready_idx` / `tasks_repo_idx`. Index creation must
    /// follow the migrations above: pre-v3 databases don't have `priority`
    /// until `migrate_work_executions_v3` adds it, and SQLite's `CREATE
    /// INDEX IF NOT EXISTS` errors on missing columns rather than silently
    /// skipping. The same rule applies to `tasks_repo_idx` against pre-v5
    /// databases that haven't yet been migrated.
    fn post_v3_indexes(conn: &Connection) -> Result<()> {
        conn.execute(
            "CREATE INDEX IF NOT EXISTS work_executions_ready_idx
                ON work_executions(status, priority, created_at)",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS tasks_repo_idx
                ON tasks(repo_remote_url, deleted_at)
                WHERE repo_remote_url IS NOT NULL",
            [],
        )?;
        Ok(())
    }

    /// Write the coarse `metadata.schema_version` marker both init paths
    /// leave behind.
    fn stamp_schema_version(conn: &Connection) -> Result<()> {
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES ('schema_version', '32')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )?;
        Ok(())
    }

    /// The original schema-init batch: every base table and index a
    /// pre-migration database needs before the incremental `migrate_*`
    /// steps can run. `IF NOT EXISTS` throughout, so it is idempotent on an
    /// already-current database. Its own step in the chain timing ledger.
    fn schema_init_batch(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS metadata (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS products (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                slug TEXT NOT NULL UNIQUE,
                description TEXT NOT NULL DEFAULT '',
                repo_remote_url TEXT,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                last_status_actor TEXT,
                status_basis TEXT,
                default_model TEXT,
                default_driver TEXT,
                ci_attempt_budget INTEGER NOT NULL DEFAULT 3,
                dispatch_preamble TEXT,
                design_guidance TEXT,
                external_tracker_kind TEXT,
                external_tracker_config TEXT,
                design_repo TEXT,
                worker_branch_prefix TEXT,
                merge_mechanism TEXT
            );

            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY,
                product_id TEXT NOT NULL REFERENCES products(id),
                name TEXT NOT NULL,
                slug TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                goal TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL,
                priority TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                design_doc_repo_remote_url TEXT,
                design_doc_branch TEXT,
                design_doc_path TEXT
            );

            CREATE UNIQUE INDEX IF NOT EXISTS projects_product_slug_idx
                ON projects(product_id, slug);

            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                product_id TEXT NOT NULL REFERENCES products(id),
                project_id TEXT REFERENCES projects(id),
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL,
                ordinal INTEGER,
                pr_url TEXT,
                deleted_at TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                autostart INTEGER NOT NULL DEFAULT 1,
                deferred INTEGER NOT NULL DEFAULT 0,
                human_driven INTEGER NOT NULL DEFAULT 0,
                completion_summary TEXT,
                priority TEXT NOT NULL DEFAULT 'medium',
                repo_remote_url TEXT,
                created_via TEXT NOT NULL DEFAULT 'unknown',
                effort_level TEXT,
                model_override TEXT,
                reasoning TEXT,
                driver TEXT,
                ci_attempt_budget INTEGER,
                ci_attempts_used INTEGER NOT NULL DEFAULT 0,
                external_ref_kind TEXT,
                external_ref_canonical_id TEXT,
                external_ref_raw TEXT,
                external_ref_synced_at TEXT,
                external_ref_unbound_at TEXT,
                archived_by TEXT,
                archived_at TEXT,
                archived_reason TEXT
            );

            CREATE INDEX IF NOT EXISTS tasks_product_idx
                ON tasks(product_id, kind, deleted_at);

            CREATE INDEX IF NOT EXISTS tasks_project_idx
                ON tasks(project_id, deleted_at, ordinal);

            CREATE TABLE IF NOT EXISTS work_executions (
                id TEXT PRIMARY KEY,
                work_item_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                repo_remote_url TEXT NOT NULL,
                cube_repo_id TEXT,
                cube_lease_id TEXT,
                cube_workspace_id TEXT,
                workspace_path TEXT,
                priority INTEGER NOT NULL DEFAULT 0,
                preferred_workspace_id TEXT,
                created_at TEXT NOT NULL,
                started_at TEXT,
                finished_at TEXT
            );

            CREATE INDEX IF NOT EXISTS work_executions_work_item_idx
                ON work_executions(work_item_id, created_at);

            CREATE TABLE IF NOT EXISTS work_runs (
                id TEXT PRIMARY KEY,
                execution_id TEXT NOT NULL REFERENCES work_executions(id) ON DELETE CASCADE,
                agent_id TEXT NOT NULL,
                status TEXT NOT NULL,
                error_text TEXT,
                result_summary TEXT,
                transcript_path TEXT,
                artifacts_path TEXT,
                created_at TEXT NOT NULL,
                started_at TEXT,
                liveness_anchor_at TEXT,
                finished_at TEXT,
                host_id TEXT NOT NULL DEFAULT 'local',
                cube_workspace_id TEXT,
                remote_pid INTEGER,
                shell_pid INTEGER,
                tmux_server_label TEXT,
                tmux_session_name TEXT,
                tmux_spawn_token TEXT,
                tmux_spawn_state TEXT,
                tmux_pane_pid INTEGER,
                tmux_hosted INTEGER NOT NULL DEFAULT 0
            );

            CREATE INDEX IF NOT EXISTS work_runs_execution_idx
                ON work_runs(execution_id, created_at);
            CREATE UNIQUE INDEX IF NOT EXISTS work_runs_tmux_spawn_token_idx
                ON work_runs(tmux_spawn_token)
                WHERE tmux_spawn_token IS NOT NULL;

            CREATE TABLE IF NOT EXISTS work_attention_items (
                id TEXT PRIMARY KEY,
                execution_id TEXT REFERENCES work_executions(id) ON DELETE CASCADE,
                work_item_id TEXT,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                title TEXT NOT NULL,
                body_markdown TEXT NOT NULL,
                created_at TEXT NOT NULL,
                resolved_at TEXT,
                converted_task_id TEXT,
                last_raised_at TEXT,
                CHECK (
                    (execution_id IS NOT NULL AND work_item_id IS NULL)
                    OR (execution_id IS NULL AND work_item_id IS NOT NULL)
                )
            );

            CREATE INDEX IF NOT EXISTS work_attention_items_execution_idx
                ON work_attention_items(execution_id, created_at);

            CREATE TABLE IF NOT EXISTS pane_summaries (
                work_item_id TEXT PRIMARY KEY,
                summary TEXT NOT NULL,
                basis_hash TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS work_item_dependencies (
                dependent_id     TEXT NOT NULL,
                prerequisite_id  TEXT NOT NULL,
                relation         TEXT NOT NULL DEFAULT 'blocks',
                created_at       TEXT NOT NULL,
                PRIMARY KEY (dependent_id, prerequisite_id, relation),
                CHECK (dependent_id <> prerequisite_id)
            );

            CREATE INDEX IF NOT EXISTS work_item_dependencies_prereq_idx
                ON work_item_dependencies(prerequisite_id, relation);

            CREATE INDEX IF NOT EXISTS work_item_dependencies_dependent_idx
                ON work_item_dependencies(dependent_id, relation);

            CREATE TABLE IF NOT EXISTS project_property_audit (
                id          TEXT PRIMARY KEY,
                project_id  TEXT NOT NULL,
                property    TEXT NOT NULL,
                old_value   TEXT,
                new_value   TEXT,
                actor       TEXT NOT NULL,
                changed_at  TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS project_property_audit_project_idx
                ON project_property_audit(project_id, changed_at);
            ",
        )?;
        Ok(())
    }

    /// Open the one raw connection a `WorkDb` (and every clone of it) will
    /// ever use, with every per-connection PRAGMA/behavior setting applied
    /// once up front. `WorkDb::connect()` just locks this connection's
    /// mutex — see the docs on `WorkDb::conn`.
    pub(in crate::work) fn open_raw_connection(path: &Path, memory: Option<&InMemoryAnchor>) -> Result<Connection> {
        let mut conn = if let Some(mem) = memory {
            // For in-memory databases, connect via the named shared-cache URI.
            Connection::open_with_flags(
                &mem.uri,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                    | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                    | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            )
            .with_context(|| format!("failed to open in-memory db {}", mem.uri))?
        } else {
            Connection::open(path).with_context(|| format!("failed to open work db {}", path.display()))?
        };
        // WAL lets readers and writers coexist (read-side concurrency
        // is unaffected by an in-flight write) and `busy_timeout`
        // turns lock contention into latency rather than an error
        // returned to the caller. `synchronous = NORMAL` is the
        // recommended pairing for WAL — durable across application
        // crashes, only loses commits on OS/power loss, which is fine
        // for engine state we can rebuild.
        conn.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;\n\
             PRAGMA synchronous = NORMAL;\n\
             PRAGMA foreign_keys = ON;",
        )?;
        // Default writes to `BEGIN IMMEDIATE`. With the previous
        // `BEGIN DEFERRED`, two concurrent writers could each open a
        // read-mode transaction, then both try to upgrade to write,
        // and the loser fails with `SQLITE_BUSY_SNAPSHOT` — which the
        // busy-timeout handler does NOT retry. `IMMEDIATE` acquires
        // the write lock up front so the second caller waits inside
        // the busy handler instead of racing.
        conn.set_transaction_behavior(TransactionBehavior::Immediate);
        Ok(conn)
    }

    /// Borrow the shared connection. Every call site gets what looks like
    /// its own `Connection` (via `Deref`/`DerefMut`) but is really a mutex
    /// guard over the one connection this `WorkDb` (and every clone of it)
    /// shares — see the docs on `WorkDb::conn`. A function that already
    /// holds a `connect()` guard must drop it (e.g. scope it in a block)
    /// before calling anything that connects again, or it deadlocks against
    /// itself.
    pub(crate) fn connect(&self) -> Result<PooledConnection<'_>> {
        self.conn
            .lock()
            .map_err(|_| anyhow::anyhow!("work db connection lock poisoned"))
    }

    /// Escape hatch: open a brand-new connection to this database instead of
    /// borrowing the shared pooled one. For the rare caller that genuinely
    /// needs an independent connection — e.g. a long-running `VACUUM INTO`
    /// snapshot that must not hold up every other operation on this `WorkDb`
    /// for its duration (see `database_backup::take_backup`). Most callers
    /// want [`Self::connect`], not this.
    pub(crate) fn connect_new(&self) -> Result<Connection> {
        Self::open_raw_connection(&self.path, self.memory.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Coverage of record for the real incremental migration chain — the
    /// fast fresh-database path (`apply_final_schema_template`) reaches an
    /// end state captured FROM a run of this same chain, so this is the
    /// only place that actually exercises every `migrate_*` step in order
    /// against a blank database.
    #[test]
    fn full_migration_chain_produces_current_schema() {
        let conn = Connection::open_in_memory().unwrap();
        WorkDb::run_full_migration_chain(&conn).unwrap();

        let schema_version: String = conn
            .query_row("SELECT value FROM metadata WHERE key = 'schema_version'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(schema_version, "32");

        let boothby_passes_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'boothby_passes')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            boothby_passes_exists,
            "expected boothby_passes table from migrate_boothby_tables, the last migration in the chain"
        );

        let dispatch_failed_reason_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'dispatch_failed_reason'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            dispatch_failed_reason_columns, 1,
            "expected tasks.dispatch_failed_reason from migrate_tasks_dispatch_failure_columns"
        );

        let local_host_exists: bool = conn
            .query_row("SELECT EXISTS(SELECT 1 FROM hosts WHERE id = 'local')", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(
            local_host_exists,
            "expected ensure_local_host to have seeded the local host row"
        );

        let worker_proposals_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'worker_proposals')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            worker_proposals_exists,
            "expected worker_proposals table from migrate_worker_proposals_table"
        );

        let work_attachments_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'work_attachments')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            work_attachments_exists,
            "expected work_attachments table from migrate_work_attachments_table"
        );

        let pr_review_verdicts_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'pr_review_verdicts')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            pr_review_verdicts_exists,
            "expected pr_review_verdicts table from migrate_pr_review_verdicts_table"
        );

        let pr_review_batches_exist: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name IN ('pr_review_batches', 'pr_review_batch_members')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            pr_review_batches_exist, 2,
            "expected both review-batch tables from migrate_pr_review_batches_tables"
        );

        let verdict_batch_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('pr_review_verdicts')
                 WHERE name IN ('batch_id', 'proposal_id')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            verdict_batch_columns, 2,
            "expected pr_review_verdicts.batch_id and proposal_id from migrate_pr_review_verdicts_batch_columns"
        );

        let verdicts_since_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key = ?1)",
                rusqlite::params![super::super::review_verdicts::PR_REVIEW_VERDICTS_SINCE_METADATA_KEY],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            verdicts_since_exists,
            "expected pr_review_verdicts_since metadata stamp from migrate_stamp_pr_review_verdicts_since"
        );

        let run_cost_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('work_runs')
                 WHERE name IN (
                     'model',
                     'output_tokens',
                     'input_tokens',
                     'cache_creation_tokens',
                     'cache_read_tokens',
                     'cache_creation_5m_tokens',
                     'cache_creation_1h_tokens',
                     'rounds',
                     'agent_active_ms'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(run_cost_columns, 9, "expected all per-run cost columns");

        let tmux_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('work_runs')
                 WHERE name IN (
                     'tmux_server_label',
                     'tmux_session_name',
                     'tmux_spawn_token',
                     'tmux_spawn_state',
                     'tmux_pane_pid'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tmux_columns, 5, "expected all per-run tmux columns");

        let liveness_anchor: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('work_runs')
                 WHERE name = 'liveness_anchor_at'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(liveness_anchor, 1, "expected work_runs.liveness_anchor_at");

        let semantic_progress_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('work_runs')
                 WHERE name IN ('semantic_progress_at', 'semantic_tool_condition')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            semantic_progress_columns, 2,
            "expected per-run semantic progress columns",
        );

        let tmux_token_index_exists: bool = conn
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM sqlite_master
                     WHERE type = 'index' AND name = 'work_runs_tmux_spawn_token_idx'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(tmux_token_index_exists, "expected unique tmux token index");
    }

    /// The fast path a brand-new database actually takes must reach the
    /// exact same schema shape as the real chain: same tables, same final
    /// column set per table (post-`ALTER TABLE ... ADD COLUMN`), same
    /// indexes.
    #[test]
    fn fresh_schema_template_matches_full_migration_chain() {
        let via_chain = Connection::open_in_memory().unwrap();
        WorkDb::run_full_migration_chain(&via_chain).unwrap();

        let via_template = Connection::open_in_memory().unwrap();
        WorkDb::apply_final_schema_template(&via_template).unwrap();

        let capture = |conn: &Connection| -> Vec<String> {
            let mut stmt = conn
                .prepare(
                    "SELECT type || ':' || name || ':' || sql FROM sqlite_master \
                     WHERE sql IS NOT NULL ORDER BY type, name",
                )
                .unwrap();
            stmt.query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<String>>>()
                .unwrap()
        };

        assert_eq!(capture(&via_chain), capture(&via_template));
    }
}
