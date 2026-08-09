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

/// `(column name, its declaration in the rebuilt table)` for every column
/// `projects` can carry, in schema order.
///
/// The rebuild derives its `CREATE TABLE`/`INSERT`/`SELECT` column lists
/// from this table rather than from three hand-transcribed copies of the
/// same list. It also asserts that `PRAGMA table_info` reports nothing
/// outside this set and fails loudly if it does, so a later additive
/// migration cannot silently drop a column here by forgetting to update
/// it. The rebuilt table declares exactly the subset that is live in
/// *this* database, so a database predating a later column is rebuilt
/// without it instead of gaining an empty one.
///
/// The list is therefore a superset of any one database's live columns:
/// `design_doc` / `design_doc_updated_at` / `design_doc_draft` are the
/// pre-`design_doc_*`-pointer generation of design-doc storage, and no
/// migration ever dropped them, so a database old enough still carries
/// them (and their contents).
const PROJECTS_STATUS_CHECK_COLUMNS: &[(&str, &str)] = &[
    ("id", "id TEXT PRIMARY KEY"),
    ("product_id", "product_id TEXT NOT NULL REFERENCES products(id)"),
    ("name", "name TEXT NOT NULL"),
    ("slug", "slug TEXT NOT NULL"),
    ("description", "description TEXT NOT NULL DEFAULT ''"),
    ("goal", "goal TEXT NOT NULL DEFAULT ''"),
    (
        "status",
        "status TEXT NOT NULL CHECK (status IN ('planned', 'active', 'blocked', 'done', 'archived'))",
    ),
    ("priority", "priority TEXT NOT NULL"),
    ("created_at", "created_at TEXT NOT NULL"),
    ("updated_at", "updated_at TEXT NOT NULL"),
    ("design_doc_repo_remote_url", "design_doc_repo_remote_url TEXT"),
    ("design_doc_branch", "design_doc_branch TEXT"),
    ("design_doc_path", "design_doc_path TEXT"),
    ("design_doc", "design_doc TEXT"),
    ("design_doc_updated_at", "design_doc_updated_at TEXT"),
    ("design_doc_draft", "design_doc_draft TEXT"),
    ("last_status_actor", "last_status_actor TEXT NOT NULL DEFAULT 'human'"),
    ("short_id", "short_id INTEGER"),
    ("status_basis", "status_basis TEXT"),
];

/// Add current and historical provenance for project status changes.
///
/// `projects.status_basis` travels with the denormalized current status so
/// ordinary project reads can explain it. `project_property_audit.basis`
/// extends the existing general project-property ledger; status writers add
/// `property = 'status'` rows there so a later transition cannot erase the
/// evidence needed to explain an earlier one.
pub(crate) fn migrate_project_status_provenance(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "projects", "status_basis")? {
        conn.execute("ALTER TABLE projects ADD COLUMN status_basis TEXT", [])?;
    }
    if !table_has_column(conn, "project_property_audit", "basis")? {
        conn.execute("ALTER TABLE project_property_audit ADD COLUMN basis TEXT", [])?;
    }
    Ok(())
}

/// Every index on `projects`, recreated after the rebuild — `DROP TABLE`
/// takes a table's indexes with it.
const PROJECTS_STATUS_CHECK_INDEXES: &str = "\
    CREATE UNIQUE INDEX IF NOT EXISTS projects_product_slug_idx
        ON projects(product_id, slug);
    CREATE UNIQUE INDEX IF NOT EXISTS projects_product_short_id_idx
        ON projects(product_id, short_id) WHERE short_id IS NOT NULL;";

