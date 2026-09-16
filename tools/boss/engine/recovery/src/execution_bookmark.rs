//! Engine-created recovery references in the shared jj repository.
//!
//! The worker advances the execution bookmark before editing a new change;
//! jj itself follows rewrites of the bookmarked change. Recovery never reads
//! the originating working copy. A separate baseline preserves the initial
//! state; published upstream history is excluded when checking for abandoned work.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionBookmark {
    pub execution_id: String,
    pub repo_path: PathBuf,
    pub host_id: String,
}

impl ExecutionBookmark {
    pub fn head(&self) -> String {
        format!("boss-recovery/{}", self.execution_id)
    }

    /// Publication refs can be consumed by cube after merging. Recovery owns
    /// a separate namespace and does not need a handshake with cube's pool.
    pub fn publication(&self) -> String {
        format!("boss/{}", self.execution_id)
    }

    pub fn base(&self) -> String {
        format!("boss-base/{}", self.execution_id)
    }
}

/// The transport is supplied by the host adapter; no engine types belong here.
#[async_trait]
pub trait Jj: Send + Sync {
    async fn run(&self, repo: &Path, args: &[&str]) -> Result<String>;
    async fn shared_repo(&self, workspace: &Path) -> Result<PathBuf>;
}

pub struct LocalJj;

pub fn jj_binary() -> std::ffi::OsString {
    std::env::var_os("BOSS_JJ_BIN").unwrap_or_else(|| "jj".into())
}

#[async_trait]
impl Jj for LocalJj {
    async fn run(&self, repo: &Path, args: &[&str]) -> Result<String> {
        let output = tokio::time::timeout(
            Duration::from_secs(60),
            tokio::process::Command::new(jj_binary())
                .arg("--no-pager")
                .arg("-R")
                .arg(repo)
                .args(args)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("jj recovery command timed out after 60s")?
        .context("could not run jj recovery command")?;
        ensure!(
            output.status.success(),
            "jj {args:?} in {} failed: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).context("jj output was not UTF-8")
    }

    async fn shared_repo(&self, workspace: &Path) -> Result<PathBuf> {
        let pointer = workspace.join(".jj/repo");
        if pointer.is_dir() {
            return workspace
                .canonicalize()
                .context("cannot resolve shared repository root");
        }
        shared_repo_from_pointer(
            &std::fs::read_to_string(&pointer)
                .with_context(|| format!("cannot read shared jj store pointer {}", pointer.display()))?,
        )
    }
}

pub fn shared_repo_from_pointer(pointer: &str) -> Result<PathBuf> {
    let path = Path::new(pointer.trim());
    ensure!(
        path.is_absolute() && path.ends_with(".jj/repo"),
        "unexpected shared jj store pointer: {pointer:?}"
    );
    Ok(path
        .parent()
        .and_then(Path::parent)
        .context("missing shared repository root")?
        .to_path_buf())
}

fn revision(bookmark: &str) -> String {
    // A literal bookmark selector, never a user-provided revset or name heuristic.
    format!(
        "bookmarks(exact:{})",
        serde_json::to_string(bookmark).expect("string serializes")
    )
}

async fn resolve(jj: &dyn Jj, repo: &Path, bookmark: &str) -> Result<String> {
    let output = jj
        .run(
            repo,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                &revision(bookmark),
                "-T",
                "change_id ++ \"\\n\"",
            ],
        )
        .await?;
    let ids: Vec<_> = output.lines().filter(|s| !s.is_empty()).collect();
    ensure!(
        ids.len() == 1,
        "recovery bookmark {bookmark} must resolve to exactly one change; found {}",
        ids.len()
    );
    Ok(ids[0].to_owned())
}

/// Called after positioning, before a worker can run. Existing refs are errors:
/// retry callers must use their persisted record, never overwrite provenance.
pub async fn create(jj: &dyn Jj, workspace: &Path, execution_id: &str, host_id: &str) -> Result<ExecutionBookmark> {
    create_from(jj, workspace, execution_id, host_id, None).await
}

/// Preserve inherited unpublished work across consecutive interrupted runs,
/// even when the next worker makes no additional edits.
pub async fn create_from(
    jj: &dyn Jj,
    workspace: &Path,
    execution_id: &str,
    host_id: &str,
    predecessor: Option<&ExecutionBookmark>,
) -> Result<ExecutionBookmark> {
    ensure!(
        !execution_id.is_empty() && execution_id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
        "invalid execution id"
    );
    let record = ExecutionBookmark {
        execution_id: execution_id.to_owned(),
        repo_path: jj.shared_repo(workspace).await?,
        host_id: host_id.to_owned(),
    };
    let baseline = if let Some(prior) = predecessor {
        ensure!(
            prior.repo_path == record.repo_path && prior.host_id == record.host_id,
            "inherited baseline belongs to another recovery store"
        );
        diff(jj, prior).await?;
        revision(&prior.base())
    } else {
        "@-".to_owned()
    };
    jj.run(workspace, &["bookmark", "create", &record.base(), "-r", &baseline])
        .await?;
    jj.run(
        workspace,
        &["bookmark", "create", &record.head(), &record.publication(), "-r", "@"],
    )
    .await?;
    // Validate through the shared store, not the workspace we just positioned.
    diff(jj, &record).await?;
    Ok(record)
}

/// A successful empty diff proves an empty run. Missing/conflicted references,
/// unavailable jj, and unrelated targets are errors, never empty results.
pub async fn diff(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<String> {
    let head = resolve(jj, &record.repo_path, &record.head()).await?;
    let base = resolve(jj, &record.repo_path, &record.base()).await?;
    let ancestry = format!("({base})::({head}) & ({head})");
    let connected = jj
        .run(
            &record.repo_path,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                &ancestry,
                "-T",
                "change_id",
            ],
        )
        .await?;
    if connected.trim() != head {
        bail!(
            "recovery bookmark {} is not descended from its engine-created baseline",
            record.head()
        );
    }
    let patch = jj
        .run(
            &record.repo_path,
            &[
                "--ignore-working-copy",
                "diff",
                "--git",
                "--from",
                &revision(&record.base()),
                "--to",
                &revision(&record.head()),
            ],
        )
        .await?;
    Ok(crate::recovery_apply::filter_bookkeeping(&patch).text)
}

/// Fork from the reference so later edits cannot rewrite the predecessor's work.
pub async fn restore(jj: &dyn Jj, record: &ExecutionBookmark, workspace: &Path) -> Result<bool> {
    ensure!(
        jj.shared_repo(workspace).await? == record.repo_path,
        "recovery destination belongs to a different shared repository"
    );
    let has_work = !unpublished_diff(jj, record).await?.trim().is_empty();
    if !has_work {
        return Ok(false);
    }
    jj.run(
        workspace,
        &[
            "new",
            &revision(&record.head()),
            "-m",
            "Resume recovered execution work",
        ],
    )
    .await?;
    Ok(has_work)
}

/// Ignore work already reachable from a remote bookmark, including an empty
/// working-copy child left after publishing. Still validate both local refs.
pub async fn unpublished_diff(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<String> {
    let patch = diff(jj, record).await?;
    if patch.trim().is_empty() {
        return Ok(patch);
    }
    let range = format!(
        "({}::{} ~ ::remote_bookmarks()) & ~empty()",
        revision(&record.base()),
        revision(&record.head())
    );
    let unpublished = jj
        .run(
            &record.repo_path,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                &range,
                "-T",
                "change_id ++ \"\\n\"",
            ],
        )
        .await?;
    Ok(if unpublished.trim().is_empty() {
        String::new()
    } else {
        patch
    })
}

#[cfg(test)]
mod tests;
