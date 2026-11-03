//! One durable reference for unpublished work when a workspace is released.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreservedTip {
    pub change_id: String,
    pub commit_id: String,
    pub count: u64,
}

/// Teardown result: the tip to record, plus an optional inspect verdict that
/// must not hold the lease.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PreserveOutcome {
    pub tip: Option<PreservedTip>,
    pub inspect: Option<PreserveInspect>,
}

/// Both the already-bookmarked work and the workspace tip, for a human to
/// inspect. Nothing is deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreserveInspect {
    pub existing_commit_id: Option<String>,
    pub workspace_change_id: Option<String>,
    pub workspace_commit_id: Option<String>,
    pub workspace_count: Option<u64>,
    pub reason: String,
}

pub fn preserved_name(execution_id: &str) -> String {
    format!("preserved/{execution_id}")
}

pub fn preserved_archive_name(execution_id: &str, commit: &str) -> String {
    format!("preserved-archive/{execution_id}/{commit}")
}

pub(super) async fn log(jj: &dyn Jj, repo: &Path, rev: &str, template: &str) -> Result<String> {
    jj.run(
        repo,
        &["--ignore-working-copy", "log", "--no-graph", "-r", rev, "-T", template],
    )
    .await
}

pub(super) fn unpublished(tip: &str) -> String {
    format!("(::{tip} ~ ::remote_bookmarks()) ~ empty()")
}

/// Newest-first window for journal walks that load operation views.
/// The shared store's op log can be tens of GB; an unbounded walk of
/// views stalls lease release and dispatch.
pub(super) const OP_LOG_LIMIT: &str = "256";

/// Safety cap for the forget-operation lookup. Unlike [`OP_LOG_LIMIT`], this
/// walk prints only operation ids and timestamps that match an exact
/// description; it loads no views. The bound is large enough to cover the
/// whole store log so a lost workspace remains findable after a busy period.
pub(super) const FORGET_OP_LOG_LIMIT: &str = "100000";

/// Concurrent operation branches are topologically displayed by jj, not
/// necessarily in time order. Sort their journal timestamps before choosing.
pub(super) async fn operations(jj: &dyn Jj, repo: &Path) -> Result<Vec<(i64, u32, String)>> {
    let output = jj
        .run(
            repo,
            &[
                "--ignore-working-copy",
                "op",
                "log",
                "--no-graph",
                "-n",
                OP_LOG_LIMIT,
                "-T",
                "time.end().format(\"%s.%f\") ++ \" \" ++ id ++ \"\\n\"",
            ],
        )
        .await?;
    let mut operations = Vec::new();
    for line in output.lines() {
        let (time, id) = line.split_once(' ').context("operation journal omitted id")?;
        let (seconds, nanos) = time.split_once('.').context("operation journal omitted timestamp")?;
        operations.push((seconds.parse()?, nanos.parse()?, id.to_owned()));
    }
    operations.sort_unstable_by_key(|op| std::cmp::Reverse((op.0, op.1)));
    Ok(operations)
}

async fn working_copy_at(jj: &dyn Jj, repo: &Path, operation: Option<&str>, rev: &str) -> Result<String> {
    let mut args = vec!["--ignore-working-copy"];
    if let Some(operation) = operation {
        args.extend(["--at-op", operation]);
    }
    args.extend(["log", "--no-graph", "-r", rev, "-T", "commit_id"]);
    jj.run(repo, &args).await
}

