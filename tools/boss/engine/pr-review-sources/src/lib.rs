//! Immutable, source-grounded packets for PR review guides.
//!
//! This crate owns the portable packet contract and the GitHub-backed
//! collector. It deliberately has no database or engine dependency: core owns
//! reconciliation, observation ordering, and durable persistence while this
//! crate only reads pinned GitHub objects and validates references against the
//! resulting packet.

use futures_util::{StreamExt, stream};
use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Version 3 records REST `observed_base_sha` as the comparison identity
/// (the base that determined `merge_base_sha`) and optional `probe_base_sha`
/// for a GraphQL `baseRefOid` that was allowed to differ. Per-side
/// `terminal` on omissions makes retryability data rather than a substring
/// of `reason`.
const PACKET_SCHEMA_VERSION: u32 = 3;

/// Per-file capture budget. A single side larger than this is recorded as a
/// [`SourceOmission`] instead of being inlined into the packet.
pub const MAX_PINNED_SOURCE_BYTES: u64 = 1_048_576;

const BINARY_SOURCE_OMISSION: &str =
    "pinned source is binary or non-UTF-8; raw-byte SHA-256 was recorded without lossy decoding";
const SYMLINK_SOURCE_OMISSION: &str =
    "pinned tree entry is a symlink; omitted so Contents API cannot follow it and mis-attribute the target's bytes";

/// Immutable endpoints supplied by a reconciler observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedComparison {
    pub base_sha: String,
    pub head_sha: String,
}

/// A source packet frozen for exactly one PR comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct SourcePacket {
    pub schema_version: u32,
    pub canonical_pr_url: String,
    pub pr_number: u64,
    pub title: String,
    pub body: Option<String>,
    pub base_repository: String,
    pub head_repository: String,
    /// REST `pulls/{n}` `base.sha` — the identity that determined
    /// [`Self::merge_base_sha`]. Comparison rows and in-flight capture keys
    /// use this value, not a GraphQL `baseRefOid` that may track the live
    /// base tip.
    pub observed_base_sha: String,
    /// GraphQL `baseRefOid` from an independently observed lifecycle probe.
    /// `None` when the observation was the same REST resource that supplied
    /// [`Self::observed_base_sha`].
    #[serde(default)]
    pub probe_base_sha: Option<String>,
    pub merge_base_sha: String,
    pub head_sha: String,
    pub files: Vec<SourceFile>,
    pub omissions: Vec<SourceOmission>,
}

impl SourcePacket {
    /// Settled-packet predicate stored as the durable `complete` column.
    ///
    /// A packet is complete when no requested source side can still become
    /// capturable: each side was read, or omitted for a reason that is a
    /// property of the immutable revision (see [`PinnedSource::is_settled`]).
    /// A complete packet may therefore contain sides with no `content` —
    /// symlink, submodule, over-budget, and tree-absent omissions all settle.
    /// A missing API patch remains a packet-level omission and does not by
    /// itself keep the packet incomplete.
    pub fn is_complete(&self) -> bool {
        self.files.iter().all(SourceFile::is_complete)
    }

    /// Symmetric alias of [`Self::is_complete`] — the per-side predicate is
    /// [`PinnedSource::is_settled`].
    pub fn is_settled(&self) -> bool {
        self.is_complete()
    }

    /// Stable SHA-256 over the serialized source contract, for storage and
    /// diagnostic integrity checks. The digest is not embedded in the packet
    /// itself, avoiding a self-referential serialization contract.
    pub fn content_hash(&self) -> Result<String> {
        let bytes = serde_json::to_vec(self).context("serialize source packet for hashing")?;
        Ok(hex_digest(&bytes))
    }
}

/// One changed file and the pinned source material available for its two
/// sides. `before` is read from the merge base, never from a mutable base
/// branch; `after` is read from the PR head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct SourceFile {
    pub path: String,
    pub previous_path: Option<String>,
    pub change_kind: ChangeKind,
    pub additions: u64,
    pub deletions: u64,
    pub patch: Option<String>,
    pub before: Option<PinnedSource>,
    pub after: Option<PinnedSource>,
}

impl SourceFile {
    fn is_complete(&self) -> bool {
        self.before.as_ref().is_none_or(PinnedSource::is_settled)
            && self.after.as_ref().is_none_or(PinnedSource::is_settled)
    }
}

/// A source side captured at an immutable revision. The content is retained
/// in the packet so later diagnostics never need to resolve a branch again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct PinnedSource {
    pub repository: String,
    pub sha: String,
    pub path: String,
    /// Git object identity from the tree manifest. It is distinct from the
    /// commit SHA above and remains useful when textual content is omitted.
    pub object_sha: Option<String>,
    /// Git tree mode, preserving ordinary files, symlinks, and submodules.
    pub mode: Option<String>,
    /// GitHub tree-entry type, usually `blob` or `commit` for a submodule.
    pub object_type: Option<String>,
    pub content: Option<String>,
    pub content_hash: Option<String>,
    pub byte_count: Option<u64>,
    pub omission: Option<String>,
    /// When `omission` is set, whether this side is settled (`Some(true)`)
    /// or should be retried (`Some(false)`). `None` only on packets
    /// deserialized from schema v2, which lacked the field;
    /// [`Self::is_settled`] then applies the legacy reason-string
    /// classification so already-stored packets keep their retry behavior.
    #[serde(default)]
    pub terminal: Option<bool>,
}

impl PinnedSource {
    fn captured(
        repository: &str,
        sha: &str,
        path: &str,
        content: String,
        entry: &boss_github::trees::PinnedTreeEntry,
    ) -> Self {
        let byte_count = content.len() as u64;
        let content_hash = hex_digest(content.as_bytes());
        Self {
            repository: repository.to_owned(),
            sha: sha.to_owned(),
            path: path.to_owned(),
            object_sha: Some(entry.object_sha.clone()),
            mode: Some(entry.mode.clone()),
            object_type: Some(entry.object_type.clone()),
            content: Some(content),
            content_hash: Some(content_hash),
            byte_count: Some(byte_count),
            omission: None,
            terminal: None,
        }
    }

    fn omitted(
        repository: &str,
        sha: &str,
        path: &str,
        omission: String,
        entry: Option<&boss_github::trees::PinnedTreeEntry>,
        terminal: bool,
    ) -> Self {
        Self {
            repository: repository.to_owned(),
            sha: sha.to_owned(),
            path: path.to_owned(),
            object_sha: entry.map(|entry| entry.object_sha.clone()),
            mode: entry.map(|entry| entry.mode.clone()),
            object_type: entry.map(|entry| entry.object_type.clone()),
            content: None,
            content_hash: None,
            byte_count: entry.and_then(|entry| entry.size),
            omission: Some(omission),
            terminal: Some(terminal),
        }
    }

    fn omitted_binary(
        repository: &str,
        sha: &str,
        path: &str,
        bytes: Vec<u8>,
        entry: &boss_github::trees::PinnedTreeEntry,
    ) -> Self {
        Self {
            repository: repository.to_owned(),
            sha: sha.to_owned(),
            path: path.to_owned(),
            object_sha: Some(entry.object_sha.clone()),
            mode: Some(entry.mode.clone()),
            object_type: Some(entry.object_type.clone()),
            content: None,
            content_hash: Some(hex_digest(&bytes)),
            byte_count: Some(bytes.len() as u64),
            omission: Some(BINARY_SOURCE_OMISSION.to_owned()),
            terminal: Some(true),
        }
    }

    fn is_captured(&self) -> bool {
        self.object_sha.is_some()
            && self.content_hash.is_some()
            && self.byte_count.is_some()
            && (self.content.is_some() || self.omission.as_deref() == Some(BINARY_SOURCE_OMISSION))
    }

    /// True when this side will not become capturable later: it was read
    /// successfully, or the omission is a property of the immutable revision
    /// (symlink, submodule, over-budget, absent) rather than a retryable
    /// tree/Contents request failure.
    fn is_settled(&self) -> bool {
        if self.is_captured() {
            return true;
        }
        let Some(reason) = self.omission.as_ref() else {
            return false;
        };
        match self.terminal {
            Some(terminal) => terminal,
            None => !legacy_retryable_omission(reason),
        }
    }
}

/// Schema v2 packets stored retryability only as substrings of `reason`.
/// New packets set [`PinnedSource::terminal`] at construction; this helper
/// exists solely so already-persisted v2 rows keep the same settle/retry
/// behavior when deserialized without the field.
fn legacy_retryable_omission(reason: &str) -> bool {
    reason.contains("could not read pinned tree")
        || reason.contains("pinned source read failed")
        || reason.contains("directory response was truncated")
}

/// GitHub's Trees API 404 (`NotFound`) and 401/403 (`NotAuthorized`) are
/// ambiguous for private repos — GitHub returns 404 rather than 403 when
/// the token cannot see a private repo. `NotFound` is treated as terminal:
/// a deleted head fork or a SHA that no longer resolves will not become
/// capturable at this comparison. `NotAuthorized` stays retryable so
/// restoring a token can recover the same immutable SHA, matching
/// rate-limited and unreachable tree failures.
fn tree_error_is_terminal(kind: boss_github::trees::TreeApiErrorKind) -> bool {
    matches!(kind, boss_github::trees::TreeApiErrorKind::NotFound)
}

