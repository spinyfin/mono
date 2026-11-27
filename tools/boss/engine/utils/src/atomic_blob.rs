//! Atomic publication shared by content-addressed stores; retention belongs to callers.
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Publish complete bytes using an exclusively created, per-attempt sibling.
/// Concurrent publishers never share a staging file. Sync failures are fatal.
pub fn write_blob_atomic(destination: &Path, mut bytes: &[u8]) -> io::Result<()> {
    write_stream_atomic(destination, &mut bytes).map(|_| ())
}

/// Publish complete bytes atomically with an explicit Unix file mode applied at
/// staging-file creation time (for example `0o600` for private caches).
#[cfg(unix)]
pub fn write_blob_atomic_with_mode(destination: &Path, mut bytes: &[u8], mode: u32) -> io::Result<()> {
    write_stream_atomic_with_mode(destination, &mut bytes, mode).map(|_| ())
}

/// Stream into an exclusive sibling, fsync, and atomically publish it.
/// Returns the bytes copied; failed attempts clean up their staging file.
pub fn write_stream_atomic(destination: &Path, reader: &mut impl Read) -> io::Result<u64> {
    write_stream_atomic_inner(destination, reader, None)
}

/// Stream into an exclusive sibling created with Unix `mode`, fsync, and
/// atomically publish it.
#[cfg(unix)]
pub fn write_stream_atomic_with_mode(destination: &Path, reader: &mut impl Read, mode: u32) -> io::Result<u64> {
    write_stream_atomic_inner(destination, reader, Some(mode))
}

fn write_stream_atomic_inner(destination: &Path, reader: &mut impl Read, mode: Option<u32>) -> io::Result<u64> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let (staging, mut file) = loop {
        let mut name = destination.as_os_str().to_owned();
        name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let path = PathBuf::from(name);
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(file_mode) = mode {
            opts.mode(file_mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        match opts.open(&path) {
            Ok(file) => break (Staging(path), file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let bytes = io::copy(reader, &mut file)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&staging.0, destination)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_failure_preserves_published_file_and_removes_staging() {
        struct FailingReader;
        impl Read for FailingReader {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("source interrupted"))
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("blob");
        write_blob_atomic(&destination, b"original").unwrap();
        assert!(write_stream_atomic(&destination, &mut FailingReader).is_err());
        assert_eq!(fs::read(destination).unwrap(), b"original");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn write_blob_atomic_with_mode_sets_permissions_at_creation() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("private.json");
        write_blob_atomic_with_mode(&destination, b"secret", 0o600).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"secret");
        let mode = fs::metadata(&destination).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
