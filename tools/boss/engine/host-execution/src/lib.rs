//! Host filesystem execution shared by the hub's local adapter and the spoke.
//! Work policy, GitHub decisions and scheduling remain in the hub.
pub mod homes;
pub mod process;
pub mod recovery_ops;
pub mod worker;
pub mod cube {
    //! Shared implementation of the `cube` CLI command surface.
    //!
    //! Both the hub's local cube client (which
    //! shells out to a local `cube` binary) and
    //! the SSH adapter (which runs `cube`
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
    use std::time::Duration;

    use anyhow::{Context, Result};
    use async_trait::async_trait;
    use serde::Deserialize;

    use crate::types::{CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus};

    /// Cadence between lease heartbeat passes, hub and spoke alike. Deliberately
    /// far below [`LEASE_TTL_SECS`] so many consecutive missed beats are tolerated.
    pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(300);

    /// TTL (seconds) every heartbeat asks cube to set. Matches cube's own
    /// default of 24 h (`DEFAULT_LEASE_TTL_SECS`) and is passed explicitly so the
    /// interval-much-less-than-TTL relationship is owned here. A short TTL would
    /// let cube's sweep free the workspace of a live worker during a daemon
    /// outage or host sleep.
    pub const LEASE_TTL_SECS: u64 = 86400;

    /// Local CLI transport. Configuration belongs to the host, never to wire input.
    #[derive(Clone)]
    pub struct LocalCube {
        pub command: PathBuf,
        pub args: Vec<String>,
        pub cwd: PathBuf,
    }

    impl LocalCube {
        pub async fn run(&self, args: &[&str]) -> Result<serde_json::Value> {
            use std::process::Stdio;
            let output = tokio::process::Command::new(&self.command)
                .args(&self.args)
                .args(args)
                .current_dir(&self.cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output()
                .await
                .with_context(|| {
                    format!(
                        "failed to spawn Cube command: {} {}",
                        self.command.display(),
                        self.args.join(" ")
                    )
                })?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                return Err(CubeCliError {
                    host: "local".to_owned(),
                    exit_code: output.status.code(),
                    stderr,
                    stdout,
                }
                .into());
            }
            serde_json::from_slice(&output.stdout).context("failed to decode Cube JSON output")
        }
    }

    #[async_trait]
    impl CubeJsonTransport for LocalCube {
        async fn run_cube_json(&self, args: &[&str]) -> Result<serde_json::Value> {
            self.run_cube_json_within(args, Duration::from_secs(30)).await
        }

        async fn run_cube_json_within(&self, args: &[&str], budget: Duration) -> Result<serde_json::Value> {
            tokio::time::timeout(budget, self.run(args))
                .await
                .context("local Cube command timed out")?
        }
    }

    pub async fn goto_workspace<T: CubeJsonTransport + ?Sized>(
        transport: &T,
        workspace: &Path,
        selector: &str,
        value: &str,
    ) -> Result<()> {
        transport
            .run_cube_json_within(
                &[
                    "--json",
                    "workspace",
                    "goto",
                    "--workspace",
                    &workspace.display().to_string(),
                    selector,
                    value,
                ],
                crate::cube_op_budget::GOTO,
            )
            .await?;
        Ok(())
    }

    /// A failed `cube` CLI invocation that preserves the structured signals a
    /// post-mortem needs — the process exit code and the captured
    /// stderr/stdout — instead of collapsing them into an opaque string.
    ///
    /// The SSH adapter returns this error on nonzero exit. Lease failure events
    /// include its exit code and captured diagnostics, including setup failures
    /// after a lease has already been granted.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
        /// Only cube's explicit structured refusal suppresses positioning retries.
        /// Older binaries, transport errors and malformed payloads remain retryable.
        pub fn positioning_refused(&self) -> bool {
            self.structured_refusal().is_some()
        }

        /// Cube's own refusal message (the envelope's `error` string), free of the
        /// JSON wrapper, for the blocked label and the attention body.
        pub fn refusal_message(&self) -> Option<String> {
            self.structured_refusal()?["error"].as_str().map(str::to_owned)
        }

        /// The refusal envelope, wherever it sits in the captured output. Cube
        /// prints other diagnostics (fetch retries, op-divergence recovery) to
        /// stderr before the envelope, so the whole stream is not one document:
        /// try each top-level `{` line, last first, and accept the first object
        /// that parses and carries the refusal kind.
        fn structured_refusal(&self) -> Option<serde_json::Value> {
            [&self.stderr, &self.stdout].into_iter().find_map(|output| {
                let starts = output
                    .match_indices('{')
                    .map(|(index, _)| index)
                    .filter(|&index| index == 0 || output.as_bytes()[index - 1] == b'\n');
                let starts: Vec<usize> = starts.collect();
                starts.into_iter().rev().find_map(|start| {
                    let value = serde_json::Deserializer::from_str(&output[start..])
                        .into_iter::<serde_json::Value>()
                        .next()?
                        .ok()?;
                    (value["error_kind"] == "workspace_positioning_refused" && value["retryable"] == false)
                        .then_some(value)
                })
            })
        }

