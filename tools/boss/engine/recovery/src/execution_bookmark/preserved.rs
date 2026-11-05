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

pub fn preserved_archive_prefix(execution_id: &str) -> String {
    format!("preserved-archive/{execution_id}/")
}

/// How teardown may read the workspace tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceTipSource {
    /// The workspace still belongs to this execution. Snapshot live `@`.
    Live,
    /// The workspace no longer provably belongs to this execution. Resolve the
    /// registered tip as of this execution's last confirmed lease time. The
    /// live working copy is never snapshotted, because nothing proves that a
    /// later holder did not write to it; newer registered work that the
    /// boundary tip does not contain is reported for inspection instead.
    LeaseBoundary { unix_millis: i64 },
    /// No lease boundary was recorded, so no historical tip can be chosen.
    /// Teardown raises inspection rather than guessing.
    UnknownBoundary,
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

/// Safety cap for the metadata-only operation journal. Walks print only
/// operation ids and timestamps and load a view for at most the operations
/// they actually resolve, so the bound covers the whole store log and a lost
/// or long-reassigned workspace remains findable after a busy period.
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
                FORGET_OP_LOG_LIMIT,
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

/// A resolved workspace tip plus registered work it does not contain.
#[derive(Default)]
struct WorkspaceTip {
    commit: Option<String>,
    /// Live registered tip and its unpublished change count when it holds work
    /// absent from `commit`, whose ownership cannot be established.
    unclaimed: Option<(String, u64)>,
}

/// Snapshot after the writer stops only with current ownership evidence.
/// Unresolved history yields inspection, never proof of empty work.
async fn workspace_tip(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    since_unix: Option<i64>,
    tip_source: WorkspaceTipSource,
) -> Result<WorkspaceTip> {
    if matches!(tip_source, WorkspaceTipSource::Live) && jj.workspace_exists(workspace).await? {
        ensure!(
            jj.shared_repo(workspace).await? == record.repo_path,
            "workspace recovery store changed"
        );
        return Ok(WorkspaceTip {
            commit: Some(
                jj.run(workspace, &["log", "--no-graph", "-r", "@", "-T", "commit_id"])
                    .await?,
            ),
            unclaimed: None,
        });
    }
    historical_workspace_tip(jj, record, workspace, since_unix, tip_source).await
}

async fn historical_workspace_tip(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    since_unix: Option<i64>,
    tip_source: WorkspaceTipSource,
) -> Result<WorkspaceTip> {
    let boundary = match tip_source {
        WorkspaceTipSource::LeaseBoundary { unix_millis } => Some(unix_millis),
        WorkspaceTipSource::Live => None,
        WorkspaceTipSource::UnknownBoundary => return Ok(WorkspaceTip::default()),
    };
    let name = workspace
        .file_name()
        .and_then(|s| s.to_str())
        .context("workspace name unavailable")?;
    let rev = format!("present({}@)", serde_json::to_string(name)?);
    // Metadata-only journal: load a view only for the newest operation at or
    // before the boundary, never for the operations after it.
    for (seconds, nanos, id) in operations(jj, &record.repo_path).await? {
        if since_unix.is_some_and(|since| seconds < since) {
            continue;
        }
        if boundary.is_some_and(|before| seconds * 1000 + i64::from(nanos / 1_000_000) > before) {
            continue;
        }
        let tip = match working_copy_at(jj, &record.repo_path, Some(&id), &rev).await {
            Ok(tip) => tip.trim().to_owned(),
            Err(err) if is_missing_revision_error(&err) => continue,
            Err(err) => return Err(err),
        };
        if tip.is_empty() {
            continue;
        }
        let commit = visible_historical_tip(jj, &record.repo_path, &tip).await?;
        let unclaimed = if boundary.is_some() {
            unclaimed_registered_work(jj, &record.repo_path, &rev, commit.as_deref()).await?
        } else {
            None
        };
        return Ok(WorkspaceTip { commit, unclaimed });
    }
    // A forgotten workspace's journaled tip is read from its pre-forget view.
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
        if since_unix.is_some_and(|since| seconds < since) || boundary.is_some_and(|before| seconds * 1000 > before) {
            continue;
        }
        let tip = working_copy_at(jj, &record.repo_path, Some(&format!("{id}-")), &rev).await?;
        if !tip.trim().is_empty() {
            return Ok(WorkspaceTip {
                commit: visible_historical_tip(jj, &record.repo_path, tip.trim()).await?,
                unclaimed: None,
            });
        }
    }
    Ok(WorkspaceTip::default())
}

