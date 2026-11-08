//! Production [`CubeClient`]: shells out to the `cube` CLI through the
//! engine's configured agent command.
//!
//! This is concrete cube I/O, not dispatch policy, so it lives with the
//! engine runtime rather than with the dispatch coordinator that consumes it
//! through the [`CubeClient`] contract.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use boss_execution_contracts::{CubeClient, LifecycleServices, RebaseOutcome};
use boss_host_execution::types::{
    CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
};

use crate::config::RuntimeConfig;

#[derive(Clone)]
pub struct CommandCubeClient {
    cfg: Arc<RuntimeConfig>,
    lifecycle: Arc<dyn LifecycleServices>,
}

impl std::fmt::Debug for CommandCubeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandCubeClient")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

/// Strip the trailing jj conflict-type descriptor (e.g. `"    2-sided
/// conflict"` or `"    2-sided conflict including 1 deletion"`) that `jj
/// resolve --list` glues onto each path, column-aligned with whitespace
/// padding. Paths can themselves contain spaces, so this deliberately
/// anchors on the `<N>-sided conflict` marker rather than splitting on
/// whitespace: it finds the marker, walks back over the digit run that
/// precedes it, and treats everything before that (whitespace-trimmed) as
/// the path. Lines that don't match the pattern (already-bare paths, or a
/// future jj format change) are returned unchanged.
fn strip_jj_conflict_descriptor(line: &str) -> &str {
    const MARKER: &str = "-sided conflict";
    let trimmed = line.trim();
    let Some(marker_idx) = trimmed.rfind(MARKER) else {
        return trimmed;
    };
    let digits_start = trimmed[..marker_idx]
        .rfind(|c: char| !c.is_ascii_digit())
        .map(|i| i + 1)
        .unwrap_or(0);
    if digits_start == marker_idx {
        // No digits immediately before the marker — not actually the
        // descriptor we're looking for.
        return trimmed;
    }
    let path = trimmed[..digits_start].trim_end();
    if path.is_empty() { trimmed } else { path }
}

/// Parse `cube workspace rebase --json`'s `payload` object
/// (`{status, pushed, conflicted_files, ...}`) into a [`RebaseOutcome`].
/// Shared by [`CommandCubeClient::rebase_workspace`] and
/// [`CommandCubeClient::rebase_workspace_no_push`] — the two commands differ
/// only by the `--no-push` flag, not by output shape. `conflicted_files`
/// entries come from `jj resolve --list` and may carry a trailing
/// conflict-type descriptor after the path (see
/// [`strip_jj_conflict_descriptor`]); that descriptor is stripped here so
/// every downstream consumer of [`RebaseOutcome::conflicted_files`] sees
/// bare paths.
fn parse_rebase_payload(payload: serde_json::Value) -> Result<RebaseOutcome> {
    let status = payload
        .get("status")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("cube workspace rebase returned no `status` field: {payload}"))?;
    let clean = match status {
        "clean" => true,
        "conflicts" => false,
        other => return Err(anyhow!("cube workspace rebase returned unexpected status `{other}`")),
    };
    let pushed = payload.get("pushed").and_then(|v| v.as_bool()).unwrap_or(false);
    // Absent on a cube older than the replay-only collapse; 0 is the correct
    // reading there (that cube never collapsed anything).
    let linearized_commits = payload.get("linearized_commits").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    // Absent on a clean rebase (nothing declined) and on a cube older than the
    // decline-reason plumbing.
    let linearize_decline = payload
        .get("linearize_decline")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let conflicted_files = payload
        .get("conflicted_files")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| strip_jj_conflict_descriptor(s).to_owned())
                .collect()
        })
        .unwrap_or_default();
    Ok(RebaseOutcome {
        clean,
        pushed,
        conflicted_files,
        linearized_commits,
        linearize_decline,
    })
}

impl CommandCubeClient {
    /// `lifecycle` supplies the merge-parent deletion tripwire.
    pub fn new(cfg: Arc<RuntimeConfig>, lifecycle: Arc<dyn LifecycleServices>) -> Self {
        Self { cfg, lifecycle }
    }

    async fn run_json(&self, args: &[&str]) -> Result<serde_json::Value> {
        let cwd = self.cfg.work.cwd.clone();
        self.run_json_in_dir(&cwd, args).await
    }

    /// Like [`Self::run_json`] but runs the cube subprocess with an explicit
    /// working directory. Required for cwd-sensitive subcommands such as
    /// `cube workspace rebase`, which resolves the target workspace from the
    /// current directory rather than a `--workspace` flag.
    async fn run_json_in_dir(&self, cwd: &Path, args: &[&str]) -> Result<serde_json::Value> {
        let agent = self.cfg.agent()?;
        boss_host_execution::cube::LocalCube {
            command: agent.cube.command.clone().into(),
            args: agent.cube.args.clone(),
            cwd: cwd.to_path_buf(),
        }
        .run(args)
        .await
    }
}

#[async_trait]
impl crate::cube_commands::CubeJsonTransport for CommandCubeClient {
    async fn run_cube_json(&self, args: &[&str]) -> Result<serde_json::Value> {
        self.run_json(args).await
    }
}

