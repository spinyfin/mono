//! The Yes path of a `create_prerequisite_task` operator question.
//!
//! A blocked worker that cannot proceed until some other, unrelated piece of
//! work lands proposes that work as a prerequisite task. On a Yes answer,
//! [`approve_in_tx`] runs inside the answer transaction and:
//!
//! 1. finds an equivalent open task in the product or creates the proposed
//!    one in the same project (or a standalone chore) with normal dispatch,
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

/// Open-task lookup for the dedup check: same product and, for project work,
/// same project; same name once trimmed and case-folded, not yet satisfied
/// (`done` / `archived`) and not deleted. Never re-parent a name match.
///
/// Oldest first, so a repeat resolves to the same task every time. The blocked
/// task itself is never its own prerequisite, a task a human has blocked is
/// skipped, as are deferred and human-driven tasks (nothing would dispatch
/// them). A candidate that already depends (transitively) on the blocked task
/// is skipped — linking to it would form a cycle the dependency layer rejects.
fn find_equivalent_open_task(conn: &Connection, task: &Task, name: &str) -> Result<Option<Task>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM tasks
         WHERE product_id = ?1 AND id != ?2 AND deleted_at IS NULL
           AND (?4 IS NULL OR project_id = ?4)
           AND status NOT IN ('done', 'archived') AND human_driven = 0 AND deferred = 0
           AND (status != 'blocked' OR blocked_reason = 'dependency')
           AND lower(trim(name)) = lower(trim(?3))
         ORDER BY created_at ASC, id ASC",
    )?;
    let candidates = stmt
        .query_map(params![task.product_id, task.id, name, task.project_id], |row| {
            row.get::<_, String>(0)
        })?
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
            let prerequisite = if let Some(project_id) = &task.project_id {
                insert_task_in_tx(
                    conn,
                    CreateTaskInput::builder()
                        .product_id(&task.product_id)
                        .project_id(project_id)
                        .name(name)
                        .description(brief)
                        .maybe_repo_remote_url(task.repo_remote_url.clone())
                        .created_via(CREATED_VIA_ENGINE_AUTO)
                        .force_duplicate(true)
                        .build(),
                )?
            } else {
                insert_chore_in_tx(
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
                )?
            };
            (prerequisite, true)
        }
    };

    order_project_prerequisites(conn, task, &prerequisite, now)?;

    // A same-name task parked in the backlog (autostart off) would never be
    // dispatched, leaving the dependent waiting with no signal.
    let schedulable = matches!(prerequisite.status, TaskStatus::Todo | TaskStatus::Blocked);
    let enabled_dispatch = !created && schedulable && !prerequisite.autostart;
    if enabled_dispatch {
        crate::work::task_status_audit::execute(
            conn,
            "approve_in_tx",
            "engine",
            "UPDATE tasks SET autostart = 1, updated_at = ?2 WHERE id = ?1",
            params![prerequisite.id, now],
        )?;
    }

    // Approval is an explicit restart, including after a terminal prior run.
    // Queue it here: the periodic reconciler intentionally cannot restart one.
    if !created && schedulable && query_live_execution_for_work_item(conn, &prerequisite.id)?.is_none() {
        if deps::gating_prereqs_for(conn, &prerequisite.id)?.is_empty() {
            request_execution_in_tx_with_live_check(
                pending,
                conn,
                RequestExecutionInput::builder().work_item_id(&prerequisite.id).build(),
                |_| false,
            )?;
        } else {
            queue_gated_execution(conn, &prerequisite, question)?;
        }
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
    crate::work::task_status_audit::execute(
        conn,
        "approve_in_tx",
        "human",
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

/// Move the unfinished same-project dependency closure with the prerequisite.
/// Revisions share their chain root's place in the project sequence.
fn order_project_prerequisites(conn: &Connection, task: &Task, prerequisite: &Task, now: &str) -> Result<()> {
    let Some(project_id) = &task.project_id else {
        return Ok(());
    };
    let anchor = if task.ordinal.is_some() {
        task.clone()
    } else {
        let Some(root) = get_chain_root_task(conn, &task.id)? else {
            return Ok(());
        };
        root
    };
    if anchor.ordinal.is_none() || prerequisite.project_id.as_ref() != Some(project_id) {
        return Ok(());
    }
    let mut visited = std::collections::HashSet::new();
    let mut ordered = Vec::new();
    let mut stack = vec![(prerequisite.id.clone(), false)];
    while let Some((id, expanded)) = stack.pop() {
        if expanded {
            ordered.push(id);
            continue;
        }
        if !visited.insert(id.clone()) {
            continue;
        }
        let Some(member) = query_task(conn, &id)? else {
            continue;
        };
        if member.project_id.as_ref() != Some(project_id)
            || member.ordinal.is_none()
            || matches!(member.status, TaskStatus::Done | TaskStatus::Archived)
        {
            continue;
        }
        stack.push((id.clone(), true));
        for dependency in deps::prerequisites_of(conn, &id, Some(RELATION_BLOCKS))? {
            stack.push((dependency.prerequisite_id, false));
        }
    }
    let mut stmt = conn
        .prepare("SELECT id, ordinal FROM tasks WHERE project_id = ?1 AND ordinal IS NOT NULL ORDER BY ordinal, id")?;
    let members = stmt
        .query_map([project_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let moving: std::collections::HashSet<_> = ordered.iter().cloned().collect();
    let insertion = members
        .iter()
        .position(|(id, _)| id == &anchor.id || moving.contains(id));
    let Some(insertion) = insertion else {
        return Ok(());
    };
    let mut ids: Vec<_> = members
        .iter()
        .filter(|(id, _)| !moving.contains(id))
        .map(|(id, _)| id.clone())
        .collect();
    ids.splice(insertion..insertion, ordered);
    for (id, (previous_id, ordinal)) in ids.iter().zip(&members) {
        if id != previous_id {
            conn.execute(
                "UPDATE tasks SET ordinal = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, ordinal, now],
            )?;
        }
    }
    Ok(())
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
    if query_latest_execution_for_work_item(conn, &task.id)?.is_some_and(|execution| !execution.status.is_terminal()) {
        return Ok(());
    }
    let preferred = query_execution(conn, &question.execution_id)?
        .filter(|asking| asking.work_item_id == task.id)
        .and_then(|asking| asking.preferred_workspace_id);
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
        "The prerequisite task was created and scheduled for dispatch."
    } else if enabled_dispatch {
        "An equivalent task already existed in the backlog with autostart off, so this task now links to it, and it has been scheduled for dispatch once its dependencies are satisfied."
    } else {
        "An equivalent open task already existed, so this task now links to it instead of creating a duplicate."
    };
    format!(
        "\n\n---\n\n## Prerequisite approved by the repo owner ({timestamp})\n\n\
- **Prerequisite task:** {label}\n\
- **Why this task cannot proceed without it:** {}\n\
- **Asked by run:** `{}`\n\n\
The repo owner approved this. {outcome} This task now depends on it (a `blocks` dependency): it stays parked until the prerequisite is done, and it is then dispatched again. When you resume, assume the prerequisite's change is on `main`; do not redo its work here.\n",
        question.explanation, question.execution_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_note_avoids_internal_vocabulary_in_every_outcome() {
        let (_tmp, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let prerequisite = crate::test_support::create_test_chore(&db, product.id.clone(), "Fix the shared fixture");
        let question = OperatorQuestionView::builder()
            .id("question-1")
            .text("Create the prerequisite?")
            .answer_type(boss_protocol::OperatorAnswerType::CreatePrerequisiteTask {
                name: "Fix the shared fixture".to_owned(),
                brief: "Fix the shared fixture on main.".to_owned(),
            })
            .explanation("CI cannot go green until the fixture is fixed on main.")
            .asked_at("2026-10-23")
            .execution_id("exec_example")
            .build();
        // created; linked with dispatch enabled; already linked.
        for (created, enabled_dispatch) in [(true, false), (false, true), (false, false)] {
            let note = prerequisite_note(
                &question,
                &prerequisite,
                created,
                enabled_dispatch,
                "2026-10-23 00:00 UTC",
            );
            crate::prose_vocabulary::assert_no_forbidden_prose("prerequisite approval note", &note);
            assert!(note.contains("## Prerequisite approved by the repo owner (2026-10-23 00:00 UTC)"));
        }
    }
}
