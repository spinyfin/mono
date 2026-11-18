//! Publish a finished backup into the per-host destination folder as one
//! complete file.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use boss_engine_utils::atomic_blob::write_stream_atomic;

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
/// See [`copy_open_to_offsite`] for how the copy is staged and published.
pub fn copy_to_offsite(src: &Path, host_dir: &Path, staging_dir: &Path) -> Result<CopyOutcome> {
    let name = backup_file_name(src)?;
    let source = File::open(src).with_context(|| format!("open {}", src.display()))?;
    copy_open_to_offsite(source, name, host_dir, staging_dir)
}

/// Validate that `src` names a finished backup and return its file name.
pub fn backup_file_name(src: &Path) -> Result<&str> {
    let name = src
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("backup path has no UTF-8 file name: {}", src.display()))?;
    if !name.starts_with(BACKUP_FILE_PREFIX) {
        // Guard the invariant that only finished backups are copied — never
        // the live state.db / -wal / -shm.
        bail!("refusing to copy {name}: not a finished `{BACKUP_FILE_PREFIX}*` backup");
    }
    Ok(name)
}

/// Like [`copy_to_offsite`], streaming from an already-open `source` so the
/// copy survives the local path being pruned in the meantime.
///
/// The destination is watched by a sync agent, which uploads whatever it
/// finds there, including a file that is still being written. So the copy is
/// written and fsynced in `staging_dir`, a directory outside the destination
/// that the caller owns, and only then renamed into `host_dir`. On the same
/// volume that rename is atomic: the agent sees one complete new file that is
/// never modified afterwards.
///
/// When `staging_dir` is on a different volume, a second copy is staged
/// outside the configured destination on its volume and renamed into place.
///
/// Either way the published file is checked against the copied byte count
/// before the copy is reported as done.
pub fn copy_open_to_offsite(mut source: File, name: &str, host_dir: &Path, staging_dir: &Path) -> Result<CopyOutcome> {
    copy_stream_to_offsite(&mut source, name, host_dir, staging_dir)
}

fn copy_stream_to_offsite(
    source: &mut impl Read,
    name: &str,
    host_dir: &Path,
    staging_dir: &Path,
) -> Result<CopyOutcome> {
    let same_volume = on_same_volume(staging_dir, host_dir);
    copy_via_route(
        source,
        name,
        host_dir,
        staging_dir,
        same_volume,
        &relay_candidates(host_dir),
    )
}

/// Directories outside the synced tree that may sit on the destination's
/// volume, tried in order when the local staging directory is not.
pub fn relay_candidates(host_dir: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![std::env::temp_dir().join("boss-offsite-relay")];
    // The configured destination is the host directory's parent. Its sibling
    // can live on an external disk even when the system temp directory cannot.
    if let Some(parent) = host_dir.parent().and_then(Path::parent) {
        candidates.push(parent.join(".boss-offsite-relay"));
    }
    // Also protect the orphan sweep from a relay symlink into the destination.
    if let Some(destination) = host_dir.parent().and_then(|dir| dir.canonicalize().ok()) {
        candidates.retain(|dir| !dir.canonicalize().is_ok_and(|dir| dir.starts_with(&destination)));
    }
    candidates
}

/// The copy itself, with the volume decision made by the caller.
///
/// The backup is always written and fsynced to `staging_dir` first. The
/// publish step then either renames it into place (same volume) or relays it
/// across volumes; see [`publish_across_volumes`].
fn copy_via_route(
    source: &mut impl Read,
    name: &str,
    host_dir: &Path,
    staging_dir: &Path,
    same_volume: bool,
    relays: &[PathBuf],
) -> Result<CopyOutcome> {
    let final_path = host_dir.join(name);
    std::fs::create_dir_all(staging_dir).with_context(|| format!("create staging dir {}", staging_dir.display()))?;
    let bytes = publish_from_local_staging(source, &staging_dir.join(name), &final_path, same_volume, relays)
        .with_context(|| format!("copy {name} to {}", final_path.display()))?;
    verify_published(&final_path, bytes)?;
    Ok(CopyOutcome {
        copied_path: final_path,
        bytes,
    })
}

/// Removes its file when dropped, so a failed attempt leaves nothing behind.
struct Cleanup<'a>(&'a Path);
impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

/// Write the whole copy to `staged`, then publish it at `final_path`.
fn publish_from_local_staging(
    source: &mut impl Read,
    staged: &Path,
    final_path: &Path,
    same_volume: bool,
    relays: &[PathBuf],
) -> Result<u64> {
    publish_with_ops(
        source,
        staged,
        final_path,
        same_volume,
        relays,
        &mut |from, to| std::fs::rename(from, to),
        &mut |from, to| write_stream_atomic(to, &mut File::open(from)?),
    )
}

