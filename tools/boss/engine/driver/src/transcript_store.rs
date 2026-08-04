//! Durable, transcript-only storage for isolated agent homes.
//!
//! Claude already keeps conversations in `$HOME/.claude/projects`; Codex and
//! Grok use that same durable store for their session JSONL while retaining
//! credentials, hooks, and all other per-run state in their temporary homes.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

/// Test/operator override for the existing Claude transcript-store root.
/// Production uses `$HOME/.claude/projects`.
pub const WORKER_TRANSCRIPTS_ROOT_ENV: &str = "BOSS_WORKER_TRANSCRIPTS_ROOT";

/// Resolve the existing durable transcript store used by Claude Code.
pub fn transcript_store_root() -> anyhow::Result<PathBuf> {
    if let Some(root) = std::env::var_os(WORKER_TRANSCRIPTS_ROOT_ENV).filter(|root| !root.is_empty()) {
        return Ok(PathBuf::from(root));
    }
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or_else(|| anyhow::anyhow!("HOME is unset; cannot retain worker transcript in ~/.claude/projects"))?;
    Ok(PathBuf::from(home).join(".claude").join("projects"))
}

/// Make `home/sessions` a link to a dedicated transcript-only directory in
/// Claude's existing transcript store. The destination is namespaced by the
/// workspace and run so neither driver can collide with a Claude session.
///
/// This is deliberately a provisioning error, not a best-effort completion
/// action: a killed or orphaned worker is still writing to the durable file,
/// and a failed setup is visible before the worker starts.
pub fn provision_durable_sessions(
    home: &Path,
    driver: &str,
    workspace: &Path,
    run_id: &str,
) -> anyhow::Result<PathBuf> {
    let root = transcript_store_root()?;
    provision_durable_sessions_in(home, &root, driver, workspace, run_id)
}

/// Injectable form of [`provision_durable_sessions`] for tests.
pub fn provision_durable_sessions_in(
    home: &Path,
    store_root: &Path,
    driver: &str,
    workspace: &Path,
    run_id: &str,
) -> anyhow::Result<PathBuf> {
    let driver = safe_segment(driver, "driver")?;
    let run_id = safe_segment(run_id, "run_id")?;
    let workspace_slug = workspace_slug(workspace)?;
    let destination = store_root
        .join(workspace_slug)
        .join(format!("boss-{driver}-{run_id}"))
        .join("sessions");
    fs::create_dir_all(&destination)
        .with_context(|| format!("creating durable worker transcript directory {}", destination.display()))?;

    let sessions = home.join("sessions");
    match fs::symlink_metadata(&sessions) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let actual = fs::canonicalize(&sessions)
                .with_context(|| format!("resolving existing sessions link {}", sessions.display()))?;
            let expected = fs::canonicalize(&destination)
                .with_context(|| format!("resolving durable transcript directory {}", destination.display()))?;
            if actual != expected {
                bail!(
                    "refusing to replace sessions link {} targeting {}; expected {}",
                    sessions.display(),
                    actual.display(),
                    expected.display()
                );
            }
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

fn workspace_slug(workspace: &Path) -> anyhow::Result<String> {
    let value = workspace.to_string_lossy();
    let slug = value.replace(['/', '\\'], "-");
    if slug.is_empty() || slug == "-" {
        bail!(
            "workspace path has no usable Claude transcript slug: {}",
            workspace.display()
        );
    }
    Ok(slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_link_retains_only_the_transcript_subtree() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("temporary-home");
        let store = tmp.path().join("claude-projects");
        fs::create_dir_all(home.join("sessions")).unwrap();
        fs::write(home.join("auth.json"), "credential").unwrap();

        let durable =
            provision_durable_sessions_in(&home, &store, "codex", Path::new("/work/project"), "exec-run").unwrap();
        fs::write(home.join("sessions/rollout.jsonl"), "full conversation\n").unwrap();

        assert_eq!(
            fs::read_to_string(durable.join("rollout.jsonl")).unwrap(),
            "full conversation\n"
        );
        assert!(home.join("auth.json").is_file());
        fs::remove_dir_all(&home).unwrap();
        assert!(durable.join("rollout.jsonl").is_file());
    }
}
