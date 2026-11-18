//! Filesystem helpers for tests that build large real-repository fixtures.
//!
//! Copying a cached jj store or populating an operation store with tens of
//! thousands of files is bound by per-file syscall latency, not bandwidth, so
//! the work is spread over a few threads.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Threads used for fixture I/O. Enough to overlap per-file latency without
/// starving the other tests sharing the binary's thread pool.
const WORKERS: usize = 8;

/// Run `work` over every item on [`WORKERS`] threads and surface the first
/// error. Items are split into contiguous chunks so the hot loop takes no lock.
pub(crate) fn for_each_parallel<T: Sync>(items: &[T], work: impl Fn(&T) -> io::Result<()> + Sync) -> io::Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let chunk = items.len().div_ceil(WORKERS);
    std::thread::scope(|scope| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|chunk| scope.spawn(|| chunk.iter().try_for_each(&work)))
            .collect();
        handles
            .into_iter()
            .try_for_each(|handle| handle.join().expect("fixture worker panicked"))
    })
}

/// Copy the directory tree at `source` into `destination`, creating it.
///
/// Every file is rewritten rather than cloned so the copy is writable even
/// when the source is a read-only Bazel output. Only directories and regular
/// files are allowed: a jj store contains nothing else. A symlink to a regular
/// file counts as that file, and its target's contents are copied: Bazel
/// expands a tree artifact in a test's runfiles into one such link per file on
/// Linux. Any other entry, including a link to a directory, is rejected.
pub(crate) fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    let mut directories = Vec::new();
    let mut files = Vec::new();
    collect(source, Path::new(""), &mut directories, &mut files)?;
    fs::create_dir_all(destination)?;
    for directory in &directories {
        fs::create_dir_all(destination.join(directory))?;
    }
    for_each_parallel(&files, |relative| {
        fs::write(destination.join(relative), fs::read(source.join(relative))?)
    })
}

fn collect(root: &Path, relative: &Path, directories: &mut Vec<PathBuf>, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(root.join(relative))? {
        let entry = entry?;
        let path = relative.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            directories.push(path.clone());
            collect(root, &path, directories, files)?;
        } else {
            let is_file =
                kind.is_file() || (kind.is_symlink() && fs::metadata(root.join(&path)).is_ok_and(|m| m.is_file()));
            assert!(
                is_file,
                "fixture must contain only directories and files: {}",
                path.display()
            );
            files.push(path);
        }
    }
    Ok(())
}

/// Create `count` empty files of `size` logical bytes named by their index in
/// `directory`. The files are sparse: they keep a real inode, directory entry
/// and size without the write bandwidth of filling them.
pub(crate) fn create_sparse_files(directory: &Path, count: u64, size: u64) -> io::Result<()> {
    let indices: Vec<u64> = (1..=count).collect();
    for_each_parallel(&indices, |n| {
        fs::File::create(directory.join(format!("{n:0128x}")))?.set_len(size)
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::copy_tree;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn copies_a_symlinked_file_as_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let (store, source, target) = (dir.path().join("store"), dir.path().join("src"), dir.path().join("dst"));
        fs::create_dir_all(source.join("sub")).unwrap();
        fs::create_dir(&store).unwrap();
        fs::write(store.join("config"), "x\n").unwrap();
        symlink(store.join("config"), source.join("sub/config")).unwrap();
        copy_tree(&source, &target).unwrap();
        let copied = target.join("sub/config");
        assert!(!fs::symlink_metadata(&copied).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(copied).unwrap(), "x\n");
    }

    #[test]
    #[should_panic(expected = "fixture must contain only directories and files")]
    fn rejects_a_symlink_to_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (source, target) = (dir.path().join("src"), dir.path().join("dst"));
        fs::create_dir_all(dir.path().join("other")).unwrap();
        fs::create_dir(&source).unwrap();
        symlink(dir.path().join("other"), source.join("link")).unwrap();
        let _ = copy_tree(&source, &target);
    }

    #[test]
    #[should_panic(expected = "fixture must contain only directories and files")]
    fn rejects_a_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let (source, target) = (dir.path().join("src"), dir.path().join("dst"));
        fs::create_dir(&source).unwrap();
        symlink(dir.path().join("missing"), source.join("link")).unwrap();
        let _ = copy_tree(&source, &target);
    }
}
