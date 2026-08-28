//! Incremental JSONL tail-watcher for agent-driver transcript files.
//!
//! Each worker session writes a transcript JSONL file at a path the
//! engine records on `WorkRun.transcript_path` — reported by the run's
//! driver via `AgentDriver::transcript_path_for_session`. The
//! hooks-to-socket channel gives us discrete events; the transcript
//! carries richer per-token content. This module is the primitive that
//! streams that file as it grows, returning each newly-written JSONL
//! line as a parsed [`serde_json::Value`], driver-agnostic — callers pass
//! each line through the driver's `normalize_transcript_entry` before
//! redaction or summarisation.
//!
//! The watcher tolerates:
//!
//! - The file not existing yet (returns no events; will pick up content
//!   when it appears).
//! - Partial trailing lines (held in an internal buffer and flushed on
//!   the next poll once the line completes).
//! - Truncation / rotation (size shrinking below our cursor resets the
//!   cursor and re-reads from the start).
//!
//! Callers choose the polling cadence themselves — this module exposes
//! a single [`TranscriptTail::poll`] method rather than spawning a task.

use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

const CHECKPOINT_BYTES: u64 = 4096;

#[derive(Debug, Error)]
pub enum TailError {
    #[error("transcript io: {0}")]
    Io(#[from] io::Error),
    #[error("transcript bytes are not valid utf-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("transcript line is not valid JSON: {error} (line: {line:?})")]
    Json { error: serde_json::Error, line: String },
    #[error("transcript containment: {0}")]
    Containment(String),
}

#[derive(Debug)]
struct TranscriptContainment {
    root: PathBuf,
    canonical_root: PathBuf,
    root_identity: FileIdentity,
    file_identity: FileIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug)]
struct FileCheckpoint {
    prefix: Vec<u8>,
    suffix_start: u64,
    suffix: Vec<u8>,
}

#[derive(Debug, bon::Builder)]
#[builder(on(String, into))]
pub struct TranscriptTail {
    path: PathBuf,
    #[builder(skip)]
    containment: Option<TranscriptContainment>,
    #[builder(skip)]
    file_identity: Option<FileIdentity>,
    /// Bounded byte windows at the beginning and end of the consumed prefix.
    /// Comparing them before every incremental read detects same-inode
    /// truncate-and-regrow rewrites whose final size is equal to or larger
    /// than the prior cursor.
    #[builder(skip)]
    checkpoint: Option<FileCheckpoint>,
    #[builder(skip)]
    offset: u64,
    /// Incremented whenever the watched file is truncated or, for
    /// unrestricted tails, replaced by a different inode. Callers that fold
    /// lines into cumulative state use this to discard the old generation
    /// before ingesting the replay from byte zero.
    #[builder(skip)]
    generation: u64,
    /// Buffer for a trailing partial line carried across polls.
    #[builder(skip)]
    partial: String,
}

impl TranscriptTail {
    pub fn new<P: Into<PathBuf>>(path: P) -> Self {
        Self {
            path: path.into(),
            containment: None,
            file_identity: None,
            checkpoint: None,
            offset: 0,
            generation: 0,
            partial: String::new(),
        }
    }