/// Snapshot once after the writer stops. A lost directory first reads the
/// still-registered working copy at the current operation. Forget history is
/// consulted only when that name is absent, and only the newest forget whose
/// timestamp is at or after this execution's lease or start is used. Cube
/// names jj workspaces after their final directories, including during
/// staged provisioning, and can reuse a name after a forget. Missing history
/// is `Ok(None)` so teardown can keep the existing bookmark and release.
async fn workspace_tip(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    since_unix: Option<i64>,
) -> Result<Option<String>> {
    if jj.workspace_exists(workspace).await? {
        ensure!(
            jj.shared_repo(workspace).await? == record.repo_path,
            "workspace recovery store changed"
        );
        return Ok(Some(
            jj.run(workspace, &["log", "--no-graph", "-r", "@", "-T", "commit_id"])
                .await?,
        ));
    }
    let name = workspace
        .file_name()
        .and_then(|s| s.to_str())
        .context("workspace name unavailable")?;
    let rev = format!("present({}@)", serde_json::to_string(name)?);
    match working_copy_at(jj, &record.repo_path, None, &rev).await {
        Ok(current) if !current.trim().is_empty() => return Ok(Some(current)),
        Ok(_) => {}
        // A forgotten name is not a revision at the current operation; look
        // up the forget instead of treating the missing selector as fatal.
        Err(err) if is_missing_revision_error(&err) => {}
        Err(err) => return Err(err),
    }
    let forget = format!("forget workspace {name}");
    let forgotten = jj
        .run(
            &record.repo_path,
            &[
                "--ignore-working-copy",
                "op",
                "log",
                "--no-graph",
                "-n",
                FORGET_OP_LOG_LIMIT,
                "-T",
                &format!(
                    "if(description == {}, time.end().format(\"%s\") ++ \" \" ++ id ++ \"\\n\")",
                    serde_json::to_string(&forget)?
                ),
            ],
        )
        .await?;
    for line in forgotten.lines() {
        let (time, id) = line.split_once(' ').context("forget journal omitted id")?;
        let seconds: i64 = time.parse()?;
        if since_unix.is_some_and(|since| seconds < since) {
            continue;
        }
        let parent = format!("{id}-");
        let tip = working_copy_at(jj, &record.repo_path, Some(&parent), &rev).await?;
        if !tip.trim().is_empty() {
            return Ok(Some(tip));
        }
        break;
    }
    for (_, _, id) in operations(jj, &record.repo_path).await? {
        let tip = working_copy_at(jj, &record.repo_path, Some(&id), &rev).await?;
        if !tip.trim().is_empty() {
            return Ok(Some(tip));
        }
    }
    Ok(None)
}

async fn forget_preserved(jj: &dyn Jj, repo: &Path, name: &str) -> Result<()> {
    // Forget, not delete: a git-exported preserved bookmark is also tracked
    // as `name@origin` after fetch. Delete would leave that remote pointer,
    // and `bookmark list` would still name it.
    jj.run(repo, &["--ignore-working-copy", "bookmark", "forget", name])
        .await?;
    Ok(())
}

async fn point_bookmark(jj: &dyn Jj, repo: &Path, name: &str, tip: &str, create: bool) -> Result<()> {
    if create {
        jj.run(repo, &["--ignore-working-copy", "bookmark", "create", name, "-r", tip])
            .await?;
    } else {
        jj.run(
            repo,
            &[
                "--ignore-working-copy",
                "bookmark",
                "set",
                name,
                "-r",
                tip,
                "--allow-backwards",
            ],
        )
        .await?;
    }
    Ok(())
}

fn inspect(
    existing: Option<&PreservedTip>,
    workspace: Option<(&str, &str, u64)>,
    reason: impl Into<String>,
) -> PreserveOutcome {
    PreserveOutcome {
        tip: existing.cloned(),
        inspect: Some(PreserveInspect {
            existing_commit_id: existing.map(|tip| tip.commit_id.clone()),
            workspace_change_id: workspace.map(|(change, _, _)| change.to_owned()),
            workspace_commit_id: workspace.map(|(_, commit, _)| commit.to_owned()),
            workspace_count: workspace.map(|(_, _, count)| count),
            reason: reason.into(),
        }),
    }
}

pub(super) fn change_revset(change_id: &str) -> String {
    // Change ids are revset symbols. `present()` yields nothing when the
    // change is gone, including hidden originals that already have a successor.
    // Exclude `root()`: jj's root change id is 32 `z`s, so a missing recorded
    // id that happens to be that sentinel would otherwise resolve to root.
    format!("present({change_id}) & ~root()")
}

pub(super) async fn log_change(jj: &dyn Jj, repo: &Path, rev: &str) -> Result<String> {
    match log(jj, repo, rev, "commit_id ++ \"\\n\"").await {
        Ok(output) => Ok(output),
        Err(err) if is_missing_revision_error(&err) => Ok(String::new()),
        Err(err) => Err(err),
    }
}

async fn visible_commits(jj: &dyn Jj, repo: &Path, change_id: &str) -> Result<String> {
    log_change(jj, repo, &change_revset(change_id)).await
}

