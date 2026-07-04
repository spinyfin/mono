//! Shared implementation of the `cube` CLI command surface.
//!
//! Both [`CommandCubeClient`](crate::coordinator::CommandCubeClient) (which
//! shells out to a local `cube` binary) and
//! [`SshHostAdapter`](crate::host_adapter::SshHostAdapter) (which runs `cube`
//! over SSH on a remote host) need to invoke the same handful of cube
//! subcommands — `repo ensure`, `workspace lease`, `change create`,
//! `workspace release/status/heartbeat/force-release/list`, and `repo list`.
//!
//! Each of those is identical apart from the transport used to actually run
//! the command and collect its JSON output. To avoid maintaining two copies
//! of the argument-building and JSON-decoding logic, the command bodies live
//! here as free functions generic over a [`CubeJsonTransport`], and the two
//! `impl` blocks become thin wrappers that delegate to these helpers.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::coordinator::{CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus};

/// A failed `cube` CLI invocation that preserves the structured signals a
/// post-mortem needs — the process exit code and the captured
/// stderr/stdout — instead of collapsing them into an opaque string.
///
/// Motivation (the anaplian failure-mode A): a remote `cube workspace
/// lease` GRANTED the lease, then a post-lease setup step exited non-zero;
/// the engine surfaced this only as `reason: "cube_error"` in the dispatch
/// stream, so the real cause (`setup step copy-config-secrets failed …`)
/// was flattened into a free-text `error_message` an operator had to dig
/// out with `dispatch diagnose`. The remote SSH adapter returns this typed
/// error on any non-zero cube exit; the `cube_workspace_lease_failed` emit
/// sites downcast to it and copy `exit_code`/`stderr` straight into the
/// event `details`, so the next remote-host failure is attributable in one
/// read.
#[derive(Debug, Clone)]
pub struct CubeCliError {
    /// Host the cube command ran on (`"local"` or a remote host id).
    pub host: String,
    /// Cube's process exit code, or `None` when the process was killed by
    /// a signal / the transport could not determine one.
    pub exit_code: Option<i32>,
    /// Captured stderr (may be empty).
    pub stderr: String,
    /// Captured stdout (may be empty).
    pub stdout: String,
}

impl CubeCliError {
    /// The single-line human detail: trimmed stderr, else trimmed stdout,
    /// else a synthetic exit-status clause. Matches the message the SSH
    /// adapter historically built so `error_message` and logs are byte
    /// identical after this change.
    pub fn detail(&self) -> String {
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return stderr.to_owned();
        }
        let stdout = self.stdout.trim();
        if !stdout.is_empty() {
            return stdout.to_owned();
        }
        match self.exit_code {
            Some(code) => format!("exit status {code}"),
            None => "exit status unknown (killed by signal?)".to_owned(),
        }
    }

    /// Trimmed stderr clipped to at most `max` bytes (on a char boundary)
    /// for embedding in a dispatch-event `details` object without bloating
    /// the JSONL when a remote step dumps a large trace.
    pub fn clipped_stderr(&self, max: usize) -> String {
        let stderr = self.stderr.trim();
        if stderr.len() <= max {
            return stderr.to_owned();
        }
        let mut end = max;
        while end > 0 && !stderr.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}… (clipped)", &stderr[..end])
    }
}

impl fmt::Display for CubeCliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ssh cube command failed on host {}: {}", self.host, self.detail())
    }
}

impl std::error::Error for CubeCliError {}

/// A transport capable of running a `cube` invocation that emits JSON on
/// stdout and returning the parsed value.
///
/// The local client runs `cube` as a child process; the SSH adapter runs it
/// on a remote host. Both decode stdout into a [`serde_json::Value`]; the
/// command helpers below own everything past that point.
#[async_trait]
pub trait CubeJsonTransport: Send + Sync {
    /// Run `cube <args>` and return its parsed JSON stdout.
    async fn run_cube_json(&self, args: &[&str]) -> Result<serde_json::Value>;
}

// --- Wire payload structs shared by both transports -------------------------
//
// These small wrappers mirror the envelope cube wraps each subcommand's
// JSON in. The leaf records deserialize straight into the public
// `Cube*`/`CubeWorkspaceStatus` handle types from `coordinator` (which
// derive `Deserialize` with field names/renames matching the wire shape),
// so there is no second copy of those field sets to keep in sync.

#[derive(Deserialize)]
struct RepoEnsurePayload {
    repo_id: String,
}

#[derive(Deserialize)]
struct LeasePayload {
    workspace: LeaseWorkspace,
}

