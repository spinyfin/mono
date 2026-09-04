//! Post-PR CI-monitoring prompt directive.

use crate::work::WorkExecution;

/// Guides workers to leave CI monitoring to the engine while retaining the
/// distinction between genuinely failed and merely human-gated checks.
pub(super) fn ci_monitoring_directive(execution: &WorkExecution) -> String {
    let mut out = String::new();
    out.push_str("\n## After the PR is open: do not babysit CI\n\n");
    out.push_str(
        "Once your branch is pushed and the PR exists, your deliverable is done — print the PR URL and stop. Do NOT sit in a loop polling `gh pr checks` / `gh pr view` waiting for every check to turn green. That loop can run forever and strands your slot.\n\n",
    );
    out.push_str(
        "Why this is safe: the engine polls this PR's CI on its own cadence and auto-transitions the task to Review the moment CI is *effectively green*. \"Effectively green\" matches the engine's own definition — every required CI check has reached a passing terminal state (`SUCCESS`, `NEUTRAL`, or `SKIPPED`). It deliberately does NOT require checks that are gated on a human action and never resolve from CI alone; waiting on those is waiting forever.\n\n",
    );
    if let Ok(slug) = crate::completion::parse_repo_slug(&execution.repo_remote_url) {
        let owner = slug.split('/').next().unwrap_or("");
        let names = crate::merge_poller::review_signal_checks_for_owner(owner);
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
    out.push_str(
        "A required CI check that has genuinely *failed* (not merely pending) is different — classify it per the CI-failure rules at the top of this prompt (caused by you: fix; unrelated and trivial: fix and state the category in the PR; unrelated and not trivial: flag, do not absorb). Waiting for a CI-fix revision is not a substitute for fixing a failure you could handle in this run — that path is the fallback for a failure that genuinely needs a separate change. A still-running or human-gated check never blocks your completion.\n",
    );
    out
}