        /// The single-line human detail: trimmed stderr, else trimmed stdout,
        /// else a synthetic exit-status clause. The SSH adapter uses the same
        /// detail in error messages and structured logs.
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
            boss_engine_utils::string_clip::clip_to_bytes_with_suffix(stderr, max, "… (clipped)")
        }
    }

    #[cfg(test)]
    mod positioning_error_tests {
        use super::CubeCliError;

        #[test]
        fn only_structured_refusals_stop_retries_on_local_and_remote_hosts() {
            for host in ["local", "remote-host"] {
                for (payload, refused) in [
                    (
                        r#"{"error_kind":"workspace_positioning_refused","retryable":false,"error":"arbitrary diagnostic"}"#,
                        true,
                    ),
                    (
                        r#"{"error_kind":"workspace_positioning_refused","retryable":true}"#,
                        false,
                    ),
                    (r#"{"error":"workspace_positioning_refused unpushed content"}"#, false),
                    ("workspace_positioning_refused", false),
                    (r#"{"retryable":false}"#, false),
                    (
                        "warning: jj fetch retry 1/3\n{\n  \"error\": \"x\",\n  \"error_kind\": \"workspace_positioning_refused\",\n  \"retryable\": false\n}",
                        true,
                    ),
                ] {
                    for stderr in [true, false] {
                        let error = CubeCliError {
                            host: host.into(),
                            exit_code: Some(2),
                            stderr: if stderr { payload.into() } else { String::new() },
                            stdout: if stderr { String::new() } else { payload.into() },
                        };
                        assert_eq!(error.positioning_refused(), refused, "{error}");
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod refusal_message_tests {
        use super::CubeCliError;

        #[test]
        fn refusal_message_is_cubes_error_string_without_the_envelope() {
            let error = CubeCliError {
                host: "local".into(),
                exit_code: Some(2),
                stdout: String::new(),
                stderr: "warning: retrying fetch\n{\n  \"error\": \"PR 7 is MERGED\",\n  \"error_kind\": \"workspace_positioning_refused\",\n  \"retryable\": false\n}\n".into(),
            };
            assert_eq!(error.refusal_message().as_deref(), Some("PR 7 is MERGED"));
            let transient = CubeCliError {
                stderr: r#"{"error":"network","error_kind":"command_failed","retryable":true}"#.into(),
                ..error
            };
            assert_eq!(transient.refusal_message(), None);
        }
    }

    impl fmt::Display for CubeCliError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            if self.host == "local" {
                write!(f, "Cube command failed: {}", self.detail())
            } else {
                write!(f, "ssh cube command failed on host {}: {}", self.host, self.detail())
            }
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

        /// Run `cube <args>` with an explicit wall-clock budget, for a
        /// provisioning-class operation whose cost is dominated by work on the
        /// machine cube runs on rather than by the call itself — `workspace
        /// lease` and `repo ensure` (see [`crate::cube_op_budget`]).
        ///
        /// The default delegates to transports with no internal timeout. Bounded
        /// transports, including LocalCube and SSH, override this method to honor
        /// the caller's operation budget instead of their generic command timeout.
        async fn run_cube_json_within(&self, args: &[&str], budget: Duration) -> Result<serde_json::Value> {
            let _ = budget;
            self.run_cube_json(args).await
        }
    }

    // --- Wire payload structs shared by both transports -------------------------
    //
    // These small wrappers mirror the envelope cube wraps each subcommand's
    // JSON in. The leaf records deserialize straight into the public
    // `Cube*`/`CubeWorkspaceStatus` handle types from `crate::types` (which
    // derive `Deserialize` with field names/renames matching the wire shape),
    // so there is no second copy of those field sets to keep in sync.

    #[derive(Deserialize)]
    struct RepoEnsurePayload {
        repo_id: String,
    }

    #[derive(Deserialize)]
    struct LeasePayload {
        workspace: LeaseWorkspace,
        /// Cube's per-candidate health report. On an `--allow-dirty` reclaim it
        /// holds exactly one entry carrying `dirty_verified` — see
        /// [`lease_workspace`].
        #[serde(default)]
        health_check: Vec<LeaseHealthCheck>,
    }

    #[derive(Deserialize)]
    struct LeaseWorkspace {
        lease_id: Option<String>,
        workspace_id: String,
        workspace_path: PathBuf,
    }

    #[derive(Deserialize)]
    struct LeaseHealthCheck {
        /// Present only on the `--allow-dirty` reclaim path. `Some(false)` means
        /// cube handed the workspace over but its working copy held nothing left
        /// to recover.
        #[serde(default)]
        dirty_verified: Option<bool>,
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
        workspaces: Vec<crate::types::RegisteredWorkspace>,
    }

    #[derive(Deserialize)]
    struct ListReposPayload {
        repos: Vec<CubeRepoSummary>,
    }

    // --- Command helpers --------------------------------------------------------

    /// Ensure cube knows about `origin`, cloning it on the first call for a
    /// given machine.
    ///
    /// Runs under [`crate::cube_op_budget::REPO_ENSURE`] rather than the
    /// transport's generic bound: the first ensure for a repo performs a
    /// `jj git clone`, which on a remote host that has never seen the repo is
    /// the single most expensive cube call the dispatcher makes.
    pub async fn ensure_repo<T: CubeJsonTransport + ?Sized>(transport: &T, origin: &str) -> Result<CubeRepoHandle> {
        let payload: RepoEnsurePayload = serde_json::from_value(
            transport
                .run_cube_json_within(
                    &repo_ensure_args(origin),
                    crate::cube_op_budget::repo_ensure_transport_budget(),
                )
                .await?,
        )
        .context("decoding `cube repo ensure` payload")?;
        Ok(CubeRepoHandle {
            repo_id: payload.repo_id,
        })
    }

    /// Lease a cube workspace.
    ///
    /// When `allow_dirty` is set this is a *recovery* lease: cube reclaims the
    /// named workspace without resetting it. Note that a successful
    /// `--allow-dirty` lease does not establish that anything was recovered — a
    /// workspace that had already been reset is handed over just as happily. The
    /// returned [`CubeWorkspaceLease::dirty_verified`] is cube's verdict on
    /// whether the working copy really still held work; `Some(false)` is the
    /// signal to fall back to the engine's own recovery patch.
    ///
    /// Runs under [`crate::cube_op_budget::LEASE`] rather than the transport's
    /// generic bound. A lease is not a quick remote command: cube stacks a
    /// health scan, a quarantine-reclaim scan and `jj workspace add` + setup
    /// steps behind it, which is exactly why the dispatcher's bound for it is
    /// 90s and not 30s.
    pub async fn lease_workspace<T: CubeJsonTransport + ?Sized>(
        transport: &T,
        repo_id: &str,
        task: &str,
        prefer_workspace_id: Option<&str>,
        allow_dirty: bool,
        exclude_workspace_ids: &[&str],
    ) -> Result<CubeWorkspaceLease> {
        // Cube releases the lease before reporting a setup failure. Otherwise the
        // caller receives an error without the lease identity needed for cleanup.
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
        let payload: LeasePayload = serde_json::from_value(
            transport
                .run_cube_json_within(&args, crate::cube_op_budget::lease_transport_budget())
                .await?,
        )
        .context("decoding `cube workspace lease` payload")?;
        let lease_id = payload
            .workspace
            .lease_id
            .context("cube workspace lease response missing lease_id")?;
        // `dirty_verified` is cube's positive recovery check (see the flag docs
        // in `cube workspace lease --help`). It is reported only on the
        // `--allow-dirty` reclaim path; `None` elsewhere means "not applicable",
        // NOT "nothing recovered".
        let dirty_verified = payload.health_check.iter().find_map(|hc| hc.dirty_verified);
        Ok(CubeWorkspaceLease {
            lease_id,
            workspace_id: payload.workspace.workspace_id,
            workspace_path: payload.workspace.workspace_path,
            dirty_verified,
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

    /// End-of-run release: skip cube's dirty-preservation probe so the
    /// workspace returns to the pool. Use it only after teardown preservation
    /// succeeded; after a failed preservation use [`release_workspace`] so
    /// cube's guard keeps or salvages a dirty tree.
    pub async fn release_workspace_with_reset<T: CubeJsonTransport + ?Sized>(
        transport: &T,
        lease_id: &str,
    ) -> Result<()> {
        let _ = transport
            .run_cube_json(&["--json", "workspace", "release", "--lease", lease_id, "--force-reset"])
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
        Ok(registered_workspaces(transport)
            .await?
            .into_iter()
            .map(|row| row.status)
            .collect())
    }

    pub async fn registered_workspaces<T: CubeJsonTransport + ?Sized>(
        transport: &T,
    ) -> Result<Vec<crate::types::RegisteredWorkspace>> {
        let payload: ListWorkspacesPayload =
            serde_json::from_value(transport.run_cube_json(&["--json", "workspace", "list"]).await?)
                .context("decoding `cube workspace list` payload")?;
        Ok(payload.workspaces)
    }

    pub async fn list_repos<T: CubeJsonTransport + ?Sized>(transport: &T) -> Result<Vec<CubeRepoSummary>> {
        let payload: ListReposPayload =
            serde_json::from_value(transport.run_cube_json(&["--json", "repo", "list"]).await?)
                .context("decoding `cube repo list` payload")?;
        Ok(payload.repos)
    }

    /// Run `cube repo gc <repo_id>`: abandon the shared jj store's operations
    /// older than cube's keep window and collect what nothing references.
    ///
    /// Runs under [`crate::cube_op_budget::REPO_GC`]. A collection deletes one
    /// file per abandoned operation and view and waits for an instant with no
    /// `jj` process on the host, so it takes minutes, not the transport's
    /// generic bound.
    pub async fn gc_repo<T: CubeJsonTransport + ?Sized>(transport: &T, repo_id: &str) -> Result<()> {
        anyhow::ensure!(
            !repo_id.is_empty() && !repo_id.starts_with('-') && !repo_id.chars().any(char::is_whitespace),
            "invalid cube repo id {repo_id:?}"
        );
        transport
            .run_cube_json_within(
                &["--json", "repo", "gc", repo_id],
                crate::cube_op_budget::repo_gc_transport_budget(),
            )
            .await?;
        Ok(())
    }

    pub fn repo_ensure_args(repo_ref: &str) -> Vec<&str> {
        if git_utils::repo_slug::is_bare_repo_slug(repo_ref) {
            vec!["--json", "repo", "ensure", repo_ref.trim()]
        } else {
            vec!["--json", "repo", "ensure", "--origin", repo_ref]
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;
        use std::sync::Mutex;

        #[tokio::test]
        async fn local_failure_preserves_structured_diagnostics() {
            let cwd = tempfile::tempdir().unwrap();
            let cube = LocalCube {
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "printf output; printf failure >&2; exit 7".into()],
                cwd: cwd.path().to_path_buf(),
            };
            let error = cube.run(&[]).await.unwrap_err();
            let detail = error.downcast_ref::<CubeCliError>().unwrap();
            assert_eq!(detail.host, "local");
            assert_eq!(detail.exit_code, Some(7));
            assert_eq!(detail.stdout, "output");
            assert_eq!(detail.stderr, "failure");
            assert_eq!(error.to_string(), "Cube command failed: failure");
        }

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

        /// A [`CubeJsonTransport`] that records the argv (and the budget, if
        /// any) of its single expected call and replays a canned JSON envelope.
        /// Tests assert the recorded argv and the decoded handle — observable
        /// behavior only.
        ///
        /// The explicit-budget override records the caller's requested timeout,
        /// while plain calls retain None.
        struct RecordingTransport {
            recorded: Mutex<Option<Vec<String>>>,
            budget: Mutex<Option<Duration>>,
            response: serde_json::Value,
        }

        impl RecordingTransport {
            fn new(response: serde_json::Value) -> Self {
                Self {
                    recorded: Mutex::new(None),
                    budget: Mutex::new(None),
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

            /// The budget the helper requested, or `None` if it used the plain
            /// unbudgeted call.
            fn budget(&self) -> Option<Duration> {
                *self.budget.lock().unwrap()
            }
        }

        #[async_trait]
        impl CubeJsonTransport for RecordingTransport {
            async fn run_cube_json(&self, args: &[&str]) -> Result<serde_json::Value> {
                *self.recorded.lock().unwrap() = Some(args.iter().map(|s| s.to_string()).collect());
                Ok(self.response.clone())
            }

            async fn run_cube_json_within(&self, args: &[&str], budget: Duration) -> Result<serde_json::Value> {
                *self.budget.lock().unwrap() = Some(budget);
                self.run_cube_json(args).await
            }
        }

        // --- Per-operation budgets -------------------------------------------

        /// Provisioning helpers request their named budget from bounded transports.
        #[tokio::test]
        async fn lease_workspace_requests_the_lease_budget() {
            let t = RecordingTransport::new(json!({
                "workspace": { "lease_id": "lease-1", "workspace_id": "ws-1", "workspace_path": "/tmp/ws-1" }
            }));
            lease_workspace(&t, "repo-1", "do a thing", None, false, &[])
                .await
                .unwrap();
            assert_eq!(t.budget(), Some(crate::cube_op_budget::lease_transport_budget()));
        }

        #[tokio::test]
        async fn goto_requests_the_positioning_budget() {
            let transport = RecordingTransport::new(json!({}));
            goto_workspace(&transport, Path::new("/tmp/workspace"), "--pr", "42")
                .await
                .unwrap();
            assert_eq!(transport.budget(), Some(crate::cube_op_budget::GOTO));
            assert_eq!(
                transport.argv(),
                [
                    "--json",
                    "workspace",
                    "goto",
                    "--workspace",
                    "/tmp/workspace",
                    "--pr",
                    "42"
                ]
            );
        }

        /// `repo ensure` performs the first-time clone, so it gets its own
        /// budget for the same reason.
        #[tokio::test]
        async fn ensure_repo_requests_the_repo_ensure_budget() {
            let t = RecordingTransport::new(json!({ "repo_id": "repo-123" }));
            ensure_repo(&t, "bduff").await.unwrap();
            assert_eq!(t.budget(), Some(crate::cube_op_budget::repo_ensure_transport_budget()));
        }

        /// A store collection takes minutes, so it asks for its own budget, and a
        /// repo id can never be read as a flag.
        #[tokio::test]
        async fn gc_repo_requests_the_collection_budget_and_rejects_flag_like_ids() {
            let t = RecordingTransport::new(json!({ "store_gc": {} }));
            gc_repo(&t, "mono").await.unwrap();
            assert_eq!(t.budget(), Some(crate::cube_op_budget::repo_gc_transport_budget()));
            assert_eq!(t.argv(), ["--json", "repo", "gc", "mono"]);

            let rejected = RecordingTransport::new(json!({}));
            for id in ["", "--dry-run", "two words"] {
                assert!(gc_repo(&rejected, id).await.is_err(), "{id:?}");
            }
            assert_eq!(rejected.budget(), None, "nothing ran");
        }

        /// Cheap reads keep the transport default so unreachable hosts fail promptly.
        #[tokio::test]
        async fn cheap_reads_do_not_request_an_extended_budget() {
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
            workspace_status(&t, Path::new("/tmp/ws-1")).await.unwrap();
            assert_eq!(t.budget(), None);

            let t = RecordingTransport::new(json!({ "ok": true }));
            heartbeat_lease(&t, "lease-1", None).await.unwrap();
            assert_eq!(t.budget(), None);

            let t = RecordingTransport::new(json!({ "ok": true }));
            release_workspace(&t, "lease-1").await.unwrap();
            assert_eq!(t.budget(), None);
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
                    dirty_verified: None,
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

        /// The recovery lease decodes cube's `dirty_verified` verdict: `true`
        /// means cube handed back a working copy that still held unrecovered
        /// work, so no patch fallback is needed.
        #[tokio::test]
        async fn lease_workspace_decodes_dirty_verified_true() {
            let t = RecordingTransport::new(json!({
                "workspace": { "lease_id": "lease-r", "workspace_id": "ws-r", "workspace_path": "/tmp/ws-r" },
                "health_check": [{ "workspace_id": "ws-r", "allow_dirty": true, "dirty_verified": true }],
            }));
            let lease = lease_workspace(&t, "repo-1", "resume", Some("ws-r"), true, &[])
                .await
                .unwrap();
            assert_eq!(lease.dirty_verified, Some(true));
        }

        /// `dirty_verified: false` is the whole point of the field: the lease
        /// succeeded, but cube recovered nothing and the caller must fall back to
        /// its own patch rather than assume a resumed tree.
        #[tokio::test]
        async fn lease_workspace_decodes_dirty_verified_false() {
            let t = RecordingTransport::new(json!({
                "workspace": { "lease_id": "lease-r", "workspace_id": "ws-r", "workspace_path": "/tmp/ws-r" },
                "health_check": [{ "workspace_id": "ws-r", "allow_dirty": true, "dirty_verified": false }],
            }));
            let lease = lease_workspace(&t, "repo-1", "resume", Some("ws-r"), true, &[])
                .await
                .unwrap();
            assert_eq!(lease.dirty_verified, Some(false));
        }

        /// A normal (non-recovery) lease reports no verdict at all. `None` must
        /// stay distinguishable from `Some(false)`: "not applicable" is not
        /// "nothing recovered".
        #[tokio::test]
        async fn lease_workspace_reports_no_verdict_on_a_normal_lease() {
            let t = RecordingTransport::new(json!({
                "workspace": { "lease_id": "lease-n", "workspace_id": "ws-n", "workspace_path": "/tmp/ws-n" },
                "health_check": [{ "workspace_id": "ws-n", "health": "clean", "skipped": false }],
            }));
            let lease = lease_workspace(&t, "repo-1", "task", None, false, &[]).await.unwrap();
            assert_eq!(lease.dirty_verified, None);
        }

        /// Cube versions that predate the field (or a payload without a health
        /// check at all) must decode cleanly rather than failing the lease.
        #[tokio::test]
        async fn lease_workspace_tolerates_a_payload_without_health_check() {
            let t = RecordingTransport::new(json!({
                "workspace": { "lease_id": "lease-o", "workspace_id": "ws-o", "workspace_path": "/tmp/ws-o" },
            }));
            let lease = lease_workspace(&t, "repo-1", "task", None, false, &[]).await.unwrap();
            assert_eq!(lease.dirty_verified, None);
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
        async fn release_workspace_with_reset_builds_argv() {
            let t = RecordingTransport::new(json!({ "ok": true }));
            release_workspace_with_reset(&t, "lease-7").await.unwrap();
            assert_eq!(
                t.argv(),
                ["--json", "workspace", "release", "--lease", "lease-7", "--force-reset"]
            );
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
}
pub mod cube_op_budget {
    //! Shared wall-clock budgets for cube provisioning operations.
    //! Transports honor these budgets so network and setup work can exceed the
    //! generic read timeout while remaining bounded.

    use std::time::Duration;

    /// Backstop for workspace creation and setup after cube's bounded health scan.
    pub const LEASE: Duration = Duration::from_secs(90);

    /// Upper bound on a single `cube repo ensure` invocation. Normally fast
    /// (an idempotent record lookup), but the first ensure for a repo on a
    /// given machine performs the initial `jj git clone`, and the same
    /// wedged-subprocess hang class as [`LEASE`] applies, so it is bounded too.
    pub const REPO_ENSURE: Duration = Duration::from_secs(60);

    /// Positioning may fetch a large repository; stay within the executor's 300s bound.
    pub const GOTO: Duration = Duration::from_secs(290);

    /// Upper bound on one `cube repo gc`. The mono store's collection took
    /// 622 s once the host was quiet, and cube waits up to five more minutes
    /// for that instant, so this leaves room for a slow disk.
    pub const REPO_GC: Duration = Duration::from_secs(45 * 60);

    /// Margin between a caller's outer budget and the budget handed to the
    /// transport underneath it.
    ///
    /// Both bounds are real and both must stay: the outer one
    /// (`tokio::time::timeout` around the adapter call) is the backstop that
    /// fires even if a transport forgets to bound itself, and the inner one is
    /// the transport's own. Handing the transport the *same* number would make
    /// which of the two fires a coin flip, and they do not produce equally
    /// useful errors — the transport's names the host and the exact argv, the
    /// backstop's does not. Giving the transport slightly less guarantees the
    /// better-attributed error wins, without changing the end-to-end bound the
    /// caller advertises.
    pub const TRANSPORT_GRACE: Duration = Duration::from_secs(10);

    /// The budget to hand a transport for an operation whose outer bound is
    /// `outer`. Always strictly less than `outer` (see [`TRANSPORT_GRACE`]).
    pub const fn transport_budget(outer: Duration) -> Duration {
        outer.saturating_sub(TRANSPORT_GRACE)
    }

    /// Transport-level budget for `cube workspace lease`.
    pub const fn lease_transport_budget() -> Duration {
        transport_budget(LEASE)
    }

    /// Transport-level budget for `cube repo ensure`.
    pub const fn repo_ensure_transport_budget() -> Duration {
        transport_budget(REPO_ENSURE)
    }

    /// Transport-level budget for `cube repo gc`.
    pub const fn repo_gc_transport_budget() -> Duration {
        transport_budget(REPO_GC)
    }
}
pub mod input {
    //! Driver provisioning and recorded input-home reclamation, without work DB policy.
    use anyhow::{Context, Result};
    use boss_engine_driver::AgentDriver;
    use boss_protocol::DriverRuntimeState;
    use std::path::Path;

    /// Driver provisioning owns the initial-prompt relay and returns its cleanup handle.
    /// Validate bytes before any driver filesystem side effect.
    pub async fn provision(
        driver: &dyn AgentDriver,
        workspace: &Path,
        prompt_utf8: &[u8],
        run_id: &str,
    ) -> Result<Option<DriverRuntimeState>> {
        let prompt = std::str::from_utf8(prompt_utf8).context("initial prompt must be UTF-8")?;
        driver.provision_workspace(workspace, prompt, run_id).await
    }

    /// Reclaim only the home recorded by provisioning, after the caller has retained
    /// any required transcript evidence. Never infer ownership from a workspace path.
    pub fn reclaim(state: &DriverRuntimeState) -> Result<()> {
        if let Some(grok) = grok_state(state) {
            // The container holds both the Grok home and the scoped process home.
            if let Some(container) = grok.grok_home.parent() {
                boss_engine_driver::grok::reclaim_grok_home(container)?;
            }
            return Ok(());
        }
        if let Some(home) = boss_engine_driver::initial_prompt::owned_home_from_runtime_state(state) {
            reclaim_home(&home)?;
        }
        Ok(())
    }

    fn grok_state(state: &DriverRuntimeState) -> Option<boss_engine_driver::grok::GrokRuntimeState> {
        state.as_value().get("grok_home")?;
        boss_engine_driver::grok::GrokRuntimeState::from_driver_runtime_state(state).ok()
    }

    /// The per-run home a driver's runtime state owns and holds the delivery
    /// markers: Codex and Claude via the shared rule, plus a Grok home.
    pub fn owned_home(state: &DriverRuntimeState) -> Option<std::path::PathBuf> {
        boss_engine_driver::initial_prompt::owned_home_from_runtime_state(state)
            .or_else(|| grok_state(state).map(|grok| grok.grok_home))
    }

    /// Reclaim an already recorded home after hub retention authorizes it.
    pub fn reclaim_home(home: &Path) -> Result<()> {
        boss_engine_driver::codex::reclaim_codex_home(home)
    }
}
pub mod types {
    use std::path::PathBuf;

    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct CubeRepoHandle {
        pub repo_id: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct CubeWorkspaceLease {
        pub lease_id: String,
        pub workspace_id: String,
        pub workspace_path: PathBuf,
        /// Cube's positive recovery check, reported only on an `--allow-dirty`
        /// reclaim:
        ///
        /// - `Some(true)`  — the workspace still held work that exists on no
        ///   remote, i.e. cube recovered the dead worker's tree in place. This is
        ///   the good path: the work is live, with its jj operation log intact.
        /// - `Some(false)` — the lease succeeded but there was nothing left to
        ///   recover (the tree had already been reset). The caller must fall back
        ///   to the engine's saved recovery patch.
        /// - `None`        — not a recovery lease; the field does not apply.
        ///
        /// The distinction matters because lease success alone never proved
        /// recovery: a caller that inferred it would start from an empty tree
        /// while believing it was resuming.
        pub dirty_verified: Option<bool>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct CubeChangeHandle {
        pub change_id: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq, bon::Builder, serde::Serialize, serde::Deserialize)]
    #[builder(on(String, into))]
    pub struct CubeWorkspaceStatus {
        pub workspace_id: String,
        pub workspace_path: PathBuf,
        pub state: String,
        pub lease_id: Option<String>,
        pub holder: Option<String>,
        pub task: Option<String>,
        /// Exact task identity recorded by Cube when the previous lease ended.
        #[serde(default)]
        pub last_task: Option<String>,
        pub leased_at_epoch_s: Option<i64>,
        /// Cube's monotonic claim counter for this workspace. Absent from older
        /// cube builds.
        #[serde(default)]
        pub lease_generation: Option<i64>,
        pub lease_expires_at_epoch_s: Option<i64>,
    }

    /// Pool-config view of a repo as returned by `cube repo list --json`.
    /// Supplies pool defaults to the hub scheduler and registered paths to the executor.
    #[derive(Debug, Clone, PartialEq, Eq, bon::Builder, serde::Serialize, serde::Deserialize)]
    #[builder(on(String, into))]
    pub struct CubeRepoSummary {
        #[serde(rename = "repo")]
        pub repo_id: String,
        pub origin: String,
        pub main_branch: String,
        pub workspace_root: PathBuf,
        pub workspace_prefix: String,
        #[serde(default)]
        pub source: Option<PathBuf>,
    }

    /// Registry ownership used by the host executor; the hub's legacy status view
    /// remains the same flattened status type.
    #[derive(serde::Deserialize)]
    pub struct RegisteredWorkspace {
        #[serde(default)]
        pub repo: String,
        #[serde(flatten)]
        pub status: CubeWorkspaceStatus,
    }
}
mod validation {
    use anyhow::{Result, bail, ensure};
    use boss_protocol_spoke::v1::{Bookmark, Run, WorkspaceKind as Kind, WorkspaceOp};

    fn id(value: &str) -> Result<()> {
        ensure!(
            !value.is_empty()
                && value.len() <= 256
                && value != "."
                && value != ".."
                && !value.starts_with('-')
                && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "invalid opaque identifier"
        );
        Ok(())
    }

    pub(crate) fn store_operation(kind: Kind) -> bool {
        matches!(
            kind,
            Kind::CheckpointBaseline | Kind::CheckPreservedUnpublished | Kind::ForgetPreserved
        )
    }

    /// Operations retired together with execution-pointer reconciliation. A peer
    /// that still sends one is told so, rather than having it silently ignored.
    pub(crate) fn retired_operation(kind: Kind) -> bool {
        matches!(
            kind,
            Kind::RestoreRebased | Kind::PrepareRecoveryPr | Kind::ReconcileBookmark
        )
    }

    fn reference(value: &str) -> Result<()> {
        ensure!(
            !value.is_empty()
                && value.len() <= 512
                && !value.starts_with(['-', '/'])
                && !value.contains("..")
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/._-@".contains(&b)),
            "invalid revision reference"
        );
        Ok(())
    }

    pub(crate) fn commit(value: &str) -> Result<()> {
        ensure!(
            matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit()),
            "expected full commit identity"
        );
        Ok(())
    }

    fn bookmark(value: &Bookmark, repo: &str) -> Result<()> {
        id(&value.execution_id)?;
        ensure!(
            value
                .execution_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_'),
            "invalid recovery execution id"
        );
        ensure!(value.repo_id == repo, "bookmark belongs to another repository");
        if !value.anchor_commit.is_empty() {
            commit(&value.anchor_commit)?;
        }
        if !value.pr_head_before.is_empty() {
            commit(&value.pr_head_before)?;
        }
        Ok(())
    }

    /// Each kind has an explicit field allowlist. Unused fields are rejected, so a
    /// workspace command cannot become an argv/path tunnel as the schema evolves.
    pub fn validate_workspace(op: &WorkspaceOp, run: Option<&Run>) -> Result<Kind> {
        let kind = Kind::try_from(op.kind)?;
        ensure!(
            !retired_operation(kind),
            "workspace operation {kind:?} is retired and no longer supported"
        );
        let mut allowed = WorkspaceOp {
            kind: op.kind,
            ..Default::default()
        };
        if !matches!(kind, Kind::EnsureRepo | Kind::ListRepos | Kind::ListWorkspaces) {
            let run = run.ok_or_else(|| anyhow::anyhow!("workspace mutation requires assignment"))?;
            id(&run.execution_id)?;
            ensure!(
                run.execution_id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
                "invalid recovery execution id"
            );
            id(&run.run_id)?;
            id(&run.spawn_token)?;
            ensure!(run.assignment_generation > 0, "missing assignment generation");
            id(&op.repo_id)?;
            allowed.repo_id.clone_from(&op.repo_id);
        }
        if !store_operation(kind)
            && !matches!(
                kind,
                Kind::EnsureRepo | Kind::ListRepos | Kind::ListWorkspaces | Kind::Lease
            )
        {
            id(&op.workspace_id)?;
            id(&op.lease_id)?;
            allowed.workspace_id.clone_from(&op.workspace_id);
            allowed.lease_id.clone_from(&op.lease_id);
        }
        match kind {
            Kind::WorkspaceUnspecified => bail!("unspecified workspace operation"),
            Kind::EnsureRepo => {
                let origin = &op.origin;
                ensure!(
                    !origin.is_empty()
                        && origin.len() <= 2048
                        && !origin.starts_with(['-', '/'])
                        && !origin.contains("..")
                        && origin
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b":/._-@".contains(&b))
                        && (origin.starts_with("https://")
                            || origin.starts_with("ssh://")
                            || (origin.contains('@') && origin.contains(':'))
                            || !origin.contains(':')),
                    "unsupported repository origin"
                );
                if !origin.contains(':') {
                    // The shared CLI builder passes non-slugs as --origin.
                    // A slash-only reference would therefore be a local path.
                    id(origin)?;
                }
                allowed.origin.clone_from(origin);
            }
            Kind::Lease => {
                if !op.workspace_id.is_empty() {
                    id(&op.workspace_id)?;
                }
                ensure!(
                    !op.allow_dirty || !op.workspace_id.is_empty(),
                    "dirty reclaim requires a workspace"
                );
                ensure!(
                    op.task_label.len() <= 1024 && !op.task_label.contains('\0'),
                    "invalid task label"
                );
                ensure!(op.exclude_workspace_ids.len() <= 256, "too many workspace exclusions");
                for value in &op.exclude_workspace_ids {
                    id(value)?;
                }
                ensure!(
                    !op.exclude_workspace_ids.contains(&op.workspace_id),
                    "preferred workspace is excluded"
                );
                allowed.workspace_id.clone_from(&op.workspace_id);
                allowed.task_label.clone_from(&op.task_label);
                allowed.allow_dirty = op.allow_dirty;
                allowed.exclude_workspace_ids.clone_from(&op.exclude_workspace_ids);
            }
            Kind::Heartbeat => {
                ensure!(
                    op.ttl_seconds <= i64::MAX as u64,
                    "lease TTL exceeds host integer range"
                );
                allowed.ttl_seconds = op.ttl_seconds;
            }
            Kind::ForceRelease => {
                ensure!(
                    !op.reason.is_empty() && op.reason.len() <= 4096 && !op.reason.contains('\0'),
                    "force release requires a bounded reason"
                );
                allowed.reason.clone_from(&op.reason);
            }
            Kind::CreateChange => {
                ensure!(
                    !op.title.is_empty() && op.title.len() <= 4096 && !op.title.contains('\0'),
                    "change requires a bounded title"
                );
                allowed.title.clone_from(&op.title);
            }
            Kind::GotoPr => {
                ensure!(op.pr_number > 0, "missing PR number");
                allowed.pr_number = op.pr_number;
            }
            Kind::GotoRevision => {
                commit(&op.revision)?;
                allowed.revision.clone_from(&op.revision);
            }
            Kind::CreateBookmark => {
                if let Some(predecessor) = &op.predecessor {
                    bookmark(predecessor, &op.repo_id)?;
                }
                if !op.inherited_base.is_empty() {
                    reference(&op.inherited_base)?;
                }
                allowed.predecessor.clone_from(&op.predecessor);
                allowed.inherited_base.clone_from(&op.inherited_base);
            }
            Kind::RestoreRebased | Kind::PrepareRecoveryPr | Kind::ReconcileBookmark => {
                unreachable!("retired operations are rejected before field validation")
            }
            Kind::PositionedParent | Kind::StartFresh => {}
            Kind::ConfirmRecovery
            | Kind::RestorePreserved
            | Kind::CheckPreservedUnpublished
            | Kind::ForgetPreserved => {
                ensure!(
                    !op.request_json.is_empty(),
                    "recovery operation requires its request payload"
                );
                let request = crate::recovery_ops::parse_request(&op.request_json)?;
                if kind == Kind::CheckPreservedUnpublished {
                    ensure!(
                        request.recorded.is_some(),
                        "preserved-work check requires the recorded tip"
                    );
                }
                allowed.request_json.clone_from(&op.request_json);
            }
            Kind::Release
            | Kind::ReleaseWithReset
            | Kind::Status
            | Kind::ListWorkspaces
            | Kind::ListRepos
            | Kind::CheckpointBaseline => {}
        }
        if matches!(
            kind,
            Kind::CheckpointBaseline
                | Kind::ConfirmRecovery
                | Kind::RestorePreserved
                | Kind::CheckPreservedUnpublished
                | Kind::ForgetPreserved
        ) {
            let value = op
                .bookmark
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing recovery bookmark"))?;
            bookmark(value, &op.repo_id)?;
            ensure!(
                value.execution_id == run.expect("validated assignment").execution_id,
                "bookmark belongs to another execution"
            );
            allowed.bookmark.clone_from(&op.bookmark);
        }
        ensure!(*op == allowed, "fields not permitted for workspace operation");
        Ok(kind)
    }
}
pub mod workspace {
    use crate::{
        cube::{self, CubeJsonTransport},
        types::{CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus},
        validate_workspace,
    };
    use anyhow::{Context, Result, ensure};
    use boss_engine_recovery::execution_bookmark::{self as recovery, ExecutionBookmark, Jj, LocalJj};
    use boss_protocol_spoke::{
        Validate,
        v1::{
            Bookmark, Command, CommandResult, ResultCode, Run, WorkspaceKind as Kind, WorkspaceOp, WorkspaceResult,
            command, command_result,
        },
    };
    use serde_json::json;
    use std::path::{Path, PathBuf};

    /// A serialized host executor; callers persist attempting before invoking it.
    /// It accepts identifiers only. Cube's local registry supplies filesystem paths.
    pub struct Executor<T> {
        pub cube: T,
        pub host_id: String,
    }

    /// Stable cube task ownership across reconnects and lost acquisition replies.
    pub fn lease_owner(run: &Run) -> String {
        format!(
            "boss-spoke:{}:{}:{}:{}",
            run.execution_id, run.run_id, run.assignment_generation, run.spawn_token
        )
    }

    fn owns_task(task: Option<&str>, owner: &str) -> bool {
        task.and_then(|task| task.lines().next()) == Some(owner)
    }

    fn optional(value: &str) -> Option<&str> {
        (!value.is_empty()).then_some(value)
    }

    #[derive(Default)]
    enum Phase {
        #[default]
        Authorizing,
        Effect,
    }
    impl Phase {
        fn failure(&self) -> ResultCode {
            match self {
                Self::Authorizing => ResultCode::Rejected,
                Self::Effect => ResultCode::OutcomeUncertain,
            }
        }
    }

    impl<T: CubeJsonTransport> Executor<T> {
        pub async fn execute(&self, command: &Command, interrupted: bool, now_ms: i64) -> CommandResult {
            let mut result = CommandResult {
                command_id: command.command_id.clone(),
                ..Default::default()
            };
            let validated = command.validate().map_err(anyhow::Error::from).and_then(|()| {
                let Some(command::Body::Workspace(op)) = &command.body else {
                    anyhow::bail!("execution capability unavailable");
                };
                Ok((op, validate_workspace(op, command.run.as_ref())?))
            });
            let (op, kind) = match validated {
                Ok(value) => value,
                Err(error) => {
                    result.code = ResultCode::Rejected.into();
                    result.reason = error.to_string();
                    return result;
                }
            };
            if !interrupted && command.not_after_unix_ms <= now_ms {
                result.code = ResultCode::Rejected.into();
                result.reason = "command expired".into();
                return result;
            }
            // Reads, acquisition and release reconciliation may run after an uncertain
            // effect. In particular, replaying a heartbeat would extend a relative
            // TTL beyond the original attempt; that policy decision belongs to the hub.
            if interrupted
                && !matches!(
                    kind,
                    Kind::Lease
                        | Kind::Release
                        | Kind::ReleaseWithReset
                        | Kind::ForceRelease
                        | Kind::ListRepos
                        | Kind::ListWorkspaces
                        | Kind::Status
                        | Kind::PositionedParent
                        | Kind::CheckPreservedUnpublished
                )
            {
                result.code = ResultCode::OutcomeUncertain.into();
                result.reason = "workspace operation interrupted; hub reconciliation required".into();
                return result;
            }
            let mut phase = Phase::Authorizing;
            let outcome = tokio::time::timeout(
                std::time::Duration::from_secs(300),
                self.dispatch(command, op, kind, interrupted, &mut phase),
            )
            .await;
            match outcome {
                Ok(Ok((value, uncertain))) => {
                    result.code = if uncertain {
                        ResultCode::OutcomeUncertain
                    } else {
                        ResultCode::Ok
                    }
                    .into();
                    if uncertain {
                        result.reason = "lease ownership recovered; setup completion is unknown".into();
                    }
                    result.value = Some(command_result::Value::Workspace(value));
                }
                Ok(Err(error)) => {
                    result.code = phase.failure().into();
                    result.reason = format!("workspace operation failed: {error:#}");
                    if let Some(cube_error) = error.downcast_ref::<cube::CubeCliError>()
                        && cube_error.positioning_refused()
                    {
                        // A cube refusal is definitive even after entering the effect phase.
                        // Preserve the typed diagnostic across the spoke boundary.
                        result.code = ResultCode::Rejected.into();
                        let mut diagnostic = cube_error.clone();
                        diagnostic.host = self.host_id.clone();
                        result.value = Some(command_result::Value::Workspace(WorkspaceResult {
                            cube_result_json: serde_json::to_vec(&diagnostic).expect("cube error is serializable"),
                            ..Default::default()
                        }));
                    }
                }
                Err(_) => {
                    result.code = phase.failure().into();
                    result.reason = "workspace operation timed out".into();
                }
            }
            // Keep failure diagnostics bounded too; never truncate a success payload.
            result.reason = boss_engine_utils::string_clip::clip_to_bytes_with_suffix(&result.reason, 4096, "…");
            result
        }

        async fn repo(&self, id: &str) -> Result<CubeRepoSummary> {
            cube::list_repos(&self.cube)
                .await?
                .into_iter()
                .find(|repo| repo.repo_id == id)
                .context("repository is not registered on this host")
        }

        fn path(repo: &CubeRepoSummary, workspace: &CubeWorkspaceStatus) -> Result<PathBuf> {
            let root = repo
                .workspace_root
                .canonicalize()
                .context("workspace root unavailable")?;
            let path = workspace
                .workspace_path
                .canonicalize()
                .context("workspace unavailable")?;
            ensure!(
                path.parent() == Some(root.as_path())
                    && path
                        .file_name()
                        .is_some_and(|name| name == workspace.workspace_id.as_str()),
                "workspace path escapes registered root"
            );
            ensure!(
                !std::fs::symlink_metadata(&workspace.workspace_path)?
                    .file_type()
                    .is_symlink(),
                "workspace path is a symlink"
            );
            Ok(path)
        }

        async fn owned(&self, op: &WorkspaceOp, run: &Run) -> Result<(PathBuf, CubeWorkspaceStatus)> {
            let row = cube::registered_workspaces(&self.cube)
                .await?
                .into_iter()
                .find(|row| row.status.workspace_id == op.workspace_id && row.repo == op.repo_id)
                .context("workspace is not registered in repository")?;
            let ws = row.status;
            ensure!(
                ws.state == "leased"
                    && ws.lease_id.as_deref() == Some(&op.lease_id)
                    && owns_task(ws.task.as_deref(), &lease_owner(run)),
                "workspace lease belongs to another assignment"
            );
            // Lease administration addresses cube's recorded lease, even when its
            // working directory disappeared. Only filesystem operations open it.
            let path = if matches!(
                Kind::try_from(op.kind)?,
                Kind::Release | Kind::ReleaseWithReset | Kind::ForceRelease | Kind::Heartbeat | Kind::Status
            ) {
                ws.workspace_path.clone()
            } else {
                Self::path(&self.repo(&op.repo_id).await?, &ws)?
            };
            Ok((path, ws))
        }

        async fn acquire(
            &self,
            op: &WorkspaceOp,
            run: &Run,
            interrupted: bool,
            phase: &mut Phase,
        ) -> Result<(WorkspaceResult, bool)> {
            let repo = self.repo(&op.repo_id).await?;
            let owner = lease_owner(run);
            let existing: Vec<_> = cube::registered_workspaces(&self.cube)
                .await?
                .into_iter()
                .filter(|row| owns_task(row.status.task.as_deref(), &owner) && row.status.state == "leased")
                .collect();
            ensure!(
                existing.len() <= 1,
                "assignment owns multiple leases; reconciliation required"
            );
            if let Some(row) = existing.first() {
                ensure!(
                    row.repo == op.repo_id,
                    "assignment already owns a lease in another repository"
                );
                let ws = &row.status;
                let path = Self::path(&repo, ws)?;
                return Ok((
                    WorkspaceResult {
                        repo_id: op.repo_id.clone(),
                        workspace_id: ws.workspace_id.clone(),
                        lease_id: ws.lease_id.clone().context("recorded lease has no identity")?,
                        display_path: path.display().to_string(),
                        cube_result_json: serde_json::to_vec(&CubeWorkspaceLease {
                            workspace_id: ws.workspace_id.clone(),
                            workspace_path: ws.workspace_path.clone(),
                            lease_id: ws.lease_id.clone().context("recorded lease has no identity")?,
                            dirty_verified: None,
                        })?,
                        ..Default::default()
                    },
                    true,
                ));
            }
            ensure!(
                !interrupted,
                "interrupted acquisition has no recorded owner; refusing a second acquisition"
            );
            let exclusions: Vec<_> = op.exclude_workspace_ids.iter().map(String::as_str).collect();
            let task = if op.task_label.is_empty() {
                owner
            } else {
                format!("{owner}\n{}", op.task_label)
            };
            *phase = Phase::Effect;
            let lease = cube::lease_workspace(
                &self.cube,
                &op.repo_id,
                &task,
                optional(&op.workspace_id),
                op.allow_dirty,
                &exclusions,
            )
            .await?;
            let bound = WorkspaceOp {
                workspace_id: lease.workspace_id.clone(),
                lease_id: lease.lease_id.clone(),
                ..op.clone()
            };
            let (path, _) = self.owned(&bound, run).await?;
            Ok((
                WorkspaceResult {
                    repo_id: op.repo_id.clone(),
                    workspace_id: lease.workspace_id.clone(),
                    lease_id: lease.lease_id.clone(),
                    display_path: path.display().to_string(),
                    cube_result_json: serde_json::to_vec(&lease)?,
                    ..Default::default()
                },
                false,
            ))
        }

        /// After authorization, cube may commit a release and then lose its
        /// reply. Its registry proves whether this exact lease is still held.
        async fn release(&self, op: &WorkspaceOp, kind: Kind) -> Result<()> {
            let outcome = match kind {
                Kind::ReleaseWithReset => cube::release_workspace_with_reset(&self.cube, &op.lease_id).await,
                Kind::ForceRelease => cube::force_release_lease(&self.cube, &op.lease_id, Some(&op.reason)).await,
                _ => cube::release_workspace(&self.cube, &op.lease_id).await,
            };
            if outcome.is_err() && self.lease_is_gone(op).await? {
                return Ok(());
            }
            outcome
        }

        async fn lease_is_gone(&self, op: &WorkspaceOp) -> Result<bool> {
            Ok(!cube::registered_workspaces(&self.cube)
                .await?
                .iter()
                .any(|row| row.status.lease_id.as_deref() == Some(&op.lease_id)))
        }

        async fn dispatch(
            &self,
            command: &Command,
            op: &WorkspaceOp,
            kind: Kind,
            interrupted: bool,
            phase: &mut Phase,
        ) -> Result<(WorkspaceResult, bool)> {
            let run = command.run.as_ref();
            let mut result = WorkspaceResult {
                repo_id: op.repo_id.clone(),
                workspace_id: op.workspace_id.clone(),
                lease_id: op.lease_id.clone(),
                ..Default::default()
            };
            let mut value = json!({});
            // A replay of a journaled, interrupted release may arrive after
            // cube ended the lease. Never touch a newer holder of the workspace.
            if interrupted
                && matches!(kind, Kind::Release | Kind::ReleaseWithReset | Kind::ForceRelease)
                && self.lease_is_gone(op).await?
            {
                result.cube_result_json = serde_json::to_vec(&value)?;
                return Ok((result, false));
            }
            match kind {
                Kind::EnsureRepo => {
                    *phase = Phase::Effect;
                    result.repo_id = cube::ensure_repo(&self.cube, &op.origin).await?.repo_id;
                }
                Kind::ListRepos => {
                    value = serde_json::to_value(cube::list_repos(&self.cube).await?)?;
                }
                Kind::ListWorkspaces => {
                    value = serde_json::to_value(cube::list_workspaces(&self.cube).await?)?;
                }
                Kind::Lease => return self.acquire(op, run.expect("validated run"), interrupted, phase).await,
                _ => {
                    let run = run.expect("validated run");
                    let (path, status) = if crate::validation::store_operation(kind) {
                        let source = self
                            .repo(&op.repo_id)
                            .await?
                            .source
                            .context("registered repository has no shared source")?;
                        (source.canonicalize()?, None)
                    } else {
                        let (path, status) = self.owned(op, run).await?;
                        (path, Some(status))
                    };
                    result.display_path = path.display().to_string();
                    if matches!(
                        kind,
                        Kind::Release
                            | Kind::ReleaseWithReset
                            | Kind::ForceRelease
                            | Kind::Heartbeat
                            | Kind::CreateChange
                            | Kind::GotoPr
                            | Kind::GotoRevision
                            | Kind::StartFresh
                    ) {
                        *phase = Phase::Effect;
                    }
                    match kind {
                        Kind::Release | Kind::ReleaseWithReset | Kind::ForceRelease => self.release(op, kind).await?,
                        Kind::Heartbeat => {
                            cube::heartbeat_lease(
                                &self.cube,
                                &op.lease_id,
                                (op.ttl_seconds != 0).then_some(op.ttl_seconds),
                            )
                            .await?
                        }
                        Kind::Status => value = serde_json::to_value(status.expect("workspace status"))?,
                        Kind::CreateChange => {
                            result.change_id = cube::create_change(&self.cube, &path, &op.title).await?.change_id
                        }
                        Kind::GotoPr => {
                            cube::goto_workspace(&self.cube, &path, "--pr", &op.pr_number.to_string()).await?
                        }
                        Kind::GotoRevision => {
                            cube::goto_workspace(&self.cube, &path, "--revision", &op.revision).await?
                        }
                        Kind::PositionedParent => value = crate::recovery_ops::positioned_parent(&path).await?,
                        Kind::StartFresh => value = crate::recovery_ops::start_fresh(&path).await?,
                        _ => {
                            let record = self.record(&path, &run.execution_id, op.bookmark.as_ref()).await?;
                            value = match kind {
                                Kind::CreateBookmark => {
                                    let predecessor = match &op.predecessor {
                                        Some(p) => Some(self.record(&path, &p.execution_id, Some(p)).await?),
                                        None => None,
                                    };
                                    *phase = Phase::Effect;
                                    let created = recovery::create_from(
                                        &LocalJj,
                                        &path,
                                        &run.execution_id,
                                        &self.host_id,
                                        predecessor.as_ref(),
                                        optional(&op.inherited_base),
                                    )
                                    .await?;
                                    result.bookmark = Some(wire_bookmark(&op.repo_id, &created));
                                    json!({})
                                }
                                Kind::CheckpointBaseline => {
                                    *phase = Phase::Effect;
                                    json!(record.base_commit.clone().unwrap_or_default())
                                }
                                Kind::ConfirmRecovery => {
                                    *phase = Phase::Effect;
                                    let request = crate::recovery_ops::parse_request(&op.request_json)?;
                                    crate::recovery_ops::confirm(&record, &path, &request).await?
                                }
                                Kind::RestorePreserved => {
                                    *phase = Phase::Effect;
                                    let request = crate::recovery_ops::parse_request(&op.request_json)?;
                                    crate::recovery_ops::restore_preserved(&record, &path, &request).await?
                                }
                                Kind::CheckPreservedUnpublished => {
                                    let request = crate::recovery_ops::parse_request(&op.request_json)?;
                                    crate::recovery_ops::preserved_unpublished(&record, &request).await?
                                }
                                Kind::ForgetPreserved => {
                                    *phase = Phase::Effect;
                                    let request = crate::recovery_ops::parse_request(&op.request_json)?;
                                    crate::recovery_ops::forget_preserved(&record, &request).await?
                                }
                                _ => anyhow::bail!("unsupported workspace operation"),
                            };
                        }
                    }
                }
            }
            result.cube_result_json = serde_json::to_vec(&value)?;
            Ok((result, false))
        }

        async fn record(&self, path: &Path, execution: &str, bookmark: Option<&Bookmark>) -> Result<ExecutionBookmark> {
            Ok(ExecutionBookmark {
                execution_id: execution.into(),
                repo_path: LocalJj.shared_repo(path).await?,
                host_id: self.host_id.clone(),
                base_commit: bookmark.and_then(|b| optional(&b.anchor_commit)).map(str::to_owned),
                pr_head_before: bookmark.and_then(|b| optional(&b.pr_head_before)).map(str::to_owned),
            })
        }
    }

    fn wire_bookmark(repo: &str, record: &ExecutionBookmark) -> Bookmark {
        Bookmark {
            repo_id: repo.into(),
            execution_id: record.execution_id.clone(),
            anchor_commit: record.base_commit.clone().unwrap_or_default(),
            pr_head_before: record.pr_head_before.clone().unwrap_or_default(),
        }
    }

    /// Discover the recovery remote for both host execution and hub PR repair.
    pub async fn recovery_remote(jj: &dyn Jj, workspace: &Path) -> Result<String> {
        let remotes = jj.run(workspace, &["git", "remote", "list"]).await?;
        Ok(git_utils::repo_slug::parse_github_remote(&remotes)
            .map(|(name, _)| name)
            .unwrap_or_else(|| "origin".into()))
    }
}

pub use validation::validate_workspace;

/// Recovery-PR base lookup shared by every `recovery_pr_base` default.
pub mod recovery_pr {
    /// The one implementation behind every `recovery_pr_base` default.
    pub async fn fetch_recovery_pr_base(origin: &str, pr: u64) -> anyhow::Result<String> {
        // A non-GitHub origin can never yield a PR base; every other failure here
        // (gh, network) is transient and must stay retryable.
        let slug = git_utils::repo_slug::parse_github_slug(origin).ok_or_else(|| {
            boss_engine_recovery::execution_bookmark::base_unresolvable_error(format!(
                "origin `{origin}` is not a GitHub repository"
            ))
        })?;
        git_utils::gh_cli::fetch_open_pr_base(&slug, pr)
            .await?
            .ok_or_else(closed_recovery_pr_error)
    }

    /// Typed error for a recovery PR that is no longer open; callers downcast to
    /// it (`err.is::<ClosedRecoveryPr>()`) to tell a closed PR from a transient
    /// lookup failure.
    #[derive(Debug)]
    pub struct ClosedRecoveryPr;

    pub fn closed_recovery_pr_error() -> anyhow::Error {
        ClosedRecoveryPr.into()
    }

    impl std::fmt::Display for ClosedRecoveryPr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("PR is not open")
        }
    }
    impl std::error::Error for ClosedRecoveryPr {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use boss_protocol_spoke::v1::{self, WorkspaceKind as Kind, command, command_result};
    use serde_json::{Value, json};
    use std::{path::PathBuf, sync::Mutex};

    pub(super) fn run() -> v1::Run {
        v1::Run {
            execution_id: "exec_test".into(),
            run_id: "run-test".into(),
            spawn_token: "token".into(),
            assignment_generation: 1,
        }
    }
    pub(super) fn op(kind: Kind) -> v1::WorkspaceOp {
        v1::WorkspaceOp {
            kind: kind.into(),
            repo_id: "repo".into(),
            workspace_id: "worker".into(),
            lease_id: "lease".into(),
            ..Default::default()
        }
    }
    pub(super) fn command(op: v1::WorkspaceOp) -> v1::Command {
        v1::Command {
            sequence: 1,
            command_id: "command".into(),
            run: Some(run()),
            not_after_unix_ms: 1000,
            body: Some(command::Body::Workspace(op)),
        }
    }
    #[derive(bon::Builder)]
    #[builder(on(String, into))]
    pub(super) struct Cube {
        #[builder(default = true)]
        lost_reply: bool,
        #[builder(default)]
        lose_release_reply: bool,
        #[builder(default)]
        refuse_positioning: bool,
        root: PathBuf,
        path: PathBuf,
        #[builder(default)]
        pub(super) owner: Mutex<Option<String>>,
        #[builder(default)]
        calls: Mutex<Vec<Vec<String>>>,
    }
    impl Cube {
        pub(super) fn new(root: &std::path::Path) -> Self {
            Self::builder().root(root.into()).path(root.join("worker")).build()
        }
        fn lease_calls(&self) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|args| args.get(2).is_some_and(|arg| arg == "lease"))
                .count()
        }
    }
    #[async_trait]
    impl cube::CubeJsonTransport for Cube {
        async fn run_cube_json(&self, args: &[&str]) -> Result<Value> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| arg.to_string()).collect());
            match args {
                ["--json", "repo", "list"] => Ok(
                    json!({"repos": [{"repo": "repo", "origin": "https://github.com/example/repo", "main_branch": "main", "workspace_root": self.root, "workspace_prefix": "worker", "source": self.root.join("shared")}]}),
                ),
                ["--json", "workspace", "list"] => {
                    let owner = self.owner.lock().unwrap();
                    Ok(
                        json!({"workspaces": [{"repo": "repo", "workspace_id": "worker", "workspace_path": self.path, "state": if owner.is_some() { "leased" } else { "free" }, "lease_id": owner.as_ref().map(|_| "lease"), "task": *owner}]}),
                    )
                }
                ["--json", "workspace", "lease", _, "--task", owner, ..] => {
                    *self.owner.lock().unwrap() = Some((*owner).into());
                    if self.lost_reply {
                        anyhow::bail!("reply lost after cube committed owner");
                    }
                    Ok(
                        json!({"workspace": {"workspace_id": "worker", "lease_id": "lease", "workspace_path": self.path}}),
                    )
                }
                ["--json", "workspace", "release" | "force-release", ..] if self.lose_release_reply => {
                    *self.owner.lock().unwrap() = None;
                    anyhow::bail!("reply lost after cube released lease");
                }
                ["--json", "workspace", "goto", ..] if self.refuse_positioning => Err(cube::CubeCliError {
                    host: "local".into(),
                    exit_code: Some(2),
                    stdout: String::new(),
                    stderr: json!({"error_kind":"workspace_positioning_refused",
                            "retryable":false, "error":"preserve abc123 before positioning def456"})
                    .to_string(),
                }
                .into()),
                ["--json", "change", "create", ..] => Ok(json!({"change": {"change_id": "change"}})),
                _ => Ok(json!({})),
            }
        }
    }

    #[tokio::test]
    async fn interrupted_acquisition_reconciles_owner_without_second_lease() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("worker")).unwrap();
        let executor = workspace::Executor {
            cube: Cube::new(root.path()),
            host_id: "host".into(),
        };
        let command = command(v1::WorkspaceOp {
            kind: Kind::Lease.into(),
            repo_id: "repo".into(),
            ..Default::default()
        });
        assert_eq!(
            executor.execute(&command, false, 1).await.code,
            v1::ResultCode::OutcomeUncertain as i32
        );
        let result = executor.execute(&command, true, 2000).await;
        assert_eq!(result.code, v1::ResultCode::OutcomeUncertain as i32);
        let Some(command_result::Value::Workspace(lease)) = result.value else {
            panic!("missing recovered lease");
        };
        let decoded: types::CubeWorkspaceLease = serde_json::from_slice(&lease.cube_result_json).unwrap();
        assert_eq!(decoded.dirty_verified, None);
        assert_eq!(decoded.lease_id, "lease");
        assert_eq!(lease.lease_id, "lease");
        assert_eq!(lease.workspace_id, "worker");
        assert_eq!(executor.cube.lease_calls(), 1);
    }

    #[tokio::test]
    async fn missing_owner_after_interruption_never_acquires_again() {
        let root = tempfile::tempdir().unwrap();
        let executor = workspace::Executor {
            cube: Cube::new(root.path()),
            host_id: "host".into(),
        };
        let result = executor
            .execute(
                &command(v1::WorkspaceOp {
                    kind: Kind::Lease.into(),
                    repo_id: "repo".into(),
                    ..Default::default()
                }),
                true,
                1,
            )
            .await;
        assert_eq!(result.code, v1::ResultCode::Rejected as i32);
        assert_eq!(executor.cube.lease_calls(), 0);
    }

    #[tokio::test]
    async fn lease_administration_does_not_require_a_working_directory() {
        let root = tempfile::tempdir().unwrap();
        let cube = Cube::new(root.path());
        *cube.owner.lock().unwrap() = Some(workspace::lease_owner(&run()));
        let executor = workspace::Executor {
            cube,
            host_id: "host".into(),
        };
        assert!(!executor.cube.path.exists());
        for kind in [Kind::Heartbeat, Kind::Release, Kind::ForceRelease] {
            let mut request = op(kind);
            if kind == Kind::ForceRelease {
                request.reason = "missing workspace".into();
            }
            let result = executor.execute(&command(request), false, 1).await;
            assert_eq!(result.code, v1::ResultCode::Ok as i32, "{}", result.reason);
        }
    }

    #[tokio::test]
    async fn rejects_foreign_lease_and_symlink_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let cube = Cube::new(root.path());
        *cube.owner.lock().unwrap() = Some("other assignment".into());
        std::os::unix::fs::symlink(outside.path(), &cube.path).unwrap();
        let executor = workspace::Executor {
            cube,
            host_id: "host".into(),
        };
        let request = command(v1::WorkspaceOp {
            title: "must not write through symlink".into(),
            ..op(Kind::CreateChange)
        });
        assert_eq!(
            executor.execute(&request, false, 1).await.code,
            v1::ResultCode::Rejected as i32
        );
        *executor.cube.owner.lock().unwrap() = Some(workspace::lease_owner(&run()));
        assert_eq!(
            executor.execute(&request, false, 1).await.code,
            v1::ResultCode::Rejected as i32
        );
        assert!(outside.path().exists());
        assert!(executor.cube.calls.lock().unwrap().iter().all(|args| args[2] == "list"));
    }

    #[test]
    fn every_workspace_kind_has_required_and_forbidden_fields() {
        let assignment = run();
        let bookmark = v1::Bookmark {
            repo_id: "repo".into(),
            execution_id: assignment.execution_id.clone(),
            anchor_commit: "a".repeat(40),
            pr_head_before: String::new(),
        };
        for number in 1..=28 {
            // Retired kinds have their numbers reserved in the schema.
            let Ok(kind) = Kind::try_from(number) else {
                continue;
            };
            let mut request = op(kind);
            if crate::validation::retired_operation(kind) {
                let error = validate_workspace(&request, Some(&assignment)).unwrap_err();
                assert!(error.to_string().contains("retired"), "{kind:?}: {error}");
                continue;
            }
            match kind {
                Kind::EnsureRepo | Kind::ListRepos | Kind::ListWorkspaces => {
                    request = v1::WorkspaceOp {
                        kind: kind.into(),
                        ..Default::default()
                    };
                    if kind == Kind::EnsureRepo {
                        request.origin = "https://github.com/example/repo".into();
                    }
                }
                Kind::Lease => {
                    request.lease_id.clear();
                }
                Kind::CreateChange => request.title = "Implement workspace execution".into(),
                Kind::GotoPr => request.pr_number = 1,
                Kind::GotoRevision => request.revision = "a".repeat(40),
                Kind::ForceRelease => request.reason = "hub authorized release".into(),
                Kind::ConfirmRecovery | Kind::RestorePreserved | Kind::ForgetPreserved => {
                    request.request_json = b"{}".to_vec();
                }
                Kind::CheckPreservedUnpublished => {
                    request.request_json = serde_json::to_vec(
                        &serde_json::json!({"recorded": {"commit_id": "a".repeat(40), "change_id": "k".repeat(32), "count": 1}}),
                    )
                    .unwrap();
                }
                _ => {}
            }
            if crate::validation::store_operation(kind) {
                request.workspace_id.clear();
                request.lease_id.clear();
            }
            if matches!(
                kind,
                Kind::CheckpointBaseline
                    | Kind::ConfirmRecovery
                    | Kind::RestorePreserved
                    | Kind::CheckPreservedUnpublished
                    | Kind::ForgetPreserved
            ) {
                request.bookmark = Some(bookmark.clone());
            }
            assert_eq!(validate_workspace(&request, Some(&assignment)).unwrap(), kind);
            request.title = "unexpected extra field".into();
            if kind == Kind::CreateChange {
                request.origin = "unexpected".into();
            }
            assert!(validate_workspace(&request, Some(&assignment)).is_err(), "{kind:?}");
        }
    }

    #[tokio::test]
    async fn rejects_wire_paths_revsets_and_unknown_operations_before_cube() {
        let root = tempfile::tempdir().unwrap();
        let executor = workspace::Executor {
            cube: Cube::new(root.path()),
            host_id: "host".into(),
        };
        let mut invalid = vec![
            op(Kind::WorkspaceUnspecified),
            v1::WorkspaceOp {
                kind: 999,
                ..Default::default()
            },
        ];
        for path in ["/tmp/worker", "../worker", "--workspace", "."] {
            invalid.push(v1::WorkspaceOp {
                workspace_id: path.into(),
                ..op(Kind::Release)
            });
        }
        invalid.push(v1::WorkspaceOp {
            revision: "all()".into(),
            ..op(Kind::GotoRevision)
        });
        invalid.push(v1::WorkspaceOp {
            origin: "file:///tmp/repo".into(),
            kind: Kind::EnsureRepo.into(),
            ..Default::default()
        });
        for origin in [".", "./repo", "owner/repo", "owner//repo", "owner/repo/subdir"] {
            invalid.push(v1::WorkspaceOp {
                origin: origin.into(),
                kind: Kind::EnsureRepo.into(),
                ..Default::default()
            });
        }
        for request in invalid {
            assert_eq!(
                executor.execute(&command(request), false, 1).await.code,
                v1::ResultCode::Rejected as i32
            );
        }
        assert!(executor.cube.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn interrupted_mutations_are_not_replayed() {
        let root = tempfile::tempdir().unwrap();
        let executor = workspace::Executor {
            cube: Cube::new(root.path()),
            host_id: "host".into(),
        };
        for request in [
            v1::WorkspaceOp {
                title: "new change".into(),
                ..op(Kind::CreateChange)
            },
            op(Kind::Heartbeat),
            v1::WorkspaceOp {
                kind: Kind::EnsureRepo.into(),
                origin: "https://github.com/example/repo".into(),
                ..Default::default()
            },
        ] {
            let result = executor.execute(&command(request), true, 2000).await;
            assert_eq!(result.code, v1::ResultCode::OutcomeUncertain as i32);
        }
        assert!(executor.cube.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn retired_pointer_operations_are_rejected_without_touching_cube() {
        let root = tempfile::tempdir().unwrap();
        let executor = workspace::Executor {
            cube: Cube::new(root.path()),
            host_id: "host".into(),
        };
        for kind in [Kind::RestoreRebased, Kind::PrepareRecoveryPr, Kind::ReconcileBookmark] {
            let result = executor.execute(&command(op(kind)), false, 1).await;
            assert_eq!(result.code, v1::ResultCode::Rejected as i32, "{kind:?}");
            assert!(result.reason.contains("retired"), "{kind:?}: {}", result.reason);
        }
        assert!(executor.cube.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn input_is_validated_before_provisioning_and_only_owned_home_is_reclaimed() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            input::provision(&boss_engine_driver::ClaudeDriver, root.path(), &[255], "invalid")
                .await
                .is_err()
        );
        assert!(!root.path().join(".claude").exists());
        let run_id: String = root
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        let state = input::provision(
            &boss_engine_driver::ClaudeDriver,
            root.path(),
            b"private input",
            &run_id,
        )
        .await
        .unwrap()
        .unwrap();
        let home = boss_engine_driver::initial_prompt::owned_home_from_runtime_state(&state).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join(".claude/initial-prompt.txt")).unwrap(),
            "private input"
        );
        input::reclaim(&state).unwrap();
        input::reclaim(&state).unwrap();
        assert!(!home.exists());
        let foreign = boss_protocol::DriverRuntimeState::new(json!({"initial_prompt_home": root.path()}));
        assert!(input::reclaim(&foreign).is_err());
        assert!(root.path().exists());
    }

    #[tokio::test]
    async fn spoke_preserves_structured_positioning_refusal() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("worker")).unwrap();
        let mut cube = Cube::new(root.path());
        cube.refuse_positioning = true;
        *cube.owner.lock().unwrap() = Some(workspace::lease_owner(&run()));
        let executor = workspace::Executor {
            cube,
            host_id: "remote-host".into(),
        };
        let mut request = op(Kind::GotoPr);
        request.pr_number = 42;
        let result = executor.execute(&command(request), false, 1).await;
        assert_eq!(result.code, v1::ResultCode::Rejected as i32);
        let Some(command_result::Value::Workspace(value)) = result.value else {
            panic!("missing structured refusal");
        };
        let error: cube::CubeCliError = serde_json::from_slice(&value.cube_result_json).unwrap();
        assert!(error.positioning_refused());
        assert_eq!(error.host, "remote-host");
        assert!(error.detail().contains("abc123"));
    }

    #[tokio::test]
    async fn lease_and_workspace_operations_use_registry_path_and_assignment() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("worker")).unwrap();
        let mut cube = Cube::new(root.path());
        cube.lost_reply = false;
        let executor = workspace::Executor {
            cube,
            host_id: "host".into(),
        };
        let lease = executor
            .execute(
                &command(v1::WorkspaceOp {
                    kind: Kind::Lease.into(),
                    repo_id: "repo".into(),
                    task_label: "workspace execution".into(),
                    ..Default::default()
                }),
                false,
                1,
            )
            .await;
        assert_eq!(lease.code, v1::ResultCode::Ok as i32, "{}", lease.reason);
        let Some(command_result::Value::Workspace(value)) = &lease.value else {
            panic!("missing lease")
        };
        let decoded: types::CubeWorkspaceLease = serde_json::from_slice(&value.cube_result_json).unwrap();
        assert_eq!(decoded.lease_id, "lease");
        assert_eq!(decoded.dirty_verified, None);
        for kind in [
            Kind::Status,
            Kind::Heartbeat,
            Kind::CreateChange,
            Kind::GotoPr,
            Kind::GotoRevision,
            Kind::Release,
            Kind::ReleaseWithReset,
            Kind::ForceRelease,
        ] {
            let mut request = op(kind);
            match kind {
                Kind::CreateChange => request.title = "workspace execution".into(),
                Kind::GotoPr => request.pr_number = 42,
                Kind::GotoRevision => request.revision = "a".repeat(40),
                Kind::ForceRelease => request.reason = "hub authorized".into(),
                _ => {}
            }
            let result = executor.execute(&command(request), false, 1).await;
            assert_eq!(result.code, v1::ResultCode::Ok as i32, "{kind:?}: {}", result.reason);
        }
        assert_eq!(executor.cube.lease_calls(), 1);
        let calls = executor.cube.calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|args| args == &["--json", "workspace", "release", "--lease", "lease", "--force-reset"])
        );
        let goto = calls
            .iter()
            .find(|args| args.get(2).is_some_and(|s| s == "goto"))
            .unwrap();
        assert_eq!(
            goto[4],
            root.path().join("worker").canonicalize().unwrap().to_str().unwrap()
        );
    }

    #[tokio::test]
    async fn release_reconciles_cube_commit_and_interrupted_replay() {
        for kind in [Kind::Release, Kind::ReleaseWithReset, Kind::ForceRelease] {
            let root = tempfile::tempdir().unwrap();
            let mut cube = Cube::new(root.path());
            cube.lose_release_reply = true;
            *cube.owner.lock().unwrap() = Some(workspace::lease_owner(&run()));
            let executor = workspace::Executor {
                cube,
                host_id: "host".into(),
            };
            let mut request = op(kind);
            if kind == Kind::ForceRelease {
                request.reason = "operator fenced".into();
            }
            let command = command(request);
            let result = executor.execute(&command, false, 1).await;
            assert_eq!(result.code, v1::ResultCode::Ok as i32, "{}", result.reason);
            let before = executor.cube.calls.lock().unwrap().len();
            let replay = executor.execute(&command, true, 1).await;
            assert_eq!(replay.code, v1::ResultCode::Ok as i32, "{}", replay.reason);
            let calls = executor.cube.calls.lock().unwrap();
            assert!(calls[before..].iter().all(|args| args[2] == "list"));
        }
    }

    #[tokio::test]
    async fn dispatch_creates_no_recovery_or_publication_refs() {
        use boss_engine_recovery::execution_bookmark::{Jj, LocalJj};
        let root = tempfile::tempdir().unwrap();
        let repo = boss_engine_test_git::jj::JjRepo::new(root.path());
        let cube = Cube::new(root.path());
        *cube.owner.lock().unwrap() = Some(workspace::lease_owner(&run()));
        let executor = workspace::Executor {
            cube,
            host_id: "host".into(),
        };
        std::fs::write(repo.worker.join("preserved.txt"), "unpublished").unwrap();
        LocalJj.run(&repo.worker, &["status"]).await.unwrap();
        let result = executor.execute(&command(op(Kind::CreateBookmark)), false, 1).await;
        assert_eq!(result.code, v1::ResultCode::Ok as i32, "{}", result.reason);
        let Some(command_result::Value::Workspace(created)) = result.value else {
            panic!("missing bookmark result")
        };
        let names = LocalJj
            .run(&repo.repo, &["bookmark", "list", "-T", "name ++ \"\\n\""])
            .await
            .unwrap();
        assert!(
            names.lines().all(|name| name == "main"),
            "dispatch must not create recovery or publication refs: {names}"
        );
        assert!(
            !created.bookmark.unwrap().anchor_commit.is_empty(),
            "dispatch records the commit it started from"
        );
    }

    #[tokio::test]
    async fn failed_read_is_rejected() {
        struct FailedRead;
        #[async_trait]
        impl cube::CubeJsonTransport for FailedRead {
            async fn run_cube_json(&self, _: &[&str]) -> Result<Value> {
                anyhow::bail!("registry unavailable")
            }
        }
        let executor = workspace::Executor {
            cube: FailedRead,
            host_id: "host".into(),
        };
        for kind in [Kind::ListRepos, Kind::ListWorkspaces] {
            let request = command(v1::WorkspaceOp {
                kind: kind.into(),
                ..Default::default()
            });
            assert_eq!(
                executor.execute(&request, false, 1).await.code,
                v1::ResultCode::Rejected as i32
            );
        }
    }
}

