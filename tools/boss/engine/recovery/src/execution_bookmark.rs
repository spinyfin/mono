//! Engine-created recovery references in the shared jj repository.
//!
//! Teardown preserves unpublished workspace edits; workers create their
//! publication branch immediately before opening a PR. Legacy recovery
//! references remain readable so existing records still restore. The baseline
//! is the commit id recorded on the execution row at dispatch (`pr_head_before`
//! for a bound PR, the trunk commit for a fresh item), so rewriting or
//! abandoning any bookmark never invalidates it; published upstream history is
//! excluded when checking for abandoned work.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Editable setup state, without authored content, a message, or merge topology.
const EMPTY_SETUP_CHANGES: &str = r#"(empty() & description(exact:"")) ~ (merges() | root())"#;

mod preserved;
#[cfg(test)]
mod preserved_tests;
pub use preserved::{
    BoundaryOwnership, PreserveInspect, PreserveOutcome, PreservedTip, WorkspaceTipSource, preserve, preserve_from,
    preserved_archive_name, preserved_archive_prefix, preserved_name,
};
mod preserved_restore;
pub use preserved_restore::restore_preserved;

mod pr_anchor;
pub use pr_anchor::is_legacy_pr_anchor_blocker;
#[cfg(test)]
mod pr_anchor_tests;

mod snapshot;
pub use snapshot::{PointerSnapshot, restore_pointers, snapshot_pointers};

mod redispatch;
pub use redispatch::{RestoreReport, restore_rebased};

mod prune;
pub use prune::{PruneOutcome, Voucher, prune};

#[cfg(test)]
mod prune_tests;

#[cfg(test)]
mod archive_tests;

#[cfg(test)]
mod redispatch_tests;

mod reconcile;
pub use reconcile::{
    Anchor, Containment, MissingChange, Reconciliation, RewrittenChange, SupersededPointer, adopt_pushed_head,
    reconcile, resolve_bookmark_commit,
};

#[cfg(test)]
mod reconcile_tests;

/// Which invariant a damaged pointer broke. Lineage between the pointers and
/// the baseline is deliberately not an invariant: jj rewrites commits and
/// merges concurrent operations as a matter of course, so the recorded
/// baseline commit id is a floor, never something the heads must descend from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PointerIntegrityKind {
    /// Reconciliation was attempted and the preserved work is not carried by
    /// anything the engine can vouch for. Nothing was moved.
    NotContained,
    /// The disposable PR bookmark is missing or conflicted; refresh it from GitHub.
    PrHeadUnavailable,
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

/// Neither execution head exists. Dispatch can start cleanly; inspection and
/// crash backups still report this as missing, never as a verified empty run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingPointerError(pub String);

impl std::fmt::Display for MissingPointerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MissingPointerError {}

pub fn is_missing_pointer_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<MissingPointerError>())
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionBookmark {
    pub execution_id: String,
    pub repo_path: PathBuf,
    pub host_id: String,
    /// The baseline commit id recorded at dispatch: the floor of this
    /// execution's work. Recovery reads this, never the `boss-base/…` bookmark.
    /// Legacy records may lack it, in which case `pr_head_before` is the floor
    /// for a bound PR, then trunk.
    #[serde(default)]
    pub base_commit: Option<String>,
    /// Bound-PR head snapshotted on the execution row. Used when `base_commit`
    /// is missing or no longer in the store, before falling back to trunk.
    #[serde(default)]
    pub pr_head_before: Option<String>,
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

/// Resolve a bookmark to the single commit it targets. Commit ids, unlike
/// change ids, stay unique when a change is divergent (two visible commits
/// sharing one change id), so ancestry checks built on them cannot match a
/// sibling of the bookmarked commit. A conflicted bookmark resolves to several
/// commits and is a real pointer-integrity error.
async fn resolve_optional(jj: &dyn Jj, repo: &Path, bookmark: &str) -> Result<Option<String>> {
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
                "commit_id ++ \"\\n\"",
            ],
        )
        .await?;
    let ids: Vec<_> = output.lines().filter(|s| !s.is_empty()).collect();
    if ids.len() > 1 {
        return Err(pointer_integrity_error(format!(
            "recovery bookmark {bookmark} {UNRESOLVED_MESSAGE}; found {}",
            ids.len()
        )));
    }
    Ok(ids.first().map(|id| (*id).to_owned()))
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

fn missing_heads_error(record: &ExecutionBookmark) -> anyhow::Error {
    anyhow::Error::new(MissingPointerError(format!(
        "expected recovery pointer {} or {} to resolve to exactly one change; both are missing",
        record.head(),
        record.publication()
    )))
}

