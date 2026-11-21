//! Durable storage for the transcripts of isolated agent homes.
//!
//! Codex, Grok and Antigravity (`agy`) retain their session records under
//! Boss's state root while keeping credentials, hooks, and all other per-run
//! state in their temporary homes. The per-execution layout mirrors Boss's
//! other durable artifacts. Codex and Grok link a dedicated `sessions`
//! directory that holds nothing but session JSONL; `agy` has no such
//! directory (its transcript lives inside the per-conversation `brain/` tree),
//! so it links `brain` itself — see [`AGY_BRAIN_LEAF`].

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

// Default leaf names of the per-run homes roots, reused directly so a rename
// of either original cannot silently desync this rewrite from where the homes
// actually live.
use crate::agy::{
    AGY_HOMES_DIR_NAME as AGY_HOMES_DIR_MARKER, GEMINI_DIR as AGY_GEMINI_DIR,
    PROCESS_HOME_LEAF as AGY_PROCESS_HOME_LEAF, STATE_DIR as AGY_STATE_DIR,
};
use crate::codex::CODEX_HOMES_DIR_NAME as CODEX_HOMES_DIR_MARKER;
use crate::grok::GROK_HOMES_DIR_NAME as GROK_HOMES_DIR_MARKER;

/// Test/operator override for the Boss-owned worker transcript-store root.
pub const WORKER_TRANSCRIPTS_ROOT_ENV: &str = "BOSS_WORKER_TRANSCRIPTS_ROOT";

/// The `agy` directory that is linked into the durable store, relative to a
/// per-run `HOME`: `.gemini/antigravity-cli/brain`. Every conversation's
/// transcript (`brain/<cid>/.system_generated/logs/transcript_full.jsonl`)
/// lives beneath it, so one link covers conversations whose ids are not known
/// until the CLI has started.
pub const AGY_BRAIN_LEAF: &str = "brain";
/// The driver slug `agy` files its transcripts under.
pub const AGY_TRANSCRIPT_DRIVER: &str = "agy";

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
    provision_durable_link_in(&home.join("sessions"), store_root, driver, run_id)
}

