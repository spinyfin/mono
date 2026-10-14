//! Post-hoc audit of a queue-side CI-fix `delivered` declaration. The
//! declaration gate is purely local (it runs on the termination path), so the
//! facts it cannot check without GitHub are checked here, off that path:
//! the verified head is the PR's head and contains the stated `main_sha`, and
//! an unchanged-head requeue receipt names a live queue entry at that head.
//! A contradiction files the flagged attention; it never reverses completion.

use boss_protocol::QueueCiEvidence;

use super::*;

/// The first contradiction between the evidence and GitHub, if any.
/// Lookups that fail are skipped: an audit that cannot ask proves nothing.
async fn find_contradiction(
    branch_verifier: &dyn BranchVerifier,
    repo_slug: &str,
    pr_number: u64,
    attempt: &crate::work::CiRemediation,
    evidence: &QueueCiEvidence,
) -> Option<Contradiction> {
    if let Ok(head) = branch_verifier.fetch_pr_head_oid_fresh(repo_slug, pr_number).await
        && head != evidence.verified_head_sha
    {
        return Some(Contradiction::plain(format!(
            "`verified_head_sha` is `{}` but the PR head is `{head}`: the verified tree is not the branch that was pushed.",
            evidence.verified_head_sha
        )));
    }
    if let Ok(false) = branch_verifier
        .fetch_is_ancestor(repo_slug, &evidence.main_sha, &evidence.verified_head_sha)
        .await
    {
        return Some(Contradiction::plain(format!(
            "`main_sha` `{}` is not an ancestor of `verified_head_sha` `{}`: the run was not on a main-rebased tree.",
            evidence.main_sha, evidence.verified_head_sha
        )));
    }
    let receipt = evidence.queue_entry_id.as_deref()?;
    if attempt.failure_kind.as_deref() != Some("merge_queue_rebounce") {
        return None;
    }
    match branch_verifier.fetch_pr_merge_queue_entry(repo_slug, pr_number).await {
        Ok(Some((id, head))) if id == receipt && head == evidence.verified_head_sha => None,
        Ok(Some((id, head))) => Some(Contradiction::plain(format!(
            "the PR is queued under entry `{id}` at `{head}`, not the receipt `{receipt}` at `{}`.",
            evidence.verified_head_sha
        ))),
        Ok(None) => Some(Contradiction {
            detail: format!(
                "the requeue receipt `{receipt}` does not correspond to a live queue entry; the PR is not in the queue."
            ),
            rearm: true,
        }),
        Err(_) => None,
    }
}

struct Contradiction {
    detail: String,
    /// The retired attempt's failure signal should resurface: the PR was left
    /// out of the queue with no active attempt.
    rearm: bool,
}

impl Contradiction {
    fn plain(detail: String) -> Self {
        Self { detail, rearm: false }
    }
}

pub(super) async fn audit_queue_ci_delivery(
    branch_verifier: &dyn BranchVerifier,
    work_db: &crate::work::WorkDb,
    publisher: &dyn ExecutionPublisher,
    execution_id: &str,
    work_item_id: &str,
    repo_remote_url: &str,
    pr_url: &str,
) {
    let Ok(Some((attempt, evidence))) = work_db.queue_ci_delivery_for_audit(execution_id) else {
        return;
    };
    let (Ok(repo_slug), Some(pr_number)) = (parse_repo_slug(repo_remote_url), pr_number_from_url(pr_url)) else {
        return;
    };
    let Some(found) = find_contradiction(branch_verifier, &repo_slug, pr_number, &attempt, &evidence).await else {
        return;
    };
    tracing::warn!(execution_id, work_item_id, pr_url, detail = %found.detail, "queue CI evidence audit: delivery contradicted");
    if found.rearm
        && let Err(err) = work_db.rearm_blocked_ci_failure_signal(work_item_id)
    {
        tracing::warn!(
            execution_id,
            ?err,
            "queue CI evidence audit: failed to re-arm the failure signal"
        );
    }
    let body = format!(
        "This execution declared a verified queue-CI delivery for {pr_url}, and the declaration was accepted on local checks alone. A post-hoc audit against GitHub found: {}\n\n\
         The completion stands, but the queue-failure evidence is contradicted. Check that the PR is in the merge queue at the tested head and decide whether to re-run the verification.",
        found.detail
    );
    match work_db.create_attention_item(CreateAttentionItemInput {
        execution_id: Some(execution_id.to_owned()),
        work_item_id: None,
        kind: RUN_DONE_AUDIT_FLAGGED_ATTENTION_KIND.to_owned(),
        status: None,
        title: "Queue CI delivery evidence contradicted by GitHub".to_owned(),
        body_markdown: body,
        resolved_at: None,
    }) {
        Ok(item) => {
            if let Ok(work_item) = work_db.get_work_item(work_item_id) {
                let product_id = work_item.product_id().to_string();
                publisher
                    .publish_frontend_event_on_product(&product_id, FrontendEvent::AttentionItemCreated { item })
                    .await;
            }
        }
        Err(err) => tracing::warn!(
            execution_id,
            ?err,
            "queue CI evidence audit: failed to file attention item"
        ),
    }
}
