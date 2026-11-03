//! One durable reference for unpublished work when a workspace is released.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreservedTip {
    pub change_id: String,
    pub commit_id: String,
    pub count: u64,
}

pub fn preserved_name(execution_id: &str) -> String {
    format!("preserved/{execution_id}")
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
/// staged provisioning, and can reuse a name after a forget.
async fn workspace_tip(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    since_unix: Option<i64>,
) -> Result<String> {
    if jj.workspace_exists(workspace).await? {
        ensure!(
            jj.shared_repo(workspace).await? == record.repo_path,
            "workspace recovery store changed"
        );
        return jj
            .run(workspace, &["log", "--no-graph", "-r", "@", "-T", "commit_id"])
            .await;
    }
    let name = workspace
        .file_name()
        .and_then(|s| s.to_str())
        .context("workspace name unavailable")?;
    let rev = format!("present({}@)", serde_json::to_string(name)?);
    match working_copy_at(jj, &record.repo_path, None, &rev).await {
        Ok(current) if !current.trim().is_empty() => return Ok(current),
        Ok(_) => {}
        // A forgotten name is not a revision at the current operation; look
        // up the forget instead of treating the missing selector as fatal.
        Err(_) => {}
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
            return Ok(tip);
        }
        break;
    }
    for (_, _, id) in operations(jj, &record.repo_path).await? {
        let tip = working_copy_at(jj, &record.repo_path, Some(&id), &rev).await?;
        if !tip.trim().is_empty() {
            return Ok(tip);
        }
    }
    Err(pointer_integrity_error(format!(
        "no journaled working copy for {name}; keep preserved/{} and recovery references",
        record.execution_id
    )))
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

/// Update retained work only when the workspace has adopted its unpublished
/// changes. A fresh workspace proves nothing about an earlier attempt. A
/// missing bookmark still treats the recorded commit as existing work.
pub async fn preserve(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    recorded: Option<&PreservedTip>,
    since_unix: Option<i64>,
) -> Result<Option<PreservedTip>> {
    let name = preserved_name(&record.execution_id);
    let bookmark_commits = log(jj, &record.repo_path, &revision(&name), "commit_id ++ \"\\n\"").await?;
    let bookmark_missing = bookmark_commits.trim().is_empty();
    let mut recorded_work = String::new();
    let mut existing = bookmark_commits.clone();
    if let Some(recorded) = recorded {
        let present = log(
            jj,
            &record.repo_path,
            &format!("present({})", recorded.commit_id),
            "commit_id ++ \"\\n\"",
        )
        .await?;
        if present.lines().any(|id| id == recorded.commit_id) {
            recorded_work = log(
                jj,
                &record.repo_path,
                &unpublished(&recorded.commit_id),
                "change_id ++ \"\\n\"",
            )
            .await?;
            if bookmark_missing {
                existing = format!("{}\n", recorded.commit_id);
            }
        } else if bookmark_missing {
            return Err(pointer_integrity_error(format!(
                "{name} is missing and recorded commit {} is not visible; leave the recorded tip intact",
                recorded.commit_id
            )));
        }
    }
    let tip = workspace_tip(jj, record, workspace, since_unix).await?;
    let tip = tip.trim();
    let commits = log(jj, &record.repo_path, &unpublished(tip), "change_id ++ \"\\n\"").await?;
    let count = commits.lines().count() as u64;
    let ancestors = log(jj, &record.repo_path, &format!("::{tip}"), "change_id ++ \"\\n\"").await?;
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
                return Err(pointer_integrity_error(format!(
                    "{name} contains unpublished work absent from the current workspace; leave both intact"
                )));
            }
        }
    }
    if count > 0
        && !recorded_work.is_empty()
        && !recorded_work
            .lines()
            .all(|change| ancestors.lines().any(|id| id == change))
    {
        return Err(pointer_integrity_error(format!(
            "{name} recorded unpublished work is absent from the current workspace; leave both intact"
        )));
    }
    if existing.lines().count() > 1 {
        if count == 0 || contained_variant {
            return Ok(retained);
        }
        return Err(pointer_integrity_error(format!(
            "{name} has multiple preserved tips unrelated to current work; leave all intact"
        )));
    }
    if count == 0 {
        if retained.is_some() {
            if bookmark_missing && let Some(kept) = retained.as_ref() {
                point_bookmark(jj, &record.repo_path, &name, &kept.commit_id, true).await?;
            }
            return Ok(retained);
        }
        if !recorded_work.is_empty()
            && let Some(recorded) = recorded
        {
            point_bookmark(jj, &record.repo_path, &name, &recorded.commit_id, bookmark_missing).await?;
            return Ok(Some(recorded.clone()));
        }
        if !bookmark_missing {
            jj.run(
                &record.repo_path,
                &["--ignore-working-copy", "bookmark", "delete", &name],
            )
            .await?;
        }
        return Ok(None);
    }
    let change_id = log(jj, &record.repo_path, tip, "change_id").await?;
    if bookmark_missing || bookmark_commits.trim() != tip {
        point_bookmark(jj, &record.repo_path, &name, tip, bookmark_missing).await?;
    }
    Ok(Some(PreservedTip {
        change_id: change_id.trim().to_owned(),
        commit_id: tip.to_owned(),
        count,
    }))
}
