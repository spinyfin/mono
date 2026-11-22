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
use super::pr_flow::question_restart::RestartAfterAnswer;
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

/// The proposed prerequisite and the timestamps of the Yes that approved it.
pub(super) struct Approval<'a> {
    pub question: &'a OperatorQuestionView,
    pub proposed: (&'a str, &'a str),
    pub answered_at: DateTime<Utc>,
    pub now: &'a str,
}

pub(super) fn approve_in_tx(
    pending: &mut PendingEvents,
    conn: &Connection,
    task: &Task,
    approval: Approval<'_>,
    attribution: crate::task_status_audit::WriteAttribution<'_>,
) -> Result<LinkedPrerequisite> {
    let Approval {
        question,
        proposed: (name, brief),
        answered_at,
        now,
    } = approval;
    if let Some(owner) = prerequisite_link::explicit_owner(conn, task, &question.answer_type)? {
        return approve_existing_in_tx(
            pending,
            conn,
            task,
            ExistingApproval {
                question,
                existing_id: &owner.id,
                answered_at,
                now,
            },
            attribution,
        );
    }
    let (prerequisite, created) = match find_equivalent_open_task(conn, task, name)? {
        Some(existing) => (existing, false),
        None => {
            let prerequisite = if let Some(project_id) = &task.project_id {
                insert_task_in_tx_as_actor_with_provenance(
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
                    attribution.actor,
                    attribution.provenance,
                )?
            } else {
                insert_chore_in_tx_as_actor_with_provenance(
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
                    attribution.actor,
                    attribution.provenance,
                )?
            };
            (prerequisite, true)
        }
    };
    park_behind(
        pending,
        conn,
        task,
        ParkBehind {
            question,
            prerequisite: &prerequisite,
            created,
            answered_at,
            now,
        },
        false,
        attribution,
    )
}

/// The Yes path of a `link_prerequisite_task` operator question: add a
/// `blocks` edge to the already-resolved task and park. Never creates a task.
pub(super) struct ExistingApproval<'a> {
    pub question: &'a OperatorQuestionView,
    pub existing_id: &'a str,
    pub answered_at: DateTime<Utc>,
    pub now: &'a str,
}

pub(super) fn approve_existing_in_tx(
    pending: &mut PendingEvents,
    conn: &Connection,
    task: &Task,
    approval: ExistingApproval<'_>,
    attribution: crate::task_status_audit::WriteAttribution<'_>,
) -> Result<LinkedPrerequisite> {
    let ExistingApproval {
        question,
        existing_id,
        answered_at,
        now,
    } = approval;
    let Some(prerequisite) = query_task(conn, existing_id)? else {
        bail!("the named task no longer exists");
    };
    if prerequisite.deleted_at.is_some() {
        bail!("the named task no longer exists");
    }
    if matches!(prerequisite.status, TaskStatus::Done | TaskStatus::Archived) {
        bail!("the named task is already finished");
    }
    if let Some(owner) = prerequisite_link::explicit_owner(conn, task, &question.answer_type)? {
        anyhow::ensure!(
            owner.id == existing_id,
            "the pull request owner changed since the question was asked"
        );
    }
    if prerequisite.id == task.id {
        bail!("a task cannot wait on itself");
    }
    if prerequisite_link::in_askers_revision_chain(conn, task, &prerequisite)? {
        bail!(prerequisite_link::SAME_CHAIN_REJECTION);
    }
    if deps::would_create_cycle(conn, &task.id, &prerequisite.id)? {
        bail!("linking that task would create a cycle of waiting tasks");
    }
    park_behind(
        pending,
        conn,
        task,
        ParkBehind {
            question,
            prerequisite: &prerequisite,
            created: false,
            answered_at,
            now,
        },
        true,
        attribution,
    )
}

struct ParkBehind<'a> {
    question: &'a OperatorQuestionView,
    prerequisite: &'a Task,
    created: bool,
    answered_at: DateTime<Utc>,
    now: &'a str,
}

