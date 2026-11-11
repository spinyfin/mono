//! The eight progression verb effects (design 2.4). Each one checks scope and its
//! own preconditions, then the fingerprint rule and the bounds, then routes
//! through the existing engine path the design names for it:
//!
//! | verb | engine path |
//! | --- | --- |
//! | `revise` | `assert_parent_revisable_and_insert_as_actor`, the path `create_revision` runs |
//! | `precursor` | `insert_chore_in_tx_as_actor` plus `add_dependency_edge_in_tx` |
//! | `depend` / `undepend` | `add_dependency_edge_in_tx` / `delete_edge` plus the dependency unblock |
//! | `redispatch` | `request_execution_in_tx_with_live_check` (an explicit start), or the review restart for an `in_review` item |
//! | `clear-signal` | the merge poller's retire writes (`clear_blocked_*_in_tx`) |
//! | `archive-revision` | `archive_revision_task_as_actor` plus the dependent cascade |
//! | `unblock` | the `blocked -> todo` (or `in_review`, when the item already owns a bound PR) move |
//!
//! Every write is made as `progression`, so the row's `last_status_actor` and the
//! ledger read as the agent's decision and the dependency cascade treats a
//! progression block the way it treats a human's.

use super::*;
use crate::chain_helpers::{ARCHIVE_MECHANISM_PROGRESSION, ArchiveProvenance, archive_revision_task_as_actor};
use boss_protocol::LAST_STATUS_ACTOR_PROGRESSION as PROGRESSION;

/// `Task::name` is a card title; keep the derived one short.
const DERIVED_NAME_CHARS: usize = 60;

fn truncated(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    match line.char_indices().nth(DERIVED_NAME_CHARS) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_owned(),
    }
}

/// Stamp the row a verb created as the progression agent's own. The insert
/// helpers record the actor in the status ledger but leave `last_status_actor` at
/// its column default, so the stamp is written here.
fn stamp_progression_actor(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET last_status_actor = ?2 WHERE id = ?1",
        params![id, PROGRESSION],
    )?;
    Ok(())
}

// ── revise ───────────────────────────────────────────────────────────────────

/// The open progression revision on the PR the chain rooted at `root` owns.
/// Revisions are always parented on their chain root. "Open" is anything not yet
/// `done`/`archived`, an `in_review` revision included: it has pushed, but the
/// PR has not landed, and a second progression revision beside it would be a
/// second attempt at a PR whose first has not been judged.
fn open_progression_revision(conn: &Connection, root: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT id FROM tasks
             WHERE kind = 'revision' AND parent_task_id = ?1 AND created_via LIKE 'progression:%'
               AND deleted_at IS NULL AND status NOT IN ('done', 'archived')
             ORDER BY created_at, id LIMIT 1",
            [root],
            |row| row.get(0),
        )
        .optional()?)
}

pub(super) fn revise(
    conn: &Connection,
    pending: &mut PendingEvents,
    ctx: &VerbContext,
    payload: &ReviseProposalPayload,
    pr_checker: &dyn PrStateChecker,
) -> VerbResult<Applied> {
    let verb = ProposalKind::Revise;
    ctx.scope(conn, &ctx.item.id)?;
    ctx.refuse_repeat(conn, verb, "")?;
    ctx.refuse_over_budget(conn)?;
    let root = chain_root(conn, &ctx.item.id)?;
    if let Some(revision) = open_progression_revision(conn, &root)? {
        return Err(ProgressionVerbRefusal::OpenProgressionRevision { revision }.into());
    }
    let input = CreateRevisionInput::builder()
        .parent_task_id(ctx.item.id.clone())
        .description(payload.brief.clone())
        .name(format!("Unstick: {}", truncated(&ctx.item.name)))
        .created_via(ctx.created_via.clone())
        // The one-open-revision rule and the budget above are the duplicate guard.
        .force_duplicate(true)
        .build();
    // The create-time gate checks the chain root's PR is open: a closed or
    // merged PR is refused before anything is written, and the chain-tail gate
    // holds the new revision `blocked` while another writer is live on the PR.
    let revision = match assert_parent_revisable_and_insert_as_actor(pending, conn, input, pr_checker, PROGRESSION) {
        Err(error) if error.downcast_ref::<RevisionGateError>().is_some() => {
            return Err(ProgressionVerbRefusal::PrNotOpen {
                detail: error.to_string(),
            }
            .into());
        }
        other => via_engine_path(verb, other)?,
    };
    stamp_progression_actor(conn, &revision.id)?;
    Ok(Applied {
        subject: String::new(),
        result_ref: Some(revision.id),
        signal_retirement: None,
    })
}

