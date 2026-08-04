//! Durable, transcript-only storage for isolated agent homes.
//!
//! Codex and Grok retain their session JSONL under Boss's state root while
//! keeping credentials, hooks, and all other per-run state in their temporary
//! homes. The per-execution layout mirrors Boss's other durable artifacts.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

/// Test/operator override for the Boss-owned worker transcript-store root.
pub const WORKER_TRANSCRIPTS_ROOT_ENV: &str = "BOSS_WORKER_TRANSCRIPTS_ROOT";

/// Resolve Boss's durable worker transcript store.
///
/// Production uses `<Boss state root>/executions`, alongside other
/// per-execution artifacts. Tests and operators can override that root with
/// [`WORKER_TRANSCRIPTS_ROOT_ENV`].
pub fn transcript_store_root() -> anyhow::Result<PathBuf> {
    if let Some(root) = std::env::var_os(WORKER_TRANSCRIPTS_ROOT_ENV).filter(|root| !root.is_empty()) {
        return Ok(PathBuf::from(root));
    }
    boss_log_files::default_state_root()
        .map(|root| root.join("executions"))
        .ok_or_else(|| anyhow::anyhow!("HOME is unset; cannot resolve Boss worker transcript storage"))
}

/// Make `home/sessions` a link to a dedicated transcript-only directory in
/// Boss's per-execution artifact directory. The destination is namespaced by
/// run and driver so neither driver can collide.
///
/// This is deliberately a provisioning error, not a best-effort completion
/// action: a killed or orphaned worker is still writing to the durable file,
/// and a failed setup is visible before the worker starts.
pub fn provision_durable_sessions(home: &Path, driver: &str, run_id: &str) -> anyhow::Result<PathBuf> {
    let root = transcript_store_root()?;
    provision_durable_sessions_in(home, &root, driver, run_id)
}

/// Injectable form of [`provision_durable_sessions`] for tests.
pub fn provision_durable_sessions_in(
    home: &Path,
    store_root: &Path,
    driver: &str,
    run_id: &str,
) -> anyhow::Result<PathBuf> {
    let destination = durable_sessions_dir(store_root, driver, run_id)?;
    fs::create_dir_all(&destination)
        .with_context(|| format!("creating durable worker transcript directory {}", destination.display()))?;

    let sessions = home.join("sessions");
    match fs::symlink_metadata(&sessions) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            verified_durable_sessions_dir(home, store_root, driver, run_id)?;
        }
        Ok(metadata) if metadata.is_dir() => {
            // Provisioning creates this empty directory before the driver has
            // started. Never relocate a non-empty directory: that could hide
            // an already-written transcript instead of retaining it.
            if fs::read_dir(&sessions)?.next().is_some() {
                bail!(
                    "refusing to replace non-empty temporary sessions directory {}",
                    sessions.display()
                );
            }
            fs::remove_dir(&sessions)
                .with_context(|| format!("removing empty temporary sessions directory {}", sessions.display()))?;
            symlink_dir(&destination, &sessions)?;
        }
        Ok(_) => bail!("refusing unexpected sessions path {}", sessions.display()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => symlink_dir(&destination, &sessions)?,
        Err(err) => return Err(err).with_context(|| format!("stat sessions path {}", sessions.display())),
    }
    Ok(destination)
}

/// Return the dedicated durable sessions directory for one driver run.
pub fn durable_sessions_dir(store_root: &Path, driver: &str, run_id: &str) -> anyhow::Result<PathBuf> {
    let driver = safe_segment(driver, "driver")?;
    let run_id = safe_segment(run_id, "run_id")?;
    Ok(store_root
        .join(run_id)
        .join("transcripts")
        .join(driver)
        .join("sessions"))
}

/// Resolve and verify a provisioned `home/sessions` link against its
/// per-execution durable destination.
pub fn verified_durable_sessions_dir(
    home: &Path,
    store_root: &Path,
    driver: &str,
    run_id: &str,
) -> anyhow::Result<PathBuf> {
    let sessions = home.join("sessions");
    let metadata =
        fs::symlink_metadata(&sessions).with_context(|| format!("stat sessions path {}", sessions.display()))?;
    if !metadata.file_type().is_symlink() {
        bail!("expected durable sessions link at {}", sessions.display());
    }
    let actual =
        fs::canonicalize(&sessions).with_context(|| format!("resolving sessions link {}", sessions.display()))?;
    let destination = durable_sessions_dir(store_root, driver, run_id)?;
    let expected = fs::canonicalize(&destination)
        .with_context(|| format!("resolving durable transcript directory {}", destination.display()))?;
    if actual != expected {
        bail!(
            "sessions link {} targets {}; expected {}",
            sessions.display(),
            actual.display(),
            expected.display()
        );
    }
    Ok(expected)
}

fn symlink_dir(target: &Path, link: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).with_context(|| {
            format!(
                "linking {} to durable transcript directory {}",
                link.display(),
                target.display()
            )
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (target, link);
        bail!("durable worker transcript links require a Unix host")
    }
}

