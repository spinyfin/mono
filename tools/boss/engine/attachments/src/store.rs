//! The content-addressed blob store and the ingest validation in front of it.
//!
//! ## Why the state root, not the workspace
//!
//! A cube workspace is ephemeral scratch: it is released at the end of a run
//! and re-leased by an unrelated task, which may reset it to `main`. Evidence
//! that lived there would be gone by the time someone inspected the run
//! locally. The store is
//! therefore under the **engine state root**, whose lifetime is the install's,
//! and the path a worker submitted is kept only as a basename for provenance.
//!
//! ## Why content addressing
//!
//! `<root>/<first two hex chars>/<sha256>.<ext>`. Deduplication falls out for
//! free — a revision chain that re-renders an unchanged view stores one blob,
//! not five — and it makes ingest idempotent without a separate key: the same
//! bytes from the same execution are the same attachment.
//!
//! ## Why the submitted path is confined
//!
//! The engine reads a path the *worker* chose, which is a confused-deputy
//! shape: without a rule, a worker could name `state.db` and have the engine
//! copy it into a store that is served over HTTP. Two independent gates close
//! that. [`confine_path`] canonicalises the path (so a symlink out of the
//! workspace is resolved and then caught, not followed blindly) and requires
//! it to land under a root the worker legitimately writes to; and the bytes
//! must sniff as a real image ([`crate::image::sniff`]), which no database,
//! key file, or transcript does.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use boss_protocol::{ATTACHMENT_MAX_BYTES, ATTACHMENT_MAX_PIXELS_PER_SIDE, AttachmentMediaType, ProposalFieldError};
use sha2::{Digest, Sha256};

use crate::image::{self, HEADER_SCAN_BYTES};

/// Directory name of the blob store under the engine state root.
pub const STORE_DIR_NAME: &str = "attachments";

/// Why an ingest was refused.
///
/// Every variant maps to a [`ProposalFieldError`] naming the flag the worker
/// typed, because the worker is an LLM mid-run and "rejected" is not an
/// instruction — the whole reason ingest is synchronous is so it can fix the
/// call and retry before it finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestRejection {
    /// No such file, or it is not readable.
    Unreadable { path: String, detail: String },
    /// The path exists but is a directory, socket, device, …
    NotAFile { path: String },
    /// Canonicalised outside every root this execution may submit from.
    OutsideAllowedRoots { path: String, roots: Vec<String> },
    /// Larger than [`ATTACHMENT_MAX_BYTES`].
    TooLarge { size_bytes: u64 },
    /// Not a PNG or JPEG by magic number, whatever the extension says.
    UnrecognisedFormat { path: String },
    /// Dimensions outside `1..=`[`ATTACHMENT_MAX_PIXELS_PER_SIDE`].
    DimensionsOutOfRange { width: u32, height: u32 },
    /// The blob could not be written into the store.
    StoreWriteFailed { detail: String },
}

impl IngestRejection {
    /// The payload field the complaint is about, as the caller named it.
    pub fn field(&self) -> &'static str {
        "path"
    }

    pub fn into_field_error(self) -> ProposalFieldError {
        ProposalFieldError::new(self.field(), self.to_string())
    }
}

impl fmt::Display for IngestRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IngestRejection::Unreadable { path, detail } => {
                write!(f, "cannot read `{path}`: {detail}")
            }
            IngestRejection::NotAFile { path } => {
                write!(f, "`{path}` is not a regular file")
            }
            IngestRejection::OutsideAllowedRoots { path, roots } => write!(
                f,
                "`{path}` resolves outside every directory this run may attach from. Attach an \
                 image written inside your own workspace or a temp dir. Allowed roots: {}",
                roots.join(", ")
            ),
            IngestRejection::TooLarge { size_bytes } => write!(
                f,
                "{size_bytes} bytes exceeds the {ATTACHMENT_MAX_BYTES}-byte cap. Attach a smaller \
                 render — the image is rejected rather than downscaled, so what local inspection shows is \
                 always exactly what you produced"
            ),
            IngestRejection::UnrecognisedFormat { path } => write!(
                f,
                "`{path}` is not a PNG or JPEG (checked by magic number, not by extension)"
            ),
            IngestRejection::DimensionsOutOfRange { width, height } => write!(
                f,
                "{width}x{height} is outside the accepted range 1..={ATTACHMENT_MAX_PIXELS_PER_SIDE} \
                 pixels per side"
            ),
            IngestRejection::StoreWriteFailed { detail } => {
                write!(f, "failed to store the image: {detail}")
            }
        }
    }
}