/// `(column name, its declaration in the rebuilt table)` for every column
/// `tasks` can carry — see [`PROJECTS_STATUS_CHECK_COLUMNS`] for how this
/// is used and why it is not transcribed into the SQL by hand.
///
/// This list is deliberately a superset of any one database's live column
/// set: `investigation_doc_repo_remote_url` survives only on databases old
/// enough to predate `migrate_drop_tasks_investigation_doc_columns` (which
/// drops its two siblings but not it), and is filtered out of the rebuild
/// on every database that does not have it.
const TASKS_STATUS_CHECK_COLUMNS: &[(&str, &str)] = &[
    ("id", "id TEXT PRIMARY KEY"),
    ("product_id", "product_id TEXT NOT NULL REFERENCES products(id)"),
    ("project_id", "project_id TEXT REFERENCES projects(id)"),
    ("kind", "kind TEXT NOT NULL"),
    ("name", "name TEXT NOT NULL"),
    ("description", "description TEXT NOT NULL DEFAULT ''"),
    (
        "status",
        "status TEXT NOT NULL CHECK (status IN ('todo', 'active', 'blocked', 'in_review', 'done', 'archived'))",
    ),
    ("ordinal", "ordinal INTEGER"),
    ("pr_url", "pr_url TEXT"),
    ("deleted_at", "deleted_at TEXT"),
    ("created_at", "created_at TEXT NOT NULL"),
    ("updated_at", "updated_at TEXT NOT NULL"),
    ("autostart", "autostart INTEGER NOT NULL DEFAULT 1"),
    ("deferred", "deferred INTEGER NOT NULL DEFAULT 0"),
    ("human_driven", "human_driven INTEGER NOT NULL DEFAULT 0"),
    ("completion_summary", "completion_summary TEXT"),
    ("priority", "priority TEXT NOT NULL DEFAULT 'medium'"),
    ("repo_remote_url", "repo_remote_url TEXT"),
    ("created_via", "created_via TEXT NOT NULL DEFAULT 'unknown'"),
    ("effort_level", "effort_level TEXT"),
    ("model_override", "model_override TEXT"),
    ("reasoning", "reasoning TEXT"),
    ("driver", "driver TEXT"),
    ("ci_attempt_budget", "ci_attempt_budget INTEGER"),
    ("ci_attempts_used", "ci_attempts_used INTEGER NOT NULL DEFAULT 0"),
    ("external_ref_kind", "external_ref_kind TEXT"),
    ("external_ref_canonical_id", "external_ref_canonical_id TEXT"),
    ("external_ref_raw", "external_ref_raw TEXT"),
    ("external_ref_synced_at", "external_ref_synced_at TEXT"),
    ("external_ref_unbound_at", "external_ref_unbound_at TEXT"),
    ("last_status_actor", "last_status_actor TEXT NOT NULL DEFAULT 'human'"),
    ("blocked_reason", "blocked_reason TEXT"),
    ("blocked_attempt_id", "blocked_attempt_id TEXT"),
    (
        "investigation_doc_repo_remote_url",
        "investigation_doc_repo_remote_url TEXT",
    ),
    ("doc_repo_remote_url", "doc_repo_remote_url TEXT"),
    ("doc_branch", "doc_branch TEXT"),
    ("doc_path", "doc_path TEXT"),
    ("short_id", "short_id INTEGER"),
    ("ci_required_state", "ci_required_state TEXT"),
    ("review_required_state", "review_required_state TEXT"),
    ("ci_required_detail", "ci_required_detail TEXT"),
    ("review_required_detail", "review_required_detail TEXT"),
    ("pr_state_polled_at", "pr_state_polled_at TEXT"),
    ("merge_queue_state", "merge_queue_state TEXT"),
    ("pr_mergeable_state", "pr_mergeable_state TEXT"),
    ("parent_task_id", "parent_task_id TEXT"),
    (
        "source_automation_id",
        "source_automation_id TEXT REFERENCES automations(id)",
    ),
    ("external_ref_upstream_title", "external_ref_upstream_title TEXT"),
    ("external_ref_upstream_body", "external_ref_upstream_body TEXT"),
    ("external_ref_upstream_checksum", "external_ref_upstream_checksum TEXT"),
    ("external_ref_boss_checksum", "external_ref_boss_checksum TEXT"),
    ("review_cycle", "review_cycle INTEGER NOT NULL DEFAULT 0"),
    ("last_reviewed_sha", "last_reviewed_sha TEXT"),
    ("origin_task_short_id", "origin_task_short_id INTEGER"),
    ("origin_pr_number", "origin_pr_number INTEGER"),
    ("completed_at", "completed_at TEXT"),
    ("planner_run_id", "planner_run_id TEXT"),
    ("archived_reason", "archived_reason TEXT"),
    ("dispatch_failed_reason", "dispatch_failed_reason TEXT"),
    ("dispatch_failed_error", "dispatch_failed_error TEXT"),
    ("dispatch_failed_at", "dispatch_failed_at TEXT"),
    ("merge_queue_detail", "merge_queue_detail TEXT"),
    ("blocked_detail", "blocked_detail TEXT"),
    ("effort_matched_rule", "effort_matched_rule TEXT"),
    ("effort_reasons", "effort_reasons TEXT"),
    ("pr_merge_state_status", "pr_merge_state_status TEXT"),
    ("pr_head_sha", "pr_head_sha TEXT"),
    ("pr_status_observed_at", "pr_status_observed_at TEXT"),
    ("tags", "tags TEXT NOT NULL DEFAULT '[]'"),
];

