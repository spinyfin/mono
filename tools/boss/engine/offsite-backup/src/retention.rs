//! Hourly + daily retention for off-machine copies.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::copy::BACKUP_FILE_PREFIX;

/// Delete copies in `host_dir` that fall outside the retention policy and
/// return the paths removed.
///
/// A copy is kept when it is the newest copy within one of the `keep_hourly`
/// most recent distinct hours, or the newest within one of the `keep_daily`
/// most recent distinct days (both measured over the copies that exist, not
/// wall-clock time, so an engine that was off for a week does not prune its
/// only copies). Files that do not parse as `state.db.bak-YYYYMMDD-HHMMSS`
/// (including `.partial` temp files and anything else a user put there) are
/// never touched. Removal failures are logged and skipped.
pub fn prune(host_dir: &Path, keep_hourly: usize, keep_daily: usize) -> Result<Vec<PathBuf>> {
    let read_dir = std::fs::read_dir(host_dir).with_context(|| format!("read {}", host_dir.display()))?;
    let mut copies: Vec<(String, PathBuf)> = read_dir
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let stamp = name.strip_prefix(BACKUP_FILE_PREFIX)?;
            is_stamp(stamp).then(|| (stamp.to_owned(), e.path()))
        })
        .collect();
    // `YYYYMMDD-HHMMSS` sorts lexicographically; newest first.
    copies.sort_by(|a, b| b.0.cmp(&a.0));

    let mut keep: HashSet<usize> = HashSet::new();
    let mut seen_hours = HashSet::new();
    let mut seen_days = HashSet::new();
    for (idx, (stamp, _)) in copies.iter().enumerate() {
        let hour = &stamp[..11];
        let day = &stamp[..8];
        if seen_hours.len() < keep_hourly && seen_hours.insert(hour) {
            keep.insert(idx);
        }
        if seen_days.len() < keep_daily && seen_days.insert(day) {
            keep.insert(idx);
        }
    }

    let mut removed = Vec::new();
    for (idx, (_, path)) in copies.iter().enumerate() {
        if keep.contains(&idx) {
            continue;
        }
        match std::fs::remove_file(path) {
            Ok(()) => removed.push(path.clone()),
            Err(err) => tracing::warn!(
                path = %path.display(),
                error = %err,
                "offsite-backup: could not delete old copy (non-fatal)",
            ),
        }
    }
    Ok(removed)
}

fn is_stamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 15 && b[8] == b'-' && b.iter().enumerate().all(|(i, c)| i == 8 || c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn touch(dir: &Path, stamp: &str) {
        std::fs::write(dir.join(format!("{BACKUP_FILE_PREFIX}{stamp}")), b"x").unwrap();
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn hourly_window_keeps_newest_per_hour_and_daily_keeps_one_per_day() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        // Day 3: three copies within hour 10, one in hour 09. Day 2: two. Day 1: one.
        for s in [
            "20260103-100000",
            "20260103-103000",
            "20260103-105900",
            "20260103-090000",
            "20260102-230000",
            "20260102-010000",
            "20260101-120000",
        ] {
            touch(d, s);
        }
        // Hourly 2 -> newest of hour 10 (105900) and hour 09 (090000).
        // Daily 3 -> newest of Jan 3 (105900), Jan 2 (230000), Jan 1 (120000).
        let removed = prune(d, 2, 3).unwrap();
        assert_eq!(removed.len(), 3);
        assert_eq!(
            names(d),
            vec![
                "state.db.bak-20260101-120000",
                "state.db.bak-20260102-230000",
                "state.db.bak-20260103-090000",
                "state.db.bak-20260103-105900",
            ]
        );
    }

    #[test]
    fn never_touches_unrelated_or_partial_files() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        touch(d, "20260101-000000");
        touch(d, "20260102-000000");
        std::fs::write(d.join("notes.txt"), b"x").unwrap();
        std::fs::write(d.join(".state.db.bak-20260101-000000.partial"), b"x").unwrap();
        std::fs::write(d.join("state.db.bak-garbage"), b"x").unwrap();
        prune(d, 1, 0).unwrap();
        let n = names(d);
        assert!(n.contains(&"notes.txt".to_owned()));
        assert!(n.contains(&".state.db.bak-20260101-000000.partial".to_owned()));
        assert!(n.contains(&"state.db.bak-garbage".to_owned()));
        assert!(!n.contains(&"state.db.bak-20260101-000000".to_owned()));
        assert!(n.contains(&"state.db.bak-20260102-000000".to_owned()));
    }

    #[test]
    fn noop_when_under_limits() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "20260101-000000");
        assert!(prune(tmp.path(), 24, 14).unwrap().is_empty());
    }

    #[test]
    fn missing_dir_is_an_error() {
        let tmp = TempDir::new().unwrap();
        assert!(prune(&tmp.path().join("gone"), 1, 1).is_err());
    }
}