fn park_behind(
    pending: &mut PendingEvents,
    conn: &Connection,
    task: &Task,
    park: ParkBehind<'_>,
    from_link: bool,
    attribution: crate::task_status_audit::WriteAttribution<'_>,
) -> Result<LinkedPrerequisite> {
    let ParkBehind {
        question,
        prerequisite,
        created,
        answered_at,
        now,
    } = park;
    order_project_prerequisites(conn, task, prerequisite, now)?;

    // A same-name task parked in the backlog (autostart off) would never be
    // dispatched, leaving the dependent waiting with no signal. A task that
    // already owns a PR must not get a new primary execution — that path
    // refuses with "use a revision to change it" and would roll the Yes back.
    // The create path only ever links to a task `find_equivalent_open_task`
    // would accept (todo, or blocked on a dependency, and not human-driven
    // or deferred). The link path reuses this helper without that filter, so
    // a restart here would withdraw the target's own open question or fail
    // the Yes for a human-driven / question-dismissed row. Wait-only then.
    let schedulable = matches!(prerequisite.status, TaskStatus::Todo | TaskStatus::Blocked);
    let eligible = parking::upkeep_eligible(conn, &prerequisite.id)?;
    let owns_pr = prerequisite.pr_url.as_deref().is_some_and(|url| !url.is_empty());
    let create_path_runnable = !prerequisite.human_driven
        && !prerequisite.deferred
        && match prerequisite.status {
            TaskStatus::Todo => true,
            TaskStatus::Blocked => prerequisite.blocked_reason.as_deref() == Some("dependency"),
            _ => false,
        };
    let needs_fresh_run = eligible && !created && schedulable && !owns_pr && (!from_link || create_path_runnable);
    let enabled_dispatch = needs_fresh_run && !prerequisite.autostart;
    let actor = attribution.actor;
    let basis = attribution.basis("approve_in_tx");
    // One operation across the autostart enable, the todo reset and the
    // dependency-edge demotion to `blocked`, so the history correlates them.
    let operation = crate::task_status_audit::begin_operation_on(conn)?;
    if enabled_dispatch {
        crate::task_status_audit::execute(
            conn,
            &basis,
            actor,
            "UPDATE tasks SET autostart = 1, updated_at = ?2 WHERE id = ?1",
            params![prerequisite.id, now],
        )?;
    }

    // Approval is an explicit restart, including after a terminal prior run.
    // Queue it here: the periodic reconciler intentionally cannot restart one.
    // A missing repo is a sticky attention on the prerequisite, the same as
    // [`queue_gated_execution`]: it must not refuse the Yes that linked it.
    if needs_fresh_run && query_live_execution_for_work_item(conn, &prerequisite.id)?.is_none() {
        if !deps::gating_prereqs_for(conn, &prerequisite.id)?.is_empty() {
            queue_gated_execution(conn, prerequisite, question)?;
        } else if resolve_repo_for_work_item(conn, &prerequisite.id)?.is_some() {
            request_execution_in_tx_with_live_check(
                pending,
                conn,
                RequestExecutionInput::builder()
                    .work_item_id(&prerequisite.id)
                    .audit_actor(actor)
                    .maybe_audit_basis(attribution.provenance.map(str::to_owned))
                    .created_via("prerequisite_question:approval")
                    .build(),
                |_| false,
            )?;
        }
    }

    let timestamp = answered_at.format("%Y-%m-%d %H:%M UTC").to_string();
    let description = format!(
        "{}{}",
        task.description,
        prerequisite_note(question, prerequisite, created, enabled_dispatch, from_link, &timestamp,)
    );
    super::description_guard::validate_description_update(&task.description, &description, false)?;
    // `todo` + autostart first, so the edge below parks the task through the
    // ordinary engine auto-block (`blocked` / `dependency`) and the cascade
    // can later move it back to `todo` and dispatch it.
    crate::task_status_audit::execute(
        conn,
        &basis,
        actor,
        "UPDATE tasks SET description = ?2, status = 'todo', blocked_reason = NULL,
         blocked_detail = NULL, autostart = 1, last_status_actor = ?4, updated_at = ?3 WHERE id = ?1",
        params![task.id, description, now, actor],
    )?;
    add_dependency_edge_in_tx(pending, conn, &task.id, &prerequisite.id, RELATION_BLOCKS, now)?;
    queue_gated_execution(conn, task, question)?;
    stamp_answered_waiting(conn, &task.id)?;
    operation.end()?;
    tracing::info!(
        work_item_id = %task.id,
        prerequisite_id = %prerequisite.id,
        created,
        "operator approved a prerequisite task; dependent parked behind it",
    );
    Ok(LinkedPrerequisite {
        id: prerequisite.id.clone(),
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
            || matches!(member.status, TaskStatus::Done | TaskStatus::Archived)
        {
            continue;
        }
        // Revisions have no ordinal, but their prerequisites still need to move.
        if member.ordinal.is_some() {
            stack.push((id.clone(), true));
        }
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

/// Per-answer knobs for [`restart_after_answer_from`].
#[derive(Clone, Copy)]
pub(super) struct RestartOptions<'a> {
    pub now: &'a str,
    /// Record the answer but never request an explicit start for the task.
    pub suppress_start: bool,
}

impl<'a> RestartOptions<'a> {
    pub(super) fn new(now: &'a str, suppress_start: bool) -> Self {
        Self { now, suppress_start }
    }
}

/// Prefix of the `blocked_detail` that marks a parked task whose question is
/// answered and which now waits on its gating prerequisites. The board card
/// reads it to show "Answered · waits for <prerequisite>".
const ANSWERED_WAITING_DETAIL_PREFIX: &str = "Answered · waits for ";

/// After an answer parks `task_id` behind its gating prerequisites, record that
/// it is answered and waiting, naming the prerequisites. No-op unless the task
/// is parked `blocked` / `dependency`. The marker is cleared when the dependency
/// block lifts or the task is re-blocked (see [`clear_answered_waiting`]).
pub(crate) fn stamp_answered_waiting(conn: &Connection, task_id: &str) -> Result<()> {
    let names = deps::gating_prereqs_for(conn, task_id)?
        .iter()
        .map(|id| Ok(query_task(conn, id)?.map_or_else(|| id.clone(), |prerequisite| prerequisite.name)))
        .collect::<Result<Vec<_>>>()?;
    if names.is_empty() {
        return Ok(());
    }
    conn.execute(
        "UPDATE tasks SET blocked_detail = ?2 WHERE id = ?1 AND status = 'blocked' AND blocked_reason = 'dependency'",
        params![task_id, format!("{ANSWERED_WAITING_DETAIL_PREFIX}{}", names.join(", "))],
    )?;
    Ok(())
}

/// Copy force-launch lineage onto the latest continuation of `task_id`.
/// When start is not suppressed, persist operator-forced admission so the
/// drain can admit it through an operator pause, and report the task for
/// `launch_answered_work`; returns `None` when no continuation exists to mark.
///
/// This is deliberately lineage-driven, not hold-driven: a run admitted by an
/// operator force launch keeps operator-forced standing through every
/// answered-question resume, the same as `bossctl agents launch`, whether or
/// not a pause is active when the answer arrives. That way the resume cannot
/// be stranded by a pause or focus change between answer and drain. The
/// hard pool cap and the writer/preflight/predecessor-lease holds still apply.
pub(super) fn inherit_force_launch_continuation(
    tx: &rusqlite::Transaction<'_>,
    task_id: &str,
    asking_execution_id: &str,
    suppress_start: bool,
) -> Result<Option<String>> {
    if !super::dispatch::execution_force_launched_in(tx, asking_execution_id)? {
        return Ok(None);
    }
    // Resolve the continuation once so the lineage stamp and the operator-forced
    // mark always land on the same row.
    let Some(execution) = query_latest_execution_for_work_item(tx, task_id)?.filter(|e| !e.status.is_terminal()) else {
        return Ok(None);
    };
    super::dispatch::mark_execution_force_launched_in(tx, &execution.id)?;
    if suppress_start {
        return Ok(None);
    }
    super::conflict_mismatch::mark_execution_operator_forced_admission_in_tx(tx, &execution.id)?;
    Ok(Some(task_id.to_string()))
}

/// After an answer on an ungated, eligible task: leave a schedulable
/// continuation, then (unless `suppress_start`) try an explicit start.
/// Reports whether an explicit start minted the execution and, when the asking
/// run was force-launched, the work item to launch through the pause.
pub(super) fn continue_after_answer(
    pending: &mut PendingEvents,
    tx: &rusqlite::Transaction<'_>,
    task: &Task,
    question: &OperatorQuestionView,
    suppress_start: bool,
    attribution: crate::task_status_audit::WriteAttribution<'_>,
) -> Result<RestartAfterAnswer> {
    let actor = attribution.actor;
    // Recording the answer must not depend on starting the task, so the
    // explicit start is a best-effort step under a savepoint: a refusal
    // (quarantine, parked row) rolls back only the start. It runs first: for a
    // revision whose spawning attempt has retired, the start reopens that
    // attempt, whereas reconciling first would settle the row blocked and
    // release its same-chain dependents before the reopen.
    let task = query_task(tx, &task.id).require("task", &task.id)?;
    if !suppress_start {
        let asking = query_execution(tx, &question.execution_id).require("execution", &question.execution_id)?;
        tx.execute_batch("SAVEPOINT answer_explicit_start")?;
        let started = request_execution_in_tx_with_live_check(
            pending,
            tx,
            RequestExecutionInput::builder()
                .work_item_id(&task.id)
                .maybe_preferred_workspace_id(asking.preferred_workspace_id)
                .entry_point(boss_protocol::DispatchAdmissionEntryPoint::OperatorAnswer)
                .explicit_start(true)
                .audit_actor(actor)
                .maybe_audit_basis(attribution.provenance.map(str::to_owned))
                .created_via("operator_answer:request_execution")
                .build(),
            // Match request_execution's conservative behavior when
            // no live-worker oracle is supplied.
            |_| true,
        );
        match started {
            Ok(_) => {
                tx.execute_batch("RELEASE answer_explicit_start")?;
                let dispatch_work_item_id =
                    inherit_force_launch_continuation(tx, &task.id, &question.execution_id, false)?;
                return Ok(RestartAfterAnswer {
                    minted: true,
                    dispatch_work_item_id,
                });
            }
            Err(error) => {
                tx.execute_batch("ROLLBACK TO answer_explicit_start; RELEASE answer_explicit_start")?;
                tracing::warn!(
                    work_item_id = %task.id,
                    %error,
                    "answer recorded; explicit start refused, a queued continuation stays for the scheduler",
                );
            }
        }
    }
    // No start happened (suppressed or refused): leave a schedulable
    // continuation, since the asking run has ended and nothing else mints a
    // replacement after a terminal run. A force-launched asker stamps
    // operator-forced admission on that continuation so the drain can admit
    // it through an operator pause; otherwise the scheduler applies the
    // ordinary pause and capacity rules.
    if task.kind == TaskKind::Revision {
        // A revision whose spawning attempt retired unsuccessfully is settled by
        // the reconciler's retired-attempt gate (blocked, cascading to its
        // dependents). Leave it `todo`: only an explicit start reopens the
        // attempt, and settling it here would release dependents the gate
        // should still hold.
        if retired_spawning_attempt(tx, &task)?.is_none() {
            reconcile_revision_execution(pending, tx, &mut ExecutionReconcileResult::default(), &task)?;
        }
    } else {
        super::prerequisite_question::queue_ready_execution(tx, &task, question)?;
    }
    let dispatch_work_item_id =
        inherit_force_launch_continuation(tx, &task.id, &question.execution_id, suppress_start)?;
    Ok(RestartAfterAnswer {
        minted: false,
        dispatch_work_item_id,
    })
}

/// Drop the answered-waiting marker from `task_id`'s `blocked_detail`, leaving
/// any other detail alone. Called whenever the dependency block lifts or is
/// re-applied so a later block never shows a stale prerequisite name.
pub(crate) fn clear_answered_waiting(conn: &Connection, task_id: &str) -> Result<bool> {
    let cleared = conn.execute(
        "UPDATE tasks SET blocked_detail = NULL WHERE id = ?1 AND blocked_detail LIKE ?2",
        params![task_id, format!("{ANSWERED_WAITING_DETAIL_PREFIX}%")],
    )?;
    Ok(cleared > 0)
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
pub(super) fn queue_gated_execution(conn: &Connection, task: &Task, question: &OperatorQuestionView) -> Result<()> {
    if deps::gating_prereqs_for(conn, &task.id)?.is_empty() {
        return Ok(());
    }
    queue_continuation_execution(conn, task, question, ExecutionStatus::WaitingDependency)
}

/// Leave a `ready` execution behind for an answered task that nothing gates.
///
/// Recording an answer must not depend on starting the task: the asking run
/// has already ended, and nothing else mints a replacement after a terminal
/// run. This row is the schedulable continuation, so the scheduler admits it
/// under the ordinary pause and capacity rules whether or not the caller also
/// asked for an explicit start. Revisions are left to their own reconciler.
pub(super) fn queue_ready_execution(conn: &Connection, task: &Task, question: &OperatorQuestionView) -> Result<()> {
    queue_continuation_execution(conn, task, question, ExecutionStatus::Ready)
}

fn queue_continuation_execution(
    conn: &Connection,
    task: &Task,
    question: &OperatorQuestionView,
    status: ExecutionStatus,
) -> Result<()> {
    if !parking::upkeep_eligible(conn, &task.id)? {
        return Ok(());
    }
    let kind = execution_kind_for_work_item(conn, &task.id)?;
    if kind == ExecutionKind::RevisionImplementation || primary_pr_awaits_review(conn, &task.id)? {
        return Ok(());
    }
    let Some(repo_remote_url) = resolve_repo_for_work_item(conn, &task.id)? else {
        // No execution can be minted without a repository. File the sticky
        // `repo_unresolved` attention directly (the reconciler would not: the
        // asking run is terminal). Once the repository resolves, the
        // reconciler sees the open attention and mints the continuation.
        let label = repo_unresolved_kind_label(conn, &task.id)?;
        record_repo_unresolved_attention(conn, &task.id, label)?;
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
            .created_via("prerequisite_question_restart")
            .work_item_id(&task.id)
            .kind(kind)
            .status(status)
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
    from_link: bool,
    timestamp: &str,
) -> String {
    // The canonical id, never the friendly short id: a worker that quotes the
    // note in a commit or PR body must not trip the work-item-id leakage check.
    let label = format!("{} (`{}`)", prerequisite.name, prerequisite.id);
    let outcome = if created {
        "The prerequisite task was created and scheduled for dispatch."
    } else if from_link && enabled_dispatch {
        "This task now waits for the named existing task, and that task has been scheduled for dispatch once its dependencies are satisfied."
    } else if from_link {
        "This task now waits for the named existing task."
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
                file_count_bypass: None,
                option_labels: None,
                existing_id: None,
                existing_pr: None,
            })
            .explanation("CI cannot go green until the fixture is fixed on main.")
            .asked_at("2026-10-23")
            .execution_id("exec_example")
            .build();
        // created; linked with dispatch enabled; already linked; named-existing link.
        for (created, enabled_dispatch, from_link) in [
            (true, false, false),
            (false, true, false),
            (false, false, false),
            (false, false, true),
            (false, true, true),
        ] {
            let note = prerequisite_note(
                &question,
                &prerequisite,
                created,
                enabled_dispatch,
                from_link,
                "2026-10-23 00:00 UTC",
            );
            crate::prose_vocabulary::assert_no_forbidden_prose("prerequisite approval note", &note);
            assert!(note.contains("## Prerequisite approved by the repo owner (2026-10-23 00:00 UTC)"));
            if from_link {
                assert!(note.contains("This task now waits for the named existing task"));
                assert!(!note.contains("instead of creating a duplicate"));
            }
        }
    }
}
