//! Atomic copy of a finished backup into the per-host destination folder.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

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
/// Streams through the shared atomic publisher's exclusive staging sibling.
/// Retention removes crash-orphaned staging files after 24 hours.
pub fn copy_to_offsite(src: &Path, host_dir: &Path) -> Result<CopyOutcome> {
    let name = src
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("backup path has no UTF-8 file name: {}", src.display()))?;
    if !name.starts_with(BACKUP_FILE_PREFIX) {
        // Guard the invariant that only finished backups are copied — never
        // the live state.db / -wal / -shm.
        bail!("refusing to copy {name}: not a finished `{BACKUP_FILE_PREFIX}*` backup");
    }
    let final_path = host_dir.join(name);
    let mut source = File::open(src).with_context(|| format!("open {}", src.display()))?;
    let bytes = boss_engine_utils::atomic_blob::write_stream_atomic(&final_path, &mut source)
        .with_context(|| format!("copy {} to {}", src.display(), final_path.display()))?;
    Ok(CopyOutcome {
        copied_path: final_path,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn copies_content_and_leaves_no_partial() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("state.db.bak-20260101-120000");
        std::fs::write(&src, b"snapshot").unwrap();
        let host = tmp.path().join("dest").join("host");
        std::fs::create_dir_all(&host).unwrap();
        let out = copy_to_offsite(&src, &host).unwrap();
        assert_eq!(out.copied_path, host.join("state.db.bak-20260101-120000"));
        assert_eq!(out.bytes, 8);
        assert_eq!(std::fs::read(&out.copied_path).unwrap(), b"snapshot");
        let names: Vec<_> = std::fs::read_dir(&host)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "no .partial left behind: {names:?}");
    }

    #[test]
    fn existing_partial_is_not_used_as_staging() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("state.db.bak-20260101-120000");
        std::fs::write(&src, b"good").unwrap();
        let host = tmp.path().join("host");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join(".state.db.bak-20260101-120000.partial"), b"half").unwrap();
        let out = copy_to_offsite(&src, &host).unwrap();
        assert_eq!(std::fs::read(out.copied_path).unwrap(), b"good");
        assert_eq!(
            std::fs::read(host.join(".state.db.bak-20260101-120000.partial")).unwrap(),
            b"half"
        );
    }

    #[test]
    fn failure_leaves_no_final_file_and_cleans_temp() {
        let tmp = TempDir::new().unwrap();
        let missing_src = tmp.path().join("state.db.bak-20260101-120000");
        let host = tmp.path().join("host");
        std::fs::create_dir_all(&host).unwrap();
        assert!(copy_to_offsite(&missing_src, &host).is_err());
        assert_eq!(std::fs::read_dir(&host).unwrap().count(), 0);
    }

    #[test]
    fn non_directory_destination_fails() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("state.db.bak-20260101-120000");
        std::fs::write(&src, b"x").unwrap();
        let blocked = tmp.path().join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();
        assert!(copy_to_offsite(&src, &blocked).is_err());
    }

    #[test]
    fn refuses_live_database_files() {
        let tmp = TempDir::new().unwrap();
        for name in ["state.db", "state.db-wal", "state.db-shm"] {
            let src = tmp.path().join(name);
            std::fs::write(&src, b"live").unwrap();
            let err = copy_to_offsite(&src, tmp.path()).unwrap_err();
            assert!(err.to_string().contains("refusing"), "{err}");
        }
    }
}
