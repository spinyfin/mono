//! Assemble a [`ProgressionBriefPacket`] from stored work-db state.

use anyhow::{Context, Result, bail};
use boss_engine_storage_values::attention_kinds::{NUDGE_BREAKER_ATTENTION_KIND, RUN_DONE_BLOCKED_ATTENTION_KIND};
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
    let declared_blocker = declared_blocker_from(&item, &failure_basis, &attentions, &questions);

    // A revision never carries a PR: the PR, its poll snapshot and its CI state
    // live on the chain root.
    let pr_item = if chain_root == work_item_id {
        None
    } else {
        Some(task_from_work_item(db, &chain_root)?)
    };
    let pr_source = pr_item.as_ref().unwrap_or(&item);
    let snapshot = db.get_pr_status_snapshot(&pr_source.id)?;
    let conflict_attempts = db.list_conflict_resolutions(None, &[], Some(work_item_id), None)?;
    let ci_attempts = db.list_ci_remediations(None, &[], Some(work_item_id), None)?;
    let other_pr_ci = db.list_product_pr_ci_snapshots(&item.product_id, &pr_source.id)?;

    let prior_progression = executions
        .iter()
        .filter(|row| row.kind == ExecutionKind::Progression.as_str())
        .map(|row| PriorProgressionRun {
            execution_id: row.id.clone(),
            status: row.status.clone(),
            created_at: row.created_at.clone(),
            run_done_outcome: row.run_done_outcome.clone(),
            last_error: row.last_error.clone().or_else(|| row.latest_run_error.clone()),
        })
        .collect();

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
        redispatch_remaining: redispatch_remaining(&executions),
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

/// The 2.5 bound of one progression redispatch per item, less the work runs a
/// progression run has started since the last work run that was not itself a
/// progression redispatch and reached `completed`.
pub(crate) fn redispatch_remaining(executions: &[ExecutionDiagnosis]) -> u32 {
    let from_progression = |row: &ExecutionDiagnosis| {
        row.created_via
            .as_deref()
            .is_some_and(|via| via.starts_with("progression:"))
    };
    let is_work = |row: &&ExecutionDiagnosis| row.kind != ExecutionKind::Progression.as_str();
    let last_clean = executions
        .iter()
        .rposition(|row| is_work(&row) && !from_progression(row) && row.status == "completed");
    let since = &executions[last_clean.map_or(0, |index| index + 1)..];
    let used = since.iter().filter(is_work).filter(|row| from_progression(row)).count();
    1u32.saturating_sub(u32::try_from(used).unwrap_or(u32::MAX))
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

fn declared_blocker_from(
    item: &Task,
    failure_basis: &Option<String>,
    attentions: &[(String, boss_protocol::WorkAttentionItem)],
    questions: &[OperatorQuestionRecord],
) -> Option<DeclaredBlocker> {
    let parked = item.dispatch_failed_reason.as_deref() == Some("deliberate_park");
    let producer = if parked {
        // The park note does not say which producer parked the item; the open
        // declaration attention does.
        attentions
            .iter()
            .filter(|(_, attention)| attention.status == "open")
            .find_map(|(_, attention)| producer_for_kind(&attention.kind))
            .unwrap_or("deliberate_park")
    } else {
        failure_basis
            .as_deref()
            .filter(|basis| matches!(*basis, "declared_blocked" | "nudge_breaker"))?
    };
    let attention_kind = match producer {
        "nudge_breaker" => NUDGE_BREAKER_ATTENTION_KIND,
        _ => RUN_DONE_BLOCKED_ATTENTION_KIND,
    };
    let from_attention = attentions
        .iter()
        .rev()
        .find(|(_, attention)| attention.kind == attention_kind && attention.status == "open")
        .map(|(_, attention)| attention.body_markdown.clone());
    let from_question = || {
        questions.last().map(|record| {
            let explanation = record.question.explanation.trim();
            if explanation.is_empty() {
                record.question.text.clone()
            } else {
                format!("{}\n\n{explanation}", record.question.text)
            }
        })
    };
    let verbatim = from_attention
        .or_else(from_question)
        .or_else(|| item.blocked_detail.clone())
        .unwrap_or_else(|| "(no blocker text stored)".to_owned());
    Some(DeclaredBlocker {
        producer: producer.to_owned(),
        verbatim,
        sweep_note: if parked {
            item.dispatch_failed_error.clone()
        } else {
            None
        },
    })
}

fn producer_for_kind(kind: &str) -> Option<&'static str> {
    match kind {
        NUDGE_BREAKER_ATTENTION_KIND => Some("nudge_breaker"),
        RUN_DONE_BLOCKED_ATTENTION_KIND => Some("declared_blocked"),
        _ => None,
    }
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
