//! Assemble a [`ProgressionBriefPacket`] from stored work-db state.

use anyhow::{Context, Result, bail};
use boss_engine_storage_values::attention_kinds::NUDGE_BREAKER_ATTENTION_KIND;
use boss_protocol::{
    DependencyDirection, ListDependenciesInput, OperatorQuestionRecord, Task, WorkItem, WorkItemDependencyDetail,
};
use boss_work_db::{CHURN_GUARD_WINDOW_SECS, ExecutionDiagnosis, ExecutionKind, WorkDb};

use crate::packet::{
    DeclaredBlocker, Neighbour, PriorProgressionRun, ProgressionBriefPacket, VerbBudget, compute_stuck_fingerprint,
};

/// Load the diagnosis packet for `work_item_id`. All reads are stored state;
/// nothing talks to git or GitHub. A failed read fails the whole gather so the
/// caller falls back, rather than rendering the gap as an empty section.
pub fn gather_progression_brief(db: &WorkDb, work_item_id: &str) -> Result<ProgressionBriefPacket> {
    let item = task_from_work_item(db, work_item_id)?;
    let product_name = db
        .get_product(&item.product_id)?
        .map(|product| product.name)
        .unwrap_or_else(|| item.product_id.clone());
    let project_name = match item.project_id.as_deref() {
        Some(project_id) => Some(db.get_project(project_id).context("load project")?.name),
        None => None,
    };

    let clauses = db.progression_clauses(work_item_id)?;
    let stuck_kind = clauses
        .as_ref()
        .and_then(|row| row.stuck_kind.clone())
        .unwrap_or_else(|| fallback_stuck_kind(&item));
    let stuck_since = clauses.as_ref().map(|row| row.stuck_since);

    let ledger = db.list_recent_status_audit(work_item_id, 3)?;
    let failure_basis = if item.blocked_reason.as_deref() == Some("worker_failed") {
        db.latest_status_audit_basis(work_item_id, "blocked_reason")?
    } else {
        None
    };

    let chain_root = db.review_cycle_root_id(work_item_id);
    let chain_ids = db.list_chain_work_item_ids(&chain_root)?;
    let deps = db.list_dependencies_detailed(ListDependenciesInput {
        work_item: work_item_id.to_owned(),
        direction: Some(DependencyDirection::Both),
    })?;
    let prerequisites = neighbours(db, &deps, true)?;
    let dependents = neighbours(db, &deps, false)?;

    let executions = db.list_execution_diagnoses_for_chain(&chain_root)?;
    let now = now_epoch_secs();
    let churn_window_terminal_count =
        db.count_recent_terminal_executions(work_item_id, now.saturating_sub(CHURN_GUARD_WINDOW_SECS), None)?;

    let attentions = db.list_attention_items_for_work_items(&chain_ids)?;
    let mut questions = Vec::new();
    for id in &chain_ids {
        questions.extend(db.list_operator_questions(id).context("load operator questions")?);
    }
    let declared_blocker = declared_blocker_from(db, &item, &failure_basis, &executions, &attentions, &questions)?;

    // A revision never carries a PR: the PR, its poll snapshot and its CI state
    // live on the chain root.
    let pr_item = if chain_root == work_item_id {
        None
    } else {
        Some(task_from_work_item(db, &chain_root)?)
    };
    let pr_source = pr_item.as_ref().unwrap_or(&item);
    let snapshot = db.get_pr_status_snapshot(&pr_source.id)?;
    let conflict_attempts = db.list_conflict_resolutions(None, &[], Some(&pr_source.id), None)?;
    let ci_attempts = db.list_ci_remediations(None, &[], Some(&pr_source.id), None)?;
    let other_pr_ci = db.list_product_pr_ci_snapshots(&item.product_id, &pr_source.id)?;

    let prior_progression = executions
        .iter()
        .filter(|row| row.kind == ExecutionKind::Progression.as_str() && is_terminal_status(&row.status))
        .map(|row| {
            Ok(PriorProgressionRun {
                fingerprint: db.get_metadata(&format!("progression.fingerprint.{}", row.id))?,
                actions: db
                    .list_worker_proposals(
                        Some(&row.id),
                        None,
                        None,
                        Some(boss_protocol::ProposalState::Applied),
                        None,
                    )?
                    .into_iter()
                    .map(|p| format!("{}: {}", p.kind, p.payload_json))
                    .collect(),
                execution_id: row.id.clone(),
                status: row.status.clone(),
                created_at: row.created_at.clone(),
                run_done_outcome: row.run_done_outcome.clone(),
                last_error: row.last_error.clone().or_else(|| row.latest_run_error.clone()),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // The fingerprint must not move because a progression run ended: only work
    // runs count as "something happened since".
    let latest_terminal = executions
        .iter()
        .rev()
        .filter(|row| row.kind != ExecutionKind::Progression.as_str())
        .find(|row| is_terminal_status(&row.status))
        .map(|row| row.id.as_str());
    let pr_head_sha = snapshot.as_ref().and_then(|row| row.head_sha.clone());
    let fingerprint = compute_stuck_fingerprint(
        &stuck_kind,
        item.blocked_reason.as_deref(),
        pr_head_sha.as_deref(),
        latest_terminal,
    );

    let create_budget = db.progression_create_budget(work_item_id)?;
    let budget = VerbBudget {
        create_used: create_budget.used,
        create_max: create_budget.max,
        create_window_secs: create_budget.window_secs,
        redispatch_remaining: redispatch_remaining(db, &executions, work_item_id, &stuck_kind)?,
    };

    Ok(ProgressionBriefPacket {
        product_name,
        project_name,
        stuck_kind,
        stuck_since,
        failure_basis,
        ledger,
        prerequisites,
        dependents,
        executions,
        churn_window_terminal_count,
        declared_blocker,
        pr_url: pr_source.pr_url.clone(),
        pr_owner: pr_item.as_ref().map(|root| root.id.clone()),
        pr_mergeable: snapshot.as_ref().and_then(|row| row.mergeable.clone()),
        pr_merge_state_status: snapshot.as_ref().and_then(|row| row.merge_state_status.clone()),
        pr_head_sha,
        pr_observed_at: snapshot.as_ref().and_then(|row| row.observed_at.clone()),
        ci_required_state: pr_source.ci_required_state.clone(),
        ci_required_detail: pr_source.ci_required_detail.clone(),
        review_required_state: pr_source.review_required_state.clone(),
        conflict_attempts,
        ci_attempts,
        other_pr_ci,
        attentions,
        questions,
        prior_progression,
        fingerprint,
        budget,
        create_budget,
        item,
    })
}

/// One redispatch per item and stuck kind, reset by any completed work run on
/// that item, including work started by progression.
pub(crate) fn redispatch_remaining(
    db: &WorkDb,
    executions: &[ExecutionDiagnosis],
    item: &str,
    kind: &str,
) -> Result<u32> {
    for row in executions
        .iter()
        .rev()
        .filter(|row| row.work_item_id == item && row.kind != ExecutionKind::Progression.as_str())
    {
        if row.status == "completed" {
            return Ok(1);
        }
        if let Some(origin) = row
            .created_via
            .as_deref()
            .and_then(|via| via.strip_prefix("progression:"))
            && executions.iter().any(|producer| {
                producer.id == origin
                    && producer.work_item_id == item
                    && producer.kind == ExecutionKind::Progression.as_str()
            })
            && db.get_metadata(&format!("progression.stuck_kind.{origin}"))?.as_deref() == Some(kind)
        {
            return Ok(0);
        }
    }
    Ok(1)
}

fn task_from_work_item(db: &WorkDb, work_item_id: &str) -> Result<Task> {
    match db.get_work_item(work_item_id).context("load work item")? {
        WorkItem::Task(task) | WorkItem::Chore(task) => Ok(task),
        WorkItem::Project(project) => {
            bail!("progression brief is for a task or chore, not project {}", project.id)
        }
        WorkItem::Product(product) => {
            bail!("progression brief is for a task or chore, not product {}", product.id)
        }
    }
}

fn neighbours(db: &WorkDb, deps: &WorkItemDependencyDetail, prereqs: bool) -> Result<Vec<Neighbour>> {
    let edges = if prereqs { &deps.prerequisites } else { &deps.dependents };
    let mut out = Vec::with_capacity(edges.len());
    for edge in edges {
        let live = db
            .get_live_execution_for_work_item(&edge.id, "")
            .with_context(|| format!("load live execution for {}", edge.id))?
            .map(|execution| (execution.id, execution.kind.as_str().to_owned()));
        out.push(Neighbour {
            edge: edge.clone(),
            live_execution: live,
        });
    }
    Ok(out)
}

pub(crate) fn declared_blocker_from(
    db: &WorkDb,
    item: &Task,
    failure_basis: &Option<String>,
    executions: &[ExecutionDiagnosis],
    attentions: &[(String, boss_protocol::WorkAttentionItem)],
    questions: &[OperatorQuestionRecord],
) -> Result<Option<DeclaredBlocker>> {
    let parked = item.dispatch_failed_reason.as_deref() == Some("deliberate_park");
    if !parked && !matches!(failure_basis.as_deref(), Some("declared_blocked" | "nudge_breaker")) {
        return Ok(None);
    }
    let nudge = attentions
        .iter()
        .filter(|(owner, a)| owner == &item.id && a.status == "open" && a.kind == NUDGE_BREAKER_ATTENTION_KIND)
        .max_by_key(|(_, a)| (&a.created_at, &a.id));
    let blocked = executions.iter().rev().find(|e| {
        e.work_item_id == item.id
            && e.kind != ExecutionKind::Progression.as_str()
            && e.run_done_outcome.as_deref() == Some("blocked")
    });
    let use_nudge = failure_basis.as_deref() == Some("nudge_breaker")
        || (parked
            && nudge.is_some()
            && nudge.is_some_and(|(_, a)| blocked.is_none_or(|e| a.created_at >= e.created_at)));
    let execution_id = if use_nudge {
        nudge.and_then(|(_, a)| a.execution_id.as_deref())
    } else {
        blocked.map(|e| e.id.as_str())
    };
    let mut verbatim = None;
    if use_nudge {
        verbatim = nudge.map(|(_, a)| a.body_markdown.clone());
    } else if let Some(id) = execution_id {
        let proposals = db.list_worker_proposals(
            Some(id),
            Some(&item.id),
            None,
            Some(boss_protocol::ProposalState::Applied),
            None,
        )?;
        for p in proposals {
            let value: serde_json::Value = serde_json::from_str(&p.payload_json)?;
            if (p.kind == boss_protocol::ProposalKind::RunDone && value["outcome"] == "blocked")
                || p.kind == boss_protocol::ProposalKind::Blocked
            {
                let mut words = Vec::new();
                for field in ["summary", "reason"] {
                    if let Some(text) = value[field].as_str() {
                        words.push(text.to_owned());
                    }
                }
                if let Some(q) = value.get("question").filter(|q| !q.is_null()) {
                    words.push(serde_json::to_string_pretty(q)?);
                }
                if !words.is_empty() {
                    verbatim = Some(words.join("\n\n"));
                    break;
                }
            }
        }
    }
    let question = questions
        .iter()
        .filter(|q| {
            q.work_item_id == item.id
                && q.status == boss_protocol::OperatorQuestionStatus::Open
                && execution_id.is_none_or(|id| q.question.execution_id == id)
        })
        .max_by_key(|q| (&q.question.asked_at, &q.question.id));
    let verbatim = verbatim
        .or_else(|| question.map(|q| format!("{}\n\n{}", q.question.text, q.question.explanation)))
        .unwrap_or_else(|| "(no worker declaration stored)".to_owned());
    Ok(Some(DeclaredBlocker {
        producer: if use_nudge { "nudge_breaker" } else { "declared_blocked" }.to_owned(),
        verbatim,
        sweep_note: item
            .dispatch_failed_error
            .clone()
            .or_else(|| item.blocked_detail.clone()),
    }))
}

/// The candidate view's label when the item still has one. An item that has
/// left the view is not classified: the raw reason stays on its own line and
/// the sentinel keeps the fingerprint from hashing a label the view never emits.
fn fallback_stuck_kind(item: &Task) -> String {
    let label = match (item.dispatch_failed_reason.as_deref(), item.blocked_reason.as_deref()) {
        (Some("churn_guard"), _) => Some("S2_churn_guard"),
        (Some("deliberate_park"), _) => Some("S3_deliberate_park"),
        (Some(_), _) => Some("S1_dispatch_failure"),
        (None, Some("worker_failed")) => Some("S7_worker_failed"),
        (None, Some("execution_recovery_failed")) => Some("S9_execution_recovery"),
        (None, Some("spawning_attempt_retired_unsuccessfully")) => Some("S10_retired_attempt"),
        (None, Some("merge_conflict")) => Some("S5_merge_conflict"),
        (None, Some("ci_failure" | "ci_failure_exhausted")) => Some("S6_ci_failure"),
        (None, Some("pr_binding_unresolved")) => Some("S11_pr_binding"),
        _ => None,
    };
    label.unwrap_or(NO_LONGER_CLASSIFIED).to_owned()
}

/// Stuck-kind sentinel for an item the candidate view no longer classifies.
pub const NO_LONGER_CLASSIFIED: &str = "(no longer classified)";

fn is_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "abandoned" | "cancelled" | "orphaned")
}

fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
