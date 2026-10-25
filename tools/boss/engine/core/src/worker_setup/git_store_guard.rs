//! Materialisation of the shared-source-store write gate
//! (`boss-git-store-guard.py`) next to the worker settings file.
//!
//! The script itself lives in the driver crate
//! ([`boss_engine_driver::git_store_guard`]) because all three drivers run
//! the same bytes; this module only owns how the engine lays it on disk —
//! content-addressed and write-once, exactly like the path guard and the
//! checkleft push guard — so a Codex worker that attested one build's bytes
//! can never be bricked by another build writing different bytes to the same
//! path. Split out of `worker_setup` (which sits at the repo's file-size
//! limit) to keep the module boundary reviewable.

use std::io;
use std::path::{Path, PathBuf};

pub use boss_engine_driver::git_store_guard::{GIT_STORE_GUARD_SCRIPT, GIT_STORE_GUARD_SCRIPT_NAME};

/// Directory-name prefix for a content-addressed materialisation
/// (`git-store-guard-<sha256>/boss-git-store-guard.py`). Distinct from the
/// other guards' prefixes so each script can change independently.
pub(crate) const GIT_STORE_GUARD_KIND: &str = "git-store-guard";

/// Absolute path to this engine build's shared-source-store gate script,
/// under [`super::worker_settings_dir`]. Content-addressed on the script
/// bytes like [`super::path_guard_script_path`].
pub fn git_store_guard_script_path() -> PathBuf {
    git_store_guard_script_path_in(&super::worker_settings_dir())
}

/// [`git_store_guard_script_path`] resolved under `dir` instead of the
/// shared worker-settings dir.
pub(super) fn git_store_guard_script_path_in(dir: &Path) -> PathBuf {
    super::content_addressed_guard_path(
        dir,
        GIT_STORE_GUARD_KIND,
        GIT_STORE_GUARD_SCRIPT_NAME,
        GIT_STORE_GUARD_SCRIPT.as_bytes(),
    )
}

/// Write the gate script into a content-addressed directory under `dir`.
/// Same write-once contract as [`super::ensure_path_guard_script_in`].
pub fn ensure_git_store_guard_script_in(dir: &Path) -> io::Result<PathBuf> {
    super::ensure_content_addressed_script(
        dir,
        GIT_STORE_GUARD_KIND,
        GIT_STORE_GUARD_SCRIPT_NAME,
        GIT_STORE_GUARD_SCRIPT.as_bytes(),
    )
}
