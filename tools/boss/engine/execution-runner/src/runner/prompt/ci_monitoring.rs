//! Post-PR CI-monitoring prompt directive.

use crate::work::WorkExecution;

/// Adds repository-specific human-gated checks to the shared CI policy in worker rules.
pub(super) fn ci_monitoring_directive(
    execution: &WorkExecution,
    review_signal_checks: fn(&str) -> &'static [&'static str],
) -> String {
    let mut out = String::new();
    if let Some(slug) = git_utils::repo_slug::parse_github_slug(&execution.repo_remote_url) {
        let owner = slug.split('/').next().unwrap_or("");
        let names = review_signal_checks(owner);
        if !names.is_empty() {
            let rendered = names
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!(
                "This PR's org (`{owner}`) ships required check(s) that are human-gated and never auto-resolve from CI: {rendered}. The engine's CI-completion check treats them as NOT blocking — they stay pending until a human approves. You must do the same: their pending/running state is not a reason to keep this run alive.\n\n",
            ));
        }
    }
    out
}
