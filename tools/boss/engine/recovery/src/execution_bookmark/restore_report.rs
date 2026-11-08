//! The report a restore hands to the successor worker's prompt.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct RestoreReport {
    pub pointer: String,
    pub commits: String,
    pub base_sha: String,
    pub conflicts: String,
    /// Baseline of staged history; predecessor provenance stays untouched.
    #[serde(default)]
    pub inherited_base: Option<String>,
    /// True when a PR head bookmark was restaged (a revision), so the PR must be
    /// rewritten through `cube pr update`.
    #[serde(default)]
    #[builder(default)]
    pub pr_bound: bool,
    /// Set when the PR's own base could not be used and history was restaged
    /// onto a substitute branch; records the requested base, the branch used
    /// and why.
    #[serde(default)]
    pub base_fallback: Option<String>,
    /// Commit ids of conflicted preserved variants that were not selected.
    #[serde(default)]
    #[builder(default)]
    pub discarded_tips: String,
    /// Unpublished unselected variants that still have a named archive
    /// reference. Dispatch raises attention when this is non-empty.
    #[serde(default)]
    #[builder(default)]
    pub unrestaged_unpublished: String,
}

impl RestoreReport {
    pub fn instructions(&self) -> String {
        let conflict_task = if self.conflicts.is_empty() {
            "The rebase completed without conflicts.".to_owned()
        } else {
            format!(
                "FIRST TASK: resolve the following rebase conflicts, preserving both sides and all inherited commits, before continuing the implementation. Inspect each listed commit with `jj resolve --list -r <commit>`; conflicts may be in ancestors of `@`.\n{}",
                self.conflicts
            )
        };
        let pr_rewrite = if !self.pr_bound {
            String::new()
        } else {
            let base = match &self.base_fallback {
                Some(note) => format!(
                    "WARNING: the PR's own base branch could not be used ({note}). History was restaged onto that substitute branch instead, so if the PR is stacked, its unmerged parent's commits may now appear among the inherited commits: inspect them and drop any that belong to the parent PR before publishing. "
                ),
                None => "For a revision, the PR history was restaged onto its current base branch (including a stacked PR base). ".to_owned(),
            };
            format!(
                "{base}The PR head-branch bookmark still points at the original head: publishing requires moving that bookmark to the completed history with `jj bookmark set --allow-backwards` and rewriting the existing PR through `cube pr update`.\n\n"
            )
        };
        let discarded = if self.discarded_tips.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\n\nConflicted preserved variants not selected:\n```text\n{}```\n",
                self.discarded_tips
            )
        };
        format!(
            "## EXECUTION BOOKMARK RECOVERY\n\nRestored pointer: `{}`. After fetching the upstream, the engine restaged this history onto base SHA `{}`.\n\nOriginal commits (before rebasing):\n```text\n{}```\n{discarded}\n{conflict_task}\n\n{pr_rewrite}Stay at `@` and inspect the inherited history. The old workspace was not used: re-run the required build and tests in your own leased workspace before publishing; earlier validation does not satisfy this run's gate.\n\n",
            self.pointer, self.base_sha, self.commits
        )
    }
}