// ── precursor ────────────────────────────────────────────────────────────────

/// The open progression-created precursor the item waits on: a non-revision task
/// stamped `progression:%` that is the item's prerequisite and not yet finished.
fn open_progression_precursor(conn: &Connection, item: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT t.id FROM work_item_dependencies d
             JOIN tasks t ON t.id = d.prerequisite_id
             WHERE d.dependent_id = ?1 AND d.relation = 'blocks'
               AND t.kind <> 'revision' AND t.created_via LIKE 'progression:%'
               AND t.deleted_at IS NULL AND t.status NOT IN ('done', 'archived')
             ORDER BY t.created_at, t.id LIMIT 1",
            [item],
            |row| row.get(0),
        )
        .optional()?)
}

pub(super) fn precursor(
    conn: &Connection,
    pending: &mut PendingEvents,
    ctx: &VerbContext,
    payload: &PrecursorProposalPayload,
) -> VerbResult<Applied> {
    let verb = ProposalKind::Precursor;
    ctx.scope(conn, &ctx.item.id)?;
    ctx.refuse_live_work(conn, verb)?;
    ctx.refuse_repeat(conn, verb, "")?;
    ctx.refuse_over_budget(conn)?;
    if let Some(precursor) = open_progression_precursor(conn, &ctx.item.id)? {
        return Err(ProgressionVerbRefusal::OpenProgressionPrecursor { precursor }.into());
    }
    // A chore in the item's product, born `todo` with autostart on at the default
    // priority. It is deliberately not placed in the item's project: it is work on
    // another part of the tree, and project sequencing would order it behind the
    // item that now waits on it.
    let input = CreateChoreInput::builder()
        .product_id(ctx.item.product_id.clone())
        .name(payload.name.clone())
        .description(payload.brief.clone())
        .maybe_repo_remote_url(ctx.item.repo_remote_url.clone())
        .created_via(ctx.created_via.clone())
        .force_duplicate(true)
        .build();
    let chore = via_engine_path(verb, insert_chore_in_tx_as_actor(conn, input, PROGRESSION))?;
    stamp_progression_actor(conn, &chore.id)?;
    // The edge makes the item `blocked: dependency` behind the chore; the
    // dependency cascade releases it when the chore is satisfied.
    via_engine_path(
        verb,
        crate::dep_helpers::add_dependency_edge_in_tx_as_actor(
            pending,
            conn,
            &ctx.item.id,
            &chore.id,
            RELATION_BLOCKS,
            &now_string(),
            PROGRESSION,
        ),
    )?;
    stage_task_board_change(pending, conn, &chore.id)?;
    Ok(Applied {
        subject: String::new(),
        result_ref: Some(chore.id),
        signal_retirement: None,
    })
}

// ── depend / undepend ────────────────────────────────────────────────────────

