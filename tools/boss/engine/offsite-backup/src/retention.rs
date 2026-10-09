//! Hourly + daily retention for off-machine copies.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::copy::BACKUP_FILE_PREFIX;

/// Best-effort retention report, including enumeration and deletion failures.
#[derive(Debug, Default)]
pub struct PruneOutcome {
    pub removed: Vec<PathBuf>,
    pub failures: Vec<(PathBuf, std::io::Error)>,
}

/// Keep the newest copy in each retained hour/day. Unrelated files are
/// untouched; recognized staging files older than 24 hours are removed.
pub fn prune(host_dir: &Path, keep_hourly: usize, keep_daily: usize) -> Result<PruneOutcome> {
    let read_dir = std::fs::read_dir(host_dir).with_context(|| format!("read {}", host_dir.display()))?;
    let mut outcome = PruneOutcome::default();
    let mut copies = Vec::new();
    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                outcome.failures.push((host_dir.to_owned(), error));
                continue;
            }
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(stamp) = name.strip_prefix(BACKUP_FILE_PREFIX).filter(|s| is_stamp(s)) {
            copies.push((stamp.to_owned(), entry.path()));
        } else if is_staging_name(name) {
            match entry.metadata().and_then(|m| m.modified()) {
                Ok(modified) if modified.elapsed().unwrap_or_default().as_secs() >= 86400 => {
                    remove(&entry.path(), &mut outcome);
                }
                Ok(_) => {}
                Err(error) => outcome.failures.push((entry.path(), error)),
            }
        }
    }
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

    for (idx, (_, path)) in copies.iter().enumerate() {
        if keep.contains(&idx) {
            continue;
        }
        remove(path, &mut outcome);
    }
    Ok(outcome)
}

fn remove(path: &Path, outcome: &mut PruneOutcome) {
    match std::fs::remove_file(path) {
        Ok(()) => outcome.removed.push(path.to_owned()),
        Err(error) => outcome.failures.push((path.to_owned(), error)),
    }
}

fn is_staging_name(name: &str) -> bool {
    if let Some(stamp) = name
        .strip_prefix(".")
        .and_then(|n| n.strip_prefix(BACKUP_FILE_PREFIX))
        .and_then(|n| n.strip_suffix(".partial"))
    {
        return is_stamp(stamp);
    }
    let Some(tail) = name
        .strip_prefix(BACKUP_FILE_PREFIX)
        .and_then(|n| n.strip_suffix(".tmp"))
    else {
        return false;
    };
    let mut parts = tail.split('.');
    matches!((parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(stamp), Some(pid), Some(sequence), None)
        if is_stamp(stamp) && !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())
            && !sequence.is_empty() && sequence.bytes().all(|b| b.is_ascii_digit()))
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
        assert_eq!(removed.removed.len(), 3);
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
    fn never_touches_unrelated_or_recent_partial_files() {
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
        assert!(prune(tmp.path(), 24, 14).unwrap().removed.is_empty());
    }

    #[test]
    fn deletion_failure_is_reported_and_other_files_are_pruned() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("state.db.bak-20200101-000000")).unwrap();
        touch(tmp.path(), "20200102-000000");
        touch(tmp.path(), "20260101-000000");
        let result = prune(tmp.path(), 1, 0).unwrap();
        assert_eq!(result.failures.len(), 1);
        assert_eq!(result.failures[0].0, tmp.path().join("state.db.bak-20200101-000000"));
        assert_eq!(result.removed.len(), 1);
    }

    #[test]
    fn old_staging_files_are_pruned_but_recent_and_unrelated_files_survive() {
        let tmp = TempDir::new().unwrap();
        for name in [
            ".state.db.bak-20260101-000000.partial",
            "state.db.bak-20260101-000000.123.4.tmp",
            "unrelated.tmp",
        ] {
            let file = std::fs::File::create(tmp.path().join(name)).unwrap();
            file.set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(86401)),
            )
            .unwrap();
        }
        std::fs::write(tmp.path().join(".state.db.bak-20260102-000000.partial"), b"active").unwrap();
        let result = prune(tmp.path(), 1, 1).unwrap();
        assert!(result.failures.is_empty());
        assert_eq!(result.removed.len(), 2);
        assert!(tmp.path().join("unrelated.tmp").exists());
        assert!(tmp.path().join(".state.db.bak-20260102-000000.partial").exists());
    }

    #[test]
    fn missing_dir_is_an_error() {
        let tmp = TempDir::new().unwrap();
        assert!(prune(&tmp.path().join("gone"), 1, 1).is_err());
    }
}
