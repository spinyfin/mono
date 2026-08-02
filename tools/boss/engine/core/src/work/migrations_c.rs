use super::*;

/// Repair rows whose `projects.status` holds a value outside
/// [`boss_protocol::ProjectStatus`]'s five-value vocabulary — corruption,
/// not a valid state. Closes a real incident: the untyped shared
/// engine-status writer that predated
/// [`crate::work::dep_helpers::write_engine_project_status`] /
/// `write_engine_task_status` let the auto-unblock cascade write the
/// `TaskStatus` literal `"todo"` into a `proj_` row whenever the
/// cascade's dependent happened to be a project rather than a task.
///
/// Every out-of-enum row is mapped to `'planned'` — both known-corrupted
/// rows were derived to have held that status before the bad write (each
/// was blocked within seconds of project creation and still carried only
/// its untouched seed design task, leaving no window for a manual
/// `active` move in between).
///
/// Restamps `updated_at` and `last_status_actor = 'engine'` together so
/// this write reads as one coherent correction. A prior hand-repair
/// against the live incident database fixed `status` alone via a direct
/// `UPDATE`, leaving those two columns pointing at the original corrupt
/// write instead of the fix that superseded it — this migration is what
/// makes the repair safe to replay identically on every other database
/// (or a restored backup) without carrying that same inconsistency
/// forward.
///
/// Logs each repaired row at `error!` (project id + the invalid value
/// found) so a recurrence is diagnosable from the trace instead of a
/// screenshot. Idempotent: the `WHERE status NOT IN (...)` guard matches
/// nothing once every row is repaired, so re-running this on a clean
/// database (every engine startup) is a cheap empty `SELECT`. Must run
/// before [`migrate_projects_tasks_status_check`], whose table rebuild
/// would otherwise reject any still-corrupt row via the new `CHECK`.
pub(crate) fn migrate_repair_invalid_project_status(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, status FROM projects
         WHERE status NOT IN ('planned', 'active', 'blocked', 'done', 'archived')",
    )?;
    let bad_rows: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    let now = now_string();
    for (project_id, invalid_status) in bad_rows {
        tracing::error!(
            project_id,
            invalid_status,
            repaired_status = "planned",
            "repairing project row with out-of-enum status (see write_engine_status type-hole postmortem)",
        );
        conn.execute(
            "UPDATE projects
             SET status = 'planned', last_status_actor = 'engine', updated_at = ?2
             WHERE id = ?1",
            params![project_id, now],
        )?;
    }
    Ok(())
}

