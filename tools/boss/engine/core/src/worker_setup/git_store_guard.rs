//! Materialisation of the shared-source-store write gate
//! (`boss-git-store-guard.py`) next to the worker settings file.
//!
//! The script itself lives in the driver crate
//! ([`boss_engine_driver::git_store_guard`]) because all three drivers run
//! the same bytes; this module only owns how the engine lays it on disk —
//! content-addressed and write-once, exactly like the path guard and the
//! checkleft push guard — so a Codex worker that attested one build's bytes
//! can never be bricked by another build writing different bytes to the same
//! path. Split out of `worker_setup` (which sits at the repo's file-size
//! limit) to keep the module boundary reviewable.

use std::io;
use std::path::{Path, PathBuf};

pub use boss_engine_driver::git_store_guard::{GIT_STORE_GUARD_SCRIPT, GIT_STORE_GUARD_SCRIPT_NAME};

/// Directory-name prefix for a content-addressed materialisation
/// (`git-store-guard-<sha256>/boss-git-store-guard.py`). Distinct from the
/// other guards' prefixes so each script can change independently.
pub(crate) const GIT_STORE_GUARD_KIND: &str = "git-store-guard";

/// Absolute path to this engine build's shared-source-store gate script,
/// under [`super::worker_settings_dir`]. Content-addressed on the script
/// bytes like [`super::path_guard_script_path`].
pub fn git_store_guard_script_path() -> PathBuf {
    git_store_guard_script_path_in(&super::worker_settings_dir())
}

/// [`git_store_guard_script_path`] resolved under `dir` instead of the
/// shared worker-settings dir.
pub(super) fn git_store_guard_script_path_in(dir: &Path) -> PathBuf {
    super::content_addressed_guard_path(
        dir,
        GIT_STORE_GUARD_KIND,
        GIT_STORE_GUARD_SCRIPT_NAME,
        GIT_STORE_GUARD_SCRIPT.as_bytes(),
    )
}