/// GitHub's changed-file classification, preserving the API value instead of
/// flattening a rename or deletion into an ordinary modification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Copied,
    Deleted,
    Modified,
    Renamed,
    Changed,
    /// GitHub reported a status this collector does not recognise (including
    /// documented values such as `unchanged`). Both sides are still fetched;
    /// the raw status is retained so the packet does not silently claim a
    /// modification.
    Unknown(String),
}

impl ChangeKind {
    fn from_api(status: &str) -> Self {
        match status.to_ascii_lowercase().as_str() {
            "added" => Self::Added,
            "copied" => Self::Copied,
            "deleted" | "removed" => Self::Deleted,
            "renamed" => Self::Renamed,
            "changed" => Self::Changed,
            "modified" => Self::Modified,
            other => Self::Unknown(other.to_owned()),
        }
    }

    fn has_before(&self) -> bool {
        !matches!(self, Self::Added | Self::Copied)
    }

    fn has_after(&self) -> bool {
        !matches!(self, Self::Deleted)
    }
}

/// A decisive collection limitation. These are packet data, not log-only
/// warnings, so a later guide validator can fail closed with the real cause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceOmission {
    pub path: Option<String>,
    pub side: Option<SourceSide>,
    pub reason: String,
    /// Copied from the corresponding [`PinnedSource::terminal`] (or set
    /// directly for packet-level omissions). Diagnostic; settling reads
    /// the per-side field. `false` on schema v2 rows that lack the key.
    #[serde(default)]
    pub terminal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceSide {
    Before,
    After,
}

/// A validated source range and its immutable GitHub permalink.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceReference {
    pub side: SourceSide,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub href: String,
}

/// Rendering adapters may use a mutable PR-diff target only after independently
/// validating its file anchor/side/line against a captured hunk for this
/// comparison. The Files-changed adapter constructs
/// [`RenderedTarget::Validated`] only when the cited range sits inside a
/// unified-diff hunk on the owning file and the packet still names the live
/// comparison; otherwise it falls back to the immutable blob permalink.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RenderedTarget {
    Validated {
        href: String,
        adapter_version: String,
        evidence: RenderedTargetEvidence,
    },
    PinnedFallback {
        href: String,
        reason: String,
    },
}

/// Cached proof that a Files-changed fragment corresponded to a captured
/// hunk at adapter version [`RENDERED_TARGET_ADAPTER_VERSION`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct RenderedTargetEvidence {
    pub head_sha: String,
    pub merge_base_sha: String,
    /// Inventory path whose SHA-256 is the GitHub `#diff-` hash (the current
    /// filename, including on a rename's Before side).
    pub file_path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub side: SourceSide,
    /// Unified-diff hunk header that contained `[start_line, end_line]`.
    pub hunk: String,
}

/// The Files-changed adapter version. Fragment construction is
/// `sha256(inventory path)` as lowercase hex, and Validated requires the
/// cited range to sit inside a captured hunk for the bound comparison.
pub const RENDERED_TARGET_ADAPTER_VERSION: &str = "files-changed-diff-fragment-v2";

/// Validate a range against the captured source and build a full-SHA permalink.
pub fn validate_pinned_reference(
    packet: &SourcePacket,
    side: SourceSide,
    path: &str,
    start_line: u32,
    end_line: u32,
) -> Result<SourceReference> {
    if start_line == 0 || end_line < start_line {
        bail!("invalid source range {start_line}..{end_line}");
    }
    let source = packet
        .files
        .iter()
        .find_map(|file| match side {
            SourceSide::Before if file.before.as_ref().is_some_and(|source| source.path == path) => {
                file.before.as_ref()
            }
            SourceSide::After if file.after.as_ref().is_some_and(|source| source.path == path) => file.after.as_ref(),
            _ => None,
        })
        .with_context(|| format!("no captured {side:?} source for `{path}`"))?;
    let text = source
        .content
        .as_deref()
        .with_context(|| format!("captured source `{path}` has no readable content"))?;
    let actual_hash = hex_digest(text.as_bytes());
    if source.content_hash.as_deref() != Some(actual_hash.as_str()) {
        bail!("captured source `{path}` failed its immutable content-hash check");
    }
    let line_count = text.lines().count() as u32;
    if end_line > line_count {
        bail!("source range {start_line}..{end_line} exceeds `{path}`'s {line_count} lines");
    }
    let fragment = if start_line == end_line {
        format!("#L{start_line}")
    } else {
        format!("#L{start_line}-L{end_line}")
    };
    Ok(SourceReference {
        side,
        path: path.to_owned(),
        start_line,
        end_line,
        href: format!(
            "https://github.com/{}/blob/{}/{}{}",
            source.repository,
            source.sha,
            boss_github::trees::encode_tree_path(&source.path),
            fragment
        ),
    })
}

/// Give callers a Files-changed URL when the range still validates against
/// a captured hunk for `current`, otherwise the immutable blob permalink.
/// The reason accompanies a fallback so a guide's navigation diagnostics
/// stay explicit.
pub fn rendered_target_or_pinned_fallback(
    packet: &SourcePacket,
    reference: &SourceReference,
    current: &PinnedComparison,
    reason: impl Into<String>,
) -> RenderedTarget {
    let fallback = |why: String| RenderedTarget::PinnedFallback {
        href: reference.href.clone(),
        reason: format!("{why} ({RENDERED_TARGET_ADAPTER_VERSION})"),
    };
    if validate_pinned_reference(
        packet,
        reference.side,
        &reference.path,
        reference.start_line,
        reference.end_line,
    )
    .is_err()
    {
        return fallback(reason.into());
    }
    if current.head_sha != packet.head_sha || current.base_sha != packet.observed_base_sha {
        return fallback(format!(
            "packet comparison {}/{} is not the live comparison {}/{}",
            packet.observed_base_sha, packet.head_sha, current.base_sha, current.head_sha
        ));
    }
    let Some(file) = source_file_for_reference(packet, reference) else {
        return fallback(format!(
            "no inventory file owns {} {:?}",
            reference.path, reference.side
        ));
    };
    let Some(patch) = file.patch.as_deref() else {
        return fallback(format!(
            "GitHub omitted the API patch for `{}`; no rendered hunk to bind",
            file.path
        ));
    };
    let Some(hunk) = hunk_covering(patch, reference.side, reference.start_line, reference.end_line) else {
        return fallback(format!(
            "range {}..{} on {:?} `{}` is outside every captured diff hunk",
            reference.start_line, reference.end_line, reference.side, file.path
        ));
    };
    RenderedTarget::Validated {
        href: files_changed_href(packet, file, reference),
        adapter_version: RENDERED_TARGET_ADAPTER_VERSION.to_owned(),
        evidence: RenderedTargetEvidence {
            head_sha: packet.head_sha.clone(),
            merge_base_sha: packet.merge_base_sha.clone(),
            file_path: file.path.clone(),
            start_line: reference.start_line,
            end_line: reference.end_line,
            side: reference.side,
            hunk,
        },
    }
}

fn source_file_for_reference<'a>(packet: &'a SourcePacket, reference: &SourceReference) -> Option<&'a SourceFile> {
    packet.files.iter().find(|file| match reference.side {
        SourceSide::Before => file.before.as_ref().is_some_and(|source| source.path == reference.path),
        SourceSide::After => file.after.as_ref().is_some_and(|source| source.path == reference.path),
    })
}

fn files_changed_href(packet: &SourcePacket, file: &SourceFile, reference: &SourceReference) -> String {
    // GitHub keys a rename's Files-changed entry on the current filename;
    // L (old) and R (new) line anchors share that one `#diff-` hash.
    let hash = hex_digest(file.path.as_bytes());
    let fragment = if reference.start_line == reference.end_line {
        match reference.side {
            SourceSide::Before => format!("L{}", reference.start_line),
            SourceSide::After => format!("R{}", reference.start_line),
        }
    } else {
        match reference.side {
            SourceSide::Before => format!("L{}-L{}", reference.start_line, reference.end_line),
            SourceSide::After => format!("R{}-R{}", reference.start_line, reference.end_line),
        }
    };
    format!("{}/files#diff-{hash}{fragment}", packet.canonical_pr_url)
}

struct DiffHunk {
    header: String,
    old_start: u32,
    old_len: u32,
    new_start: u32,
    new_len: u32,
}

fn hunk_covering(patch: &str, side: SourceSide, start_line: u32, end_line: u32) -> Option<String> {
    parse_diff_hunks(patch).into_iter().find_map(|hunk| {
        let (hunk_start, hunk_len) = match side {
            SourceSide::Before => (hunk.old_start, hunk.old_len),
            SourceSide::After => (hunk.new_start, hunk.new_len),
        };
        if hunk_len == 0 {
            return None;
        }
        let hunk_end = hunk_start + hunk_len - 1;
        (start_line >= hunk_start && end_line <= hunk_end).then_some(hunk.header)
    })
}

