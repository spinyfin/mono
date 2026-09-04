//! [`PaneSpawnRunner`]: the libghostty-pane [`ExecutionRunner`], plus the
//! boss-event shim install/resolve helpers it relies on.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;

use crate::config::RuntimeConfig;
use crate::coordinator::slot_id_from_worker_id;
use crate::pane_summary;
use crate::spawn_flow::{StartWorkerInput, TmuxWorkerHost, start_worker};
use crate::work::{WorkDb, WorkExecution, WorkItem};
use boss_protocol::{ExecutionKind, ExecutionStatus, ReviewBatchMemberRole, WorkItemBinding};

use super::prompt::structured_output_env_vars;
use super::work_item::{followup_pr_body_prefix_for_work_item, work_item_id, work_item_name, work_item_task_kind};
use super::worker_spawn::{ComposedWorkerSpawn, WorkerSpawnOpts, compose_worker_spawn};
use super::{ExecutionRunner, RunOutcome, RunWaitState, bound_events_socket_path};

/// Render one driver-supplied [`crate::driver::EnvDirective`] as a shell
/// statement to prepend to the worker pane's spawn command. Generic over
/// every driver: the engine knows how to turn a `Set`/`Unset` directive into
/// shell syntax, but never which vars a given driver names (that knowledge
/// stays in the driver's [`crate::driver::SpawnPlan`]).
pub(crate) fn render_env_directive(directive: &crate::driver::EnvDirective) -> String {
    match directive {
        crate::driver::EnvDirective::Set(key, value) => {
            format!("export {key}={}; ", crate::ssh_transport::shell_quote(value))
        }
        crate::driver::EnvDirective::Unset(key) => format!("unset {key}; "),
    }
}

#[cfg(test)]
mod render_env_directive_tests {
    use super::render_env_directive;
    use crate::driver::EnvDirective;

    #[test]
    fn renders_unset() {
        assert_eq!(
            render_env_directive(&EnvDirective::Unset("ANTHROPIC_API_KEY".to_string())),
            "unset ANTHROPIC_API_KEY; "
        );
    }

    #[test]
    fn renders_set_with_plain_value() {
        assert_eq!(
            render_env_directive(&EnvDirective::Set("CODEX_HOME".to_string(), "/opt/codex".to_string())),
            "export CODEX_HOME='/opt/codex'; "
        );
    }

    #[test]
    fn renders_set_quoting_a_value_with_a_single_quote() {
        assert_eq!(
            render_env_directive(&EnvDirective::Set(
                "CODEX_HOME".to_string(),
                "/Users/a b/it's".to_string()
            )),
            "export CODEX_HOME='/Users/a b/it'\\''s'; "
        );
    }
}

#[cfg(test)]
mod apply_permission_extra_args_tests {
    use crate::driver::{AgentDriver, SpawnRequest};
    use crate::driver::{CodexDriver, WorkerKind, apply_permission_extra_args, codex::codex_sandbox_extra_args};

    #[test]
    fn empty_extra_args_leave_command_unchanged() {
        let cmd = "exec claude --model opus\n";
        assert_eq!(apply_permission_extra_args(cmd, &[]), cmd);
    }

    #[test]
    fn reviewer_sandbox_replaces_workspace_write_default() {
        let plan = CodexDriver::default().spawn_invocation(SpawnRequest {
            model: "gpt-5.6-terra",
            effort: None,
            settings_path: None,
            non_opus_auto_mode: false,
            permission_mode_override: None,
            run_id: Some("exec-review-1"),
        });
        assert!(
            plan.command.contains("workspace-write"),
            "Codex spawn default includes workspace-write: {}",
            plan.command
        );
        let merged = apply_permission_extra_args(&plan.command, &codex_sandbox_extra_args(WorkerKind::Reviewer, false));
        assert!(
            merged.contains("workspace-write"),
            "Reviewer must get --sandbox workspace-write: {merged}"
        );
        // Required contract flags survive the rewrite.
        assert!(merged.contains("--strict-config"), "{merged}");
        assert!(merged.contains("--no-alt-screen"), "{merged}");
        assert!(merged.contains("-a never"), "{merged}");
    }

    #[test]
    fn standard_sandbox_unenforced_replaces_workspace_write_default_with_danger_full_access() {
        let plan = CodexDriver::default().spawn_invocation(SpawnRequest {
            model: "gpt-5.6-terra",
            effort: None,
            settings_path: None,
            non_opus_auto_mode: false,
            permission_mode_override: None,
            run_id: Some("exec-standard-1"),
        });
        assert!(
            plan.command.contains("workspace-write"),
            "Codex spawn default includes workspace-write: {}",
            plan.command
        );
        let merged = apply_permission_extra_args(&plan.command, &codex_sandbox_extra_args(WorkerKind::Standard, false));
        assert!(
            merged.contains("danger-full-access"),
            "Standard with codex_sandbox_enforced off must get --sandbox danger-full-access: {merged}"
        );
        assert!(
            !merged.contains("workspace-write"),
            "default sandbox must be replaced: {merged}"
        );
        // Required contract flags survive the rewrite.
        assert!(merged.contains("--strict-config"), "{merged}");
        assert!(merged.contains("--no-alt-screen"), "{merged}");
        assert!(merged.contains("-a never"), "{merged}");
    }