fn publish_with_ops(
    source: &mut impl Read,
    staged: &Path,
    final_path: &Path,
    same_volume: bool,
    relays: &[PathBuf],
    rename: &mut impl FnMut(&Path, &Path) -> std::io::Result<()>,
    relay_copy: &mut impl FnMut(&Path, &Path) -> std::io::Result<u64>,
) -> Result<u64> {
    let _cleanup = Cleanup(staged);
    let bytes = write_stream_atomic(staged, source).with_context(|| format!("stage {}", staged.display()))?;
    if same_volume {
        match rename(staged, final_path) {
            Ok(()) => return Ok(bytes),
            Err(err) if is_cross_device(&err) => {
                tracing::warn!(
                    staged = %staged.display(),
                    destination = %final_path.display(),
                    "offsite backup: rename crossed devices despite matching volume ids; relaying instead"
                );
            }
            Err(err) => {
                return Err(err).with_context(|| format!("move {} into the destination", staged.display()));
            }
        }
    }
    publish_across_volumes(staged, final_path, relays, rename, relay_copy)?;
    Ok(bytes)
}

fn is_cross_device(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::CrossesDevices || err.raw_os_error() == Some(18)
}

/// Publish the already-staged `staged` into a different volume.
///
/// Copy to a relay outside the configured destination on its volume, then
/// rename it in. Device IDs are only a hint: EXDEV rejects a candidate even
/// when they match. Never stream into the destination itself.
fn publish_across_volumes(
    staged: &Path,
    final_path: &Path,
    relays: &[PathBuf],
    rename: &mut impl FnMut(&Path, &Path) -> std::io::Result<()>,
    relay_copy: &mut impl FnMut(&Path, &Path) -> std::io::Result<u64>,
) -> Result<()> {
    let host_dir = final_path.parent().context("destination has no parent directory")?;
    let name = final_path.file_name().context("destination has no file name")?;
    for relay in relays {
        if std::fs::create_dir_all(relay).is_err() || !on_same_volume(relay, host_dir) {
            tracing::warn!(staging_dir = %relay.display(), destination = %host_dir.display(),
                "offsite backup: relay unavailable or on another volume; trying next staging directory");
            continue;
        }
        let destination = host_dir
            .parent()
            .context("host directory has no destination root")?
            .canonicalize()?;
        if relay.canonicalize()?.starts_with(&destination) {
            tracing::warn!(staging_dir = %relay.display(), destination = %destination.display(),
                "offsite backup: relay is inside the destination; trying next staging directory");
            continue;
        }
        // Unique per attempt: concurrent copies of the same backup name must
        // not delete or rename each other's relayed file.
        static RELAY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = RELAY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let relayed = relay.join(format!("{}.{}-{seq}.relay", name.to_string_lossy(), std::process::id()));
        let _cleanup = Cleanup(&relayed);
        relay_copy(staged, &relayed).with_context(|| format!("relay {}", relayed.display()))?;
        match rename(&relayed, final_path) {
            Ok(()) => {
                tracing::warn!(staging_dir = %relay.display(), destination = %host_dir.display(),
                    "offsite backup: published through outside staging on the destination volume");
                return Ok(());
            }
            Err(err) if is_cross_device(&err) => {
                tracing::warn!(staging_dir = %relay.display(), destination = %host_dir.display(),
                    "offsite backup: relay rename crossed devices; trying next staging directory");
            }
            Err(err) => return Err(err).with_context(|| format!("move {} into the destination", relayed.display())),
        }
    }
    tracing::warn!(
        staging_dirs = ?relays,
        destination = %host_dir.display(),
        "offsite backup: no outside staging directory can publish into the destination"
    );
    bail!(
        "no outside staging directory can publish into {}; choose a destination with writable outside staging that accepts a rename",
        host_dir.display()
    )
}

/// Whether a rename from `staging_dir` into `host_dir` stays on one volume.
/// Unknown is treated as "different", which selects outside relay staging.
fn on_same_volume(staging_dir: &Path, host_dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let device = |dir: &Path| std::fs::metadata(dir).map(|meta| meta.dev()).ok();
        matches!((device(staging_dir), device(host_dir)), (Some(a), Some(b)) if a == b)
    }
    #[cfg(not(unix))]
    {
        let _ = (staging_dir, host_dir);
        false
    }
}

/// Confirm the destination holds the complete copy under its final name.
fn verify_published(final_path: &Path, bytes: u64) -> Result<()> {
    let meta =
        std::fs::metadata(final_path).with_context(|| format!("verify published copy {}", final_path.display()))?;
    if !meta.is_file() || meta.len() != bytes {
        bail!(
            "published copy {} is incomplete: expected a {bytes}-byte file, found {} bytes",
            final_path.display(),
            meta.len()
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "copy_tests.rs"]
mod tests;