/// A validated image, ready to be recorded in `work_attachments`. Produced by
/// [`AttachmentStore::ingest`] *after* the bytes are durably in the store.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct IngestedImage {
    pub content_digest: String,
    pub media_type: AttachmentMediaType,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub size_bytes: u64,
    /// Basename only — the directory named a workspace that will not exist
    /// by the time anyone reads this row.
    pub source_name: String,
}

/// The set of directories one execution may attach from.
///
/// Built from the execution's own `workspace_path` plus the machine's temp
/// dirs, because those are where the two documented render routes actually
/// write: the app's `--capture-to` takes an arbitrary path (workers use
/// `/tmp`), and a throwaway XCTest writes to `TEST_UNDECLARED_OUTPUTS_DIR`
/// under Bazel's output base, which is reached through the `bazel-*`
/// convenience symlinks in the workspace root and canonicalises well outside
/// it.
///
/// ## What allowing the whole temp dir does and does not cost
///
/// The gate exists to stop a worker making the engine read a file the
/// *worker itself cannot*: the engine holds `state.db`, the events socket,
/// and the operator's home, and the worker's own settings deny it the Boss
/// support dir outright. Temp dirs are already worker-writable and
/// worker-readable, so admitting them grants no privilege the worker did not
/// have — which is exactly why the rule is drawn here rather than at a
/// narrower per-run scratch directory that would break both documented
/// render routes.
///
/// What it does admit is a sibling worker's temp file: worker A writes
/// `/tmp/x.png`, worker B attaches it. Both agents can already read each
/// other's temp files directly, so the only thing gained is publication to a
/// loopback gallery — noted, accepted, and bounded by the format gate, which
/// admits nothing but a real PNG or JPEG.
#[derive(Debug, Clone, Default)]
pub struct AllowedRoots {
    roots: Vec<PathBuf>,
}

impl AllowedRoots {
    /// Roots for an execution whose cube workspace is `workspace_path`
    /// (`None` for a run with no recorded workspace — it then gets temp dirs
    /// only, which is strictly narrower, never wider).
    pub fn for_workspace(workspace_path: Option<&Path>) -> Self {
        let mut roots: Vec<PathBuf> = Vec::new();
        let mut push = |path: PathBuf| {
            if let Ok(canonical) = path.canonicalize()
                && !roots.contains(&canonical)
            {
                roots.push(canonical);
            }
        };

        if let Some(workspace) = workspace_path {
            push(workspace.to_path_buf());
            // Bazel's convenience symlinks point into the output base, which
            // is outside the workspace. A test that rendered into
            // `TEST_UNDECLARED_OUTPUTS_DIR` is reached through one of these,
            // so their targets are legitimate roots for this run.
            for link in ["bazel-out", "bazel-bin", "bazel-testlogs"] {
                push(workspace.join(link));
            }
        }
        push(std::env::temp_dir());
        push(PathBuf::from("/tmp"));

        Self { roots }
    }

    /// Explicit roots (tests).
    pub fn from_paths(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            roots: paths.into_iter().filter_map(|p| p.canonicalize().ok()).collect(),
        }
    }

    pub fn display_list(&self) -> Vec<String> {
        self.roots.iter().map(|p| p.display().to_string()).collect()
    }

    fn contains(&self, candidate: &Path) -> bool {
        self.roots.iter().any(|root| candidate.starts_with(root))
    }
}

