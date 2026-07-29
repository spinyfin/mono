//! Free space on the volume the workspace pool lives on.
//!
//! Reads the volume behind the workspace pool so the lease path can treat free
//! space as an input rather than an assumption.
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
    ///
    /// Under `cfg(test)` this answers the injected reading described in
    /// [`self::testing`] instead of calling `statvfs`, so no test's outcome
    /// depends on how full the machine running it happens to be.
    pub(super) fn probe(path: &Path) -> Result<Self> {
        #[cfg(test)]
        if let Some(injected) = testing::injected_reading() {
            return Ok(injected);
        }
        Self::probe_volume(path)
    }

    /// The real `statvfs` read, unconditionally. Production `probe` is this;
    /// tests reach it only through [`testing::use_real_volume`].
    fn probe_volume(path: &Path) -> Result<Self> {
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

/// The seam that keeps free space out of every test that is not *about* free
/// space.
///
/// [`DiskSpace::probe`] statvfs's whatever volume the caller's path sits on,
/// which for a test is the volume its tempdir landed on — the machine's. That
/// makes any behaviour keyed off the floor (notably the mint refusal in
/// `assert_mint_headroom`) a function of how full the developer's or CI
/// agent's disk is, and a lease test that never mentions disk would start
/// failing on a host below 20 GiB free.
///
/// So under `cfg(test)` the probe answers an injected reading, defaulting to
/// [`AMPLE_TEST_VOLUME`] — comfortably above any floor the defaults produce,
/// so the disk gate is inert unless a test asks otherwise. Tests that *are*
/// about the floor either configure the floor around this reading (a
/// `min-free-bytes` of `u64::MAX` is below nothing) or inject their own with
/// [`with_reading`]. The state is thread-local, so it is scoped to the test
/// that set it under libtest's thread-per-test model, and the returned guard
/// restores the previous value so it stays scoped under `--test-threads=1`
/// too.
#[cfg(test)]
pub(super) mod testing {
    use std::cell::Cell;

    use super::DiskSpace;

    /// The default reading every test sees: a 1 TiB volume with 512 GiB free.
    /// Both halves matter — `total_bytes` feeds the proportional floor, and a
    /// zero total would make `min-free-percent` vacuous.
    pub(in crate::app) const AMPLE_TEST_VOLUME: DiskSpace = DiskSpace {
        available_bytes: 512 * 1024 * 1024 * 1024,
        total_bytes: 1024 * 1024 * 1024 * 1024,
    };

    thread_local! {
        /// `None` means "call the real `statvfs`", which only the probe's own
        /// tests want.
        static READING: Cell<Option<DiskSpace>> = const { Cell::new(Some(AMPLE_TEST_VOLUME)) };
    }

    pub(super) fn injected_reading() -> Option<DiskSpace> {
        READING.get()
    }

    /// Restores the previous injected reading when dropped.
    #[must_use = "the injected reading is restored when this guard drops"]
    pub(in crate::app) struct ReadingGuard(Option<DiskSpace>);

    impl Drop for ReadingGuard {
        fn drop(&mut self) {
            READING.set(self.0);
        }
    }

    /// Make every `DiskSpace::probe` on this thread answer `space` until the
    /// returned guard drops.
    pub(in crate::app) fn with_reading(space: DiskSpace) -> ReadingGuard {
        ReadingGuard(READING.replace(Some(space)))
    }

    /// Opt back into the real `statvfs` — for the probe's own tests, which are
    /// the one place that should be reading the host.
    pub(in crate::app) fn use_real_volume() -> ReadingGuard {
        ReadingGuard(READING.replace(None))
    }

    /// A volume with `available_bytes` free out of [`AMPLE_TEST_VOLUME`]'s
    /// total, for tests that need the floor to bite.
    pub(in crate::app) fn volume_with_free(available_bytes: u64) -> DiskSpace {
        DiskSpace {
            available_bytes,
            ..AMPLE_TEST_VOLUME
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_the_volume_containing_a_real_path() {
        let _real = testing::use_real_volume();
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
        let _real = testing::use_real_volume();
        let err = DiskSpace::probe(Path::new("/definitely/not/a/real/mount/point-cube")).unwrap_err();
        assert!(matches!(err, CubeError::Io(_)), "expected an I/O error, got {err:?}");
    }

    #[test]
    fn tests_read_an_injected_volume_not_the_host_one() {
        // The point of the seam: a test that says nothing about disk gets a
        // reading no host can perturb, and one that cares can pin its own for
        // exactly as long as it needs it.
        let tempdir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            DiskSpace::probe(tempdir.path()).expect("probe"),
            testing::AMPLE_TEST_VOLUME,
        );
        {
            let _low = testing::with_reading(testing::volume_with_free(1024));
            assert_eq!(DiskSpace::probe(tempdir.path()).expect("probe").available_bytes, 1024);
        }
        assert_eq!(
            DiskSpace::probe(tempdir.path()).expect("probe"),
            testing::AMPLE_TEST_VOLUME,
            "the guard restores what it replaced",
        );
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
