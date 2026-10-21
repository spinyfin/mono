use std::fs::{self, File, OpenOptions};
use std::path::Path;

use fs4::fs_std::FileExt;

use crate::app::CubeError;

pub struct RepoLock {
    _file: File,
}

impl RepoLock {
    pub fn acquire(lock_path: &Path) -> Result<Self, CubeError> {
        Self::open_and_lock(lock_path, true)?.ok_or_else(|| unreachable!("a blocking lock always acquires"))
    }

    /// Take the lock if nobody holds it; `None` when it is held elsewhere.
    pub fn try_acquire(lock_path: &Path) -> Result<Option<Self>, CubeError> {
        Self::open_and_lock(lock_path, false)
    }

    fn open_and_lock(lock_path: &Path, blocking: bool) -> Result<Option<Self>, CubeError> {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|e| CubeError::LockIo {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|e| CubeError::LockIo {
                path: lock_path.to_path_buf(),
                source: e,
            })?;
        let io_error = |source| CubeError::LockIo {
            path: lock_path.to_path_buf(),
            source,
        };
        if blocking {
            FileExt::lock_exclusive(&file).map_err(io_error)?;
        } else if !FileExt::try_lock_exclusive(&file).map_err(io_error)? {
            return Ok(None);
        }
        Ok(Some(Self { _file: file }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;

    #[test]
    fn acquire_creates_parent_dir() {
        let dir = tempdir().unwrap();
        let lock_path = dir.path().join("nested/locks/repo.lock");
        let _lock = RepoLock::acquire(&lock_path).unwrap();
        assert!(lock_path.parent().unwrap().is_dir());
    }

    #[test]
    fn try_acquire_reports_a_held_lock_instead_of_waiting() {
        let dir = tempdir().unwrap();
        let lock_path = dir.path().join("repo.lock");
        let held = RepoLock::try_acquire(&lock_path).unwrap().expect("free lock");
        assert!(RepoLock::try_acquire(&lock_path).unwrap().is_none());
        drop(held);
        assert!(RepoLock::try_acquire(&lock_path).unwrap().is_some());
    }

    #[test]
    fn drop_releases_so_reacquire_succeeds() {
        let dir = tempdir().unwrap();
        let lock_path = dir.path().join("repo.lock");
        let lock = RepoLock::acquire(&lock_path).unwrap();
        drop(lock);
        let _again = RepoLock::acquire(&lock_path).unwrap();
    }

    #[test]
    fn second_acquire_blocks_until_first_drops() {
        let dir = tempdir().unwrap();
        let lock_path = dir.path().join("contention.lock");
        let holder_path = lock_path.clone();
        let waiter_path = lock_path.clone();

        let (acquired_tx, acquired_rx) = mpsc::channel();
        let hold_for = Duration::from_millis(150);

        let holder = thread::spawn(move || {
            let lock = RepoLock::acquire(&holder_path).unwrap();
            acquired_tx.send(()).unwrap();
            thread::sleep(hold_for);
            drop(lock);
        });

        acquired_rx.recv().unwrap();
        let waited_from = Instant::now();
        let _waiter = RepoLock::acquire(&waiter_path).unwrap();
        let elapsed = waited_from.elapsed();

        holder.join().unwrap();

        assert!(
            elapsed >= Duration::from_millis(50),
            "second acquire returned in {elapsed:?}, expected to block until holder dropped"
        );
    }
}