/// Resolve `submitted` to a real file under one of `allowed`, or explain why
/// not.
///
/// Canonicalisation happens *before* the containment test on purpose: a
/// symlink inside the workspace pointing at `~/.ssh/id_ed25519` resolves to
/// its target and is then rejected, where a textual prefix check on the
/// submitted path would have accepted it and read the target anyway.
pub fn confine_path(submitted: &Path, allowed: &AllowedRoots) -> Result<PathBuf, IngestRejection> {
    let shown = submitted.display().to_string();
    let canonical = submitted.canonicalize().map_err(|err| IngestRejection::Unreadable {
        path: shown.clone(),
        detail: err.to_string(),
    })?;
    let metadata = std::fs::metadata(&canonical).map_err(|err| IngestRejection::Unreadable {
        path: shown.clone(),
        detail: err.to_string(),
    })?;
    if !metadata.is_file() {
        return Err(IngestRejection::NotAFile { path: shown });
    }
    if !allowed.contains(&canonical) {
        return Err(IngestRejection::OutsideAllowedRoots {
            path: shown,
            roots: allowed.display_list(),
        });
    }
    Ok(canonical)
}

/// The blob store rooted at `<state_root>/attachments`.
#[derive(Debug, Clone)]
pub struct AttachmentStore {
    root: PathBuf,
}

impl AttachmentStore {
    /// Open (without creating) the store under an engine state root.
    pub fn under_state_root(state_root: &Path) -> Self {
        Self {
            root: state_root.join(STORE_DIR_NAME),
        }
    }

    /// Open a store at an exact path (tests, `bossctl --state-root`).
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where a blob with this digest lives. Two-hex-char fan-out keeps any
    /// single directory small enough that `ls` stays usable after years of
    /// retained evidence.
    pub fn blob_path(&self, content_digest: &str, media_type: AttachmentMediaType) -> PathBuf {
        let shard: String = content_digest.chars().take(2).collect();
        self.root
            .join(shard)
            .join(format!("{content_digest}.{}", media_type.extension()))
    }