#[cfg(test)]
mod recovery_tests {
    use crate::{
        tests::{Cube, command, op, run},
        workspace,
    };
    use boss_engine_recovery::execution_bookmark::{Jj, LocalJj};
    use boss_protocol_spoke::v1::{self, WorkspaceKind as Kind, command_result};

    #[tokio::test]
    async fn executor_runs_store_and_workspace_recovery_operations() {
        let root = tempfile::tempdir().unwrap();
        let repo = boss_engine_test_git::jj::JjRepo::new(root.path());
        let cube = Cube::new(root.path());
        *cube.owner.lock().unwrap() = Some(workspace::lease_owner(&run()));
        let executor = workspace::Executor {
            cube,
            host_id: "host".into(),
        };
        std::fs::write(repo.worker.join("work.txt"), "preserved").unwrap();
        LocalJj.run(&repo.worker, &["status"]).await.unwrap();
        let created = executor.execute(&command(op(Kind::CreateBookmark)), false, 1).await;
        assert_eq!(created.code, v1::ResultCode::Ok as i32, "{}", created.reason);
        let Some(command_result::Value::Workspace(created)) = created.value else {
            panic!("missing bookmark")
        };
        let mut request = op(Kind::CheckpointBaseline);
        request.bookmark = created.bookmark.clone();
        request.workspace_id.clear();
        request.lease_id.clear();
        let result = executor.execute(&command(request), false, 1).await;
        assert_eq!(result.code, v1::ResultCode::Ok as i32, "{}", result.reason);
        let Some(command_result::Value::Workspace(value)) = result.value else {
            panic!("missing checkpoint")
        };
        let base: String = serde_json::from_slice(&value.cube_result_json).unwrap();
        assert_eq!(base, created.bookmark.as_ref().unwrap().anchor_commit);
        assert_eq!(
            std::fs::read_to_string(repo.worker.join("work.txt")).unwrap(),
            "preserved"
        );
    }
}
