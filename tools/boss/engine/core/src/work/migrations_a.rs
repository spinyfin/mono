use super::*;

pub(crate) fn migrate_work_executions_v3(conn: &Connection) -> Result<()> {
    for (column, ddl) in [
        (
            "cube_workspace_id",
            "ALTER TABLE work_executions ADD COLUMN cube_workspace_id TEXT",
        ),
        (
            "priority",
            "ALTER TABLE work_executions ADD COLUMN priority INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "preferred_workspace_id",
            "ALTER TABLE work_executions ADD COLUMN preferred_workspace_id TEXT",
        ),
    ] {
        if !work_executions_has_column(conn, column)? {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}

pub(crate) fn migrate_work_executions_pre_start_retry(conn: &Connection) -> Result<()> {
    for (column, ddl) in [
        (
            "pre_start_failure_count",
            "ALTER TABLE work_executions ADD COLUMN pre_start_failure_count INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "dispatch_not_before",
            "ALTER TABLE work_executions ADD COLUMN dispatch_not_before TEXT",
        ),
    ] {
        if !work_executions_has_column(conn, column)? {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}

pub(crate) fn migrate_work_executions_pr_url(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "pr_url")? {
        conn.execute("ALTER TABLE work_executions ADD COLUMN pr_url TEXT", [])?;
    }
    Ok(())
}

/// `pr_head_before`: the head SHA of the chore's bound PR captured
/// at the moment this execution started running. The Stop boundary's
/// SHA-delta gate uses it to decide whether a resume run actually
/// contributed to the bound PR before falling through to the
/// `PROBE_NO_PR` nudge — see the resume-bounce nudge-loop fix.
/// Idempotent.
pub(crate) fn migrate_work_executions_pr_head_before(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "pr_head_before")? {
        conn.execute("ALTER TABLE work_executions ADD COLUMN pr_head_before TEXT", [])?;
    }
    Ok(())
}

/// `pr_head_after`: the PR head SHA read directly from GitHub at the
/// PR-completion terminalization seam, before the worker pane is torn down.
/// This preserves the head that the engine acted on even if a later force-push
/// makes it impossible to reconstruct from the live PR. Idempotent.
pub(crate) fn migrate_work_executions_pr_head_after(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "pr_head_after")? {
        conn.execute("ALTER TABLE work_executions ADD COLUMN pr_head_after TEXT", [])?;
    }
    Ok(())
}

/// `pr_body_before` + `metadata_fix_confirmed_at`: positive-evidence
/// columns for the metadata-only CI-fix finalize gate (issue #1252).
///
/// `pr_body_before` holds the bound PR's description/body captured at
/// the moment this execution started running (the baseline against
/// which `on_stop` detects an operator-visible PR-metadata delta).
/// `metadata_fix_confirmed_at` is a timestamp the on-Stop handler
/// stamps once it observes — at a *real* Stop boundary — that this
/// revision produced such a delta. It is the load-bearing signal that
/// lets the merge poller finalize a PR-description-only CI fix when CI
/// goes green *after* the worker stopped, WITHOUT the #1262 regression
/// of finalizing a dead/cut-off worker that never reached a clean Stop
/// and contributed nothing. Both NULL on pre-migration rows and on the
/// new-PR flow (no bound PR to snapshot). Idempotent.
pub(crate) fn migrate_work_executions_metadata_fix_columns(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "pr_body_before")? {
        conn.execute("ALTER TABLE work_executions ADD COLUMN pr_body_before TEXT", [])?;
    }
    if !work_executions_has_column(conn, "metadata_fix_confirmed_at")? {
        conn.execute(
            "ALTER TABLE work_executions ADD COLUMN metadata_fix_confirmed_at TEXT",
            [],
        )?;
    }
    Ok(())
}

