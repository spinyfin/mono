//! Immutable, source-grounded packets for PR review guides.
//!
//! This crate owns the portable packet contract and the GitHub-backed
//! collector. It deliberately has no database or engine dependency: core owns
//! reconciliation, observation ordering, and durable persistence while this
//! crate only reads pinned GitHub objects and validates references against the
//! resulting packet.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const PACKET_SCHEMA_VERSION: u32 = 1;

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
    pub content: Option<String>,
    pub content_hash: Option<String>,
    pub byte_count: Option<u64>,
    pub omission: Option<String>,
}

impl PinnedSource {
    fn captured(repository: &str, sha: &str, path: &str, content: String) -> Self {
        let byte_count = content.len() as u64;
        let content_hash = hex_digest(content.as_bytes());
        Self {
            repository: repository.to_owned(),
            sha: sha.to_owned(),
            path: path.to_owned(),
            content: Some(content),
            content_hash: Some(content_hash),
            byte_count: Some(byte_count),
            omission: None,
        }
    }

    fn omitted(repository: &str, sha: &str, path: &str, omission: String) -> Self {
        Self {
            repository: repository.to_owned(),
            sha: sha.to_owned(),
            path: path.to_owned(),
            content: None,
            content_hash: None,
            byte_count: None,
            omission: Some(omission),
        }
    }

    fn is_captured(&self) -> bool {
        self.content.is_some() && self.content_hash.is_some() && self.omission.is_none()
    }
}

/// GitHub's changed-file classification, preserving the API value instead of
/// flattening a rename or deletion into an ordinary modification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Copied,
    Deleted,
    Modified,
    Renamed,
    Changed,
}

impl ChangeKind {
    fn from_api(status: &str) -> Self {
        match status.to_ascii_lowercase().as_str() {
            "added" => Self::Added,
            "copied" => Self::Copied,
            "deleted" | "removed" => Self::Deleted,
            "renamed" => Self::Renamed,
            "changed" => Self::Changed,
            _ => Self::Modified,
        }
    }

    fn has_before(self) -> bool {
        !matches!(self, Self::Added | Self::Copied)
    }

    fn has_after(self) -> bool {
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
    if let Some(expected_head_branch) = expected_head_branch
        && metadata.head_ref_name != expected_head_branch
    {
        bail!(
            "PR head branch changed or belongs to another execution: expected `{expected_head_branch}`, got `{}`",
            metadata.head_ref_name,
        );
    }
    if metadata.base_sha != observed.base_sha || metadata.head_sha != observed.head_sha {
        bail!(
            "PR endpoints changed while collecting sources: observed {}/{} but metadata returned {}/{}",
            observed.base_sha,
            observed.head_sha,
            metadata.base_sha,
            metadata.head_sha,
        );
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

    let mut omissions = Vec::new();
    let mut files = Vec::with_capacity(inventory.len());
    for file in inventory {
        let change_kind = ChangeKind::from_api(&file.status);
        let before_path = file.previous_filename.clone().unwrap_or_else(|| file.filename.clone());
        let before = if change_kind.has_before() {
            let source = fetch_source(&metadata.base_repository, &merge_base_sha, &before_path).await;
            record_omission(&mut omissions, &source, SourceSide::Before);
            Some(source)
        } else {
            None
        };
        let after = if change_kind.has_after() {
            let source = fetch_source(&metadata.head_repository, &metadata.head_sha, &file.filename).await;
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

async fn fetch_source(repository: &str, sha: &str, path: &str) -> PinnedSource {
    let Some((owner, repo)) = repository.split_once('/') else {
        return PinnedSource::omitted(repository, sha, path, "repository is not owner/repo".to_owned());
    };
    match boss_github::contents::fetch_repo_file(owner, repo, path, sha).await {
        Ok(Some(content)) => PinnedSource::captured(repository, sha, path, content),
        Ok(None) => PinnedSource::omitted(
            repository,
            sha,
            path,
            "file was absent at the pinned revision".to_owned(),
        ),
        Err(error) => PinnedSource::omitted(repository, sha, path, format!("pinned source read failed: {error:#}")),
    }
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
                )),
                after: Some(PinnedSource::captured(
                    "acme/widget",
                    &"c".repeat(40),
                    "src/with space.rs",
                    "first\nsecond\n".to_owned(),
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
}
