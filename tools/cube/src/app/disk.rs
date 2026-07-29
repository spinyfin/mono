//! Free space on the volume the workspace pool lives on.
//!
//! Until this module existed, cube had no idea how much disk it was using or
//! how much was left: the lease path minted workspaces on a volume at 100%
//! utilisation exactly as happily as on an empty one, which is how a pool grew
//! to 520 workspaces / ~191 GB and took a 1.8 TiB volume to exhaustion.
//!
//! What this module is NOT is a cap on the pool. cube grows the pool on demand
//! and a lease for a reachable repo always succeeds — that is a deliberate
//! design decision and it stands. Free space is an input to *reclamation*: a
//! lease that finds the volume below its floor compacts before it does
//! anything else (see [`crate::app::reclaim`]), and only a mint that still
//! cannot clear the floor afterwards fails. The number that gates a call here
//! is bytes on the volume, never how many workspaces exist.

use std::path::Path;

use crate::app::errors::{CubeError, Result};

/// A point-in-time reading of the volume containing some path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(super) struct DiskSpace {
    /// Bytes available to this (non-privileged) user. This is the number that
    /// matters — `free_space` includes root-reserved blocks cube can't use.
    pub(super) available_bytes: u64,
    /// Total size of the volume.
    pub(super) total_bytes: u64,
}

impl DiskSpace {
    /// Read the volume containing `path`.
    ///
    /// `path` need not be the mount point; any path on the volume works, which
    /// is why callers pass a repo's `workspace_root` directly.
    pub(super) fn probe(path: &Path) -> Result<Self> {
        let stats = fs4::statvfs(path).map_err(CubeError::Io)?;
        Ok(Self {
            available_bytes: stats.available_space(),
            total_bytes: stats.total_space(),
        })
    }

    /// Whether available space has fallen below `floor_bytes`.
    pub(super) fn is_below(&self, floor_bytes: u64) -> bool {
        self.available_bytes < floor_bytes
    }

    /// How many bytes short of `floor_bytes` this reading is; 0 when at or
    /// above the floor.
    pub(super) fn shortfall_below(&self, floor_bytes: u64) -> u64 {
        floor_bytes.saturating_sub(self.available_bytes)
    }
}

/// Render a byte count the way an operator reads one (`20.9 GiB`).
///
/// Audit fields carry raw byte counts; this is only for the human-facing
/// error and warning text, where "not enough disk: 3221225472 available" is a
/// worse message than "not enough disk: 3.0 GiB available".
pub(super) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_the_volume_containing_a_real_path() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let space = DiskSpace::probe(tempdir.path()).expect("probe");
        // Nothing about the host is assertable beyond internal consistency:
        // a mounted volume has a non-zero size and can't have more available
        // than it has in total.
        assert!(space.total_bytes > 0, "a mounted volume should report a total size");
        assert!(space.available_bytes <= space.total_bytes);
    }

    #[test]
    fn probe_fails_loudly_for_a_path_that_does_not_exist() {
        let err = DiskSpace::probe(Path::new("/definitely/not/a/real/mount/point-cube")).unwrap_err();
        assert!(matches!(err, CubeError::Io(_)), "expected an I/O error, got {err:?}");
    }

    #[test]
    fn below_and_shortfall_agree_about_the_floor() {
        let space = DiskSpace {
            available_bytes: 100,
            total_bytes: 1_000,
        };
        assert!(space.is_below(150));
        assert_eq!(space.shortfall_below(150), 50);
        // At the floor exactly is not below it.
        assert!(!space.is_below(100));
        assert_eq!(space.shortfall_below(100), 0);
        assert!(!space.is_below(50));
        assert_eq!(space.shortfall_below(50), 0);
    }

    #[test]
    fn human_bytes_reads_like_an_operator_expects() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(20 * 1024 * 1024 * 1024), "20.0 GiB");
        // The largest single target/ dir found in the incident.
        assert_eq!(human_bytes(22_473_368_863), "20.9 GiB");
    }
}
