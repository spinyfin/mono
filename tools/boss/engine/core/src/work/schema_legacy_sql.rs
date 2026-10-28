// Historical SQL kept unchanged for legacy upgrades and their fixtures.
/// Schema 43: the baseline commit a recovery record was created at, the
/// pointer-reconciliation note for the worker prompt, and the retirement
/// marker that stops a record being a recovery source.
pub(super) const EXECUTION_BOOKMARK_COLUMNS_43: &str = "ALTER TABLE execution_bookmarks ADD COLUMN base_commit TEXT;
     ALTER TABLE execution_bookmarks ADD COLUMN reconciliation_note TEXT;
     ALTER TABLE execution_bookmarks ADD COLUMN retired_at TEXT;
     ALTER TABLE execution_bookmarks ADD COLUMN retired_reason TEXT";

/// Schema 60: when the bookmark prune sweep last examined a recovery record
/// (Unix epoch seconds) and what it concluded (`published`, `already_gone`,
/// `unproven`, `pr_open`, `error`), so a record that could not be proven
/// published is re-examined on a schedule instead of on every pass.
pub(super) const EXECUTION_BOOKMARK_PRUNE_COLUMNS_60: &str =
    "ALTER TABLE execution_bookmarks ADD COLUMN prune_checked_at INTEGER;
     ALTER TABLE execution_bookmarks ADD COLUMN prune_outcome TEXT";

/// Schema 45: GitHub's `updatedAt` for the task's PR, written by the merge
/// poller. The Review column orders its Pending section by it.
pub(super) const TASKS_PR_UPDATED_AT_45: &str = "ALTER TABLE tasks ADD COLUMN pr_updated_at TEXT";

/// Schema 46: provenance of a raise to the human-only `critical`
/// priority — the resolved caller tier and the timestamp. Both NULL
/// unless `priority = 'critical'`; see `work/critical_priority.rs`.
pub(super) const TASKS_CRITICAL_PROVENANCE_46: &str = "ALTER TABLE tasks ADD COLUMN critical_set_by TEXT;
     ALTER TABLE tasks ADD COLUMN critical_set_at TEXT";

/// Schema 50: the durable turn condition of a run's worker
/// (`in_turn` / `ended` / `unknown`), written beside `semantic_tool_condition`
/// by the same driver events. Re-adoption after an engine restart seeds the
/// slot's activity from it, so a worker that was mid-turn — including one
/// waiting on its own background command, whose tool condition reads `idle`
/// — comes back `Working` rather than `Idle`. NULL on every pre-existing row,
/// which parses as `unknown`; see `semantic_progress::SemanticTurnCondition`.
pub(super) const WORK_RUNS_SEMANTIC_TURN_CONDITION_50: &str =
    "ALTER TABLE work_runs ADD COLUMN semantic_turn_condition TEXT";

/// Serves the shared-PR probes (`o.pr_url = t.pr_url`, `pr_url = ?`) so they
/// are index lookups instead of full `tasks` scans.
pub(super) const TASKS_PR_URL_INDEX_SQL: &str =
    "CREATE INDEX IF NOT EXISTS tasks_pr_url_idx ON tasks(pr_url) WHERE pr_url IS NOT NULL";

pub(super) const IN_FLIGHT_WORKSPACES_INDEX_SQL: &str = "CREATE INDEX IF NOT EXISTS work_executions_in_flight_idx
     ON work_executions(created_at, id, repo_remote_url, workspace_path)
     WHERE status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
       AND cube_lease_id IS NOT NULL";

/// Lets attention reconcile prove a kind has no open rows without scanning
/// the resolved history.
pub(super) const ATTENTION_OPEN_KIND_INDEX_SQL: &str =
    "CREATE INDEX IF NOT EXISTS work_attention_items_open_kind_idx ON work_attention_items(kind) WHERE status = 'open'";