    /// Create a tail whose file must remain a regular, single-link file under
    /// `root` at every poll. Construction validates the persisted path;
    /// [`Self::poll`] repeats the check against the descriptor it actually
    /// consumes so path replacement after persistence cannot redirect reads.
    pub fn new_contained<P: Into<PathBuf>, R: Into<PathBuf>>(path: P, root: R) -> Result<Self, TailError> {
        let path = path.into();
        let root = root.into();
        let (canonical_root, root_identity, file_identity) = verify_initial_containment(&path, &root)?;
        Ok(Self {
            path,
            containment: Some(TranscriptContainment {
                root,
                canonical_root,
                root_identity,
                file_identity,
            }),
            file_identity: Some(file_identity),
            checkpoint: None,
            offset: 0,
            generation: 0,
            partial: String::new(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Read any new content since the last poll and return parsed JSONL
    /// values. Returns an empty vec if the file does not exist or has
    /// not grown.
    pub async fn poll(&mut self) -> Result<Vec<serde_json::Value>, TailError> {
        let mut file = match open_no_follow(&self.path).await {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };

        let metadata = file.metadata().await?;
        if let Some(containment) = self.containment.as_ref() {
            verify_opened_containment(&self.path, containment, &metadata).await?;
        }
        let size = metadata.len();
        let opened_identity = file_identity(&metadata);
        let replaced = self
            .file_identity
            .is_some_and(|prior_identity| prior_identity != opened_identity);
        let checkpoint_changed = if !replaced && size >= self.offset {
            match self.checkpoint.as_ref() {
                Some(checkpoint) => !checkpoint_matches(&mut file, checkpoint).await?,
                None => self.offset > 0,
            }
        } else {
            false
        };

        // Truncation / rotation: a shorter file, a new unrestricted inode, or
        // changed prefix/checkpoint bytes means byte offsets and every
        // cumulative consumer derived from them belong to an obsolete
        // generation. Reset and re-read from the top. Contained tails reject
        // path replacement above instead of following it.
        if size < self.offset || replaced || checkpoint_changed {
            self.offset = 0;
            self.checkpoint = None;
            self.partial.clear();
            self.generation = self.generation.saturating_add(1);
        }
        self.file_identity = Some(opened_identity);

        if size == self.offset {
            self.checkpoint = read_checkpoint(&mut file, size).await?;
            return Ok(Vec::new());
        }

        file.seek(SeekFrom::Start(self.offset)).await?;
        let to_read = size - self.offset;
        let mut buf = Vec::with_capacity(to_read as usize);
        (&mut file).take(to_read).read_to_end(&mut buf).await?;

        let chunk = String::from_utf8(buf)?;
        let mut combined = self.partial.clone();
        combined.push_str(&chunk);

        let mut values = Vec::new();
        let mut start = 0usize;
        let bytes = combined.as_bytes();
        for i in 0..bytes.len() {
            if bytes[i] == b'\n' {
                let line = &combined[start..i];
                start = i + 1;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(trimmed) {
                    Ok(value) => values.push(value),
                    Err(error) => {
                        return Err(TailError::Json {
                            error,
                            line: trimmed.to_owned(),
                        });
                    }
                }
            }
        }
        let checkpoint = read_checkpoint(&mut file, size).await?;
        self.offset = size;
        self.checkpoint = checkpoint;
        self.partial = combined[start..].to_owned();

        Ok(values)
    }
}

async fn checkpoint_matches(file: &mut fs::File, checkpoint: &FileCheckpoint) -> io::Result<bool> {
    file.seek(SeekFrom::Start(0)).await?;
    let mut current_prefix = vec![0; checkpoint.prefix.len()];
    if let Err(error) = file.read_exact(&mut current_prefix).await {
        return if error.kind() == io::ErrorKind::UnexpectedEof {
            Ok(false)
        } else {
            Err(error)
        };
    }
    if current_prefix != checkpoint.prefix {
        return Ok(false);
    }
    if checkpoint.suffix_start == 0 {
        return Ok(true);
    }

    file.seek(SeekFrom::Start(checkpoint.suffix_start)).await?;
    let mut current_suffix = vec![0; checkpoint.suffix.len()];
    match file.read_exact(&mut current_suffix).await {
        Ok(_) => Ok(current_suffix == checkpoint.suffix),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

async fn read_checkpoint(file: &mut fs::File, offset: u64) -> io::Result<Option<FileCheckpoint>> {
    if offset == 0 {
        return Ok(None);
    }
    let length = offset.min(CHECKPOINT_BYTES);
    file.seek(SeekFrom::Start(0)).await?;
    let mut prefix = vec![0; length as usize];
    file.read_exact(&mut prefix).await?;

    let suffix_start = offset - length;
    let suffix = if suffix_start == 0 {
        prefix.clone()
    } else {
        file.seek(SeekFrom::Start(suffix_start)).await?;
        let mut suffix = vec![0; length as usize];
        file.read_exact(&mut suffix).await?;
        suffix
    };
    Ok(Some(FileCheckpoint {
        prefix,
        suffix_start,
        suffix,
    }))
}

fn verify_initial_containment(path: &Path, root: &Path) -> Result<(PathBuf, FileIdentity, FileIdentity), TailError> {
    let root_meta = std::fs::symlink_metadata(root).map_err(TailError::Io)?;
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        return Err(TailError::Containment(format!(
            "root {} is not a real directory",
            root.display()
        )));
    }
    let canonical_root = std::fs::canonicalize(root).map_err(TailError::Io)?;
    let path_meta = std::fs::symlink_metadata(path).map_err(TailError::Io)?;
    if path_meta.file_type().is_symlink() || !path_meta.is_file() {
        return Err(TailError::Containment(format!(
            "path {} is not a real file",
            path.display()
        )));
    }
    let canonical_path = std::fs::canonicalize(path).map_err(TailError::Io)?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(TailError::Containment(format!(
            "path {} is outside root {}",
            canonical_path.display(),
            canonical_root.display()
        )));
    }
    Ok((canonical_root, file_identity(&root_meta), file_identity(&path_meta)))
}

async fn open_no_follow(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    options.open(path).await
}

async fn verify_opened_containment(
    path: &Path,
    containment: &TranscriptContainment,
    opened: &std::fs::Metadata,
) -> Result<(), TailError> {
    let root_meta = fs::symlink_metadata(&containment.root).await?;
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        return Err(TailError::Containment(format!(
            "root {} was replaced",
            containment.root.display()
        )));
    }
    let canonical_root = fs::canonicalize(&containment.root).await?;
    if canonical_root != containment.canonical_root {
        return Err(TailError::Containment(format!(
            "root {} changed identity",
            containment.root.display()
        )));
    }
    if file_identity(&root_meta) != containment.root_identity {
        return Err(TailError::Containment(format!(
            "root {} was replaced in place",
            containment.root.display()
        )));
    }

    let path_meta = fs::symlink_metadata(path).await?;
    if path_meta.file_type().is_symlink() || !path_meta.is_file() || !opened.is_file() {
        return Err(TailError::Containment(format!(
            "path {} is no longer a real file",
            path.display()
        )));
    }
    let canonical_path = fs::canonicalize(path).await?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(TailError::Containment(format!(
            "path {} escaped root {}",
            canonical_path.display(),
            canonical_root.display()
        )));
    }
    if !same_file_identity(opened, &path_meta) {
        return Err(TailError::Containment(format!(
            "opened file {} differs from the persisted path",
            path.display()
        )));
    }
    if file_identity(opened) != containment.file_identity {
        return Err(TailError::Containment(format!(
            "path {} was replaced in place",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;

    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(not(unix))]
fn file_identity(_metadata: &std::fs::Metadata) -> FileIdentity {
    FileIdentity { device: 0, inode: 0 }
}

#[cfg(unix)]
fn same_file_identity(opened: &std::fs::Metadata, path: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    opened.dev() == path.dev() && opened.ino() == path.ino() && opened.nlink() == 1
}

#[cfg(not(unix))]
fn same_file_identity(_opened: &std::fs::Metadata, _path: &std::fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;
    use tokio::io::AsyncWriteExt;

    fn sample_path(dir: &TempDir, name: &str) -> PathBuf {
        dir.path().join(name)
    }

    #[tokio::test]
    async fn missing_file_yields_no_events() {
        let dir = TempDir::new().unwrap();
        let mut tail = TranscriptTail::new(sample_path(&dir, "missing.jsonl"));
        let events = tail.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn empty_file_yields_no_events() {
        let dir = TempDir::new().unwrap();
        let path = sample_path(&dir, "empty.jsonl");
        fs::File::create(&path).await.unwrap();
        let mut tail = TranscriptTail::new(path);
        let events = tail.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn returns_each_complete_line_then_holds_partial() {
        let dir = TempDir::new().unwrap();
        let path = sample_path(&dir, "log.jsonl");
        let mut tail = TranscriptTail::new(path.clone());

        // First write: two complete lines + a partial.
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, r#"{{"i": 1}}"#).unwrap();
            writeln!(f, r#"{{"i": 2}}"#).unwrap();
            write!(f, r#"{{"i":"#).unwrap(); // partial
        }

        let events = tail.poll().await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["i"], 1);
        assert_eq!(events[1]["i"], 2);

        // Second write: complete the partial + append another line.
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f, r#" 3}}"#).unwrap();
            writeln!(f, r#"{{"i": 4}}"#).unwrap();
        }

        let events = tail.poll().await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["i"], 3);
        assert_eq!(events[1]["i"], 4);
    }

    #[tokio::test]
    async fn no_new_content_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = sample_path(&dir, "log.jsonl");
        std::fs::write(&path, "{\"x\":1}\n").unwrap();
        let mut tail = TranscriptTail::new(path);

        let first = tail.poll().await.unwrap();
        assert_eq!(first.len(), 1);

        let second = tail.poll().await.unwrap();
        assert!(second.is_empty());
    }

    #[tokio::test]
    async fn truncation_resets_cursor_and_reemits() {
        let dir = TempDir::new().unwrap();
        let path = sample_path(&dir, "log.jsonl");
        std::fs::write(&path, "{\"a\":1}\n{\"a\":2}\n").unwrap();

        let mut tail = TranscriptTail::new(path.clone());
        let first = tail.poll().await.unwrap();
        assert_eq!(first.len(), 2);

        // Truncate to a single short line.
        std::fs::write(&path, "{\"b\":3}\n").unwrap();
        let second = tail.poll().await.unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0]["b"], 3);
        assert_eq!(tail.generation(), 1);
    }

    #[tokio::test]
    async fn equal_size_same_inode_rewrite_resets_cursor_and_reemits() {
        let dir = TempDir::new().unwrap();
        let path = sample_path(&dir, "log.jsonl");
        std::fs::write(&path, "{\"old\":1}\n").unwrap();
        let identity = file_identity(&std::fs::metadata(&path).unwrap());

        let mut tail = TranscriptTail::new(path.clone());
        let first = tail.poll().await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["old"], 1);

        std::fs::write(&path, "{\"new\":2}\n").unwrap();
        assert_eq!(file_identity(&std::fs::metadata(&path).unwrap()), identity);
        let second = tail.poll().await.unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0]["new"], 2);
        assert_eq!(tail.generation(), 1);
    }

    #[tokio::test]
    async fn larger_same_inode_rewrite_resets_cursor_and_reemits_prefix() {
        let dir = TempDir::new().unwrap();
        let path = sample_path(&dir, "log.jsonl");
        std::fs::write(&path, "{\"old\":1}\n").unwrap();
        let identity = file_identity(&std::fs::metadata(&path).unwrap());

        let mut tail = TranscriptTail::new(path.clone());
        assert_eq!(tail.poll().await.unwrap().len(), 1);

        std::fs::write(&path, "{\"new\":2}\n{\"extra\":3}\n").unwrap();
        assert_eq!(file_identity(&std::fs::metadata(&path).unwrap()), identity);
        let second = tail.poll().await.unwrap();
        assert_eq!(second.len(), 2);
        assert_eq!(second[0]["new"], 2);
        assert_eq!(second[1]["extra"], 3);
        assert_eq!(tail.generation(), 1);
    }

    #[tokio::test]
    async fn blank_lines_are_skipped() {
        let dir = TempDir::new().unwrap();
        let path = sample_path(&dir, "log.jsonl");
        std::fs::write(&path, "\n{\"x\":1}\n\n{\"x\":2}\n\n").unwrap();
        let mut tail = TranscriptTail::new(path);
        let events = tail.poll().await.unwrap();
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn malformed_line_returns_json_error_with_line_text() {
        let dir = TempDir::new().unwrap();
        let path = sample_path(&dir, "log.jsonl");
        std::fs::write(&path, "{\"ok\":1}\n{not json\n").unwrap();
        let mut tail = TranscriptTail::new(path);
        let result = tail.poll().await;
        match result {
            Err(TailError::Json { line, .. }) => assert_eq!(line, "{not json"),
            other => panic!("expected JSON error with line, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn file_appearing_after_initial_poll_is_picked_up() {
        let dir = TempDir::new().unwrap();
        let path = sample_path(&dir, "later.jsonl");
        let mut tail = TranscriptTail::new(path.clone());

        // First poll: file does not exist yet.
        assert!(tail.poll().await.unwrap().is_empty());

        // Now create it with content.
        let mut f = fs::File::create(&path).await.unwrap();
        f.write_all(b"{\"hello\":\"world\"}\n").await.unwrap();
        f.sync_all().await.unwrap();
        drop(f);

        let events = tail.poll().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["hello"], "world");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn contained_tail_rejects_file_replaced_by_symlink_after_persistence() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        let root = dir.path().join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("rollout.jsonl");
        std::fs::write(&path, "{\"safe\":true}\n").unwrap();
        let outside = dir.path().join("outside.jsonl");
        std::fs::write(&outside, "{\"stolen\":true}\n").unwrap();
        let mut tail = TranscriptTail::new_contained(&path, &root).unwrap();

        std::fs::remove_file(&path).unwrap();
        symlink(&outside, &path).unwrap();

        assert!(tail.poll().await.is_err(), "O_NOFOLLOW must reject the replaced file");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn contained_tail_rejects_file_replaced_by_regular_file_after_persistence() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("rollout.jsonl");
        std::fs::write(&path, "{\"safe\":true}\n").unwrap();
        let mut tail = TranscriptTail::new_contained(&path, &root).unwrap();

        // Sibling-then-rename, not unlink-and-recreate: linux-sandbox tmpfs
        // reuses the freed inode, which would make this look like the same
        // file and miss the containment pin this test is for.
        let replacement = root.join("rollout.replacement");
        std::fs::write(&replacement, "{\"replaced\":true}\n").unwrap();
        std::fs::rename(&replacement, &path).unwrap();

        assert!(
            matches!(tail.poll().await, Err(TailError::Containment(_))),
            "the tail must remain pinned to the file selected at construction"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn contained_tail_rejects_parent_replaced_after_persistence() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        let root = dir.path().join("sessions");
        let day = root.join("2026-07-26");
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join("rollout.jsonl");
        std::fs::write(&path, "{\"safe\":true}\n").unwrap();
        let outside_day = dir.path().join("outside-day");
        std::fs::create_dir_all(&outside_day).unwrap();
        std::fs::write(outside_day.join("rollout.jsonl"), "{\"stolen\":true}\n").unwrap();
        let mut tail = TranscriptTail::new_contained(&path, &root).unwrap();

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&day).unwrap();
        symlink(&outside_day, &day).unwrap();

        assert!(
            matches!(tail.poll().await, Err(TailError::Containment(_))),
            "the opened descriptor must be rejected when a parent redirects outside the root"
        );
    }
}