/// Every index on `tasks`, recreated after the rebuild.
const TASKS_STATUS_CHECK_INDEXES: &str = "\
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
          AND deleted_at               IS NULL;";

/// Add `CHECK (status IN (...))` to `projects` and `tasks`, closing the
/// gap that let `migrate_repair_invalid_project_status`'s incident
/// happen in the first place: the column was `TEXT NOT NULL` with no
/// constraint of its own, so an out-of-vocabulary write only ever failed
/// later, at read time, in `map_project`/`map_task`. SQLite cannot
/// `ALTER TABLE ... ADD CONSTRAINT`, so both tables are rebuilt. Must run
/// after [`migrate_repair_invalid_project_status`] so every existing row
/// already satisfies the new constraint before the rebuild's
/// `INSERT ... SELECT` runs.
///
/// ## The rebuild follows SQLite's 12-step procedure, deliberately
///
/// <https://sqlite.org/lang_altertable.html#otheralter>. The two
/// precedent rebuilds in this codebase
/// ([`migrate_work_attention_items_work_item_id`],
/// [`migrate_conflict_resolutions_widen_unique_key`]) issue a bare
/// `execute_batch` with foreign keys left enabled, and are safe only
/// because neither of their tables is an FK parent. `projects` and
/// `tasks` both are — `tasks.project_id` and
/// `attention_groups.association_project_id` reference `projects`;
/// `attention_groups.association_task_id`, `task_targets.task_id`,
/// `automation_runs.produced_task_id` and
/// `automation_dedup_suppressions.surviving_task_id` reference `tasks` —
/// and under enforced foreign keys `DROP TABLE` performs an implicit
/// `DELETE FROM` that aborts with `FOREIGN KEY constraint failed` the
/// moment a referencing row exists. So:
///
/// - `PRAGMA foreign_keys = OFF` is set *outside* the transaction (the
///   pragma is a no-op inside one) and restored afterwards on both the
///   success and the error path;
/// - the whole rebuild runs inside one explicit transaction, so an abort
///   part-way can never leave a committed `_v2` scratch table stranded;
/// - `PRAGMA foreign_key_check` runs before the commit and any reported
///   row fails the migration loudly rather than committing a database
///   whose references no longer resolve.
///
/// ## Recovering an already-damaged database
///
/// The version of this migration that shipped in `boss-v1.0.485` had
/// neither the pragma nor the transaction, so on any database with a
/// referencing `tasks` row it committed `CREATE TABLE projects_v2` and
/// its `INSERT`, then aborted on `DROP TABLE projects` — leaving the
/// scratch table behind and wedging every subsequent startup on
/// `table projects_v2 already exists`. Dropping the scratch tables up
/// front is what lets a fixed build open such a database. They are
/// scratch artifacts of a failed attempt and hold nothing the live table
/// does not, so they are dropped rather than adopted — `CREATE TABLE IF
/// NOT EXISTS` in their place would silently keep a possibly-stale copy.
///
/// ## Idempotence
///
/// Guarded by inspecting each table's live `CREATE TABLE` DDL for the
/// `CHECK` clause, mirroring
/// `migrate_conflict_resolutions_widen_unique_key` (no new column is
/// added here, so `table_has_column` can't tell old from new). The check
/// is whitespace-insensitive: the shipped version tested for the literal
/// `"CHECK (status IN"`, which the `tasks` DDL it generated split across
/// two lines and so never matched, re-running that rebuild on every
/// single startup.
pub(crate) fn migrate_projects_tasks_status_check(conn: &Connection) -> Result<()> {
    let projects_constrained = table_has_status_check(conn, "projects")?;
    let tasks_constrained =
        table_has_status_check(conn, "tasks")? && !table_status_check_includes(conn, "tasks", "cancelled")?;
    let scratch_present = table_exists(conn, "projects_v2")? || table_exists(conn, "tasks_v2")?;
    if projects_constrained && tasks_constrained && !scratch_present {
        return Ok(());
    }

    // Steps 1 and 12 of the 12-step procedure. `PRAGMA foreign_keys` is a
    // no-op inside a transaction, so it has to be toggled around the
    // outside of one.
    let foreign_keys_were_on: bool = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    if foreign_keys_were_on {
        conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
    }
    let rebuilt = rebuild_projects_and_tasks_with_status_check(conn, projects_constrained, tasks_constrained);
    // Restore on the error path too: a failed rebuild must not leave this
    // connection running unenforced for the rest of its life.
    let restored = if foreign_keys_were_on {
        conn.execute_batch("PRAGMA foreign_keys = ON;")
    } else {
        Ok(())
    };
    rebuilt?;
    restored?;
    Ok(())
}

