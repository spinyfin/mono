//! Refresh disposable PR anchors without changing preserved execution history.
use super::*;

/// Exact legacy failures that can be repaired once on upgrade. New repair
/// failures deliberately use different text, so they cannot re-arm forever.
pub fn is_legacy_pr_anchor_blocker(detail: &str) -> bool {
    let Some((number, suffix)) = detail
        .strip_prefix("PR head bookmark pr/")
        .and_then(|s| s.split_once(' '))
    else {
        return false;
    };
    !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit())
        && (suffix == "does not exist in the shared store; the bound-PR checkout must write it before reconciliation"
            || suffix == "must resolve to exactly one change; it is conflicted")
}

/// `head_sha` and `head_ref` must come from the bound PR on GitHub, never a
/// local bookmark. Fetching may move publication pointers; restore them before
/// returning, including on failure. Only the PR alias adopts the remote head.
pub async fn repair_pr_anchor(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    pr: u64,
    remote: &str,
    head_ref: &str,
    head_sha: &str,
) -> Result<()> {
    ensure!(
        head_sha.len() == 40 && head_sha.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid GitHub PR head SHA"
    );
    let saved = snapshot_pointers(jj, record).await?;
    let fetched = jj
        .run(
            &record.repo_path,
            &[
                "git",
                "fetch",
                "--remote",
                remote,
                "--branch",
                &format!("exact:{head_ref}"),
            ],
        )
        .await;
    restore_pointers(jj, record, &saved).await?;
    fetched?;
    jj.run(
        &record.repo_path,
        &[
            "bookmark",
            "set",
            &format!("pr/{pr}"),
            "-r",
            head_sha,
            "--allow-backwards",
        ],
    )
    .await?;
    Ok(())
}
