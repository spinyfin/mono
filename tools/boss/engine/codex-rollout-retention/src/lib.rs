//! Retention adapter for recorded Codex homes, plus rollout-path helpers.
//!
//! Planning, reclaim, and sizing live in
//! `boss_engine_driver_home_retention`. This crate keeps Codex's policy
//! constants and env names, and the rollout discovery helpers that are
//! Codex-specific.
//!
//! Per-run `CODEX_HOME` trees live under a Boss-controlled root (see
//! `boss_engine_driver::codex::codex_homes_root`) and hold session rollouts
//! at `$CODEX_HOME/sessions/{YYYY}/{MM}/{DD}/rollout-{local-timestamp}-{thread_id}.jsonl`
//! (ghostty-codex-pane-viability Q7 — filename embeds a *local* start
//! timestamp, so discovery is a glob on `thread_id`, never a rendered
//! template path).
//!
//! Cleanup operates **only** on paths recorded in per-execution
//! driver-owned runtime state (`CodexRuntimeState.codex_home`), never
//! inferred from the engine environment and never gated on cwd.
//! Interactive history under `~/.codex` is never scanned.

pub use boss_engine_driver_home_retention::{
    OwnedHome as OwnedCodexHome, ReclaimPlan, ReclaimReport, RetentionPolicy, directory_size_bytes,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

use walkdir::WalkDir;

/// Default retention window for terminal homes (matches terminal
/// `work_executions` retention: long enough for "what happened last week?").
pub const DEFAULT_MAX_AGE_DAYS: u64 = 14;

/// Default total-bytes backstop across retained terminal homes.
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 500 * 1024 * 1024;

/// Env var overriding [`DEFAULT_MAX_AGE_DAYS`].
pub const MAX_AGE_DAYS_ENV: &str = "BOSS_CODEX_HOME_RETENTION_DAYS";
/// Env var overriding [`DEFAULT_MAX_TOTAL_BYTES`].
pub const MAX_TOTAL_BYTES_ENV: &str = "BOSS_CODEX_HOME_MAX_BYTES";

/// Operator-tunable retention bound. See [`RetentionPolicy`] for the shared
/// retention rule.
#[derive(Debug, Clone, Copy)]
pub struct CodexHomeRetentionPolicy {
    pub max_age: Duration,
    pub max_total_bytes: u64,
}

impl Default for CodexHomeRetentionPolicy {
    fn default() -> Self {
        let p = RetentionPolicy::from_env(
            MAX_AGE_DAYS_ENV,
            MAX_TOTAL_BYTES_ENV,
            DEFAULT_MAX_AGE_DAYS,
            DEFAULT_MAX_TOTAL_BYTES,
        );
        Self::new(p.max_age, p.max_total_bytes)
    }
}

impl CodexHomeRetentionPolicy {
    /// Construct a policy with explicit knobs (tests / bossctl flags).
    pub fn new(max_age: Duration, max_total_bytes: u64) -> Self {
        Self {
            max_age,
            max_total_bytes,
        }
    }
}

pub fn plan_reclaim(homes: &[OwnedCodexHome], now_epoch: i64, policy: &CodexHomeRetentionPolicy) -> ReclaimPlan {
    boss_engine_driver_home_retention::plan_reclaim(
        homes,
        now_epoch,
        &RetentionPolicy::new(policy.max_age, policy.max_total_bytes),
    )
}

pub fn execute_reclaim(
    homes: &[OwnedCodexHome],
    now_epoch: i64,
    policy: &CodexHomeRetentionPolicy,
    dry_run: bool,
    safety: &dyn Fn(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<ReclaimReport> {
    boss_engine_driver_home_retention::execute_reclaim(
        homes,
        now_epoch,
        &RetentionPolicy::new(policy.max_age, policy.max_total_bytes),
        dry_run,
        safety,
    )
}

/// Locate a Codex rollout JSONL by `thread_id` under a Boss-owned home.
///
/// Path shape (Q7):
/// `$CODEX_HOME/sessions/{YYYY}/{MM}/{DD}/rollout-{local-timestamp}-{thread_id}.jsonl`
///
/// The local start timestamp is not known a priori, so discovery is a
/// glob on `thread_id`, never a path template. Returns the first match
/// (Codex writes one rollout per thread) or `None`.
pub fn find_rollout_by_thread_id(codex_home: &Path, thread_id: &str) -> Option<PathBuf> {
    if thread_id.is_empty() {
        return None;
    }
    let sessions = codex_home.join("sessions");
    if !sessions.is_dir() {
        return None;
    }
    let suffix = format!("-{thread_id}.jsonl");
    for entry in WalkDir::new(&sessions).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if name.starts_with("rollout-") && name.ends_with(&suffix) {
            return Some(entry.path().to_path_buf());
        }
    }
    None
}

/// Best-effort extraction of `session_meta.payload.cwd` from a rollout
/// file (first ~20 lines). Retained as a diagnostic helper; retention
/// decisions never use cwd to decide what is safe to delete.
pub fn parse_session_cwd(path: &Path) -> Option<PathBuf> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().take(20).map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if value.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
            continue;
        }
        if let Some(cwd) = value.get("payload").and_then(|p| p.get("cwd")).and_then(|c| c.as_str()) {
            return Some(PathBuf::from(cwd));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialise env-mutating policy tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn find_rollout_by_thread_id_globs_local_timestamp_filename() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions/2026/07/26");
        std::fs::create_dir_all(&sessions).unwrap();
        let thread = "019f9e0a-bf69-7cc0-8bbb-6848d03fb1c1";
        let expected = sessions.join(format!("rollout-2026-07-26T03-48-52-{thread}.jsonl"));
        std::fs::write(&expected, "{}\n").unwrap();
        // Unrelated rollout must not match.
        std::fs::write(
            sessions.join("rollout-2026-07-26T01-00-00-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl"),
            "{}\n",
        )
        .unwrap();

        let found = find_rollout_by_thread_id(dir.path(), thread).unwrap();
        assert_eq!(found, expected);
        assert!(find_rollout_by_thread_id(dir.path(), "no-such-thread").is_none());
        assert!(find_rollout_by_thread_id(dir.path(), "").is_none());
    }

    #[test]
    fn parse_session_cwd_reads_real_codex_rollout_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &path,
            r#"{"timestamp":"2026-07-25T03:41:31.960Z","type":"session_meta","payload":{"id":"abc","cwd":"/cube/workspaces/w1","originator":"codex_exec"}}
{"timestamp":"2026-07-25T03:41:32.000Z","type":"turn_context","payload":{}}
"#,
        )
        .unwrap();
        assert_eq!(parse_session_cwd(&path), Some(PathBuf::from("/cube/workspaces/w1")));
    }

    #[test]
    fn default_policy_reads_env_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: serialised by ENV_LOCK.
        unsafe {
            std::env::set_var(MAX_AGE_DAYS_ENV, "3");
            std::env::set_var(MAX_TOTAL_BYTES_ENV, "1024");
        }
        let policy = CodexHomeRetentionPolicy::default();
        assert_eq!(policy.max_age, Duration::from_secs(3 * 24 * 60 * 60));
        assert_eq!(policy.max_total_bytes, 1024);
        unsafe {
            std::env::remove_var(MAX_AGE_DAYS_ENV);
            std::env::remove_var(MAX_TOTAL_BYTES_ENV);
        }
    }
}
