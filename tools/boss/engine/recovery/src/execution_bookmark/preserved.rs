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

/// Newest-first window for journal walks. The shared store's op log can be
/// tens of GB; an unbounded walk stalls lease release and dispatch.
pub(super) const OP_LOG_LIMIT: &str = "256";

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

/// Snapshot once after the writer stops. A lost directory uses the last
/// working-copy entry in jj's operation journal. Cube names jj workspaces
/// after their final directories, including during staged provisioning.
async fn workspace_tip(jj: &dyn Jj, record: &ExecutionBookmark, workspace: &Path) -> Result<String> {
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
    // Locate the forget operation directly, even after a busy store has
    // accumulated many unrelated operations. The cap is a safety backstop.
    let forgotten = jj
        .run(
            &record.repo_path,
            &[
                "--ignore-working-copy",
                "op",
                "log",
                "--no-graph",
                "-n",
                "100000",
                "-T",
                &format!(
                    "if(description.contains(\"forget workspace\") && description.contains({}), id ++ \"\\n\")",
                    serde_json::to_string(name)?
                ),
            ],
        )
        .await?;
    let recent = operations(jj, &record.repo_path).await?;
    let candidates = forgotten
        .lines()
        .map(|id| format!("{id}-"))
        .chain(recent.into_iter().map(|(_, _, id)| id));
    for operation in candidates {
        let tip = jj
            .run(
                &record.repo_path,
                &[
                    "--ignore-working-copy",
                    "--at-op",
                    &operation,
                    "log",
                    "--no-graph",
                    "-r",
                    &rev,
                    "-T",
                    "commit_id",
                ],
            )
            .await;
        let tip = tip?;
        if !tip.trim().is_empty() {
            return Ok(tip);
        }
    }
    Err(pointer_integrity_error(format!(
        "no journaled working copy for {name}; keep preserved/{} and recovery references",
        record.execution_id
    )))
}

/// Update retained work only when the workspace has adopted its unpublished
/// changes. A fresh workspace proves nothing about an earlier attempt.
pub async fn preserve(jj: &dyn Jj, record: &ExecutionBookmark, workspace: &Path) -> Result<Option<PreservedTip>> {
    let name = preserved_name(&record.execution_id);
    let existing = log(jj, &record.repo_path, &revision(&name), "commit_id ++ \"\\n\"").await?;
    let tip = workspace_tip(jj, record, workspace).await?;
    let tip = tip.trim();
    let commits = log(jj, &record.repo_path, &unpublished(tip), "change_id ++ \"\\n\"").await?;
    let count = commits.lines().count() as u64;
    let ancestors = log(jj, &record.repo_path, &format!("::{tip}"), "change_id ++ \"\\n\"").await?;
    let mut retained = None;
    let mut contained_variant = false;
    for old in existing.lines() {
        let work = log(jj, &record.repo_path, &unpublished(old), "change_id ++ \"\\n\"").await?;
        let contained = work.lines().all(|change| ancestors.lines().any(|id| id == change));
        contained_variant |= contained;
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
            return Ok(retained);
        }
        if !existing.trim().is_empty() {
            jj.run(
                &record.repo_path,
                &["--ignore-working-copy", "bookmark", "delete", &name],
            )
            .await?;
        }
        return Ok(None);
    }
    let change_id = log(jj, &record.repo_path, tip, "change_id").await?;
    let existing_commit = existing.trim();
    if existing_commit.is_empty() {
        jj.run(
            &record.repo_path,
            &["--ignore-working-copy", "bookmark", "create", &name, "-r", tip],
        )
        .await?;
    } else if existing_commit != tip {
        jj.run(
            &record.repo_path,
            &[
                "--ignore-working-copy",
                "bookmark",
                "set",
                &name,
                "-r",
                tip,
                "--allow-backwards",
            ],
        )
        .await?;
    }
    Ok(Some(PreservedTip {
        change_id: change_id.trim().to_owned(),
        commit_id: tip.to_owned(),
        count,
    }))
}