fn parse_diff_hunks(patch: &str) -> Vec<DiffHunk> {
    patch.lines().filter_map(parse_hunk_header).collect()
}

fn parse_hunk_header(line: &str) -> Option<DiffHunk> {
    let rest = line.strip_prefix("@@ ")?;
    let header_end = rest.find(" @@")?;
    let spec = rest[..header_end].trim();
    let mut parts = spec.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let (old_start, old_len) = parse_hunk_span(old)?;
    let (new_start, new_len) = parse_hunk_span(new)?;
    Some(DiffHunk {
        header: format!("@@ {spec} @@"),
        old_start,
        old_len,
        new_start,
        new_len,
    })
}

fn parse_hunk_span(span: &str) -> Option<(u32, u32)> {
    match span.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => Some((span.parse().ok()?, 1)),
    }
}

/// GitHub-backed reads used by source collection. Production uses
/// [`LiveSourceTransport`]; tests inject a fixture so the composition
/// function can be driven without spawning `gh`.
trait SourceTransport {
    fn fetch_pr_comparison_metadata(
        &self,
        pr_url: &str,
    ) -> impl std::future::Future<Output = Result<boss_github::pr_files::PrComparisonMetadata>> + Send;
    fn fetch_merge_base(
        &self,
        repository: &str,
        base_sha: &str,
        head_sha: &str,
    ) -> impl std::future::Future<Output = Result<String>> + Send;
    fn fetch_complete_pr_file_inventory(
        &self,
        repository: &str,
        number: u64,
        expected_changed_files: u64,
    ) -> impl std::future::Future<Output = Result<Vec<boss_github::pr_files::PrFileInventoryEntry>>> + Send;
    fn fetch_pinned_tree_directory(
        &self,
        owner: &str,
        repo: &str,
        commit_sha: &str,
        directory: &str,
        names: &HashSet<String>,
    ) -> impl std::future::Future<
        Output = std::result::Result<boss_github::trees::PinnedTree, boss_github::trees::TreeApiError>,
    > + Send;
    fn fetch_repo_file_bytes(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        sha: &str,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>>> + Send;
}

struct LiveSourceTransport;

impl SourceTransport for LiveSourceTransport {
    async fn fetch_pr_comparison_metadata(&self, pr_url: &str) -> Result<boss_github::pr_files::PrComparisonMetadata> {
        boss_github::pr_files::fetch_pr_comparison_metadata(pr_url).await
    }

    async fn fetch_merge_base(&self, repository: &str, base_sha: &str, head_sha: &str) -> Result<String> {
        boss_github::compare::fetch_merge_base(repository, base_sha, head_sha).await
    }

    async fn fetch_complete_pr_file_inventory(
        &self,
        repository: &str,
        number: u64,
        expected_changed_files: u64,
    ) -> Result<Vec<boss_github::pr_files::PrFileInventoryEntry>> {
        boss_github::pr_files::fetch_complete_pr_file_inventory(repository, number, expected_changed_files).await
    }

    async fn fetch_pinned_tree_directory(
        &self,
        owner: &str,
        repo: &str,
        commit_sha: &str,
        directory: &str,
        names: &HashSet<String>,
    ) -> std::result::Result<boss_github::trees::PinnedTree, boss_github::trees::TreeApiError> {
        boss_github::trees::fetch_pinned_tree_directory(owner, repo, commit_sha, directory, |candidate| {
            names.contains(candidate)
        })
        .await
    }

    async fn fetch_repo_file_bytes(&self, owner: &str, repo: &str, path: &str, sha: &str) -> Result<Option<Vec<u8>>> {
        boss_github::contents::fetch_repo_file_bytes(owner, repo, path, sha).await
    }
}

/// Collect the complete changed-file inventory and pinned source sides for a
/// PR observation. The caller supplies the base/head seen by its own verified
/// lifecycle probe; a mismatch is an error rather than an opportunity to
/// silently capture a newer or older comparison.
pub async fn collect_pinned_source_packet(
    pr_url: &str,
    observed: &PinnedComparison,
    expected_head_branch: Option<&str>,
) -> Result<SourcePacket> {
    let metadata = boss_github::pr_files::fetch_pr_comparison_metadata(pr_url).await?;
    collect_pinned_source_packet_with_metadata(pr_url, observed, expected_head_branch, metadata, true).await
}

/// Same as [`collect_pinned_source_packet`], but reuses metadata the caller
/// already fetched. `endpoints_are_independently_observed` is true only when
/// `observed` came from a distinct lifecycle probe (the merge poller), not
/// from this same REST resource a moment earlier.
pub async fn collect_pinned_source_packet_with_metadata(
    pr_url: &str,
    observed: &PinnedComparison,
    expected_head_branch: Option<&str>,
    metadata: boss_github::pr_files::PrComparisonMetadata,
    endpoints_are_independently_observed: bool,
) -> Result<SourcePacket> {
    collect_pinned_source_packet_with_transport(
        pr_url,
        observed,
        expected_head_branch,
        metadata,
        endpoints_are_independently_observed,
        &LiveSourceTransport,
    )
    .await
}

async fn collect_pinned_source_packet_with_transport<T: SourceTransport + Sync>(
    pr_url: &str,
    observed: &PinnedComparison,
    expected_head_branch: Option<&str>,
    metadata: boss_github::pr_files::PrComparisonMetadata,
    endpoints_are_independently_observed: bool,
    transport: &T,
) -> Result<SourcePacket> {
    if let Some(expected_head_branch) = expected_head_branch
        && metadata.head_ref_name != expected_head_branch
    {
        bail!(
            "PR head branch changed or belongs to another execution: expected `{expected_head_branch}`, got `{}`",
            metadata.head_ref_name,
        );
    }
    // Independently observed endpoints come from GraphQL `headRefOid` /
    // `baseRefOid` (`gh pr view --json`). A probe-shaped observation is
    // only required to match REST on `head_sha`. REST `base.sha` is the
    // comparison identity stored as `observed_base_sha` (it determined
    // the merge base); the probe's GraphQL oid is stored separately as
    // `probe_base_sha`. REST `base.sha` is re-checked against itself
    // across the two REST reads so a mid-collection REST move still
    // bails.
    if endpoints_are_independently_observed {
        require_stable_head(observed, &metadata)?;
        if observed.base_sha != metadata.base_sha {
            tracing::warn!(
                probe_base_sha = %observed.base_sha,
                rest_base_sha = %metadata.base_sha,
                head_sha = %observed.head_sha,
                "review-guide source capture: GraphQL baseRefOid diverged from REST base.sha; keying the packet on REST identity"
            );
        }
    } else {
        require_stable_endpoints(observed, &metadata)?;
    }
    let merge_base_sha = transport
        .fetch_merge_base(&metadata.base_repository, &metadata.base_sha, &metadata.head_sha)
        .await?;
    let inventory = transport
        .fetch_complete_pr_file_inventory(&metadata.base_repository, metadata.number, metadata.changed_files)
        .await?;
    let latest = transport.fetch_pr_comparison_metadata(pr_url).await?;
    let rest_identity = PinnedComparison {
        base_sha: metadata.base_sha.clone(),
        head_sha: metadata.head_sha.clone(),
    };
    require_stable_endpoints(&rest_identity, &latest)?;
    if endpoints_are_independently_observed {
        require_stable_head(observed, &latest)?;
    }

    let before_paths: HashSet<String> = inventory
        .iter()
        .filter(|file| ChangeKind::from_api(&file.status).has_before())
        .map(|file| file.previous_filename.clone().unwrap_or_else(|| file.filename.clone()))
        .collect();
    let after_paths: HashSet<String> = inventory
        .iter()
        .filter(|file| ChangeKind::from_api(&file.status).has_after())
        .map(|file| file.filename.clone())
        .collect();
    let (before_entries, before_omissions) = fetch_pinned_tree_entries(
        transport,
        &metadata.base_repository,
        &merge_base_sha,
        &before_paths,
        SourceSide::Before,
    )
    .await?;
    let (after_entries, after_omissions) = fetch_pinned_tree_entries(
        transport,
        &metadata.head_repository,
        &metadata.head_sha,
        &after_paths,
        SourceSide::After,
    )
    .await?;
    let before_errors = omission_reasons(before_omissions);
    let after_errors = omission_reasons(after_omissions);

    let mut source_work = Vec::new();
    for file in &inventory {
        let change_kind = ChangeKind::from_api(&file.status);
        if change_kind.has_before() {
            let before_path = file.previous_filename.clone().unwrap_or_else(|| file.filename.clone());
            source_work.push(SourceFetch {
                key: (before_path.clone(), SourceSide::Before),
                repository: metadata.base_repository.clone(),
                sha: merge_base_sha.clone(),
                entry: before_entries.get(&before_path).cloned(),
                tree_error: before_errors.get(&before_path).cloned(),
            });
        }
        if change_kind.has_after() {
            source_work.push(SourceFetch {
                key: (file.filename.clone(), SourceSide::After),
                repository: metadata.head_repository.clone(),
                sha: metadata.head_sha.clone(),
                entry: after_entries.get(&file.filename).cloned(),
                tree_error: after_errors.get(&file.filename).cloned(),
            });
        }
    }

    let mut pending = stream::iter(source_work)
        .map(|work| async move {
            let source = fetch_source(
                transport,
                &work.repository,
                &work.sha,
                &work.key.0,
                work.entry.as_ref(),
                work.tree_error.as_ref(),
            )
            .await;
            (work.key, source)
        })
        .buffer_unordered(8);
    let mut fetched = HashMap::new();
    while let Some((key, source)) = pending.next().await {
        fetched.insert(key, source);
    }

    let mut omissions = Vec::new();
    let mut files = Vec::with_capacity(inventory.len());
    for file in inventory {
        let change_kind = ChangeKind::from_api(&file.status);
        if let ChangeKind::Unknown(status) = &change_kind {
            omissions.push(SourceOmission {
                path: Some(file.filename.clone()),
                side: None,
                reason: format!(
                    "unrecognised GitHub file status `{status}`; fetched both sides without asserting a modification"
                ),
                terminal: true,
            });
        }
        let before_path = file.previous_filename.clone().unwrap_or_else(|| file.filename.clone());
        let before = if change_kind.has_before() {
            fetched.remove(&(before_path, SourceSide::Before))
        } else {
            None
        };
        let after = if change_kind.has_after() {
            fetched.remove(&(file.filename.clone(), SourceSide::After))
        } else {
            None
        };
        if let Some(source) = &before {
            record_omission(&mut omissions, source, SourceSide::Before);
        }
        if let Some(source) = &after {
            record_omission(&mut omissions, source, SourceSide::After);
        }
        if file.patch.is_none() {
            omissions.push(SourceOmission {
                path: Some(file.filename.clone()),
                side: None,
                reason: "GitHub omitted the API patch; pinned source was collected instead".to_owned(),
                terminal: true,
            });
        }
        files.push(SourceFile {
            path: file.filename,
            previous_path: file.previous_filename,
            change_kind,
            additions: file.additions,
            deletions: file.deletions,
            patch: file.patch,
            before,
            after,
        });
    }

    Ok(SourcePacket {
        schema_version: PACKET_SCHEMA_VERSION,
        canonical_pr_url: pr_url.to_owned(),
        pr_number: metadata.number,
        title: metadata.title,
        body: metadata.body,
        base_repository: metadata.base_repository,
        head_repository: metadata.head_repository,
        observed_base_sha: metadata.base_sha.clone(),
        probe_base_sha: endpoints_are_independently_observed.then(|| observed.base_sha.clone()),
        merge_base_sha,
        head_sha: observed.head_sha.clone(),
        files,
        omissions,
    })
}

struct SourceFetch {
    key: (String, SourceSide),
    repository: String,
    sha: String,
    entry: Option<boss_github::trees::PinnedTreeEntry>,
    tree_error: Option<TreeSideError>,
}

#[derive(Clone)]
struct TreeSideError {
    reason: String,
    terminal: bool,
}

/// Reject a collection whose live PR metadata no longer matches the pinned
/// comparison identity. Path/count equality is not enough: a same-count
/// force-push would otherwise mix a new inventory into an old packet.
pub fn require_stable_endpoints(
    observed: &PinnedComparison,
    metadata: &boss_github::pr_files::PrComparisonMetadata,
) -> Result<()> {
    require_matching_endpoints(observed, metadata, true)
}

fn require_stable_head(
    observed: &PinnedComparison,
    metadata: &boss_github::pr_files::PrComparisonMetadata,
) -> Result<()> {
    require_matching_endpoints(observed, metadata, false)
}

fn require_matching_endpoints(
    observed: &PinnedComparison,
    metadata: &boss_github::pr_files::PrComparisonMetadata,
    compare_base: bool,
) -> Result<()> {
    if metadata.head_sha != observed.head_sha || (compare_base && metadata.base_sha != observed.base_sha) {
        if compare_base {
            bail!(
                "PR endpoints changed while collecting sources: observed {}/{} but metadata returned {}/{}",
                observed.base_sha,
                observed.head_sha,
                metadata.base_sha,
                metadata.head_sha,
            );
        }
        bail!(
            "PR head changed while collecting sources: observed {} but metadata returned {}",
            observed.head_sha,
            metadata.head_sha,
        );
    }
    Ok(())
}

fn group_paths(paths: &HashSet<String>) -> HashMap<String, HashSet<String>> {
    let mut grouped: HashMap<String, HashSet<String>> = HashMap::new();
    for path in paths {
        let (directory, name) = split_repo_path(path);
        grouped.entry(directory.to_owned()).or_default().insert(name.to_owned());
    }
    grouped
}

fn join_repo_path(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_owned()
    } else {
        format!("{directory}/{name}")
    }
}

