//! Engine-created recovery references in the shared jj repository.
//!
//! The worker advances the execution bookmark before editing a new change;
//! jj itself follows rewrites of the bookmarked change. Recovery never reads
//! the originating working copy. A separate baseline preserves the initial
//! state; published upstream history is excluded when checking for abandoned work.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

mod baseline;
pub use baseline::checkpoint_baseline;

mod pr_anchor;
pub use pr_anchor::{is_legacy_pr_anchor_blocker, repair_pr_anchor};
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
mod redispatch_tests;

mod reconcile;
pub use reconcile::{
    Anchor, Containment, MissingChange, Reconciliation, RewriteEvidence, RewrittenChange, SupersededPointer,
    adopt_pushed_head, reconcile, resolve_bookmark_commit,
};

#[cfg(test)]
mod reconcile_tests;

/// Which invariant a damaged pointer broke. The engine repairs the first
/// three only against pointers it recorded itself (the PR head cube wrote,
/// the baseline commit in the record, or the other head of the same
/// execution). A missing or conflicted PR alias requires a fresh GitHub head
/// lookup; other pointer failures cannot be repaired by merely re-running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PointerIntegrityKind {
    /// A head no longer descends from `boss-base/<execution id>`: the
    /// baseline sits on a variant of its change that the head's lineage
    /// does not include (the change was rewritten under it).
    NotDescended,
    /// `boss-recovery/…` and `boss/…` are on different lines of history:
    /// one followed the published PR lineage, the other a local rewrite.
    Diverged,
    /// `boss-base/<execution id>` no longer exists and the record carries
    /// no baseline commit (or that commit is gone) while a head still does.
    MissingBaseline,
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

/// The message a pointer that is not descended from its baseline reports,
/// followed by ` boss-base/<execution id>` and the commits involved. Shared so a stored work-item blocker can
/// be recognised exactly, rather than by a copied string.
pub const NOT_DESCENDED_MESSAGE: &str = "preserved execution pointer is not descended from its engine-created baseline";

/// The sentence two preserved pointers on different lines report, preceded
/// by `preserved pointers <recovery> and <publication> `.
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

/// True when `error` is a pointer-shape failure that running the same
/// dispatch again cannot change: the preserved pointers do not descend from
/// their baseline, have diverged, have lost their baseline, or (after
/// reconciliation was attempted) carry work nothing recorded vouches for.
/// Those depend only on the pointers in the store and the PR head they were
/// checked against, and both stay where they are until a human moves them or
/// a push lands. [`PointerIntegrityKind::PrHeadUnavailable`] is not one of
/// them: cube rewrites the `pr/<n>` alias on every attempt, so a retry
/// repeats the GitHub lookup that may repair it. Neither is
/// [`PointerIntegrityKind::Other`], whose causes include a record that lives
/// on a different host than the one dispatch selected, which another attempt
/// may select differently.
pub fn is_deterministic_pointer_error(error: &anyhow::Error) -> bool {
    matches!(
        pointer_integrity_kind(error),
        Some(
            PointerIntegrityKind::NotDescended
                | PointerIntegrityKind::Diverged
                | PointerIntegrityKind::MissingBaseline
                | PointerIntegrityKind::NotContained
        )
    )
}

