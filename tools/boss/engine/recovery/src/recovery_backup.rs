//! Optional patch exports from engine-created execution bookmarks.
//! Recovery follows the reference in the shared store; patch exports retain
//! evidence without inspecting an old workspace or pushing unfinished work.

use std::path::{Path, PathBuf};

use crate::execution_bookmark::{self, ExecutionBookmark, Jj};
use anyhow::{Context, Result};

/// Environment override for the recovery directory. Set by tests to
/// redirect captures into a tempdir; an operator can also point it at an
/// alternate location. When unset, [`default_recovery_dir`] falls back to
/// the engine's `Application Support` tree.
pub const RECOVERY_DIR_ENV: &str = "BOSS_RECOVERY_DIR";

/// Resolve the engine-owned recovery directory.
///
/// Honours [`RECOVERY_DIR_ENV`] first, then falls back to
/// `$HOME/Library/Application Support/Boss/recovery` (the same
/// `Application Support/Boss` tree that holds `state.db`). Returns `None`
/// only when neither the override nor `HOME` is set — in which case there
/// is nowhere durable to write and the caller skips the backup.
pub fn default_recovery_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(RECOVERY_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    Some(boss_log_files::default_state_root()?.join("recovery"))
}

/// Write `diff` to `<recovery_dir>/<exec-id>.patch` when it holds real work.
///
/// Creates `recovery_dir` if needed. An all-whitespace/empty diff means
/// the workspace had no uncommitted work — there is nothing to back up,
/// so no file is written and `Ok(None)` is returned. A diff holding
/// *only* Boss's own bookkeeping is treated the same way: see the filter
/// below. Otherwise the (filtered) patch is written and its path returned.
pub fn write_patch_if_nonempty(recovery_dir: &Path, execution_id: &str, diff: &str) -> Result<Option<PathBuf>> {
    if diff.trim().is_empty() {
        return Ok(None);
    }
    // Drop Boss's own bookkeeping (`.boss/events-pending.jsonl` and friends)
    // at capture time as well as at apply time. Three of the four patches
    // taken at 14:42 PDT on 2026-07-23 were 203 KB / 197 KB / 38 KB of
    // nothing but that hook spool — patches that looked substantial, held no
    // work, and would have replayed stale hook events into a fresh
    // workspace. Filtering here makes the artifact on disk honest about what
    // it holds; `recovery_apply` filters again so the ~86 patches already on
    // disk are handled too.
    let filtered = crate::recovery_apply::filter_bookkeeping(diff);
    if filtered.is_empty() {
        tracing::debug!(
            execution_id,
            filtered_paths = ?filtered.filtered_paths,
            "recovery-backup: diff held only Boss bookkeeping; nothing worth capturing",
        );
        return Ok(None);
    }
    let diff = filtered.text.as_str();
    std::fs::create_dir_all(recovery_dir)
        .with_context(|| format!("failed to create recovery dir {}", recovery_dir.display()))?;
    let path = recovery_dir.join(crate::recovery_apply::patch_file_name(execution_id));
    std::fs::write(&path, diff.as_bytes())
        .with_context(|| format!("failed to write recovery patch {}", path.display()))?;
    Ok(Some(path))
}