async fn fetch_pinned_tree_entries<T: SourceTransport + Sync>(
    transport: &T,
    repository: &str,
    sha: &str,
    paths: &HashSet<String>,
    side: SourceSide,
) -> Result<(
    HashMap<String, boss_github::trees::PinnedTreeEntry>,
    Vec<SourceOmission>,
)> {
    let (owner, repo) = repository
        .split_once('/')
        .filter(|(owner, repo)| !owner.is_empty() && !repo.is_empty())
        .with_context(|| format!("invalid pinned source repository identity {repository}"))?;
    fetch_directory_entries(repository, sha, paths, side, |directory, names| async move {
        transport
            .fetch_pinned_tree_directory(owner, repo, sha, &directory, &names)
            .await
    })
    .await
}

async fn fetch_directory_entries<F, Fut>(
    repository: &str,
    sha: &str,
    paths: &HashSet<String>,
    side: SourceSide,
    fetch: F,
) -> Result<(
    HashMap<String, boss_github::trees::PinnedTreeEntry>,
    Vec<SourceOmission>,
)>
where
    F: Fn(String, HashSet<String>) -> Fut,
    Fut: std::future::Future<
            Output = std::result::Result<boss_github::trees::PinnedTree, boss_github::trees::TreeApiError>,
        >,
{
    let mut pending = stream::iter(group_paths(paths))
        .map(|(directory, names)| {
            let result = fetch(directory.clone(), names.clone());
            async move { (directory, names, result.await) }
        })
        .buffer_unordered(8);
    let mut entries = HashMap::new();
    let mut omissions = Vec::new();
    while let Some((directory, names, result)) = pending.next().await {
        match result {
            Ok(tree) if !tree.truncated => {
                for mut entry in tree.entries {
                    entry.path = join_repo_path(&directory, &entry.path);
                    entries.insert(entry.path.clone(), entry);
                }
            }
            result => {
                let (cause, terminal) = match result {
                    Ok(_) => ("directory response was truncated".to_owned(), true),
                    Err(error) => (error.to_string(), tree_error_is_terminal(error.kind)),
                };
                for name in names {
                    omissions.push(SourceOmission {
                        path: Some(join_repo_path(&directory, &name)),
                        side: Some(side),
                        reason: format!("could not read pinned tree {repository}@{sha} directory {directory}: {cause}"),
                        terminal,
                    });
                }
            }
        }
    }
    omissions.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((entries, omissions))
}