/// True when `error` is one of the failures [`reconcile`] can repair from an
/// engine-recorded pointer: a stranded or missing baseline, or diverged heads.
pub fn is_reconcilable_pointer_error(error: &anyhow::Error) -> bool {
    matches!(
        pointer_integrity_kind(error),
        Some(
            PointerIntegrityKind::NotDescended | PointerIntegrityKind::Diverged | PointerIntegrityKind::MissingBaseline
        )
    )
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
/// report) is still a real pointer problem and stays blocked. Exact shapes
/// only: a trailing context or a changed wording is not recognised.
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
    /// The last checkpointed baseline commit. This identity, or a verified
    /// evolution successor, can rebuild a missing baseline bookmark.
    /// Legacy records may lack this provenance.
    #[serde(default)]
    pub base_commit: Option<String>,
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

/// True when `commit` is an ancestor of (or is) every head of the record.
async fn commit_in_ancestry(jj: &dyn Jj, record: &ExecutionBookmark, commit: &str, heads: &Heads) -> Result<bool> {
    let ancestry = heads
        .named(record)
        .iter()
        .map(|(_, head)| format!("::{head}"))
        .collect::<Vec<_>>()
        .join(" & ");
    let out = jj
        .run(
            &record.repo_path,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                &format!("{commit} & ({ancestry})"),
                "-T",
                "commit_id",
            ],
        )
        .await?;
    Ok(out.trim() == commit)
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

/// The commits the engine's three references for one execution resolve to.
/// Produced by [`resolve_pointers`], consumed by the lineage check and by
/// reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pointers {
    pub(crate) base: String,
    /// False when `boss-base/…` is gone and `base` came from the record's
    /// `base_commit` instead; the bookmark can be recreated from it.
    pub(crate) base_bookmarked: bool,
    pub(crate) heads: Heads,
}

impl Pointers {
    /// `(bookmark name, commit)` for each head that exists.
    pub(crate) fn heads(&self, record: &ExecutionBookmark) -> Vec<(String, String)> {
        self.heads.named(record)
    }
}

