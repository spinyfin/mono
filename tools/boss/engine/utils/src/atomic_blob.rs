//! Atomic publication shared by content-addressed stores; retention belongs to callers.
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
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

/// Stream into an exclusive sibling, fsync, and atomically publish it.
/// Returns the bytes copied; failed attempts clean up their staging file.
pub fn write_stream_atomic(destination: &Path, reader: &mut impl Read) -> io::Result<u64> {
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
        match OpenOptions::new().write(true).create_new(true).open(&path) {
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
}
