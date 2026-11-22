//! End-to-end worker spawn helper.
//!
//! Combines the pieces that need to fire when the engine starts a
//! pane-hosted worker for a run:
//!
//! 1. Render and write `<workspace>/.claude/CLAUDE.md` (and a
//!    self-excluding `.gitignore`) plus the worker settings file —
//!    which lives *outside* the workspace tree — from the templates in
//!    [`crate::worker_setup`].
//! 2. Create a detached tmux worker and attach an optional app viewer.
//! 3. Register the returned shell pid in the
//!    [`crate::worker_registry::WorkerRegistry`] so subsequent hook
//!    events from the boss-event shim can be correlated back to the
//!    run via the registry's ancestor walk.
//!
//! This module is just the helper; the pane-driven runner that drives
//! it lives in `runner::PaneSpawnRunner`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration as StdDuration;

use anyhow::{Context, anyhow};
use boss_tmux::TMUX_SPAWN_TOKEN_ENV;
use boss_tmux::{DisplayField, NewSession};
use thiserror::Error;
use tokio::time::Duration;

use std::sync::Arc;

use crate::driver::{AgentDriver, Capability, ProgressFidelity, ProgressIngress, ProgressObservationConfig};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::protocol::{AttachWorkerPaneInput, EngineToAppError, EngineToAppRequest, EngineToAppResponse, EnvVar};
use crate::worker_registry::WorkerRegistry;
use crate::worker_setup::{WorkerKind, WorkerSetupInput, write_workspace_files};

/// Sanitized PATH for worker panes. Excludes `~/bin` (where the
/// `bossctl` symlink lives in this user's setup) and any other
/// per-user bin dir, so a worker that tries to invoke `bossctl`
/// directly fails with a PATH miss. Per `v2-design-risks.md` R3.
///
/// Order: Homebrew first (modern Apple-silicon paths), then the
/// system bins. `/usr/local/bin` is included for legacy x86 brew
/// installs but Apple-silicon machines ignore it.
pub(crate) const WORKER_SANITIZED_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";

/// Serialises a test that sets `BOSS_BIN_DIR` in the process environment
/// against a test that asserts on the exact `PATH` a spawn derives from it.
/// Tests in one binary share that environment, so whenever two such tests
/// land in the same shard they interleave without this.
#[cfg(test)]
pub(crate) static BOSS_BIN_DIR_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The shell and PATH seed shared by a worker pane and local capability
/// discovery.
///
/// A pane starts the user's interactive login shell, which may intentionally
/// extend the sanitized seed from shell-profile files. Keeping that setup here
/// means the tmux launcher and local probe use the same shell mode and seed.
/// The caller's ambient `PATH` never participates; the shell itself comes
/// from `$SHELL`, so its profile-supplied entries resolve as they do for a
/// pane.
#[derive(Debug, Clone)]
pub(crate) struct WorkerPaneLaunch {
    login_shell: PathBuf,
}

impl WorkerPaneLaunch {
    /// Build the launch configuration from the user's configured shell. A
    /// relative or absent `$SHELL` is not a runnable pane shell, so retain the
    /// POSIX fallback used by terminal launchers. Profile-selection environment
    /// variables still inherit from the caller, just as they do for a pane.
    pub(crate) fn from_environment() -> Self {
        let login_shell = std::env::var_os("SHELL")
            .filter(|shell| Path::new(shell).is_absolute())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/bin/sh"));
        Self { login_shell }
    }

    /// Construct a configuration for a known login shell. Kept available to
    /// focused tests that model a profile-only command without changing the
    /// process environment shared by parallel tests.
    #[cfg(test)]
    pub(crate) fn with_login_shell(login_shell: PathBuf) -> Self {
        Self { login_shell }
    }

    /// The PATH environment entry sent to the pane before the login shell
    /// reads profile files.
    pub(crate) fn path_env(&self) -> EnvVar {
        EnvVar {
            key: "PATH".into(),
            value: WORKER_SANITIZED_PATH.into(),
        }
    }

    /// Build a tmux command that starts `script` with the same interactive
    /// login shell used by a worker pane.
    pub(crate) fn tmux_command(&self, script: &str) -> String {
        format!(
            "{} -l -i -c {}",
            crate::ssh_transport::shell_quote(&self.login_shell.display().to_string()),
            crate::ssh_transport::shell_quote(script),
        )
    }