#[async_trait]
impl CubeClient for CommandCubeClient {
    async fn create_execution_bookmark(
        &self,
        workspace: &Path,
        execution_id: &str,
        predecessor: Option<&boss_engine_recovery::execution_bookmark::ExecutionBookmark>,
        inherited_base: Option<&str>,
    ) -> Result<boss_engine_recovery::execution_bookmark::ExecutionBookmark> {
        boss_engine_recovery::execution_bookmark::create_from(
            &boss_engine_recovery::execution_bookmark::LocalJj,
            workspace,
            execution_id,
            "local",
            predecessor,
            inherited_base,
        )
        .await
    }
    async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle> {
        crate::cube_commands::ensure_repo(self, origin).await
    }

    async fn lease_workspace(
        &self,
        repo_id: &str,
        task: &str,
        prefer_workspace_id: Option<&str>,
        allow_dirty: bool,
        exclude_workspace_ids: &[&str],
    ) -> Result<CubeWorkspaceLease> {
        crate::cube_commands::lease_workspace(
            self,
            repo_id,
            task,
            prefer_workspace_id,
            allow_dirty,
            exclude_workspace_ids,
        )
        .await
    }

    async fn create_change(&self, workspace_path: &Path, title: &str) -> Result<CubeChangeHandle> {
        crate::cube_commands::create_change(self, workspace_path, title).await
    }

    async fn release_workspace(&self, lease_id: &str) -> Result<()> {
        crate::cube_commands::release_workspace(self, lease_id).await
    }

    async fn release_workspace_with_reset(&self, lease_id: &str) -> Result<()> {
        crate::cube_commands::release_workspace_with_reset(self, lease_id).await
    }

    async fn workspace_status(&self, workspace_path: &Path) -> Result<CubeWorkspaceStatus> {
        crate::cube_commands::workspace_status(self, workspace_path).await
    }

    async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()> {
        crate::cube_commands::heartbeat_lease(self, lease_id, ttl_seconds).await
    }

