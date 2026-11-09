//! Engine-owned scratch working directory for a `progression` run.
//!
//! A progression agent holds no code checkout and no cube lease, yet the
//! spawn machinery writes the agent-rules file, the settings pointer and the
//! initial-input script under the run's working directory. That directory is
//! this per-execution scratch directory: it lives under the system temp dir,
//! outside every repository, and is removed when the run is torn down.
//!
//! The path is derived from the execution id alone, so teardown never trusts a
//! path read back from the database to decide what to delete.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

/// Directory (under the system temp dir) holding every progression scratch
/// directory.
const ROOT_DIR_NAME: &str = "boss-progression-scratch";

/// The directory that holds every progression scratch directory.
pub fn scratch_root() -> PathBuf {
    std::env::temp_dir().join(ROOT_DIR_NAME)
}

fn checked_id(execution_id: &str) -> Result<&str> {
    ensure!(
        !execution_id.is_empty()
            && execution_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "execution id {execution_id:?} is not a safe scratch directory name"
    );
    Ok(execution_id)
}

/// The scratch directory for `execution_id`. Errors for an id that is not a
/// plain file name, so a malformed id can never escape [`scratch_root`].
pub fn scratch_dir_for(execution_id: &str) -> Result<PathBuf> {
    Ok(scratch_root().join(checked_id(execution_id)?))
}

/// Create (or reuse) the scratch directory for `execution_id` and return it.
pub fn create_scratch_dir(execution_id: &str) -> Result<PathBuf> {
    let dir = scratch_dir_for(execution_id)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating progression scratch dir {}", dir.display()))?;
    Ok(dir)
}

/// Remove the scratch directory for `execution_id`. Idempotent: an absent
/// directory is success. Returns whether a directory was removed.
pub fn remove_scratch_dir(execution_id: &str) -> Result<bool> {
    let dir = scratch_dir_for(execution_id)?;
    remove_dir_if_present(&dir)
}

fn remove_dir_if_present(dir: &Path) -> Result<bool> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("removing progression scratch dir {}", dir.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_dir_is_per_execution_and_outside_any_repo() {
        let a = scratch_dir_for("exec_abc_1").unwrap();
        let b = scratch_dir_for("exec_abc_2").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.parent().unwrap(), scratch_root());
        assert!(a.starts_with(std::env::temp_dir()));
    }

    #[test]
    fn unsafe_ids_are_rejected() {
        for id in ["", "..", "a/b", "../x", "a b"] {
            assert!(scratch_dir_for(id).is_err(), "{id:?}");
        }
    }

    #[test]
    fn create_then_remove_is_idempotent() {
        let id = format!("exec_scratch_test_{}", std::process::id());
        let dir = create_scratch_dir(&id).unwrap();
        assert!(dir.is_dir());
        std::fs::write(dir.join("file"), b"x").unwrap();
        assert!(remove_scratch_dir(&id).unwrap());
        assert!(!dir.exists());
        assert!(!remove_scratch_dir(&id).unwrap());
    }
}
