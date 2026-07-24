use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, bail};

use crate::coordinator::{DEFAULT_REVIEW_POOL_SIZE, MAX_AUTOMATION_POOL_SIZE, MAX_WORKER_POOL_SIZE};

/// Environment override for the SQLite state database path.
pub const DB_PATH_ENV: &str = "BOSS_DB_PATH";

/// Environment override for the worker events socket path. Read here (to
/// seed [`WorkConfig::events_socket_path`]) and by the `boss-event` shim;
/// nothing else in the engine may re-read it — see the field's doc comment.
pub const EVENTS_SOCKET_ENV: &str = "BOSS_EVENTS_SOCKET";

/// Environment override for the engine pid-file path.
pub const PID_PATH_ENV: &str = "BOSS_ENGINE_PID_PATH";

/// Default value for [`WorkConfig::max_review_cycles`]. Matches the
/// "~3 cycles at worst" mental model from the review-cycle-cap design, §7.
pub const DEFAULT_MAX_REVIEW_CYCLES: usize = 3;

/// Default threshold for the no-op / trivial-diff skip gate (review-cycle-cap
/// design, §8).
/// Zero means "skip only when the effective diff is literally empty (0 changed
/// lines)"; operators can raise this to also skip small cosmetic-only pushes.
pub const DEFAULT_MIN_REVIEW_CHANGED_LINES: u64 = 0;

/// Default line threshold for embedding `gh pr diff` output directly in the
/// reviewer's initial prompt. PRs whose diff is at or below this many lines
/// get the diff pre-embedded so the reviewer skips one `gh pr diff` tool
/// call. Set to 0 to disable embedding entirely. Operators can lower this for
/// cost-sensitive deployments or raise it to cover larger PRs.
pub const DEFAULT_MAX_EMBED_DIFF_LINES: u64 = 500;

/// Default value for [`WorkConfig::enable_revision_triggered_reviews`]. ON:
/// a `revision` task (CI-fix, conflict-resolution, operator-filed, or
/// reviewer-spawned) that pushes new commits to its parent PR triggers
/// another automated `pr_review` pass, same as the PR's first push does.
/// This is a 2026-07-01 experiment (gap: revisions previously landed
/// post-review commits with zero re-review) — operators can flip it off via
/// `BOSS_ENABLE_REVISION_TRIGGERED_REVIEWS=false` without a code revert if
/// the extra review cycles prove too slow or costly.
pub const DEFAULT_ENABLE_REVISION_TRIGGERED_REVIEWS: bool = true;

/// Default value for [`WorkConfig::merge_order_stagger_secs`]. **Zero disables
/// the stagger** (the conservative default the design specifies): the
/// non-blocking `merge_order` merge-sequencing relation is always active, but
/// the *optional* bounded dispatch stagger — delaying the "later" sibling of a
/// high-overlap pair by a small window so their diffs interleave less — ships
/// off. Operators opt in via `BOSS_MERGE_ORDER_STAGGER_SECS`. See the
/// merge-conflict-reduction design, Layer 3 / direction 2.
pub const DEFAULT_MERGE_ORDER_STAGGER_SECS: u64 = 0;

/// **Hard cap** on [`WorkConfig::merge_order_stagger_secs`]. The stagger is a
/// small offset (minutes), never "wait until the first sibling merges", so any
/// configured value is clamped to this ceiling at load time. 600s (10 min)
/// bounds the worst-case dispatch delay a misconfiguration can impose.
pub const MAX_MERGE_ORDER_STAGGER_SECS: u64 = 600;