    async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()> {
        crate::cube_commands::force_release_lease(self, lease_id, reason).await
    }

    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        crate::cube_commands::list_workspaces(self).await
    }

    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        crate::cube_commands::list_repos(self).await
    }

    async fn goto_workspace(&self, workspace_path: &Path, pr: u64) -> Result<()> {
        crate::cube_commands::goto_workspace(self, workspace_path, "--pr", &pr.to_string()).await
    }

    async fn goto_workspace_revision(&self, workspace_path: &Path, sha: &str) -> Result<()> {
        crate::cube_commands::goto_workspace(self, workspace_path, "--revision", sha).await
    }

    async fn rebase_workspace(&self, workspace_path: &Path, pr: u64) -> Result<RebaseOutcome> {
        let pr_str = pr.to_string();
        // `cube workspace rebase` resolves the workspace from the current
        // directory (no `--workspace` flag), so it must run inside the leased
        // workspace. Without `--no-push`, a clean rebase advances and pushes
        // the boss bookmark itself. The `--json` output is the command's
        // `payload` object (`{status, pushed, conflicted_files, ...}`).
        let payload = self
            .run_json_in_dir(workspace_path, &["--json", "workspace", "rebase", "--pr", &pr_str])
            .await?;
        parse_rebase_payload(payload)
    }

    async fn rebase_workspace_no_push(&self, workspace_path: &Path, pr: u64) -> Result<RebaseOutcome> {
        let pr_str = pr.to_string();
        // Same command as `rebase_workspace` plus `--no-push`: a clean
        // rebase never advances/pushes the boss bookmark, so the real PR
        // branch is untouched. Used for the speculative/throwaway rebase
        // (the conflict-against-main result-gate layer) — the caller only
        // wants to observe whether the PR would conflict against current
        // `main`.
        let payload = self
            .run_json_in_dir(
                workspace_path,
                &["--json", "workspace", "rebase", "--pr", &pr_str, "--no-push"],
            )
            .await?;
        parse_rebase_payload(payload)
    }

    async fn push_resolution(&self, workspace_path: &Path, pr: u64) -> Result<()> {
        let pr_str = pr.to_string();
        // `cube workspace push` resolves the workspace from the current
        // directory (no `--workspace` flag), so it must run inside the
        // leased workspace, same as `rebase_workspace` above.
        let payload = self
            .run_json_in_dir(workspace_path, &["--json", "workspace", "push", "--pr", &pr_str])
            .await?;
        let pushed = payload.get("pushed").and_then(|v| v.as_bool()).unwrap_or(false);
        if !pushed {
            return Err(anyhow!("cube workspace push did not report pushed=true: {payload}"));
        }
        Ok(())
    }

    async fn verify_deletion_tripwire(
        &self,
        repo_slug: &str,
        head_before: &str,
        base_sha: &str,
        pr_number: u64,
    ) -> Vec<String> {
        self.lifecycle
            .verify_deletion_tripwire(repo_slug, head_before, base_sha, pr_number)
            .await
    }

    fn command_repr(&self, args: &[&str]) -> Option<(String, String)> {
        let Ok(agent) = self.cfg.agent() else { return None };
        let cmd = std::iter::once(agent.cube.command.as_str())
            .chain(agent.cube.args.iter().map(String::as_str))
            .chain(args.iter().copied())
            .map(crate::ssh_transport::shell_quote)
            .collect::<Vec<_>>()
            .join(" ");
        let cwd = self.cfg.work.cwd.display().to_string();
        Some((cmd, cwd))
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_rebase_payload, strip_jj_conflict_descriptor};

    /// Reproduces the live incident (flunge, PR brianduff/flunge#906,
    /// 2026-07-16): rung 1's `conflicted_files` come straight from `jj
    /// resolve --list`, whose entries carry a trailing conflict-type
    /// descriptor glued onto the path (e.g. `"MODULE.bazel.lock    2-sided
    /// conflict"`). Left unstripped, rung 0's resolver lookup does a
    /// `file_name()` match against the whole annotated string and never
    /// matches, so the batch gets declined and escalated to an agent even
    /// though a resolver for the bare filename exists.
    #[test]
    fn strip_jj_conflict_descriptor_removes_trailing_annotation() {
        assert_eq!(
            strip_jj_conflict_descriptor("MODULE.bazel.lock    2-sided conflict"),
            "MODULE.bazel.lock",
        );
        // Descriptor can carry extra detail past the sided-conflict marker.
        assert_eq!(
            strip_jj_conflict_descriptor("f.txt    2-sided conflict including 1 deletion"),
            "f.txt",
        );
        // Paths with internal spaces must not be truncated at the first space.
        assert_eq!(
            strip_jj_conflict_descriptor("a b long name.txt    2-sided conflict"),
            "a b long name.txt",
        );
        // Already-bare paths (or an unrecognized future format) pass through.
        assert_eq!(strip_jj_conflict_descriptor("MODULE.bazel.lock"), "MODULE.bazel.lock");
        assert_eq!(strip_jj_conflict_descriptor("  spaced.txt  "), "spaced.txt");
    }

    #[test]
    fn parse_rebase_payload_strips_conflict_descriptors_from_conflicted_files() {
        let payload = serde_json::json!({
            "status": "conflicts",
            "pushed": false,
            "conflicted_files": [
                "MODULE.bazel.lock    2-sided conflict",
                "a b long name.txt    3-sided conflict including 1 deletion",
            ],
        });
        let outcome = parse_rebase_payload(payload).expect("payload parses");
        assert!(!outcome.clean);
        assert!(!outcome.pushed);
        assert_eq!(
            outcome.conflicted_files,
            vec!["MODULE.bazel.lock".to_owned(), "a b long name.txt".to_owned()],
        );
        assert_eq!(
            outcome.linearized_commits, 0,
            "the conflict path never collapses anything"
        );
    }

    /// `linearized_commits` carries cube's replay-only collapse count through to
    /// the ladder. A clean rebase that only became pushable because cube
    /// collapsed conflicted *ancestors* (after proving the pushed tree
    /// unchanged) is still a rung-1 success, but the trace must be able to tell
    /// the two apart — see `RebaseOutcome::linearized_commits`.
    #[test]
    fn parse_rebase_payload_carries_the_replay_only_collapse_count() {
        let payload = serde_json::json!({
            "status": "clean",
            "pushed": true,
            "conflicted_files": [],
            "linearized_commits": 2,
        });
        let outcome = parse_rebase_payload(payload).expect("payload parses");
        assert!(outcome.clean);
        assert!(outcome.pushed);
        assert_eq!(outcome.linearized_commits, 2);
    }

    /// A cube predating the collapse omits the field entirely; 0 is the correct
    /// reading (that cube never collapsed anything), not a parse failure.
    #[test]
    fn parse_rebase_payload_defaults_the_collapse_count_when_cube_omits_it() {
        let payload = serde_json::json!({
            "status": "clean",
            "pushed": true,
            "conflicted_files": [],
        });
        let outcome = parse_rebase_payload(payload).expect("payload parses");
        assert!(outcome.clean);
        assert_eq!(outcome.linearized_commits, 0);
        assert_eq!(
            outcome.linearize_decline, None,
            "an older cube reports no decline reason, which must parse as absent rather than fail"
        );
    }

    /// The decline reason cube stamps on the conflict payload must reach the
    /// ladder: without it every rung-1 fall-through looks alike in the trace and
    /// guard hits cannot be counted.
    #[test]
    fn parse_rebase_payload_carries_the_linearize_decline_reason() {
        let payload = serde_json::json!({
            "status": "conflicts",
            "pushed": false,
            "conflicted_files": ["src/a.rs"],
            "linearized_commits": 0,
            "linearize_decline": "tree_drift",
        });
        let outcome = parse_rebase_payload(payload).expect("payload parses");
        assert!(!outcome.clean);
        assert_eq!(outcome.linearize_decline.as_deref(), Some("tree_drift"));
    }
}