    /// Grok spells its model flag `--model` (not ` -m `) and has no stdin
    /// redirect, so `apply_permission_extra_args`'s insertion-point search
    /// (before ` -m `, else before ` < `, else at the last `\n`) falls
    /// through to the newline fallback — the ONE branch no existing test of
    /// this function exercised (every prior test above is Codex-only, and
    /// Codex always hits the ` -m ` branch). Confirms the fallback lands the
    /// extras before the trailing newline (still the single typed pty line)
    /// rather than dropping or interleaving them.
    #[test]
    fn grok_extra_args_compose_via_the_newline_fallback_not_the_dash_m_branch() {
        use crate::driver::GrokDriver;
        use crate::driver::grok::{GROK_HOMES_ENV_TEST_LOCK, GROK_HOMES_ROOT_ENV, grok_home_for_run};

        // Stamp a disposable `$GROK_HOME` (session id + workspace-path
        // files) so `spawn_invocation` can build a real command without
        // running the network-touching `provision_workspace` — same shape
        // as `grok::tests::spawn_invocation_matches_execution_shape`,
        // reachable here only through `grok`'s public re-exports.
        let _lock = GROK_HOMES_ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prior_homes_env = std::env::var_os(GROK_HOMES_ROOT_ENV);
        let homes_root = tempfile::TempDir::new().unwrap();
        // SAFETY: serialised by `_lock`, held for this whole test.
        unsafe { std::env::set_var(GROK_HOMES_ROOT_ENV, homes_root.path()) };
        let run_id = "run-grok-extra-args-1";
        let grok_home = grok_home_for_run(run_id).unwrap();
        std::fs::create_dir_all(&grok_home).unwrap();
        std::fs::write(
            grok_home.join("boss-session-id"),
            "22222222-3333-4444-8555-666666666666\n",
        )
        .unwrap();
        std::fs::write(grok_home.join("boss-workspace-path"), "/tmp/ws-extra-args\n").unwrap();

        let plan = GrokDriver::default().spawn_invocation(SpawnRequest {
            model: "grok-4.6",
            effort: Some("high"),
            settings_path: None,
            non_opus_auto_mode: false,
            permission_mode_override: None,
            run_id: Some(run_id),
        });

        assert!(
            !plan.command.contains(" -m "),
            "grok spells its model flag --model, not -m: {}",
            plan.command
        );
        assert!(
            !plan.command.contains(" < "),
            "grok has no stdin redirect: {}",
            plan.command
        );
        assert_eq!(
            plan.command.matches('\n').count(),
            1,
            "must be a single typed line before extras are applied: {}",
            plan.command
        );

        let extra_args = vec![
            "--sandbox".to_owned(),
            "off".to_owned(),
            "--deny".to_owned(),
            "Bash(rm -rf *)".to_owned(),
        ];
        let merged = apply_permission_extra_args(&plan.command, &extra_args);

        // Every token — including flag names — comes back individually
        // shell-quoted (`boss_ssh_transport::shell_quote` applied per
        // element), not just values.
        assert!(merged.contains("'--sandbox' 'off'"), "{merged}");
        assert!(merged.contains("'--deny' 'Bash(rm -rf *)'"), "{merged}");
        assert_eq!(
            merged.matches('\n').count(),
            1,
            "extras must land on the single typed line, not add a second: {merged}"
        );
        assert!(
            merged.contains("\"$(cat .grok/initial-prompt.txt)\""),
            "prompt substitution must survive composition: {merged}"
        );
        // Newline-fallback: extras land right before the trailing '\n' —
        // i.e. AFTER everything else, including the positional prompt
        // substitution — confirmed safe by the work item's own CLI
        // characterisation (flags placed after the positional prompt
        // parse fine for the installed grok CLI).
        assert!(
            merged.trim_end().ends_with("'--deny' 'Bash(rm -rf *)'"),
            "extras must be appended after the rest of the command, not interleaved: {merged}"
        );

        // SAFETY: still serialised by `_lock`.
        match prior_homes_env {
            Some(v) => unsafe { std::env::set_var(GROK_HOMES_ROOT_ENV, v) },
            None => unsafe { std::env::remove_var(GROK_HOMES_ROOT_ENV) },
        }
    }
}

#[cfg(test)]
mod pty_initial_input_tests;

/// `ExecutionRunner` that drives the libghostty pane RPC: writes the
/// per-lease worker config files, asks the macOS app to host a
/// worker pane, and registers the returned shell pid against the
/// run id so events-socket hook deliveries can correlate.
///
/// Returns `WorkerPaneAlive` immediately on a successful spawn — the
/// pane stays alive in the app with its agent working, and the
/// workspace lease is retained until a follow-up flow concludes the
/// run. Real lifecycle (the pane signaling "Stop" → run completes)
/// lands once the events-socket consumer drives state transitions.
pub struct PaneSpawnRunner {
    cfg: Arc<RuntimeConfig>,
    /// Backing store for the pane-titlebar summary cache. Looked up
    /// in `run_execution` to compute a 2–4 word label for the work
    /// item before asking the app to spawn the pane.
    work_db: Arc<WorkDb>,
    /// Feature flags store — checked at spawn time to decide whether
    /// editorial controls are active for this execution.
    feature_flags: Arc<crate::feature_flags::FeatureFlagsStore>,
    /// Set after construction via [`PaneSpawnRunner::set_server_state`].
    /// Stored as `Weak` to avoid the runner ↔ ServerState reference
    /// cycle. Resolved each call.
    server_state: std::sync::OnceLock<Weak<dyn crate::spawn_flow::WorkerSpawner>>,
    /// Test-injection override for the boss-event binary path. When set,
    /// `boss_event_binary()` returns this directly without consulting the
    /// environment — so tests don't depend on host PATH/filesystem layout.
    boss_event_path_override: std::sync::OnceLock<PathBuf>,
}

impl PaneSpawnRunner {
    pub fn new(
        cfg: Arc<RuntimeConfig>,
        work_db: Arc<WorkDb>,
        feature_flags: Arc<crate::feature_flags::FeatureFlagsStore>,
    ) -> Self {
        Self {
            cfg,
            work_db,
            feature_flags,
            server_state: std::sync::OnceLock::new(),
            boss_event_path_override: std::sync::OnceLock::new(),
        }
    }

    pub fn set_server_state(&self, server_state: Weak<dyn crate::spawn_flow::WorkerSpawner>) {
        let _ = self.server_state.set(server_state);
    }

    /// Inject a known absolute boss-event path for tests so they don't
    /// depend on the host filesystem or `BOSS_EVENT_BIN` env var.
    #[cfg(test)]
    pub(crate) fn set_boss_event_path(&self, path: PathBuf) {
        let _ = self.boss_event_path_override.set(path);
    }

    fn events_socket_path(&self) -> PathBuf {
        bound_events_socket_path(&self.cfg)
    }

    fn boss_event_binary(&self) -> PathBuf {
        if let Some(injected) = self.boss_event_path_override.get() {
            return injected.clone();
        }
        let engine_path = std::env::current_exe().unwrap_or_default();
        let workspace = std::env::var_os("BUILD_WORKSPACE_DIRECTORY").map(PathBuf::from);
        let env_override = std::env::var_os("BOSS_EVENT_BIN").map(PathBuf::from);
        let boss_bin_dir = std::env::var_os("BOSS_BIN_DIR").map(PathBuf::from);
        let stable_bin_dir = boss_log_files::default_state_root().map(|root| root.join("bin"));
        resolve_boss_event_binary(
            &engine_path,
            workspace.as_deref(),
            env_override.as_deref(),
            boss_bin_dir.as_deref(),
            stable_bin_dir.as_deref(),
        )
        .unwrap_or_else(|| {
            panic!(
                "boss-event binary not found: none of BOSS_EVENT_BIN, BOSS_BIN_DIR, \
                 the stable bin dir, runfiles, bazel-bin, or the engine-sibling resolved \
                 to an existing file. A bare 'boss-event' in hook commands causes silent \
                 event-emission failures when the worker's sanitized PATH does not include it. \
                 Set BOSS_EVENT_BIN to the absolute boss-event path to fix this."
            )
        })
    }

    fn tmux_worker_host(&self, slot_id: u8, execution_id: &str) -> Result<TmuxWorkerHost> {
        let short_execution_id: String = execution_id
            .strip_prefix("exec_")
            .unwrap_or(execution_id)
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(12)
            .collect();
        if short_execution_id.is_empty() {
            anyhow::bail!("cannot derive tmux session name from empty execution id");
        }
        let session_name = format!("boss-{slot_id}-{short_execution_id}");
        let tmux = boss_tmux::Tmux::resolve(self.cfg.work.resolved_tmux_socket_path())
            .with_context(|| format!("resolving tmux for execution {execution_id}"))?;
        let spawn_store: Arc<dyn crate::spawn_flow::TmuxSpawnStore> = self.work_db.clone();
        Ok(TmuxWorkerHost::new(tmux, spawn_store, session_name))
    }
}