/// Make `link` a symlink to `driver`'s durable transcript directory for
/// `run_id`, creating that directory first.
///
/// The general form of [`provision_durable_sessions_in`]: Codex and Grok link
/// `<home>/sessions`, `agy` links `<home>/.gemini/antigravity-cli/brain`. The
/// link's parent must already exist. An existing link is verified rather than
/// replaced; an empty directory is replaced; a non-empty one is refused.
pub fn provision_durable_link_in(
    link: &Path,
    store_root: &Path,
    driver: &str,
    run_id: &str,
) -> anyhow::Result<PathBuf> {
    let destination = durable_sessions_dir(store_root, driver, run_id)?;
    fs::create_dir_all(&destination)
        .with_context(|| format!("creating durable worker transcript directory {}", destination.display()))?;

    match fs::symlink_metadata(link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            verified_durable_link(link, store_root, driver, run_id)?;
        }
        Ok(metadata) if metadata.is_dir() => {
            // A reused home or a driver that started first may leave this
            // directory behind. Never relocate a non-empty one: that could
            // hide an already-written transcript instead of retaining it.
            if fs::read_dir(link)?.next().is_some() {
                bail!(
                    "refusing to replace non-empty temporary transcript directory {}",
                    link.display()
                );
            }
            fs::remove_dir(link)
                .with_context(|| format!("removing empty temporary transcript directory {}", link.display()))?;
            symlink_dir(&destination, link)?;
        }
        Ok(_) => bail!("refusing unexpected transcript path {}", link.display()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => symlink_dir(&destination, link)?,
        Err(err) => return Err(err).with_context(|| format!("stat transcript path {}", link.display())),
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

/// Resolve a verified durable link, including after its home is reclaimed.
/// Legacy directories and stat failures leave containment to the caller.
pub(crate) fn durable_transcript_containment_root(
    link: &Path,
    driver: &str,
    run_id: &str,
) -> anyhow::Result<Option<PathBuf>> {
    match fs::symlink_metadata(link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            verified_durable_link(link, &transcript_store_root()?, driver, run_id).map(Some)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let durable = durable_sessions_dir(&transcript_store_root()?, driver, run_id)?;
            Ok(fs::canonicalize(durable).ok().filter(|canonical| canonical.is_dir()))
        }
        Err(error) => {
            tracing::debug!(?error, path = %link.display(), "stat of transcript link failed; falling back to legacy containment");
            Ok(None)
        }
        Ok(_) => Ok(None),
    }
}

/// Rewrite a driver-stamped transcript path onto Boss's durable store.
///
/// Codex and Grok provision `home/sessions`, and `agy` provisions
/// `home/.gemini/antigravity-cli/brain`, as a symlink into
/// `<state>/executions/<run_id>/transcripts/<driver>/sessions`. Hook payloads
/// stamp the path *through* that link, which dies when the per-run home is
/// reclaimed. The bytes are already in the durable directory; this returns
/// that location so `work_runs.transcript_path` survives reclaim.
///
/// Paths that are not under a provisioned ephemeral home (Claude, or a
/// stamp that is already durable) are returned unchanged.
pub fn persistable_transcript_path(path: &Path) -> PathBuf {
    if let Some(rewritten) = rewrite_through_sessions_symlink(path) {
        return rewritten;
    }
    if let Some(rewritten) = rewrite_known_ephemeral_layout(path) {
        // Prefer the reconstructed durable path when the stamped location is
        // already gone (backfill after reclaim) or when the durable file is
        // already the writer. If only the ephemeral real directory still
        // holds bytes (pre-symlink homes), leave the stamped path alone.
        if !path.exists() || rewritten.exists() {
            return rewritten;
        }
    }
    if looks_like_ephemeral_agent_home(path) {
        tracing::warn!(
            path = %path.display(),
            "transcript path is under a per-run agent home that reclaim will delete, \
             but it could not be rewritten onto the durable transcript store"
        );
    }
    path.to_path_buf()
}

fn rewrite_through_sessions_symlink(path: &Path) -> Option<PathBuf> {
    for ancestor in path.ancestors() {
        let Some(name) = ancestor.file_name() else {
            continue;
        };
        let agy_brain = name == AGY_BRAIN_LEAF && is_agy_state_dir(ancestor.parent());
        if name != "sessions" && !agy_brain {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(ancestor) else {
            continue;
        };
        if !meta.file_type().is_symlink() {
            continue;
        }
        let Ok(target) = fs::canonicalize(ancestor) else {
            continue;
        };
        // `brain` is a name any tool might use for a symlink, and a stamped
        // path is data the agent's CLI produced. Only follow it when it lands
        // inside Boss's own transcript store, so a redirected link can never
        // make `work_runs.transcript_path` point at an arbitrary directory.
        if agy_brain && !is_inside_transcript_store(&target) {
            continue;
        }
        let suffix = path.strip_prefix(ancestor).ok()?;
        return Some(target.join(suffix));
    }
    None
}

/// Whether `dir` is an `agy` `.gemini/antigravity-cli` state directory.
fn is_agy_state_dir(dir: Option<&Path>) -> bool {
    dir.is_some_and(|dir| {
        dir.file_name().is_some_and(|name| name == AGY_STATE_DIR)
            && dir
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == AGY_GEMINI_DIR)
    })
}

fn is_inside_transcript_store(canonical_target: &Path) -> bool {
    transcript_store_root()
        .ok()
        .and_then(|root| fs::canonicalize(root).ok())
        .is_some_and(|root| canonical_target.starts_with(root))
}

/// The run id segment recovered here is read back out of the path itself, so
/// it is whatever [`grok::home::sanitize_run_id_for_home`] /
/// [`codex::sanitize_run_id_for_home`] mapped the real run id to (any
/// character outside `[A-Za-z0-9_-]` becomes `_`), not the raw run id.
/// [`durable_sessions_dir`] is then called with that sanitized value even
/// though production always calls it with the raw run id elsewhere. This is
/// a no-op today because every run id Boss issues (`exec_<hex>`) is already
/// a fixed point of the sanitizer, but if that ever stops holding, this
/// reconstructs a directory that was never provisioned. `agy` has no such
/// gap: it provisions its durable directory from the sanitized container name
/// in the first place (`agy::home::durable_run_id`).
fn rewrite_known_ephemeral_layout(path: &Path) -> Option<PathBuf> {
    let comps: Vec<_> = path.iter().collect();
    let marker_idx = comps
        .iter()
        .position(|c| *c == GROK_HOMES_DIR_MARKER || *c == CODEX_HOMES_DIR_MARKER || *c == AGY_HOMES_DIR_MARKER)?;
    let marker = comps[marker_idx];
    let run_id = comps.get(marker_idx + 1)?.to_str()?;
    let (driver, sessions_idx) = if marker == AGY_HOMES_DIR_MARKER {
        // `<homes>/<run>/process-home/.gemini/antigravity-cli/brain/<cid>/…`
        let layout = [AGY_PROCESS_HOME_LEAF, AGY_GEMINI_DIR, AGY_STATE_DIR, AGY_BRAIN_LEAF];
        for (offset, expected) in layout.iter().enumerate() {
            if *comps.get(marker_idx + 2 + offset)? != *expected {
                return None;
            }
        }
        (AGY_TRANSCRIPT_DRIVER, marker_idx + 2 + layout.len() - 1)
    } else if marker == GROK_HOMES_DIR_MARKER {
        let grok_home = *comps.get(marker_idx + 2)?;
        let sessions = *comps.get(marker_idx + 3)?;
        if grok_home != "grok-home" || sessions != "sessions" {
            return None;
        }
        ("grok", marker_idx + 3)
    } else {
        let sessions = *comps.get(marker_idx + 2)?;
        if sessions != "sessions" {
            return None;
        }
        ("codex", marker_idx + 2)
    };
    let suffix: PathBuf = comps.iter().skip(sessions_idx + 1).collect();
    let store = transcript_store_root().ok()?;
    Some(durable_sessions_dir(&store, driver, run_id).ok()?.join(suffix))
}

fn looks_like_ephemeral_agent_home(path: &Path) -> bool {
    path.iter()
        .any(|c| c == GROK_HOMES_DIR_MARKER || c == CODEX_HOMES_DIR_MARKER || c == AGY_HOMES_DIR_MARKER)
}

/// Resolve and verify a provisioned `home/sessions` link against its
/// per-execution durable destination.
pub fn verified_durable_sessions_dir(
    home: &Path,
    store_root: &Path,
    driver: &str,
    run_id: &str,
) -> anyhow::Result<PathBuf> {
    verified_durable_link(&home.join("sessions"), store_root, driver, run_id)
}

/// Resolve and verify `link` against `driver`'s per-execution durable
/// destination for `run_id`. The general form of
/// [`verified_durable_sessions_dir`].
pub fn verified_durable_link(link: &Path, store_root: &Path, driver: &str, run_id: &str) -> anyhow::Result<PathBuf> {
    let metadata = fs::symlink_metadata(link).with_context(|| format!("stat transcript link {}", link.display()))?;
    if !metadata.file_type().is_symlink() {
        bail!("expected durable transcript link at {}", link.display());
    }
    let actual = fs::canonicalize(link).with_context(|| format!("resolving transcript link {}", link.display()))?;
    let destination = durable_sessions_dir(store_root, driver, run_id)?;
    let expected = fs::canonicalize(&destination)
        .with_context(|| format!("resolving durable transcript directory {}", destination.display()))?;
    if actual != expected {
        bail!(
            "transcript link {} targets {}; expected {}",
            link.display(),
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
        let _override = crate::test_support::transcript_store_override(&override_root);
        assert_eq!(transcript_store_root().unwrap(), override_root);
    }

    #[test]
    fn transcript_store_root_defaults_to_boss_executions_directory() {
        // Scoped-home fixtures temporarily clear TEST_TMPDIR. Keep the wrapper's
        // isolated root available while the default resolver reads it.
        let _home = crate::scoped_home::SCOPED_HOME_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _override = crate::test_support::TranscriptStoreOverride::unset();
        assert_eq!(
            transcript_store_root().unwrap(),
            boss_log_files::default_state_root().unwrap().join("executions")
        );
    }

    #[test]
    fn drivers_contain_transcripts_to_their_own_durable_sessions_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("executions");
        {
            let _codex_homes = crate::test_support::codex_homes_override(&tmp.path().join("codex-homes"));
            let _store = crate::test_support::transcript_store_override(&store);
            let codex_run_id = "codex-containment";
            let codex_home = codex_home_for_run(codex_run_id).unwrap();
            fs::create_dir_all(&codex_home).unwrap();
            let codex_expected = provision_durable_sessions(&codex_home, "codex", codex_run_id).unwrap();
            fs::remove_dir_all(&codex_home).unwrap();
            assert_eq!(
                CodexDriver::default()
                    .transcript_containment_root(codex_run_id)
                    .unwrap(),
                Some(fs::canonicalize(codex_expected).unwrap())
            );
        }

        {
            let _grok_homes = crate::test_support::grok_homes_override(&tmp.path().join("grok-homes"));
            let _store = crate::test_support::transcript_store_override(&store);
            let grok_run_id = "grok-containment";
            let grok_home = grok_home_for_run(grok_run_id).unwrap();
            fs::create_dir_all(&grok_home).unwrap();
            let grok_expected = provision_durable_sessions(&grok_home, "grok", grok_run_id).unwrap();
            fs::remove_dir_all(&grok_home).unwrap();
            assert_eq!(
                GrokDriver::default().transcript_containment_root(grok_run_id).unwrap(),
                Some(fs::canonicalize(grok_expected).unwrap())
            );
        }
    }

    #[test]
    fn persistable_path_leaves_claude_stamps_unchanged() {
        let claude = Path::new("/home/u/.claude/projects/foo/sess-1.jsonl");
        assert_eq!(persistable_transcript_path(claude), claude);
    }

    #[test]
    fn ephemeral_home_markers_include_agy_grok_and_codex() {
        assert!(looks_like_ephemeral_agent_home(
            &PathBuf::from("/tmp")
                .join(AGY_HOMES_DIR_MARKER)
                .join("run/process-home/x")
        ));
        assert!(looks_like_ephemeral_agent_home(
            &PathBuf::from("/tmp")
                .join(GROK_HOMES_DIR_MARKER)
                .join("run/grok-home/x")
        ));
        assert!(looks_like_ephemeral_agent_home(
            &PathBuf::from("/tmp")
                .join(CODEX_HOMES_DIR_MARKER)
                .join("run/sessions/x")
        ));
        assert!(!looks_like_ephemeral_agent_home(Path::new(
            "/home/u/.claude/projects/foo/sess-1.jsonl"
        )));
    }

    /// The stamped path of an `agy` conversation under `home`.
    fn agy_stamp(home: &Path) -> PathBuf {
        home.join(".gemini/antigravity-cli/brain/cid-1/.system_generated/logs/transcript_full.jsonl")
    }

    fn agy_brain(home: &Path) -> PathBuf {
        home.join(".gemini/antigravity-cli").join(AGY_BRAIN_LEAF)
    }

    #[test]
    fn agy_brain_link_retains_the_transcript_after_the_home_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("process-home");
        let store = tmp.path().join("executions");
        fs::create_dir_all(home.join(".gemini/antigravity-cli")).unwrap();
        fs::write(home.join(".gemini/antigravity-cli/settings.json"), "credential").unwrap();

        let durable = provision_durable_link_in(&agy_brain(&home), &store, AGY_TRANSCRIPT_DRIVER, "exec-agy").unwrap();
        assert_eq!(durable, store.join("exec-agy/transcripts/agy/sessions"));
        let stamped = agy_stamp(&home);
        fs::create_dir_all(stamped.parent().unwrap()).unwrap();
        fs::write(&stamped, "{\"step_index\":0}\n").unwrap();

        fs::remove_dir_all(&home).unwrap();
        assert_eq!(
            fs::read_to_string(durable.join("cid-1/.system_generated/logs/transcript_full.jsonl")).unwrap(),
            "{\"step_index\":0}\n",
            "the conversation transcript must outlive the reclaimed per-run home",
        );
    }

    #[test]
    fn persistable_path_rewrites_an_agy_stamp_through_the_brain_link() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("executions");
        let _store = crate::test_support::transcript_store_override(&store);
        let home = tmp.path().join("process-home");
        fs::create_dir_all(home.join(".gemini/antigravity-cli")).unwrap();
        let durable = provision_durable_link_in(&agy_brain(&home), &store, AGY_TRANSCRIPT_DRIVER, "exec-agy").unwrap();
        let stamped = agy_stamp(&home);
        fs::create_dir_all(stamped.parent().unwrap()).unwrap();
        fs::write(&stamped, "live\n").unwrap();

        let recorded = persistable_transcript_path(&stamped);

        assert_eq!(
            recorded,
            fs::canonicalize(&durable)
                .unwrap()
                .join("cid-1/.system_generated/logs/transcript_full.jsonl")
        );
        fs::remove_dir_all(&home).unwrap();
        assert_eq!(fs::read_to_string(&recorded).unwrap(), "live\n");
    }

    #[test]
    fn persistable_path_does_not_follow_an_agy_brain_link_that_leaves_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let _store = crate::test_support::transcript_store_override(&tmp.path().join("executions"));
        let home = tmp.path().join("process-home");
        let elsewhere = tmp.path().join("somewhere-else");
        fs::create_dir_all(home.join(".gemini/antigravity-cli")).unwrap();
        fs::create_dir_all(elsewhere.join("cid-1/.system_generated/logs")).unwrap();
        symlink_dir(&elsewhere, &agy_brain(&home)).unwrap();
        let stamped = agy_stamp(&home);
        fs::write(&stamped, "x\n").unwrap();

        assert_eq!(
            persistable_transcript_path(&stamped),
            stamped,
            "a redirected brain link must not become a recorded transcript path"
        );
    }

    #[test]
    fn persistable_path_rewrites_a_known_agy_layout_after_the_home_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("executions");
        let _store = crate::test_support::transcript_store_override(&store);
        let run_id = "exec-agy-backfill";
        let durable = durable_sessions_dir(&store, AGY_TRANSCRIPT_DRIVER, run_id)
            .unwrap()
            .join("cid-1/.system_generated/logs/transcript_full.jsonl");
        fs::create_dir_all(durable.parent().unwrap()).unwrap();
        fs::write(&durable, "durable\n").unwrap();

        let stamped = PathBuf::from(format!(
            "/var/folders/xx/T/{AGY_HOMES_DIR_MARKER}/{run_id}/process-home/.gemini/antigravity-cli/brain/cid-1/.system_generated/logs/transcript_full.jsonl"
        ));
        assert!(!stamped.exists(), "precondition: ephemeral stamp is already gone");
        assert_eq!(persistable_transcript_path(&stamped), durable);
    }

    #[test]
    fn persistable_path_leaves_a_legacy_agy_brain_directory_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let _store = crate::test_support::transcript_store_override(&tmp.path().join("executions"));
        let home = tmp.path().join(AGY_HOMES_DIR_MARKER).join("exec-legacy/process-home");
        let stamped = agy_stamp(&home);
        fs::create_dir_all(stamped.parent().unwrap()).unwrap();
        fs::write(&stamped, "only copy\n").unwrap();

        assert_eq!(
            persistable_transcript_path(&stamped),
            stamped,
            "a home provisioned before the durable store existed still holds the only copy"
        );
    }

    #[test]
    fn agy_layout_rewrite_ignores_paths_that_are_not_the_brain_tree() {
        let store = tempfile::tempdir().unwrap();
        let _store = crate::test_support::transcript_store_override(store.path());
        let conversations = PathBuf::from(format!(
            "/var/folders/xx/T/{AGY_HOMES_DIR_MARKER}/exec-1/process-home/.gemini/antigravity-cli/conversations/cid-1.db"
        ));
        assert_eq!(persistable_transcript_path(&conversations), conversations);
    }

    #[test]
    fn persistable_path_rewrites_a_known_grok_layout_after_the_home_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("executions");
        let _store = crate::test_support::transcript_store_override(&store);
        let run_id = "exec-backfill";
        let durable = durable_sessions_dir(&store, "grok", run_id)
            .unwrap()
            .join("%2Ftmp")
            .join("sess-1")
            .join("updates.jsonl");
        fs::create_dir_all(durable.parent().unwrap()).unwrap();
        fs::write(&durable, "durable\n").unwrap();

        let stamped = PathBuf::from(format!(
            "/var/folders/xx/T/{GROK_HOMES_DIR_MARKER}/{run_id}/grok-home/sessions/%2Ftmp/sess-1/updates.jsonl"
        ));
        assert!(!stamped.exists(), "precondition: ephemeral stamp is already gone");
        assert_eq!(persistable_transcript_path(&stamped), durable);
    }

    #[test]
    fn persistable_path_rewrites_a_known_codex_layout_after_the_home_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("executions");
        let _store = crate::test_support::transcript_store_override(&store);
        let run_id = "exec-codex-backfill";
        let durable = durable_sessions_dir(&store, "codex", run_id)
            .unwrap()
            .join("2026")
            .join("09")
            .join("04")
            .join("rollout.jsonl");
        fs::create_dir_all(durable.parent().unwrap()).unwrap();
        fs::write(&durable, "rollout\n").unwrap();

        let stamped = PathBuf::from(format!(
            "/var/folders/xx/T/{CODEX_HOMES_DIR_MARKER}/{run_id}/sessions/2026/09/04/rollout.jsonl"
        ));
        assert!(!stamped.exists(), "precondition: ephemeral stamp is already gone");
        assert_eq!(persistable_transcript_path(&stamped), durable);
    }

    /// The recorded Grok transcript path must still resolve after the
    /// per-run home is reclaimed. Reclaim is the lifecycle event that
    /// made 95/97 sampled grok rows point at a directory that no longer
    /// exists; a helper-only assertion would miss that.
    #[test]
    fn recorded_grok_transcript_path_survives_home_reclaim() {
        let tmp = tempfile::tempdir().unwrap();
        let homes = tmp.path().join(GROK_HOMES_DIR_MARKER);
        let store = tmp.path().join("executions");
        let _homes = crate::test_support::grok_homes_override(&homes);
        let _store = crate::test_support::transcript_store_override(&store);

        let run_id = "exec-lifecycle";
        let grok_home = grok_home_for_run(run_id).unwrap();
        fs::create_dir_all(&grok_home).unwrap();
        provision_durable_sessions(&grok_home, "grok", run_id).unwrap();

        let stamped = grok_home
            .join("sessions")
            .join("%2Ftmp%2Fws")
            .join("sess-1")
            .join("updates.jsonl");
        fs::create_dir_all(stamped.parent().unwrap()).unwrap();
        fs::write(&stamped, "{\"session\":\"live\"}\n").unwrap();

        let raw = serde_json::json!({
            "sessionId": "sess-1",
            "hookEventName": "stop",
            "transcriptPath": stamped.to_string_lossy(),
        });
        let recorded = GrokDriver::default()
            .transcript_path_for_session(&raw)
            .expect("Grok stamps transcriptPath on Stop");

        assert_ne!(
            Path::new(&recorded),
            stamped.as_path(),
            "recorded path must not be the ephemeral GROK_HOME stamp"
        );

        let container = grok_home.parent().expect("grok-home lives in a run container");
        crate::grok::reclaim_grok_home(container).unwrap();
        assert!(!stamped.exists(), "precondition: reclaim deleted the ephemeral stamp");

        let recorded_path = Path::new(&recorded);
        assert!(
            recorded_path.is_file(),
            "recorded transcript_path must still resolve after reclaim: {recorded}"
        );
        assert_eq!(fs::read_to_string(recorded_path).unwrap(), "{\"session\":\"live\"}\n");
    }
}
