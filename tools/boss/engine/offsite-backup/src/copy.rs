//! Publish a finished backup into the per-host destination folder as one
//! complete file.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use boss_engine_utils::atomic_blob::write_stream_atomic;

/// File-name prefix of finished backups (shared with the local backup).
pub const BACKUP_FILE_PREFIX: &str = "state.db.bak-";

/// Result of a successful copy.
#[derive(Debug)]
pub struct CopyOutcome {
    pub copied_path: PathBuf,
    pub bytes: u64,
}

/// Copy the finished backup `src` into `host_dir` under its own file name.
///
/// See [`copy_open_to_offsite`] for how the copy is staged and published.
pub fn copy_to_offsite(src: &Path, host_dir: &Path, staging_dir: &Path) -> Result<CopyOutcome> {
    let name = backup_file_name(src)?;
    let source = File::open(src).with_context(|| format!("open {}", src.display()))?;
    copy_open_to_offsite(source, name, host_dir, staging_dir)
}

/// Validate that `src` names a finished backup and return its file name.
pub fn backup_file_name(src: &Path) -> Result<&str> {
    let name = src
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("backup path has no UTF-8 file name: {}", src.display()))?;
    if !name.starts_with(BACKUP_FILE_PREFIX) {
        // Guard the invariant that only finished backups are copied — never
        // the live state.db / -wal / -shm.
        bail!("refusing to copy {name}: not a finished `{BACKUP_FILE_PREFIX}*` backup");
    }
    Ok(name)
}

/// Like [`copy_to_offsite`], streaming from an already-open `source` so the
/// copy survives the local path being pruned in the meantime.
///
/// The destination is watched by a sync agent, which uploads whatever it
/// finds there, including a file that is still being written. So the copy is
/// written and fsynced in `staging_dir`, a directory outside the destination
/// that the caller owns, and only then renamed into `host_dir`. On the same
/// volume that rename is atomic: the agent sees one complete new file that is
/// never modified afterwards.
///
/// When `staging_dir` is on a different volume from `host_dir` a rename
/// cannot cross over, so the copy is staged as a sibling inside `host_dir`
/// instead. The final name still only ever appears complete, but the agent
/// can observe the staging sibling.
///
/// Either way the published file is checked against the copied byte count
/// before the copy is reported as done.
pub fn copy_open_to_offsite(mut source: File, name: &str, host_dir: &Path, staging_dir: &Path) -> Result<CopyOutcome> {
    copy_stream_to_offsite(&mut source, name, host_dir, staging_dir)
}

fn copy_stream_to_offsite(
    source: &mut impl Read,
    name: &str,
    host_dir: &Path,
    staging_dir: &Path,
) -> Result<CopyOutcome> {
    let final_path = host_dir.join(name);
    std::fs::create_dir_all(staging_dir).with_context(|| format!("create staging dir {}", staging_dir.display()))?;
    let bytes = if on_same_volume(staging_dir, host_dir) {
        publish_from_local_staging(source, &staging_dir.join(name), &final_path)
    } else {
        write_stream_atomic(&final_path, source).map_err(Into::into)
    }
    .with_context(|| format!("copy {name} to {}", final_path.display()))?;
    verify_published(&final_path, bytes)?;
    Ok(CopyOutcome {
        copied_path: final_path,
        bytes,
    })
}

/// Write the whole copy to `staged`, then move it to `final_path` in one
/// rename. A failed attempt removes its staged file.
fn publish_from_local_staging(source: &mut impl Read, staged: &Path, final_path: &Path) -> Result<u64> {
    struct Staged<'a>(&'a Path);
    impl Drop for Staged<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0);
        }
    }
    let staged = Staged(staged);
    let bytes = write_stream_atomic(staged.0, source).with_context(|| format!("stage {}", staged.0.display()))?;
    std::fs::rename(staged.0, final_path)
        .with_context(|| format!("move {} into the destination", staged.0.display()))?;
    Ok(bytes)
}

/// Whether a rename from `staging_dir` into `host_dir` stays on one volume.
/// Unknown is treated as "different", which selects in-destination staging.
fn on_same_volume(staging_dir: &Path, host_dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let device = |dir: &Path| std::fs::metadata(dir).map(|meta| meta.dev()).ok();
        matches!((device(staging_dir), device(host_dir)), (Some(a), Some(b)) if a == b)
    }
    #[cfg(not(unix))]
    {
        let _ = (staging_dir, host_dir);
        false
    }
}