/// Collapse retired task status data before rebuilding the status constraint.
/// The existing `archived_reason` is intentionally retained as the one
/// provenance surface for both automatic archival and this historical
/// migration; no second terminal state or extra column is needed.
pub(crate) fn migrate_tasks_cancelled_status_to_archived(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE tasks
         SET status = 'archived',
             archived_reason = COALESCE(NULLIF(archived_reason, ''), 'migrated from legacy cancelled status'),
             completed_at = COALESCE(completed_at, created_at),
             merge_queue_state = NULL,
             merge_queue_detail = NULL
         WHERE status = 'cancelled'",
        [],
    )?;
    Ok(())
}

/// Steps 2–11: one transaction covering both table rebuilds, the
/// pre-commit `foreign_key_check`, and the scratch-table cleanup that
/// recovers a database wedged by the `boss-v1.0.485` build.
fn rebuild_projects_and_tasks_with_status_check(
    conn: &Connection,
    projects_constrained: bool,
    tasks_constrained: bool,
) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let result = (|| -> Result<()> {
        conn.execute_batch("DROP TABLE IF EXISTS projects_v2;\nDROP TABLE IF EXISTS tasks_v2;")?;
        if !projects_constrained {
            rebuild_table_with_status_check(
                conn,
                "projects",
                PROJECTS_STATUS_CHECK_COLUMNS,
                PROJECTS_STATUS_CHECK_INDEXES,
            )?;
        }
        if !tasks_constrained {
            rebuild_table_with_status_check(conn, "tasks", TASKS_STATUS_CHECK_COLUMNS, TASKS_STATUS_CHECK_INDEXES)?;
        }
        assert_no_foreign_key_violations(conn)
    })();
    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT;")?;
            Ok(())
        }
        Err(err) => {
            if let Err(rollback_err) = conn.execute_batch("ROLLBACK;") {
                tracing::error!(
                    error = %rollback_err,
                    "rolling back the failed projects/tasks status CHECK rebuild also failed",
                );
            }
            Err(err)
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Test hook: set to a table name to delete one row from that table's
    /// `_v2` staging copy between the copy and the row-count assertion,
    /// so that assertion — the guardrail standing between a botched copy
    /// and permanent data loss — is exercised for real rather than
    /// asserted about. Thread-local, not global, so it cannot leak into
    /// the other tests sharing this process.
    pub(crate) static SABOTAGE_STAGED_COPY_FOR_TABLE: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

/// Steps 4–8 for one table: build `<table>_v2` from `columns` filtered to
/// what this database actually has, copy every row, **verify the copy is
/// complete**, and only then swap it in and recreate the indexes
/// `DROP TABLE` took with it.
///
/// Two assertions guard the destructive step, and both abort the whole
/// transaction rather than proceed:
///
/// - every column on the live table must have a declaration in `columns`.
///   A live column with nowhere to go would otherwise be dropped
///   silently, which is how `tasks.investigation_doc_repo_remote_url`
///   (live on databases predating
///   `migrate_drop_tasks_investigation_doc_columns`, which drops its two
///   siblings but not it) was one shipped rebuild away from vanishing.
/// - the staging table's row count must equal the source's *before*
///   `DROP TABLE` runs. A rebuild that dropped the original after an
///   incomplete copy would destroy rows with no way back; this turns
///   that into a failed migration instead.
fn rebuild_table_with_status_check(
    conn: &Connection,
    table: &str,
    columns: &[(&str, &str)],
    indexes: &str,
) -> Result<()> {
    let live = live_column_names(conn, table)?;
    let unknown: Vec<&str> = live
        .iter()
        .map(String::as_str)
        .filter(|name| !columns.iter().any(|(known, _)| *known == *name))
        .collect();
    if !unknown.is_empty() {
        bail!(
            "`{table}` carries column(s) [{}] that the status-CHECK rebuild has no declaration for; \
             add them to this migration's column list — rebuilding without them would drop the data",
            unknown.join(", ")
        );
    }
    let carried: Vec<(&str, &str)> = columns
        .iter()
        .filter(|(name, _)| live.iter().any(|live_name| live_name == name))
        .copied()
        .collect();
    let declarations = carried
        .iter()
        .map(|(_, declaration)| format!("    {declaration}"))
        .collect::<Vec<_>>()
        .join(",\n");
    let names = carried.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(", ");

    // Non-destructive half: stage a complete copy alongside the original.
    conn.execute_batch(&format!(
        "CREATE TABLE {table}_v2 (\n{declarations}\n);\n\
         INSERT INTO {table}_v2 ({names})\n\
         SELECT {names} FROM {table};"
    ))
    .with_context(|| format!("staging the `{table}` rebuild with a status CHECK constraint"))?;

    #[cfg(test)]
    if SABOTAGE_STAGED_COPY_FOR_TABLE.with(std::cell::Cell::get) == Some(table) {
        conn.execute(
            &format!("DELETE FROM {table}_v2 WHERE rowid = (SELECT MIN(rowid) FROM {table}_v2)"),
            [],
        )?;
    }

    let source_rows: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))?;
    let staged_rows: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}_v2"), [], |row| row.get(0))?;
    if source_rows != staged_rows {
        bail!(
            "the `{table}` status CHECK rebuild staged {staged_rows} of {source_rows} row(s); \
             refusing to drop `{table}` with the copy incomplete (rolling back — no rows are lost, \
             but this database cannot be migrated until the cause is understood)"
        );
    }

    // Destructive half, reached only with a verified-complete copy in hand.
    conn.execute_batch(&format!(
        "DROP TABLE {table};\n\
         ALTER TABLE {table}_v2 RENAME TO {table};\n\
         {indexes}"
    ))
    .with_context(|| format!("swapping in the rebuilt `{table}` with a status CHECK constraint"))?;
    Ok(())
}