#[derive(Deserialize)]
struct LeaseWorkspace {
    lease_id: Option<String>,
    workspace_id: String,
    workspace_path: PathBuf,
}

#[derive(Deserialize)]
struct ChangePayload {
    change: ChangeRecord,
}

#[derive(Deserialize)]
struct ChangeRecord {
    change_id: String,
}

#[derive(Deserialize)]
struct StatusPayload {
    workspace: CubeWorkspaceStatus,
}

#[derive(Deserialize)]
struct ListWorkspacesPayload {
    workspaces: Vec<CubeWorkspaceStatus>,
}

#[derive(Deserialize)]
struct ListReposPayload {
    repos: Vec<CubeRepoSummary>,
}

// --- Command helpers --------------------------------------------------------

pub async fn ensure_repo<T: CubeJsonTransport + ?Sized>(transport: &T, origin: &str) -> Result<CubeRepoHandle> {
    let payload: RepoEnsurePayload = serde_json::from_value(
        transport
            .run_cube_json(&crate::repo_slug::repo_ensure_args(origin))
            .await?,
    )
    .context("decoding `cube repo ensure` payload")?;
    Ok(CubeRepoHandle {
        repo_id: payload.repo_id,
    })
}

pub async fn lease_workspace<T: CubeJsonTransport + ?Sized>(
    transport: &T,
    repo_id: &str,
    task: &str,
    prefer_workspace_id: Option<&str>,
    allow_dirty: bool,
    exclude_workspace_ids: &[&str],
) -> Result<CubeWorkspaceLease> {
    // `--release-on-setup-failure`: the engine never wants a lease left
    // stranded when a post-lease setup step exits non-zero (the anaplian
    // failure-mode A leak — the engine treats the non-zero exit as a lease
    // failure and never learns the lease id to release it). With this flag
    // cube hands the workspace back before returning the setup error, so a
    // remote-host setup failure surfaces as a clean lease failure with
    // nothing leaked.
    let mut args: Vec<&str> = vec![
        "--json",
        "workspace",
        "lease",
        repo_id,
        "--task",
        task,
        "--release-on-setup-failure",
    ];
    if let Some(prefer) = prefer_workspace_id {
        args.extend_from_slice(&["--prefer", prefer]);
    }
    if allow_dirty {
        args.push("--allow-dirty");
    }
    for excluded in exclude_workspace_ids {
        args.extend_from_slice(&["--exclude", excluded]);
    }
    let payload: LeasePayload = serde_json::from_value(transport.run_cube_json(&args).await?)
        .context("decoding `cube workspace lease` payload")?;
    let lease_id = payload
        .workspace
        .lease_id
        .context("cube workspace lease response missing lease_id")?;
    Ok(CubeWorkspaceLease {
        lease_id,
        workspace_id: payload.workspace.workspace_id,
        workspace_path: payload.workspace.workspace_path,
    })
}

pub async fn create_change<T: CubeJsonTransport + ?Sized>(
    transport: &T,
    workspace_path: &Path,
    title: &str,
) -> Result<CubeChangeHandle> {
    let workspace_arg = workspace_path.display().to_string();
    let payload: ChangePayload = serde_json::from_value(
        transport
            .run_cube_json(&[
                "--json",
                "change",
                "create",
                "--workspace",
                workspace_arg.as_str(),
                "--title",
                title,
            ])
            .await?,
    )
    .context("decoding `cube change create` payload")?;
    Ok(CubeChangeHandle {
        change_id: payload.change.change_id,
    })
}

pub async fn release_workspace<T: CubeJsonTransport + ?Sized>(transport: &T, lease_id: &str) -> Result<()> {
    let _ = transport
        .run_cube_json(&["--json", "workspace", "release", "--lease", lease_id])
        .await?;
    Ok(())
}

pub async fn workspace_status<T: CubeJsonTransport + ?Sized>(
    transport: &T,
    workspace_path: &Path,
) -> Result<CubeWorkspaceStatus> {
    let workspace_arg = workspace_path.display().to_string();
    let payload: StatusPayload = serde_json::from_value(
        transport
            .run_cube_json(&["--json", "workspace", "status", "--workspace", workspace_arg.as_str()])
            .await?,
    )
    .context("decoding `cube workspace status` payload")?;
    Ok(payload.workspace)
}

pub async fn heartbeat_lease<T: CubeJsonTransport + ?Sized>(
    transport: &T,
    lease_id: &str,
    ttl_seconds: Option<u64>,
) -> Result<()> {
    let ttl_string = ttl_seconds.map(|ttl| ttl.to_string());
    let mut args: Vec<&str> = vec!["--json", "workspace", "heartbeat", "--lease", lease_id];
    if let Some(ttl) = ttl_string.as_deref() {
        args.extend_from_slice(&["--ttl-seconds", ttl]);
    }
    let _ = transport.run_cube_json(&args).await?;
    Ok(())
}