/// Write the gate script into a content-addressed directory under `dir`.
/// Same write-once contract as [`super::ensure_path_guard_script_in`].
pub fn ensure_git_store_guard_script_in(dir: &Path) -> io::Result<PathBuf> {
    super::ensure_content_addressed_script(
        dir,
        GIT_STORE_GUARD_KIND,
        GIT_STORE_GUARD_SCRIPT_NAME,
        GIT_STORE_GUARD_SCRIPT.as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    //! Engine-side wiring of the shared-source-store write gate: the script is
    //! materialised content-addressed next to the settings file, every local
    //! worker kind's settings carry the hook, and its refusals read cleanly.
    //! The decision matrix itself lives with the script in
    //! `boss_engine_driver::git_store_guard`.

    use super::super::tests::helpers::*;
    use super::super::*;
    use super::*;

    /// Locate the shared-source-store hook command in rendered settings.
    fn git_store_guard_command(parsed: &serde_json::Value) -> Option<(usize, String)> {
        parsed["hooks"]["PreToolUse"]
            .as_array()?
            .iter()
            .enumerate()
            .find_map(|(index, e)| {
                let command = e["hooks"][0]["command"].as_str()?;
                command
                    .contains(GIT_STORE_GUARD_SCRIPT_NAME)
                    .then(|| (index, command.to_owned()))
            })
    }

    #[test]
    fn git_store_guard_denials_avoid_internal_vocabulary() {
        for name in ["MALFORMED", "WRITE_RECOVERY", "SHALLOW_RECOVERY", "UNRESOLVED_RECOVERY"] {
            let prefix = format!("{name} = (\n");
            let body = GIT_STORE_GUARD_SCRIPT
                .split_once(&prefix)
                .unwrap_or_else(|| panic!("{name} message missing"))
                .1
                .split_once("\n)")
                .unwrap()
                .0;
            let mut message = String::new();
            for line in body.lines() {
                message.push_str(
                    &serde_json::from_str::<String>(line.trim()).unwrap_or_else(|e| panic!("{name}: {e}: {line}")),
                );
            }
            crate::prose_vocabulary::assert_no_forbidden_prose(name, &message);
        }
    }

    #[test]
    fn settings_json_adds_git_store_guard_hook_after_the_path_guard() {
        let input = sample_input();
        let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input, &ClaudeDriver)).unwrap();
        let (index, cmd) =
            git_store_guard_command(&parsed).expect("PreToolUse must include the shared-source-store guard hook");
        // Forwarder shim first, path guard second, this gate third: the two
        // boundary gates come before every posture-specific guard.
        assert_eq!(index, 2, "{parsed}");
        assert_eq!(parsed["hooks"]["PreToolUse"][index]["matcher"], "Bash");
        assert!(cmd.contains("python3"), "guard must run via python3: {cmd}");
        assert!(
            cmd.contains(&format!(
                "BOSS_WORKSPACE={}",
                shell_quote(&input.workspace_path.display().to_string())
            )),
            "guard must be told the leased workspace so it can find the repos root: {cmd}",
        );
        let script = git_store_guard_script_path();
        assert!(
            cmd.contains(&shell_quote(&script.display().to_string())),
            "guard must invoke the absolute content-addressed script path: {cmd}",
        );
        assert!(
            script
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n.to_string_lossy().starts_with(&format!("{GIT_STORE_GUARD_KIND}-"))),
            "script must live in a content-addressed directory: {}",
            script.display()
        );
    }

    #[test]
    fn git_store_guard_is_armed_for_every_local_worker_kind() {
        // The shared store is a boundary like the Boss data dir, not a posture:
        // reviewers, triage, answer agents and review guides get it too. The
        // review guide's pinned reads are exactly the explicit `--git-dir`
        // read-only form the gate permits.
        for (kind, execution_kind) in [
            (WorkerKind::Standard, "chore_implementation"),
            (WorkerKind::Standard, "revision_implementation"),
            (WorkerKind::Reviewer, "pr_review"),
            (WorkerKind::Triage, "automation_triage"),
            (WorkerKind::AnswerAgent, "answer_agent"),
            (WorkerKind::ReviewGuide, "pr_review_guide"),
        ] {
            let mut input = sample_input();
            input.worker_kind = kind.clone();
            input.execution_kind = execution_kind.into();
            let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input, &ClaudeDriver)).unwrap();
            assert!(
                git_store_guard_command(&parsed).is_some(),
                "{kind:?}/{execution_kind} must carry the shared-source-store guard: {parsed}"
            );
        }
    }

    #[test]
    fn remote_settings_omit_the_git_store_guard() {
        // The script is never shipped to a remote host, so remote settings must
        // not reference a path that does not exist there.
        let input = sample_input();
        let parsed: serde_json::Value =
            serde_json::from_str(&render_remote_settings_json(&input, &ClaudeDriver)).unwrap();
        assert!(
            git_store_guard_command(&parsed).is_none(),
            "remote settings must not carry the local gate script: {parsed}"
        );
    }

    #[test]
    fn write_workspace_files_materialises_the_git_store_guard_outside_the_workspace() {
        let _shared = lock_shared_settings_dir();
        let _home = ClaudeConfigGuard::new();
        let dir = TempDir::new().unwrap();
        let mut input = sample_input();
        input.workspace_path = dir.path().to_path_buf();
        let written = write_workspace_files(&input, &ClaudeDriver).unwrap();
        let script = git_store_guard_script_path();
        assert!(
            script.is_file(),
            "{} must exist after writing settings",
            script.display()
        );
        assert_eq!(
            std::fs::read_to_string(&script).unwrap(),
            GIT_STORE_GUARD_SCRIPT,
            "materialised bytes must be this build's script"
        );
        assert!(
            !script.starts_with(dir.path()),
            "the gate must live outside the workspace tree: {}",
            script.display()
        );
        let settings = std::fs::read_to_string(&written.settings_path).unwrap();
        assert!(
            settings.contains(&shell_quote(&script.display().to_string())),
            "settings must point at the materialised script: {settings}"
        );
    }

    #[test]
    fn content_addressed_git_store_guard_dirs_are_recognised_as_referenced() {
        // The prune sweep keeps any hashed guard dir a settings file still names;
        // the new kind must be part of that recognition or a live worker's gate
        // could be swept out from under it.
        let dir = TempDir::new().unwrap();
        let path = ensure_git_store_guard_script_in(dir.path()).unwrap();
        let hashed_dir = path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(is_content_addressed_guard_dir_name(&hashed_dir, GIT_STORE_GUARD_KIND));
        std::fs::write(
            dir.path().join("ws.json"),
            format!(
                "{{\"hooks\":{{\"PreToolUse\":[{{\"command\":\"python3 '{}'\"}}]}}}}",
                path.display()
            ),
        )
        .unwrap();
        assert!(
            referenced_guard_dir_names(dir.path()).contains(&hashed_dir),
            "the settings reference must keep {hashed_dir} alive"
        );
    }

    #[test]
    fn git_store_guard_script_has_the_load_bearing_logic() {
        let s = GIT_STORE_GUARD_SCRIPT;
        assert!(
            s.contains("BOSS_WORKSPACE"),
            "must learn the workspace from the hook env"
        );
        assert!(
            s.contains(".jj"),
            "must derive the repos root from the .jj/repo pointer"
        );
        assert!(
            s.contains("GIT_DIR") && s.contains("--git-dir") && s.contains("-C"),
            "must follow every way git is pointed at a repo"
        );
        assert!(
            s.contains("--depth") && s.contains("--shallow-since") && s.contains("--update-shallow"),
            "must know the shallow options"
        );
        assert!(
            s.contains("\"decision\"") && s.contains("\"block\""),
            "must be able to emit a block decision"
        );
        assert!(
            s.contains("jj git fetch") && s.contains("throwaway clone"),
            "refusals must redirect to jj / a throwaway clone"
        );
        assert!(s.contains("realpath"), "must compare the symlink-resolved spelling too");
    }
}
