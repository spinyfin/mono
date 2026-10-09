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
/// Writes to a `.<name>.partial` temp file in `host_dir`, fsyncs, then
/// renames, so a sync agent never sees a half-written backup under the final
/// name. A leftover `.partial` from a crashed earlier attempt is overwritten.
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
    let tmp_path = host_dir.join(format!(".{name}.partial"));

    let result = (|| -> Result<u64> {
        let bytes = std::fs::copy(src, &tmp_path)
            .with_context(|| format!("copy {} to {}", src.display(), tmp_path.display()))?;
        File::open(&tmp_path)
            .and_then(|f| f.sync_all())
            .with_context(|| format!("fsync {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &final_path)
            .with_context(|| format!("rename {} to {}", tmp_path.display(), final_path.display()))?;
        Ok(bytes)
    })();
    match result {
        Ok(bytes) => Ok(CopyOutcome {
            copied_path: final_path,
            bytes,
        }),
        Err(err) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(err)
        }
    }
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
    fn stale_partial_is_overwritten_and_final_name_never_partial() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("state.db.bak-20260101-120000");
        std::fs::write(&src, b"good").unwrap();
        let host = tmp.path().join("host");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join(".state.db.bak-20260101-120000.partial"), b"half").unwrap();
        let out = copy_to_offsite(&src, &host).unwrap();
        assert_eq!(std::fs::read(out.copied_path).unwrap(), b"good");
        assert!(!host.join(".state.db.bak-20260101-120000.partial").exists());
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
    fn missing_destination_dir_fails() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("state.db.bak-20260101-120000");
        std::fs::write(&src, b"x").unwrap();
        assert!(copy_to_offsite(&src, &tmp.path().join("gone")).is_err());
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