/// Resolve the absolute path of the `boss-event` shim. Thin re-export of
/// [`boss_engine_worker_bin::resolve_boss_event_binary`] so existing
/// `crate::runner::resolve_boss_event_binary` call sites (and the
/// hard-panic contract in [`PaneSpawnRunner::boss_event_binary`]) stay
/// put. Shared with `resolve_boss_cli` via
/// [`boss_engine_worker_bin::resolve_engine_binary`].
pub(crate) fn resolve_boss_event_binary(
    engine_path: &Path,
    workspace_dir: Option<&Path>,
    env_override: Option<&Path>,
    boss_bin_dir: Option<&Path>,
    stable_bin_dir: Option<&Path>,
) -> Option<PathBuf> {
    boss_engine_worker_bin::resolve_boss_event_binary(
        engine_path,
        workspace_dir,
        env_override,
        boss_bin_dir,
        stable_bin_dir,
    )
}

/// One `PATH`-prepend clause for the worker's first shell line, e.g.
/// `[ -n "$BOSS_BIN_DIR" ] && export PATH="$BOSS_BIN_DIR:$PATH"; `.
///
/// The `[ -n … ]` guard makes an unset var a no-op, so the clause is
/// safe to emit unconditionally. Clauses compose left to right: the
/// *last* one emitted ends up first on `PATH`.
pub(crate) fn path_prepend_clause(var: &str) -> String {
    format!("[ -n \"${var}\" ] && export PATH=\"${var}:$PATH\"; ")
}

/// First statement on every local worker pane's assembled command: marks
/// the pane's already-running login shell as Darwin background priority
/// (`PRIO_DARWIN_BG`, via `taskpolicy -b`), which every process it
/// subsequently execs or forks — the driver CLI and every build/test tool
/// call it runs — inherits. Applied with `-p $$` to the current shell
/// rather than by wrapping a new process, since this line is *sourced*
/// (`. .boss/initial-input.sh`) into the pane's existing login shell, not
/// exec'd as a fresh one.
///
/// Workers are batch CPU/IO consumers competing with the coordinator's
/// interactive pane for the same host's scheduler; this makes them yield
/// under contention without touching the workers' own nice value (which
/// would need `setpriority` per spawned tool-call subprocess to have the
/// same reach) and without requiring root. Absolute path, matching this
/// clause's care elsewhere about not depending on a PATH a login shell's
/// own init scripts might still be rebuilding.
///
/// This is best-effort by design: `taskpolicy` failing (missing binary,
/// unexpected sandbox) is swallowed by the trailing `>/dev/null 2>&1;` so a
/// broken environment can never block a worker from starting, but that also
/// means a pane that failed to enter background class looks identical to
/// one that succeeded — there is no engine-side signal or scrollback trace.
/// To check a live pane, run `ps -o nice -p <pane pid>` (background class
/// shows up as nice 5) or `taskpolicy -p <pid>`.
///
/// The policy also outlives the pane it was applied to: every long-lived
/// daemon a tool call forks from this shell (most notably a workspace's
/// `bazel` server, which idles for hours after the pane exits) keeps
/// `PRIO_DARWIN_BG` for the rest of its life, including for later
/// invocations against that same daemon from outside a worker pane (a
/// human, or the coordinator, re-leasing the workspace). If a bazel server
/// (or other daemon) started by a worker seems to be running slower than
/// expected, that is why — clear it with `bazel shutdown` (from within the
/// tainted workspace) or `taskpolicy -B -p <server pid>` (unprivileged for
/// one's own processes).
pub(crate) const WORKER_BACKGROUND_PRIORITY_CLAUSE: &str = "/usr/bin/taskpolicy -b -p $$ >/dev/null 2>&1; ";

/// macOS tty canonical-mode line cap (`MAX_CANON`,
/// `/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/include/sys/syslimits.h:89`).
/// Past this many bytes in one canonical-mode pty input line — content plus
/// its terminating newline — the kernel silently discards the ENTIRE line,
/// not just the overflow, while the tty still echoes the first `MAX_CANON`
/// bytes. The shell never receives a newline, so the typed command is never
/// run and the worker never starts. Confirmed by a local pty experiment: a
/// 1023-byte line + `\n` (1024 bytes total) is delivered whole; a 1024-byte
/// line + `\n` (1025 bytes total) delivers zero bytes.
const MAX_CANON_LINE_BYTES: usize = 1024;

/// Workspace-relative path of the script holding the full assembled pane
/// spawn command. Kept relative — never embedded as an absolute path in the
/// typed line — so the *typed* line's length never grows with the
/// workspace path. The pane's cwd is always the workspace root (see
/// `SpawnWorkerPaneInput::workspace_path` / `GhosttyTerminalView`'s
/// `config.working_directory`), so a relative reference resolves correctly.
const INITIAL_INPUT_SCRIPT_REL_PATH: &str = ".boss/initial-input.sh";

/// Write the full assembled pane-spawn shell script (`PATH` prepends,
/// driver env directives, driver command) to
/// `<workspace_path>/.boss/initial-input.sh` and return the short,
/// fixed-length line to actually type into the pty.
///
/// Exists because pane spawn used to type the WHOLE assembled command
/// directly into the pty. On macOS the tty line discipline in canonical
/// mode silently drops any single input line over [`MAX_CANON_LINE_BYTES`]
/// bytes instead of truncating it, so a long enough driver/permission/
/// workspace-path/prompt combination meant the worker never started, with
/// no error surfaced anywhere. Sourcing a file keeps the typed line's
/// length independent of all of those — mirroring the remote-spawn path,
/// which already ships its initial input as a file for the same class of
/// reason (`ssh_spawn::RemoteSpawnPlan::initial_input_file`).
fn write_initial_input_script(workspace_path: &Path, script: &str) -> Result<String> {
    let script_path = workspace_path.join(INITIAL_INPUT_SCRIPT_REL_PATH);
    if let Some(parent) = script_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {} for the pane's initial-input script", parent.display()))?;
    }
    std::fs::write(&script_path, script)
        .with_context(|| format!("writing pane initial-input script to {}", script_path.display()))?;
    Ok(format!(". {INITIAL_INPUT_SCRIPT_REL_PATH}\n"))
}

/// Fail loudly rather than let a too-long canonical-mode pty line vanish
/// silently. `line` is what will actually be typed into the pane;
/// `driver_name` is named in the error so a `bossctl dispatch diagnose`-style
/// read immediately shows which driver tripped it.
///
/// This is a regression backstop, not the fix itself: after
/// [`write_initial_input_script`], `line` is a small fixed string regardless
/// of driver, permission-rule count, workspace path, or prompt size. But if
/// some future change reintroduces typing the full command (or otherwise
/// grows this line), this is what catches it at the very first spawn
/// attempt — as a loud dispatch failure — instead of a human noticing a
/// pane that silently never starts.
fn check_initial_input_length(line: &str, driver_name: &str) -> Result<()> {
    let len = line.len();
    if len > MAX_CANON_LINE_BYTES {
        return Err(anyhow!(
            "refusing to spawn {driver_name} worker: pane initial_input is {len} bytes, over the \
             macOS tty canonical-mode line cap of {MAX_CANON_LINE_BYTES} bytes (MAX_CANON); past \
             this length the kernel silently drops the ENTIRE typed line rather than just the \
             overflow, so the shell never receives a newline and the worker never starts"
        ));
    }
    Ok(())
}

