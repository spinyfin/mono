//! Refresh preserved execution history before handing it to another worker.
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
        format!(
            "## EXECUTION BOOKMARK RECOVERY\n\nRestored pointer: `{}`. After fetching the upstream, the engine staged copies of this history onto base SHA `{}`.\n\nOriginal commits (before rebasing):\n```text\n{}```\n\n{conflict_task}\n\n{pr_rewrite}Stay at `@` and inspect the inherited history. The old workspace was not used: re-run the required build and tests in your own leased workspace before publishing; earlier validation does not satisfy this run's gate.\n\n",
            self.pointer, self.base_sha, self.commits
        )
    }
}

/// The optional PR bookmark is `pr/<number>`, written by cube's bound-PR
/// checkout, never inferred from a branch name or a workspace's old contents.
pub async fn restore_rebased(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    pr_bookmark: Option<&str>,
    base_branch: &str,
    upstream: &str,
) -> Result<RestoreReport> {
    if jj.shared_repo(workspace).await? != record.repo_path {
        return Err(pointer_integrity_error(
            "recovery destination belongs to a different shared repository",
        ));
    }
    diff(jj, record).await?;
    jj.run(workspace, &["git", "fetch", "--remote", upstream]).await?;
    let base_ref = format!(
        "remote_bookmarks(exact:{}, exact:{})",
        serde_json::to_string(base_branch)?,
        serde_json::to_string(upstream)?
    );
    let base_sha = base_commit(jj, workspace, &base_ref, base_branch, upstream).await?;
    let pointer = head_bookmark(jj, record).await?;
    let head = revision(&pointer);
    let pr_head = match pr_bookmark {
        Some(bookmark) => Some(one_commit(jj, workspace, &revision(bookmark)).await?),
        None => None,
    };
    let sources = match &pr_head {
        Some(pr) => format!("{base_sha}..({head} | {pr})"),
        None => format!("{base_sha}..{head}"),
    };
    let commits = jj
        .run(
            workspace,
            &[
                "log",
                "--no-graph",
                "-r",
                &sources,
                "-T",
                "commit_id ++ \" \" ++ description.first_line() ++ \"\\n\"",
            ],
        )
        .await?;
    let inherited_base = if let Some(pr) = &pr_head {
        // Stage copies on the PR base, never rewrite the predecessor or the bound PR.
        // Every fallible command can be retried from the original durable refs.
        jj.run(workspace, &["new", &base_sha, "-m", "Resume recovered execution work"])
            .await?;
        jj.run(
            workspace,
            &["duplicate", &format!("{base_sha}..{pr}"), "--insert-before", "@"],
        )
        .await?;
        let baseline = one_commit(jj, workspace, "@-").await?;
        let unpublished = format!("({}..{head}) ~ (::{pr} | ::{base_sha})", revision(&record.base()));
        jj.run(workspace, &["duplicate", &unpublished, "--insert-before", "@"])
            .await?;
        Some(baseline)
    } else {
        jj.run(workspace, &["new", &base_sha, "-m", "Resume recovered execution work"])
            .await?;
        jj.run(
            workspace,
            &[
                "duplicate",
                &format!("{}..{head} ~ ::{base_sha}", revision(&record.base())),
                "--insert-before",
                "@",
            ],
        )
        .await?;
        Some(base_sha.clone())
    };
    let conflicted = jj
        .run(
            workspace,
            &[
                "log",
                "--no-graph",
                "-r",
                &format!("({base_sha}..@) & conflicts()"),
                "-T",
                "commit_id ++ \"\\n\"",
            ],
        )
        .await?;
    let mut conflicts = String::new();
    for commit in conflicted.lines().filter(|line| !line.is_empty()) {
        conflicts.push_str(&format!("Commit {commit}:\n"));
        conflicts.push_str(&jj.run(workspace, &["resolve", "--list", "-r", commit]).await?);
    }
    let pointer = match pr_bookmark {
        Some(pr) => format!("{pointer} + {pr}"),
        None => pointer,
    };
    Ok(RestoreReport::builder()
        .pointer(pointer)
        .commits(commits)
        .base_sha(base_sha)
        .conflicts(conflicts)
        .maybe_inherited_base(inherited_base)
        .pr_bound(pr_bookmark.is_some())
        .build())
}

async fn one_commit(jj: &dyn Jj, workspace: &Path, revset: &str) -> Result<String> {
    let output = jj
        .run(
            workspace,
            &["log", "--no-graph", "-r", revset, "-T", "commit_id ++ \"\\n\""],
        )
        .await?;
    let commits: Vec<_> = output.lines().filter(|s| !s.is_empty()).collect();
    ensure!(
        commits.len() == 1,
        "expected exactly one commit for {revset}; found {}",
        commits.len()
    );
    Ok(commits[0].to_owned())
}

/// Like [`one_commit`], but a base that resolves to no commit is the typed
/// [`base_unresolvable_error`] so callers can tell it from a transient failure.
async fn base_commit(jj: &dyn Jj, workspace: &Path, revset: &str, branch: &str, upstream: &str) -> Result<String> {
    let output = jj
        .run(
            workspace,
            &["log", "--no-graph", "-r", revset, "-T", "commit_id ++ \"\\n\""],
        )
        .await?;
    let commits: Vec<_> = output.lines().filter(|s| !s.is_empty()).collect();
    if commits.is_empty() {
        return Err(base_unresolvable_error(format!(
            "base branch `{branch}` has no commit on remote `{upstream}`"
        )));
    }
    ensure!(
        commits.len() == 1,
        "expected exactly one commit for {revset}; found {}",
        commits.len()
    );
    Ok(commits[0].to_owned())
}