pub(super) fn depend(
    conn: &Connection,
    pending: &mut PendingEvents,
    ctx: &VerbContext,
    payload: &DependProposalPayload,
) -> VerbResult<Applied> {
    let verb = ProposalKind::Depend;
    let on = via_engine_path(verb, resolve_target(conn, &payload.on))?;
    ctx.scope(conn, &ctx.item.id)?;
    ctx.scope(conn, &on)?;
    if on == ctx.item.id {
        return Err(ProgressionVerbRefusal::Precondition {
            verb: verb.to_string(),
            message: "an item cannot depend on itself".into(),
        }
        .into());
    }
    ctx.refuse_live_work(conn, verb)?;
    ctx.refuse_repeat(conn, verb, &on)?;
    via_engine_path(
        verb,
        crate::dep_helpers::add_dependency_edge_in_tx_as_actor(
            pending,
            conn,
            &ctx.item.id,
            &on,
            RELATION_BLOCKS,
            &now_string(),
            PROGRESSION,
        ),
    )?;
    Ok(Applied {
        result_ref: Some(on.clone()),
        subject: on,
        signal_retirement: None,
    })
}

/// Whether the prerequisite `on` is one `undepend` may drop: finished, missing,
/// or a dead hidden revision whose parent PR is green. Anything else is a
/// prerequisite the item is still legitimately waiting on.
fn undependable(conn: &Connection, on: &str) -> Result<std::result::Result<(), String>> {
    let status = deps::lookup_work_item_status(conn, on)?;
    let Some(status) = status else {
        return Ok(Ok(())); // a missing prerequisite row gates forever
    };
    if matches!(status.as_str(), "done" | "archived") {
        return Ok(Ok(()));
    }
    let Some(task) = query_task(conn, on)? else {
        return Ok(Err(format!(
            "{on} is `{status}`, a live prerequisite the item is still waiting on"
        )));
    };
    if task.deleted_at.is_some() {
        return Ok(Ok(()));
    }
    if task.kind != TaskKind::Revision {
        return Ok(Err(format!(
            "{on} is `{status}` and unfinished; the item is still waiting on it"
        )));
    }
    if let Some((execution, execution_status)) = live_or_pending_execution(conn, on)? {
        return Ok(Err(format!(
            "{on} is a revision with live or pending work ({execution}, `{execution_status}`)"
        )));
    }
    let root = get_chain_root_task(conn, on)?;
    if root.as_ref().and_then(|root| root.ci_required_state.as_deref()) != Some("success") {
        return Ok(Err(format!(
            "{on} is a revision whose parent PR is not green, so it may still be doing needed work"
        )));
    }
    Ok(Ok(()))
}

pub(super) fn undepend(
    conn: &Connection,
    pending: &mut PendingEvents,
    ctx: &VerbContext,
    payload: &UndependProposalPayload,
) -> VerbResult<Applied> {
    let verb = ProposalKind::Undepend;
    let on = via_engine_path(verb, resolve_target(conn, &payload.on))?;
    ctx.scope(conn, &ctx.item.id)?;
    ctx.scope(conn, &on)?;
    let precondition = |message: String| -> VerbError {
        ProgressionVerbRefusal::Precondition {
            verb: verb.to_string(),
            message,
        }
        .into()
    };
    if deps::query_edge(conn, &ctx.item.id, &on, RELATION_BLOCKS)?.is_none() {
        return Err(precondition(format!("{} does not depend on {on}", ctx.item.id)));
    }
    ctx.refuse_live_work(conn, verb)?;
    if let Err(message) = undependable(conn, &on)? {
        return Err(precondition(message));
    }
    ctx.refuse_repeat(conn, verb, &on)?;
    deps::delete_edge(conn, &ctx.item.id, &on, RELATION_BLOCKS)?;
    // The engine-owned dependency block lifts once nothing gates the item; a
    // progression or human block of another reason is left alone.
    crate::dep_helpers::maybe_engine_unblock_dependent_as_actor(
        pending,
        conn,
        &ctx.item.id,
        &now_string(),
        PROGRESSION,
    )?;
    stage_task_board_change(pending, conn, &ctx.item.id)?;
    Ok(Applied {
        result_ref: Some(on.clone()),
        subject: on,
        signal_retirement: None,
    })
}

// ── redispatch ───────────────────────────────────────────────────────────────

