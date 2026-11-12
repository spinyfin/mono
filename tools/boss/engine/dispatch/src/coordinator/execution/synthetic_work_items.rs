//! Synthetic `Chore` work items for executions whose `work_item_id` binds to
//! something other than a task (an automation, a comment, a review-guide
//! comparison). Part of the `execution` module split.
use super::*;

impl ExecutionCoordinator {
    /// Build the synthetic `Chore` work item for an `automation_triage`
    /// execution from the bound automation. `None` when the automation row is
    /// gone (deleted mid-flight) — the caller then falls back to the normal
    /// `get_work_item`, which fails cleanly.
    pub(super) fn synthetic_triage_work_item(&self, execution: &WorkExecution) -> Option<WorkItem> {
        let automation = self.work_db.get_automation(&execution.work_item_id).ok().flatten()?;
        let task = boss_protocol::Task::builder()
            .id(automation.id.clone())
            .product_id(automation.product_id.clone())
            .kind(TaskKind::Chore)
            .name(format!("Automation triage: {}", automation.name))
            .description(automation.standing_instruction.clone())
            .status(TaskStatus::Active)
            .repo_remote_url(execution.repo_remote_url.clone())
            .created_at(automation.created_at.clone())
            .updated_at(automation.updated_at.clone())
            .build();
        Some(WorkItem::Chore(task))
    }

    /// Build the synthetic `Chore` work item for an `answer_agent`
    /// execution (P3b) from the bound comment and its resolved doc owner.
    /// `None` when the comment is gone, or its doc owner no longer resolves
    /// (both are the same "engine state raced under us mid-flight"
    /// tolerance `synthetic_triage_work_item` already applies — the caller
    /// falls back to the normal `get_work_item`, which fails cleanly).
    ///
    /// `product_id` is the doc owner task's product — needed for host
    /// capability resolution ([`Self::select_host_for_execution`]); `name`/
    /// `description` surface the question in cube's task label / change
    /// title. Like the triage synthetic item, these fields only feed spawn
    /// plumbing: the runner (P3b) composes the real answer-agent prompt
    /// separately, and the completion handler branches on `kind` to
    /// finalise the run instead of doing PR detection.
    pub(super) fn synthetic_answer_agent_work_item(&self, execution: &WorkExecution) -> Option<WorkItem> {
        let comment = self.work_db.get_comment(&execution.work_item_id).ok().flatten()?;
        let target = self
            .work_db
            .resolve_feedback_target(&comment.artifact_kind, &comment.artifact_id)
            .ok()
            .flatten()?;
        let owner_item = self.work_db.get_work_item(target.owner_task_id()).ok()?;
        let product_id = owner_item.product_id().to_string();
        let short_quote = if comment.body.chars().count() > 60 {
            format!("{}…", comment.body.chars().take(60).collect::<String>())
        } else {
            comment.body.clone()
        };
        let task = boss_protocol::Task::builder()
            .id(comment.id.clone())
            .product_id(product_id)
            .kind(TaskKind::Chore)
            .name(format!("Answer comment: {short_quote}"))
            .description(comment.body.clone())
            .status(TaskStatus::Active)
            .repo_remote_url(execution.repo_remote_url.clone())
            .created_at(comment.created_at.clone())
            .updated_at(comment.updated_at.clone())
            .build();
        Some(WorkItem::Chore(task))
    }

    /// Build the synthetic `Chore` work item for a `pr_review_guide`
    /// execution from its bound immutable comparison (`work_item_id` is a
    /// `pr_review_guide_source_comparisons.id`, per
    /// [`crate::work::WorkDb::create_pr_review_guide_execution`]). `None`
    /// when the comparison is gone (same engine-raced-under-us tolerance as
    /// the other synthetic builders). `product_id` is the owning root task's
    /// real product, needed for host capability resolution; `name`/
    /// `description` only feed cube's task label / change title — the
    /// runner composes the real review-guide prompt (with the embedded
    /// source packet) separately, and the completion handler branches on
    /// `kind` to run publication fencing instead of PR detection.
    pub(super) fn synthetic_review_guide_work_item(&self, execution: &WorkExecution) -> Option<WorkItem> {
        let capture = self
            .work_db
            .get_pr_review_guide_comparison_by_id(&execution.work_item_id)
            .ok()??;
        let root_item = self.work_db.get_work_item(&capture.root_task_id).ok()?;
        let task = boss_protocol::Task::builder()
            .id(capture.comparison_id.clone())
            .product_id(root_item.product_id().to_string())
            .kind(TaskKind::Chore)
            .name(format!("Review guide: {}", capture.packet.title))
            .description(format!(
                "Generate a review guide for {}",
                capture.packet.canonical_pr_url
            ))
            .status(TaskStatus::Active)
            .repo_remote_url(execution.repo_remote_url.clone())
            .created_at(capture.captured_at.clone())
            .updated_at(capture.captured_at.clone())
            .build();
        Some(WorkItem::Chore(task))
    }
}