/// Default value for [`WorkConfig::enable_spawn_capability_breaker`]. **ON**
/// by default. History: the breaker (`engine/core/src/spawn_health.rs`, PR
/// #1824) tripped for the first time ever on 2026-07-15 on what turned out
/// to be a benign cause — display sleep + App Nap throttling the app's
/// MainActor, making spawn acks 24-87s late — and latched the entire
/// fleet's dispatch, reviews included, for ~40 minutes until a human
/// manually resumed it. That incident drove the flag to default off
/// between PR #2041 and this change, via `BOSS_ENABLE_SPAWN_CAPABILITY_BREAKER`
/// opt-in only. Two fixes have since landed: the App Nap opt-out (display
/// sleep no longer degrades spawn acks) and the enabled-mode half-open
/// auto-recovery probe (a transient blip self-heals instead of latching), so
/// the breaker's pause is safe to re-enable by default for the genuine
/// app-dead/ghost-pane incident class it was designed for. Operators who
/// need to disable it can still opt out via
/// `BOSS_ENABLE_SPAWN_CAPABILITY_BREAKER=false`. The failure-window
/// tracking, logging, dispatch events, and attention item stay active
/// either way — only the actual dispatch pause (and its automatic recovery
/// machinery) are gated by this flag.
pub const DEFAULT_ENABLE_SPAWN_CAPABILITY_BREAKER: bool = true;

/// Default value for [`WorkConfig::coordinator_model`]. The Boss coordinator
/// session (the macOS app's single always-on Claude Code pane) launches on
/// this model unless overridden. Top-tier models are opt-in only: the
/// coordinator session is always-on and its token cost dominates, so it
/// defaults to `opus` independent of the effort table. Set
/// `BOSS_COORDINATOR_MODEL=fable` to opt an installation back into the
/// higher tier — no code change required.
pub const DEFAULT_COORDINATOR_MODEL: &str = "opus";

// Bare name used as the PATH fallback. In installed Boss.app the engine
// resolves cube from the bundle first (see resolve_cube_command); this
// constant is only reached in dev mode or when the bundle copy is absent.
const DEFAULT_CUBE_COMMAND: &str = "cube";

