//! Assemble a [`ProgressionBriefPacket`] from stored work-db state.

use anyhow::{Context, Result, bail};
use boss_engine_storage_values::attention_kinds::{NUDGE_BREAKER_ATTENTION_KIND, RUN_DONE_BLOCKED_ATTENTION_KIND};
use boss_protocol::{DependencyDirection, ListDependenciesInput, Task, WorkItem, WorkItemDependencyDetail};
use boss_work_db::{CHURN_GUARD_WINDOW_SECS, ExecutionKind, WorkDb};

use crate::packet::{
    DeclaredBlocker, Neighbour, PriorProgressionRun, ProgressionBriefPacket, VerbBudget, compute_stuck_fingerprint,
};

/// Load the diagnosis packet for `work_item_id`. All reads are stored state;
/// nothing talks to git or GitHub.
pub fn gather_progression_brief(db: &WorkDb, work_item_id: &str) -> Result<ProgressionBriefPacket> {
    let item = task_from_work_item(db, work_item_id)?;
    let product_name = db
        .get_product(&item.product_id)?
        .map(|product| product.name)
        .unwrap_or_else(|| item.product_id.clone());
    let project_name = item
        .project_id
        .as_deref()
        .and_then(|project_id| db.get_project(project_id).ok().map(|project| project.name));

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

    let attentions = db.list_attention_items_for_work_item(work_item_id).unwrap_or_default();
    let questions = db.list_operator_questions(work_item_id).unwrap_or_default();
    let declared_blocker = declared_blocker_from(&item, &failure_basis, &attentions);

    let snapshot = db.get_pr_status_snapshot(work_item_id)?;
    let conflict_attempts = db
        .list_conflict_resolutions(None, &[], Some(work_item_id), None)
        .unwrap_or_default();
    let ci_attempts = db
        .list_ci_remediations(None, &[], Some(work_item_id), None)
        .unwrap_or_default();
    let other_pr_ci = db
        .list_product_pr_ci_snapshots(&item.product_id, work_item_id)
        .unwrap_or_default();

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

    let latest_terminal = executions
        .iter()
        .rev()
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
        redispatch_remaining: 1,
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
        pr_url: item.pr_url.clone(),
        pr_mergeable: snapshot.as_ref().and_then(|row| row.mergeable.clone()),
        pr_merge_state_status: snapshot.as_ref().and_then(|row| row.merge_state_status.clone()),
        pr_head_sha,
        pr_observed_at: snapshot.as_ref().and_then(|row| row.observed_at.clone()),
        ci_required_state: item.ci_required_state.clone(),
        ci_required_detail: item.ci_required_detail.clone(),
        review_required_state: item.review_required_state.clone(),
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
            .ok()
            .flatten()
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
    attentions: &[boss_protocol::WorkAttentionItem],
) -> Option<DeclaredBlocker> {
    if item.dispatch_failed_reason.as_deref() == Some("deliberate_park") {
        return Some(DeclaredBlocker {
            producer: "deliberate_park".to_owned(),
            verbatim: item
                .dispatch_failed_error
                .clone()
                .unwrap_or_else(|| "(no dispatch_failed_error stored)".to_owned()),
        });
    }
    let producer = failure_basis
        .as_deref()
        .filter(|basis| matches!(*basis, "declared_blocked" | "nudge_breaker"));
    if let Some(producer) = producer {
        let attention_kind = if producer == "nudge_breaker" {
            NUDGE_BREAKER_ATTENTION_KIND
        } else {
            RUN_DONE_BLOCKED_ATTENTION_KIND
        };
        let from_attention = attentions
            .iter()
            .find(|item| item.kind == attention_kind && item.status == "open")
            .map(|item| item.body_markdown.clone());
        let verbatim = from_attention
            .or_else(|| item.blocked_detail.clone())
            .unwrap_or_else(|| "(no blocker text stored)".to_owned());
        return Some(DeclaredBlocker {
            producer: producer.to_owned(),
            verbatim,
        });
    }
    None
}

fn fallback_stuck_kind(item: &Task) -> String {
    if let Some(reason) = item.dispatch_failed_reason.as_deref() {
        return match reason {
            "churn_guard" => "S2_churn_guard".to_owned(),
            "deliberate_park" => "S3_deliberate_park".to_owned(),
            other => format!("S1_dispatch_failure:{other}"),
        };
    }
    if let Some(reason) = item.blocked_reason.as_deref() {
        let kind = match reason {
            "worker_failed" => "S7_worker_failed",
            "execution_recovery_failed" => "S9_execution_recovery",
            "merge_conflict" => "S5_merge_conflict",
            "ci_failure" | "ci_failure_exhausted" => "S6_ci_failure",
            "pr_binding_unresolved" => "S11_pr_binding",
            "dependency" => "S4_dependency",
            other => other,
        };
        return format!("{kind}:{reason}");
    }
    "unknown".to_owned()
}

fn is_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "abandoned" | "cancelled" | "orphaned")
}

fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