/// The two heads of one execution. Both are optional (cube may have
/// consumed `boss/…`), but never both absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Heads {
    pub(crate) recovery: Option<String>,
    pub(crate) publication: Option<String>,
}

impl Heads {
    /// `(bookmark name, commit)` for each head that exists.
    pub(crate) fn named(&self, record: &ExecutionBookmark) -> Vec<(String, String)> {
        [
            (record.head(), &self.recovery),
            (record.publication(), &self.publication),
        ]
        .into_iter()
        .filter_map(|(name, commit)| commit.clone().map(|commit| (name, commit)))
        .collect()
    }
}

/// The recovery head and the floor of the execution's work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pointers {
    /// A revset naming the floor: the recorded baseline commit id, or `trunk()`.
    pub(crate) base: String,
    pub(crate) heads: Heads,
}

pub(crate) async fn resolve_heads(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<Heads> {
    let recovery = resolve_optional(jj, &record.repo_path, &record.head()).await?;
    let publication = resolve_optional(jj, &record.repo_path, &record.publication()).await?;
    if recovery.is_none() && publication.is_none() {
        return Err(missing_heads_error(record));
    }
    Ok(Heads { recovery, publication })
}

/// The floor of the execution's work: the commit id recorded on its row. A
/// rewritten or abandoned baseline commit still exists in the store, so it
/// keeps working as a floor. A record with no usable `base_commit` (legacy, or
/// collected from the store) uses the bound PR's `pr_head_before`, then trunk.
/// Neither is an error.
pub(crate) async fn resolve_base(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<String> {
    if let Some(commit) = usable_commit(jj, &record.repo_path, record.base_commit.as_deref()).await? {
        return Ok(commit);
    }
    if let Some(commit) = usable_commit(jj, &record.repo_path, record.pr_head_before.as_deref()).await? {
        return Ok(commit);
    }
    Ok("trunk()".to_owned())
}

async fn usable_commit(jj: &dyn Jj, repo: &Path, commit: Option<&str>) -> Result<Option<String>> {
    match commit {
        Some(commit) if commit_exists(jj, repo, commit).await? => Ok(Some(commit.to_owned())),
        _ => Ok(None),
    }
}

pub(crate) async fn resolve_pointers(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<Pointers> {
    let heads = resolve_heads(jj, record).await?;
    let base = resolve_base(jj, record).await?;
    Ok(Pointers { base, heads })
}

/// Work above the floor: everything the head reaches that the floor's own
/// ancestry does not. Ancestry is subtracted by commit id, so the floor needs
/// no bookmark and no descent relationship with the head: a head that was
/// rebased away from it (or a floor that was abandoned) still has a
/// well-defined range.
pub(crate) fn above_floor(base: &str, head: &str) -> String {
    format!("(::({head}) ~ ::({base}))")
}

/// True when `descendant` is reachable from `ancestor` (or is it).
pub(crate) async fn descends(jj: &dyn Jj, repo: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let connected = jj
        .run(
            repo,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                &format!("({ancestor})::({descendant}) & ({descendant})"),
                "-T",
                "commit_id",
            ],
        )
        .await?;
    Ok(connected.trim() == descendant)
}

/// The head bookmarks to resume from. One head normally; the one that
/// descends from the other when they sit on the same line. Two heads on
/// different lines are not an integrity failure and neither is dropped: both
/// are resumed, so work that only one of them carries is still recovered.
pub(crate) async fn resume_heads(jj: &dyn Jj, record: &ExecutionBookmark, pointers: &Pointers) -> Result<Vec<String>> {
    match (&pointers.heads.recovery, &pointers.heads.publication) {
        // Unreachable while `resolve_heads` rejects two missing heads;
        // typed so an edit that moves it degrades instead of panicking.
        (None, None) => Err(missing_heads_error(record)),
        (Some(recovery), Some(publication)) if recovery != publication => {
            if descends(jj, &record.repo_path, recovery, publication).await? {
                return Ok(vec![record.publication()]);
            }
            if descends(jj, &record.repo_path, publication, recovery).await? {
                return Ok(vec![record.head()]);
            }
            Ok(vec![record.head(), record.publication()])
        }
        (Some(_), _) => Ok(vec![record.head()]),
        (None, Some(_)) => Ok(vec![record.publication()]),
    }
}

/// A revset naming every resumed head.
pub(crate) fn heads_revset(heads: &[String]) -> String {
    format!(
        "({})",
        heads.iter().map(|head| revision(head)).collect::<Vec<_>>().join(" | ")
    )
}

pub(crate) async fn resume_heads_of(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<Vec<String>> {
    let pointers = resolve_pointers(jj, record).await?;
    resume_heads(jj, record, &pointers).await
}

/// Compatibility fixtures that still need the pre-migration three-bookmark layout.
pub mod test_support;

/// Compatibility alias for [`test_support::create_legacy_three_bookmark_fixture`].
/// Dispatch uses [`create_from`], which never creates baseline or publication names.
pub use test_support::{create_legacy_three_bookmark_fixture as create, create_legacy_three_bookmark_fixture_from};

/// Pointer initialization failed after identifying the shared store. Dispatch
/// archives these pointers before creating fresh ones.
#[derive(Debug)]
pub struct CreationFailure {
    /// Includes partially created pointers, even before the database row exists.
    pub record: ExecutionBookmark,
}

impl std::fmt::Display for CreationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("could not initialize execution recovery pointers")
    }
}

impl std::error::Error for CreationFailure {}

/// Preserve inherited unpublished work across consecutive interrupted runs.
/// A staged revision supplies its copied PR baseline through the restore report.
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
        // the floor only when it still exists. A missing hash is not copied:
        // `jj bookmark create` would fail on every retry. Fall back to the
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
        // Fresh dispatch records only its starting commit. Retain the legacy
        // recovery head only when the positioned lease carries unpublished work;
        // teardown preserves subsequent edits from the workspace itself.
        let unpublished = jj
            .run(
                workspace,
                &[
                    "log",
                    "--no-graph",
                    "-r",
                    &format!("({baseline}..@) ~ (::remote_bookmarks() | empty())"),
                    "-T",
                    "commit_id",
                ],
            )
            .await?;
        if !unpublished.trim().is_empty() {
            // Only the recovery head is created here. A leftover baseline or
            // publication name from a partial legacy initialization is not a
            // conflict: dispatch neither writes nor needs those names.
            let targets = snapshot::resolve_bookmark_targets(jj, &record.repo_path, &[record.head()]).await?;
            if !targets.is_empty() {
                return Err(pointer_integrity_error(
                    "execution recovery pointer names already exist",
                ));
            }
            jj.run(workspace, &["bookmark", "create", &record.head(), "-r", "@"])
                .await?;
        }
        Ok(record.clone())
    }
    .await;
    result.map_err(|error: anyhow::Error| error.context(CreationFailure { record }))
}

