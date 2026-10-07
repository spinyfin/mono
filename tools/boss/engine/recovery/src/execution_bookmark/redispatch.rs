//! Refresh preserved execution history before handing it to another worker.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreReport {
    pub pointer: String,
    pub commits: String,
    pub base_sha: String,
    pub conflicts: String,
    /// Baseline of staged revision history; predecessor provenance stays untouched.
    #[serde(default)]
    pub inherited_base: Option<String>,
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
        format!(
            "## Restored work rebased onto main\n\nRestored pointer: `{}`. After `jj git fetch`, the engine rebased this history onto main SHA `{}`.\n\nOriginal commits (before rebasing):\n```text\n{}```\n\n{conflict_task}\n\nStay at `@`, inspect the inherited history, and rerun the required gates before publishing.\n\n",
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
) -> Result<RestoreReport> {
    if jj.shared_repo(workspace).await? != record.repo_path {
        return Err(pointer_integrity_error(
            "recovery destination belongs to a different shared repository",
        ));
    }
    diff(jj, record).await?;
    jj.run(workspace, &["git", "fetch"]).await?;
    let main = "remote_bookmarks(exact:main, exact:origin)";
    let base_sha = one_commit(jj, workspace, main).await?;
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
        // Stage copies on main, never rewrite the predecessor or the bound PR.
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
        jj.run(workspace, &["new", &head, "-m", "Resume recovered execution work"])
            .await?;
        jj.run(workspace, &["rebase", "-b", "@", "-d", &base_sha, "--ignore-immutable"])
            .await?;
        None
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
    Ok(RestoreReport {
        pointer: match pr_bookmark {
            Some(pr) => format!("{pointer} + {pr}"),
            None => pointer,
        },
        commits,
        base_sha,
        conflicts,
        inherited_base,
    })
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
