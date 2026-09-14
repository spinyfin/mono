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

/// Version 2 adds the immutable Git-tree manifest (object SHA, mode, and
/// type) to every captured source side. Consumers can distinguish legacy
/// packets from the stronger object-verified contract without guessing from
/// optional fields.
const PACKET_SCHEMA_VERSION: u32 = 2;

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
    pub observed_base_sha: String,
    pub merge_base_sha: String,
    pub head_sha: String,
    pub files: Vec<SourceFile>,
    pub omissions: Vec<SourceOmission>,
}

impl SourcePacket {
    /// The packet is complete only when every requested source side was read
    /// from an immutable revision. A missing API patch remains an omission in
    /// the manifest but does not make the packet incomplete when its pinned
    /// before/after sources were successfully captured.
    pub fn is_complete(&self) -> bool {
        self.files.iter().all(SourceFile::is_complete)
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
        self.before.as_ref().is_none_or(PinnedSource::is_captured)
            && self.after.as_ref().is_none_or(PinnedSource::is_captured)
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
        }
    }

    fn omitted(
        repository: &str,
        sha: &str,
        path: &str,
        omission: String,
        entry: Option<&boss_github::trees::PinnedTreeEntry>,
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
        }
    }

    fn is_captured(&self) -> bool {
        self.object_sha.is_some()
            && self.content_hash.is_some()
            && self.byte_count.is_some()
            && (self.content.is_some() || self.omission.as_deref() == Some(BINARY_SOURCE_OMISSION))
    }
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
/// validating its file anchor/side/line against GitHub's rendered page. Until
/// that adapter exists, this explicit outcome produces a pinned fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RenderedTarget {
    Validated { href: String, adapter_version: String },
    PinnedFallback { href: String, reason: String },
}

/// The source/reference contract's current adapter version. It intentionally
/// has no PR-diff renderer implementation yet, so callers get a truthfully
/// labeled immutable source link rather than an invented `#diff-*` fragment.
pub const RENDERED_TARGET_ADAPTER_VERSION: &str = "pinned-source-fallback-v1";

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
            encode_path(&source.path),
            fragment
        ),
    })
}

/// Give callers a safe output target while the rendered-diff adapter cannot
/// prove a mutable PR Files-changed fragment. The reason is durable packet
/// context and must accompany a guide's navigation diagnostics.
pub fn rendered_target_or_pinned_fallback(reference: &SourceReference, reason: impl Into<String>) -> RenderedTarget {
    RenderedTarget::PinnedFallback {
        href: reference.href.clone(),
        reason: format!("{} ({RENDERED_TARGET_ADAPTER_VERSION})", reason.into()),
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
    if let Some(expected_head_branch) = expected_head_branch
        && metadata.head_ref_name != expected_head_branch
    {
        bail!(
            "PR head branch changed or belongs to another execution: expected `{expected_head_branch}`, got `{}`",
            metadata.head_ref_name,
        );
    }
    if endpoints_are_independently_observed {
        require_stable_endpoints(observed, &metadata)?;
    }
    let merge_base_sha =
        boss_github::pr_files::fetch_merge_base(&metadata.base_repository, &metadata.base_sha, &metadata.head_sha)
            .await?;
    let inventory = boss_github::pr_files::fetch_complete_pr_file_inventory(
        &metadata.base_repository,
        metadata.number,
        metadata.changed_files,
    )
    .await?;
    let latest = boss_github::pr_files::fetch_pr_comparison_metadata(pr_url).await?;
    require_stable_endpoints(observed, &latest)?;

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
        &metadata.base_repository,
        &merge_base_sha,
        &before_paths,
        SourceSide::Before,
    )
    .await?;
    let (after_entries, after_omissions) = fetch_pinned_tree_entries(
        &metadata.head_repository,
        &metadata.head_sha,
        &after_paths,
        SourceSide::After,
    )
    .await?;
    let before_errors = omission_reasons(before_omissions);
    let after_errors = omission_reasons(after_omissions);
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
            });
        }
        let before_path = file.previous_filename.clone().unwrap_or_else(|| file.filename.clone());
        let before = if change_kind.has_before() {
            let source = fetch_source(
                &metadata.base_repository,
                &merge_base_sha,
                &before_path,
                before_entries.get(&before_path),
                before_errors.get(&before_path).map(String::as_str),
            )
            .await;
            record_omission(&mut omissions, &source, SourceSide::Before);
            Some(source)
        } else {
            None
        };
        let after = if change_kind.has_after() {
            let source = fetch_source(
                &metadata.head_repository,
                &metadata.head_sha,
                &file.filename,
                after_entries.get(&file.filename),
                after_errors.get(&file.filename).map(String::as_str),
            )
            .await;
            record_omission(&mut omissions, &source, SourceSide::After);
            Some(source)
        } else {
            None
        };
        if file.patch.is_none() {
            omissions.push(SourceOmission {
                path: Some(file.filename.clone()),
                side: None,
                reason: "GitHub omitted the API patch; pinned source was collected instead".to_owned(),
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
        observed_base_sha: observed.base_sha.clone(),
        merge_base_sha,
        head_sha: observed.head_sha.clone(),
        files,
        omissions,
    })
}