/// The redispatch bound (design 2.5): at most one progression `redispatch` per
/// `(item, stuck kind)` until a non-progression work run on the item completes.
/// A changed fingerprint, a failed run, a new terminal execution id, a changed
/// PR head, and a park or un-park all leave the bound standing; only a completed
/// work run *after* the redispatch's execution resets it. Returns the run that
/// spent the bound.
fn redispatch_bound(conn: &Connection, item: &str, stuck_kind: &str) -> Result<Option<String>> {
    let latest: Option<(String, String, Option<String>)> = conn
        .query_row(
            "SELECT execution_id, created_at, result_ref FROM progression_actions
             WHERE work_item_id = ?1 AND verb = 'redispatch' AND stuck_kind = ?2
             ORDER BY rowid DESC LIMIT 1",
            params![item, stuck_kind],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((prior_execution, acted_at, minted)) = latest else {
        return Ok(None);
    };
    // The reset is a completed run created at or after the execution the action
    // minted (its `created_at`, then its id as the tie-break); an action whose
    // execution was pruned falls back to the action's own time.
    let anchor: Option<(String, String)> = match &minted {
        Some(id) => conn
            .query_row(
                "SELECT created_at, id FROM work_executions WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?,
        None => None,
    };
    let (since, anchor_id) = anchor.unwrap_or((acted_at, String::new()));
    let reset: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_executions x
                       WHERE x.work_item_id = ?1 AND x.kind <> 'progression' AND x.status = 'completed'
                         AND (CAST(x.created_at AS INTEGER) > CAST(?2 AS INTEGER)
                              OR (CAST(x.created_at AS INTEGER) = CAST(?2 AS INTEGER) AND x.id >= ?3)))",
        params![item, since, anchor_id],
        |row| row.get(0),
    )?;
    Ok((!reset).then_some(prior_execution))
}

pub(super) fn redispatch(
    conn: &Connection,
    pending: &mut PendingEvents,
    ctx: &VerbContext,
    payload: &RedispatchProposalPayload,
    pr_checker: &dyn PrStateChecker,
) -> VerbResult<Applied> {
    let verb = ProposalKind::Redispatch;
    let item = &ctx.item;
    ctx.scope(conn, &item.id)?;
    ctx.refuse_live_work(conn, verb)?;

    // Holds that are the owner's: an open question, and a recorded refusal.
    let question_open: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM operator_questions WHERE work_item_id = ?1 AND status = 'open')",
        [&item.id],
        |row| row.get(0),
    )?;
    if question_open || item.blocked_reason.as_deref() == Some("awaiting_operator_answer") {
        return Err(ProgressionVerbRefusal::AwaitingOperatorAnswer { item: item.id.clone() }.into());
    }
    if item.blocked_reason.as_deref() == Some("worker_failed") {
        match crate::stuck_report::latest_basis(conn, &item.id, "blocked_reason")?.as_deref() {
            Some("operator_no") => {
                return Err(ProgressionVerbRefusal::OperatorRefusal {
                    verb: verb.to_string(),
                    item: item.id.clone(),
                }
                .into());
            }
            Some("unknown_legacy") => {
                return Err(ProgressionVerbRefusal::ProvenanceUnresolved { item: item.id.clone() }.into());
            }
            _ => {}
        }
    }

    ctx.refuse_repeat(conn, verb, "")?;
    if let Some(prior_execution) = redispatch_bound(conn, &item.id, &ctx.stuck_kind)? {
        return Err(ProgressionVerbRefusal::RedispatchBound {
            item: item.id.clone(),
            stuck_kind: ctx.stuck_kind.clone(),
            prior_execution,
        }
        .into());
    }

    // The addendum quotes a declared blocker and the evidence it is stale, in the
    // brief the next worker reads, so that worker does not re-declare it.
    if let Some(addendum) = payload.brief_addendum.as_deref() {
        let description = format!(
            "{}\n\n## Progression note ({})\n\n{}\n",
            item.description,
            ctx.execution_id,
            addendum.trim()
        );
        via_engine_path(
            verb,
            crate::description_guard::validate_description_update(&item.description, &description, false),
        )?;
        conn.execute(
            "UPDATE tasks SET description = ?2, updated_at = ?3 WHERE id = ?1",
            params![item.id, description, now_string()],
        )?;
    }

    let execution = if item.status == TaskStatus::InReview {
        // A dead AI review on an open PR: restart it through the path the review
        // sweep and `bossctl review start` use, and release the churn-guard
        // attention that parked it, as an explicit start does.
        resolve_attention_kind_in_tx(conn, &item.id, CHURN_GUARD_PARKED_ATTENTION_KIND)?;
        via_engine_path(verb, request_pr_review_in_tx(conn, &item.id, pr_checker))?
    } else {
        // An explicit start: it clears `churn_guard`, resolves a `deliberate_park`
        // attention, and lifts a non-refusal `worker_failed` block, all in this
        // transaction (`request_execution_in_tx_with_live_check`).
        let execution = via_engine_path(
            verb,
            crate::dispatch_helpers::request_execution_in_tx_as_actor(
                pending,
                conn,
                RequestExecutionInput::builder()
                    .work_item_id(item.id.clone())
                    .explicit_start(true)
                    .build(),
                |_| true,
                PROGRESSION,
            ),
        )?;
        conn.execute(
            "UPDATE tasks SET last_status_actor = ?2 WHERE id = ?1",
            params![item.id, PROGRESSION],
        )?;
        execution
    };
    // The churn guard's window and the brief's redispatch allowance count by this
    // stamp; a reused row keeps its own provenance.
    conn.execute(
        "UPDATE work_executions SET created_via = ?2 WHERE id = ?1 AND created_via IS NULL",
        params![execution.id, ctx.created_via],
    )?;
    stage_task_board_change(pending, conn, &item.id)?;
    Ok(Applied {
        subject: String::new(),
        result_ref: Some(execution.id),
        signal_retirement: None,
    })
}