/// Confirm the destination holds the complete copy under its final name.
fn verify_published(final_path: &Path, bytes: u64) -> Result<()> {
    let meta =
        std::fs::metadata(final_path).with_context(|| format!("verify published copy {}", final_path.display()))?;
    if !meta.is_file() || meta.len() != bytes {
        bail!(
            "published copy {} is incomplete: expected a {bytes}-byte file, found {} bytes",
            final_path.display(),
            meta.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// A destination folder and a staging folder outside it.
    fn dirs(tmp: &TempDir) -> (PathBuf, PathBuf) {
        let host = tmp.path().join("dest").join("host");
        std::fs::create_dir_all(&host).unwrap();
        (host, tmp.path().join("staging"))
    }

    #[test]
    fn copies_content_and_leaves_no_staging_file() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("state.db.bak-20260101-120000");
        std::fs::write(&src, b"snapshot").unwrap();
        let (host, staging) = dirs(&tmp);
        let out = copy_to_offsite(&src, &host, &staging).unwrap();
        assert_eq!(out.copied_path, host.join("state.db.bak-20260101-120000"));
        assert_eq!(out.bytes, 8);
        assert_eq!(std::fs::read(&out.copied_path).unwrap(), b"snapshot");
        assert_eq!(names(&host), ["state.db.bak-20260101-120000"]);
        assert!(names(&staging).is_empty(), "staging emptied: {:?}", names(&staging));
    }

    /// Yields its bytes in two reads and, between them, records what each
    /// folder holds while the copy is half written.
    struct ObservingReader<'a> {
        chunks: Vec<&'static [u8]>,
        host: &'a Path,
        staging: &'a Path,
        seen_in_host: Vec<String>,
        seen_in_staging: Vec<String>,
    }

    impl Read for ObservingReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.chunks.is_empty() {
                return Ok(0);
            }
            self.seen_in_host.extend(names(self.host));
            self.seen_in_staging.extend(names(self.staging));
            let chunk = self.chunks.remove(0);
            buf[..chunk.len()].copy_from_slice(chunk);
            Ok(chunk.len())
        }
    }

    #[test]
    fn destination_sees_nothing_until_the_complete_file_appears() {
        let tmp = TempDir::new().unwrap();
        let (host, staging) = dirs(&tmp);
        let mut reader = ObservingReader {
            chunks: vec![b"first-", b"second"],
            host: &host,
            staging: &staging,
            seen_in_host: Vec::new(),
            seen_in_staging: Vec::new(),
        };
        let name = "state.db.bak-20260101-120000";
        let out = copy_stream_to_offsite(&mut reader, name, &host, &staging).unwrap();
        assert!(
            reader.seen_in_host.is_empty(),
            "nothing may appear in the destination mid-copy: {:?}",
            reader.seen_in_host
        );
        assert!(
            !reader.seen_in_staging.is_empty(),
            "the copy is staged outside the destination"
        );
        assert_eq!(out.bytes, 12);
        assert_eq!(std::fs::read(host.join(name)).unwrap(), b"first-second");
        assert_eq!(names(&host), [name]);
        assert!(names(&staging).is_empty());
    }

    #[test]
    fn unrelated_partial_in_destination_is_left_alone() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("state.db.bak-20260101-120000");
        std::fs::write(&src, b"good").unwrap();
        let (host, staging) = dirs(&tmp);
        std::fs::write(host.join(".state.db.bak-20260101-120000.partial"), b"half").unwrap();
        let out = copy_to_offsite(&src, &host, &staging).unwrap();
        assert_eq!(std::fs::read(out.copied_path).unwrap(), b"good");
        assert_eq!(
            std::fs::read(host.join(".state.db.bak-20260101-120000.partial")).unwrap(),
            b"half"
        );
    }

    #[test]
    fn leftover_staged_file_is_replaced_not_published() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("state.db.bak-20260101-120000");
        std::fs::write(&src, b"good").unwrap();
        let (host, staging) = dirs(&tmp);
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("state.db.bak-20260101-120000"), b"crashed-half").unwrap();
        let out = copy_to_offsite(&src, &host, &staging).unwrap();
        assert_eq!(std::fs::read(out.copied_path).unwrap(), b"good");
        assert!(names(&staging).is_empty());
    }

    #[test]
    fn failure_leaves_no_final_file_and_cleans_staging() {
        struct FailingReader(bool);
        impl Read for FailingReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if std::mem::replace(&mut self.0, true) {
                    return Err(std::io::Error::other("source interrupted"));
                }
                buf[..4].copy_from_slice(b"half");
                Ok(4)
            }
        }
        let tmp = TempDir::new().unwrap();
        let (host, staging) = dirs(&tmp);
        let name = "state.db.bak-20260101-120000";
        assert!(copy_stream_to_offsite(&mut FailingReader(false), name, &host, &staging).is_err());
        assert!(names(&host).is_empty());
        assert!(names(&staging).is_empty());

        let missing_src = tmp.path().join(name);
        assert!(copy_to_offsite(&missing_src, &host, &staging).is_err());
        assert!(names(&host).is_empty());
    }

    #[test]
    fn non_directory_destination_fails_and_cleans_staging() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("state.db.bak-20260101-120000");
        std::fs::write(&src, b"x").unwrap();
        let blocked = tmp.path().join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let staging = tmp.path().join("staging");
        assert!(copy_to_offsite(&src, &blocked, &staging).is_err());
        assert!(names(&staging).is_empty());
    }

    #[test]
    fn same_volume_is_detected_and_unknown_means_different() {
        let tmp = TempDir::new().unwrap();
        let (host, staging) = dirs(&tmp);
        std::fs::create_dir_all(&staging).unwrap();
        assert!(on_same_volume(&staging, &host));
        assert!(!on_same_volume(&staging, &tmp.path().join("unmounted")));
    }

    #[test]
    fn incomplete_published_copy_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let published = tmp.path().join("state.db.bak-20260101-120000");
        std::fs::write(&published, b"short").unwrap();
        assert!(verify_published(&published, 5).is_ok());
        let err = verify_published(&published, 9).unwrap_err();
        assert!(err.to_string().contains("incomplete"), "{err}");
        assert!(verify_published(&tmp.path().join("absent"), 5).is_err());
        assert!(verify_published(tmp.path(), 5).is_err());
    }

    #[test]
    fn refuses_live_database_files() {
        let tmp = TempDir::new().unwrap();
        for name in ["state.db", "state.db-wal", "state.db-shm"] {
            let src = tmp.path().join(name);
            std::fs::write(&src, b"live").unwrap();
            let err = copy_to_offsite(&src, tmp.path(), &tmp.path().join("staging")).unwrap_err();
            assert!(err.to_string().contains("refusing"), "{err}");
        }
    }
}
