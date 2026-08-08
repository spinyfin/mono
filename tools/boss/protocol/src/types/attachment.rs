//! Work attachments: the durable, engine-owned image evidence a worker hands
//! to a human reviewer.
//!
//! A worker making a UI change can render a screenshot but has nowhere to put
//! it. Committing capture PNGs to the branch is forbidden (and rots — the one
//! PR in `spinyfin/mono` that ever linked `raw.githubusercontent.com` images
//! 404s today, because the branch was deleted on merge), and GitHub's
//! attachment upload endpoint is not part of the public REST API, so `gh`
//! cannot reach it. The gap this type closes is the one between "the worker
//! saw the image" and "the reviewer can look at it".
//!
//! `WorkAttachment` mirrors the `work_attachments` table verbatim. Bytes live
//! outside the row, content-addressed under the engine state root, so an
//! attachment survives the cube workspace that produced it being released and
//! recycled — the workspace is ephemeral scratch, the evidence is not.
//!
//! Ingest is mediated exactly like a proposal is: the worker submits over the
//! frontend socket, the engine attributes the call from the socket peer's pid,
//! validates, and answers synchronously with a typed error the worker can act
//! on mid-run (the caps below are the validation vocabulary). Attachments are
//! deliberately *not* a [`crate::ProposalKind`]: a proposal is a JSON payload
//! carrying a judgment that the apply pipeline dispositions, while an
//! attachment is bytes with no judgment attached and nothing to decide.
//!
//! Design: `tools/boss/docs/designs/worker-screenshot-evidence-attachments.md`.

use serde::{Deserialize, Serialize};

/// Largest single image the engine will ingest, in bytes.
///
/// A full-screen Retina PNG of the Boss window is well under 2 MiB, so 8 MiB
/// is roughly 4x headroom rather than a ceiling anyone bumps into
/// accidentally. Oversize submissions are **rejected**, never downscaled or
/// truncated: silently storing something other than what the worker rendered
/// would make the evidence a lie.
pub const ATTACHMENT_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Largest accepted pixel dimension on either axis. Bounds the decode work
/// the HTML gallery asks a browser to do, and catches a "screenshot" that is
/// really a decompression bomb header.
pub const ATTACHMENT_MAX_PIXELS_PER_SIDE: u32 = 10_000;

/// How many attachments one execution may store. Runaway-loop protection in
/// the same spirit as [`crate::PROPOSAL_CAP_PER_KIND_PER_EXECUTION`] — a run
/// showing two dozen before/after renders is already an unusually visual PR;
/// a run submitting two hundred is looping.
pub const ATTACHMENT_CAP_PER_EXECUTION: usize = 24;

/// How many attachments may accumulate across every execution of one work
/// item. A revision chain produces several runs against one PR, so the
/// per-execution cap alone would let a long chain grow without bound.
pub const ATTACHMENT_CAP_PER_WORK_ITEM: usize = 96;

/// Image formats the engine accepts.
///
/// Deliberately tiny and closed. Both are recognised by magic bytes rather
/// than by the submitted filename's extension — the extension is a claim, the
/// header is evidence — and both have a header the engine can read dimensions
/// out of without decoding pixel data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentMediaType {
    Png,
    Jpeg,
}

impl AttachmentMediaType {
    pub const ALL: &'static [AttachmentMediaType] = &[AttachmentMediaType::Png, AttachmentMediaType::Jpeg];

    /// The MIME type, as served by the evidence HTTP surface.
    pub fn mime(self) -> &'static str {
        match self {
            AttachmentMediaType::Png => "image/png",
            AttachmentMediaType::Jpeg => "image/jpeg",
        }
    }

    /// The extension the content-addressed blob is stored under. Cosmetic —
    /// the digest is the identity — but it makes the store browsable with
    /// `open(1)` when someone is debugging by hand.
    pub fn extension(self) -> &'static str {
        match self {
            AttachmentMediaType::Png => "png",
            AttachmentMediaType::Jpeg => "jpg",
        }
    }

    /// The stored (and wire) discriminant.
    pub fn as_str(self) -> &'static str {
        match self {
            AttachmentMediaType::Png => "png",
            AttachmentMediaType::Jpeg => "jpeg",
        }
    }
}

impl std::fmt::Display for AttachmentMediaType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for AttachmentMediaType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "png" => Ok(AttachmentMediaType::Png),
            "jpeg" | "jpg" => Ok(AttachmentMediaType::Jpeg),
            other => Err(format!(
                "unknown attachment media type `{other}`; expected one of: png, jpeg"
            )),
        }
    }
}