/// Schema 49: per-item version counters for the board runtime cache.
///
/// `board_runtime_epoch.epoch` increases whenever an input of that item's
/// `TaskRuntime` is written (see `dispatch_helpers::compute_task_runtime` for
/// the input list), so a cached runtime is reusable exactly while its epoch is
/// unchanged. The bump happens in the writer's own transaction, which makes it
/// visible to a reader in the same snapshot as the data, and it fires for any
/// connection or process that writes the database.
///
/// Rows are never deleted: epochs only grow, so a recreated id can never alias
/// an older cached value, and the table holds at most one small row per item
/// that ever had an execution. `work_executions` bumps on any column update
/// (over-invalidating is harmless and survives new runtime fields); `work_runs`
/// bumps on the columns `query_latest_run` reads.
pub(super) const BOARD_RUNTIME_EPOCH_49: &str = "CREATE TABLE IF NOT EXISTS board_runtime_epoch (
         work_item_id TEXT PRIMARY KEY NOT NULL,
         epoch INTEGER NOT NULL
     ) WITHOUT ROWID;
     CREATE TRIGGER IF NOT EXISTS board_runtime_epoch_execution_insert
     AFTER INSERT ON work_executions
     BEGIN
         INSERT INTO board_runtime_epoch(work_item_id, epoch) VALUES (NEW.work_item_id, 1)
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END;
     CREATE TRIGGER IF NOT EXISTS board_runtime_epoch_execution_update
     AFTER UPDATE ON work_executions
     BEGIN
         INSERT INTO board_runtime_epoch(work_item_id, epoch) VALUES (NEW.work_item_id, 1)
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END;
     CREATE TRIGGER IF NOT EXISTS board_runtime_epoch_execution_reassigned
     AFTER UPDATE OF work_item_id ON work_executions
     WHEN OLD.work_item_id IS NOT NEW.work_item_id
     BEGIN
         INSERT INTO board_runtime_epoch(work_item_id, epoch) VALUES (OLD.work_item_id, 1)
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END;
     CREATE TRIGGER IF NOT EXISTS board_runtime_epoch_execution_delete
     AFTER DELETE ON work_executions
     BEGIN
         INSERT INTO board_runtime_epoch(work_item_id, epoch) VALUES (OLD.work_item_id, 1)
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END;
     CREATE TRIGGER IF NOT EXISTS board_runtime_epoch_run_insert
     AFTER INSERT ON work_runs
     BEGIN
         INSERT INTO board_runtime_epoch(work_item_id, epoch)
         SELECT work_item_id, 1 FROM work_executions WHERE id = NEW.execution_id
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END;
     CREATE TRIGGER IF NOT EXISTS board_runtime_epoch_run_update
     AFTER UPDATE OF status, execution_id, created_at ON work_runs
     BEGIN
         INSERT INTO board_runtime_epoch(work_item_id, epoch)
         SELECT work_item_id, 1 FROM work_executions WHERE id = NEW.execution_id
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_runtime_epoch(work_item_id, epoch)
         SELECT work_item_id, 1 FROM work_executions
         WHERE id = OLD.execution_id AND OLD.execution_id IS NOT NEW.execution_id
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END;
     CREATE TRIGGER IF NOT EXISTS board_runtime_epoch_run_delete
     AFTER DELETE ON work_runs
     BEGIN
         INSERT INTO board_runtime_epoch(work_item_id, epoch)
         SELECT work_item_id, 1 FROM work_executions WHERE id = OLD.execution_id
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END";