    /// Run `script` with exactly the shell mode and PATH seed used by the tmux
    /// worker launcher. Profile files may extend the seed; the caller's
    /// ambient PATH never participates. The probe has no terminal, so silence
    /// all stdio rather than allowing profile output to leak to its caller.
    pub(crate) fn login_shell_command(&self, script: &str) -> Command {
        let mut command = Command::new(&self.login_shell);
        command
            .env("PATH", WORKER_SANITIZED_PATH)
            .args(["-l", "-i", "-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }
}

/// Env keys allowed to flow from the runner's `extra_env` into the
/// worker pane. Anything outside this set is dropped with a warning;
/// the goal is to prevent ambient env (e.g., a stray
/// `BOSS_CONTROL_SOCKET` left over from an interactive run, or
/// arbitrary tokens carried from the user's shell) from reaching
/// workers. Standard env (HOME, USER, SHELL, TERM, LANG, locale)
/// inherits naturally from the engine process and is not in this list
/// because we never set it explicitly here.
const WORKER_EXTRA_ENV_ALLOWLIST: &[&str] = &[
    "BOSS_TASK_ID",
    // Absolute paths of the engine-owned structured-output artifacts the
    // worker writes: its designated payload (review findings / triage decision
    // / followups) and the PR URL. See `crate::structured_output`.
    "BOSS_PR_URL_OUTPUT",
    "BOSS_STRUCTURED_OUTPUT",
    // Engine-owned directory holding this workspace's `boss` launcher
    // (and nothing else — notably not `bossctl`). Prepended to the
    // worker's PATH so a bare `boss` runs the CLI shipped with this
    // engine rather than a build-from-source shim in the user's `~/bin`.
    // See `boss_engine_worker_bin`.
    boss_engine_worker_bin::WORKER_BIN_DIR_ENV,
    "CUBE_LEASE_ID",
    "CUBE_REPO",
];

/// Value forced into `EDITOR`/`VISUAL`/`GIT_EDITOR`/`JJ_EDITOR` for
/// worker panes. `false` exits non-zero immediately, so any tool that
/// falls through to `$EDITOR` aborts loudly instead of silently popping
/// the user's vim/VS Code window. The CLAUDE.md template tells workers
/// to always pass `-m` inline; this is the safety net that turns a
/// forgotten `-m` into a fast, recoverable error.
const WORKER_EDITOR_NOOP: &str = "false";

/// Set `env[key] = value`, overwriting an existing entry rather than
/// pushing a duplicate. A later duplicate wins for the shell env prefix a
/// worker pane actually runs under, but any consumer that does
/// `.find(|e| e.key == ..)` (e.g. the `worker_bin_dir_is_prepended_ahead_of_the_sanitized_path`
/// test, and any future one) sees the FIRST match — so a duplicate key is
/// never safe to leave behind even when shell semantics happen to paper
/// over it.
fn set_env_var(env: &mut Vec<EnvVar>, key: &str, value: String) {
    if let Some(existing) = env.iter_mut().find(|e| e.key == key) {
        existing.value = value;
    } else {
        env.push(EnvVar {
            key: key.to_owned(),
            value,
        });
    }
}

/// Propagate the running app's bundled CLI directory into a worker env.
///
/// When `boss_bin_dir` is non-empty (installed mode: set by the macOS app
/// to `…/Boss.app/Contents/Resources/bin`):
/// - export `BOSS_BIN_DIR`, `BOSS_BIN` (`$BOSS_BIN_DIR/boss`), and `CUBE_BIN`
///   (`$BOSS_BIN_DIR/cube`) — the bundle ships both CLIs side by side, so a
///   worker whose per-workspace launcher dir write later fails (see
///   `start_worker`'s `worker_bin_dir` handling below) still has a
///   bundle-derived `CUBE_BIN` to name, rather than a prompt that names an
///   unset variable.
/// - prepend the directory to `PATH` so bare `boss` hits the
///   version-matched copy ahead of any sanitized-PATH fallback. `bossctl`
///   also ships in this directory, but stays coordinator-only: it is never
///   exported as its own env var, and `BOSS_LAUNCH_GUARD_COMMAND` blocks it
///   by basename regardless of where PATH resolution finds it.
///
/// The per-workspace launchers take priority in both installed and dev mode;
/// their absolute paths are exported for driver tool shells as well.
pub(crate) fn apply_worker_tool_env(env: &mut Vec<EnvVar>, boss_bin_dir: Option<&str>) {
    apply_boss_bin_dir_to_worker_env(env, boss_bin_dir);

    // The per-workspace launcher dir goes ahead of everything, including
    // BOSS_BIN_DIR: it holds a `boss` pinned to an absolute path, so it
    // is the one entry that stays correct in dev mode (no bundle) as
    // well as installed mode. Applied after the BOSS_BIN_DIR prepend
    // above so it lands in front of it, mirroring the ordering of the
    // pane's first shell line in `pane_spawn`.
    let worker_bin_dir = env
        .iter()
        .find(|e| e.key == boss_engine_worker_bin::WORKER_BIN_DIR_ENV)
        .map(|e| e.value.clone());
    if let Some(worker_bin_dir) = worker_bin_dir.filter(|dir| !dir.is_empty()) {
        if let Some(path_entry) = env.iter_mut().find(|e| e.key == "PATH") {
            path_entry.value = format!("{worker_bin_dir}:{}", path_entry.value);
        }
        // Name the binaries, not a PATH entry. A driver shell snapshot can
        // demote the launcher dir (Codex does this today); `"$BOSS_BIN"` /
        // `"$CUBE_BIN"` still exec the launchers. Overwrites the bundle
        // `BOSS_BIN`/`CUBE_BIN` set above (if any) rather than pushing a
        // duplicate key: a duplicate is last-wins for the shell env prefix a
        // worker pane runs under, but any `.find(|e| e.key == ..)` consumer
        // — including this crate's own tests — sees the FIRST match, which
        // would silently be the bundle path instead of the launcher.
        set_env_var(
            env,
            boss_engine_worker_bin::BOSS_BIN_ENV,
            boss_engine_worker_bin::boss_bin_in(Path::new(&worker_bin_dir))
                .display()
                .to_string(),
        );
        set_env_var(
            env,
            boss_engine_worker_bin::CUBE_BIN_ENV,
            boss_engine_worker_bin::cube_bin_in(Path::new(&worker_bin_dir))
                .display()
                .to_string(),
        );
    }
}

/// Export `CHECKLEFT_BIN` naming the per-workspace `checkleft` launcher, for
/// a workspace whose `REPOBIN.toml` declares checkleft. A repository that
/// does not declare it gets no launcher and keeps its PATH-based checkleft,
/// so the variable must stay unset there: the launch guard watches a bare
/// `checkleft` only while `CHECKLEFT_BIN` is set.
fn apply_checkleft_bin_env(env: &mut Vec<EnvVar>, workspace_path: &Path) {
    let worker_bin_dir = env
        .iter()
        .find(|e| e.key == boss_engine_worker_bin::WORKER_BIN_DIR_ENV)
        .map(|e| e.value.clone());
    let Some(worker_bin_dir) = worker_bin_dir.filter(|dir| !dir.is_empty()) else {
        return;
    };
    // Gate on the launcher file actually existing, not merely on
    // REPOBIN.toml declaring checkleft. Exporting CHECKLEFT_BIN for a
    // launcher that was never written would name a file that is not there
    // -- and, because the launch guard derives WATCH_CHECKLEFT from
    // CHECKLEFT_BIN being set, a bare `checkleft` would be blocked at the
    // same time with nothing that works left.
    let checkleft_bin = boss_engine_worker_bin::checkleft_bin_in(Path::new(&worker_bin_dir));
    if boss_engine_worker_bin::environment::workspace_declares_repo_tool(workspace_path, "checkleft")
        && checkleft_bin.is_file()
    {
        set_env_var(
            env,
            boss_engine_worker_bin::CHECKLEFT_BIN_ENV,
            checkleft_bin.display().to_string(),
        );
    }
}

fn apply_boss_bin_dir_to_worker_env(env: &mut Vec<EnvVar>, boss_bin_dir: Option<&str>) {
    let Some(boss_bin_dir) = boss_bin_dir.filter(|d| !d.is_empty()) else {
        return;
    };
    set_env_var(env, "BOSS_BIN_DIR", boss_bin_dir.to_owned());
    set_env_var(env, "BOSS_BIN", format!("{boss_bin_dir}/boss"));
    set_env_var(env, "CUBE_BIN", format!("{boss_bin_dir}/cube"));
    if let Some(path_entry) = env.iter_mut().find(|e| e.key == "PATH") {
        path_entry.value = format!("{boss_bin_dir}:{}", path_entry.value);
    }
}

/// Value forced into `XAI_API_KEY` for every worker pane, same belt-and-
/// suspenders rationale as `WORKER_EDITOR_NOOP` above: a caller-side
/// precheck (`grok::home::provision_grok_home`) already refuses to spawn
/// the driver's own Grok pane when the real `GROK_HOME/auth.json` is
/// missing, but that guard only covers the one path that calls it. Any
/// other Grok invocation inside a worker pane — an ad hoc `grok agent
/// leader ...` a worker runs directly via Bash, a probe script, a future
/// driver path — inherits no such precheck, and the bundled Grok CLI's
/// own default behavior on a missing/expired session token is to
/// proactively call its `authenticate` extension method and escalate
/// through device-code/browser OAuth (confirmed live: `xai_grok_pager`
/// logs `auto-triggering login at startup` and, when the account's
/// device-flow endpoint is unavailable, falls through to loopback OAuth,
/// which shells out to open a real browser window — the incident this
/// guards against).
///
/// Per Grok's own documented auth precedence
/// (`~/.grok/docs/user-guide/02-authentication.md#auth-precedence`), an
/// active session token always wins over `XAI_API_KEY`, so a real,
/// already-provisioned `GROK_HOME/auth.json` is unaffected by this and
/// nothing changes for a correctly-provisioned Grok pane. Only when no
/// session token is available does Grok fall back to `XAI_API_KEY` —
/// and forcing that fallback to an intentionally-invalid value makes the
/// missing-credential case fail loudly and immediately (a 400 from
/// `api.x.ai`: "Incorrect API key provided") instead of escalating to an
/// interactive browser flow. Verified empirically: with no `auth.json`
/// present, the real pane invocation shape returns this exact API error
/// in under a second and never calls the platform's URL-open command,
/// versus opening a browser (or hanging on a device-code prompt) with
/// this var absent.
const WORKER_XAI_API_KEY_NO_INTERACTIVE_AUTH: &str = "xai-boss-worker-no-interactive-auth-fallback";

/// Current environment contract for Boss-owned tmux sessions.
/// [`crate::tmux_adoption`] imports this directly (rather than
/// redeclaring it, unlike the env var names below) because it is a fact
/// about *this* engine build's own contract, not an echo of something a
/// different process generation wrote — importing means a version bump
/// here is automatically enforced there with no second edit to keep in
/// sync. That module rejects adopting a session whose schema is missing,
/// unparseable, or newer than this value.
pub const TMUX_SESSION_SCHEMA: &str = "1";
/// Name of the environment variable [`TMUX_SESSION_SCHEMA`] is carried in.
/// Adoption reads this name from a different process generation's live
/// session, so it remains local to that compatibility boundary.
const TMUX_SESSION_SCHEMA_ENV: &str = "BOSS_SESSION_SCHEMA";
const TMUX_SPAWN_TOKEN_OPTION: &str = "@boss_spawn_token";

/// Durable writes surrounding a tmux session creation. Kept as a narrow
/// seam so the spawn-ordering test can exercise the real tmux command shape
/// without requiring a SQLite fixture; [`crate::work::WorkDb`] is the production store.
pub use boss_execution_runner::host::{TmuxSpawnStore, TmuxWorkerHost};

/// Create one detached tmux session using the durable write ordering required
/// for restart adoption. Once the intent write succeeds, every later failure
/// deliberately leaves that `intended` record in place: it is the durable
/// evidence a future reconciler needs to distinguish a never-created session
/// from a session created just before a crash.
async fn start_tmux_worker(
    host: &TmuxWorkerHost,
    pane_launch: &WorkerPaneLaunch,
    execution_id: &str,
    workspace_path: &Path,
    command: &str,
    env: &[EnvVar],
    driver_name: &str,
) -> Result<i32, StartWorkerError> {
    let spawn_token = crate::engine_control::generate_token();
    let intent_recorded = host
        .spawn_store
        .record_tmux_spawn_intent(
            execution_id,
            &host.tmux().server_identity(),
            host.session_name(),
            &spawn_token,
        )
        .context("recording tmux spawn intent")
        .map_err(StartWorkerError::Tmux)?;
    if !intent_recorded {
        return Err(StartWorkerError::Tmux(anyhow!(
            "no active run accepted tmux spawn intent for execution {execution_id}"
        )));
    }

    let mut environment = env
        .iter()
        .map(|EnvVar { key, value }| (key.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    environment.insert(TMUX_SPAWN_TOKEN_ENV.to_owned(), spawn_token.clone());
    environment.insert(TMUX_SESSION_SCHEMA_ENV.to_owned(), TMUX_SESSION_SCHEMA.to_owned());
    // `BOSS_RUN_ID` is normally already in `env`; insert again here to make
    // the session identity contract explicit and prevent a future env
    // refactor from accidentally dropping it from the atomic `-e` set.
    environment.insert("BOSS_RUN_ID".to_owned(), execution_id.to_owned());
    crate::tmux_session_options::insert_color_environment(&mut environment);

    crate::tmux_session_options::prepare_server(host.tmux())
        .await
        .context("preparing Boss tmux server before creating worker session")
        .map_err(StartWorkerError::Tmux)?;
    host.tmux()
        .new_session(&NewSession {
            name: host.session_name().to_owned(),
            environment,
            working_directory: workspace_path.to_path_buf(),
            command: pane_launch.tmux_command(command),
        })
        .await
        .context("creating detached tmux session")
        .map_err(StartWorkerError::Tmux)?;
    crate::tmux_session_options::apply(host.tmux(), host.session_name())
        .await
        .context("applying Boss worker tmux session options")
        .map_err(StartWorkerError::Tmux)?;
    // Driver-specific tmux session config (e.g. codex's `mouse on` for
    // wheel scrollback — see `crate::driver::tmux_session_config_for`).
    // Sourced with `-t` so it is scoped to this one session, never the
    // whole server; see `boss_tmux::Tmux::source_file`. `-t` on
    // `source-file` needs tmux 3.4+ (Boss's floor is 3.2), so probe before
    // sending it — an older server rejects the flag outright, which would
    // otherwise fail the whole spawn over a cosmetic scrollback fix.
    if let Some(config) = crate::driver::tmux_session_config_for(driver_name) {
        match boss_tmux::session_options::source_driver_session_config(host.tmux(), host.session_name(), config)
            .await
            .with_context(|| format!("sourcing {driver_name} tmux session config"))
            .map_err(StartWorkerError::Tmux)?
        {
            boss_tmux::session_options::SessionConfigOutcome::Sourced => {}
            boss_tmux::session_options::SessionConfigOutcome::UnsupportedTmux(version) => {
                tracing::warn!(
                    execution_id = %execution_id,
                    driver_name = %driver_name,
                    tmux_major = version.major,
                    tmux_minor = version.minor,
                    "tmux predates 3.4's `source-file -t`; skipping session-scoped tmux config for this driver",
                );
            }
            boss_tmux::session_options::SessionConfigOutcome::VersionUnreadable(error) => {
                tracing::warn!(
                    execution_id = %execution_id,
                    driver_name = %driver_name,
                    %error,
                    "could not probe tmux version; skipping session-scoped tmux config for this driver",
                );
            }
        }
    }
    host.tmux()
        .set_option(host.session_name(), TMUX_SPAWN_TOKEN_OPTION, &spawn_token)
        .await
        .context("mirroring tmux spawn token in session option")
        .map_err(StartWorkerError::Tmux)?;
    let pane_pid = host
        .tmux()
        .display_message(host.session_name(), DisplayField::PanePid)
        .await
        .context("reading tmux pane pid")
        .map_err(StartWorkerError::Tmux)?
        .parse::<i32>()
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| StartWorkerError::Tmux(anyhow!("tmux returned an invalid pane pid for {execution_id}")))?;
    let created_recorded = host
        .spawn_store
        .record_tmux_session_created(
            execution_id,
            &spawn_token,
            i64::from(pane_pid),
            host.session_name(),
            &host.tmux().server_identity(),
        )
        .context("recording tmux session creation")
        .map_err(StartWorkerError::Tmux)?;
    if !created_recorded {
        return Err(StartWorkerError::Tmux(anyhow!(
            "tmux session was created but no intent row accepted its confirmation for execution {execution_id}"
        )));
    }
    Ok(pane_pid)
}

pub use boss_execution_runner::host::{StartWorkerInput, StartWorkerTimings, StartedWorker};

#[derive(Debug, Error)]
pub enum StartWorkerError {
    #[error("worker viewer attach rejected: {0}")]
    ViewerRejected(#[source] EngineToAppError),
    #[error("worker viewer attach rejected: {error}; process teardown unconfirmed: {reason}")]
    ViewerAbortFailed {
        #[source]
        error: EngineToAppError,
        reason: String,
    },
    #[error("writing worker config: {0}")]
    WriteFiles(std::io::Error),
    #[error("preparing progress ingress: {0}")]
    ProgressIngress(String),
    #[error("tmux-hosted worker spawn: {0:#}")]
    Tmux(#[source] anyhow::Error),
    #[error("local dispatch requires Rich progress fidelity so wedge recovery can judge cadence; driver reports {0:?}")]
    ProgressFidelity(ProgressFidelity),
}

/// What re-establishing a re-adopted run's progress ingress did, for the
/// dispatch trace. Produced by [`WorkerSpawner::reestablish_progress_ingress`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngressReadoption {
    /// The run's driver observes progress some other way (hook callbacks).
    NotFileIngress,
    /// The tail is back, reading from the recorded resume point.
    Reestablished,
    /// The run recorded no checkpoint, so whether it needed one is unknown.
    Unknown,
    /// It needed one and could not be re-established; an attention item says so.
    Failed,
    /// This process already holds the run's slot, so its tail (if any) is
    /// already in place and nothing was re-established.
    AlreadyObserved,
}

impl IngressReadoption {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFileIngress => "not_file_ingress",
            Self::Reestablished => "reestablished",
            Self::Unknown => "unknown",
            Self::Failed => "failed",
            Self::AlreadyObserved => "already_observed",
        }
    }
}

impl StartWorkerError {
    /// Stable, machine-readable name for *which spawn step* refused.
    ///
    /// Carried on the `spawn_failed` dispatch event so a diagnose
    /// signature can key on the failing step rather than substring-matching
    /// a prose error, and so two different causes (an unwritable settings
    /// dir vs an ingress precondition) never collapse into one generic
    /// "spawn failed" bucket. The strings are wire values — treat them the
    /// same as `Stage::as_str()` and do not rename them casually.
    pub fn class(&self) -> &'static str {
        match self {
            Self::WriteFiles(_) => "write_files",
            Self::ViewerRejected(_) => "viewer_rejected",
            Self::ViewerAbortFailed { .. } => "viewer_abort_failed",
            Self::ProgressFidelity(_) => "progress_fidelity",
            Self::ProgressIngress(_) => "progress_ingress",
            Self::Tmux(_) => "tmux_host",
        }
    }

    /// The specific thing this step rejected, with the generic wrapper
    /// stripped.
    ///
    /// For [`Self::ProgressIngress`] that is the precondition text the
    /// ingress preparer returned verbatim (e.g.
    /// `/…/sessions is not a real directory`) — the actionable half of the
    /// message, and the part a recovery hint has to be able to name. Every
    /// other variant has no inner detail worth separating from its own
    /// `Display`, so it returns that.
    pub fn cause_detail(&self) -> String {
        match self {
            Self::ProgressIngress(detail) => detail.clone(),
            other => other.to_string(),
        }
    }
}

/// Public API for callers that want to wire pane-spawning into the
/// coordinator (or a test). The trait is implemented by
/// `boss_engine::app::ServerState`; users should typically call through
/// `ServerState` directly, but the trait makes the dependency
/// explicit and lets stub implementations stand in for unit tests.
#[async_trait::async_trait]
pub trait WorkerSpawner: Send + Sync {
    async fn send_to_app_request(
        &self,
        request: EngineToAppRequest,
        timeout: Duration,
    ) -> Result<EngineToAppResponse, boss_execution_contracts::SendToAppError>;

    /// Abort our newly started process without detaching another run's viewer.
    async fn abort_worker_spawn(&self, _run_id: &str) -> Result<(), String> {
        Ok(())
    }

    fn worker_registry(&self) -> &WorkerRegistry;

    /// The work database, used to capture a progression pane's neighbourhood
    /// when its pid is registered ([`WorkerRegistry::register_for_run`]).
    /// `None` in tests that never spawn a progression run; a progression
    /// spawn without it is registered confined with no scope.
    fn work_db(&self) -> Option<&crate::work::WorkDb> {
        None
    }

    /// The GitHub-backed design-docs service, which also reads a repo's
    /// `BOSS.md` for coding workers' prompts. `None` in tests that never
    /// compose a worker prompt; those prompts simply carry no such block.
    fn design_docs(&self) -> Option<Arc<boss_engine_design_docs::DesignDocsService>> {
        None
    }

    /// Engine's live per-slot state registry. Implementations return
    /// `None` from in-process tests that don't care about the live
    /// state surface; the spawn flow then skips the registration
    /// step. Production `ServerState` always returns `Some`.
    fn live_worker_state_registry(&self) -> Option<&LiveWorkerStateRegistry> {
        None
    }

    /// Hook called after `LiveWorkerStateRegistry` is updated so the
    /// caller can broadcast the snapshot on the worker live-state
    /// topic. Default no-op for tests.
    async fn publish_live_worker_states(&self) {}

    /// Hook called when the spawn-time `AttachWorkerPane` did not
    /// produce a viewer even though the worker itself started (the app
    /// answered with an error other than `SlotBusy`, or the request
    /// failed outright). The worker keeps running headless; production
    /// `ServerState` schedules its bounded viewer re-attach so the pane
    /// is retried without waiting for an app relaunch. Default no-op
    /// for tests.
    async fn note_worker_viewer_attach_failed(&self, _run_id: &str) {}

    /// Hook called after a slot has been registered so the engine can
    /// spawn the per-slot live-status summarizer task. Default no-op
    /// for tests; production `ServerState` starts the task via its
    /// `LiveStatusManager`. The task tears itself down when
    /// `release_worker_pane` runs.
    fn start_live_status_slot(&self, _slot_id: u8, _run_id: &str, _driver: Arc<dyn AgentDriver>) {}

    /// Prepare a run-correlated progress source before pane spawn. File
    /// ingress implementations snapshot pre-existing candidates here but do
    /// not dispatch until [`Self::activate_progress_ingress`].
    fn prepare_progress_ingress(
        &self,
        _run_id: &str,
        _driver: Arc<dyn AgentDriver>,
        _ingress: ProgressIngress,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Activate the prepared source after live-state registration.
    fn activate_progress_ingress(&self, _run_id: &str) {}

    /// Cancel a prepared or active source after spawn failure/teardown.
    fn stop_progress_ingress(&self, _run_id: &str) {}

    /// Re-establish a *re-adopted* run's progress ingress from the durable
    /// record of where it got to, after an engine restart rebuilt the run's
    /// slot. A hook-callback driver needs nothing here — its shim reconnects
    /// to the events socket on its own — but a file-tailing driver (Codex)
    /// is observed only through the tail this engine process holds, and a
    /// rebuilt slot with no tail stays frozen at whatever activity it was
    /// seeded with and never sees the turn boundary that completes the run.
    /// Production `ServerState` resumes the tail at the recorded byte or
    /// files an attention item; the default is for tests.
    async fn reestablish_progress_ingress(
        &self,
        _execution: &crate::work::WorkExecution,
        _driver: Option<Arc<dyn AgentDriver>>,
    ) -> IngressReadoption {
        IngressReadoption::Unknown
    }

    /// Whether the `default_pr_draft_mode` setting is enabled. When
    /// `true`, the worker's CLAUDE.md gets a directive to pass
    /// `--draft` to `gh pr create`. Default `false` for tests.
    fn draft_pr_mode(&self) -> bool {
        false
    }

    /// Whether Sonnet/Haiku workers should use `--permission-mode auto`
    /// instead of `--dangerously-skip-permissions`. Controlled by the
    /// `workers.non_opus_permission_mode` setting. Default `false` (skip
    /// permissions) for tests; corp users set it to `true`.
    fn non_opus_auto_mode(&self) -> bool {
        false
    }

    /// Verify and tear down the tmux session and its process tree for
    /// `run_id` (the execution id). Used by the spawn flow to reap a
    /// worker that was cancelled *during* its spawn window: at cancel
    /// time the pid had not yet materialized, so the cancel path could
    /// not reap it and deliberately left the cube lease held. Once the
    /// spawn returns and the pid is registered, the runner calls this to
    /// kill the just-spawned worker before the coordinator releases the
    /// deferred lease. Default no-op for test spawners that don't host
    /// real panes; production `ServerState` delegates to
    /// `boss_engine::app::ServerState::release_worker_pane`.
    async fn reap_worker_pane(&self, _run_id: &str) {}
}

/// Render the worker-config files, start a detached tmux worker,
/// register the resulting shell pid for hook-event correlation, and
/// return the slot id + pid for the caller to record.
pub async fn start_worker<S: WorkerSpawner + ?Sized>(
    spawner: &S,
    input: StartWorkerInput,
    spawn_timeout: StdDuration,
) -> Result<StartedWorker, StartWorkerError> {
    // Buffer hooks that race the live-state registration below (the CLI is
    // running, and its hooks can land, before the pane-attach await returns).
    // Re-arming here also drops evidence left over from an earlier attempt.
    // The guard disarms on every exit that did not consume the entry —
    // errors and a dropped (cancelled / timed-out) future alike. Disarming
    // after a successful registration is a no-op: registration drained it.
    let _pending_hooks = spawner.live_worker_state_registry().map(|live_states| {
        live_states.arm_pending_hooks(&input.run_id);
        PendingHooksGuard {
            live_states,
            run_id: input.run_id.clone(),
        }
    });
    start_worker_armed(spawner, input, spawn_timeout).await
}

/// Disarms a run's pre-registration hook buffer on drop.
struct PendingHooksGuard<'a> {
    live_states: &'a crate::live_worker_state::LiveWorkerStateRegistry,
    run_id: String,
}

impl Drop for PendingHooksGuard<'_> {
    fn drop(&mut self) {
        self.live_states.disarm_pending_hooks(&self.run_id);
    }
}

async fn start_worker_armed<S: WorkerSpawner + ?Sized>(
    spawner: &S,
    input: StartWorkerInput,
    spawn_timeout: StdDuration,
) -> Result<StartedWorker, StartWorkerError> {
    // Local dispatch is only recoverable when the driver supplies Rich
    // per-tool progress boundaries. A Coarse/Minimal driver would silently
    // lose automatic wedge recovery, so refuse before writing files or
    // creating a pane.
    let fidelity = input.driver.progress_fidelity();
    if fidelity != ProgressFidelity::Rich {
        return Err(StartWorkerError::ProgressFidelity(fidelity));
    }

    let progress_ingress = input.driver.progress_observation_wiring(&ProgressObservationConfig {
        events_socket_path: input.events_socket_path.clone(),
        lease_id: input.lease_id.clone(),
        run_id: input.run_id.clone(),
        workspace_path: input.workspace_path.clone(),
        forwarder_binary: input.boss_event_path.clone(),
    });

    // 1. Write CLAUDE.md + .gitignore into the workspace and the worker
    //    settings file outside it (see worker_setup module docs).
    // Deliberately use the builder: forwarding a new setting here is one
    // explicit edit, instead of requiring every WorkerSetupInput caller to change.
    let setup = WorkerSetupInput::builder()
        .run_id(input.run_id.clone())
        .lease_id(input.lease_id.clone())
        .workspace_path(input.workspace_path.clone())
        .events_socket_path(input.events_socket_path.clone())
        .boss_event_path(input.boss_event_path.clone())
        .draft_pr_mode(input.draft_pr_mode)
        .execution_kind(input.execution_kind.clone())
        .maybe_task_kind(input.task_kind.clone())
        .worker_kind(input.worker_kind.clone())
        .automation_outcome_proposals_seam_enabled(input.automation_outcome_proposals_seam_enabled)
        .run_done_proposals_seam_enabled(input.run_done_proposals_seam_enabled)
        .worker_signal_proposals_seam_enabled(input.worker_signal_proposals_seam_enabled)
        .is_review_supervisor(input.is_review_supervisor)
        .is_post_merge_reviewer(input.is_post_merge_reviewer)
        .checkout_positioned_on_pr_head(input.checkout_positioned_on_pr_head)
        .build();
    let workspace_files_began = std::time::Instant::now();
    let written = write_workspace_files(&setup, input.driver.as_ref()).map_err(StartWorkerError::WriteFiles)?;
    let workspace_files = workspace_files_began.elapsed();
    spawner
        .prepare_progress_ingress(&input.run_id, input.driver.clone(), progress_ingress)
        .map_err(StartWorkerError::ProgressIngress)?;

    // 2. Build the tmux worker environment. Workers get a strict env
    //    allowlist (per `v2-design-risks.md` R3): a sanitized PATH
    //    (no `bossctl`), the engine-injected `BOSS_EVENTS_SOCKET`,
    //    `BOSS_SOCKET_PATH` (the bound frontend socket, so `boss`
    //    CLI verbs survive a driver that scopes `$HOME`),
    //    `BOSS_ENGINE_CONTROL_TOKEN_PATH` (the bound token, so
    //    `boss engine stop` does not derive a sibling of the socket),
    //    `BOSS_LEASE_ID` for leased workers, and caller-provided `extra_env` keys
    //    that survive the allowlist filter. Anything else is dropped.
    //
    //    Editor env vars are forced to `false` so a worker that runs
    //    `git commit` / `jj describe` without `-m` doesn't pop the
    //    user's vim/VS Code window — the command exits non-zero and
    //    the worker corrects course by passing `-m` inline. The
    //    matching CLAUDE.md guidance tells the worker the rule; this
    //    is the belt that catches it when the suspenders slip.
    let pane_launch = WorkerPaneLaunch::from_environment();
    let mut env = vec![
        pane_launch.path_env(),
        EnvVar {
            key: "BOSS_EVENTS_SOCKET".into(),
            value: input.events_socket_path.display().to_string(),
        },
    ];
    // Bound frontend socket, not `$HOME/Library/.../engine.sock`. Grok
    // scopes worker `HOME` to a per-run process-home; without this export
    // the `boss` CLI looks for a socket the engine has never created there,
    // and `--socket-path` at the production data dir is blocked by the
    // path-guard hook. Claude and Codex currently keep the host HOME, but
    // the same export makes every driver independent of that.
    //
    // Setting `BOSS_SOCKET_PATH` flips CLI discovery into its
    // sibling-derivation branch, which would look for
    // `<stem>.control-token`. Production writes `engine-control.token`, so
    // the resolved token path must ride along as
    // `BOSS_ENGINE_CONTROL_TOKEN_PATH` (checked first by `Discovery::from_env`).
    if let Some(frontend_socket_path) = &input.frontend_socket_path {
        env.push(EnvVar {
            key: crate::config::FRONTEND_SOCKET_ENV.into(),
            value: frontend_socket_path.display().to_string(),
        });
        if let Some(control_token_path) = &input.control_token_path {
            env.push(EnvVar {
                key: crate::engine_control::TOKEN_PATH_ENV.into(),
                value: control_token_path.display().to_string(),
            });
        } else {
            tracing::warn!(
                run_id = %input.run_id,
                "spawn_flow: no bound control-token path was stamped on the config; \
                 worker `boss engine stop`/`restart` will derive <socket-stem>.control-token \
                 from BOSS_SOCKET_PATH, which production does not write",
            );
        }
    } else {
        tracing::warn!(
            run_id = %input.run_id,
            "spawn_flow: no bound frontend socket was stamped on the config; \
             worker `boss` verbs will fall back to HOME-relative discovery",
        );
    }
    // Progression runs use a scratch directory without a cube lease. Omit
    // only their lease export; an empty value for a leased worker must still
    // reach tmux validation and fail as an invalid spawn context.
    if input.worker_kind != WorkerKind::Progression {
        env.push(EnvVar {
            key: "BOSS_LEASE_ID".into(),
            value: input.lease_id.clone(),
        });
    }
    env.extend([
        EnvVar {
            // Read by `boss-event` and embedded in every hook payload
            // as `_boss_run_id`. The engine uses this to correlate
            // hook events to runs even before the tmux pane pid has
            // been registered. Startup hooks must not depend on a
            // shell-pid lookup that can race process creation.
            key: "BOSS_RUN_ID".into(),
            value: input.run_id.clone(),
        },
        EnvVar {
            key: "EDITOR".into(),
            value: WORKER_EDITOR_NOOP.into(),
        },
        EnvVar {
            key: "VISUAL".into(),
            value: WORKER_EDITOR_NOOP.into(),
        },
        EnvVar {
            key: "GIT_EDITOR".into(),
            value: WORKER_EDITOR_NOOP.into(),
        },
        EnvVar {
            key: "JJ_EDITOR".into(),
            value: WORKER_EDITOR_NOOP.into(),
        },
        EnvVar {
            key: "XAI_API_KEY".into(),
            value: WORKER_XAI_API_KEY_NO_INTERACTIVE_AUTH.into(),
        },
    ]);
    for (k, v) in input.extra_env {
        if WORKER_EXTRA_ENV_ALLOWLIST.contains(&k.as_str()) {
            env.push(EnvVar { key: k, value: v });
        } else {
            tracing::warn!(
                key = %k,
                "spawn_flow: dropping non-allowlisted env key from worker spawn",
            );
        }
    }

    // Installed mode: propagate BOSS_BIN_DIR (set by the app when
    // launching the engine from Boss.app/Contents/Resources/bin/).
    // Workers prepend this directory to PATH so bare `boss` resolves the
    // bundled copy, and export BOSS_BIN (absolute path to the bundled
    // `boss`) for the same reason the coordinator session does —
    // absolute-path invocation must be legal and version-matched.
    // `bossctl` lives in the same directory but stays coordinator-only:
    // this prepend does not export a BOSS_BIN-shaped var for it, and
    // BOSS_LAUNCH_GUARD_COMMAND blocks any `bossctl` invocation by
    // basename regardless of path.
    // Unset in dev mode (no bundle bin/ directory); the per-workspace
    // `boss` launcher (BOSS_WORKER_BIN_DIR) still pins bare `boss` to the
    // engine's own CLI without searching PATH.
    apply_worker_tool_env(&mut env, std::env::var("BOSS_BIN_DIR").ok().as_deref());
    apply_checkleft_bin_env(&mut env, &input.workspace_path);

    let slot_id = input.slot_id;
    let tmux_host = &input.tmux_host;
    let tmux_socket_path = tmux_host
        .socket_path()
        .ok_or_else(|| {
            StartWorkerError::Tmux(anyhow!(
                "tmux host for {} has no socket path; cannot attach a viewer",
                input.run_id
            ))
        })?
        .display()
        .to_string();
    let tmux_start_began = std::time::Instant::now();
    let shell_pid = match start_tmux_worker(
        tmux_host,
        &pane_launch,
        &input.run_id,
        &input.workspace_path,
        &input.initial_input,
        &env,
        input.driver.descriptor().name,
    )
    .await
    {
        Ok(shell_pid) => shell_pid,
        Err(err) => {
            spawner.stop_progress_ingress(&input.run_id);
            return Err(err);
        }
    };
    let tmux_start = tmux_start_began.elapsed();
    // The detached tmux session owns the worker, so a missing app session
    // only means it runs without a viewer until the app reconnects. A
    // SlotBusy rejection is different: reconciliation has already tried to
    // clear stale occupancy, so the failure is propagated and the spawned
    // worker is reaped. Other failures are stamped onto the live state so
    // the card says "Couldn't attach the terminal: <reason>" instead of
    // reading as still connecting. A successful attach clears the error.
    let viewer_attach_began = std::time::Instant::now();
    let attach_response = spawner
        .send_to_app_request(
            EngineToAppRequest::AttachWorkerPane(AttachWorkerPaneInput {
                run_id: input.run_id.clone(),
                slot_id,
                session_name: tmux_host.session_name().to_owned(),
                tmux_socket_path,
                summary: input.title_summary.clone(),
                task_title: input.task_title.clone(),
            }),
            spawn_timeout,
        )
        .await;
    let viewer_attach = viewer_attach_began.elapsed();
    let viewer_attach_error: Option<String> = match attach_response {
        Ok(EngineToAppResponse::AttachWorkerPane { result: Ok(_) }) => {
            tracing::info!(
                run_id = %input.run_id,
                slot_id = slot_id,
                session_name = tmux_host.session_name(),
                "attached tmux-hosted worker pane",
            );
            None
        }
        Ok(EngineToAppResponse::AttachWorkerPane {
            result: Err(err @ EngineToAppError::SlotBusy { .. }),
        }) => {
            tracing::error!(run_id = %input.run_id, slot_id, %err, "worker viewer attach rejected; aborting spawn");
            let teardown = spawner.abort_worker_spawn(&input.run_id).await;
            spawner.stop_progress_ingress(&input.run_id);
            if let Err(reason) = teardown {
                return Err(StartWorkerError::ViewerAbortFailed { error: err, reason });
            }
            return Err(StartWorkerError::ViewerRejected(err));
        }
        Ok(response) => {
            tracing::warn!(
                run_id = %input.run_id,
                slot_id = slot_id,
                ?response,
                "tmux worker started but app did not attach its viewer surface",
            );
            Some(viewer_attach_failure_reason(&response))
        }
        Err(err) => {
            tracing::debug!(
                run_id = %input.run_id,
                slot_id = slot_id,
                ?err,
                "tmux worker started without an app viewer surface",
            );
            // `NotRegistered` is the one expected case: the app is not
            // running, so there is nobody to show a card. Registration
            // re-attaches every live worker and no failure is recorded.
            match err {
                boss_execution_contracts::SendToAppError::NotRegistered => None,
                other => Some(format!("request to the app failed: {other}")),
            }
        }
    };

    spawner
        .worker_registry()
        .register_tmux_run_slot(input.run_id.clone(), slot_id, tmux_host.session_name());
    spawner.worker_registry().register_for_run(
        shell_pid,
        &input.run_id,
        input.worker_kind == crate::worker_setup::WorkerKind::Progression,
        spawner.work_db(),
    );

    // 4. Stamp the initial LiveWorkerState so bossctl/UI immediately
    //    see "Spawning" with the launch-default model — no more
    //    "Claude Unknown" while we wait for SessionStart to fire.
    if let Some(live_states) = spawner.live_worker_state_registry() {
        // Ask the resolved driver, rather than assume: this derives the
        // capability from the actual driver's declared capabilities and
        // passes it straight into registration, so there is no window
        // between spawn and a follow-up setter call where a concurrently
        // delivered hook event would be evaluated against a stale default.
        live_states.register_spawn_with_capabilities(
            slot_id,
            input.run_id.clone(),
            input.model,
            shell_pid,
            input.work_item_binding,
            input.driver.capabilities().provides(Capability::AwaitingInputSignal),
            // Always stamp the execution kind; production callers pass a
            // non-empty snake_case kind, tests that leave the field at a
            // placeholder still surface it on the wire. Pool may be
            // `None` for tests that never set `StartWorkerInput.pool`.
            crate::live_worker_state::LiveSpawnRouting::new_with_hosting(input.pool, input.execution_kind, true)
                .with_metadata(input.live_metadata),
        );
        // Declare this slot's driver-reported progress fidelity so
        // `stale_worker_sweep` judges cadence-based staleness against the
        // driver that is actually running, not an assumed Claude rhythm
        // (see `ProgressFidelity::stale_threshold_secs`).
        live_states.set_progress_fidelity(slot_id, input.driver.progress_fidelity());
        if let Some(reason) = viewer_attach_error.clone() {
            live_states.set_viewer_attach_error(&input.run_id, Some(reason));
        }
        spawner.publish_live_worker_states().await;
        // 5. Spin up the live-status summarizer for this slot. The
        //    manager owns the task lifecycle and will be torn down
        //    on `release_worker_pane`.
        spawner.start_live_status_slot(slot_id, &input.run_id, input.driver.clone());
    }
    spawner.activate_progress_ingress(&input.run_id);
    if viewer_attach_error.is_some() {
        spawner.note_worker_viewer_attach_failed(&input.run_id).await;
    }

    Ok(StartedWorker {
        slot_id,
        shell_pid,
        written_files: written,
        timings: StartWorkerTimings {
            workspace_files,
            tmux_start,
            viewer_attach,
        },
    })
}

/// Human-readable reason for an `AttachWorkerPane` response that is not a
/// success, for [`boss_protocol::LiveWorkerState::viewer_attach_error`].
/// The typed app error is spelled out (not `Debug`-formatted) because the
/// text is rendered verbatim on the worker's pane card.
pub fn viewer_attach_failure_reason(response: &EngineToAppResponse) -> String {
    match response {
        EngineToAppResponse::AttachWorkerPane { result: Err(err) } => format!("the app refused the viewer: {err}"),
        EngineToAppResponse::AttachWorkerPane { result: Ok(_) } => {
            unreachable!("a successful attach has no failure reason")
        }
        other => format!("the app answered the viewer attach with an unexpected response: {other:?}"),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod checkleft_bin_env_tests {
    use super::*;
    use tempfile::TempDir;

    fn env_with_worker_bin(dir: &Path) -> Vec<EnvVar> {
        vec![EnvVar {
            key: boss_engine_worker_bin::WORKER_BIN_DIR_ENV.to_owned(),
            value: dir.display().to_string(),
        }]
    }

    #[test]
    fn undeclared_workspace_does_not_export_checkleft_bin() {
        let workspace = TempDir::new().unwrap();
        let worker_bin = TempDir::new().unwrap();
        let mut env = env_with_worker_bin(worker_bin.path());
        apply_checkleft_bin_env(&mut env, workspace.path());
        assert!(
            !env.iter().any(|e| e.key == boss_engine_worker_bin::CHECKLEFT_BIN_ENV),
            "a repository without a REPOBIN.toml checkleft declaration must retain PATH behaviour",
        );
    }

    #[test]
    fn declared_checkleft_exports_the_worker_bin_launcher() {
        let workspace = TempDir::new().unwrap();
        std::fs::write(workspace.path().join("REPOBIN.toml"), "[tools.checkleft]\n").unwrap();
        let worker_bin = TempDir::new().unwrap();
        std::fs::write(worker_bin.path().join("checkleft"), b"#!/bin/sh\n").unwrap();
        let mut env = env_with_worker_bin(worker_bin.path());
        apply_checkleft_bin_env(&mut env, workspace.path());
        let expected = worker_bin.path().join("checkleft").display().to_string();
        assert_eq!(
            env.iter()
                .find(|e| e.key == boss_engine_worker_bin::CHECKLEFT_BIN_ENV)
                .map(|e| e.value.as_str()),
            Some(expected.as_str()),
            "declared checkleft must name the worker-bin launcher",
        );
    }

    #[test]
    fn declared_checkleft_without_a_written_launcher_is_not_exported() {
        let workspace = TempDir::new().unwrap();
        std::fs::write(workspace.path().join("REPOBIN.toml"), "[tools.checkleft]\n").unwrap();
        let worker_bin = TempDir::new().unwrap();
        let mut env = env_with_worker_bin(worker_bin.path());
        apply_checkleft_bin_env(&mut env, workspace.path());
        assert!(
            !env.iter().any(|e| e.key == boss_engine_worker_bin::CHECKLEFT_BIN_ENV),
            "must not export CHECKLEFT_BIN when the launcher file was never written",
        );
    }
}