fn split_repo_path(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

fn omission_reasons(omissions: Vec<SourceOmission>) -> HashMap<String, TreeSideError> {
    omissions
        .into_iter()
        .filter_map(|omission| {
            omission.path.map(|path| {
                (
                    path,
                    TreeSideError {
                        reason: omission.reason,
                        terminal: omission.terminal,
                    },
                )
            })
        })
        .collect()
}

async fn fetch_source<T: SourceTransport + Sync>(
    transport: &T,
    repository: &str,
    sha: &str,
    path: &str,
    entry: Option<&boss_github::trees::PinnedTreeEntry>,
    tree_error: Option<&TreeSideError>,
) -> PinnedSource {
    if let Some(error) = tree_error {
        return PinnedSource::omitted(repository, sha, path, error.reason.clone(), None, error.terminal);
    }
    let Some(entry) = entry else {
        return PinnedSource::omitted(
            repository,
            sha,
            path,
            "path was absent from the pinned Git tree".to_owned(),
            None,
            true,
        );
    };
    if let Some(reason) = pinned_entry_omission(entry) {
        return PinnedSource::omitted(repository, sha, path, reason, Some(entry), true);
    }
    let Some((owner, repo)) = repository.split_once('/') else {
        return PinnedSource::omitted(
            repository,
            sha,
            path,
            "repository is not owner/repo".to_owned(),
            Some(entry),
            true,
        );
    };
    match transport.fetch_repo_file_bytes(owner, repo, path, sha).await {
        Ok(Some(bytes)) => {
            if bytes.len() as u64 > MAX_PINNED_SOURCE_BYTES {
                return PinnedSource::omitted(
                    repository,
                    sha,
                    path,
                    format!("pinned source exceeds the {MAX_PINNED_SOURCE_BYTES}-byte per-file capture budget"),
                    Some(entry),
                    true,
                );
            }
            match String::from_utf8(bytes) {
                Ok(content) => PinnedSource::captured(repository, sha, path, content, entry),
                Err(error) => PinnedSource::omitted_binary(repository, sha, path, error.into_bytes(), entry),
            }
        }
        Ok(None) => PinnedSource::omitted(
            repository,
            sha,
            path,
            "pinned source read failed: Contents returned no bytes after the pinned tree listed a blob".to_owned(),
            Some(entry),
            false,
        ),
        Err(error) => PinnedSource::omitted(
            repository,
            sha,
            path,
            format!("pinned source read failed: {error:#}"),
            Some(entry),
            false,
        ),
    }
}

fn pinned_entry_omission(entry: &boss_github::trees::PinnedTreeEntry) -> Option<String> {
    if entry.object_type != "blob" {
        return Some(format!(
            "pinned tree entry is `{}` rather than a readable blob",
            entry.object_type
        ));
    }
    if entry.mode == "120000" {
        return Some(SYMLINK_SOURCE_OMISSION.to_owned());
    }
    if entry.size.is_some_and(|size| size > MAX_PINNED_SOURCE_BYTES) {
        return Some(format!(
            "pinned source exceeds the {MAX_PINNED_SOURCE_BYTES}-byte per-file capture budget"
        ));
    }
    None
}

fn record_omission(omissions: &mut Vec<SourceOmission>, source: &PinnedSource, side: SourceSide) {
    if let Some(reason) = &source.omission {
        omissions.push(SourceOmission {
            path: Some(source.path.clone()),
            side: Some(side),
            reason: reason.clone(),
            terminal: source.terminal.unwrap_or(false),
        });
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_grouping_and_rejoining_preserve_repository_paths() {
        let paths: HashSet<String> = ["lib.rs", "src/lib.rs", "src/main.rs", "other/lib.rs"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let grouped = group_paths(&paths);
        assert_eq!(grouped.len(), 3);
        assert_eq!(grouped["src"].len(), 2);
        assert!(grouped[""].contains("lib.rs"));
        let reconstructed: HashSet<String> = grouped
            .into_iter()
            .flat_map(|(directory, names)| names.into_iter().map(move |name| join_repo_path(&directory, &name)))
            .collect();
        assert_eq!(reconstructed, paths);
    }

    #[test]
    fn one_directory_failure_preserves_other_sources_and_precise_omissions() {
        use futures_util::FutureExt;
        let paths = ["lib.rs", "broken/a.rs", "broken/b.rs"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let result = fetch_directory_entries(
            "acme/widget",
            "pinned",
            &paths,
            SourceSide::Before,
            |directory, names| {
                let result = if directory == "broken" {
                    Err(boss_github::trees::TreeApiError {
                        kind: boss_github::trees::TreeApiErrorKind::Unreachable,
                        message: "rate limited".to_owned(),
                    })
                } else {
                    let mut entry = blob_entry();
                    entry.path = "lib.rs".to_owned();
                    assert!(names.contains(&entry.path));
                    Ok(boss_github::trees::PinnedTree {
                        sha: "pinned".to_owned(),
                        truncated: false,
                        entries: vec![entry],
                    })
                };
                std::future::ready(result)
            },
        )
        .now_or_never()
        .expect("fixture reads are ready")
        .unwrap();
        assert!(result.0.contains_key("lib.rs"));
        assert_eq!(result.1.len(), 2);
        let source = fetch_source(
            &LiveSourceTransport,
            "acme/widget",
            "pinned",
            "broken/a.rs",
            None,
            Some(&TreeSideError {
                reason: result.1[0].reason.clone(),
                terminal: result.1[0].terminal,
            }),
        )
        .now_or_never()
        .expect("tree failure needs no Contents request");
        assert_eq!(source.omission.as_deref(), Some(result.1[0].reason.as_str()));
        assert_eq!(source.terminal, Some(false));
        assert_eq!(result.1[0].path.as_deref(), Some("broken/a.rs"));
        assert_eq!(result.1[1].path.as_deref(), Some("broken/b.rs"));
        assert!(
            result
                .1
                .iter()
                .all(|omission| omission.side == Some(SourceSide::Before) && omission.reason.contains("rate limited"))
        );
    }

    fn blob_entry() -> boss_github::trees::PinnedTreeEntry {
        boss_github::trees::PinnedTreeEntry {
            path: "src/with space.rs".to_owned(),
            object_sha: "d".repeat(40),
            mode: "100644".to_owned(),
            object_type: "blob".to_owned(),
            size: None,
        }
    }

    fn packet() -> SourcePacket {
        SourcePacket {
            schema_version: PACKET_SCHEMA_VERSION,
            canonical_pr_url: "https://github.com/acme/widget/pull/4".to_owned(),
            pr_number: 4,
            title: "Capture source".to_owned(),
            body: None,
            base_repository: "acme/widget".to_owned(),
            head_repository: "acme/widget".to_owned(),
            observed_base_sha: "a".repeat(40),
            probe_base_sha: None,
            merge_base_sha: "b".repeat(40),
            head_sha: "c".repeat(40),
            files: vec![SourceFile {
                path: "src/with space.rs".to_owned(),
                previous_path: None,
                change_kind: ChangeKind::Modified,
                additions: 1,
                deletions: 1,
                patch: Some("@@ -1 +1 @@".to_owned()),
                before: Some(PinnedSource::captured(
                    "acme/widget",
                    &"b".repeat(40),
                    "src/with space.rs",
                    "old\n".to_owned(),
                    &blob_entry(),
                )),
                after: Some(PinnedSource::captured(
                    "acme/widget",
                    &"c".repeat(40),
                    "src/with space.rs",
                    "first\nsecond\n".to_owned(),
                    &blob_entry(),
                )),
            }],
            omissions: Vec::new(),
        }
    }

    #[test]
    fn packet_hash_is_stable_and_changes_with_content() {
        let packet = packet();
        assert_eq!(packet.content_hash().unwrap(), packet.content_hash().unwrap());
        let mut changed = packet.clone();
        changed.title = "different".to_owned();
        assert_ne!(packet.content_hash().unwrap(), changed.content_hash().unwrap());
    }

    #[test]
    fn validates_ranges_against_pinned_content_and_encodes_path() {
        let reference = validate_pinned_reference(&packet(), SourceSide::After, "src/with space.rs", 1, 2).unwrap();
        assert_eq!(
            reference.href,
            format!(
                "https://github.com/acme/widget/blob/{}/src/with%20space.rs#L1-L2",
                "c".repeat(40)
            )
        );
        assert!(validate_pinned_reference(&packet(), SourceSide::After, "src/with space.rs", 3, 3).is_err());
    }

    #[test]
    fn reference_validation_rejects_a_tampered_source_hash() {
        let mut packet = packet();
        packet.files[0].after.as_mut().unwrap().content_hash = Some("wrong".to_owned());
        assert!(validate_pinned_reference(&packet, SourceSide::After, "src/with space.rs", 1, 1).is_err());
    }

    fn live_comparison(packet: &SourcePacket) -> PinnedComparison {
        PinnedComparison {
            base_sha: packet.observed_base_sha.clone(),
            head_sha: packet.head_sha.clone(),
        }
    }

    #[test]
    fn rendered_navigation_constructs_a_files_changed_target() {
        let packet = packet();
        let reference = validate_pinned_reference(&packet, SourceSide::After, "src/with space.rs", 1, 1).unwrap();
        let hash = hex_digest(b"src/with space.rs");
        match rendered_target_or_pinned_fallback(&packet, &reference, &live_comparison(&packet), "unused") {
            RenderedTarget::Validated {
                href,
                adapter_version,
                evidence,
            } => {
                assert_eq!(adapter_version, RENDERED_TARGET_ADAPTER_VERSION);
                assert_eq!(
                    href,
                    format!("https://github.com/acme/widget/pull/4/files#diff-{hash}R1")
                );
                assert_eq!(evidence.file_path, "src/with space.rs");
                assert_eq!(evidence.hunk, "@@ -1 +1 @@");
                assert_eq!(evidence.head_sha, packet.head_sha);
            }
            other => panic!("expected Validated, got {other:?}"),
        }
    }

    #[test]
    fn rendered_navigation_falls_back_when_the_range_no_longer_validates() {
        let packet = packet();
        let mut reference = validate_pinned_reference(&packet, SourceSide::After, "src/with space.rs", 1, 1).unwrap();
        reference.end_line = 99;
        assert!(matches!(
            rendered_target_or_pinned_fallback(
                &packet,
                &reference,
                &live_comparison(&packet),
                "range exceeds captured source"
            ),
            RenderedTarget::PinnedFallback { .. }
        ));
    }

    #[test]
    fn rendered_navigation_falls_back_for_an_out_of_hunk_line() {
        let packet = packet();
        let reference = validate_pinned_reference(&packet, SourceSide::After, "src/with space.rs", 2, 2).unwrap();
        match rendered_target_or_pinned_fallback(&packet, &reference, &live_comparison(&packet), "unused") {
            RenderedTarget::PinnedFallback { reason, .. } => {
                assert!(reason.contains("outside every captured diff hunk"), "{reason}");
            }
            other => panic!("expected PinnedFallback, got {other:?}"),
        }
    }

    #[test]
    fn rendered_navigation_falls_back_when_the_file_has_no_patch() {
        let mut packet = packet();
        packet.files[0].patch = None;
        let reference = validate_pinned_reference(&packet, SourceSide::After, "src/with space.rs", 1, 1).unwrap();
        match rendered_target_or_pinned_fallback(&packet, &reference, &live_comparison(&packet), "unused") {
            RenderedTarget::PinnedFallback { reason, .. } => {
                assert!(reason.contains("omitted the API patch"), "{reason}");
            }
            other => panic!("expected PinnedFallback, got {other:?}"),
        }
    }

    #[test]
    fn rendered_navigation_falls_back_when_the_head_moved() {
        let packet = packet();
        let reference = validate_pinned_reference(&packet, SourceSide::After, "src/with space.rs", 1, 1).unwrap();
        let mut current = live_comparison(&packet);
        current.head_sha = "moved-head".to_owned();
        match rendered_target_or_pinned_fallback(&packet, &reference, &current, "unused") {
            RenderedTarget::PinnedFallback { reason, .. } => {
                assert!(reason.contains("is not the live comparison"), "{reason}");
            }
            other => panic!("expected PinnedFallback, got {other:?}"),
        }
    }

    #[test]
    fn rendered_navigation_hashes_the_current_filename_on_both_rename_sides() {
        let mut packet = packet();
        packet.files[0].path = "new.rs".to_owned();
        packet.files[0].previous_path = Some("old.rs".to_owned());
        packet.files[0].change_kind = ChangeKind::Renamed;
        packet.files[0].patch = Some("@@ -1 +1 @@".to_owned());
        packet.files[0].before.as_mut().unwrap().path = "old.rs".to_owned();
        packet.files[0].after.as_mut().unwrap().path = "new.rs".to_owned();
        packet.files[0].before.as_mut().unwrap().content = Some("was\n".to_owned());
        packet.files[0].before.as_mut().unwrap().content_hash = Some(hex_digest(b"was\n"));
        packet.files[0].after.as_mut().unwrap().content = Some("now\n".to_owned());
        packet.files[0].after.as_mut().unwrap().content_hash = Some(hex_digest(b"now\n"));
        let before = validate_pinned_reference(&packet, SourceSide::Before, "old.rs", 1, 1).unwrap();
        let after = validate_pinned_reference(&packet, SourceSide::After, "new.rs", 1, 1).unwrap();
        let hash = hex_digest(b"new.rs");
        let current = live_comparison(&packet);
        match rendered_target_or_pinned_fallback(&packet, &before, &current, "unused") {
            RenderedTarget::Validated { href, evidence, .. } => {
                assert_eq!(
                    href,
                    format!("https://github.com/acme/widget/pull/4/files#diff-{hash}L1")
                );
                assert_eq!(evidence.file_path, "new.rs");
            }
            other => panic!("expected Validated before-side, got {other:?}"),
        }
        match rendered_target_or_pinned_fallback(&packet, &after, &current, "unused") {
            RenderedTarget::Validated { href, evidence, .. } => {
                assert_eq!(
                    href,
                    format!("https://github.com/acme/widget/pull/4/files#diff-{hash}R1")
                );
                assert_eq!(evidence.file_path, "new.rs");
            }
            other => panic!("expected Validated after-side, got {other:?}"),
        }
    }

    #[test]
    fn omitted_source_makes_the_packet_incomplete() {
        let mut packet = packet();
        packet.files[0].after = Some(PinnedSource::omitted(
            "acme/widget",
            &"c".repeat(40),
            "src/with space.rs",
            "pinned source read failed: timeout".to_owned(),
            None,
            false,
        ));
        assert!(!packet.is_complete());
    }

    #[test]
    fn terminal_symlink_omission_settles_the_packet() {
        let mut packet = packet();
        let link = boss_github::trees::PinnedTreeEntry {
            path: "link".to_owned(),
            object_sha: "e".repeat(40),
            mode: "120000".to_owned(),
            object_type: "blob".to_owned(),
            size: Some(11),
        };
        let source = PinnedSource::omitted(
            "acme/widget",
            &"c".repeat(40),
            "link",
            SYMLINK_SOURCE_OMISSION.to_owned(),
            Some(&link),
            true,
        );
        assert!(!source.is_captured());
        assert!(source.is_settled());
        packet.files[0].after = Some(source);
        assert!(packet.is_complete());
    }

    #[test]
    fn omitted_api_patch_does_not_hide_available_pinned_sources() {
        let mut packet = packet();
        packet.files[0].patch = None;
        packet.omissions.push(SourceOmission {
            path: Some("src/with space.rs".to_owned()),
            side: None,
            reason: "GitHub omitted the API patch; pinned source was collected instead".to_owned(),
            terminal: true,
        });
        assert!(packet.is_complete());
    }

    #[test]
    fn binary_source_is_an_explicit_immutable_omission_without_lossy_text() {
        let source = PinnedSource::omitted_binary(
            "acme/widget",
            &"c".repeat(40),
            "image.bin",
            vec![0, 0xff, 1],
            &boss_github::trees::PinnedTreeEntry {
                path: "image.bin".to_owned(),
                object_sha: "d".repeat(40),
                mode: "100644".to_owned(),
                object_type: "blob".to_owned(),
                size: Some(3),
            },
        );
        assert!(source.content.is_none());
        assert_eq!(source.byte_count, Some(3));
        assert!(source.content_hash.is_some());
        assert!(source.omission.as_deref().unwrap().contains("non-UTF-8"));
        assert!(source.is_captured());
        let mut packet = packet();
        packet.files[0].after = Some(source);
        packet.omissions.push(SourceOmission {
            path: Some("image.bin".to_owned()),
            side: Some(SourceSide::After),
            reason: BINARY_SOURCE_OMISSION.to_owned(),
            terminal: true,
        });
        assert!(
            packet.is_complete(),
            "a successfully hashed binary side must not make the packet incomplete"
        );
    }

    #[test]
    fn symlink_tree_entries_are_omitted_rather_than_followed() {
        let link = boss_github::trees::PinnedTreeEntry {
            path: "link".to_owned(),
            object_sha: "e".repeat(40),
            mode: "120000".to_owned(),
            object_type: "blob".to_owned(),
            size: Some(11),
        };
        let reason = pinned_entry_omission(&link).expect("symlink must be omitted");
        assert!(reason.contains("symlink"));
        let source = PinnedSource::omitted("acme/widget", &"c".repeat(40), "link", reason, Some(&link), true);
        assert!(!source.is_captured());
        assert!(source.is_settled());
        assert_eq!(source.object_sha.as_deref(), Some(link.object_sha.as_str()));
        assert_eq!(source.mode.as_deref(), Some("120000"));
    }

    #[test]
    fn unrecognised_file_status_is_not_flattened_into_modified() {
        assert_eq!(ChangeKind::from_api("renamed"), ChangeKind::Renamed);
        assert_eq!(
            ChangeKind::from_api("unchanged"),
            ChangeKind::Unknown("unchanged".to_owned())
        );
        assert!(ChangeKind::from_api("unchanged").has_before());
        assert!(ChangeKind::from_api("unchanged").has_after());
    }

    #[test]
    fn same_count_force_push_is_rejected_by_endpoint_revalidation() {
        let observed = PinnedComparison {
            base_sha: "base".to_owned(),
            head_sha: "head-one".to_owned(),
        };
        let moved = boss_github::pr_files::PrComparisonMetadata {
            number: 4,
            title: "Capture source".to_owned(),
            body: None,
            base_repository: "acme/widget".to_owned(),
            head_repository: "acme/widget".to_owned(),
            head_ref_name: "feature".to_owned(),
            base_sha: "base".to_owned(),
            head_sha: "head-two".to_owned(),
            changed_files: 1,
        };
        let err = require_stable_endpoints(&observed, &moved).unwrap_err().to_string();
        assert!(err.contains("head-one"));
        assert!(err.contains("head-two"));
    }

    #[test]
    fn oversized_tree_entry_is_omitted_before_contents_read() {
        let entry = boss_github::trees::PinnedTreeEntry {
            path: "huge.bin".to_owned(),
            object_sha: "f".repeat(40),
            mode: "100644".to_owned(),
            object_type: "blob".to_owned(),
            size: Some(MAX_PINNED_SOURCE_BYTES + 1),
        };
        assert!(
            pinned_entry_omission(&entry)
                .unwrap()
                .contains("per-file capture budget")
        );
    }

    #[test]
    fn permalink_leaves_tilde_unescaped_like_tree_paths() {
        let mut packet = packet();
        packet.files[0].path = "a~b/c d".to_owned();
        packet.files[0].after.as_mut().unwrap().path = "a~b/c d".to_owned();
        packet.files[0].after.as_mut().unwrap().content = Some("first\n".to_owned());
        packet.files[0].after.as_mut().unwrap().content_hash = Some(hex_digest(b"first\n"));
        let reference = validate_pinned_reference(&packet, SourceSide::After, "a~b/c d", 1, 1).unwrap();
        assert_eq!(
            reference.href,
            format!("https://github.com/acme/widget/blob/{}/a~b/c%20d#L1", "c".repeat(40))
        );
    }

    fn rest_metadata(base: &str, head: &str, changed_files: u64) -> boss_github::pr_files::PrComparisonMetadata {
        boss_github::pr_files::PrComparisonMetadata {
            number: 4,
            title: "Capture source".to_owned(),
            body: None,
            base_repository: "acme/widget".to_owned(),
            head_repository: "acme/widget".to_owned(),
            head_ref_name: "feature".to_owned(),
            base_sha: base.to_owned(),
            head_sha: head.to_owned(),
            changed_files,
        }
    }

    fn tree_blob(path: &str, size: Option<u64>) -> boss_github::trees::PinnedTreeEntry {
        boss_github::trees::PinnedTreeEntry {
            path: path.to_owned(),
            object_sha: format!("obj-{path}"),
            mode: "100644".to_owned(),
            object_type: "blob".to_owned(),
            size,
        }
    }

    fn inventory_entry(
        filename: &str,
        previous: Option<&str>,
        status: &str,
        patch: Option<&str>,
    ) -> boss_github::pr_files::PrFileInventoryEntry {
        boss_github::pr_files::PrFileInventoryEntry {
            filename: filename.to_owned(),
            previous_filename: previous.map(str::to_owned),
            status: status.to_owned(),
            additions: 1,
            deletions: 1,
            patch: patch.map(str::to_owned),
        }
    }

    #[derive(Clone)]
    struct FixtureTransport {
        latest: boss_github::pr_files::PrComparisonMetadata,
        merge_base: String,
        inventory: Vec<boss_github::pr_files::PrFileInventoryEntry>,
        trees: HashMap<
            (String, String),
            std::result::Result<boss_github::trees::PinnedTree, boss_github::trees::TreeApiError>,
        >,
        blobs: HashMap<(String, String), Vec<u8>>,
    }

    #[test]
    fn truncated_immutable_directory_settles_the_packet() {
        use futures_util::FutureExt;
        let metadata = rest_metadata("base", "head", 1);
        let transport = FixtureTransport {
            latest: metadata.clone(),
            merge_base: "merge".to_owned(),
            inventory: vec![inventory_entry("a.rs", None, "added", Some("@@"))],
            trees: HashMap::from([(
                ("head".to_owned(), String::new()),
                Ok(boss_github::trees::PinnedTree {
                    sha: "head".to_owned(),
                    entries: Vec::new(),
                    truncated: true,
                }),
            )]),
            blobs: HashMap::new(),
        };
        let packet = collect_pinned_source_packet_with_transport(
            "https://github.com/acme/widget/pull/4",
            &PinnedComparison {
                base_sha: "base".to_owned(),
                head_sha: "head".to_owned(),
            },
            None,
            metadata,
            false,
            &transport,
        )
        .now_or_never()
        .unwrap()
        .unwrap();
        assert!(packet.is_complete());
        assert_eq!(packet.files[0].after.as_ref().unwrap().terminal, Some(true));
        assert!(
            packet
                .omissions
                .iter()
                .any(|o| o.reason.contains("directory response was truncated"))
        );
    }

    impl SourceTransport for FixtureTransport {
        async fn fetch_pr_comparison_metadata(
            &self,
            _pr_url: &str,
        ) -> Result<boss_github::pr_files::PrComparisonMetadata> {
            Ok(self.latest.clone())
        }

        async fn fetch_merge_base(&self, _repository: &str, _base_sha: &str, _head_sha: &str) -> Result<String> {
            Ok(self.merge_base.clone())
        }

        async fn fetch_complete_pr_file_inventory(
            &self,
            _repository: &str,
            _number: u64,
            expected_changed_files: u64,
        ) -> Result<Vec<boss_github::pr_files::PrFileInventoryEntry>> {
            anyhow::ensure!(
                self.inventory.len() as u64 == expected_changed_files,
                "fixture inventory count mismatch"
            );
            Ok(self.inventory.clone())
        }

        async fn fetch_pinned_tree_directory(
            &self,
            _owner: &str,
            _repo: &str,
            commit_sha: &str,
            directory: &str,
            names: &HashSet<String>,
        ) -> std::result::Result<boss_github::trees::PinnedTree, boss_github::trees::TreeApiError> {
            match self.trees.get(&(commit_sha.to_owned(), directory.to_owned())) {
                Some(Ok(tree)) => {
                    let mut tree = tree.clone();
                    tree.entries.retain(|entry| names.contains(&entry.path));
                    Ok(tree)
                }
                Some(Err(error)) => Err(error.clone()),
                None => Ok(boss_github::trees::PinnedTree {
                    sha: commit_sha.to_owned(),
                    entries: Vec::new(),
                    truncated: false,
                }),
            }
        }

        async fn fetch_repo_file_bytes(
            &self,
            _owner: &str,
            _repo: &str,
            path: &str,
            sha: &str,
        ) -> Result<Option<Vec<u8>>> {
            Ok(self.blobs.get(&(sha.to_owned(), path.to_owned())).cloned())
        }
    }

    fn tree(
        sha: &str,
        entries: Vec<boss_github::trees::PinnedTreeEntry>,
    ) -> std::result::Result<boss_github::trees::PinnedTree, boss_github::trees::TreeApiError> {
        Ok(boss_github::trees::PinnedTree {
            sha: sha.to_owned(),
            entries,
            truncated: false,
        })
    }

    #[test]
    fn collector_accepts_a_probe_shaped_base_that_differs_from_rest() {
        use futures_util::FutureExt;
        let rest_base = "rest-base";
        let graphql_base = "graphql-live-tip";
        let head = "head";
        let merge = "merge-base";
        let metadata = rest_metadata(rest_base, head, 1);
        let mut trees = HashMap::new();
        trees.insert(
            (merge.to_owned(), String::new()),
            tree(merge, vec![tree_blob("lib.rs", Some(4))]),
        );
        trees.insert(
            (head.to_owned(), String::new()),
            tree(head, vec![tree_blob("lib.rs", Some(6))]),
        );
        let mut blobs = HashMap::new();
        blobs.insert((merge.to_owned(), "lib.rs".to_owned()), b"old\n".to_vec());
        blobs.insert((head.to_owned(), "lib.rs".to_owned()), b"after\n".to_vec());
        let transport = FixtureTransport {
            latest: metadata.clone(),
            merge_base: merge.to_owned(),
            inventory: vec![inventory_entry("lib.rs", None, "modified", Some("@@"))],
            trees,
            blobs,
        };
        let observed = PinnedComparison {
            base_sha: graphql_base.to_owned(),
            head_sha: head.to_owned(),
        };
        let packet = collect_pinned_source_packet_with_transport(
            "https://github.com/acme/widget/pull/4",
            &observed,
            Some("feature"),
            metadata,
            true,
            &transport,
        )
        .now_or_never()
        .expect("fixture collector is ready")
        .unwrap();
        assert_eq!(packet.observed_base_sha, rest_base);
        assert_eq!(packet.probe_base_sha.as_deref(), Some(graphql_base));
        assert_eq!(packet.merge_base_sha, merge);
        assert_eq!(packet.head_sha, head);
        assert!(packet.is_complete());
        assert_eq!(
            packet.files[0].before.as_ref().unwrap().content.as_deref(),
            Some("old\n")
        );
        assert_eq!(
            packet.files[0].after.as_ref().unwrap().content.as_deref(),
            Some("after\n")
        );
    }

    #[test]
    fn collector_covers_add_delete_rename_tree_failure_and_oversize() {
        use futures_util::FutureExt;
        let rest_base = "rest-base";
        let head = "head";
        let merge = "merge-base";
        let metadata = rest_metadata(rest_base, head, 5);
        let mut trees = HashMap::new();
        trees.insert(
            (merge.to_owned(), String::new()),
            tree(merge, vec![tree_blob("gone.rs", Some(3)), tree_blob("old.rs", Some(3))]),
        );
        trees.insert(
            (head.to_owned(), String::new()),
            tree(
                head,
                vec![
                    tree_blob("added.rs", Some(4)),
                    tree_blob("new.rs", Some(3)),
                    boss_github::trees::PinnedTreeEntry {
                        path: "huge.bin".to_owned(),
                        object_sha: "huge".to_owned(),
                        mode: "100644".to_owned(),
                        object_type: "blob".to_owned(),
                        size: Some(MAX_PINNED_SOURCE_BYTES + 1),
                    },
                ],
            ),
        );
        trees.insert(
            (head.to_owned(), "broken".to_owned()),
            Err(boss_github::trees::TreeApiError {
                kind: boss_github::trees::TreeApiErrorKind::Unreachable,
                message: "rate limited".to_owned(),
            }),
        );
        let mut blobs = HashMap::new();
        blobs.insert((merge.to_owned(), "gone.rs".to_owned()), b"old\n".to_vec());
        blobs.insert((merge.to_owned(), "old.rs".to_owned()), b"was\n".to_vec());
        blobs.insert((head.to_owned(), "added.rs".to_owned()), b"new\n".to_vec());
        blobs.insert((head.to_owned(), "new.rs".to_owned()), b"now\n".to_vec());
        let transport = FixtureTransport {
            latest: metadata.clone(),
            merge_base: merge.to_owned(),
            inventory: vec![
                inventory_entry("added.rs", None, "added", Some("@@")),
                inventory_entry("gone.rs", None, "deleted", Some("@@")),
                inventory_entry("new.rs", Some("old.rs"), "renamed", None),
                inventory_entry("broken/a.rs", None, "modified", Some("@@")),
                inventory_entry("huge.bin", None, "added", None),
            ],
            trees,
            blobs,
        };
        let observed = PinnedComparison {
            base_sha: rest_base.to_owned(),
            head_sha: head.to_owned(),
        };
        let packet = collect_pinned_source_packet_with_transport(
            "https://github.com/acme/widget/pull/4",
            &observed,
            None,
            metadata,
            false,
            &transport,
        )
        .now_or_never()
        .expect("fixture collector is ready")
        .unwrap();
        assert_eq!(packet.files.len(), 5);
        assert!(packet.files[0].before.is_none());
        assert_eq!(
            packet.files[0].after.as_ref().unwrap().content.as_deref(),
            Some("new\n")
        );
        assert_eq!(
            packet.files[1].before.as_ref().unwrap().content.as_deref(),
            Some("old\n")
        );
        assert!(packet.files[1].after.is_none());
        assert_eq!(packet.files[2].previous_path.as_deref(), Some("old.rs"));
        assert_eq!(packet.files[2].before.as_ref().unwrap().path, "old.rs");
        assert_eq!(packet.files[2].after.as_ref().unwrap().path, "new.rs");
        assert!(
            packet.files[3]
                .after
                .as_ref()
                .unwrap()
                .omission
                .as_deref()
                .unwrap()
                .contains("rate limited")
        );
        assert_eq!(packet.files[3].after.as_ref().unwrap().terminal, Some(false));
        assert_eq!(packet.files[4].after.as_ref().unwrap().terminal, Some(true));
        assert!(
            packet.files[4]
                .after
                .as_ref()
                .unwrap()
                .omission
                .as_deref()
                .unwrap()
                .contains("per-file capture budget")
        );
        assert!(
            !packet.is_complete(),
            "a retryable tree failure must keep the packet incomplete"
        );
        let reasons: Vec<_> = packet
            .omissions
            .iter()
            .map(|o| (o.path.clone(), o.side, o.reason.clone()))
            .collect();
        assert!(
            reasons.iter().any(|(path, side, reason)| {
                path.as_deref() == Some("broken/a.rs")
                    && *side == Some(SourceSide::After)
                    && reason.contains("rate limited")
            }),
            "omission order must record the tree failure during assembly: {reasons:?}"
        );
    }

    #[test]
    fn collector_bails_when_rest_head_moves_mid_collection() {
        use futures_util::FutureExt;
        let metadata = rest_metadata("base", "head-one", 1);
        let mut latest = metadata.clone();
        latest.head_sha = "head-two".to_owned();
        let transport = FixtureTransport {
            latest,
            merge_base: "merge-base".to_owned(),
            inventory: vec![inventory_entry("lib.rs", None, "modified", Some("@@"))],
            trees: HashMap::new(),
            blobs: HashMap::new(),
        };
        let observed = PinnedComparison {
            base_sha: "base".to_owned(),
            head_sha: "head-one".to_owned(),
        };
        let err = collect_pinned_source_packet_with_transport(
            "https://github.com/acme/widget/pull/4",
            &observed,
            None,
            metadata,
            true,
            &transport,
        )
        .now_or_never()
        .expect("fixture collector is ready")
        .unwrap_err()
        .to_string();
        assert!(err.contains("head-one"));
        assert!(err.contains("head-two"));
    }

    #[test]
    fn poller_seam_head_mismatch_does_not_print_base_shas() {
        let observed = PinnedComparison {
            base_sha: "graphql-base".to_owned(),
            head_sha: "head-one".to_owned(),
        };
        let moved = rest_metadata("rest-base", "head-two", 1);
        let err = require_stable_head(&observed, &moved).unwrap_err().to_string();
        assert!(err.contains("PR head changed while collecting sources"));
        assert!(err.contains("head-one"));
        assert!(err.contains("head-two"));
        assert!(
            !err.contains("graphql-base") && !err.contains("rest-base"),
            "poller-seam head mismatch must not mention REST/GraphQL base SHAs: {err}"
        );
    }

    #[test]
    fn collector_settles_a_not_found_directory_read() {
        use futures_util::FutureExt;
        let rest_base = "rest-base";
        let head = "head";
        let merge = "merge-base";
        let metadata = rest_metadata(rest_base, head, 1);
        let mut trees = HashMap::new();
        trees.insert(
            (merge.to_owned(), String::new()),
            tree(merge, vec![tree_blob("lib.rs", Some(4))]),
        );
        trees.insert(
            (head.to_owned(), String::new()),
            Err(boss_github::trees::TreeApiError {
                kind: boss_github::trees::TreeApiErrorKind::NotFound,
                message: "Not Found".to_owned(),
            }),
        );
        let mut blobs = HashMap::new();
        blobs.insert((merge.to_owned(), "lib.rs".to_owned()), b"old\n".to_vec());
        let transport = FixtureTransport {
            latest: metadata.clone(),
            merge_base: merge.to_owned(),
            inventory: vec![inventory_entry("lib.rs", None, "modified", Some("@@"))],
            trees,
            blobs,
        };
        let packet = collect_pinned_source_packet_with_transport(
            "https://github.com/acme/widget/pull/4",
            &PinnedComparison {
                base_sha: rest_base.to_owned(),
                head_sha: head.to_owned(),
            },
            None,
            metadata,
            false,
            &transport,
        )
        .now_or_never()
        .expect("fixture collector is ready")
        .unwrap();
        assert!(
            packet.is_complete(),
            "a NotFound tree read must settle rather than retry forever"
        );
        assert_eq!(packet.files[0].after.as_ref().unwrap().terminal, Some(true));
    }

    #[test]
    fn contents_none_after_a_tree_blob_leaves_the_packet_incomplete() {
        use futures_util::FutureExt;
        let rest_base = "rest-base";
        let head = "head";
        let merge = "merge-base";
        let metadata = rest_metadata(rest_base, head, 1);
        let mut trees = HashMap::new();
        trees.insert(
            (merge.to_owned(), String::new()),
            tree(merge, vec![tree_blob("lib.rs", Some(4))]),
        );
        trees.insert(
            (head.to_owned(), String::new()),
            tree(head, vec![tree_blob("lib.rs", Some(6))]),
        );
        let mut blobs = HashMap::new();
        blobs.insert((merge.to_owned(), "lib.rs".to_owned()), b"old\n".to_vec());
        let missing = FixtureTransport {
            latest: metadata.clone(),
            merge_base: merge.to_owned(),
            inventory: vec![inventory_entry("lib.rs", None, "modified", Some("@@"))],
            trees: trees.clone(),
            blobs: blobs.clone(),
        };
        let first = collect_pinned_source_packet_with_transport(
            "https://github.com/acme/widget/pull/4",
            &PinnedComparison {
                base_sha: rest_base.to_owned(),
                head_sha: head.to_owned(),
            },
            None,
            metadata.clone(),
            false,
            &missing,
        )
        .now_or_never()
        .expect("fixture collector is ready")
        .unwrap();
        assert!(
            !first.is_complete(),
            "Contents Ok(None) after a tree blob must stay retryable"
        );
        assert_eq!(first.files[0].after.as_ref().unwrap().terminal, Some(false));
        blobs.insert((head.to_owned(), "lib.rs".to_owned()), b"after\n".to_vec());
        let recovered = FixtureTransport {
            latest: metadata.clone(),
            merge_base: merge.to_owned(),
            inventory: vec![inventory_entry("lib.rs", None, "modified", Some("@@"))],
            trees,
            blobs,
        };
        let second = collect_pinned_source_packet_with_transport(
            "https://github.com/acme/widget/pull/4",
            &PinnedComparison {
                base_sha: rest_base.to_owned(),
                head_sha: head.to_owned(),
            },
            None,
            metadata,
            false,
            &recovered,
        )
        .now_or_never()
        .expect("fixture collector is ready")
        .unwrap();
        assert!(second.is_complete());
        assert_eq!(
            second.files[0].after.as_ref().unwrap().content.as_deref(),
            Some("after\n")
        );
    }
}