/// Effective floor of a predecessor whose restore report did not supply one:
/// the floor `resolve_base` computes (its recorded `base_commit`, else
/// `pr_head_before`, else trunk), as a commit id. The legacy `boss-base/`
/// bookmark is never read. Never inferred from the successor workspace (which
/// has already been restored onto the predecessor heads) and never an
/// unchecked raw hash.
async fn predecessor_floor(jj: &dyn Jj, prior: &ExecutionBookmark) -> Result<String> {
    one_commit(jj, &prior.repo_path, &resolve_base(jj, prior).await?).await
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

/// Clean reinitialisation for a self-retry whose own heads are both gone: drop
/// the surviving baseline so fresh pointers can be created for the same
/// execution. Refuses unless both heads are confirmed absent, so it can never
/// discard provenance that a live pointer still depends on.
pub async fn discard_orphaned_baseline(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<()> {
    for head in [record.head(), record.publication()] {
        ensure!(
            resolve_optional(jj, &record.repo_path, &head).await?.is_none(),
            "refusing to discard baseline {}: pointer {head} still exists",
            record.base()
        );
    }
    if resolve_optional(jj, &record.repo_path, &record.base()).await?.is_some() {
        jj.run(&record.repo_path, &["bookmark", "delete", &record.base()])
            .await?;
    }
    Ok(())
}

/// Archive unpublished work before releasing private recovery and baseline names.
/// Release the publication name too only when this execution must reuse it.
/// Baseline, trunk, remote history and empty-only ranges need no archive. Conflicted bookmarks can have several targets; retain them all.
/// Archive names include the commit identity so repeated fallbacks never
/// overwrite earlier preserved work. Pruning only touches the three run names.
pub async fn release_heads(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    release_names: bool,
) -> Result<Vec<(String, String)>> {
    let names = [record.base(), record.head(), record.publication()];
    let targets = snapshot::resolve_bookmark_targets(jj, &record.repo_path, &names).await?;
    let mut commits: std::collections::BTreeSet<String> = targets.values().flatten().cloned().collect();
    if let Some(base) = &record.base_commit
        && commit_exists(jj, &record.repo_path, base).await?
    {
        commits.insert(base.clone());
    }
    let mut archived = Vec::new();
    let baseline = record.base_commit.as_deref();
    for commit in commits {
        let base = format!("present({})", baseline.unwrap_or("root()"));
        let unpublished = jj
            .run(
                &record.repo_path,
                &[
                    "log",
                    "--no-graph",
                    "-r",
                    &format!("({base}..{commit}) ~ (::remote_bookmarks() | ::trunk() | empty())"),
                    "-T",
                    "commit_id",
                ],
            )
            .await?;
        if unpublished.trim().is_empty() {
            continue;
        }
        let archive = preserved_archive_name(&record.execution_id, &commit);
        jj.run(&record.repo_path, &["bookmark", "set", &archive, "-r", &commit])
            .await?;
        archived.push((archive, commit));
    }
    // Preserve predecessor publication names, which may still be tracked PR branches.
    // Delete retains remote tracking; forget would untrack a pushed PR branch.
    let mut args = vec!["bookmark", "delete"];
    args.extend(
        names
            .iter()
            .filter(|name| targets.contains_key(*name) && (release_names || **name != record.publication()))
            .map(String::as_str),
    );
    if args.len() > 2 {
        jj.run(&record.repo_path, &args).await?;
    }
    Ok(archived)
}

/// Diff for each resumed head, concatenated, from the newest common ancestor
/// of the head with the floor or trunk. Bounding by trunk keeps upstream
/// changes a rebase pulled in out of the patch.
/// A successful empty patch proves an empty run (including a head that shares
/// no history with the floor beyond the fork, such as `root()`). Missing or
/// conflicted references and unavailable jj are errors, never empty results.
pub async fn diff(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<String> {
    let pointers = resolve_pointers(jj, record).await?;
    let mut patches = String::new();
    for head in resume_heads(jj, record, &pointers).await? {
        let head = revision(&head);
        let patch = jj
            .run(
                &record.repo_path,
                &[
                    "--ignore-working-copy",
                    "diff",
                    "--git",
                    "--from",
                    // Tolerates a floor that the head no longer descends from
                    // (a rewrite or an abandon). Bounding by trunk keeps the
                    // upstream changes a rebase pulled in out of the patch.
                    &format!("fork_point(heads(::({head}) & (::({}) | ::trunk())))", pointers.base),
                    "--to",
                    &head,
                ],
            )
            .await?;
        patches.push_str(&crate::recovery_apply::filter_bookkeeping(&patch).text);
    }
    Ok(patches)
}

/// Fork from the reference so later edits cannot rewrite the predecessor's work.
pub async fn restore(jj: &dyn Jj, record: &ExecutionBookmark, workspace: &Path) -> Result<bool> {
    restore_for_retry(jj, record, workspace, false).await
}

/// A retry must keep its own baseline and pointers in the working history,
/// even when that execution has no unpublished diff.
pub async fn restore_for_retry(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    self_retry: bool,
) -> Result<bool> {
    ensure!(
        jj.shared_repo(workspace).await? == record.repo_path,
        "recovery destination belongs to a different shared repository"
    );
    let has_work = !unpublished_diff(jj, record).await?.trim().is_empty();
    if !has_work && !self_retry {
        return Ok(false);
    }
    let heads = resume_heads_of(jj, record).await?;
    let head = heads_revset(&heads);
    if self_retry && heads.len() == 1 {
        let empty_setup = jj
            .run(
                workspace,
                &[
                    "log",
                    "--no-graph",
                    "-r",
                    &format!("{head} & ({EMPTY_SETUP_CHANGES})"),
                    "-T",
                    "commit_id",
                ],
            )
            .await?;
        if !empty_setup.trim().is_empty() {
            // This execution still owns its untouched editable change. Resume
            // it, so a retry cannot strand it below a newly authored child.
            // Its existing baseline and head pointers continue to follow edits.
            jj.run(workspace, &["edit", &head]).await?;
            return Ok(has_work);
        }
    }
    // Several heads (different lines) become the parents of one resumed change.
    let mut args = vec!["new".to_owned()];
    args.extend(heads.iter().map(|head| revision(head)));
    args.extend(["-m".to_owned(), "Resume recovered execution work".to_owned()]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    jj.run(workspace, &args).await?;
    Ok(has_work)
}

/// Ignore work already reachable from a remote bookmark, including an empty
/// working-copy child left after publishing. Still validate both local refs.
pub async fn unpublished_diff(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<String> {
    let patch = diff(jj, record).await?;
    if patch.trim().is_empty() {
        return Ok(patch);
    }
    let pointers = resolve_pointers(jj, record).await?;
    let range = format!(
        "({} ~ ::remote_bookmarks()) & ~empty()",
        above_floor(
            &pointers.base,
            &heads_revset(&resume_heads(jj, record, &pointers).await?)
        )
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