async fn published_or_current(jj: &dyn Jj, repo: &Path, change_id: &str, tip: &str) -> Result<String> {
    log_change(
        jj,
        repo,
        &format!("{} & (::remote_bookmarks() | ::{tip})", change_revset(change_id)),
    )
    .await
}

async fn commit_is_visible(jj: &dyn Jj, repo: &Path, commit: &str) -> Result<bool> {
    let visible = log(
        jj,
        repo,
        &format!("present({commit}) & (::visible_heads() | ::remote_bookmarks())"),
        "commit_id ++ \"\\n\"",
    )
    .await?;
    Ok(visible.lines().any(|id| id == commit))
}

async fn tip_of(jj: &dyn Jj, repo: &Path, commit: &str) -> Result<PreservedTip> {
    let change = log(jj, repo, commit, "change_id").await?;
    let work = log(jj, repo, &unpublished(commit), "change_id ++ \"\\n\"").await?;
    Ok(PreservedTip {
        change_id: change.trim().into(),
        commit_id: commit.into(),
        count: work.lines().count() as u64,
    })
}

pub(super) fn is_missing_revision_error(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_ascii_lowercase();
    text.contains("no such revision")
        || text.contains("unknown revision")
        || text.contains("doesn't exist")
        || text.contains("does not exist")
}

enum RecordedExisting {
    Adopted,
    Visible(String),
    Hidden,
    Absent,
}

/// Identify recorded work by change id. A commit already in remote history
/// or the current workspace is adopted; a hidden commit is never bookmarked.
async fn resolve_recorded(
    jj: &dyn Jj,
    repo: &Path,
    recorded: &PreservedTip,
    workspace_tip: Option<&str>,
) -> Result<RecordedExisting> {
    let adopted = match workspace_tip {
        Some(tip) => published_or_current(jj, repo, &recorded.change_id, tip).await?,
        None => {
            log_change(
                jj,
                repo,
                &format!("{} & ::remote_bookmarks()", change_revset(&recorded.change_id)),
            )
            .await?
        }
    };
    if !adopted.trim().is_empty() {
        return Ok(RecordedExisting::Adopted);
    }
    let visible = visible_commits(jj, repo, &recorded.change_id).await?;
    if !visible.trim().is_empty() {
        return Ok(RecordedExisting::Visible(visible));
    }
    let present = log(
        jj,
        repo,
        &format!("present({}) & ~root()", recorded.commit_id),
        "commit_id ++ \"\\n\"",
    )
    .await?;
    if present.lines().any(|id| id == recorded.commit_id) {
        return Ok(RecordedExisting::Hidden);
    }
    Ok(RecordedExisting::Absent)
}