/// Reject a collection whose live PR metadata no longer matches the pinned
/// comparison identity. Path/count equality is not enough: a same-count
/// force-push would otherwise mix a new inventory into an old packet.
pub fn require_stable_endpoints(
    observed: &PinnedComparison,
    metadata: &boss_github::pr_files::PrComparisonMetadata,
) -> Result<()> {
    if metadata.base_sha != observed.base_sha || metadata.head_sha != observed.head_sha {
        bail!(
            "PR endpoints changed while collecting sources: observed {}/{} but metadata returned {}/{}",
            observed.base_sha,
            observed.head_sha,
            metadata.base_sha,
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

async fn fetch_pinned_tree_entries(
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
        boss_github::trees::fetch_pinned_tree_directory(owner, repo, sha, &directory, |candidate| {
            names.contains(candidate)
        })
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
                let cause = match result {
                    Ok(_) => "directory response was truncated".to_owned(),
                    Err(error) => error.to_string(),
                };
                for name in names {
                    omissions.push(SourceOmission {
                        path: Some(join_repo_path(&directory, &name)),
                        side: Some(side),
                        reason: format!("could not read pinned tree {repository}@{sha} directory {directory}: {cause}"),
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

fn omission_reasons(omissions: Vec<SourceOmission>) -> HashMap<String, String> {
    omissions
        .into_iter()
        .filter_map(|omission| omission.path.map(|path| (path, omission.reason)))
        .collect()
}

async fn fetch_source(
    repository: &str,
    sha: &str,
    path: &str,
    entry: Option<&boss_github::trees::PinnedTreeEntry>,
    tree_error: Option<&str>,
) -> PinnedSource {
    if let Some(reason) = tree_error {
        return PinnedSource::omitted(repository, sha, path, reason.to_owned(), None);
    }
    let Some(entry) = entry else {
        return PinnedSource::omitted(
            repository,
            sha,
            path,
            "path was absent from the pinned Git tree".to_owned(),
            None,
        );
    };
    if let Some(reason) = pinned_entry_omission(entry) {
        return PinnedSource::omitted(repository, sha, path, reason, Some(entry));
    }
    let Some((owner, repo)) = repository.split_once('/') else {
        return PinnedSource::omitted(
            repository,
            sha,
            path,
            "repository is not owner/repo".to_owned(),
            Some(entry),
        );
    };
    match boss_github::contents::fetch_repo_file_bytes(owner, repo, path, sha).await {
        Ok(Some(bytes)) => {
            if bytes.len() as u64 > MAX_PINNED_SOURCE_BYTES {
                return PinnedSource::omitted(
                    repository,
                    sha,
                    path,
                    format!("pinned source exceeds the {MAX_PINNED_SOURCE_BYTES}-byte per-file capture budget"),
                    Some(entry),
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
            "file was absent at the pinned revision".to_owned(),
            Some(entry),
        ),
        Err(error) => PinnedSource::omitted(
            repository,
            sha,
            path,
            format!("pinned source read failed: {error:#}"),
            Some(entry),
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
        });
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn encode_path(path: &str) -> String {
    path.bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'/' => {
                vec![byte as char].into_iter().collect::<Vec<_>>()
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
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
        let source = fetch_source("acme/widget", "pinned", "broken/a.rs", None, Some(&result.1[0].reason))
            .now_or_never()
            .expect("tree failure needs no Contents request");
        assert_eq!(source.omission.as_deref(), Some(result.1[0].reason.as_str()));
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

    #[test]
    fn rendered_navigation_defaults_to_an_explicit_pinned_fallback() {
        let reference = validate_pinned_reference(&packet(), SourceSide::After, "src/with space.rs", 1, 1).unwrap();
        assert!(matches!(
            rendered_target_or_pinned_fallback(&reference, "rendered fragment was not validated"),
            RenderedTarget::PinnedFallback { .. }
        ));
    }

    #[test]
    fn omitted_source_makes_the_packet_incomplete() {
        let mut packet = packet();
        packet.files[0].after = Some(PinnedSource::omitted(
            "acme/widget",
            &"c".repeat(40),
            "src/with space.rs",
            "blob is unavailable".to_owned(),
            None,
        ));
        assert!(!packet.is_complete());
    }

    #[test]
    fn omitted_api_patch_does_not_hide_available_pinned_sources() {
        let mut packet = packet();
        packet.files[0].patch = None;
        packet.omissions.push(SourceOmission {
            path: Some("src/with space.rs".to_owned()),
            side: None,
            reason: "GitHub omitted the API patch; pinned source was collected instead".to_owned(),
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
        let source = PinnedSource::omitted("acme/widget", &"c".repeat(40), "link", reason, Some(&link));
        assert!(!source.is_captured());
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
}