/// Export the entire execution range, including described ancestor changes.
pub async fn backup_execution_patch(
    recovery_dir: &Path,
    jj: &dyn Jj,
    record: &ExecutionBookmark,
) -> Result<Option<PathBuf>> {
    let diff = execution_bookmark::diff(jj, record).await?;
    write_patch_if_nonempty(recovery_dir, &record.execution_id, &diff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── default_recovery_dir ──────────────────────────────────────

    /// The `BOSS_RECOVERY_DIR` override wins over the HOME-derived path.
    /// Guarded by a process-global env lock since env is process-wide.
    #[test]
    fn env_override_takes_precedence() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var_os(RECOVERY_DIR_ENV);
        unsafe { std::env::set_var(RECOVERY_DIR_ENV, "/tmp/boss-recovery-test") };
        let resolved = default_recovery_dir();
        match prev {
            Some(v) => unsafe { std::env::set_var(RECOVERY_DIR_ENV, v) },
            None => unsafe { std::env::remove_var(RECOVERY_DIR_ENV) },
        }
        assert_eq!(resolved, Some(PathBuf::from("/tmp/boss-recovery-test")));
    }

    // ── write_patch_if_nonempty ───────────────────────────────────

    #[test]
    fn empty_diff_writes_nothing() {
        let dir = TempDir::new().unwrap();
        let result = write_patch_if_nonempty(dir.path(), "exec_1", "   \n\t\n").unwrap();
        assert!(result.is_none(), "whitespace-only diff must be treated as empty");
        // Recovery dir must not even be populated with a stray file.
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(entries.is_empty(), "no patch file should be written for an empty diff");
    }

    #[test]
    fn nonempty_diff_writes_patch_with_expected_name_and_contents() {
        let dir = TempDir::new().unwrap();
        let diff = "diff --git a/foo b/foo\n+added line\n";
        let path = write_patch_if_nonempty(dir.path(), "exec_abc_3", diff)
            .unwrap()
            .expect("a non-empty diff must produce a patch path");
        assert_eq!(path, dir.path().join("exec_abc_3.patch"));
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, diff, "patch contents must match the captured diff verbatim");
    }

    /// Patch size is not a signal of value: a 200 KB capture of nothing but
    /// the `.boss/` hook spool holds no work, and writing it would leave an
    /// artifact that later looks like a recoverable crash.
    #[test]
    fn bookkeeping_only_diff_writes_nothing() {
        let dir = TempDir::new().unwrap();
        let diff = "diff --git a/.boss/events-pending.jsonl b/.boss/events-pending.jsonl\n\
                    --- a/.boss/events-pending.jsonl\n\
                    +++ b/.boss/events-pending.jsonl\n\
                    @@ -0,0 +1 @@\n\
                    +{\"event\":\"Stop\"}\n";
        assert!(
            write_patch_if_nonempty(dir.path(), "exec_spool", diff)
                .unwrap()
                .is_none()
        );
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(entries.is_empty(), "a bookkeeping-only diff must not be captured");
    }

    /// A mixed capture keeps the work and drops the spool.
    #[test]
    fn mixed_diff_is_captured_without_the_bookkeeping_section() {
        let dir = TempDir::new().unwrap();
        let diff = "diff --git a/.boss/events-pending.jsonl b/.boss/events-pending.jsonl\n\
                    --- a/.boss/events-pending.jsonl\n\
                    +++ b/.boss/events-pending.jsonl\n\
                    @@ -0,0 +1 @@\n\
                    +{\"event\":\"Stop\"}\n\
                    diff --git a/src/lib.rs b/src/lib.rs\n\
                    --- a/src/lib.rs\n\
                    +++ b/src/lib.rs\n\
                    @@ -0,0 +1 @@\n\
                    +fn real_work() {}\n";
        let path = write_patch_if_nonempty(dir.path(), "exec_mixed", diff)
            .unwrap()
            .expect("real work must still be captured");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("fn real_work"), "the work must survive: {written}");
        assert!(
            !written.contains("events-pending"),
            "the spool must not be captured: {written}"
        );
    }

    #[test]
    fn write_patch_creates_missing_recovery_dir() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("recovery").join("nested");
        let path = write_patch_if_nonempty(&nested, "exec_x", "diff --git a/x b/x\n+y\n")
            .unwrap()
            .expect("patch should be written");
        assert!(path.exists());
        assert!(nested.is_dir(), "missing recovery dir must be created");
    }

    #[tokio::test]
    async fn backup_fails_when_shared_repository_is_unavailable() {
        let dir = TempDir::new().unwrap();
        let record = ExecutionBookmark {
            execution_id: "exec_missing".into(),
            repo_path: dir.path().join("missing"),
            host_id: "local".into(),
        };
        assert!(
            backup_execution_patch(dir.path(), &crate::execution_bookmark::LocalJj, &record)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn backup_fails_when_execution_bookmark_is_missing() {
        let dir = TempDir::new().unwrap();
        let repo = boss_engine_test_git::jj::JjRepo::new(dir.path());
        let record = ExecutionBookmark {
            execution_id: "exec_missing".into(),
            repo_path: repo.repo,
            host_id: "local".into(),
        };
        assert!(
            backup_execution_patch(dir.path(), &crate::execution_bookmark::LocalJj, &record)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn captures_uncommitted_work_from_real_jj_bookmark() {
        use crate::execution_bookmark::{LocalJj, create};
        use boss_engine_test_git::jj::JjRepo;
        let dir = TempDir::new().unwrap();
        let repo = JjRepo::new(dir.path());
        let record = create(&LocalJj, &repo.worker, "exec_real_1", "local").await.unwrap();
        std::fs::write(repo.worker.join("hello.txt"), "uncommitted work\n").unwrap();
        JjRepo::run(&repo.worker, &["status"]);
        std::fs::remove_dir_all(&repo.worker).unwrap();
        let patch = backup_execution_patch(dir.path(), &LocalJj, &record)
            .await
            .unwrap()
            .expect("work must be exported");
        assert_eq!(patch, dir.path().join("exec_real_1.patch"));
        let contents = std::fs::read_to_string(patch).unwrap();
        assert!(contents.contains("hello.txt"));
        assert!(contents.contains("uncommitted work"));
    }

    #[tokio::test]
    async fn clean_jj_execution_yields_no_patch() {
        use crate::execution_bookmark::{LocalJj, create};
        let dir = TempDir::new().unwrap();
        let repo = boss_engine_test_git::jj::JjRepo::new(dir.path());
        let record = create(&LocalJj, &repo.worker, "exec_clean_1", "local").await.unwrap();
        let patch = backup_execution_patch(dir.path(), &LocalJj, &record).await.unwrap();
        assert!(patch.is_none(), "an empty execution must not produce a patch");
    }

    // ── helpers ───────────────────────────────────────────────────

    /// Process-global lock serialising tests that mutate env vars.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }
}