pub async fn force_release_lease<T: CubeJsonTransport + ?Sized>(
    transport: &T,
    lease_id: &str,
    reason: Option<&str>,
) -> Result<()> {
    let mut args: Vec<&str> = vec!["--json", "workspace", "force-release", "--lease", lease_id];
    if let Some(reason) = reason {
        args.extend_from_slice(&["--reason", reason]);
    }
    let _ = transport.run_cube_json(&args).await?;
    Ok(())
}

pub async fn list_workspaces<T: CubeJsonTransport + ?Sized>(transport: &T) -> Result<Vec<CubeWorkspaceStatus>> {
    let payload: ListWorkspacesPayload =
        serde_json::from_value(transport.run_cube_json(&["--json", "workspace", "list"]).await?)
            .context("decoding `cube workspace list` payload")?;
    Ok(payload.workspaces)
}

pub async fn list_repos<T: CubeJsonTransport + ?Sized>(transport: &T) -> Result<Vec<CubeRepoSummary>> {
    let payload: ListReposPayload = serde_json::from_value(transport.run_cube_json(&["--json", "repo", "list"]).await?)
        .context("decoding `cube repo list` payload")?;
    Ok(payload.repos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    // --- CubeCliError::detail() -------------------------------------------

    /// Build a `CubeCliError` from the three fields that drive `detail()` /
    /// `clipped_stderr()`; `host` is irrelevant to both so it's fixed.
    fn err(exit_code: Option<i32>, stderr: &str, stdout: &str) -> CubeCliError {
        CubeCliError {
            host: "test-host".to_owned(),
            exit_code,
            stderr: stderr.to_owned(),
            stdout: stdout.to_owned(),
        }
    }

    #[test]
    fn detail_prefers_trimmed_stderr_when_present() {
        // Non-empty stderr wins outright, and is trimmed.
        assert_eq!(
            err(Some(1), "  boom on stderr  \n", "ignored stdout").detail(),
            "boom on stderr"
        );
    }

    #[test]
    fn detail_falls_back_to_trimmed_stdout_when_stderr_blank() {
        // Whitespace-only stderr is treated as empty; trimmed stdout is used.
        assert_eq!(
            err(Some(2), "   \n\t", "  fallback on stdout \n").detail(),
            "fallback on stdout"
        );
    }

    #[test]
    fn detail_synthesizes_exit_status_with_code() {
        // Both streams blank → synthetic clause carrying the exit code.
        assert_eq!(err(Some(42), "", "   ").detail(), "exit status 42");
    }

    #[test]
    fn detail_synthesizes_unknown_status_when_signal_killed() {
        // Both streams blank and no code (killed by signal) → the documented
        // "unknown" clause rather than a bogus code.
        assert_eq!(err(None, "  ", "").detail(), "exit status unknown (killed by signal?)");
    }

    // --- CubeCliError::clipped_stderr() -----------------------------------

    #[test]
    fn clipped_stderr_returns_whole_trimmed_when_under_limit() {
        // Under the limit: the full trimmed stderr comes back with no marker.
        let e = err(Some(1), "  short trace  ", "");
        assert_eq!(e.clipped_stderr(64), "short trace");
    }

    #[test]
    fn clipped_stderr_returns_whole_trimmed_at_exactly_limit() {
        // Boundary: len == max is "under or equal", so still returned whole.
        let e = err(Some(1), "abcde", "");
        assert_eq!(e.clipped_stderr(5), "abcde");
    }

    #[test]
    fn clipped_stderr_clips_ascii_over_limit() {
        // Over the limit on ASCII: exactly `max` bytes kept, marker appended.
        let e = err(Some(1), "abcdefghij", "");
        assert_eq!(e.clipped_stderr(4), "abcd… (clipped)");
    }

    #[test]
    fn clipped_stderr_clips_on_char_boundary_without_splitting_codepoint() {
        // Each `€` is 3 bytes (boundaries at 0,3,6,9,12). max=4 lands mid the
        // second codepoint; the loop must back up to byte 3 so the slice is
        // valid UTF-8. If it didn't, slicing would panic.
        let e = err(Some(1), "€€€€", "");
        let clipped = e.clipped_stderr(4);
        assert_eq!(clipped, "€… (clipped)");
        // Prove we never split a codepoint: the kept prefix is whole chars.
        assert!(clipped.starts_with('€'));
    }

    #[test]
    fn clipped_stderr_can_back_up_to_empty_prefix() {
        // A single multibyte char with max below its first boundary backs the
        // end index all the way to 0 rather than panicking mid-codepoint.
        let e = err(Some(1), "€ padded so it exceeds the limit", "");
        let clipped = e.clipped_stderr(2);
        assert_eq!(clipped, "… (clipped)");
    }

    // --- Command helpers: argv construction + decode ----------------------

    /// A [`CubeJsonTransport`] that records the argv of its single expected
    /// `run_cube_json` call and replays a canned JSON envelope. Tests assert
    /// the recorded argv and the decoded handle — observable behavior only.
    struct RecordingTransport {
        recorded: Mutex<Option<Vec<String>>>,
        response: serde_json::Value,
    }

    impl RecordingTransport {
        fn new(response: serde_json::Value) -> Self {
            Self {
                recorded: Mutex::new(None),
                response,
            }
        }

        /// The argv captured from the (single) `run_cube_json` call.
        fn argv(&self) -> Vec<String> {
            self.recorded
                .lock()
                .unwrap()
                .clone()
                .expect("run_cube_json was never called")
        }
    }

    #[async_trait]
    impl CubeJsonTransport for RecordingTransport {
        async fn run_cube_json(&self, args: &[&str]) -> Result<serde_json::Value> {
            *self.recorded.lock().unwrap() = Some(args.iter().map(|s| s.to_string()).collect());
            Ok(self.response.clone())
        }
    }

    #[tokio::test]
    async fn ensure_repo_builds_argv_and_decodes_handle() {
        let t = RecordingTransport::new(json!({ "repo_id": "repo-123" }));
        let handle = ensure_repo(&t, "bduff").await.unwrap();
        // Bare slug routes through the positional `repo ensure <slug>` form.
        assert_eq!(t.argv(), ["--json", "repo", "ensure", "bduff"]);
        assert_eq!(
            handle,
            CubeRepoHandle {
                repo_id: "repo-123".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn lease_workspace_omits_optional_flags_by_default() {
        let t = RecordingTransport::new(json!({
            "workspace": { "lease_id": "lease-1", "workspace_id": "ws-1", "workspace_path": "/tmp/ws-1" }
        }));
        let lease = lease_workspace(&t, "repo-1", "do a thing", None, false, &[])
            .await
            .unwrap();
        // No --prefer, no --allow-dirty, no --exclude; but always
        // --release-on-setup-failure.
        assert_eq!(
            t.argv(),
            [
                "--json",
                "workspace",
                "lease",
                "repo-1",
                "--task",
                "do a thing",
                "--release-on-setup-failure"
            ]
        );
        assert_eq!(
            lease,
            CubeWorkspaceLease {
                lease_id: "lease-1".to_owned(),
                workspace_id: "ws-1".to_owned(),
                workspace_path: PathBuf::from("/tmp/ws-1"),
            }
        );
    }

    #[tokio::test]
    async fn lease_workspace_emits_all_optional_flags_when_supplied() {
        let t = RecordingTransport::new(json!({
            "workspace": { "lease_id": "lease-2", "workspace_id": "ws-2", "workspace_path": "/tmp/ws-2" }
        }));
        lease_workspace(&t, "repo-1", "resume", Some("ws-pref"), true, &["old-a", "old-b"])
            .await
            .unwrap();
        // --prefer only when Some, --allow-dirty only when true, one
        // --exclude per excluded id, in that order.
        assert_eq!(
            t.argv(),
            [
                "--json",
                "workspace",
                "lease",
                "repo-1",
                "--task",
                "resume",
                "--release-on-setup-failure",
                "--prefer",
                "ws-pref",
                "--allow-dirty",
                "--exclude",
                "old-a",
                "--exclude",
                "old-b",
            ]
        );
    }

    #[tokio::test]
    async fn lease_workspace_surfaces_missing_lease_id() {
        // A valid envelope shape but no lease_id → the documented error.
        let t = RecordingTransport::new(json!({
            "workspace": { "workspace_id": "ws-3", "workspace_path": "/tmp/ws-3" }
        }));
        let err = lease_workspace(&t, "repo-1", "t", None, false, &[]).await.unwrap_err();
        assert!(err.to_string().contains("missing lease_id"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn create_change_builds_argv_and_decodes_handle() {
        let t = RecordingTransport::new(json!({ "change": { "change_id": "chg-9" } }));
        let handle = create_change(&t, Path::new("/tmp/ws-1"), "My title").await.unwrap();
        assert_eq!(
            t.argv(),
            [
                "--json",
                "change",
                "create",
                "--workspace",
                "/tmp/ws-1",
                "--title",
                "My title"
            ]
        );
        assert_eq!(
            handle,
            CubeChangeHandle {
                change_id: "chg-9".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn release_workspace_builds_argv() {
        let t = RecordingTransport::new(json!({ "ok": true }));
        release_workspace(&t, "lease-7").await.unwrap();
        assert_eq!(t.argv(), ["--json", "workspace", "release", "--lease", "lease-7"]);
    }

    #[tokio::test]
    async fn workspace_status_builds_argv_and_decodes() {
        let t = RecordingTransport::new(json!({
            "workspace": {
                "workspace_id": "ws-1",
                "workspace_path": "/tmp/ws-1",
                "state": "leased",
                "lease_id": "lease-1",
                "holder": "agent-019",
                "task": "chore",
                "leased_at_epoch_s": 100,
                "lease_expires_at_epoch_s": 200
            }
        }));
        let status = workspace_status(&t, Path::new("/tmp/ws-1")).await.unwrap();
        assert_eq!(t.argv(), ["--json", "workspace", "status", "--workspace", "/tmp/ws-1"]);
        assert_eq!(status.workspace_id, "ws-1");
        assert_eq!(status.state, "leased");
        assert_eq!(status.lease_id.as_deref(), Some("lease-1"));
    }

    #[tokio::test]
    async fn heartbeat_lease_omits_ttl_when_absent() {
        let t = RecordingTransport::new(json!({ "ok": true }));
        heartbeat_lease(&t, "lease-1", None).await.unwrap();
        assert_eq!(t.argv(), ["--json", "workspace", "heartbeat", "--lease", "lease-1"]);
    }

    #[tokio::test]
    async fn heartbeat_lease_emits_ttl_when_supplied() {
        let t = RecordingTransport::new(json!({ "ok": true }));
        heartbeat_lease(&t, "lease-1", Some(90)).await.unwrap();
        assert_eq!(
            t.argv(),
            [
                "--json",
                "workspace",
                "heartbeat",
                "--lease",
                "lease-1",
                "--ttl-seconds",
                "90"
            ]
        );
    }

    #[tokio::test]
    async fn force_release_lease_omits_reason_when_absent() {
        let t = RecordingTransport::new(json!({ "ok": true }));
        force_release_lease(&t, "lease-1", None).await.unwrap();
        assert_eq!(t.argv(), ["--json", "workspace", "force-release", "--lease", "lease-1"]);
    }

    #[tokio::test]
    async fn force_release_lease_emits_reason_when_supplied() {
        let t = RecordingTransport::new(json!({ "ok": true }));
        force_release_lease(&t, "lease-1", Some("stale holder")).await.unwrap();
        assert_eq!(
            t.argv(),
            [
                "--json",
                "workspace",
                "force-release",
                "--lease",
                "lease-1",
                "--reason",
                "stale holder"
            ]
        );
    }

    #[tokio::test]
    async fn list_workspaces_builds_argv_and_decodes() {
        let t = RecordingTransport::new(json!({
            "workspaces": [
                {
                    "workspace_id": "ws-1",
                    "workspace_path": "/tmp/ws-1",
                    "state": "free",
                    "lease_id": null,
                    "holder": null,
                    "task": null,
                    "leased_at_epoch_s": null,
                    "lease_expires_at_epoch_s": null
                }
            ]
        }));
        let workspaces = list_workspaces(&t).await.unwrap();
        assert_eq!(t.argv(), ["--json", "workspace", "list"]);
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].workspace_id, "ws-1");
        assert_eq!(workspaces[0].state, "free");
    }

    #[tokio::test]
    async fn list_repos_builds_argv_and_decodes() {
        let t = RecordingTransport::new(json!({
            "repos": [
                {
                    "repo": "repo-1",
                    "origin": "git@github.com:spinyfin/mono.git",
                    "main_branch": "main",
                    "workspace_root": "/tmp/roots",
                    "workspace_prefix": "mono-agent"
                }
            ]
        }));
        let repos = list_repos(&t).await.unwrap();
        assert_eq!(t.argv(), ["--json", "repo", "list"]);
        assert_eq!(repos.len(), 1);
        // `repo` renames to `repo_id`; `source` defaults to None when absent.
        assert_eq!(repos[0].repo_id, "repo-1");
        assert_eq!(repos[0].origin, "git@github.com:spinyfin/mono.git");
        assert_eq!(repos[0].source, None);
    }
}