/// One stored image, as persisted in `work_attachments`.
///
/// Bound to **both** halves of the association question the design poses:
/// `execution_id` says which run rendered it (a reviewer comparing a revision
/// against its predecessor needs that), and `work_item_id` is the
/// reviewer-facing scope — one gallery per work item, so a revision chain's
/// several runs against one PR collect in one place instead of scattering.
///
/// `content_digest` is the sha256 of the bytes and is the blob's identity:
/// two runs that render an identical image share one file on disk, and a
/// resubmission inside one execution is a no-op rather than a duplicate row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct WorkAttachment {
    /// `atc_<hexnanos>_<hexcounter>`.
    pub id: String,
    /// The execution that submitted it. Verified from the socket peer at
    /// ingest — never a worker-supplied string.
    pub execution_id: String,
    /// Derived from the execution at ingest, so a revision's attachments land
    /// on the chain root's work item alongside its predecessors'.
    pub work_item_id: String,
    /// Worker-supplied one-line description of what the image shows. Empty
    /// when the worker gave none; the gallery falls back to the filename.
    #[builder(default)]
    pub caption: String,
    /// Lowercase hex sha256 of the stored bytes.
    pub content_digest: String,
    /// Epoch seconds, as a string — the repo's `created_at` convention.
    pub created_at: String,
    pub media_type: AttachmentMediaType,
    pub pixel_height: u32,
    pub pixel_width: u32,
    pub size_bytes: u64,
    /// Basename of the path the worker submitted, kept for provenance and as
    /// the gallery's caption fallback. Only the basename: the full path names
    /// a workspace that no longer exists by the time anyone reads this.
    pub source_name: String,
    /// Set when retention reclaimed the bytes. The row outlives the blob on
    /// purpose — a link in a merged PR body then answers "this evidence was
    /// reclaimed on <date>" instead of a bare 404 that reads like a bug.
    pub reclaimed_at: Option<String>,
}

impl WorkAttachment {
    /// Whether the bytes are gone (retention reclaimed them) and only the
    /// tombstone row remains.
    pub fn is_reclaimed(&self) -> bool {
        self.reclaimed_at.is_some()
    }
}

/// URL path of one image on the evidence HTTP surface. Shared by the engine
/// (which mints the URL a worker pastes into a PR body) and the CLI (which
/// renders listings), so the two can never disagree about the path shape.
pub fn attachment_image_path(attachment_id: &str) -> String {
    format!("/a/{attachment_id}")
}

/// URL path of one work item's evidence gallery — the link that goes in a PR
/// body. Scoped to the work item rather than the execution so the reviewer
/// gets every run's evidence for the PR, newest run first.
pub fn attachment_gallery_path(work_item_id: &str) -> String {
    format!("/w/{work_item_id}")
}

/// Absolute image URL under `base_url` (e.g. `http://127.0.0.1:8419`).
pub fn attachment_image_url(base_url: &str, attachment_id: &str) -> String {
    format!(
        "{}{}",
        base_url.trim_end_matches('/'),
        attachment_image_path(attachment_id)
    )
}

/// Absolute gallery URL under `base_url`.
pub fn attachment_gallery_url(base_url: &str, work_item_id: &str) -> String {
    format!(
        "{}{}",
        base_url.trim_end_matches('/'),
        attachment_gallery_path(work_item_id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_type_round_trips_through_str() {
        for media in AttachmentMediaType::ALL {
            assert_eq!(media.as_str().parse::<AttachmentMediaType>().unwrap(), *media);
        }
        // `jpg` is accepted as an alias on the way in but never emitted, so a
        // stored discriminant stays a single spelling.
        assert_eq!("jpg".parse::<AttachmentMediaType>().unwrap(), AttachmentMediaType::Jpeg);
    }

    #[test]
    fn urls_are_stable_and_base_slash_insensitive() {
        assert_eq!(
            attachment_image_url("http://127.0.0.1:8419", "atc_1"),
            "http://127.0.0.1:8419/a/atc_1"
        );
        assert_eq!(
            attachment_image_url("http://127.0.0.1:8419/", "atc_1"),
            "http://127.0.0.1:8419/a/atc_1"
        );
        assert_eq!(
            attachment_gallery_url("http://127.0.0.1:8419/", "task_9"),
            "http://127.0.0.1:8419/w/task_9"
        );
    }
}
