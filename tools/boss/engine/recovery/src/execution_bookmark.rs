//! The engine's record of where an execution's work sits in the shared jj
//! repository.
//!
//! Dispatch records only the commit an execution starts from (`pr_head_before`
//! for a bound PR, the trunk commit for a fresh item) and creates no bookmark.
//! Teardown preserves unpublished workspace edits under one
//! `preserved/<execution>` bookmark. The next dispatch restages from it, and
//! it is forgotten once a successor publishes or its chain has been closed for
//! a fixed age. Workers create their own publication branch immediately
//! before opening a PR.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

mod preserved;
#[cfg(test)]
mod preserved_quarantine_tests;
#[cfg(test)]
mod preserved_retry_tests;
#[cfg(test)]
mod preserved_safety_tests;
#[cfg(test)]
mod preserved_tests;
pub use preserved::{
    BoundaryOwnership, HostRecoveryRequest, PreserveInspect, PreserveOutcome, PreservedTip, WorkspaceTipSource,
    preserve, preserve_from, preserved_archive_name, preserved_archive_prefix, preserved_name,
    preserved_work_unpublished,
};
mod preserved_restore;
pub use preserved_restore::restore_preserved;

mod preserved_cleanup;
pub use preserved_cleanup::{PreservedScope, forget_preserved_references};
#[cfg(test)]
mod preserved_cleanup_tests;

/// Which invariant a damaged pointer broke. Lineage between the pointers and
/// the baseline is deliberately not an invariant: jj rewrites commits and
/// merges concurrent operations as a matter of course, so the recorded
/// baseline commit id is a floor, never something the heads must descend from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PointerIntegrityKind {
    /// A resumed head descends from work that teardown quarantined as possibly
    /// belonging to a later holder. Nothing was moved or restaged.
    Quarantined,
    /// Ambiguous execution bookmark or wrong store.
    Other,
}

/// The preserved pointer itself is ambiguous, divergent or aimed at
/// the wrong repository. Retrying cannot repair it, unlike a failed `jj`
/// invocation (network, SSH, unavailable host), which stays an ordinary error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerIntegrityError {
    pub kind: PointerIntegrityKind,
    pub message: String,
}

impl std::fmt::Display for PointerIntegrityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PointerIntegrityError {}

/// The message the retired lineage check reported, followed by
/// ` boss-base/<execution id>`. Kept only so a stored work-item blocker written
/// by an earlier engine can still be recognised exactly.
pub const NOT_DESCENDED_MESSAGE: &str = "preserved execution pointer is not descended from its engine-created baseline";

/// The sentence the retired divergence check reported, preceded by
/// `preserved pointers <recovery> and <publication> `. Legacy blocker recognition only.
pub const DIVERGED_MESSAGE: &str = "diverged; refusing to drop either history";

/// The sentence a bookmark that does not resolve reports, preceded by
/// `recovery bookmark <name> ` and followed by `; found <n>`.
pub const UNRESOLVED_MESSAGE: &str = "must resolve to exactly one change";

pub fn pointer_integrity_error(message: impl Into<String>) -> anyhow::Error {
    pointer_integrity_error_of(PointerIntegrityKind::Other, message)
}

pub fn pointer_integrity_error_of(kind: PointerIntegrityKind, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(PointerIntegrityError {
        kind,
        message: message.into(),
    })
}

/// True when `error` (or any cause in its chain) is a pointer-integrity failure.
pub fn is_pointer_integrity_error(error: &anyhow::Error) -> bool {
    pointer_integrity_kind(error).is_some()
}

/// The kind of the first pointer-integrity failure in `error`'s chain.
pub fn pointer_integrity_kind(error: &anyhow::Error) -> Option<PointerIntegrityKind> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<PointerIntegrityError>())
        .map(|cause| cause.kind)
}