/// Materialize the per-workspace launcher directory and return it, so the
/// caller can put it on the worker's `PATH`.
///
/// Keyed like [`crate::worker_setup::worker_settings_path`]: under
/// `<settings_dir>/bin/<workspace-name>/`, so concurrent spawns for
/// different workspaces never rewrite each other's `boss` launcher.
/// The write itself is atomic (temp sibling + rename) via
/// [`boss_engine_worker_bin::write_boss_launcher`].
///
/// The directory always holds a `boss` launcher and can additionally hold a
/// `cube` wrapper for a provenanced derived PR. The `boss` launcher `exec`s
/// the already-built CLI belonging to this engine, resolved by absolute path
/// via [`boss_engine_worker_bin::resolve_boss_cli`]. That resolver never
/// searches `PATH`; when it comes up empty the launcher is still written, and
/// running `boss` then fails immediately with a named diagnostic instead of
/// falling through to a repobin shim that spends ~30 seconds on `bazel build`
/// before reporting anything.
///
/// `bossctl` is deliberately absent: this directory is prepended to the
/// worker's `PATH`, and the Boss-tier control surface stays Boss-tier.
///
/// Returns `None` if the directory could not be written at all. That is
/// logged, not fatal — the worker simply keeps today's `PATH` behaviour
/// rather than losing its spawn over a temp-dir failure.
fn ensure_worker_bin_dir(settings_dir: &Path, workspace_path: &Path) -> Option<PathBuf> {
    let engine_path = std::env::current_exe().unwrap_or_default();
    let workspace_dir = std::env::var_os("BUILD_WORKSPACE_DIRECTORY").map(PathBuf::from);
    let env_override = std::env::var_os(boss_engine_worker_bin::BOSS_CLI_BIN_ENV).map(PathBuf::from);
    let boss_bin_dir = std::env::var_os("BOSS_BIN_DIR").map(PathBuf::from);

    let resolved = boss_engine_worker_bin::resolve_boss_cli(
        &engine_path,
        workspace_dir.as_deref(),
        env_override.as_deref(),
        boss_bin_dir.as_deref(),
    );
    if resolved.is_none() {
        tracing::warn!(
            "no already-built `boss` CLI found (checked BOSS_CLI_BIN, BOSS_BIN_DIR, engine \
             runfiles, bazel-bin, and the engine sibling). Workers will get a launcher that \
             fails loudly on `boss` rather than a build-from-source shim. Build it with \
             `bazel build //tools/boss/cli:boss` or set BOSS_CLI_BIN.",
        );
    }

    // Per-workspace subdirectory — same key as worker_settings_path — so
    // two concurrent spawns cannot truncate a shared `bin/boss`.
    let key = workspace_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "worker".to_owned());
    let dir = settings_dir.join(boss_engine_worker_bin::WORKER_BIN_SUBDIR).join(key);
    match boss_engine_worker_bin::write_boss_launcher(&dir, resolved.as_deref()) {
        Ok(launcher) => {
            tracing::debug!(
                launcher = %launcher.display(),
                target = %resolved.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|| "<unresolved>".to_owned()),
                "worker `boss` launcher written",
            );
        }
        Err(err) => {
            tracing::warn!(
                ?err,
                dir = %dir.display(),
                "could not write the worker `boss` launcher; the worker's PATH is unchanged and \
                 a bare `boss` may resolve to a build-from-source shim",
            );
            return None;
        }
    }

    let cube_override = std::env::var_os(boss_engine_worker_bin::CUBE_CLI_BIN_ENV).map(PathBuf::from);
    let cube_resolved = boss_engine_worker_bin::resolve_cube_cli(
        &engine_path,
        workspace_dir.as_deref(),
        cube_override.as_deref(),
        boss_bin_dir.as_deref(),
    );
    match boss_engine_worker_bin::write_cube_launcher(&dir, cube_resolved.as_deref()) {
        Ok(launcher) => {
            tracing::debug!(
                launcher = %launcher.display(),
                target = %cube_resolved
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<unresolved>".to_owned()),
                "worker `cube` launcher written",
            );
        }
        Err(err) => {
            // Unlike the `boss` arm, this directory may already hold a
            // `cube` file from a PREVIOUS worker in this workspace — a
            // derived-PR compose wrapper (see
            // `write_cube_pr_body_compose_launcher`), since `write_boss_launcher`
            // no longer deletes it on this worker's non-derived spawn (that
            // deletion moved here, to `write_cube_launcher`, so a normal
            // overwrite is the common path). A failed write means that
            // stale wrapper — if one exists — was NOT cleared, so a bare
            // `cube` (or a `CUBE_BIN` pointed at this dir) could still
            // silently run the prior worker's compose logic. Fail closed
            // exactly like the `boss` arm: don't hand back a launcher dir
            // whose `cube` entry we can't vouch for.
            tracing::warn!(
                ?err,
                dir = %dir.display(),
                "could not write the worker `cube` launcher; a bare `cube` may resolve to a \
                 build-from-source shim, or to a stale compose wrapper left by a previous \
                 worker in this workspace",
            );
            return None;
        }
    }
    Some(dir)
}

/// Copy the boss-event shim binary to a stable location in the Boss
/// support directory. Called at engine startup so the path baked into
/// new worker settings.json files remains valid after a `bazel clean`.
///
/// `source_shim` is the currently-valid binary (from the runfiles tree
/// or bazel-bin). `stable_bin_dir` is the target directory
/// (`~/Library/Application Support/Boss/bin/`). Returns the stable path
/// on success. If `source_shim` is already inside `stable_bin_dir`,
/// returns `Ok(source_shim)` without copying (no-op for installed mode).
pub(crate) fn install_boss_event_to_stable_bin(source_shim: &Path, stable_bin_dir: &Path) -> io::Result<PathBuf> {
    let stable_path = stable_bin_dir.join("boss-event");
    if stable_path == source_shim {
        return Ok(stable_path);
    }
    std::fs::create_dir_all(stable_bin_dir)?;
    std::fs::copy(source_shim, &stable_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&stable_path)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(&stable_path, perms)?;
    }
    Ok(stable_path)
}

#[async_trait]
impl ExecutionRunner for PaneSpawnRunner {
    fn tmux_hosting_enabled_for(&self, pool: &str) -> bool {
        self.server_state
            .get()
            .and_then(Weak::upgrade)
            .is_some_and(|state| state.tmux_hosting_enabled_for(pool))
    }