// ── clear-signal ─────────────────────────────────────────────────────────────

pub(super) fn clear_signal(
    conn: &Connection,
    pending: &mut PendingEvents,
    ctx: &VerbContext,
    payload: &ClearSignalProposalPayload,
) -> VerbResult<Applied> {
    let verb = ProposalKind::ClearSignal;
    let item = &ctx.item;
    ctx.scope(conn, &item.id)?;
    let precondition = |message: String| -> VerbError {
        ProgressionVerbRefusal::Precondition {
            verb: verb.to_string(),
            message,
        }
        .into()
    };
    let reason = payload.reason.as_str();
    ctx.refuse_repeat(conn, verb, reason)?;
    let is_ci = reason.starts_with("ci_failure");
    // The two CI reasons share one retire path and one probe.
    let blocked_on = item.blocked_reason.as_deref().filter(|blocked| {
        item.status == TaskStatus::Blocked && (*blocked == reason || (is_ci && blocked.starts_with("ci_failure")))
    });
    if blocked_on.is_none() {
        return Err(precondition(format!(
            "{} is not `blocked: {reason}` (status `{}`, reason `{}`)",
            item.id,
            item.status,
            item.blocked_reason.as_deref().unwrap_or("none")
        )));
    }
    let Some(pr_url) = item.pr_url.as_deref().filter(|url| !url.is_empty()) else {
        return Err(precondition(format!("{} has no PR to probe", item.id)));
    };
    ctx.refuse_live_work(conn, verb)?;

    // The probe is the engine's last stored observation of the PR (what the merge
    // poller wrote). The retire path below is the poller's own, so the verb can
    // only do what the poller would have done had it seen what is stored.
    let (mergeable, ci): (Option<String>, Option<String>) = conn.query_row(
        "SELECT pr_mergeable_state, ci_required_state FROM tasks WHERE id = ?1",
        [&item.id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (observed, required, clear) = if is_ci {
        let observed = ci.unwrap_or_else(|| "unknown".into());
        let clear = observed == "success";
        (observed, "success", clear)
    } else {
        let observed = mergeable.unwrap_or_else(|| "unknown".into());
        let clear = observed.eq_ignore_ascii_case("MERGEABLE");
        (observed, "MERGEABLE", clear)
    };
    if !clear {
        return Err(ProgressionVerbRefusal::ProbeDisagrees {
            item: item.id.clone(),
            reason: reason.to_owned(),
            observed,
            required: required.to_owned(),
        }
        .into());
    }

    let retirement = match crate::signal_retirement::clear_resolved_signal_in_tx(
        conn,
        pending,
        &item.id,
        pr_url,
        is_ci,
        PROGRESSION,
    )? {
        crate::ClearResolvedSignal::Applied(retirement) => *retirement,
        crate::ClearResolvedSignal::QueueSideUnresolved { .. } => {
            return Err(precondition(format!(
                "{} has an unresolved queue-side failure; head CI is not evidence it cleared",
                item.id
            )));
        }
        crate::ClearResolvedSignal::Nothing => {
            return Err(precondition(format!(
                "{} changed underneath the clear; nothing was written",
                item.id
            )));
        }
    };
    stage_task_board_change(pending, conn, &item.id)?;
    Ok(Applied {
        subject: reason.to_owned(),
        result_ref: Some(item.id.clone()),
        signal_retirement: Some(retirement),
    })
}

// ── archive-revision ─────────────────────────────────────────────────────────

pub(super) fn archive_revision(
    conn: &Connection,
    pending: &mut PendingEvents,
    ctx: &VerbContext,
    payload: &ArchiveRevisionProposalPayload,
) -> VerbResult<Applied> {
    let verb = ProposalKind::ArchiveRevision;
    let target = via_engine_path(verb, resolve_target(conn, &payload.revision))?;
    ctx.scope(conn, &target)?;
    let precondition = |message: String| -> VerbError {
        ProgressionVerbRefusal::Precondition {
            verb: verb.to_string(),
            message,
        }
        .into()
    };
    if target == ctx.item.id {
        return Err(precondition(
            "never the item itself; ask the owner if the item should go".into(),
        ));
    }
    let revision = query_task(conn, &target)?
        .filter(|task| task.kind == TaskKind::Revision)
        .ok_or_else(|| ProgressionVerbRefusal::NotAChainRevision { target: target.clone() })?;
    let root = chain_root(conn, &ctx.item.id)?;
    if chain_root(conn, &revision.id)? != root {
        return Err(ProgressionVerbRefusal::NotAChainRevision { target }.into());
    }
    if revision.deleted_at.is_some() || revision.status.is_terminal() {
        return Err(precondition(format!("{target} is already `{}`", revision.status)));
    }
    if let Some((execution, status)) = live_or_pending_execution(conn, &revision.id)? {
        return Err(ProgressionVerbRefusal::LiveExecution {
            verb: verb.to_string(),
            item: revision.id,
            execution,
            status,
        }
        .into());
    }
    let parent_green =
        query_task(conn, &root)?.is_some_and(|root| root.ci_required_state.as_deref() == Some("success"));
    if !parent_green {
        return Err(precondition(format!(
            "the parent PR ({root}) is not green, so {target} may still be doing needed work"
        )));
    }
    ctx.refuse_repeat(conn, verb, &target)?;

    let now = now_string();
    let archived = archive_revision_task_as_actor(
        conn,
        &revision.id,
        &now,
        ArchiveProvenance::new(ARCHIVE_MECHANISM_PROGRESSION, payload.reason.trim()),
        PROGRESSION,
    )?;
    if archived == 0 {
        return Err(precondition(format!(
            "{target} changed underneath the archive; nothing was written"
        )));
    }
    // Later revisions queued behind the dead one are released.
    cascade_dependents_after_prereq_status_change(pending, conn, &revision.id, "archived", &now)?;
    stage_task_board_change(pending, conn, &revision.id)?;
    stage_task_board_change(pending, conn, &ctx.item.id)?;
    Ok(Applied {
        result_ref: Some(revision.id),
        subject: target,
        signal_retirement: None,
    })
}

// ── unblock ──────────────────────────────────────────────────────────────────

pub(super) fn unblock(conn: &Connection, pending: &mut PendingEvents, ctx: &VerbContext) -> VerbResult<Applied> {
    let verb = ProposalKind::Unblock;
    let item = &ctx.item;
    ctx.scope(conn, &item.id)?;
    let precondition = |message: String| -> VerbError {
        ProgressionVerbRefusal::Precondition {
            verb: verb.to_string(),
            message,
        }
        .into()
    };
    if item.status != TaskStatus::Blocked {
        return Err(precondition(format!(
            "{} is `{}`; the verb moves only a `blocked` item to `todo` or `in_review`",
            item.id, item.status
        )));
    }
    match item.blocked_reason.as_deref() {
        // A dependency block is stale once nothing gates the item any more.
        Some("dependency") => {
            if !deps::gating_prereqs_for(conn, &item.id)?.is_empty() {
                return Err(precondition(format!(
                    "{} is still gated by an unfinished prerequisite",
                    item.id
                )));
            }
        }
        // A cleared block (`clear-signal` leaves none) is the verb's case, but a
        // block a human placed with no reason is the owner's decision.
        None => {
            if matches!(item.last_status_actor.as_str(), "human" | "boss") {
                return Err(precondition(format!(
                    "{} was blocked by a person; that is their decision",
                    item.id
                )));
            }
        }
        Some("merge_conflict" | "ci_failure" | "ci_failure_exhausted") => {
            return Err(precondition(format!(
                "{} is still `blocked: {}`; use `clear-signal` once the PR probe is clear",
                item.id,
                item.blocked_reason.as_deref().unwrap_or_default()
            )));
        }
        Some("awaiting_operator_answer" | "deletion_signoff") => {
            return Err(precondition(format!("{} is waiting on the owner", item.id)));
        }
        Some(other) => {
            return Err(precondition(format!(
                "{} is still `blocked: {other}`; use `redispatch` to restart the work",
                item.id
            )));
        }
    }
    ctx.refuse_live_work(conn, verb)?;
    ctx.refuse_repeat(conn, verb, "")?;
    // A primary item that already owns a bound PR cannot rest in `todo`:
    // `insert_fresh` skips minting and an explicit start bails with "use a
    // revision". The dependency helper already picks `in_review` vs `todo`
    // and reconciles a waiting execution; a cleared-reason unblock does the
    // same rest-state pick so the item is not stranded.
    if item.blocked_reason.as_deref() == Some("dependency") {
        let lifted = crate::dep_helpers::maybe_engine_unblock_dependent_as_actor(
            pending,
            conn,
            &item.id,
            &now_string(),
            PROGRESSION,
        )?;
        if !lifted {
            return Err(precondition(format!(
                "{} changed underneath the unblock; nothing was written",
                item.id
            )));
        }
    } else {
        let rest = if crate::dispatch_admission::primary_pr_awaits_review(conn, &item.id)? {
            "in_review"
        } else {
            "todo"
        };
        let rows = crate::task_status_audit::execute(
            conn,
            "progression_unblock",
            PROGRESSION,
            "UPDATE tasks
             SET status = ?4, blocked_reason = NULL, blocked_detail = NULL, blocked_attempt_id = NULL,
                 last_status_actor = ?3, updated_at = ?2
             WHERE id = ?1 AND status = 'blocked' AND deleted_at IS NULL",
            params![item.id, now_string(), PROGRESSION, rest],
        )?;
        if rows == 0 {
            return Err(precondition(format!(
                "{} changed underneath the unblock; nothing was written",
                item.id
            )));
        }
        stage_task_board_change(pending, conn, &item.id)?;
    }
    Ok(Applied {
        subject: String::new(),
        result_ref: Some(item.id.clone()),
        signal_retirement: None,
    })
}