/// Add `CHECK (status IN (...))` to `projects` and `tasks`, closing the
/// gap that let `migrate_repair_invalid_project_status`'s incident
/// happen in the first place: the column was `TEXT NOT NULL` with no
/// constraint of its own, so an out-of-vocabulary write only ever failed
/// later, at read time, in `map_project`/`map_task`. SQLite cannot
/// `ALTER TABLE ... ADD CONSTRAINT`, so both tables are rebuilt — same
/// pattern as [`migrate_work_attention_items_work_item_id`] and
/// [`migrate_conflict_resolutions_widen_unique_key`]. Must run after
/// [`migrate_repair_invalid_project_status`] so every existing row
/// already satisfies the new constraint before the rebuild's
/// `INSERT ... SELECT` runs.
///
/// The column set copied into each `_v2` table is the complete current
/// column list for that table (captured by actually running this
/// migration chain against a scratch database and reading back
/// `sqlite_master.sql`, then transcribing it verbatim plus the `CHECK`
/// clause) — not a hand-reconstruction from the `ALTER TABLE` history,
/// which spans two files and ~40 additive migrations and would be too
/// easy to get subtly wrong. Every index that existed on either table is
/// recreated afterward, since `DROP TABLE` drops its indexes too.
///
/// Idempotent: guarded by inspecting each table's live `CREATE TABLE`
/// DDL for the `CHECK` clause text, mirroring
/// `migrate_conflict_resolutions_widen_unique_key` (no new column is
/// added here, so `table_has_column` can't tell old from new).
pub(crate) fn migrate_projects_tasks_status_check(conn: &Connection) -> Result<()> {
    let projects_constrained: bool = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'projects'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|sql| sql.contains("CHECK (status IN"))
        .unwrap_or(false);
    if !projects_constrained {
        conn.execute_batch(
            "CREATE TABLE projects_v2 (
                 id TEXT PRIMARY KEY,
                 product_id TEXT NOT NULL REFERENCES products(id),
                 name TEXT NOT NULL,
                 slug TEXT NOT NULL,
                 description TEXT NOT NULL DEFAULT '',
                 goal TEXT NOT NULL DEFAULT '',
                 status TEXT NOT NULL CHECK (status IN ('planned', 'active', 'blocked', 'done', 'archived')),
                 priority TEXT NOT NULL,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 design_doc_repo_remote_url TEXT,
                 design_doc_branch TEXT,
                 design_doc_path TEXT,
                 last_status_actor TEXT NOT NULL DEFAULT 'human',
                 short_id INTEGER
             );
             INSERT INTO projects_v2
                 (id, product_id, name, slug, description, goal, status, priority, created_at, updated_at,
                  design_doc_repo_remote_url, design_doc_branch, design_doc_path, last_status_actor, short_id)
             SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at,
                    design_doc_repo_remote_url, design_doc_branch, design_doc_path, last_status_actor, short_id
               FROM projects;
             DROP TABLE projects;
             ALTER TABLE projects_v2 RENAME TO projects;
             CREATE UNIQUE INDEX IF NOT EXISTS projects_product_slug_idx
                 ON projects(product_id, slug);
             CREATE UNIQUE INDEX IF NOT EXISTS projects_product_short_id_idx
                 ON projects(product_id, short_id) WHERE short_id IS NOT NULL;",
        )?;
    }

    let tasks_constrained: bool = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'tasks'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|sql| sql.contains("CHECK (status IN"))
        .unwrap_or(false);
    if !tasks_constrained {
        conn.execute_batch(
            "CREATE TABLE tasks_v2 (
                 id TEXT PRIMARY KEY,
                 product_id TEXT NOT NULL REFERENCES products(id),
                 project_id TEXT REFERENCES projects(id),
                 kind TEXT NOT NULL,
                 name TEXT NOT NULL,
                 description TEXT NOT NULL DEFAULT '',
                 status TEXT NOT NULL CHECK (
                     status IN ('todo', 'active', 'blocked', 'in_review', 'done', 'archived', 'cancelled')
                 ),
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
                 last_status_actor TEXT NOT NULL DEFAULT 'human',
                 blocked_reason TEXT,
                 blocked_attempt_id TEXT,
                 doc_repo_remote_url TEXT,
                 doc_branch TEXT,
                 doc_path TEXT,
                 short_id INTEGER,
                 ci_required_state TEXT,
                 review_required_state TEXT,
                 ci_required_detail TEXT,
                 review_required_detail TEXT,
                 pr_state_polled_at TEXT,
                 merge_queue_state TEXT,
                 pr_mergeable_state TEXT,
                 parent_task_id TEXT,
                 source_automation_id TEXT REFERENCES automations(id),
                 external_ref_upstream_title TEXT,
                 external_ref_upstream_body TEXT,
                 external_ref_upstream_checksum TEXT,
                 external_ref_boss_checksum TEXT,
                 review_cycle INTEGER NOT NULL DEFAULT 0,
                 last_reviewed_sha TEXT,
                 origin_task_short_id INTEGER,
                 origin_pr_number INTEGER,
                 completed_at TEXT,
                 planner_run_id TEXT,
                 archived_reason TEXT,
                 dispatch_failed_reason TEXT,
                 dispatch_failed_error TEXT,
                 dispatch_failed_at TEXT,
                 merge_queue_detail TEXT,
                 blocked_detail TEXT,
                 effort_matched_rule TEXT,
                 effort_reasons TEXT,
                 pr_merge_state_status TEXT,
                 pr_head_sha TEXT,
                 pr_status_observed_at TEXT,
                 tags TEXT NOT NULL DEFAULT '[]'
             );
             INSERT INTO tasks_v2
                 (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at,
                  created_at, updated_at, autostart, deferred, human_driven, completion_summary, priority,
                  repo_remote_url, created_via, effort_level, model_override, reasoning, driver, ci_attempt_budget,
                  ci_attempts_used, external_ref_kind, external_ref_canonical_id, external_ref_raw,
                  external_ref_synced_at, external_ref_unbound_at, last_status_actor, blocked_reason,
                  blocked_attempt_id, doc_repo_remote_url, doc_branch, doc_path, short_id, ci_required_state,
                  review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at,
                  merge_queue_state, pr_mergeable_state, parent_task_id, source_automation_id,
                  external_ref_upstream_title, external_ref_upstream_body, external_ref_upstream_checksum,
                  external_ref_boss_checksum, review_cycle, last_reviewed_sha, origin_task_short_id,
                  origin_pr_number, completed_at, planner_run_id, archived_reason, dispatch_failed_reason,
                  dispatch_failed_error, dispatch_failed_at, merge_queue_detail, blocked_detail,
                  effort_matched_rule, effort_reasons, pr_merge_state_status, pr_head_sha, pr_status_observed_at,
                  tags)
             SELECT
                  id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at,
                  created_at, updated_at, autostart, deferred, human_driven, completion_summary, priority,
                  repo_remote_url, created_via, effort_level, model_override, reasoning, driver, ci_attempt_budget,
                  ci_attempts_used, external_ref_kind, external_ref_canonical_id, external_ref_raw,
                  external_ref_synced_at, external_ref_unbound_at, last_status_actor, blocked_reason,
                  blocked_attempt_id, doc_repo_remote_url, doc_branch, doc_path, short_id, ci_required_state,
                  review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at,
                  merge_queue_state, pr_mergeable_state, parent_task_id, source_automation_id,
                  external_ref_upstream_title, external_ref_upstream_body, external_ref_upstream_checksum,
                  external_ref_boss_checksum, review_cycle, last_reviewed_sha, origin_task_short_id,
                  origin_pr_number, completed_at, planner_run_id, archived_reason, dispatch_failed_reason,
                  dispatch_failed_error, dispatch_failed_at, merge_queue_detail, blocked_detail,
                  effort_matched_rule, effort_reasons, pr_merge_state_status, pr_head_sha, pr_status_observed_at,
                  tags
               FROM tasks;
             DROP TABLE tasks;
             ALTER TABLE tasks_v2 RENAME TO tasks;
             CREATE INDEX IF NOT EXISTS tasks_product_idx
                 ON tasks(product_id, kind, deleted_at);
             CREATE INDEX IF NOT EXISTS tasks_project_idx
                 ON tasks(project_id, deleted_at, ordinal);
             CREATE INDEX IF NOT EXISTS tasks_repo_idx
                 ON tasks(repo_remote_url, deleted_at) WHERE repo_remote_url IS NOT NULL;
             CREATE INDEX IF NOT EXISTS idx_tasks_parent_task_id
                 ON tasks(parent_task_id);
             CREATE UNIQUE INDEX IF NOT EXISTS tasks_product_short_id_idx
                 ON tasks(product_id, short_id) WHERE short_id IS NOT NULL;
             CREATE INDEX IF NOT EXISTS tasks_source_automation_idx
                 ON tasks(source_automation_id, status) WHERE source_automation_id IS NOT NULL;
             CREATE INDEX IF NOT EXISTS tasks_external_ref_idx
                 ON tasks (external_ref_kind, external_ref_canonical_id)
                 WHERE external_ref_canonical_id IS NOT NULL;
             CREATE UNIQUE INDEX IF NOT EXISTS tasks_external_ref_bound_uniq
                 ON tasks (external_ref_kind, external_ref_canonical_id)
                 WHERE external_ref_canonical_id IS NOT NULL
                   AND external_ref_unbound_at  IS NULL
                   AND deleted_at               IS NULL;",
        )?;
    }

    Ok(())
}