fn safe_segment(value: &str, field: &str) -> anyhow::Result<String> {
    if value.is_empty() || value == "." || value == ".." || value.contains(['/', '\\']) {
        bail!("unsafe {field} for durable worker transcript path: {value:?}");
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentDriver;
    use crate::codex::{CodexDriver, codex_home_for_run};
    use crate::grok::{GrokDriver, grok_home_for_run};
    use std::sync::Mutex;

    static TRANSCRIPT_STORE_ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct TranscriptStoreOverride {
        _lock: std::sync::MutexGuard<'static, ()>,
        prior: Option<std::ffi::OsString>,
    }

    impl TranscriptStoreOverride {
        fn new(root: &Path) -> Self {
            let lock = TRANSCRIPT_STORE_ENV_TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let prior = std::env::var_os(WORKER_TRANSCRIPTS_ROOT_ENV);
            // SAFETY: the process-wide lock above is held until this guard restores the variable.
            unsafe { std::env::set_var(WORKER_TRANSCRIPTS_ROOT_ENV, root) };
            Self { _lock: lock, prior }
        }
    }

    impl Drop for TranscriptStoreOverride {
        fn drop(&mut self) {
            // SAFETY: this guard still holds the process-wide lock.
            match self.prior.as_ref() {
                Some(value) => unsafe { std::env::set_var(WORKER_TRANSCRIPTS_ROOT_ENV, value) },
                None => unsafe { std::env::remove_var(WORKER_TRANSCRIPTS_ROOT_ENV) },
            }
        }
    }

    #[test]
    fn sessions_link_retains_only_the_transcript_subtree() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("temporary-home");
        let store = tmp.path().join("boss-executions");
        fs::create_dir_all(home.join("sessions")).unwrap();
        fs::write(home.join("auth.json"), "credential").unwrap();

        let durable = provision_durable_sessions_in(&home, &store, "codex", "exec-run").unwrap();
        fs::write(home.join("sessions/rollout.jsonl"), "full conversation\n").unwrap();

        assert_eq!(
            fs::read_to_string(durable.join("rollout.jsonl")).unwrap(),
            "full conversation\n"
        );
        assert!(home.join("auth.json").is_file());
        fs::remove_dir_all(&home).unwrap();
        assert!(durable.join("rollout.jsonl").is_file());
    }

    #[test]
    fn durable_sessions_follow_boss_per_execution_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("temporary-home");
        let store = tmp.path().join("executions");
        fs::create_dir_all(home.join("sessions")).unwrap();

        let durable = provision_durable_sessions_in(&home, &store, "codex", "exec-run").unwrap();

        assert_eq!(durable, store.join("exec-run/transcripts/codex/sessions"));
    }

    #[test]
    fn verified_durable_sessions_rejects_a_retargeted_link() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("temporary-home");
        let store = tmp.path().join("executions");
        fs::create_dir_all(&home).unwrap();
        provision_durable_sessions_in(&home, &store, "codex", "exec-run").unwrap();
        let replacement = tmp.path().join("replacement");
        fs::create_dir_all(&replacement).unwrap();
        fs::remove_file(home.join("sessions")).unwrap();
        symlink_dir(&replacement, &home.join("sessions")).unwrap();

        let err = verified_durable_sessions_dir(&home, &store, "codex", "exec-run").unwrap_err();
        assert!(err.to_string().contains("targets"));
    }

    #[test]
    fn transcript_store_root_honors_the_override() {
        let tmp = tempfile::tempdir().unwrap();
        let override_root = tmp.path().join("transcripts");
        let _override = TranscriptStoreOverride::new(&override_root);
        assert_eq!(transcript_store_root().unwrap(), override_root);
    }

    #[test]
    fn transcript_store_root_defaults_to_boss_executions_directory() {
        let _lock = TRANSCRIPT_STORE_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prior = std::env::var_os(WORKER_TRANSCRIPTS_ROOT_ENV);
        // SAFETY: the process-wide lock is held and the variable is restored below.
        unsafe { std::env::remove_var(WORKER_TRANSCRIPTS_ROOT_ENV) };
        assert_eq!(
            transcript_store_root().unwrap(),
            boss_log_files::default_state_root().unwrap().join("executions")
        );
        // SAFETY: the process-wide lock is still held.
        match prior.as_ref() {
            Some(value) => unsafe { std::env::set_var(WORKER_TRANSCRIPTS_ROOT_ENV, value) },
            None => unsafe { std::env::remove_var(WORKER_TRANSCRIPTS_ROOT_ENV) },
        }
    }

    #[test]
    fn drivers_contain_transcripts_to_their_own_durable_sessions_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("executions");
        let _store = TranscriptStoreOverride::new(&store);

        let _codex_homes = crate::test_support::codex_homes_override(&tmp.path().join("codex-homes"));
        let codex_run_id = "codex-containment";
        let codex_home = codex_home_for_run(codex_run_id).unwrap();
        fs::create_dir_all(&codex_home).unwrap();
        let codex_expected = provision_durable_sessions(&codex_home, "codex", codex_run_id).unwrap();
        assert_eq!(
            CodexDriver::default()
                .transcript_containment_root(codex_run_id)
                .unwrap(),
            Some(fs::canonicalize(codex_expected).unwrap())
        );
        drop(_codex_homes);

        let _grok_homes = crate::test_support::grok_homes_override(&tmp.path().join("grok-homes"));
        let grok_run_id = "grok-containment";
        let grok_home = grok_home_for_run(grok_run_id).unwrap();
        fs::create_dir_all(&grok_home).unwrap();
        let grok_expected = provision_durable_sessions(&grok_home, "grok", grok_run_id).unwrap();
        assert_eq!(
            GrokDriver::default().transcript_containment_root(grok_run_id).unwrap(),
            Some(fs::canonicalize(grok_expected).unwrap())
        );
    }
}