/// Schema 53: per-item version counters for the board review-state cache.
///
/// `board_review_epoch.epoch` increases whenever an input of a card's
/// AI-review projection (`ai_review_state`, `ai_review_findings_revision_id`,
/// `ai_review_badge`; see `review_badge::fetch_current_heads` and
/// `review_badge::fetch_badge_histories` for the exact column list) is written.
/// Those projections are derived per PR owner (chain root) from the
/// root's row, every revision row under it, the family's
/// `pr_review_verdicts`, the findings revision's `work_executions`,
/// and the applied `worker_proposals` payload. Each trigger therefore
/// bumps the written row's work item *and* that item's `parent_task_id`
/// (a revision's chain root, since `insert_revision_in_tx` always
/// parents a revision directly to the root). A root epoch alone does
/// not cover descendants of off-board revisions in legacy chains. The
/// reader therefore keys each family by member IDs and epochs from
/// the full SQL family, including off-board ancestors in nested
/// legacy chains.
///
/// Same discipline as `board_runtime_epoch`: the bump is in the writer's own
/// transaction, rows are never deleted, and `UPDATE OF` lists only the columns
/// the projection reads, so unrelated writes (poll timestamps, runtime
/// bookkeeping) leave the cache warm.
pub(super) const BOARD_REVIEW_EPOCH_53: &str = "CREATE TABLE IF NOT EXISTS board_review_epoch (
         work_item_id TEXT PRIMARY KEY NOT NULL,
         epoch INTEGER NOT NULL
     ) WITHOUT ROWID;
     CREATE TRIGGER IF NOT EXISTS board_review_epoch_task_insert
     AFTER INSERT ON tasks
     BEGIN
         INSERT INTO board_review_epoch(work_item_id, epoch) VALUES (NEW.id, 1)
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT NEW.parent_task_id, 1 WHERE NEW.parent_task_id IS NOT NULL
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END;
     CREATE TRIGGER IF NOT EXISTS board_review_epoch_task_update
     AFTER UPDATE OF kind, status, deleted_at, parent_task_id, pr_url, pr_head_sha,
                     ci_required_state, pr_mergeable_state, last_reviewed_sha, name, description
     ON tasks
     BEGIN
         INSERT INTO board_review_epoch(work_item_id, epoch) VALUES (NEW.id, 1)
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT NEW.parent_task_id, 1 WHERE NEW.parent_task_id IS NOT NULL
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT OLD.parent_task_id, 1
         WHERE OLD.parent_task_id IS NOT NULL AND OLD.parent_task_id IS NOT NEW.parent_task_id
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END;
     CREATE TRIGGER IF NOT EXISTS board_review_epoch_task_delete
     AFTER DELETE ON tasks
     BEGIN
         INSERT INTO board_review_epoch(work_item_id, epoch) VALUES (OLD.id, 1)
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT OLD.parent_task_id, 1 WHERE OLD.parent_task_id IS NOT NULL
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END;
     CREATE TRIGGER IF NOT EXISTS board_review_epoch_verdict_insert
     AFTER INSERT ON pr_review_verdicts
     BEGIN
         INSERT INTO board_review_epoch(work_item_id, epoch) VALUES (NEW.work_item_id, 1)
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT parent_task_id, 1 FROM tasks WHERE id = NEW.work_item_id AND parent_task_id IS NOT NULL
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END;
     CREATE TRIGGER IF NOT EXISTS board_review_epoch_verdict_update
     AFTER UPDATE ON pr_review_verdicts
     BEGIN
         INSERT INTO board_review_epoch(work_item_id, epoch) VALUES (NEW.work_item_id, 1)
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT parent_task_id, 1 FROM tasks WHERE id = NEW.work_item_id AND parent_task_id IS NOT NULL
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT OLD.work_item_id, 1 WHERE OLD.work_item_id IS NOT NEW.work_item_id
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT parent_task_id, 1 FROM tasks
         WHERE id = OLD.work_item_id AND parent_task_id IS NOT NULL AND OLD.work_item_id IS NOT NEW.work_item_id
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END;
     CREATE TRIGGER IF NOT EXISTS board_review_epoch_verdict_delete
     AFTER DELETE ON pr_review_verdicts
     BEGIN
         INSERT INTO board_review_epoch(work_item_id, epoch) VALUES (OLD.work_item_id, 1)
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT parent_task_id, 1 FROM tasks WHERE id = OLD.work_item_id AND parent_task_id IS NOT NULL
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END;
     CREATE TRIGGER IF NOT EXISTS board_review_epoch_execution_insert
     AFTER INSERT ON work_executions
     BEGIN
         INSERT INTO board_review_epoch(work_item_id, epoch) VALUES (NEW.work_item_id, 1)
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT parent_task_id, 1 FROM tasks WHERE id = NEW.work_item_id AND parent_task_id IS NOT NULL
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END;
     CREATE TRIGGER IF NOT EXISTS board_review_epoch_execution_update
     AFTER UPDATE OF work_item_id, kind, status, finished_at, pr_head_after ON work_executions
     BEGIN
         INSERT INTO board_review_epoch(work_item_id, epoch) VALUES (NEW.work_item_id, 1)
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT parent_task_id, 1 FROM tasks WHERE id = NEW.work_item_id AND parent_task_id IS NOT NULL
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT OLD.work_item_id, 1 WHERE OLD.work_item_id IS NOT NEW.work_item_id
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT parent_task_id, 1 FROM tasks
         WHERE id = OLD.work_item_id AND parent_task_id IS NOT NULL AND OLD.work_item_id IS NOT NEW.work_item_id
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END;
     CREATE TRIGGER IF NOT EXISTS board_review_epoch_execution_delete
     AFTER DELETE ON work_executions
     BEGIN
         INSERT INTO board_review_epoch(work_item_id, epoch) VALUES (OLD.work_item_id, 1)
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT parent_task_id, 1 FROM tasks WHERE id = OLD.work_item_id AND parent_task_id IS NOT NULL
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END;
     CREATE TRIGGER IF NOT EXISTS board_review_epoch_proposal_insert
     AFTER INSERT ON worker_proposals
     BEGIN
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT NEW.work_item_id, 1 WHERE NEW.work_item_id IS NOT NULL
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT parent_task_id, 1 FROM tasks WHERE id = NEW.work_item_id AND parent_task_id IS NOT NULL
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END;
     CREATE TRIGGER IF NOT EXISTS board_review_epoch_proposal_update
     AFTER UPDATE OF work_item_id, kind, state, payload_json ON worker_proposals
     BEGIN
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT NEW.work_item_id, 1 WHERE NEW.work_item_id IS NOT NULL
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT parent_task_id, 1 FROM tasks WHERE id = NEW.work_item_id AND parent_task_id IS NOT NULL
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT OLD.work_item_id, 1
         WHERE OLD.work_item_id IS NOT NULL AND OLD.work_item_id IS NOT NEW.work_item_id
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT v.work_item_id, 1 FROM pr_review_verdicts v WHERE v.proposal_id = NEW.id
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT t.parent_task_id, 1 FROM pr_review_verdicts v JOIN tasks t ON t.id = v.work_item_id
         WHERE v.proposal_id = NEW.id AND t.parent_task_id IS NOT NULL
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END;
     CREATE TRIGGER IF NOT EXISTS board_review_epoch_proposal_delete
     AFTER DELETE ON worker_proposals
     BEGIN
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT OLD.work_item_id, 1 WHERE OLD.work_item_id IS NOT NULL
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT parent_task_id, 1 FROM tasks WHERE id = OLD.work_item_id AND parent_task_id IS NOT NULL
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT v.work_item_id, 1 FROM pr_review_verdicts v WHERE v.proposal_id = OLD.id
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         INSERT INTO board_review_epoch(work_item_id, epoch)
         SELECT t.parent_task_id, 1 FROM pr_review_verdicts v JOIN tasks t ON t.id = v.work_item_id
         WHERE v.proposal_id = OLD.id AND t.parent_task_id IS NOT NULL
         ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
     END";