#[derive(Debug, Clone)]
pub struct CubeConfig {
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
#[non_exhaustive]
pub struct WorkConfig {
    pub cwd: PathBuf,
    pub db_path: PathBuf,
    /// The worker events socket this engine binds — and therefore the path
    /// baked into every worker's `settings.json` so its `boss-event` hook
    /// shim dials *this* engine.
    ///
    /// Resolved once (from `BOSS_EVENTS_SOCKET`, else the production default)
    /// and, for a test fixture, overwritten by the isolation guard in
    /// [`crate::app::run`] before the config is frozen — exactly the way
    /// `db_path` is handled. Everything downstream that needs to know where
    /// the engine is listening reads it from here rather than re-deriving it
    /// from the environment: a fixture that isolated its own socket but
    /// handed workers `$BOSS_EVENTS_SOCKET` would send every hook to the
    /// production engine.
    ///
    /// `None` means "this engine binds no events socket" — the in-process
    /// `serve(..., None, ...)` shape used by tests.
    pub events_socket_path: Option<PathBuf>,
    /// Defaults to 1 so test call sites don't need updating when new pool
    /// fields are added.
    #[builder(default = 1)]
    pub worker_pool_size: usize,
    /// Size of the dedicated automation worker pool. Configured via
    /// `BOSS_AUTOMATION_POOL_SIZE`; defaults to [`MAX_AUTOMATION_POOL_SIZE`].
    #[builder(default = 1)]
    pub automation_pool_size: usize,
    /// Size of the dedicated review worker pool. Configured via
    /// `BOSS_REVIEW_POOL_SIZE`; defaults to [`DEFAULT_REVIEW_POOL_SIZE`]
    /// (deliberately small to bound always-Opus review spend).
    #[builder(default = 1)]
    pub review_pool_size: usize,
    /// Maximum number of automated reviewer passes to run per PR.
    /// When a producing task's `review_cycle` reaches this value the engine
    /// skips the next reviewer pass and advances the task to human Review
    /// directly. Configured via `BOSS_MAX_REVIEW_CYCLES`; defaults to
    /// [`DEFAULT_MAX_REVIEW_CYCLES`] (3). Review-cycle-cap design, §7.
    #[builder(default = DEFAULT_MAX_REVIEW_CYCLES)]
    pub max_review_cycles: usize,
    /// Minimum number of changed lines (additions + deletions) required to
    /// trigger a reviewer pass when `last_reviewed_sha` is set. Pushes whose
    /// effective diff (new head vs. last-reviewed head) totals fewer lines
    /// than this threshold are skipped as trivial. Zero (the default) means
    /// skip only when the diff is completely empty; operators can raise it to
    /// also skip small cosmetic pushes. Configured via
    /// `BOSS_MIN_REVIEW_CHANGED_LINES`; defaults to
    /// [`DEFAULT_MIN_REVIEW_CHANGED_LINES`] (0). Review-cycle-cap design, §8.
    #[builder(default = DEFAULT_MIN_REVIEW_CHANGED_LINES)]
    pub min_review_changed_lines: u64,
    /// Maximum diff size (lines) at which the engine pre-embeds the full
    /// `gh pr diff` output in the reviewer's initial prompt. PRs at or below
    /// this threshold skip the reviewer's first `gh pr diff` tool call.
    /// Set to 0 to disable embedding. Configured via
    /// `BOSS_MAX_EMBED_DIFF_LINES`; defaults to
    /// [`DEFAULT_MAX_EMBED_DIFF_LINES`] (500).
    #[builder(default = DEFAULT_MAX_EMBED_DIFF_LINES)]
    pub max_review_embed_diff_lines: u64,
    /// Whether a completed `revision` task that pushed new commits to its
    /// parent PR triggers another automated reviewer pass. ON by default.
    /// Configured via `BOSS_ENABLE_REVISION_TRIGGERED_REVIEWS`; defaults to
    /// [`DEFAULT_ENABLE_REVISION_TRIGGERED_REVIEWS`] (`true`). Kill-switch
    /// for the revision-triggered-review experiment — flip off to fall back
    /// to the legacy "revisions are never re-reviewed" behaviour without a
    /// revert.
    #[builder(default = DEFAULT_ENABLE_REVISION_TRIGGERED_REVIEWS)]
    pub enable_revision_triggered_reviews: bool,
    /// Bounded dispatch stagger for high-overlap `merge_order` sibling pairs
    /// (seconds). When > 0, the "later" side of a `merge_order` pairing whose
    /// "first" side is concurrently in flight has its *first* dispatch delayed
    /// by this many seconds (one-shot), so the two workers' diffs interleave
    /// less. **Never a block and never waits for a merge** — a small offset
    /// only. Zero (the default) disables it. Configured via
    /// `BOSS_MERGE_ORDER_STAGGER_SECS`; hard-capped at
    /// [`MAX_MERGE_ORDER_STAGGER_SECS`]. Design: Layer 3 / direction 2.
    #[builder(default = DEFAULT_MERGE_ORDER_STAGGER_SECS)]
    pub merge_order_stagger_secs: u64,
    /// Whether the spawn-capability circuit breaker (`spawn_health.rs`) is
    /// allowed to actually pause dispatch when it trips. ON by default —
    /// see [`DEFAULT_ENABLE_SPAWN_CAPABILITY_BREAKER`] for the 2026-07-15
    /// incident that motivated defaulting it off between PR #2041 and this
    /// change, and why it is safe to default on again now. Configured via
    /// `BOSS_ENABLE_SPAWN_CAPABILITY_BREAKER`. When `false`, the breaker
    /// still tracks failures, logs, raises its attention item, and emits its
    /// dispatch event on trip — it just never calls `set_dispatch_paused`.
    #[builder(default = DEFAULT_ENABLE_SPAWN_CAPABILITY_BREAKER)]
    pub enable_spawn_capability_breaker: bool,
    /// Model slug the Boss coordinator session launches with, pushed to the
    /// macOS app as `EnginePoolConfig.coordinator_model` on every
    /// `RegisterAppSession`. Configured via `BOSS_COORDINATOR_MODEL`;
    /// defaults to [`DEFAULT_COORDINATOR_MODEL`] (`"opus"`). Set to `fable`
    /// (or any other model slug) to opt a given installation back into a
    /// higher tier without a code change.
    #[builder(default = DEFAULT_COORDINATOR_MODEL.to_owned())]
    pub coordinator_model: String,
}

impl WorkConfig {
    pub fn load_from_env() -> Result<Self> {
        Self::load_from(|k| std::env::var_os(k))
    }