/// `run_done_declared_at` / `run_done_outcome` / `run_undeclared_at`: the
/// durable record of whether this run's worker ever declared itself finished,
/// and — when it did not and the engine ended the run anyway — that the ending
/// was the backstop's, not the worker's.
///
/// `run_done_declared_at` + `run_done_outcome` are stamped by the
/// `run_done` proposal applier
/// ([`crate::work::proposal_apply`]) at submission time, so "the worker
/// declared" is a durable fact on the execution row rather than something
/// re-derived by scanning the proposal ledger on every read. `run_undeclared_at`
/// is stamped by the backstop
/// ([`crate::run_done_backstop`]) when it gives up waiting for a declaration
/// that never came. The three columns together are what makes a
/// declared completion distinguishable from a backstopped one in stored
/// state: a declared run has the first two set and the third NULL, a
/// backstopped run the reverse. All NULL on pre-migration rows and on any
/// run still in flight. Idempotent.
pub(crate) fn migrate_work_executions_run_done_columns(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "run_done_declared_at")? {
        conn.execute("ALTER TABLE work_executions ADD COLUMN run_done_declared_at TEXT", [])?;
    }
    if !work_executions_has_column(conn, "run_done_outcome")? {
        conn.execute("ALTER TABLE work_executions ADD COLUMN run_done_outcome TEXT", [])?;
    }
    if !work_executions_has_column(conn, "run_undeclared_at")? {
        conn.execute("ALTER TABLE work_executions ADD COLUMN run_undeclared_at TEXT", [])?;
    }
    Ok(())
}

/// `pr_title_before`: the bound PR's title captured at the same moment as
/// `pr_body_before` (execution start). Backs `boss pr body`, which returns
/// both title and body so a worker doing read-modify-write on the
/// description doesn't need a separate `gh pr view` for the title.
/// Idempotent.
pub(crate) fn migrate_work_executions_pr_title_before(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "pr_title_before")? {
        conn.execute("ALTER TABLE work_executions ADD COLUMN pr_title_before TEXT", [])?;
    }
    Ok(())
}

