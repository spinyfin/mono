//! The Yes path of a `create_prerequisite_task` operator question.
//!
//! A blocked worker that cannot proceed until some other, unrelated piece of
//! work lands proposes that work as a prerequisite task. On a Yes answer,
//! [`approve_in_tx`] runs inside the answer transaction and:
//!
//! 1. finds an equivalent open task in the product or creates the proposed
//!    one as a chore with normal dispatch (autostart on),
//! 2. declares a `blocks` edge so the blocked task depends on it, which parks
//!    the task as `blocked` / `dependency` behind the prerequisite — the
//!    existing dependency cascade redispatches it once the prerequisite is
//!    satisfied,
//! 3. appends a dated note to the blocked task's brief recording the
//!    approved prerequisite.
//!
//! Everything shares the caller's transaction, so a failure anywhere (a
//! dependency cycle, a guard rejecting the brief) rolls the whole answer back
//! and the question stays open.

use super::dispatch_admission::primary_pr_awaits_review;
use super::*;
use boss_protocol::{CREATED_VIA_ENGINE_AUTO, OperatorQuestionView, TaskStatus};
use chrono::{DateTime, Utc};

/// The prerequisite the blocked task now waits on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LinkedPrerequisite {
    pub id: String,
    /// `false` when an equivalent open task already existed and the edge was
    /// added to it instead of creating a duplicate.
    pub created: bool,
}

/// Open-task lookup for the dedup check: same product, same name once trimmed
/// and case-folded, not yet satisfied (`done` / `archived`) and not deleted.
///
/// Oldest first, so a repeat resolves to the same task every time. The blocked
/// task itself is never its own prerequisite, a task a human has blocked is
/// skipped (nothing would dispatch it), and a candidate that already
/// depends (transitively) on the blocked task is skipped — linking to it
/// would form a cycle the dependency layer rejects.
fn find_equivalent_open_task(conn: &Connection, task: &Task, name: &str) -> Result<Option<Task>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM tasks
         WHERE product_id = ?1 AND id != ?2 AND deleted_at IS NULL
           AND status NOT IN ('done', 'archived')
           AND (status != 'blocked' OR blocked_reason = 'dependency')
           AND lower(trim(name)) = lower(trim(?3))
         ORDER BY created_at ASC, id ASC",
    )?;
    let candidates = stmt
        .query_map(params![task.product_id, task.id, name], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for candidate in candidates {
        if !deps::would_create_cycle(conn, &task.id, &candidate)? {
            return query_task(conn, &candidate);
        }
    }
    Ok(None)
}

pub(super) fn approve_in_tx(
    pending: &mut PendingEvents,
    conn: &Connection,
    task: &Task,
    question: &OperatorQuestionView,
    proposed: (&str, &str),
    answered_at: DateTime<Utc>,
    now: &str,
) -> Result<LinkedPrerequisite> {
    let (name, brief) = proposed;
    let (prerequisite, created) = match find_equivalent_open_task(conn, task, name)? {
        Some(existing) => (existing, false),
        None => {
            let chore = insert_chore_in_tx(
                conn,
                CreateChoreInput::builder()
                    .product_id(task.product_id.clone())
                    .name(name)
                    .description(brief)
                    // Same repo as the task it unblocks, when the product
                    // leaves the repo to the task.
                    .maybe_repo_remote_url(task.repo_remote_url.clone())
                    .created_via(CREATED_VIA_ENGINE_AUTO)
                    // The equivalence check above is the duplicate guard here;
                    // the recent-name guard would only reject a re-proposal of
                    // a task that has just been completed.
                    .force_duplicate(true)
                    .build(),
            )?;
            (chore, true)
        }
    };

    // A same-name task parked in the backlog (autostart off) would never be
    // dispatched, leaving the dependent waiting with no signal.
    let enabled_dispatch = !created && prerequisite.status == TaskStatus::Todo && !prerequisite.autostart;
    if enabled_dispatch {
        conn.execute(
            "UPDATE tasks SET autostart = 1, updated_at = ?2 WHERE id = ?1",
            params![prerequisite.id, now],
        )?;
    }

    let timestamp = answered_at.format("%Y-%m-%d %H:%M UTC").to_string();
    let description = format!(
        "{}{}",
        task.description,
        prerequisite_note(question, &prerequisite, created, enabled_dispatch, &timestamp)
    );
    super::description_guard::validate_description_update(&task.description, &description, false)?;
    // `todo` + autostart first, so the edge below parks the task through the
    // ordinary engine auto-block (`blocked` / `dependency`) and the cascade
    // can later move it back to `todo` and dispatch it.
    conn.execute(
        "UPDATE tasks SET description = ?2, status = 'todo', blocked_reason = NULL,
         blocked_detail = NULL, autostart = 1, last_status_actor = 'human', updated_at = ?3 WHERE id = ?1",
        params![task.id, description, now],
    )?;
    add_dependency_edge_in_tx(pending, conn, &task.id, &prerequisite.id, RELATION_BLOCKS, now)?;
    queue_gated_execution(conn, task, question)?;
    tracing::info!(
        work_item_id = %task.id,
        prerequisite_id = %prerequisite.id,
        created,
        "operator approved a prerequisite task; dependent parked behind it",
    );
    Ok(LinkedPrerequisite {
        id: prerequisite.id,
        created,
    })
}