    /// Load config from an explicit env lookup rather than the process
    /// environment. Tests call this directly so they never mutate global state.
    pub fn load_from(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self> {
        let cwd = resolve_runtime_cwd_with(&lookup)?;
        let db_path = match lookup(DB_PATH_ENV) {
            Some(path) => PathBuf::from(path),
            None => default_db_path()?,
        };
        let events_socket_path = match lookup(EVENTS_SOCKET_ENV) {
            Some(path) => Some(PathBuf::from(path)),
            None => boss_log_files::default_events_socket_path(),
        };
        // Default to the hard cap so the engine pool tracks the macOS
        // app's slot count (`WorkersWorkspaceModel.workerSlotCount = 8`).
        // A smaller default left slots 5–8 idle while the dispatcher
        // silently no-op'd new work. `BOSS_WORKER_POOL_SIZE` still
        // overrides for callers that genuinely want fewer workers.
        let worker_pool_size = lookup_usize(&lookup, "BOSS_WORKER_POOL_SIZE")?.unwrap_or(MAX_WORKER_POOL_SIZE);
        let automation_pool_size =
            lookup_usize(&lookup, "BOSS_AUTOMATION_POOL_SIZE")?.unwrap_or(MAX_AUTOMATION_POOL_SIZE);
        let review_pool_size = lookup_usize(&lookup, "BOSS_REVIEW_POOL_SIZE")?.unwrap_or(DEFAULT_REVIEW_POOL_SIZE);
        let max_review_cycles = lookup_usize(&lookup, "BOSS_MAX_REVIEW_CYCLES")?.unwrap_or(DEFAULT_MAX_REVIEW_CYCLES);
        let min_review_changed_lines =
            lookup_u64(&lookup, "BOSS_MIN_REVIEW_CHANGED_LINES")?.unwrap_or(DEFAULT_MIN_REVIEW_CHANGED_LINES);
        let max_review_embed_diff_lines =
            lookup_u64(&lookup, "BOSS_MAX_EMBED_DIFF_LINES")?.unwrap_or(DEFAULT_MAX_EMBED_DIFF_LINES);
        let enable_revision_triggered_reviews = lookup_bool(&lookup, "BOSS_ENABLE_REVISION_TRIGGERED_REVIEWS")?
            .unwrap_or(DEFAULT_ENABLE_REVISION_TRIGGERED_REVIEWS);
        // Clamp to the hard cap at load time so no downstream consumer can ever
        // see an out-of-bounds stagger window (defense in depth: the coordinator
        // also treats 0 as "disabled").
        let merge_order_stagger_secs = lookup_u64(&lookup, "BOSS_MERGE_ORDER_STAGGER_SECS")?
            .unwrap_or(DEFAULT_MERGE_ORDER_STAGGER_SECS)
            .min(MAX_MERGE_ORDER_STAGGER_SECS);
        let enable_spawn_capability_breaker = lookup_bool(&lookup, "BOSS_ENABLE_SPAWN_CAPABILITY_BREAKER")?
            .unwrap_or(DEFAULT_ENABLE_SPAWN_CAPABILITY_BREAKER);
        let coordinator_model = lookup_string(&lookup, "BOSS_COORDINATOR_MODEL")
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_COORDINATOR_MODEL.to_owned());
        Ok(WorkConfig::builder()
            .cwd(cwd)
            .db_path(db_path)
            .maybe_events_socket_path(events_socket_path)
            .worker_pool_size(worker_pool_size)
            .automation_pool_size(automation_pool_size)
            .review_pool_size(review_pool_size)
            .max_review_cycles(max_review_cycles)
            .min_review_changed_lines(min_review_changed_lines)
            .max_review_embed_diff_lines(max_review_embed_diff_lines)
            .enable_revision_triggered_reviews(enable_revision_triggered_reviews)
            .merge_order_stagger_secs(merge_order_stagger_secs)
            .enable_spawn_capability_breaker(enable_spawn_capability_breaker)
            .coordinator_model(coordinator_model)
            .build())
    }
}

fn lookup_usize(lookup: impl Fn(&str) -> Option<OsString>, name: &str) -> Result<Option<usize>> {
    match lookup(name) {
        None => Ok(None),
        Some(val) => {
            let raw = val.to_string_lossy().into_owned();
            raw.parse::<usize>()
                .with_context(|| format!("could not parse {name}: {raw}"))
                .map(Some)
        }
    }
}

fn lookup_u64(lookup: impl Fn(&str) -> Option<OsString>, name: &str) -> Result<Option<u64>> {
    match lookup(name) {
        None => Ok(None),
        Some(val) => {
            let raw = val.to_string_lossy().into_owned();
            raw.parse::<u64>()
                .with_context(|| format!("could not parse {name}: {raw}"))
                .map(Some)
        }
    }
}

fn lookup_string(lookup: impl Fn(&str) -> Option<OsString>, name: &str) -> Option<String> {
    lookup(name).map(|val| val.to_string_lossy().into_owned())
}

fn lookup_bool(lookup: impl Fn(&str) -> Option<OsString>, name: &str) -> Result<Option<bool>> {
    match lookup(name) {
        None => Ok(None),
        Some(val) => {
            let raw = val.to_string_lossy().into_owned();
            match raw.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Ok(Some(true)),
                "0" | "false" | "no" | "off" => Ok(Some(false)),
                _ => bail!("could not parse {name}: {raw} (expected true/false)"),
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub anthropic_api_key: Option<String>,
    pub cube: CubeConfig,
    pub cwd: PathBuf,
}

impl AgentConfig {
    pub fn load_from_env(work: &WorkConfig) -> Result<Self> {
        let anthropic_api_key = std::env::var("ANTHROPIC_API_KEY").ok();

        let (cube_command, cube_args) = parse_command_line(
            "BOSS_CUBE_CMD",
            std::env::var("BOSS_CUBE_CMD").unwrap_or_else(|_| resolve_cube_command()),
        )?;

        log_cube_resolution(&cube_command);

        Ok(Self {
            anthropic_api_key,
            cube: CubeConfig {
                command: cube_command,
                args: cube_args,
            },
            cwd: work.cwd.clone(),
        })
    }
}

#[derive(Debug)]
pub struct RuntimeConfig {
    pub work: WorkConfig,
    agent_cell: OnceLock<Arc<AgentConfig>>,
}

impl RuntimeConfig {
    pub fn load_from_env() -> Result<Self> {
        Ok(Self {
            work: WorkConfig::load_from_env()?,
            agent_cell: OnceLock::new(),
        })
    }

    pub fn from_parts(work: WorkConfig, agent: Option<AgentConfig>) -> Self {
        let cell = OnceLock::new();
        if let Some(agent) = agent {
            let _ = cell.set(Arc::new(agent));
        }
        Self { work, agent_cell: cell }
    }

    /// Return a copy of this config with `work` replaced, carrying over any
    /// already-resolved [`AgentConfig`] so the swap doesn't force a second
    /// (fallible, env-reading) agent load.
    ///
    /// Used by `serve` to stamp the events-socket path it actually bound onto
    /// the config an in-process caller supplied without one.
    pub fn with_work(&self, work: WorkConfig) -> Self {
        let cell = OnceLock::new();
        if let Some(agent) = self.agent_cell.get() {
            let _ = cell.set(agent.clone());
        }
        Self { work, agent_cell: cell }
    }

    pub fn agent(&self) -> Result<Arc<AgentConfig>> {
        if let Some(agent) = self.agent_cell.get() {
            return Ok(agent.clone());
        }
        let loaded = AgentConfig::load_from_env(&self.work)?;
        let arc = Arc::new(loaded);
        match self.agent_cell.set(arc.clone()) {
            Ok(()) => Ok(arc),
            Err(_) => Ok(self.agent_cell.get().expect("OnceLock set after failed insert").clone()),
        }
    }
}

/// Returns the cube command to use, preferring a bundle-relative binary when
/// the engine itself was launched from a bundle (installed Boss.app).
///
/// Resolution order:
///   1. `<engine_exe_dir>/cube` — present in the bundle; used by installed
///      Boss.app so the engine never depends on the GUI launchd PATH.
///   2. `"cube"` — bare name resolved from PATH at exec time; used in dev
///      mode where the engine runs via `bazel run` outside a bundle.
///
/// Workers run inside Ghostty terminal panes which inherit the user's shell
/// PATH, so they continue to resolve cube (and jj, gh, claude, etc.) from
/// PATH naturally. This bundle-relative lookup is engine-only.
fn resolve_cube_command() -> String {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(DEFAULT_CUBE_COMMAND);
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    DEFAULT_CUBE_COMMAND.to_owned()
}

/// Logs how cube was resolved and warns if the bare name cannot be found on PATH.
fn log_cube_resolution(command: &str) {
    if command.contains('/') {
        tracing::info!(command, "cube resolved from bundle");
        return;
    }
    let path_env = std::env::var("PATH").unwrap_or_default();
    let found = std::env::split_paths(&path_env).any(|dir| dir.join(command).is_file());
    if found {
        tracing::info!(command, "cube resolved from PATH");
    } else {
        tracing::warn!(
            command,
            "cube executable not found on PATH; worker dispatch will fail — \
             install cube or set BOSS_CUBE_CMD to its full path"
        );
    }
}

fn parse_command_line(env_var: &str, command_line: String) -> Result<(String, Vec<String>)> {
    let parts = shlex::split(&command_line).with_context(|| format!("could not parse {env_var}: {command_line}"))?;

    let Some((command, args)) = parts.split_first() else {
        bail!("{env_var} resolved to an empty command");
    };

    Ok((command.clone(), args.to_vec()))
}

fn resolve_runtime_cwd_with(lookup: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
    if let Some(path) = lookup("BUILD_WORKSPACE_DIRECTORY") {
        let candidate = PathBuf::from(path);
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }

    std::env::current_dir().context("failed to resolve current working directory")
}

fn default_db_path() -> Result<PathBuf> {
    let Some(path) = boss_log_files::default_state_db_path() else {
        bail!("HOME must be set to derive the default Boss database path");
    };

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_COORDINATOR_MODEL, DEFAULT_ENABLE_REVISION_TRIGGERED_REVIEWS, DEFAULT_ENABLE_SPAWN_CAPABILITY_BREAKER,
        DEFAULT_MAX_EMBED_DIFF_LINES, DEFAULT_MAX_REVIEW_CYCLES, DEFAULT_MERGE_ORDER_STAGGER_SECS,
        DEFAULT_MIN_REVIEW_CHANGED_LINES, DEFAULT_REVIEW_POOL_SIZE, MAX_AUTOMATION_POOL_SIZE,
        MAX_MERGE_ORDER_STAGGER_SECS, MAX_WORKER_POOL_SIZE, WorkConfig,
    };
    use std::ffi::OsString;

    #[test]
    fn prefers_bazel_workspace_directory_when_present() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path = tempdir.path().join("state.db");
        // Can't use env_map here because tempdir paths are runtime values,
        // so build the closure directly.
        let config = WorkConfig::load_from(|k| match k {
            "BUILD_WORKSPACE_DIRECTORY" => Some(OsString::from(tempdir.path())),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path)),
            _ => None,
        })
        .unwrap();
        assert_eq!(config.cwd, tempdir.path());
    }

    /// `WorkConfig::load_from` must default to the hard cap
    /// (`MAX_WORKER_POOL_SIZE`) when `BOSS_WORKER_POOL_SIZE` is absent,
    /// matching the macOS app's slot count. A lower default left
    /// slots 5–8 unallocated and silently dropped any drag-to-Doing
    /// dispatch once slots 1–4 were busy.
    #[test]
    fn worker_pool_size_defaults_to_max_when_env_unset() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.worker_pool_size, MAX_WORKER_POOL_SIZE);
    }

    // Default-and-override are checked in a single test (rather than the
    // two-test pattern) so the two cases can't run in parallel and race on
    // the shared process-global `BOSS_AUTOMATION_POOL_SIZE`: `config::tests`
    // all land in the multi-threaded `engine_lib_test_rest` shard. (Same
    // rationale as `review_pool_size_defaults_and_reads_from_env` below.)
    #[test]
    fn automation_pool_size_defaults_and_reads_from_env() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");

        // Absent → falls back to the max default.
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.automation_pool_size, MAX_AUTOMATION_POOL_SIZE);

        // Set → the env value wins.
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_AUTOMATION_POOL_SIZE" => Some(OsString::from("2")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.automation_pool_size, 2);
    }

    #[test]
    fn review_pool_size_defaults_and_reads_from_env() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");

        // Absent → falls back to the small default.
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.review_pool_size, DEFAULT_REVIEW_POOL_SIZE);

        // Present → the explicit value wins.
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_REVIEW_POOL_SIZE" => Some(OsString::from("1")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.review_pool_size, 1);
    }

    #[test]
    fn min_review_changed_lines_defaults_and_reads_from_env() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");

        let config = WorkConfig::load_from(|k| match k {
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.min_review_changed_lines, DEFAULT_MIN_REVIEW_CHANGED_LINES);

        let config = WorkConfig::load_from(|k| match k {
            "BOSS_MIN_REVIEW_CHANGED_LINES" => Some(OsString::from("10")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.min_review_changed_lines, 10);
    }

    #[test]
    fn max_review_cycles_defaults_and_reads_from_env() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");

        // Absent → falls back to the hardcoded default (3).
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.max_review_cycles, DEFAULT_MAX_REVIEW_CYCLES);

        // Present → the explicit value wins.
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_MAX_REVIEW_CYCLES" => Some(OsString::from("5")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.max_review_cycles, 5);
    }

    #[test]
    fn max_embed_diff_lines_defaults_and_reads_from_env() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");

        let config = WorkConfig::load_from(|k| match k {
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.max_review_embed_diff_lines, DEFAULT_MAX_EMBED_DIFF_LINES);

        let config = WorkConfig::load_from(|k| match k {
            "BOSS_MAX_EMBED_DIFF_LINES" => Some(OsString::from("200")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.max_review_embed_diff_lines, 200);
    }

    #[test]
    fn enable_revision_triggered_reviews_defaults_on_and_reads_from_env() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");

        // Absent → defaults ON.
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(
            config.enable_revision_triggered_reviews,
            DEFAULT_ENABLE_REVISION_TRIGGERED_REVIEWS
        );
        assert!(config.enable_revision_triggered_reviews, "kill-switch defaults ON");

        // Explicit "false" → the kill-switch flips off.
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_ENABLE_REVISION_TRIGGERED_REVIEWS" => Some(OsString::from("false")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert!(!config.enable_revision_triggered_reviews);

        // Explicit "1" → also accepted as true.
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_ENABLE_REVISION_TRIGGERED_REVIEWS" => Some(OsString::from("1")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert!(config.enable_revision_triggered_reviews);
    }

    #[test]
    fn merge_order_stagger_secs_defaults_off_reads_env_and_clamps_to_cap() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");

        // Absent → defaults OFF (0).
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.merge_order_stagger_secs, DEFAULT_MERGE_ORDER_STAGGER_SECS);
        assert_eq!(config.merge_order_stagger_secs, 0, "stagger defaults off");

        // A modest value within the cap is honored verbatim.
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_MERGE_ORDER_STAGGER_SECS" => Some(OsString::from("90")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.merge_order_stagger_secs, 90);

        // An over-cap value is hard-clamped to the ceiling (never unbounded).
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_MERGE_ORDER_STAGGER_SECS" => Some(OsString::from("100000")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.merge_order_stagger_secs, MAX_MERGE_ORDER_STAGGER_SECS);
    }

    #[test]
    fn enable_spawn_capability_breaker_defaults_on_and_reads_env() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");

        // Absent → defaults ON: the App Nap opt-out and half-open
        // auto-recovery probe make the dispatch pause safe again for the
        // genuine app-dead/ghost-pane incident class it was designed for.
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(
            config.enable_spawn_capability_breaker,
            DEFAULT_ENABLE_SPAWN_CAPABILITY_BREAKER
        );
        assert!(config.enable_spawn_capability_breaker, "breaker defaults on");

        // Explicit "false" opts out.
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_ENABLE_SPAWN_CAPABILITY_BREAKER" => Some(OsString::from("false")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert!(!config.enable_spawn_capability_breaker);

        // Explicit "1" is also accepted as true.
        let config = WorkConfig::load_from(|k| match k {
            "BOSS_ENABLE_SPAWN_CAPABILITY_BREAKER" => Some(OsString::from("1")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert!(config.enable_spawn_capability_breaker);
    }

    #[test]
    fn enable_revision_triggered_reviews_rejects_unparseable_value() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");

        let err = WorkConfig::load_from(|k| match k {
            "BOSS_ENABLE_REVISION_TRIGGERED_REVIEWS" => Some(OsString::from("maybe")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect_err("unparseable bool must error");
        assert!(err.to_string().contains("BOSS_ENABLE_REVISION_TRIGGERED_REVIEWS"));
    }

    /// The coordinator model must default to `opus` (top-tier models are
    /// opt-in only) and stay overridable via `BOSS_COORDINATOR_MODEL` so an
    /// installation can opt back into Fable without a code change.
    #[test]
    fn coordinator_model_defaults_to_opus_and_reads_from_env() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");

        let config = WorkConfig::load_from(|k| match k {
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.coordinator_model, DEFAULT_COORDINATOR_MODEL);
        assert_eq!(config.coordinator_model, "opus");

        let config = WorkConfig::load_from(|k| match k {
            "BOSS_COORDINATOR_MODEL" => Some(OsString::from("fable")),
            "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
            _ => None,
        })
        .expect("config loads");
        assert_eq!(config.coordinator_model, "fable");
    }

    /// A blank or whitespace-only `BOSS_COORDINATOR_MODEL` must fall back to
    /// the default rather than producing an empty/blank model slug that
    /// would fail at pane spawn with no useful diagnostic.
    #[test]
    fn coordinator_model_ignores_blank_or_whitespace_env_value() {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path_str = tempdir.path().join("state.db");

        for blank in ["", "   "] {
            let blank_owned = blank.to_owned();
            let config = WorkConfig::load_from(|k| match k {
                "BOSS_COORDINATOR_MODEL" => Some(OsString::from(&blank_owned)),
                "BOSS_DB_PATH" => Some(OsString::from(&db_path_str)),
                _ => None,
            })
            .expect("config loads");
            assert_eq!(
                config.coordinator_model, DEFAULT_COORDINATOR_MODEL,
                "blank env value {blank:?} must fall back to the default"
            );
        }
    }
}