pub(crate) async fn resolve_heads(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<Heads> {
    let recovery = resolve_optional(jj, &record.repo_path, &record.head()).await?;
    let publication = resolve_optional(jj, &record.repo_path, &record.publication()).await?;
    if recovery.is_none() && publication.is_none() {
        // Checked before the baseline so a fully deleted record reports as
        // missing rather than as a baseline integrity failure.
        return Err(missing_heads_error(record));
    }
    Ok(Heads { recovery, publication })
}

fn missing_baseline_error(record: &ExecutionBookmark, heads: &Heads) -> anyhow::Error {
    let at = heads
        .named(record)
        .into_iter()
        .map(|(bookmark, commit)| format!("{bookmark} is at {commit}"))
        .collect::<Vec<_>>()
        .join(", ");
    let recorded = match &record.base_commit {
        Some(commit) => format!("; the recorded baseline commit {commit} is not in the store"),
        None => "; the record carries no baseline commit".to_owned(),
    };
    pointer_integrity_error_of(
        PointerIntegrityKind::MissingBaseline,
        format!(
            "recovery bookmark {} {UNRESOLVED_MESSAGE}; found 0 ({at}{recorded})",
            record.base()
        ),
    )
}

/// The baseline commit: the `boss-base/…` bookmark when it exists, else the
/// recorded commit, following verified evolution to a common ancestor of
/// the heads when necessary. `Err(MissingBaseline)` when neither can be found.
pub(crate) async fn resolve_base(jj: &dyn Jj, record: &ExecutionBookmark, heads: &Heads) -> Result<(String, bool)> {
    if let Some(base) = resolve_optional(jj, &record.repo_path, &record.base()).await? {
        let evolved = baseline::evolved_baseline(jj, record, &base, heads).await?;
        let unchanged = evolved == base;
        return Ok((evolved, unchanged));
    }
    if let Some(commit) = &record.base_commit
        && commit_exists(jj, &record.repo_path, commit).await?
    {
        let evolved = baseline::evolved_baseline(jj, record, commit, heads).await?;
        // An abandoned commit that was not rewritten and that the heads no
        // longer descend from cannot serve as a baseline.
        if evolved != *commit || commit_in_ancestry(jj, record, commit, heads).await? {
            return Ok((evolved, false));
        }
    }
    Err(missing_baseline_error(record, heads))
}

pub(crate) async fn resolve_pointers(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<Pointers> {
    let heads = resolve_heads(jj, record).await?;
    let (base, base_bookmarked) = resolve_base(jj, record, &heads).await?;
    Ok(Pointers {
        base,
        base_bookmarked,
        heads,
    })
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

/// Pick the head to resume from: the one that descends from the other, both
/// descended from the baseline. The two repairable failures name every
/// commit involved so a blocked item can be understood without a jj session.
pub(crate) async fn head_bookmark_of(jj: &dyn Jj, record: &ExecutionBookmark, pointers: &Pointers) -> Result<String> {
    let base = &pointers.base;
    for (bookmark, head) in pointers.heads(record) {
        if !descends(jj, &record.repo_path, base, &head).await? {
            return Err(pointer_integrity_error_of(
                PointerIntegrityKind::NotDescended,
                format!(
                    "{NOT_DESCENDED_MESSAGE} {} ({base}); {bookmark} is at {head}",
                    record.base()
                ),
            ));
        }
    }
    match (&pointers.heads.recovery, &pointers.heads.publication) {
        // Unreachable while `resolve_heads` rejects two missing heads;
        // typed so an edit that moves it degrades instead of panicking.
        (None, None) => Err(missing_heads_error(record)),
        (Some(recovery), Some(publication)) if recovery != publication => {
            if descends(jj, &record.repo_path, publication, recovery).await? {
                return Ok(record.head());
            }
            if descends(jj, &record.repo_path, recovery, publication).await? {
                return Ok(record.publication());
            }
            Err(pointer_integrity_error_of(
                PointerIntegrityKind::Diverged,
                format!(
                    "preserved pointers {} and {} {DIVERGED_MESSAGE} ({} is at {recovery}, {} is at {publication}; baseline {} is at {base})",
                    record.head(),
                    record.publication(),
                    record.head(),
                    record.publication(),
                    record.base()
                ),
            ))
        }
        (Some(_), _) => Ok(record.head()),
        (None, Some(_)) => Ok(record.publication()),
    }
}

async fn head_bookmark(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<String> {
    let pointers = resolve_pointers(jj, record).await?;
    head_bookmark_of(jj, record, &pointers).await
}

/// Called after positioning, before a worker can run. Existing refs are errors:
/// retry callers must use their persisted record, never overwrite provenance.
pub async fn create(jj: &dyn Jj, workspace: &Path, execution_id: &str, host_id: &str) -> Result<ExecutionBookmark> {
    create_from(jj, workspace, execution_id, host_id, None, None).await
}

/// Preserve inherited unpublished work across consecutive interrupted runs,
/// even when the next worker makes no additional edits. A staged revision
/// supplies its copied PR baseline through the persisted restore report.
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
    };
    let baseline = if let Some(prior) = predecessor {
        ensure!(
            prior.repo_path == record.repo_path && prior.host_id == record.host_id,
            "inherited baseline belongs to another recovery store"
        );
        diff(jj, prior).await?;
        inherited_base
            .map(str::to_owned)
            .unwrap_or_else(|| revision(&prior.base()))
    } else {
        baseline::durable_baseline(jj, workspace).await?
    };
    jj.run(workspace, &["bookmark", "create", &record.base(), "-r", &baseline])
        .await?;
    jj.run(
        workspace,
        &["bookmark", "create", &record.head(), &record.publication(), "-r", "@"],
    )
    .await?;
    // The bookmark is the convenience; the commit is the baseline's identity.
    record.base_commit = Some(resolve_created_base(jj, &record).await?);
    // Validate through the shared store, not the workspace we just positioned.
    diff(jj, &record).await?;
    Ok(record)
}

async fn resolve_created_base(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<String> {
    resolve_optional(jj, &record.repo_path, &record.base())
        .await?
        .ok_or_else(|| pointer_integrity_error(format!("recovery bookmark {} was not created", record.base())))
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

/// Deliberate fresh start: drop both head bookmarks so the same execution
/// can create fresh pointers. Returns `(bookmark, commit)` for each head that
/// existed so the caller records where the superseded commits are; the
/// commits themselves stay in the store. Any surviving baseline goes too.
pub async fn release_heads(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<Vec<(String, String)>> {
    let heads = resolve_heads(jj, record).await?;
    let named = heads.named(record);
    let mut names: Vec<&str> = named.iter().map(|(bookmark, _)| bookmark.as_str()).collect();
    let base = record.base();
    if resolve_optional(jj, &record.repo_path, &base).await?.is_some() {
        names.push(&base);
    }
    let mut args = vec!["bookmark", "delete"];
    args.extend(names);
    jj.run(&record.repo_path, &args).await?;
    Ok(named)
}

/// A successful empty diff proves an empty run. Missing/conflicted references,
/// unavailable jj, and unrelated targets are errors, never empty results.
pub async fn diff(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<String> {
    let pointers = resolve_pointers(jj, record).await?;
    let head_bookmark = head_bookmark_of(jj, record, &pointers).await?;
    let patch = jj
        .run(
            &record.repo_path,
            &[
                "--ignore-working-copy",
                "diff",
                "--git",
                "--from",
                &pointers.base,
                "--to",
                &revision(&head_bookmark),
            ],
        )
        .await?;
    Ok(crate::recovery_apply::filter_bookkeeping(&patch).text)
}

/// Validate the pointers and, when `boss-base/…` is gone but the record's
/// own commit vouched for the heads, recreate the bookmark there. Called on
/// the paths that are about to hand the history to a worker, so everything
/// downstream (and the worker) sees the bookmark the record describes.
pub(crate) async fn heal_baseline(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<Pointers> {
    let pointers = resolve_pointers(jj, record).await?;
    head_bookmark_of(jj, record, &pointers).await?;
    if !pointers.base_bookmarked {
        jj.run(
            &record.repo_path,
            &[
                "bookmark",
                "set",
                &record.base(),
                "-r",
                &pointers.base,
                "--allow-backwards",
            ],
        )
        .await?;
    }
    Ok(pointers)
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
    heal_baseline(jj, record).await?;
    let has_work = !unpublished_diff(jj, record).await?.trim().is_empty();
    if !has_work && !self_retry {
        return Ok(false);
    }
    jj.run(
        workspace,
        &[
            "new",
            &revision(&head_bookmark(jj, record).await?),
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
    let pointers = resolve_pointers(jj, record).await?;
    let range = format!(
        "(({})::{} ~ ::remote_bookmarks()) & ~empty()",
        pointers.base,
        revision(&head_bookmark_of(jj, record, &pointers).await?)
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

/// Confirm the shared-store pointers alone: a usable baseline and an existing
/// recovery head. Used when the workspace directory itself no longer exists,
/// so there is no working copy whose commit could need covering.
pub async fn confirm_pointers(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<()> {
    resolve_pointers(jj, record).await?;
    resolve_optional(jj, &record.repo_path, &record.head())
        .await?
        .ok_or_else(|| pointer_integrity_error("recovery head is missing"))?;
    Ok(())
}

/// Verify that the recovery head preserves the current workspace commit,
/// after snapshotting pending file edits on the owning host. When the
/// workspace has moved forward from the head (for example a trailing
/// `jj new`), the head is advanced to the workspace commit first, so an
/// ordinary teardown does not depend on the worker having re-set it. A head
/// that is not an ancestor of the workspace commit is never moved.
pub async fn confirm_workspace(jj: &dyn Jj, record: &ExecutionBookmark, workspace: &Path) -> Result<()> {
    anyhow::ensure!(
        jj.shared_repo(workspace).await? == record.repo_path,
        "workspace recovery store changed"
    );
    let current = jj
        .run(workspace, &["log", "--no-graph", "-r", "@", "-T", "commit_id"])
        .await?;
    let current = current.trim();
    // Resume also needs a usable baseline, not merely a retained head.
    resolve_pointers(jj, record).await?;
    let head = resolve_optional(jj, &record.repo_path, &record.head())
        .await?
        .ok_or_else(|| pointer_integrity_error("recovery head is missing"))?;
    if descends(jj, &record.repo_path, current, &head).await? {
        return Ok(());
    }
    anyhow::ensure!(
        descends(jj, &record.repo_path, &head, current).await?,
        "recovery head does not preserve the workspace commit"
    );
    jj.run(&record.repo_path, &["bookmark", "set", &record.head(), "-r", current])
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests;
