//! Private per-run driver-home root. Unset or a shared temp is refused.

use anyhow::{Result, bail, ensure};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Same key the driver uses for per-run Codex/Claude input homes.
pub const ROOT_ENV: &str = boss_engine_driver::codex::CODEX_HOMES_ROOT_ENV;

/// Refuse an unset, missing, world-readable, unwritable, or symlinked root.
pub fn preflight(root: Option<&Path>) -> Result<PathBuf> {
    let Some(root) = root.filter(|path| !path.as_os_str().is_empty()) else {
        bail!("private-home root is unset");
    };
    let metadata = fs::symlink_metadata(root).map_err(|_| anyhow::anyhow!("private-home root is missing"))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "private-home root must not be a symlink"
    );
    ensure!(metadata.is_dir(), "private-home root must be a directory");
    let mode = metadata.permissions().mode();
    ensure!(mode & 0o077 == 0, "private-home root must be owner-only (0700)");
    ensure!(mode & 0o200 != 0, "private-home root must be writable by the owner");
    let probe = root.join(".spoke-home-probe");
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&probe)
        .map_err(|_| anyhow::anyhow!("private-home root is not writable"))?;
    let _ = fs::remove_file(&probe);
    Ok(root.to_path_buf())
}

/// Create `path` as a 0700 directory if needed, then [`preflight`] it.
pub fn pin(path: &Path) -> Result<PathBuf> {
    if fs::symlink_metadata(path).is_err() {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    }
    preflight(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn unset_or_empty_root_is_rejected() {
        assert!(preflight(None).unwrap_err().to_string().contains("unset"));
        assert!(
            preflight(Some(Path::new("")))
                .unwrap_err()
                .to_string()
                .contains("unset")
        );
    }

    #[test]
    fn world_writable_or_unwritable_root_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("homes");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(preflight(Some(&root)).unwrap_err().to_string().contains("owner-only"));
        fs::set_permissions(&root, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(preflight(Some(&root)).unwrap_err().to_string().contains("writable"));
    }

    #[test]
    fn symlink_root_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        fs::DirBuilder::new().mode(0o700).create(&real).unwrap();
        let link = temp.path().join("homes");
        symlink(&real, &link).unwrap();
        assert!(preflight(Some(&link)).unwrap_err().to_string().contains("symlink"));
    }

    #[test]
    fn pin_creates_a_private_writable_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = pin(&temp.path().join("homes")).unwrap();
        assert_eq!(fs::symlink_metadata(&root).unwrap().permissions().mode() & 0o777, 0o700);
        preflight(Some(&root)).unwrap();
    }
}