/// Step 10: any row here means the rebuild left a reference that no
/// longer resolves, which must abort the migration rather than commit.
fn assert_no_foreign_key_violations(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
    let violations: Vec<String> = stmt
        .query_map([], |row| {
            let child: String = row.get(0)?;
            let rowid: Option<i64> = row.get(1)?;
            let parent: String = row.get(2)?;
            Ok(format!("{child}(rowid {rowid:?}) -> {parent}"))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !violations.is_empty() {
        bail!(
            "the projects/tasks status CHECK rebuild would leave {} unresolved foreign key \
             reference(s), refusing to commit: {}",
            violations.len(),
            violations.join("; ")
        );
    }
    Ok(())
}

/// `true` if `table`'s live DDL already declares the status `CHECK`.
/// Whitespace-insensitive so a `CHECK (\n status IN ...)` written across
/// lines still matches.
fn table_has_status_check(conn: &Connection, table: &str) -> Result<bool> {
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(sql
        .map(|sql| {
            let collapsed = sql.split_whitespace().collect::<Vec<_>>().join(" ");
            collapsed.contains("CHECK ( status IN") || collapsed.contains("CHECK (status IN")
        })
        .unwrap_or(false))
}

fn table_status_check_includes(conn: &Connection, table: &str, value: &str) -> Result<bool> {
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(sql.is_some_and(|sql| sql.contains(&format!("'{value}'"))))
}

/// The column names `table` actually has right now, in schema order.
fn live_column_names(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(names)
}