/// The current registered tip's unpublished work that the resolved boundary
/// tip does not contain: made by this worker after its last confirmed
/// heartbeat or by a later holder. Never bookmarked; reported for inspection.
async fn unclaimed_registered_work(
    jj: &dyn Jj,
    repo: &Path,
    rev: &str,
    resolved: Option<&str>,
) -> Result<Option<(String, u64)>> {
    let live = match working_copy_at(jj, repo, None, rev).await {
        Ok(live) => live.trim().to_owned(),
        Err(err) if is_missing_revision_error(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    if live.is_empty() || resolved == Some(live.as_str()) {
        return Ok(None);
    }
    let excluded = resolved.map_or(String::new(), |tip| format!(" ~ ::{tip}"));
    let extra = log(
        jj,
        repo,
        &format!("((::{live} ~ ::remote_bookmarks(){excluded}) ~ empty())"),
        "change_id ++ \"\\n\"",
    )
    .await?;
    let count = extra.lines().count() as u64;
    Ok((count > 0).then_some((live, count)))
}

/// Never bookmark a hidden pre-rewrite or deliberately abandoned commit. An
/// unpublished amendment of a pushed change wins over the pushed commit.
async fn visible_historical_tip(jj: &dyn Jj, repo: &Path, tip: &str) -> Result<Option<String>> {
    let change = log(jj, repo, tip, "change_id").await?;
    let unpublished_visible = log_change(
        jj,
        repo,
        &format!("{} & ~::remote_bookmarks()", change_revset(change.trim())),
    )
    .await?;
    match unpublished_visible.lines().count() {
        1 => return Ok(unpublished_visible.lines().next().map(str::to_owned)),
        0 => {}
        _ => return Ok(None),
    }
    let published = log_change(
        jj,
        repo,
        &format!("{} & ::remote_bookmarks()", change_revset(change.trim())),
    )
    .await?;
    if let Some(published) = published.lines().next() {
        return Ok(Some(published.to_owned()));
    }
    // Abandonment can leave earlier visible unpublished ancestors.
    let ancestors = log(
        jj,
        repo,
        &format!("heads(::{tip} & (::visible_heads() | ::remote_bookmarks()))"),
        "commit_id ++ \"\\n\"",
    )
    .await?;
    Ok((ancestors.lines().count() == 1).then(|| ancestors.trim().to_owned()))
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

pub(super) async fn commit_is_visible(jj: &dyn Jj, repo: &Path, commit: &str) -> Result<bool> {
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

/// Visible commits of a recorded change, a hidden recorded commit, or neither.
/// Teardown and restore must classify a recorded tip the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RecordedPresence {
    Visible(String),
    Hidden,
    Absent,
}

pub(super) async fn resolve_recorded_presence(
    jj: &dyn Jj,
    repo: &Path,
    recorded: &PreservedTip,
) -> Result<RecordedPresence> {
    let visible = visible_commits(jj, repo, &recorded.change_id).await?;
    if !visible.trim().is_empty() {
        return Ok(RecordedPresence::Visible(visible));
    }
    let present = log(
        jj,
        repo,
        &format!("present({}) & ~root()", recorded.commit_id),
        "commit_id ++ \"\\n\"",
    )
    .await?;
    if present.lines().any(|id| id == recorded.commit_id) {
        return Ok(RecordedPresence::Hidden);
    }
    Ok(RecordedPresence::Absent)
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
    Ok(match resolve_recorded_presence(jj, repo, recorded).await? {
        RecordedPresence::Visible(visible) => RecordedExisting::Visible(visible),
        RecordedPresence::Hidden => RecordedExisting::Hidden,
        RecordedPresence::Absent => RecordedExisting::Absent,
    })
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
    preserve_from(jj, record, workspace, recorded, since_unix, WorkspaceTipSource::Live).await
}

/// [`preserve`] with an owning-host workspace selection policy.
pub async fn preserve_from(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    recorded: Option<&PreservedTip>,
    since_unix: Option<i64>,
    tip_source: WorkspaceTipSource,
) -> Result<PreserveOutcome> {
    let resolved = workspace_tip(jj, record, workspace, since_unix, tip_source).await?;
    let unclaimed = resolved.unclaimed;
    let mut outcome = preserve_resolved(jj, record, workspace, recorded, tip_source, resolved.commit).await?;
    if let Some((live, count)) = unclaimed {
        let note = format!(
            "registered workspace tip {live} holds {count} unpublished change(s) absent from the work resolved at the lease boundary; leaving them unbookmarked because their owner cannot be established"
        );
        match outcome.inspect.as_mut() {
            Some(inspect) => inspect.reason.push_str(&format!("; {note}")),
            None => {
                outcome.inspect = Some(PreserveInspect {
                    existing_commit_id: outcome.tip.as_ref().map(|tip| tip.commit_id.clone()),
                    workspace_change_id: None,
                    workspace_commit_id: Some(live),
                    workspace_count: Some(count),
                    reason: note,
                });
            }
        }
    }
    Ok(outcome)
}

async fn preserve_resolved(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    recorded: Option<&PreservedTip>,
    tip_source: WorkspaceTipSource,
    workspace_commit: Option<String>,
) -> Result<PreserveOutcome> {
    let name = preserved_name(&record.execution_id);
    let bookmark_commits = log(jj, &record.repo_path, &revision(&name), "commit_id ++ \"\\n\"").await?;
    let bookmark_missing = bookmark_commits.trim().is_empty();
    let workspace_commit = workspace_commit.and_then(|commit| {
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
    let mut absent_recorded = None;
    if bookmark_missing && let Some(recorded) = recorded {
        match resolve_recorded(jj, &record.repo_path, recorded, workspace_commit.as_deref()).await? {
            RecordedExisting::Adopted => {}
            RecordedExisting::Visible(visible) => existing = visible,
            RecordedExisting::Hidden => hidden_recorded = Some(recorded.clone()),
            RecordedExisting::Absent => absent_recorded = Some(recorded.clone()),
        }
    }
    let Some(tip) = workspace_commit else {
        if matches!(tip_source, WorkspaceTipSource::Live) && bookmark_missing && recorded.is_none() {
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
            point_bookmark(jj, &record.repo_path, &name, tip, true).await?;
            tracing::info!(
                execution_id = %record.execution_id,
                hidden_commit_id = %hidden.commit_id,
                new_commit_id = %tip,
                "recorded preserved commit is hidden; bookmarking the new workspace tip"
            );
            return Ok(PreserveOutcome {
                tip: Some(PreservedTip {
                    change_id: workspace_ids.trim().to_owned(),
                    commit_id: tip.to_owned(),
                    count,
                }),
                inspect: None,
            });
        }
        // Abandoned or squashed: the change has no visible successor. Clear
        // the row so restore cannot restage the hidden commit.
        return Ok(PreserveOutcome::default());
    }
    if let Some(recorded) = absent_recorded {
        let reason = format!(
            "{name} is missing and recorded commit {} is not visible",
            recorded.commit_id
        );
        if count > 0 {
            point_bookmark(jj, &record.repo_path, &name, tip, true).await?;
            return Ok(PreserveOutcome {
                tip: Some(PreservedTip {
                    change_id: workspace_ids.trim().to_owned(),
                    commit_id: tip.to_owned(),
                    count,
                }),
                inspect: Some(PreserveInspect {
                    existing_commit_id: Some(recorded.commit_id),
                    workspace_change_id: Some(workspace_ids.trim().to_owned()),
                    workspace_commit_id: Some(tip.to_owned()),
                    workspace_count: Some(count),
                    reason,
                }),
            });
        }
        return Ok(PreserveOutcome {
            tip: None,
            inspect: Some(PreserveInspect {
                existing_commit_id: Some(recorded.commit_id),
                workspace_change_id: Some(workspace_ids.trim().to_owned()),
                workspace_commit_id: Some(tip.to_owned()),
                workspace_count: Some(count),
                reason,
            }),
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
