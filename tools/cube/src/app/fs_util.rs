//! Filesystem helpers shared by workspace purge and bazel output-base cleanup.

use std::fs;
use std::io;
use std::path::Path;

/// Recursively restore the owner read/write/search bits on every directory
/// under (and including) `root`, without following symlinks. Bazel marks
/// output-tree directories read-only; `fs::remove_dir_all` cannot unlink
/// entries inside a directory it cannot write to, and fails part-way through
/// with `Permission denied`. Only the owner bits are added; group/other bits
/// are preserved.
pub(super) fn make_dirs_writable(root: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(root)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(meta.permissions().mode() | 0o700))?;
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            make_dirs_writable(&entry.path())?;
        }
    }
    Ok(())
}

/// Remove a directory tree that may contain read-only directories, never
/// following symlinks. A path that is already gone is not an error.
pub(super) fn remove_tree_writable(path: &Path) -> io::Result<()> {
    let result = make_dirs_writable(path).and_then(|()| fs::remove_dir_all(path));
    match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound && !path.exists() => Ok(()),
        other => other,
    }
}