/// Update retained work only when the workspace has adopted its unpublished
/// changes. A fresh workspace proves nothing about an earlier attempt. Recorded
/// work is identified by change id; a bookmark that exists is trusted over a
/// recorded commit id, which may be hidden after a rewrite. Inspect verdicts
/// keep the existing bookmark and never hold the lease.
pub async fn preserve(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    recorded: Option<&PreservedTip>,
    since_unix: Option<i64>,
) -> Result<PreserveOutcome> {
    let name = preserved_name(&record.execution_id);
    let bookmark_commits = log(jj, &record.repo_path, &revision(&name), "commit_id ++ \"\\n\"").await?;
    let bookmark_missing = bookmark_commits.trim().is_empty();
    let workspace_commit = workspace_tip(jj, record, workspace, since_unix)
        .await?
        .and_then(|commit| {
            let tip = commit.trim();
            (!tip.is_empty()).then(|| tip.to_owned())
        });
    // When the bookmark exists it follows rewrites; ignore the recorded
    // commit id, which may be the hidden pre-rewrite original.
    let mut existing_ids: Vec<String> = bookmark_commits
        .lines()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect();
    existing_ids.sort();
    existing_ids.dedup();
    let mut existing = existing_ids.join("\n");
    let mut hidden_recorded = None;
    if bookmark_missing && let Some(recorded) = recorded {
        match resolve_recorded(jj, &record.repo_path, recorded, workspace_commit.as_deref()).await? {
            RecordedExisting::Adopted => {}
            RecordedExisting::Visible(visible) => existing = visible,
            RecordedExisting::Hidden => hidden_recorded = Some(recorded.clone()),
            RecordedExisting::Absent => {
                return Ok(inspect(
                    Some(recorded),
                    None,
                    format!(
                        "{name} is missing and recorded commit {} is not visible; leave the recorded tip intact",
                        recorded.commit_id
                    ),
                ));
            }
        }
    }
    let Some(tip) = workspace_commit else {
        if bookmark_missing && recorded.is_none() {
            return Ok(PreserveOutcome::default());
        }
        let existing = if bookmark_missing {
            hidden_recorded.or_else(|| recorded.cloned())
        } else if let Some(commit) = bookmark_commits.lines().next() {
            Some(tip_of(jj, &record.repo_path, commit).await?)
        } else {
            recorded.cloned()
        };
        return Ok(inspect(
            existing.as_ref(),
            None,
            format!(
                "no journaled working copy for {}; keep {name} and recovery references",
                workspace.display()
            ),
        ));
    };
    let tip = tip.trim();
    let commits = log(jj, &record.repo_path, &unpublished(tip), "change_id ++ \"\\n\"").await?;
    let count = commits.lines().count() as u64;
    let ancestors = log(jj, &record.repo_path, &format!("::{tip}"), "change_id ++ \"\\n\"").await?;
    let workspace_ids = log(jj, &record.repo_path, tip, "change_id").await?;
    let workspace_tip_ids = (workspace_ids.trim(), tip, count);
    let mut retained = None;
    let mut contained_variant = false;
    for old in existing.lines() {
        let work = log(jj, &record.repo_path, &unpublished(old), "change_id ++ \"\\n\"").await?;
        let contained = work.lines().all(|change| ancestors.lines().any(|id| id == change));
        let old_ancestors = log(jj, &record.repo_path, &format!("::{old}"), "change_id ++ \"\\n\"").await?;
        contained_variant |= commits
            .lines()
            .all(|change| old_ancestors.lines().any(|id| id == change));
        if !work.is_empty() {
            let change = log(jj, &record.repo_path, old, "change_id").await?;
            retained.get_or_insert(PreservedTip {
                change_id: change.trim().into(),
                commit_id: old.into(),
                count: work.lines().count() as u64,
            });
            if !contained && count > 0 && existing.lines().count() == 1 {
                return Ok(inspect(
                    retained.as_ref(),
                    Some(workspace_tip_ids),
                    format!("{name} contains unpublished work absent from the current workspace; leave both intact"),
                ));
            }
        }
    }
    if existing.lines().filter(|id| !id.is_empty()).count() > 1 {
        if count == 0 && retained.is_none() {
            if !bookmark_missing {
                forget_preserved(jj, &record.repo_path, &name).await?;
            }
            return Ok(PreserveOutcome::default());
        }
        if count == 0 || contained_variant {
            return Ok(PreserveOutcome {
                tip: retained,
                inspect: None,
            });
        }
        return Ok(inspect(
            retained.as_ref(),
            Some(workspace_tip_ids),
            format!("{name} has multiple preserved tips unrelated to current work; leave all intact"),
        ));
    }
    if let Some(hidden) = hidden_recorded {
        if count > 0 {
            return Ok(inspect(
                Some(&hidden),
                Some(workspace_tip_ids),
                format!(
                    "{name} is missing and recorded commit {} is hidden; leave it and the new workspace tip intact",
                    hidden.commit_id
                ),
            ));
        }
        return Ok(PreserveOutcome {
            tip: Some(hidden),
            inspect: None,
        });
    }
    if count == 0 {
        if let Some(kept) = retained.as_ref() {
            if bookmark_missing && commit_is_visible(jj, &record.repo_path, &kept.commit_id).await? {
                point_bookmark(jj, &record.repo_path, &name, &kept.commit_id, true).await?;
            }
            return Ok(PreserveOutcome {
                tip: retained,
                inspect: None,
            });
        }
        if !bookmark_missing {
            forget_preserved(jj, &record.repo_path, &name).await?;
        }
        return Ok(PreserveOutcome::default());
    }
    if bookmark_missing || bookmark_commits.trim() != tip {
        point_bookmark(jj, &record.repo_path, &name, tip, bookmark_missing).await?;
    }
    Ok(PreserveOutcome {
        tip: Some(PreservedTip {
            change_id: workspace_ids.trim().to_owned(),
            commit_id: tip.to_owned(),
            count,
        }),
        inspect: None,
    })
}