/// Leave a `waiting_dependency` execution behind for the parked task.
///
/// The cascade that fires when the prerequisite is satisfied only promotes an
/// execution that already exists: the asking run ended `failed`, and the
/// ordinary reconcile deliberately never mints a replacement after a terminal
/// run. Without this row the task would unblock to `todo` and then sit there.
/// The row carries the asking run's workspace preference, as a plain Yes does,
/// so the resumed run lands where the branch state is. Revisions are left
/// alone: their own reconciler mints the replacement when the gate clears.
fn queue_gated_execution(conn: &Connection, task: &Task, question: &OperatorQuestionView) -> Result<()> {
    let kind = execution_kind_for_work_item(conn, &task.id)?;
    if kind == ExecutionKind::RevisionImplementation
        || deps::gating_prereqs_for(conn, &task.id)?.is_empty()
        || primary_pr_awaits_review(conn, &task.id)?
    {
        return Ok(());
    }
    let Some(repo_remote_url) = resolve_repo_for_work_item(conn, &task.id)? else {
        // Same condition under which the reconciler refuses to mint; the
        // sticky `repo_unresolved` attention it files surfaces the missing
        // repository.
        return Ok(());
    };
    let preferred = query_execution(conn, &question.execution_id)?.and_then(|asking| asking.preferred_workspace_id);
    insert_execution(
        conn,
        CreateExecutionInput::builder()
            .work_item_id(&task.id)
            .kind(kind)
            .status(ExecutionStatus::WaitingDependency)
            .repo_remote_url(repo_remote_url)
            .allow_dirty(preferred.is_some())
            .prefer_is_soft(preferred.is_some())
            .maybe_preferred_workspace_id(preferred)
            .build(),
    )?;
    Ok(())
}

fn prerequisite_note(
    question: &OperatorQuestionView,
    prerequisite: &Task,
    created: bool,
    enabled_dispatch: bool,
    timestamp: &str,
) -> String {
    // The canonical id, never the friendly short id: a worker that quotes the
    // note in a commit or PR body must not trip the work-item-id leakage check.
    let label = format!("{} (`{}`)", prerequisite.name, prerequisite.id);
    let outcome = if created {
        "The engine created it as a chore with normal dispatch."
    } else if enabled_dispatch {
        "An equivalent task already existed in the backlog with autostart off, so the engine linked to it and turned autostart on so it is dispatched."
    } else {
        "An equivalent open task already existed, so the engine linked to it instead of creating a duplicate."
    };
    format!(
        "\n\n---\n\n## Operator-approved prerequisite ({timestamp})\n\n\
- **Prerequisite task:** {label}\n\
- **Why this task cannot proceed without it:** {}\n\
- **Asked by run:** `{}`\n\n\
The operator approved this on the kanban. {outcome} This task now depends on it (a `blocks` dependency): it stays parked until the prerequisite is done, and the engine then dispatches it again. When you resume, assume the prerequisite's change is on `main`; do not redo its work here.\n",
        question.explanation, question.execution_id
    )
}