fn is_execution_id(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// Recognise a work-item blocker written by the engine before pointer
/// reconciliation existed, in exactly one of its three shapes:
///
/// - `<NOT_DESCENDED_MESSAGE> boss-base/<execution id>`
/// - `preserved pointers boss-recovery/<id> and boss/<id> <DIVERGED_MESSAGE>`
/// - `recovery bookmark boss-base/<id> <UNRESOLVED_MESSAGE>; found 0`
///
/// Items blocked with any of these can run again now that dispatch
/// reconciles them against the pointers the engine recorded; anything else
/// (missing or conflicted heads, a foreign store, a precise not-contained
/// report) was handled by later migrations, including the archive-and-start-
/// fresh migration. Exact shapes only: a trailing context or a changed wording is not recognised.
pub fn is_legacy_reconcilable_blocker(detail: &str) -> bool {
    if let Some(rest) = detail.strip_prefix(NOT_DESCENDED_MESSAGE)
        && let Some(id) = rest.strip_prefix(" boss-base/exec_")
    {
        return is_execution_id(id);
    }
    if let Some(rest) = detail.strip_prefix("preserved pointers boss-recovery/exec_")
        && let Some(rest) = rest.strip_suffix(DIVERGED_MESSAGE)
        && let Some((recovery, publication)) = rest.split_once(" and boss/exec_")
        && let Some(publication) = publication.strip_suffix(' ')
    {
        return is_execution_id(recovery) && recovery == publication;
    }
    if let Some(rest) = detail.strip_prefix("recovery bookmark boss-base/exec_")
        && let Some(id) = rest.strip_suffix(&format!(" {UNRESOLVED_MESSAGE}; found 0"))
    {
        return is_execution_id(id);
    }
    false
}

/// The requested PR base branch cannot be resolved to a commit (no such remote
/// bookmark, or a non-GitHub origin). Unlike a failed fetch or SSH hop, retrying
/// cannot fix it, so it is the only failure that may justify restaging onto a
/// substitute base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseUnresolvableError(pub String);

impl std::fmt::Display for BaseUnresolvableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BaseUnresolvableError {}

pub fn base_unresolvable_error(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(BaseUnresolvableError(message.into()))
}

/// True when `error` (or any cause in its chain) is a base-unresolvable failure.
pub fn is_base_unresolvable_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<BaseUnresolvableError>())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ExecutionBookmark {
    pub execution_id: String,
    pub repo_path: PathBuf,
    pub host_id: String,
    /// The commit id recorded at dispatch: the floor of this execution's work.
    /// Legacy records may lack it, in which case `pr_head_before` is the floor
    /// for a bound PR, then trunk.
    #[serde(default)]
    pub base_commit: Option<String>,
    /// Bound-PR head snapshotted on the execution row. Used when `base_commit`
    /// is missing or no longer in the store, before falling back to trunk.
    #[serde(default)]
    pub pr_head_before: Option<String>,
}

/// The transport is supplied by the host adapter; no engine types belong here.
#[async_trait]
pub trait Jj: Send + Sync {
    async fn workspace_exists(&self, workspace: &Path) -> Result<bool> {
        match std::fs::symlink_metadata(workspace) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

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

/// True when `commit` (a full commit id) exists in the store, visible or not.
async fn commit_exists(jj: &dyn Jj, repo: &Path, commit: &str) -> Result<bool> {
    if commit.is_empty() || !commit.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Ok(false);
    }
    // A missing commit is a resolution failure, not a transport failure:
    // `jj` exits non-zero, which `run` reports as an error we deliberately
    // fold into `false`. Transport errors are indistinguishable here, so the
    // caller only ever treats `false` as "cannot vouch", never as proof.
    Ok(jj
        .run(
            repo,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                commit,
                "-T",
                "commit_id",
            ],
        )
        .await
        .map(|out| out.trim() == commit)
        .unwrap_or(false))
}

async fn usable_commit(jj: &dyn Jj, repo: &Path, commit: Option<&str>) -> Result<Option<String>> {
    match commit {
        Some(commit) if commit_exists(jj, repo, commit).await? => Ok(Some(commit.to_owned())),
        _ => Ok(None),
    }
}

/// The report a restore hands to the successor worker's prompt.
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

/// Record fixtures used only by this crate's own tests. Other crates use
/// `boss_engine_recovery_fixtures`.
#[cfg(test)]
mod test_support;
#[cfg(test)]
pub(crate) use test_support::create;

