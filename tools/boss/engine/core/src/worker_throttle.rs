//! Two-level pane priority setting, persisted in state.db like dispatch
//! concurrency. The existing boolean Settings RPC represents Off/Background.

use crate::work::WorkDb;
use anyhow::Result;

pub(crate) const SETTING_KEY: &str = "workers.background_throttle";

pub(crate) fn enabled(db: &WorkDb) -> Result<bool> {
    match db.get_metadata(SETTING_KEY)?.as_deref() {
        None | Some("off") => Ok(false),
        Some("background") => Ok(true),
        Some(value) => {
            tracing::warn!("unrecognized worker throttle level {value:?}; defaulting to off");
            Ok(false)
        }
    }
}

pub(crate) fn set(db: &WorkDb, enabled: bool) -> Result<()> {
    db.set_metadata(SETTING_KEY, if enabled { "background" } else { "off" })
}

pub(crate) fn snapshot(db: &WorkDb) -> Result<boss_protocol::SettingSnapshot> {
    Ok(boss_protocol::SettingSnapshot {
        key: SETTING_KEY.into(),
        description: "Background lowers CPU and I/O priority for all local macOS worker pools, including reviews and automation. Builds may run much slower.".into(),
        default_enabled: false,
        enabled: enabled(db)?,
    })
}

pub(crate) fn priority_clause(enabled: bool, macos: bool) -> &'static str {
    if !enabled || !macos {
        return "";
    }
    // Keep taskpolicy's stderr visible. Persist the failure status as well,
    // because a driver may clear the terminal after starting. A failed
    // priority adjustment should not prevent the worker from starting.
    "/usr/sbin/taskpolicy -b -p $$ || { printf 'Boss: taskpolicy failed (exit %s); worker priority was not changed.\\n' \"$?\" | tee -a .boss/worker-throttle.log >&2; }; "
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_off_and_persists_both_levels_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let db = WorkDb::open(path.clone()).unwrap();
        assert!(!snapshot(&db).unwrap().enabled);
        assert!(!snapshot(&db).unwrap().default_enabled);
        set(&db, true).unwrap();
        assert!(enabled(&db).unwrap());
        drop(db);
        let db = WorkDb::open(path.clone()).unwrap();
        assert!(enabled(&db).unwrap());
        set(&db, false).unwrap();
        drop(db);
        assert!(!enabled(&WorkDb::open(path.clone()).unwrap()).unwrap());
    }

    #[test]
    fn unknown_stored_level_defaults_off() {
        let db = WorkDb::open_in_memory().unwrap();
        db.set_metadata(SETTING_KEY, "unknown").unwrap();
        assert!(!enabled(&db).unwrap());
        assert!(!snapshot(&db).unwrap().enabled);
    }

    #[test]
    fn off_and_non_macos_emit_no_command() {
        assert_eq!(priority_clause(false, true), "");
        assert_eq!(priority_clause(false, false), "");
        assert_eq!(priority_clause(true, false), "");
        assert!(priority_clause(true, true).starts_with("/usr/sbin/taskpolicy -b -p $$"));
    }

    #[test]
    fn failed_priority_adjustment_is_logged_and_worker_continues() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".boss")).unwrap();
        // Exercise the real shell clause without changing the test's priority.
        let clause = priority_clause(true, true).replace("/usr/sbin/taskpolicy -b -p $$", "(exit 73)");
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &format!("{clause}printf worker-started")])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"worker-started");
        assert!(String::from_utf8_lossy(&output.stderr).contains("taskpolicy failed (exit 73)"));
        let log = std::fs::read_to_string(dir.path().join(".boss/worker-throttle.log")).unwrap();
        assert!(log.contains("taskpolicy failed (exit 73)"));
    }
}