    /// Validate `path` and copy it into the store.
    ///
    /// Ordered cheapest-refusal-first: containment, then size (from metadata,
    /// before reading anything), then format and dimensions from a bounded
    /// header read, and only then the full read and write. A worker that
    /// submits a 40 MB file never causes 40 MB to be read.
    pub fn ingest(&self, submitted: &Path, allowed: &AllowedRoots) -> Result<IngestedImage, IngestRejection> {
        let canonical = confine_path(submitted, allowed)?;
        let shown = submitted.display().to_string();

        let size_bytes = std::fs::metadata(&canonical)
            .map_err(|err| IngestRejection::Unreadable {
                path: shown.clone(),
                detail: err.to_string(),
            })?
            .len();
        if size_bytes > ATTACHMENT_MAX_BYTES {
            return Err(IngestRejection::TooLarge { size_bytes });
        }

        let mut file = std::fs::File::open(&canonical).map_err(|err| IngestRejection::Unreadable {
            path: shown.clone(),
            detail: err.to_string(),
        })?;
        let mut header = vec![0u8; HEADER_SCAN_BYTES.min(size_bytes as usize)];
        read_exact_prefix(&mut file, &mut header).map_err(|err| IngestRejection::Unreadable {
            path: shown.clone(),
            detail: err.to_string(),
        })?;
        let sniffed = image::sniff(&header).ok_or(IngestRejection::UnrecognisedFormat { path: shown.clone() })?;
        if sniffed.pixel_width == 0
            || sniffed.pixel_height == 0
            || sniffed.pixel_width > ATTACHMENT_MAX_PIXELS_PER_SIDE
            || sniffed.pixel_height > ATTACHMENT_MAX_PIXELS_PER_SIDE
        {
            return Err(IngestRejection::DimensionsOutOfRange {
                width: sniffed.pixel_width,
                height: sniffed.pixel_height,
            });
        }

        let bytes = std::fs::read(&canonical).map_err(|err| IngestRejection::Unreadable {
            path: shown.clone(),
            detail: err.to_string(),
        })?;
        let content_digest = digest_hex(&bytes);
        self.write_blob(&content_digest, sniffed.media_type, &bytes)
            .map_err(|err| IngestRejection::StoreWriteFailed {
                detail: format!("{err:#}"),
            })?;

        Ok(IngestedImage {
            content_digest,
            media_type: sniffed.media_type,
            pixel_width: sniffed.pixel_width,
            pixel_height: sniffed.pixel_height,
            size_bytes: bytes.len() as u64,
            source_name: submitted
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "image".to_owned()),
        })
    }

    /// Write bytes at their content address. Idempotent: an existing blob
    /// with this digest is the same bytes, so the write is skipped rather
    /// than repeated.
    pub fn write_blob(
        &self,
        content_digest: &str,
        media_type: AttachmentMediaType,
        bytes: &[u8],
    ) -> anyhow::Result<PathBuf> {
        let target = self.blob_path(content_digest, media_type);
        if target.is_file() {
            return Ok(target);
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Write-then-rename so a crash mid-write cannot leave a truncated
        // blob sitting at a content address that claims to be complete.
        let staging = target.with_extension(format!("{}.partial", media_type.extension()));
        std::fs::write(&staging, bytes)?;
        std::fs::rename(&staging, &target)?;
        Ok(target)
    }

    /// Read a stored blob back. `Ok(None)` when retention has reclaimed it.
    pub fn read_blob(&self, content_digest: &str, media_type: AttachmentMediaType) -> anyhow::Result<Option<Vec<u8>>> {
        let path = self.blob_path(content_digest, media_type);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Delete a stored blob. Already-absent counts as success — a retention
    /// pass must be idempotent.
    pub fn remove_blob(&self, content_digest: &str, media_type: AttachmentMediaType) -> anyhow::Result<()> {
        let path = self.blob_path(content_digest, media_type);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

/// Lowercase hex sha256.
pub fn digest_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut acc, byte| {
            use fmt::Write as _;
            let _ = write!(acc, "{byte:02x}");
            acc
        })
}

/// Fill `buf` from `reader`, tolerating a short file (which is not an error —
/// a 200-byte PNG is legal, it just has no 64 KiB of header to read).
fn read_exact_prefix(reader: &mut impl Read, buf: &mut Vec<u8>) -> std::io::Result<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    buf.truncate(filled);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::png_bytes;

    fn store_in(dir: &Path) -> AttachmentStore {
        AttachmentStore::at(dir.join("attachments"))
    }

    #[test]
    fn ingest_stores_by_digest_and_dedupes() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let store = store_in(tmp.path());
        let allowed = AllowedRoots::from_paths([workspace.clone()]);

        let bytes = png_bytes(1200, 800);
        let first = workspace.join("before.png");
        let second = workspace.join("after.png");
        std::fs::write(&first, &bytes).unwrap();
        std::fs::write(&second, &bytes).unwrap();

        let a = store.ingest(&first, &allowed).unwrap();
        let b = store.ingest(&second, &allowed).unwrap();

        assert_eq!(a.content_digest, b.content_digest, "identical bytes share a digest");
        assert_eq!((a.pixel_width, a.pixel_height), (1200, 800));
        assert_eq!(a.source_name, "before.png");
        assert_eq!(b.source_name, "after.png", "provenance is per-submission");
        assert_eq!(
            store.blob_path(&a.content_digest, a.media_type),
            store.blob_path(&b.content_digest, b.media_type),
            "one blob on disk, not two"
        );
        assert!(store.blob_path(&a.content_digest, a.media_type).is_file());
    }

    #[test]
    fn rejects_a_path_outside_the_allowed_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let store = store_in(tmp.path());
        let allowed = AllowedRoots::from_paths([workspace.clone()]);

        let outside = elsewhere.join("secret.png");
        std::fs::write(&outside, png_bytes(10, 10)).unwrap();

        assert!(matches!(
            store.ingest(&outside, &allowed),
            Err(IngestRejection::OutsideAllowedRoots { .. })
        ));
    }

    #[test]
    fn a_symlink_out_of_the_workspace_is_resolved_then_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let store = store_in(tmp.path());
        let allowed = AllowedRoots::from_paths([workspace.clone()]);

        let target = elsewhere.join("outside.png");
        std::fs::write(&target, png_bytes(10, 10)).unwrap();
        let link = workspace.join("looks-local.png");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // The submitted path is textually inside the workspace; only
        // canonicalising before the containment test catches this.
        assert!(matches!(
            store.ingest(&link, &allowed),
            Err(IngestRejection::OutsideAllowedRoots { .. })
        ));
    }

    #[test]
    fn rejects_a_non_image_with_an_image_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let store = store_in(tmp.path());
        let allowed = AllowedRoots::from_paths([workspace.clone()]);

        let fake = workspace.join("evidence.png");
        std::fs::write(&fake, b"SQLite format 3\0not an image at all").unwrap();

        assert!(matches!(
            store.ingest(&fake, &allowed),
            Err(IngestRejection::UnrecognisedFormat { .. })
        ));
    }

    #[test]
    fn rejects_absurd_dimensions() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let store = store_in(tmp.path());
        let allowed = AllowedRoots::from_paths([workspace.clone()]);

        let bomb = workspace.join("bomb.png");
        std::fs::write(&bomb, png_bytes(60_000, 60_000)).unwrap();
        assert!(matches!(
            store.ingest(&bomb, &allowed),
            Err(IngestRejection::DimensionsOutOfRange { .. })
        ));

        let empty = workspace.join("empty.png");
        std::fs::write(&empty, png_bytes(0, 0)).unwrap();
        assert!(matches!(
            store.ingest(&empty, &allowed),
            Err(IngestRejection::DimensionsOutOfRange { .. })
        ));
    }

    #[test]
    fn rejects_a_directory_and_a_missing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(workspace.join("subdir")).unwrap();
        let store = store_in(tmp.path());
        let allowed = AllowedRoots::from_paths([workspace.clone()]);

        assert!(matches!(
            store.ingest(&workspace.join("subdir"), &allowed),
            Err(IngestRejection::NotAFile { .. })
        ));
        assert!(matches!(
            store.ingest(&workspace.join("nope.png"), &allowed),
            Err(IngestRejection::Unreadable { .. })
        ));
    }

    #[test]
    fn oversize_is_refused_not_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let store = store_in(tmp.path());
        let allowed = AllowedRoots::from_paths([workspace.clone()]);

        let mut bytes = png_bytes(100, 100);
        bytes.resize(ATTACHMENT_MAX_BYTES as usize + 1, 0);
        let huge = workspace.join("huge.png");
        std::fs::write(&huge, &bytes).unwrap();

        assert!(matches!(
            store.ingest(&huge, &allowed),
            Err(IngestRejection::TooLarge { .. })
        ));
        // Nothing was written: a rejected submission leaves no trace.
        assert!(!store.root().exists() || std::fs::read_dir(store.root()).unwrap().count() == 0);
    }

    #[test]
    fn read_and_remove_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let store = store_in(tmp.path());
        let allowed = AllowedRoots::from_paths([workspace.clone()]);

        let path = workspace.join("shot.png");
        let bytes = png_bytes(64, 48);
        std::fs::write(&path, &bytes).unwrap();
        let ingested = store.ingest(&path, &allowed).unwrap();

        assert_eq!(
            store.read_blob(&ingested.content_digest, ingested.media_type).unwrap(),
            Some(bytes)
        );
        store
            .remove_blob(&ingested.content_digest, ingested.media_type)
            .unwrap();
        assert_eq!(
            store.read_blob(&ingested.content_digest, ingested.media_type).unwrap(),
            None
        );
        // Idempotent — a retention pass reruns over an already-reclaimed row.
        store
            .remove_blob(&ingested.content_digest, ingested.media_type)
            .unwrap();
    }

    #[test]
    fn digest_is_stable_sha256() {
        assert_eq!(
            digest_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