    async fn run_execution(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &Path,
        cube_change_id: Option<&str>,
    ) -> Result<RunOutcome> {
        let weak = self
            .server_state
            .get()
            .ok_or_else(|| anyhow!("PaneSpawnRunner not bound to ServerState"))?;
        let spawner = weak
            .upgrade()
            .ok_or_else(|| anyhow!("ServerState dropped before run_execution"))?;

        let lease_id = execution
            .cube_lease_id
            .clone()
            .context("execution missing cube_lease_id; coordinator must lease before spawn")?;

        // The coordinator already claimed a slot via WorkerPool —
        // `worker_id` is `worker-{N}` (main pool), `auto-worker-{N}`
        // (automation pool), or `review-{N}` (review pool); N is the slot
        // the engine owns. Decode it here and thread it into the spawn so
        // the app hosts the pane in this exact slot rather than running its
        // own (now-deleted) firstIndex(where:) heuristic.
        let slot_id = slot_id_from_worker_id(worker_id).ok_or_else(|| {
            anyhow!(
                "PaneSpawnRunner received worker_id {worker_id:?} that does not parse as worker-{{N}}, auto-worker-{{N}}, or review-{{N}}"
            )
        })?;

        // Compose the worker prompt and stash it on disk so the
        // libghostty pane can `claude "$(cat .claude/initial-prompt.txt)"`
        // — Claude Code's positional arg is treated as the first user
        // message, which gets the worker working without us having to
        // wait for a "Claude is ready" signal and then SendToPane.
        // Going through a file (rather than embedding the prompt in
        // the typed command) avoids shell quoting hell on multi-line,
        // backtick-bearing markdown.
        //
        // Prompt composition + effort/model resolution live in the
        // shared `compose_worker_spawn` so the SSH-remote adapter
        // (`SshHostAdapter::spawn_worker`) launches workers with a
        // byte-identical prompt; see that function for the per-execution
        // collaborator lookups (parent project, conflict / CI attempt,
        // crash-recovery branch, automation-triage preamble).
        let editorial_enabled = self.feature_flags.is_enabled("editorial_controls");
        // `worker_proposals` is the master kill switch for every proposal-backed
        // seam; `worker_signal_proposals_seam` is this seam's own flag. Both
        // must be on for the worker prompt to teach the `boss propose` verbs —
        // mirrors the read-path gate in
        // `completion::WorkerCompletionHandler::detect_and_file_worker_signals`
        // so the two halves of the migration move together.
        let worker_signal_proposals_seam_enabled = self.feature_flags.is_enabled("worker_proposals")
            && self.feature_flags.is_enabled("worker_signal_proposals_seam");
        // Mirrors `worker_signal_proposals_seam_enabled` above — see
        // `deferred_scope_directive`'s doc for why both halves of the
        // deferred-scope seam migration must move together.
        let deferred_scope_proposals_seam_enabled = self.feature_flags.is_enabled("worker_proposals")
            && self.feature_flags.is_enabled("deferred_scope_proposals_seam");
        // Mirrors `worker_signal_proposals_seam_enabled` above — see
        // `followups_emission_block`'s doc for why both halves of the
        // follow-ups seam migration must move together.
        let followup_proposals_seam_enabled = self.feature_flags.is_enabled("worker_proposals")
            && self.feature_flags.is_enabled("followup_proposals_seam");
        // Mirrors `worker_signal_proposals_seam_enabled` above — see
        // `WorkerSpawnOpts::automation_outcome_proposals_seam_enabled`'s doc:
        // this same value also gates the triage worker's CLAUDE.md via
        // `StartWorkerInput` below, so the preamble and CLAUDE.md never
        // disagree about which decision-declaration mechanism is live.
        let automation_outcome_proposals_seam_enabled = self.feature_flags.is_enabled("worker_proposals")
            && self.feature_flags.is_enabled("automation_outcome_proposals_seam");
        let review_batch_fanout_enabled = self.feature_flags.is_enabled("review_batch_fanout");
        // Mirrors `worker_signal_proposals_seam_enabled` above. This one is
        // the seam where the two halves moving together matters most: gating
        // the engine's completion path on a declaration the worker was never
        // taught to make would hold every run to the run-done backstop.
        let run_done_proposals_seam_enabled = self.feature_flags.is_enabled("worker_proposals")
            && self.feature_flags.is_enabled("run_done_proposals_seam");
        let ComposedWorkerSpawn {
            prompt_text,
            spawn_config,
            embedded_output_path: _,
        } = compose_worker_spawn(
            &self.work_db,
            worker_id,
            execution,
            work_item,
            workspace_path,
            cube_change_id,
            WorkerSpawnOpts::builder()
                .editorial_enabled(editorial_enabled)
                .max_embed_diff_lines(self.cfg.work.max_review_embed_diff_lines)
                .worker_signal_proposals_seam_enabled(worker_signal_proposals_seam_enabled)
                .deferred_scope_proposals_seam_enabled(deferred_scope_proposals_seam_enabled)
                .followup_proposals_seam_enabled(followup_proposals_seam_enabled)
                .automation_outcome_proposals_seam_enabled(automation_outcome_proposals_seam_enabled)
                .review_batch_fanout_enabled(review_batch_fanout_enabled)
                .run_done_proposals_seam_enabled(run_done_proposals_seam_enabled)
                .build(),
        )
        .await
        // Every other fallible step below already names itself in its error
        // context; this one did not, so a composition failure surfaced to the
        // coordinator as a bare inner error with no indication that the SPAWN
        // is what aborted. That matters now that the coordinator logs this
        // error the instant the abort happens (`spawn aborted: …` /
        // `spawn_failed`): the chain it prints is the only account of which
        // step failed.
        .with_context(|| {
            format!(
                "composing the worker prompt and spawn config for execution {}",
                execution.id
            )
        })?;

        // Resolve the driver once via the registry on the slug
        // `compose_worker_spawn` already validated. Every subsequent trait
        // call on this run (provision, spawn, settings wiring, live-state
        // capabilities) goes through this Arc — never a hardcoded concrete
        // driver type. A second registered driver therefore actually runs.
        let driver = crate::driver::DriverRegistry::default()
            .require(&spawn_config.driver)
            .map_err(|err| {
                anyhow!(
                    "spawn: resolving driver {:?} for execution {}: {err}",
                    spawn_config.driver,
                    execution.id,
                )
            })?;

        // Write the initial prompt (and gitignore + pre-trust) via the driver's
        // WorkspaceProvisioning capability. The driver's config_dir and
        // initial_prompt_filename (e.g. `.claude/initial-prompt.txt`) determine
        // the exact path the spawn_invocation `$(cat ...)` reads from.
        // Any opaque out-of-workspace runtime state the driver returns
        // (future Codex: Boss-owned CODEX_HOME / archive root) is persisted
        // on the execution so every teardown path can hand it back after
        // engine restart / orphan recovery / workspace release without
        // inferring a home from the engine environment.
        let runtime_state = driver
            .provision_workspace(workspace_path, &prompt_text, &execution.id)
            .await
            .with_context(|| {
                format!(
                    "provisioning workspace {} for execution {}",
                    workspace_path.display(),
                    execution.id,
                )
            })?;
        // Persist must succeed when the driver returned state (Codex
        // CODEX_HOME): a silent failure would make every teardown path
        // no-op, leak Boss-owned homes, and skip auth adopt. Fail the
        // spawn so the caller can retry rather than leave an untracked home.
        // Claude returns None (no out-of-workspace state); clearing a stale
        // payload is best-effort and must not block spawn.
        match runtime_state.as_ref() {
            Some(state) => {
                self.work_db
                    .set_driver_runtime_state(&execution.id, Some(state))
                    .with_context(|| {
                        format!(
                            "persisting driver_runtime_state after provision for execution {} \
                             (driver={})",
                            execution.id,
                            driver.descriptor().name,
                        )
                    })?;
            }
            None => {
                if let Err(err) = self.work_db.set_driver_runtime_state(&execution.id, None) {
                    tracing::warn!(
                        execution_id = %execution.id,
                        driver = driver.descriptor().name,
                        error = %format!("{err:#}"),
                        "failed to clear driver_runtime_state after provision (non-fatal; no runtime state)",
                    );
                }
            }
        }

        // Structured-output artifacts (PR URL / review findings / triage
        // decision / followups): create the engine-owned scratch dir and clear
        // every stale file from a prior run of this exact execution id, then
        // hand the worker the absolute paths it may write. The same paths are
        // embedded in the worker prompt (see `compose_worker_spawn`); the
        // completion handler reads + validates them. Best-effort: a prepare
        // failure is non-fatal — the engine still falls back to the driver's
        // transcript-sentinel producer.
        let structured_output_dir = crate::structured_output::default_dir();
        let structured_output_env = match crate::structured_output::prepare_all(&structured_output_dir, &execution.id) {
            Ok(()) => structured_output_env_vars(&structured_output_dir, execution, work_item),
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    dir = %structured_output_dir.display(),
                    ?err,
                    "spawn: could not prepare structured-output dir; worker will rely on \
                     the transcript-scrape fallback",
                );
                Vec::new()
            }
        };

        // The worker's session settings (boss-event hooks, deny rules)
        // live outside the workspace tree; point the agent at them with
        // `--settings` (or the driver-equivalent flag). `write_workspace_files`
        // writes the same path.
        let worker_settings_path = crate::worker_setup::worker_settings_path(workspace_path);
        // Re-prepend BOSS_BIN_DIR to PATH in the worker's first shell line,
        // mirroring the Boss/coordinator pane (see BossPaneModel.swift and
        // the feba26d2 fix). `spawn_flow` already sets PATH with
        // BOSS_BIN_DIR ahead of a sanitized PATH in the pane *surface*
        // env, but the worker pane runs a login shell whose init scripts
        // (.zprofile, .zshrc) rebuild PATH from /etc/paths and the user's
        // dotfiles — which re-prepends `~/bin`, where a `repobin` shim of
        // `cube` / `boss` / `bossctl` typically lives. That shim is
        // independently versioned and has drifted from the bundled CLI
        // (e.g. it lacks `cube pr create`), so a worker that resolves the
        // shim instead of the bundled binary silently breaks. BOSS_BIN_DIR
        // itself survives init (init scripts don't unset custom env vars),
        // so we re-prepend it here: this line runs *after* init completes
        // and *before* the agent launches, so the agent — and every
        // tool-issued `cube`/`boss` subshell it spawns — inherits the
        // bundled-first PATH. The `[ -n "$BOSS_BIN_DIR" ]` guard is a no-op
        // in dev / bazel-run mode where BOSS_BIN_DIR is unset.
        // The answer agent is capability-restricted: force deny-by-default
        // `dontAsk` so its `permissions.allow` allowlist is authoritative and
        // cannot be downgraded to `auto` / `--dangerously-skip-permissions`
        // (which would bypass the settings rules). Every other kind keeps the
        // model-derived permission mode.
        //
        // Derive the worker kind ONCE and use it for BOTH the settings posture
        // (StartWorkerInput.worker_kind below) and the forced CLI mode, so the
        // two switches can never diverge — the exhaustive `WorkerKind` matches
        // force a new restricted kind to decide both.
        let worker_kind = crate::worker_setup::worker_kind_for_execution(&execution.kind);
        let permission_mode_override = worker_kind.forced_permission_mode();
        // Only a review-batch's Supervisor-role member gets the supervisor
        // CLAUDE.md, and only a PostMergeReviewer-role member gets the
        // post-merge one, instead of the leaf reviewer's; every other
        // `pr_review` execution (leaf members, and legacy memberless
        // reviewers) keeps the existing reviewer posture unchanged.
        let review_batch_member_role = if execution.kind == ExecutionKind::PrReview {
            self.work_db
                .review_batch_member_for_execution(&execution.id)
                .map_err(|error| {
                    anyhow::anyhow!(
                        "determining whether execution {} is a review supervisor: {error}",
                        execution.id
                    )
                })?
                .map(|member| member.role)
        } else {
            None
        };
        let is_review_supervisor = review_batch_member_role == Some(ReviewBatchMemberRole::Supervisor);
        let is_post_merge_reviewer = review_batch_member_role == Some(ReviewBatchMemberRole::PostMergeReviewer);
        // Any environment scrubbing/exporting a driver's spawn needs (e.g.
        // Claude unsetting ANTHROPIC_API_KEY so it authenticates via OAuth
        // credentials instead of a stray shell-profile key) is the driver's
        // own concern, carried on the `SpawnPlan.env` built here — the engine
        // renders those directives generically without knowing which driver
        // or which vars they name.
        // Materialise local guard scripts before permission config so Codex
        // can wire absolute paths into CODEX_HOME PreToolUse hooks.
        let settings_dir = crate::worker_setup::worker_settings_dir();
        let path_guard_script = if matches!(worker_kind, crate::worker_setup::WorkerKind::Standard)
            || matches!(worker_kind, crate::worker_setup::WorkerKind::Reviewer)
            || matches!(worker_kind, crate::worker_setup::WorkerKind::Triage)
            || matches!(worker_kind, crate::worker_setup::WorkerKind::AnswerAgent)
        {
            Some(
                crate::worker_setup::ensure_path_guard_script_in(&settings_dir)
                    .with_context(|| format!("materialising path guard script for execution {}", execution.id))?,
            )
        } else {
            None
        };
        let checkleft_guard_script = Some(
            crate::worker_setup::ensure_checkleft_push_guard_script_in(&settings_dir).with_context(|| {
                format!(
                    "materialising checkleft push-guard script for execution {}",
                    execution.id
                )
            })?,
        );

        // Permission artifacts (Codex: hooks + trust attest into CODEX_HOME;
        // Claude: empty — settings still come from worker_setup).
        let permission_input = crate::driver::PermissionInput {
            worker_kind: match worker_kind {
                crate::worker_setup::WorkerKind::Standard => crate::driver::WorkerKind::Standard,
                crate::worker_setup::WorkerKind::Reviewer => crate::driver::WorkerKind::Reviewer,
                crate::worker_setup::WorkerKind::Triage => crate::driver::WorkerKind::Triage,
                crate::worker_setup::WorkerKind::AnswerAgent => crate::driver::WorkerKind::AnswerAgent,
            },
            workspace_path: workspace_path.to_path_buf(),
            events_socket_path: self.events_socket_path(),
            boss_event_path: self.boss_event_binary(),
            run_id: execution.id.clone(),
            lease_id: lease_id.clone(),
            execution_kind: execution.kind.as_str().to_owned(),
            task_kind: work_item_task_kind(work_item).map(str::to_owned),
            is_remote: false,
            path_guard_script: path_guard_script.clone(),
            checkleft_guard_script: checkleft_guard_script.clone(),
            codex_sandbox_enforced: self.feature_flags.is_enabled("codex_sandbox_enforced"),
        };
        let permission_artifacts = driver
            .write_permission_config(&permission_input, &settings_dir)
            .await
            .with_context(|| {
                format!(
                    "writing permission/hook config for execution {} (driver={})",
                    execution.id,
                    driver.descriptor().name
                )
            })?;

        let mut spawn_plan = driver.spawn_invocation(crate::driver::SpawnRequest {
            model: &spawn_config.model,
            effort: spawn_config.effort_value,
            settings_path: Some(&worker_settings_path),
            non_opus_auto_mode: spawner.non_opus_auto_mode(),
            permission_mode_override,
            run_id: Some(execution.id.as_str()),
        });
        // Merge permission-policy env (e.g. Codex CODEX_HOME) without
        // duplicating keys the spawn plan already set.
        for (key, value) in &permission_artifacts.env {
            if !spawn_plan
                .env
                .iter()
                .any(|d| matches!(d, crate::driver::EnvDirective::Set(k, _) if k == key))
            {
                spawn_plan
                    .env
                    .push(crate::driver::EnvDirective::Set(key.clone(), value.clone()));
            }
        }
        // Apply permission-policy CLI args (e.g. Codex's reviewer output-root
        // sandbox). Must run after spawn_invocation so policy replaces any
        // driver default flags rather than being ignored.
        spawn_plan.command =
            crate::driver::apply_permission_extra_args(&spawn_plan.command, &permission_artifacts.extra_args);
        // The per-workspace launcher dir goes on *after* the BOSS_BIN_DIR
        // prepend so it ends up ahead of it. Its `boss` is pinned to an
        // absolute path, which is the only form that survives a login
        // shell rebuilding PATH; BOSS_BIN_DIR's own prepend is a bare
        // directory and is a no-op in dev mode, where the user's `~/bin`
        // repobin shim was winning.
        // Compose the origin header engine-side. The PATH `cube` wrapper
        // joins it with the worker's ordinary body and passes the full text
        // through cube's existing `--body-file` path — cube gains no feature.
        let pr_body_header = followup_pr_body_prefix_for_work_item(work_item, &execution.repo_remote_url)?;
        let worker_bin_dir = ensure_worker_bin_dir(&settings_dir, workspace_path);
        if let Some(body_header) = pr_body_header {
            let dir = worker_bin_dir.as_ref().ok_or_else(|| {
                anyhow!(
                    "refusing to spawn a derived PR worker without installing its required cube body-compose wrapper"
                )
            })?;
            boss_engine_worker_bin::write_cube_pr_body_compose_launcher(dir, &body_header).with_context(|| {
                format!(
                    "installing the required cube body-compose wrapper for execution {}",
                    execution.id
                )
            })?;
        }
        let env_prefix: String = spawn_plan.env.iter().map(render_env_directive).collect();
        let assembled_command = format!(
            "{WORKER_BACKGROUND_PRIORITY_CLAUSE}{}{}{env_prefix}{}",
            path_prepend_clause("BOSS_BIN_DIR"),
            path_prepend_clause(boss_engine_worker_bin::WORKER_BIN_DIR_ENV),
            spawn_plan.command,
        );
        // Write the full assembled command to a workspace-relative script and
        // type only a short, fixed-length line that sources it — never the
        // command itself. See `MAX_CANON_LINE_BYTES` for why: the pty's tty
        // line discipline in canonical mode silently drops an entire typed
        // line once it crosses macOS's MAX_CANON (1024 bytes), rather than
        // truncating it, so a long enough driver/permission/prompt
        // combination previously meant the worker never started at all —
        // with no error, just a pane that looked stuck mid-argument. This
        // mirrors the remote-spawn path, which already ships its initial
        // input as a file for the same class of reason (see
        // `ssh_spawn::RemoteSpawnPlan::initial_input_file`).
        let initial_input = write_initial_input_script(workspace_path, &assembled_command).with_context(|| {
            format!(
                "writing pane initial-input script for execution {} in workspace {}",
                execution.id,
                workspace_path.display(),
            )
        })?;
        // Hard backstop: `initial_input` above is now a small fixed string
        // regardless of driver, permission-rule count, workspace path, or
        // prompt size, so this should never trip. It stays in place so that
        // if some future change reintroduces typing the full command, the
        // very first spawn attempt fails loudly instead of silently vanishing
        // into a pane that never starts.
        check_initial_input_length(&initial_input, driver.descriptor().name)?;

        // Look up (or generate) a 2–4 word pane-titlebar summary for
        // this work item. The full run id is still used for logs and
        // every other identifier — this label is purely visual. We
        // resolve the utility-model provider lazily and let the helper handle
        // every failure mode (no credential, API error, cache miss) so a slow
        // or unreachable provider never blocks the spawn.
        let utility = self.cfg.utility_model();
        // `derived_title_summary` exhaustively matches `ExecutionKind` (its doc
        // comment demands this — see `execution.rs`) to decide whether this kind
        // needs a pure derived phrase or can use the cached/LLM path; see its
        // doc comment for why some kinds can't share `get_or_generate`.
        let title_summary = match pane_summary::derived_title_summary(&execution.kind, work_item_name(work_item)) {
            Some(summary) => summary,
            None => pane_summary::get_or_generate(&self.work_db, utility.as_ref(), work_item).await,
        };

        let work_item_binding = Some(WorkItemBinding {
            work_item_id: work_item_id(work_item).to_owned(),
            work_item_name: work_item_name(work_item).to_owned(),
            execution_id: execution.id.clone(),
        });

        // Attributed pool (not physical slot occupancy): automation that
        // spilled into Lower Decks still reports `"automation"`. Matches
        // `ExecutionCoordinator::attributed_pool_label`.
        let has_source_automation = matches!(
            self.work_db.source_automation_id_for_work_item(&execution.work_item_id),
            Ok(Some(_))
        );
        let pool = crate::live_worker_state::attributed_pool_label(execution.kind.clone(), has_source_automation);
        let tmux_host = spawner
            .tmux_hosting_enabled_for(pool)
            .then(|| self.tmux_worker_host(slot_id, &execution.id))
            .transpose()?;

        let started = start_worker(
            spawner.as_ref(),
            StartWorkerInput {
                run_id: execution.id.clone(),
                lease_id,
                slot_id,
                workspace_path: workspace_path.to_path_buf(),
                events_socket_path: self.events_socket_path(),
                boss_event_path: self.boss_event_binary(),
                initial_input,
                extra_env: {
                    let mut env = structured_output_env;
                    if let Some(dir) = worker_bin_dir.as_ref() {
                        env.push((
                            boss_engine_worker_bin::WORKER_BIN_DIR_ENV.to_owned(),
                            dir.display().to_string(),
                        ));
                    }
                    env
                },
                title_summary,
                task_title: Some(work_item_name(work_item).to_owned()),
                work_item_binding,
                model: spawn_config.model.clone(),
                draft_pr_mode: spawner.draft_pr_mode(),
                execution_kind: execution.kind.as_str().to_owned(),
                pool: Some(pool.to_owned()),
                task_kind: work_item_task_kind(work_item).map(str::to_owned),
                // Per-kind worker posture (reviewer/triage/answer-agent are
                // restricted; everything else is a Standard implementer),
                // derived once above via the shared `worker_kind_for_execution`
                // so the settings posture and the forced CLI permission mode
                // are driven by one value and cannot diverge.
                worker_kind,
                // Same Arc resolved above for provision/spawn — settings
                // wiring and live-state capability flags use it too.
                driver: driver.clone(),
                tmux_host,
                automation_outcome_proposals_seam_enabled,
                is_review_supervisor,
                is_post_merge_reviewer,
            },
            StdDuration::from_secs(30),
        )
        .await
        .with_context(|| format!("spawning worker pane for run {}", execution.id))?;

        tracing::info!(
            worker_id,
            execution_id = %execution.id,
            slot_id = started.slot_id,
            shell_pid = started.shell_pid,
            effort_level = spawn_config
                .effort_level
                .map(|level| level.as_str())
                .unwrap_or("none"),
            effort_value = spawn_config.effort_value.unwrap_or("default"),
            model = %spawn_config.model,
            ack_timed_out = started.ack_timed_out,
            "pane spawned for execution",
        );

        // Provisional spawn: the `SpawnWorkerPane` ack timed out, so the
        // app may or may not have hosted the pane. We deliberately do NOT
        // treat this as a failure (which would release the lease under a
        // possibly-live pane and duplicate-dispatch the work item — a
        // prior incident). The execution stays tracked in `waiting_human`
        // with the slot registered; the spawn-ack sweep confirms liveness
        // (a hook/pid arrives) or reaps on total silence past the grace
        // window. Surface it loudly so the provisional state is visible in
        // the engine log and the run's result summary.
        if started.ack_timed_out {
            tracing::warn!(
                worker_id,
                execution_id = %execution.id,
                slot_id = started.slot_id,
                "spawn ack timed out; worker registered provisionally (shell_pid 0). \
                 Deferring to the spawn-ack sweep to confirm liveness or reap — the \
                 execution stays tracked and the workspace lease is retained.",
            );
        } else if started.shell_pid == 0 {
            // A SUCCESSFUL ack that reports shell_pid 0 (the app hosted the
            // pane but its surface hasn't published the shell pid yet; the
            // real pid arrives shortly via `update_worker_shell_pid`). This
            // is the exact `shell_pid: 0, ack_timed_out: false` state seen in
            // the field, and until the pid lands the slot looks identical to
            // an ack-timeout provisional spawn (activity=Spawning, pid 0) to
            // the sweeps. It was previously silent — only the ack-timeout
            // branch warned — so the window did not appear in the trace.
            // Surface it explicitly so a run that misbehaves during this
            // window is diagnosable. This is instrumentation only: the pid is
            // reconciled by `update_worker_shell_pid`, and the sweeps already
            // protect a hooking/pid-reporting worker.
            tracing::warn!(
                worker_id,
                execution_id = %execution.id,
                slot_id = started.slot_id,
                "pane spawned on a successful ack but with shell_pid 0 — provisional \
                 liveness window: awaiting update_worker_shell_pid from the app before \
                 the pid→run mapping is registered. The execution stays tracked and the \
                 slot is registered; no reap is warranted while it hooks or reports a pid.",
            );
        }

        // Mid-spawn cancel reconciliation. A cancel / force-stop
        // can land while we were awaiting the `SpawnWorkerPane`
        // round-trip: it marks the execution row `cancelled` but, with
        // no pid yet materialized, cannot reap the worker and
        // deliberately leaves the cube lease held (see
        // `WorkerCompletionHandler::force_release`). Now that the spawn
        // has returned — pid registered, slot mapped, live state stamped
        // — reap the just-spawned pane so it cannot outlive its
        // cancellation, and signal the coordinator to release the lease
        // the cancel path left for us. Without this the worker survives
        // unreaped in a workspace the engine believes is free.
        match self.work_db.get_execution(&execution.id) {
            Ok(exec) if exec.status == ExecutionStatus::Cancelled => {
                tracing::warn!(
                    worker_id,
                    execution_id = %execution.id,
                    slot_id = started.slot_id,
                    shell_pid = started.shell_pid,
                    "spawn completed after the execution was cancelled mid-spawn; reaping the worker pane and releasing the deferred lease",
                );
                spawner.reap_worker_pane(&execution.id).await;
                return Ok(RunOutcome {
                    wait_state: RunWaitState::CancelledDuringSpawn,
                    result_summary: Some(format!(
                        "Execution cancelled during spawn; reaped worker pane in slot {} (shell pid {}).",
                        started.slot_id, started.shell_pid,
                    )),
                    attention: None,
                    // The pane is already torn down — don't ask the
                    // coordinator to keep the pool slot claimed for it.
                    slot_id: None,
                    spawn_config: Some(spawn_config),
                });
            }
            Ok(_) => {}
            Err(err) => {
                // A read failure here is non-fatal: fall through to the
                // normal completion path. The worst case is the existing
                // pre-fix behaviour, not a regression.
                tracing::warn!(
                    execution_id = %execution.id,
                    ?err,
                    "post-spawn cancel re-check failed; proceeding with normal completion",
                );
            }
        }

        // The pane is up and its agent is working, so the execution stays in
        // `running` — for every kind, not just `pr_review`. Nobody is waiting
        // for a human at this instant, and writing `waiting_human` here (the
        // pre-mono#2673 behaviour) put every worker's stored status in direct
        // contradiction with its own hook stream for the whole run.
        //
        // `waiting_human` now has exactly one writer — the worker-event
        // dispatcher, on the driver's awaiting-input signal — and is cleared
        // when the worker resumes. See `RunWaitState::WorkerPaneAlive` and
        // `tools/boss/docs/worker-liveness-contract.md`.
        let wait_state = RunWaitState::WorkerPaneAlive;
        let result_summary = if started.ack_timed_out {
            format!(
                "Spawned worker pane in slot {} PROVISIONALLY — the SpawnWorkerPane ack timed out, \
                 so the pane's liveness is unconfirmed (shell pid {}). The slot is registered and \
                 the spawn-ack sweep will confirm it via the first hook event or reap it on total \
                 silence. Hook events from this run will surface on the engine events socket.",
                started.slot_id, started.shell_pid,
            )
        } else {
            format!(
                "Spawned worker pane in slot {} (shell pid {}). Hook events from this run will surface on the engine events socket.",
                started.slot_id, started.shell_pid,
            )
        };
        Ok(RunOutcome {
            wait_state,
            result_summary: Some(result_summary),
            attention: None,
            slot_id: Some(started.slot_id),
            spawn_config: Some(spawn_config),
        })
    }
}

#[cfg(test)]
mod pane_spawn_tests;