/// Schema 54: covering index for the board-review family-key read.
///
/// The warm cache pass walks each on-board owner's revision children
/// to assemble `(member_id, epoch)` keys. `idx_tasks_parent_task_id`
/// covers only `parent_task_id`, so that walk fetched every member
/// `tasks` row to test `kind`. This index covers
/// `(parent_task_id, kind, id)`, so the recursive step answers from
/// the index without touching `tasks` pages.
pub(super) const TASKS_PARENT_KIND_INDEX_54: &str =
    "CREATE INDEX IF NOT EXISTS idx_tasks_parent_kind_id ON tasks(parent_task_id, kind, id)";

/// Schema 55: partial covering index for the verdict arm of the
/// board-review family-key read, and removal of the now-redundant
/// `idx_tasks_parent_task_id`.
///
/// The key read folds in each family member's verdict-named findings
/// revision. `pr_review_verdicts_work_item_idx (work_item_id, created_at)`
/// does not hold `revision_task_id`, so every warm pass fetched every
/// historical verdict row of every on-board family member. This index holds
/// only the rows that name a revision, keyed by `(work_item_id,
/// revision_task_id)`, so the arm is index-only and its cost follows the
/// number of findings revisions rather than review history. The predicate
/// matches the key read's `WHERE` verbatim, which SQLite needs to use it.
/// `idx_tasks_parent_kind_id` (schema 54) starts with `parent_task_id`, so
/// the single-column parent index served nothing it did not and only added
/// write cost to every `tasks` update.
pub(super) const VERDICT_REVISION_INDEX_55: &str = "CREATE INDEX IF NOT EXISTS pr_review_verdicts_revision_idx
         ON pr_review_verdicts(work_item_id, revision_task_id)
         WHERE revision_task_id IS NOT NULL AND revision_task_id != '';
     DROP INDEX IF EXISTS idx_tasks_parent_task_id;";
