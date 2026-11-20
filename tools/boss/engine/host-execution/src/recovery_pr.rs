/// The one implementation behind every `recovery_pr_base` default.
pub async fn fetch_recovery_pr_base(origin: &str, pr: u64) -> anyhow::Result<String> {
    // A non-GitHub origin can never yield a PR base; every other failure here
    // (gh, network) is transient and must stay retryable.
    let slug = git_utils::repo_slug::parse_github_slug(origin).ok_or_else(|| {
        boss_engine_recovery::execution_bookmark::base_unresolvable_error(format!(
            "origin `{origin}` is not a GitHub repository"
        ))
    })?;
    git_utils::gh_cli::fetch_open_pr_base(&slug, pr)
        .await?
        .ok_or_else(closed_recovery_pr_error)
}

/// Typed error for a recovery PR that is no longer open; callers downcast to
/// it (`err.is::<ClosedRecoveryPr>()`) to tell a closed PR from a transient
/// lookup failure.
#[derive(Debug)]
pub struct ClosedRecoveryPr;

pub fn closed_recovery_pr_error() -> anyhow::Error {
    ClosedRecoveryPr.into()
}

impl std::fmt::Display for ClosedRecoveryPr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PR is not open")
    }
}
impl std::error::Error for ClosedRecoveryPr {}
