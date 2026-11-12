//! Deliver durable owner decisions through the existing merge mechanisms.

use crate::work::{ConflictResolution, WorkDb};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boss_execution_contracts::ExecutionPublisher;
use boss_protocol::WorkItem;

#[async_trait]
pub(crate) trait DecisionActions: Send + Sync {
    async fn close(&self, pr_url: &str) -> Result<()>;
}

pub(crate) struct GithubActions;

#[async_trait]
impl DecisionActions for GithubActions {
    async fn close(&self, pr_url: &str) -> Result<()> {
        // Shared GitHub transport retains quota accounting and command bounds.
        boss_github::gh_runner::run_gh(&["pr", "close", pr_url], "close owner-approved PR").await?;
        Ok(())
    }
}

pub(crate) async fn run(
    db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    actions: &dyn DecisionActions,
    merge: std::sync::Arc<dyn crate::merge_when_ready::DirectMergeExecutor>,
) {
    let pending = match db.pending_conflict_inventory_actions() {
        Ok(pending) => pending,
        Err(err) => {
            tracing::warn!(?err, "cannot read pending conflict decisions");
            return;
        }
    };
    for (id, choice, attempt) in pending {
        let result = apply(db, publisher, actions, merge.as_ref(), &id, &choice, &attempt).await;
        if let Err(err) = result {
            let message = format!("{err:#}");
            if let Err(record_err) = db.record_conflict_inventory_action(&id, Some(&message)) {
                tracing::error!(?record_err, "cannot record conflict decision failure");
            }
            let _ = db.upsert_external_tracker_attention(
                &attempt.work_item_id,
                "conflict_inventory_action_failed",
                "PR decision needs attention",
                &message,
            );
            tracing::warn!(?err, pr_url = %attempt.pr_url, "owner conflict decision failed; retained for retry");
        }
        publisher
            .publish_work_item_changed(
                &attempt.product_id,
                &attempt.work_item_id,
                "conflict_inventory_decision",
            )
            .await;
    }
}

async fn apply(
    db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    actions: &dyn DecisionActions,
    merge: &dyn crate::merge_when_ready::DirectMergeExecutor,
    id: &str,
    choice: &str,
    attempt: &ConflictResolution,
) -> Result<()> {
    let item = db.get_work_item(&attempt.work_item_id)?;
    let task = match item {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        _ => anyhow::bail!("PR owner is not a task"),
    };
    anyhow::ensure!(
        task.pr_url.as_deref() == Some(&attempt.pr_url) && task.deleted_at.is_none(),
        "the PR binding changed after the decision"
    );
    if !db.live_writers_in_chain(&task.id)?.is_empty() {
        return Ok(());
    }
    if choice == "Close" {
        actions.close(&attempt.pr_url).await?;
        db.record_conflict_inventory_action(id, None)?;
        db.resolve_external_tracker_attention(&attempt.work_item_id, "conflict_inventory_action_failed")?;
        return Ok(());
    }
    // A completed conflict fix alone is insufficient: review revisions must
    // also finish before requesting the ordinary checked merge.
    if task.status != boss_protocol::TaskStatus::InReview || !db.open_merge_revisions(&task.id)?.is_empty() {
        return Ok(());
    }
    let product = db.get_product(&attempt.product_id)?.context("missing product")?;
    match crate::merge_mechanism::MergeMechanism::parse(product.merge_mechanism.as_deref())? {
        crate::merge_mechanism::MergeMechanism::Direct => {
            let result =
                crate::merge_when_ready::execute_direct_merge(db, merge, &task.id, &product, &attempt.pr_url).await?;
            let record = crate::merge_when_ready::record_direct_merge(db, &task.id, &attempt.pr_url, &result)?;
            crate::merge_when_ready::publish_direct_merge(
                db,
                publisher,
                &attempt.product_id,
                &task.id,
                &attempt.pr_url,
                record.merge_queue_state_changed,
            )
            .await;
            db.record_conflict_inventory_action(id, None)?;
        }
        crate::merge_mechanism::MergeMechanism::TrunkQueue { target_branch } => {
            let coords = boss_engine_merge_poller::trunk_merge::parse_trunk_pr_coordinates(&attempt.pr_url)?;
            db.queue_conflict_inventory_trunk_merge(
                id,
                crate::work::TrunkMergeIntentInsertInput::builder()
                    .work_item_id(&task.id)
                    .pr_url(&attempt.pr_url)
                    .pr_number(coords.number as i64)
                    .repo(format!("{}/{}", coords.owner, coords.repo))
                    .target_branch(target_branch)
                    .build(),
            )?;
        }
    }
    db.resolve_external_tracker_attention(&attempt.work_item_id, "conflict_inventory_action_failed")?;
    Ok(())
}

#[cfg(test)]
#[path = "conflict_inventory_actions_tests.rs"]
mod tests;