/// Baseline recording failed after identifying the shared store.
#[derive(Debug)]
pub struct CreationFailure {
    /// Whatever was recorded before the failure.
    pub record: ExecutionBookmark,
}

impl std::fmt::Display for CreationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("could not initialize execution recovery pointers")
    }
}

impl std::error::Error for CreationFailure {}

/// Resolve and record the baseline commit for an execution; no bookmarks are
/// created. A staged revision supplies its copied PR baseline through the restore report.
pub async fn create_from(
    jj: &dyn Jj,
    workspace: &Path,
    execution_id: &str,
    host_id: &str,
    predecessor: Option<&ExecutionBookmark>,
    inherited_base: Option<&str>,
) -> Result<ExecutionBookmark> {
    ensure!(
        !execution_id.is_empty() && execution_id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
        "invalid execution id"
    );
    let mut record = ExecutionBookmark {
        execution_id: execution_id.to_owned(),
        repo_path: jj.shared_repo(workspace).await?,
        host_id: host_id.to_owned(),
        base_commit: None,
        pr_head_before: None,
    };
    let result = async {
        if let Some(prior) = predecessor
            && (prior.repo_path != record.repo_path || prior.host_id != record.host_id)
        {
            return Err(pointer_integrity_error(
                "inherited baseline belongs to another recovery store",
            ));
        }
        // A restore-report, positioned-commit, or `pr_head_before` hash is
        // the floor only when it still exists. A missing hash is not copied,
        // because nothing could ever resolve it. Fall back to the
        // predecessor's resolved floor, then the trunk commit. The positioned
        // workspace's ancestry is never walked to infer a floor.
        let baseline = if let Some(commit) = usable_commit(jj, &record.repo_path, inherited_base).await? {
            commit
        } else if let Some(prior) = predecessor {
            predecessor_floor(jj, prior).await?
        } else {
            one_commit(jj, workspace, "trunk()").await?
        };
        record.base_commit = Some(baseline.clone());
        Ok(record.clone())
    }
    .await;
    result.map_err(|error: anyhow::Error| error.context(CreationFailure { record }))
}

/// Effective floor of a predecessor whose restore report did not supply one:
/// its recorded `base_commit`, else its `pr_head_before`, whichever still
/// exists in the store, else the trunk commit. Never inferred from the
/// successor workspace (which has already been restored onto the
/// predecessor's work) and never an unchecked raw hash.
async fn predecessor_floor(jj: &dyn Jj, prior: &ExecutionBookmark) -> Result<String> {
    for commit in [prior.base_commit.as_deref(), prior.pr_head_before.as_deref()] {
        if let Some(commit) = usable_commit(jj, &prior.repo_path, commit).await? {
            return Ok(commit);
        }
    }
    one_commit(jj, &prior.repo_path, "trunk()").await
}

/// Resolve a revset to exactly one commit id.
pub async fn one_commit(jj: &dyn Jj, repo: &Path, revset: &str) -> Result<String> {
    let output = jj
        .run(repo, &["log", "--no-graph", "-r", revset, "-T", "commit_id ++ \"\\n\""])
        .await?;
    let commits: Vec<_> = output.lines().filter(|s| !s.is_empty()).collect();
    ensure!(
        commits.len() == 1,
        "expected exactly one commit for {revset}; found {}",
        commits.len()
    );
    Ok(commits[0].to_owned())
}

/// The commit a workspace's working copy sits on top of: after cube positions
/// a workspace this is the floor a fresh recovery pointer starts from.
pub async fn positioned_parent(jj: &dyn Jj, workspace: &Path) -> Result<String> {
    one_commit(jj, workspace, "@-").await
}

/// Position a fresh child of the repository's trunk, resolved once, and return
/// the trunk commit it landed on. The one implementation shared by the local
/// adapter and the remote host executor.
pub async fn start_fresh(jj: &dyn Jj, workspace: &Path) -> Result<String> {
    let trunk = one_commit(jj, workspace, "trunk()").await?;
    jj.run(workspace, &["new", &trunk, "-m", "Fresh execution"]).await?;
    Ok(trunk)
}

#[cfg(test)]
mod tests;