/// `dispatch_wait_reason` + `dispatch_wait_since`: the dispatcher's
/// current defer reason (`chain_serialized`, `pool_exhausted`, ...) for a
/// `ready` execution that hasn't claimed a worker slot yet, and when that
/// reason first applied. Distinct from `dispatch_failed_reason` on
/// `tasks` (a terminal give-up) — this is a live, in-progress wait. Set by
/// the two `WorkerClaimed`/`Skipped` sites in `coordinator::drain_ready`
/// (mirroring the reason already logged to `dispatch_events`), and cleared
/// the moment the execution claims a slot. `dispatch_wait_since` is only
/// stamped when the reason changes (or was previously unset) so it
/// reflects the start of the *current* wait, not the most recent poll.
/// Lets the kanban card render the real cause instead of a generic
/// "Waiting for a slot" (see T251 incident: chain_serialized read as slot
/// exhaustion for ~20 minutes with 8+ free slots). Both `NULL` for the
/// overwhelming majority of rows (anything not currently deferred).
/// Idempotent.
pub(crate) fn migrate_work_executions_dispatch_wait(conn: &Connection) -> Result<()> {
    for (column, ddl) in [
        (
            "dispatch_wait_reason",
            "ALTER TABLE work_executions ADD COLUMN dispatch_wait_reason TEXT",
        ),
        (
            "dispatch_wait_since",
            "ALTER TABLE work_executions ADD COLUMN dispatch_wait_since TEXT",
        ),
    ] {
        if !work_executions_has_column(conn, column)? {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}

/// Add `tasks.parent_task_id` — the soft FK that ties a `revision` task
/// to the task whose PR it targets — and the accompanying index so the
/// coordinator can walk the chain efficiently. Mirrors the
/// `migrate_tasks_investigation_doc_columns` pattern: `table_has_column`
/// guard makes this idempotent across re-opens. No CHECK constraint; the
/// "kind = revision ⇒ parent_task_id IS NOT NULL" invariant is enforced
/// in `insert_revision_in_tx` (Phase 2). Existing non-revision rows default
/// to `NULL` with no backfill — that is the correct value for them.
pub(crate) fn migrate_tasks_parent_task_id_column(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "tasks", "parent_task_id")? {
        conn.execute("ALTER TABLE tasks ADD COLUMN parent_task_id TEXT", [])?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_tasks_parent_task_id
             ON tasks(parent_task_id);",
    )?;
    Ok(())
}

/// Flatten nested revision parentage so every revision's `parent_task_id`
/// points at the chain root (the original non-revision work item).
///
/// Historically, `create_revision` stored the caller's parent selector
/// verbatim — so a revision filed against another revision (manual
/// `create-revision --parent <rev>`, PR-review / CI / conflict producers
/// against a revision work item) nested under that revision. The UI and
/// `list_revisions --parent <root>` only surface direct children of the
/// root, which hid nested revisions from the chain.
///
/// New inserts already canonicalize via [`assert_parent_revisable_and_insert`].
/// This migration repairs existing nested rows in place: walks each
/// revision whose parent is itself a revision up to the non-revision
/// chain root and rewrites `parent_task_id`. Soft-deleted rows are
/// included so a later restore cannot reintroduce nesting. Status,
/// executions, PR association, dependency edges, and sequence order
/// (creation-order R<n>) are untouched. Idempotent: a second pass finds
/// no nested parents and is a no-op.
pub(crate) fn migrate_flatten_nested_revision_parents(conn: &Connection) -> Result<()> {
    // Collect candidates first so we don't hold a query open while
    // rewriting (and so a long chain of nested rows is repaired in one
    // pass even when an intermediate rewrite changes the join set).
    let mut stmt = conn.prepare(
        "SELECT child.id
         FROM tasks child
         JOIN tasks parent ON parent.id = child.parent_task_id
         WHERE child.kind = 'revision'
           AND parent.kind = 'revision'
           AND child.parent_task_id IS NOT NULL",
    )?;
    let nested_ids: Vec<String> = stmt.query_map([], |row| row.get(0))?.filter_map(|r| r.ok()).collect();
    drop(stmt);

    for rev_id in nested_ids {
        let root_id = chain_root(conn, &rev_id)?;
        if root_id == rev_id {
            // Corrupt cycle / broken parent: leave the row alone rather
            // than inventing a parent. chain_root's cycle guard already
            // returned a reachable id; if that id is the revision itself
            // we have nowhere safe to reparent.
            continue;
        }
        // Skip if the resolved root is still a revision (orphaned nested
        // chain with no non-revision ancestor) — same leave-alone policy.
        let root_kind: Option<String> = conn
            .query_row("SELECT kind FROM tasks WHERE id = ?1", params![root_id], |row| {
                row.get(0)
            })
            .optional()?;
        if root_kind.as_deref() != Some("revision") {
            conn.execute(
                "UPDATE tasks SET parent_task_id = ?2 WHERE id = ?1 AND parent_task_id != ?2",
                params![rev_id, root_id],
            )?;
        }
    }
    Ok(())
}

/// Backfill revision `name` to first-line-of-description for existing rows.
///
/// The original `insert_revision_in_tx` stored the full description in both
/// `name` and `description` (see revision-tasks.md implementation). This
/// caused the macOS kanban card to display the entire multi-paragraph
/// description verbatim. The corrected insert now uses `revision_name_from_description`
/// (first non-empty line, ≤120 chars) as `name`; this migration aligns
/// pre-fix rows that still carry `name = description`.
///
/// SQLite's INSTR + SUBSTR extract the first `\n`-terminated segment.
/// Rows where `name` already differs from `description` (e.g. manually
/// patched) are left as-is. Idempotent.
pub(crate) fn migrate_revision_names_to_first_line(conn: &Connection) -> Result<()> {
    // Pull all revision task IDs + descriptions where name = description.
    // We do the first-line extraction in Rust (not raw SQL) because
    // SQLite's string functions cannot reliably handle Unicode ellipsis
    // or word-boundary truncation.
    struct Row {
        id: String,
        description: String,
    }
    let mut stmt = conn.prepare(
        "SELECT id, description FROM tasks
         WHERE kind = 'revision' AND name = description AND deleted_at IS NULL",
    )?;
    let rows: Vec<Row> = stmt
        .query_map([], |row| {
            Ok(Row {
                id: row.get(0)?,
                description: row.get(1)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    for row in rows {
        let name = revision_name_from_description(&row.description);
        if name != row.description {
            conn.execute(
                "UPDATE tasks SET name = ?1 WHERE id = ?2",
                rusqlite::params![name, row.id],
            )?;
        }
    }
    Ok(())
}

/// Add `work_executions.prefer_is_soft` — a boolean signal (stored as
/// INTEGER 0/1 per SQLite convention) that tells the coordinator's
/// `lease_workspace_with_fallback` to treat `preferred_workspace_id` as a
/// warmth hint rather than a hard requirement. Set `true` (1) for
/// `revision_implementation` executions; defaults to `false` (0) for all
/// existing rows, preserving the hard-prefer semantics used by orphan-resume.
/// See design § OQ5 and `revision-tasks.md`.
pub(crate) fn migrate_work_executions_prefer_is_soft(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "prefer_is_soft")? {
        conn.execute(
            "ALTER TABLE work_executions ADD COLUMN prefer_is_soft INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

/// `transient_failure_count`: how many times the engine has auto-resumed
/// this work item's execution chain because a worker stalled or died on
/// a transient Claude API error. Carried forward onto each fresh resume
/// execution by [`WorkDb::request_resume_execution`] so the bounded-retry
/// policy in [`crate::transient_recovery`] can cap retries and back off.
/// Idempotent; existing rows default to 0.
pub(crate) fn migrate_work_executions_transient_failure_count(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "transient_failure_count")? {
        conn.execute(
            "ALTER TABLE work_executions ADD COLUMN transient_failure_count INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

/// `allow_dirty`: boolean signal (stored as INTEGER 0/1) that tells the
/// coordinator's `lease_workspace_with_fallback` to include `--allow-dirty`
/// in the cube lease invocation, reclaiming the preferred workspace with its
/// uncommitted working copy intact. Set only on the orphan recovery
/// re-dispatch path. Defaults to 0 for all existing rows.
pub(crate) fn migrate_work_executions_allow_dirty(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "allow_dirty")? {
        conn.execute(
            "ALTER TABLE work_executions ADD COLUMN allow_dirty INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

/// Canonicalize all timestamp columns to Unix epoch seconds (decimal
/// string). Older rows in some databases hold ISO 8601 strings (e.g.
/// `2026-05-07T18:55:45.000Z`) from a pre-canonical write path; this
/// rewrites them in-place so consumers — `boss chore list --json`,
/// the macOS app's Done-lane bucketing, and any future SQL ordering —
/// see one shape. Idempotent: rows already in epoch form are skipped
/// by the LIKE filter.
pub(crate) fn migrate_timestamps_to_epoch(conn: &Connection) -> Result<()> {
    const TIMESTAMP_COLUMNS: &[(&str, &str)] = &[
        ("products", "created_at"),
        ("products", "updated_at"),
        ("projects", "created_at"),
        ("projects", "updated_at"),
        ("tasks", "created_at"),
        ("tasks", "updated_at"),
        ("tasks", "deleted_at"),
        ("work_executions", "created_at"),
        ("work_executions", "started_at"),
        ("work_executions", "finished_at"),
        ("work_runs", "created_at"),
        ("work_runs", "started_at"),
        ("work_runs", "finished_at"),
        ("work_attention_items", "created_at"),
        ("work_attention_items", "resolved_at"),
        ("pane_summaries", "created_at"),
    ];
    for (table, column) in TIMESTAMP_COLUMNS {
        // SQLite LIKE: `_` matches any single character, so this picks
        // up `YYYY-MM-DD`-prefixed values without parsing every row.
        let select_sql = format!(
            "SELECT rowid, {column} FROM {table} \
             WHERE {column} LIKE '____-__-__T%' OR {column} LIKE '____-__-__ %'"
        );
        let mut stmt = conn.prepare(&select_sql)?;
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for (rowid, value) in rows {
            if let Some(epoch) = boss_engine_utils::iso8601::parse_iso8601_to_epoch(&value) {
                let update_sql = format!("UPDATE {table} SET {column} = ?1 WHERE rowid = ?2");
                conn.execute(&update_sql, params![epoch.to_string(), rowid])?;
            }
        }
    }
    Ok(())
}

/// `stop_seen`: boolean signal (INTEGER 0/1) stamped by `on_stop_inner` the
/// first time a Stop event fires for an execution. The SHA-delta gate in
/// `recheck_for_pr` uses this as a guard for `revision_implementation`
/// executions: it only fires the gate after a Stop has been observed, ensuring
/// that the gate acts as a recovery path (Stop fired but PR detection failed
/// transiently) rather than a primary detector. Without this guard, a commit
/// pushed to the parent PR by *another* worker between the snapshot time and
/// the merge-poller check fires `Contributed`, transitioning the revision to
/// `in_review` before the revision worker has done any work (T1503/T1496
/// regression).
pub(crate) fn migrate_work_executions_stop_seen(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "stop_seen")? {
        conn.execute(
            "ALTER TABLE work_executions ADD COLUMN stop_seen INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

/// `revision_stop_contributed_head`: the PR head SHA that `on_stop_inner`'s
/// SHA-delta `Contributed` arm observed when it last attempted to finalize a
/// `revision_implementation` execution. Set just before the finalize attempt
/// so that `recheck_for_pr` can complete the transition when the first attempt
/// failed transiently (T848 recovery). `NULL` means `on_stop_inner` has never
/// seen a `Contributed` outcome for this execution, which tells `recheck_for_pr`
/// the head movement was from a *different* worker (e.g. the parent chore's
/// still-active worker pushing to the shared PR branch).
pub(crate) fn migrate_work_executions_revision_stop_contributed_head(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "revision_stop_contributed_head")? {
        conn.execute(
            "ALTER TABLE work_executions \
             ADD COLUMN revision_stop_contributed_head TEXT",
            [],
        )?;
    }
    Ok(())
}

/// `driver_runtime_state`: opaque JSON blob returned by
/// [`boss_engine_driver::AgentDriver::provision_workspace`] and later
/// handed to [`boss_engine_driver::AgentDriver::teardown_workspace`].
/// Survives engine restart, orphan recovery, and workspace release —
/// deliberately *not* cleared when `workspace_path` is nulled, so a
/// future Codex retention sweep can still find the recorded
/// Boss-owned root without scanning a shared provider home. Claude
/// returns no state, so most rows stay NULL. Idempotent.
pub(crate) fn migrate_work_executions_driver_runtime_state(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "driver_runtime_state")? {
        conn.execute("ALTER TABLE work_executions ADD COLUMN driver_runtime_state TEXT", [])?;
    }
    Ok(())
}

/// `pr_head_baseline_absorbed`: set (never cleared) the first time
/// `on_stop_inner`'s parent-push suppression path rewrites `pr_head_before`
/// mid-run — a head movement it attributed to the concurrently-active
/// parent worker rather than to this revision's own push. Once set, a
/// later Stop's "head unchanged" SHA-delta finding compares against that
/// rewritten baseline, not against the head at dispatch time, so it can no
/// longer be trusted as positive proof this run contributed nothing (see
/// `ContributionEvidence::ProvenAbsent` in `completion.rs`). Idempotent.
pub(crate) fn migrate_work_executions_pr_head_baseline_absorbed(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "pr_head_baseline_absorbed")? {
        conn.execute(
            "ALTER TABLE work_executions ADD COLUMN pr_head_baseline_absorbed INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

/// Engine-owned, bounded provider-session identity for the latest worker run.
///
/// This deliberately lives in SQLite rather than the provider's writable
/// home. It survives an engine restart while the run is active and is cleared
/// by driver teardown when the run terminates.
pub(crate) fn migrate_work_runs_progress_session_id(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "work_runs", "progress_session_id")? {
        conn.execute("ALTER TABLE work_runs ADD COLUMN progress_session_id TEXT", [])?;
    }
    Ok(())
}

/// ISO-8601 timestamp of the most recent turn boundary this **run's** driver
/// delivered ([`boss_engine_driver::AgentDriver::turn_boundary`]), i.e. the
/// durable record that the worker produced a terminal result for the process
/// currently attached to the run.
///
/// Lives on `work_runs` rather than `work_executions` precisely so it is
/// scoped to one spawned process: a resumed execution gets a fresh run row and
/// therefore a fresh (NULL) boundary, so a prior process's turn can never
/// vouch for a later process that crashed before delivering one.
///
/// Historically read to tell a one-turn-per-process driver's expected
/// end-of-life apart from a death; no registered driver declares that
/// lifetime any more and the classifier that read this column was removed
/// along with it (see `docs/investigations/codex-tui-pivot-pricing-2026-07-30.md`).
pub(crate) fn migrate_work_runs_turn_boundary_at(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "work_runs", "turn_boundary_at")? {
        conn.execute("ALTER TABLE work_runs ADD COLUMN turn_boundary_at TEXT", [])?;
    }
    Ok(())
}

/// The run's file-progress ingress resume point, as JSON — see
/// [`crate::agent_jsonl_progress::IngressCheckpoint`].
///
/// Engine-owned and durable for exactly the reason `progress_session_id`
/// above is: an engine that restarts while a long-lived agent session is
/// still running has to pick that session's rollout back up where it left
/// off. In-memory state cannot answer "how far did the last engine get?",
/// and the two wrong answers — offset 0 and end-of-file — replay every prior
/// record or discard the ones that arrived during the restart.
///
/// On `work_runs` rather than `work_executions` for the same scoping reason
/// as `turn_boundary_at`: the resume point describes one spawned process's
/// rollout file, so a later run must never inherit an earlier one's offset.
pub(crate) fn migrate_work_runs_progress_ingress_checkpoint(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "work_runs", "progress_ingress_checkpoint")? {
        conn.execute("ALTER TABLE work_runs ADD COLUMN progress_ingress_checkpoint TEXT", [])?;
    }
    Ok(())
}

/// Last driver-originated progress time and tri-state tool condition for one
/// spawned run.
///
/// Additive and nullable so legacy rows stay non-destructive until a real
/// driver event establishes their state. Distinct from the live-state
/// `last_event_at` display field, which is also written by engine inference:
/// persisting that container would let a synthesized timestamp masquerade as
/// agent progress after restart.
pub(crate) fn migrate_work_runs_semantic_progress(conn: &Connection) -> Result<()> {
    for column in ["semantic_progress_at", "semantic_tool_condition"] {
        if !table_has_column(conn, "work_runs", column)? {
            conn.execute(&format!("ALTER TABLE work_runs ADD COLUMN {column} TEXT"), [])?;
        }
    }
    Ok(())
}

/// Tmux session identity for a single spawned worker run.
///
/// All fields stay nullable so legacy and in-flight app-hosted workers retain
/// their existing reconciliation path. `tmux_spawn_token` is the opaque,
/// exact-match adoption key; the partial unique index permits legacy NULL
/// rows while rejecting two runs claiming the same live session.
pub(crate) fn migrate_work_runs_tmux_columns(conn: &Connection) -> Result<()> {
    for (column, sql_type) in [
        ("tmux_server_label", "TEXT"),
        ("tmux_session_name", "TEXT"),
        ("tmux_spawn_token", "TEXT"),
        ("tmux_spawn_state", "TEXT"),
        ("tmux_pane_pid", "INTEGER"),
    ] {
        if !table_has_column(conn, "work_runs", column)? {
            conn.execute(&format!("ALTER TABLE work_runs ADD COLUMN {column} {sql_type}"), [])?;
        }
    }
    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS work_runs_tmux_spawn_token_idx
         ON work_runs(tmux_spawn_token)
         WHERE tmux_spawn_token IS NOT NULL",
        [],
    )?;
    Ok(())
}

/// Raw per-run agent usage captured incrementally from transcript records.
///
/// Cache-write tokens keep both the provider's total and its 5-minute /
/// 1-hour breakdown. The total preserves usage when an older provider record
/// lacks the split; nullable split columns keep that absence explicit instead
/// of silently assigning the write to the wrong price.
pub(crate) fn migrate_work_runs_cost_columns(conn: &Connection) -> Result<()> {
    for (column, sql_type) in [
        ("model", "TEXT"),
        ("output_tokens", "INTEGER"),
        ("input_tokens", "INTEGER"),
        ("cache_creation_tokens", "INTEGER"),
        ("cache_read_tokens", "INTEGER"),
        ("cache_creation_5m_tokens", "INTEGER"),
        ("cache_creation_1h_tokens", "INTEGER"),
        ("rounds", "INTEGER"),
        ("agent_active_ms", "INTEGER"),
    ] {
        if !table_has_column(conn, "work_runs", column)? {
            conn.execute(&format!("ALTER TABLE work_runs ADD COLUMN {column} {sql_type}"), [])?;
        }
    }
    Ok(())
}

pub(crate) fn work_executions_has_column(conn: &Connection, column: &str) -> Result<bool> {
    table_has_column(conn, "work_executions", column)
}

pub(crate) fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}
