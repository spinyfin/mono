//! Shared explicit and un-park review admission.
use boss_work_db::{PrStateChecker, ReviewBatchDispatch, WorkDb, WorkExecution};

/// Admission preserves the batch outcome so each caller can report it in its
/// own interface. Database admission rechecks parking in its write transaction.
pub enum ReviewAdmission {
    Legacy(WorkExecution),
    Batch(ReviewBatchDispatch),
}

/// Start a review using the configured pipeline and a shared parking guard.
/// The metadata callback keeps GitHub access injectable for callers' tests.
pub async fn admit_review<F, Fut>(
    db: &WorkDb,
    work_item_id: &str,
    pr_url: Option<&str>,
    pool: Option<usize>,
    pr_checker: &dyn PrStateChecker,
    fetch_metadata: F,
) -> anyhow::Result<ReviewAdmission>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<serde_json::Value>>,
{
    db.refuse_if_parked(work_item_id)?;
    let Some(pool) = pool else {
        return db
            .request_pr_review(work_item_id, pr_checker)
            .map(ReviewAdmission::Legacy);
    };
    let pr_url = pr_url.ok_or_else(|| anyhow::anyhow!("task has no PR URL"))?;
    let metadata = fetch_metadata(pr_url.to_owned())
        .await
        .map_err(|err| anyhow::anyhow!("cannot start review batch: PR metadata unavailable: {err}"))?;
    match metadata.get("state").and_then(serde_json::Value::as_str) {
        Some("OPEN") => {}
        Some(state) => anyhow::bail!("cannot start review batch: PR is {state}"),
        None => anyhow::bail!("cannot start review batch: PR metadata omitted open state"),
    }
    let input = crate::review_batch_input_from_metadata(db, work_item_id, pr_url, metadata)?;
    db.request_pre_merge_review_batch_for_pool(input, pool)
        .map(ReviewAdmission::Batch)
}
