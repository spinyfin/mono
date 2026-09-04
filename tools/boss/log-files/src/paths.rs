//! Log-source -> on-disk path resolution, including the audit-path env
//! override and the default Boss state root.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Environment variable that overrides the audit-log path. Honoured by both
/// the engine (writer) and `bossctl` (reader) so they always agree on which
/// file they are talking about. Owning the constant here is what lets
/// `bossctl` resolve the audit path without depending on the engine crate.
pub const AUDIT_PATH_ENV: &str = "BOSS_ENGINE_AUDIT_PATH";

/// Filename of the structured engine trace log under the state root.
pub const ENGINE_TRACE_FILENAME: &str = "engine-trace.jsonl";

/// Filename of the engine lifecycle audit log under the state root.
pub const ENGINE_AUDIT_FILENAME: &str = "engine-audit.log";

/// Filename of the SQLite state database under the state root.
pub const STATE_DB_FILENAME: &str = "state.db";

/// Filename of the worker events socket under the state root.
pub const EVENTS_SOCKET_FILENAME: &str = "events.sock";

/// Filename of the engine frontend socket under the state root.
pub const FRONTEND_SOCKET_FILENAME: &str = "engine.sock";

/// Filename of the engine process-id file under the state root.
pub const ENGINE_PID_FILENAME: &str = "engine.pid";

/// Filename of the human-readable engine log under the state root.
pub const ENGINE_TEXT_LOG_FILENAME: &str = "engine.log";

/// Filename of Boss's private tmux server socket under the state root.
pub const TMUX_SOCKET_FILENAME: &str = "tmux.sock";

/// Filename of the engine-control token under the state root.
pub const CONTROL_TOKEN_FILENAME: &str = "engine-control.token";

/// Directory under the state root holding the dispatch event stream.
pub const DISPATCH_EVENTS_DIR: &str = "dispatch-events";

/// Live dispatch-events filename inside [`DISPATCH_EVENTS_DIR`].
pub const DISPATCH_EVENTS_LIVE_FILENAME: &str = "current.jsonl";

/// Directory under the state root holding day-rotated diagnostic JSONL files.
pub const DIAGNOSTICS_DIR: &str = "diagnostics";

/// Day-rotated filename prefix for worker-spawn diagnostics
/// (`spawn-YYYY-MM-DD.jsonl`).
pub const SPAWN_DIAGNOSTICS_PREFIX: &str = "spawn-";

/// Day-rotated filename prefix for engine population-timing diagnostics
/// (`engine-population-timing-YYYY-MM-DD.jsonl`).
pub const POPULATION_TIMING_PREFIX: &str = "engine-population-timing-";

/// Day-rotated filename prefix for app-side population-timing diagnostics
/// (`population-timing-YYYY-MM-DD.jsonl` under the same `diagnostics/` dir).
///
/// The macOS app writes the primary client-side stream under this prefix; the
/// engine writes under [`POPULATION_TIMING_PREFIX`]. Both belong to the
/// `population-timing` log source.
pub const APP_POPULATION_TIMING_PREFIX: &str = "population-timing-";

/// Which engine log / diagnostic stream a reader is targeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    /// `engine-trace.jsonl` — structured tracing events (primary log).
    EngineTrace,
    /// `engine-audit.log` — lifecycle events (start, socket bind, shutdown).
    Audit,
    /// `dispatch-events/current.jsonl` — dispatch pipeline stage events.
    Dispatch,
    /// `diagnostics/spawn-YYYY-MM-DD.jsonl` — worker-spawn diagnostics.
    Spawn,
    /// App + engine population-timing day files under `diagnostics/`:
    /// `population-timing-YYYY-MM-DD.jsonl` and
    /// `engine-population-timing-YYYY-MM-DD.jsonl`.
    PopulationTiming,
}

impl LogSource {
    /// A short name suitable for CLI display / JSON `"source"` fields.
    pub fn as_str(self) -> &'static str {
        match self {
            LogSource::EngineTrace => "engine",
            LogSource::Audit => "audit",
            LogSource::Dispatch => "dispatch",
            LogSource::Spawn => "spawn",
            LogSource::PopulationTiming => "population-timing",
        }
    }

    /// True when this source uses the `<base>.<unix_seconds>` rotation scheme
    /// (trace + audit + dispatch). Day-rotated sources return false.
    pub fn uses_timestamp_rotation(self) -> bool {
        matches!(self, LogSource::EngineTrace | LogSource::Audit | LogSource::Dispatch)
    }

    /// The bare live filename this source resolves to under a state root, when
    /// it is a single live file (trace / audit / dispatch). Day-rotated
    /// sources have no single live filename — use
    /// [`resolve_log_source_files`] instead.
    pub fn filename(self) -> Option<&'static str> {
        match self {
            LogSource::EngineTrace => Some(ENGINE_TRACE_FILENAME),
            LogSource::Audit => Some(ENGINE_AUDIT_FILENAME),
            LogSource::Dispatch => Some(DISPATCH_EVENTS_LIVE_FILENAME),
            LogSource::Spawn | LogSource::PopulationTiming => None,
        }
    }
}

/// Directory production's state-root files (db, events socket, control
/// token, tmux socket, audit/trace logs — everything under the state root)
/// live under, relative to `$HOME`.
///
/// Exposed so callers that need to recognize production's *shape* without
/// depending on this process's own `$HOME` can share one definition —
/// see [`is_production_shaped`].
pub const STATE_ROOT_SUFFIX: &str = "Library/Application Support/Boss";

/// Isolated state root installed lazily for a Bazel *test* process.
/// `None` until a state path is resolved, and in ordinary production
/// executables — including `bazel run` of production binaries, which
/// also live under `bazel-out`.
///
/// This is the chokepoint every `default_*_path` function in this module
/// ultimately derives from ([`default_state_root`]), so a test process gets
/// exactly one place to install isolation and every derived path (db,
/// sockets, pid, control token, audit/trace/dispatch logs) follows.
static TEST_STATE_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Install the isolated state root for this process. Idempotent — only the
/// first call wins. Test-only: production installs via
/// [`install_bazel_test_root_if_needed`]'s `get_or_init`.
#[cfg(test)]
fn install_test_state_root(root: PathBuf) {
    let _ = TEST_STATE_ROOT.set(root);
}

/// True for a process with an isolated state root already installed, or
/// a Bazel-built *test* executable (see [`is_bazel_test_executable`]).
/// `bazel run` of a production binary is not a test process: those
/// executables also live under `bazel-out`, and must keep production's
/// state root, tmux socket, and audit path.
pub fn is_test_process() -> bool {
    TEST_STATE_ROOT.get().is_some() || running_as_bazel_test_process()
}

fn bazel_test_harness_env_present() -> bool {
    ["TEST_TMPDIR", "TEST_SRCDIR", "BAZEL_TEST"]
        .iter()
        .any(|key| std::env::var_os(key).is_some())
}

fn path_is_bazel_output(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == "bazel-out" || component.as_os_str() == "bazel-bin")
}

fn executable_stem_ends_with_test(path: &Path) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.ends_with("_test"))
}

/// Pure discriminator: a Bazel-built *test* binary, not every binary
/// that happens to live under `bazel-out` / `bazel-bin`.
///
/// `bazel run` of production targets (`engine`, `Boss`, `boss`) also
/// produces executables under those trees; those must keep production's
/// state root. A test binary is identified by Bazel's test harness env
/// (`TEST_TMPDIR` / `TEST_SRCDIR` / `BAZEL_TEST`, present under
/// `bazel test`) or by the `_test` file-stem convention
/// `boss_rust_test` enforces — covering a direct
/// `bazel-bin/.../engine_lib_test` invocation that has no harness env.
fn is_bazel_test_executable(exe: &Path, bazel_test_env: bool) -> bool {
    path_is_bazel_output(exe) && (bazel_test_env || executable_stem_ends_with_test(exe))
}

fn running_as_bazel_test_process() -> bool {
    let exe = match std::env::current_exe() {
        Ok(path) => std::fs::canonicalize(&path).unwrap_or(path),
        Err(_) => return false,
    };
    is_bazel_test_executable(&exe, bazel_test_harness_env_present())
}

fn install_bazel_test_root_if_needed() {
    if TEST_STATE_ROOT.get().is_some() || !running_as_bazel_test_process() {
        return;
    }
    // `get_or_init` serializes the path choice so two racing callers cannot
    // each `create_dir_all` a distinct nanos-suffixed directory and leak
    // the loser. The path is pid-keyed, so repeated resolutions in one
    // process reuse the same directory; a recycled pid wipes any leftover.
    let _ = TEST_STATE_ROOT.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("boss-test-isolation-{}", std::process::id()));
        if root.exists() {
            let _ = std::fs::remove_dir_all(&root);
        }
        std::fs::create_dir_all(&root).unwrap_or_else(|error| {
            panic!("boss-log-files: refusing to run Bazel test without an isolated state root: {error}")
        });
        root
    });
}

/// The default Boss state root. In a production process this is
/// `$HOME/Library/Application Support/Boss` (`None` when `HOME` is unset).
/// In a test process (see [`is_test_process`]) this is the isolated root
/// installed at first resolution — **never** `$HOME`, so a test
/// binary invoked directly (bypassing `bazel test`'s `HOME` redirect and
/// seatbelt) still cannot resolve production's state root.
///
pub fn default_state_root() -> Option<PathBuf> {
    install_bazel_test_root_if_needed();
    resolve_state_root(
        is_test_process(),
        TEST_STATE_ROOT.get().map(PathBuf::as_path),
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
    )
}

/// Pure decision logic behind [`default_state_root`], split out so tests can
/// exercise every branch — including the refusal — without depending on the
/// real process-global installed-root state, which sibling tests may populate
/// while running concurrently.
fn resolve_state_root(is_test_process: bool, installed_root: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    if is_test_process {
        let root = installed_root.unwrap_or_else(|| {
            panic!(
                "boss-log-files: refusing to resolve Boss's production state root (~/{STATE_ROOT_SUFFIX}) \
                 from a Bazel test process — no isolated test root could be installed. Ensure the \
                 system temporary directory is writable."
            )
        });
        return Some(root.to_path_buf());
    }
    Some(home?.join(STATE_ROOT_SUFFIX))
}

/// Does `path` have production's *shape* for `filename` — i.e. is it named
/// exactly `filename` and does its parent end with production's state-root
/// suffix ([`STATE_ROOT_SUFFIX`])?
///
/// A structural check independent of *whose* `$HOME` produced `path`, so it
/// catches an override or ambient path inherited from a production engine
/// running under a different `$HOME` than this process (or with `HOME`
/// unset here entirely) — cases plain path-equality against this process's
/// own production model cannot see. See callers for the safe-direction
/// tradeoff this implies: a deliberately-chosen private path that happens to
/// reproduce this shape under a different root is still treated as
/// production-shaped, not honored as an intentional override.
pub fn is_production_shaped(path: &Path, filename: &str) -> bool {
    if path.file_name().and_then(|n| n.to_str()) != Some(filename) {
        return false;
    }
    path.parent().is_some_and(|parent| parent.ends_with(STATE_ROOT_SUFFIX))
}

/// Production location of the SQLite state database:
/// `<default_state_root>/state.db`. `None` when `HOME` is unset.
///
/// This and its siblings below exist so that "what path does production
/// own?" has exactly one answer. The engine's test-fixture isolation guard
/// compares resolved paths against these to tell a deliberate operator
/// override apart from a production path inherited through the environment.
pub fn default_state_db_path() -> Option<PathBuf> {
    Some(default_state_root()?.join(STATE_DB_FILENAME))
}

/// Production location of the worker events socket:
/// `<default_state_root>/events.sock`. `None` when `HOME` is unset.
pub fn default_events_socket_path() -> Option<PathBuf> {
    Some(default_state_root()?.join(EVENTS_SOCKET_FILENAME))
}

/// Production location of the engine frontend socket:
/// `<default_state_root>/engine.sock`. `None` when `HOME` is unset.
pub fn default_frontend_socket_path() -> Option<PathBuf> {
    Some(default_state_root()?.join(FRONTEND_SOCKET_FILENAME))
}

/// Production location of the engine pid file:
/// `<default_state_root>/engine.pid`. `None` when `HOME` is unset.
pub fn default_engine_pid_path() -> Option<PathBuf> {
    Some(default_state_root()?.join(ENGINE_PID_FILENAME))
}

/// Production location of the human-readable engine log:
/// `<default_state_root>/engine.log`. `None` when `HOME` is unset.
pub fn default_engine_text_log_path() -> Option<PathBuf> {
    Some(default_state_root()?.join(ENGINE_TEXT_LOG_FILENAME))
}

/// Production location of Boss's private tmux server socket:
/// `<default_state_root>/tmux.sock`. `None` when `HOME` is unset.
pub fn default_tmux_socket_path() -> Option<PathBuf> {
    Some(default_state_root()?.join(TMUX_SOCKET_FILENAME))
}

/// Production location of the engine-control token:
/// `<default_state_root>/engine-control.token`. `None` when `HOME` is unset.
pub fn default_control_token_path() -> Option<PathBuf> {
    Some(default_state_root()?.join(CONTROL_TOKEN_FILENAME))
}

/// The audit-path override from [`AUDIT_PATH_ENV`], if set to a non-empty
/// (after trimming) value. Mirrors the trim/empty handling the engine and
/// CLI both relied on before this crate consolidated it.
pub fn audit_path_override() -> Option<PathBuf> {
    let raw = std::env::var_os(AUDIT_PATH_ENV)?;
    let trimmed = raw.to_string_lossy().trim().to_owned();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// Refuse `path` when this is a test process (see [`is_test_process`]) and
/// `path` is production-shaped for `filename` (see [`is_production_shaped`]).
///
/// Guards the [`AUDIT_PATH_ENV`] override path specifically: an override is a
/// deliberate operator choice in production, but a test process can inherit
/// one from its environment (an exported shell var, or a value carried over
/// from a production engine's environment) pointing straight at production's
/// `engine-audit.log` — the exact file the incident this guard exists for
/// corrupted. Unlike [`resolve_state_root`], the override is honoured for any
/// path that is *not* production-shaped, since a test's own private override
/// path is exactly the escape hatch tests are expected to use.
fn refuse_if_test_process_inherited_production_override(path: &Path, filename: &str) {
    if is_test_process() && is_production_shaped(path, filename) {
        panic!(
            "boss-log-files: refusing to write Boss's production audit log ({}) from a test process — \
             {AUDIT_PATH_ENV} was inherited pointing at a production-shaped path. This binary must run \
             under `bazel test` with its own isolated override, not inherit one from the ambient \
             environment.",
            path.display()
        );
    }
}

/// Resolve a [`LogSource`] to its primary live on-disk path under `state_root`.
///
/// - Trace: `<state_root>/engine-trace.jsonl`
/// - Audit: [`AUDIT_PATH_ENV`] override, else `<state_root>/engine-audit.log`
/// - Dispatch: `<state_root>/dispatch-events/current.jsonl`
/// - Spawn / population-timing: the diagnostics directory (day files live under it)
///
/// Prefer [`resolve_log_source_files`] when reading — it returns every segment
/// that participates in the logical stream (rotated + day-dated).
pub fn resolve_log_source_path(source: LogSource, state_root: &Path) -> PathBuf {
    match source {
        LogSource::Audit => match audit_path_override() {
            Some(path) => {
                refuse_if_test_process_inherited_production_override(&path, ENGINE_AUDIT_FILENAME);
                path
            }
            None => state_root.join(ENGINE_AUDIT_FILENAME),
        },
        LogSource::EngineTrace => state_root.join(ENGINE_TRACE_FILENAME),
        LogSource::Dispatch => state_root.join(DISPATCH_EVENTS_DIR).join(DISPATCH_EVENTS_LIVE_FILENAME),
        LogSource::Spawn | LogSource::PopulationTiming => state_root.join(DIAGNOSTICS_DIR),
    }
}

/// Every file that participates in the logical stream for `source`, in
/// chronological order (oldest first). Callers scan this list as one stream;
/// they never need to know about rotation or day-dating.
///
/// - Trace / audit / dispatch: rotated `<name>.<unix_s>` segments (oldest
///   first) then the live file.
/// - Spawn: day-dated files under `diagnostics/`, sorted by date in the name.
/// - Population-timing: both engine (`engine-population-timing-*`) and app
///   (`population-timing-*`) day files, merged and sorted by date.
pub fn resolve_log_source_files(source: LogSource, state_root: &Path) -> Vec<PathBuf> {
    match source {
        LogSource::EngineTrace | LogSource::Audit | LogSource::Dispatch => {
            let base = resolve_log_source_path(source, state_root);
            crate::segments::segments_with_live(&base)
        }
        LogSource::Spawn => day_rotated_files(&state_root.join(DIAGNOSTICS_DIR), SPAWN_DIAGNOSTICS_PREFIX),
        LogSource::PopulationTiming => merge_day_rotated_files(
            &state_root.join(DIAGNOSTICS_DIR),
            &[POPULATION_TIMING_PREFIX, APP_POPULATION_TIMING_PREFIX],
        ),
    }
}

/// Enumerate day-rotated files named `<prefix>YYYY-MM-DD.jsonl` under `dir`,
/// sorted ascending by the date suffix (filename order == chronological).
pub fn day_rotated_files(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let suffix = ".jsonl";
    let mut files: Vec<PathBuf> = read_dir
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    let Some(rest) = n.strip_prefix(prefix) else {
                        return false;
                    };
                    let Some(date) = rest.strip_suffix(suffix) else {
                        return false;
                    };
                    is_yyyy_mm_dd(date)
                })
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    files
}

/// Merge day-rotated files for multiple prefixes, sorted by the YYYY-MM-DD
/// embedded in each filename (then by full filename for same-day stability).
pub fn merge_day_rotated_files(dir: &Path, prefixes: &[&str]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for prefix in prefixes {
        files.extend(day_rotated_files(dir, prefix));
    }
    files.sort_by(|a, b| {
        day_file_date_key(a)
            .cmp(&day_file_date_key(b))
            .then_with(|| a.file_name().cmp(&b.file_name()))
    });
    files
}

fn is_yyyy_mm_dd(date: &str) -> bool {
    date.len() == 10
        && date.as_bytes().get(4) == Some(&b'-')
        && date.as_bytes().get(7) == Some(&b'-')
        && date.bytes().all(|b| b.is_ascii_digit() || b == b'-')
}

/// Extract the trailing `YYYY-MM-DD` from a day-rotated filename
/// (`<prefix>YYYY-MM-DD.jsonl`). Returns `None` when the shape does not match.
fn day_file_date_key(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".jsonl")?;
    if stem.len() < 10 {
        return None;
    }
    let date = &stem[stem.len() - 10..];
    if is_yyyy_mm_dd(date) { Some(date) } else { None }
}

/// Resolve the default audit-log path: [`AUDIT_PATH_ENV`] if set, otherwise
/// `<default_state_root>/engine-audit.log`. Returns `None` only when neither
/// the override nor `HOME` is available.
pub fn default_audit_log_path() -> Option<PathBuf> {
    if let Some(path) = audit_path_override() {
        refuse_if_test_process_inherited_production_override(&path, ENGINE_AUDIT_FILENAME);
        return Some(path);
    }
    Some(default_state_root()?.join(ENGINE_AUDIT_FILENAME))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, Once};

    /// Serializes tests that mutate the process-global `AUDIT_PATH_ENV`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Installs one deterministic isolated test root before any test in this
    /// binary observes the process-global `TEST_STATE_ROOT`, so
    /// `engine_runtime_files_resolve_under_state_root` and
    /// `install_test_state_root_is_idempotent` — which run concurrently under
    /// libtest and both touch that same global — cannot race for who wins as
    /// first writer. Every caller gets back the one root that actually won,
    /// regardless of call order.
    static INIT_TEST_ROOT: Once = Once::new();

    fn ensure_test_root_installed() -> PathBuf {
        INIT_TEST_ROOT.call_once(|| {
            install_test_state_root(PathBuf::from("/tmp/boss-log-files-test-state-root"));
        });
        TEST_STATE_ROOT
            .get()
            .cloned()
            .expect("installed by the call_once above")
    }

    #[test]
    fn log_source_filenames_match_constants() {
        assert_eq!(LogSource::EngineTrace.filename(), Some("engine-trace.jsonl"));
        assert_eq!(LogSource::Audit.filename(), Some("engine-audit.log"));
        assert_eq!(LogSource::Dispatch.filename(), Some("current.jsonl"));
        assert_eq!(LogSource::Spawn.filename(), None);
        assert_eq!(LogSource::PopulationTiming.filename(), None);
    }

    #[test]
    fn engine_trace_resolves_under_state_root() {
        let root = Path::new("/tmp/boss-state");
        assert_eq!(
            resolve_log_source_path(LogSource::EngineTrace, root),
            root.join("engine-trace.jsonl")
        );
    }

    #[test]
    fn engine_runtime_files_resolve_under_state_root() {
        // The isolated root is installed lazily by `default_state_root()`
        // when this executable is recognised as a test process.
        // `TEST_STATE_ROOT` is a first-writer-wins OnceLock that sibling
        // tests in this binary may populate, so this pins it via
        // `ensure_test_root_installed` rather than assuming a starting
        // state. Either way, the result must stay internally consistent
        // with the other `default_*_path` functions, which is all this
        // asserts.
        let root = ensure_test_root_installed();
        assert_eq!(default_state_root(), Some(root.clone()));
        assert_eq!(default_frontend_socket_path(), Some(root.join("engine.sock")));
        assert_eq!(default_engine_pid_path(), Some(root.join("engine.pid")));
        assert_eq!(default_engine_text_log_path(), Some(root.join("engine.log")));
    }

    #[test]
    fn audit_resolves_under_state_root_without_override() {
        let _guard = lock_env();
        unsafe {
            std::env::remove_var(AUDIT_PATH_ENV);
        }
        let root = Path::new("/tmp/boss-state");
        assert_eq!(
            resolve_log_source_path(LogSource::Audit, root),
            root.join("engine-audit.log")
        );
    }

    #[test]
    fn audit_override_wins_and_is_trimmed() {
        let _guard = lock_env();
        unsafe {
            std::env::set_var(AUDIT_PATH_ENV, "  /custom/audit.log  ");
        }
        let root = Path::new("/tmp/boss-state");
        assert_eq!(
            resolve_log_source_path(LogSource::Audit, root),
            PathBuf::from("/custom/audit.log")
        );
        assert_eq!(default_audit_log_path(), Some(PathBuf::from("/custom/audit.log")));
        unsafe {
            std::env::remove_var(AUDIT_PATH_ENV);
        }
    }

    #[test]
    fn empty_override_is_ignored() {
        let _guard = lock_env();
        unsafe {
            std::env::set_var(AUDIT_PATH_ENV, "   ");
        }
        assert_eq!(audit_path_override(), None);
        let root = Path::new("/tmp/boss-state");
        assert_eq!(
            resolve_log_source_path(LogSource::Audit, root),
            root.join("engine-audit.log")
        );
        unsafe {
            std::env::remove_var(AUDIT_PATH_ENV);
        }
    }

    #[test]
    fn audit_override_pointing_at_production_shape_is_refused_in_test_process() {
        let _guard = lock_env();
        ensure_test_root_installed();
        unsafe {
            std::env::set_var(
                AUDIT_PATH_ENV,
                "/Users/tester/Library/Application Support/Boss/engine-audit.log",
            );
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(default_audit_log_path));
        unsafe {
            std::env::remove_var(AUDIT_PATH_ENV);
        }
        let err = result.expect_err("expected a panic refusing the production-shaped override");
        let message = err
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        assert!(
            message.contains("refusing to write Boss's production audit log"),
            "unexpected panic message: {message}"
        );
    }

    #[test]
    fn audit_override_pointing_at_production_shape_is_refused_via_resolve_log_source_path() {
        let _guard = lock_env();
        ensure_test_root_installed();
        unsafe {
            std::env::set_var(
                AUDIT_PATH_ENV,
                "/Users/tester/Library/Application Support/Boss/engine-audit.log",
            );
        }
        let root = Path::new("/tmp/boss-state");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resolve_log_source_path(LogSource::Audit, root)
        }));
        unsafe {
            std::env::remove_var(AUDIT_PATH_ENV);
        }
        assert!(
            result.is_err(),
            "expected a panic refusing the production-shaped override"
        );
    }

    #[test]
    fn dispatch_resolves_under_dispatch_events() {
        let root = Path::new("/tmp/boss-state");
        assert_eq!(
            resolve_log_source_path(LogSource::Dispatch, root),
            root.join("dispatch-events/current.jsonl")
        );
    }

    #[test]
    fn day_rotated_files_orders_by_date_and_filters_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("spawn-2026-07-20.jsonl"), b"x").unwrap();
        std::fs::write(dir.path().join("spawn-2026-07-19.jsonl"), b"x").unwrap();
        std::fs::write(dir.path().join("spawn-2026-07-21.jsonl"), b"x").unwrap();
        // Noise: wrong prefix, wrong suffix, bad date shape.
        std::fs::write(dir.path().join("engine-population-timing-2026-07-20.jsonl"), b"x").unwrap();
        std::fs::write(dir.path().join("spawn-2026-07-20.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("spawn-not-a-date.jsonl"), b"x").unwrap();

        let files = day_rotated_files(dir.path(), SPAWN_DIAGNOSTICS_PREFIX);
        assert_eq!(files.len(), 3);
        assert!(files[0].to_string_lossy().ends_with("spawn-2026-07-19.jsonl"));
        assert!(files[1].to_string_lossy().ends_with("spawn-2026-07-20.jsonl"));
        assert!(files[2].to_string_lossy().ends_with("spawn-2026-07-21.jsonl"));
    }

    #[test]
    fn resolve_log_source_files_spawn_lists_day_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let diag = dir.path().join(DIAGNOSTICS_DIR);
        std::fs::create_dir_all(&diag).unwrap();
        std::fs::write(diag.join("spawn-2026-07-25.jsonl"), b"{}\n").unwrap();
        std::fs::write(diag.join("spawn-2026-07-26.jsonl"), b"{}\n").unwrap();
        let files = resolve_log_source_files(LogSource::Spawn, dir.path());
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn population_timing_merges_app_and_engine_day_files_by_date() {
        let dir = tempfile::TempDir::new().unwrap();
        let diag = dir.path().join(DIAGNOSTICS_DIR);
        std::fs::create_dir_all(&diag).unwrap();
        // Intentionally write out of order and interleave prefixes.
        std::fs::write(diag.join("population-timing-2026-07-26.jsonl"), b"app26\n").unwrap();
        std::fs::write(diag.join("engine-population-timing-2026-07-25.jsonl"), b"eng25\n").unwrap();
        std::fs::write(diag.join("population-timing-2026-07-25.jsonl"), b"app25\n").unwrap();
        std::fs::write(diag.join("engine-population-timing-2026-07-26.jsonl"), b"eng26\n").unwrap();
        // Noise: spawn diagnostics must not appear.
        std::fs::write(diag.join("spawn-2026-07-25.jsonl"), b"spawn\n").unwrap();

        let files = resolve_log_source_files(LogSource::PopulationTiming, dir.path());
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "engine-population-timing-2026-07-25.jsonl",
                "population-timing-2026-07-25.jsonl",
                "engine-population-timing-2026-07-26.jsonl",
                "population-timing-2026-07-26.jsonl",
            ]
        );
    }

    // -- resolve_state_root: the test-isolation refusal gate ---------------
    //
    // Exercised directly against the pure function rather than through
    // `default_state_root()` / `is_test_process()`: those read a
    // process-global `OnceLock` that sibling tests in this binary install
    // into, so driving the pure function is the only way to pin each branch
    // — including "test process with no installed root" — deterministically.

    #[test]
    fn production_process_resolves_under_home() {
        let home = Path::new("/Users/tester");
        assert_eq!(
            resolve_state_root(false, None, Some(home)),
            Some(home.join(STATE_ROOT_SUFFIX))
        );
    }

    #[test]
    fn production_process_with_no_home_resolves_to_none() {
        assert_eq!(resolve_state_root(false, None, None), None);
    }

    #[test]
    fn test_process_uses_the_installed_root_never_home() {
        let installed = Path::new("/tmp/boss-test-isolation-abc123");
        let home = Path::new("/Users/tester");
        assert_eq!(
            resolve_state_root(true, Some(installed), Some(home)),
            Some(installed.to_path_buf()),
            "a test process must resolve its own installed root even when a real $HOME is present"
        );
    }

    #[test]
    #[should_panic(expected = "refusing to resolve Boss's production state root")]
    fn test_process_with_no_installed_root_refuses() {
        resolve_state_root(true, None, Some(Path::new("/Users/tester")));
    }

    #[test]
    fn install_test_state_root_is_idempotent() {
        // The isolated root is installed lazily by `default_state_root()`
        // when this executable is recognised as a test process.
        // `TEST_STATE_ROOT` is a first-writer-wins OnceLock that sibling
        // tests in this binary may already have populated, so this pins
        // it via `ensure_test_root_installed` rather than assuming a
        // starting state, then asserts a later `install_test_state_root`
        // cannot replace the winner.

        let already_installed = ensure_test_root_installed();
        assert!(is_test_process());
        assert_eq!(TEST_STATE_ROOT.get(), Some(&already_installed));

        install_test_state_root(PathBuf::from("/tmp/boss-test-root-late-writer"));
        assert_eq!(
            TEST_STATE_ROOT.get(),
            Some(&already_installed),
            "an already-installed root always wins over a later install_test_state_root call"
        );
    }

    // -- is_bazel_test_executable -------------------------------------------

    #[test]
    fn bazel_run_production_binary_is_not_a_test_executable() {
        let exe = Path::new(
            "/private/var/tmp/_bazel/execroot/_main/bazel-out/darwin_arm64-fastbuild/bin/tools/boss/engine/core/engine",
        );
        assert!(
            !is_bazel_test_executable(exe, false),
            "`bazel run` of a production binary lives under bazel-out but must not isolate"
        );
    }

    #[test]
    fn bazel_bin_production_binary_is_not_a_test_executable() {
        let exe = Path::new("/Users/dev/mono/bazel-bin/tools/boss/engine/core/engine");
        assert!(!is_bazel_test_executable(exe, false));
    }

    #[test]
    fn bazel_test_harness_env_marks_bazel_out_executable_as_test() {
        let exe = Path::new(
            "/private/var/tmp/_bazel/execroot/_main/bazel-out/darwin_arm64-fastbuild/bin/tools/boss/engine/core/engine",
        );
        assert!(
            is_bazel_test_executable(exe, true),
            "TEST_TMPDIR / TEST_SRCDIR / BAZEL_TEST identify `bazel test` even when the stem is not `_test`"
        );
    }

    #[test]
    fn test_stem_marks_direct_bazel_bin_test_invocation() {
        let exe = Path::new("/Users/dev/mono/bazel-bin/tools/boss/engine/core/engine_lib_test");
        assert!(
            is_bazel_test_executable(exe, false),
            "direct bazel-bin invocation of a `_test` binary must isolate even without harness env"
        );
    }

    #[test]
    fn non_bazel_output_test_stem_is_not_isolated() {
        let exe = Path::new("/Applications/Boss.app/Contents/MacOS/engine_lib_test");
        assert!(
            !is_bazel_test_executable(exe, false),
            "the `_test` stem is only meaningful for Bazel-output binaries"
        );
    }

    // -- is_production_shaped -----------------------------------------------

    #[test]
    fn production_shaped_path_matches_filename_and_suffix() {
        assert!(is_production_shaped(
            Path::new("/Users/tester/Library/Application Support/Boss/tmux.sock"),
            TMUX_SOCKET_FILENAME
        ));
    }

    #[test]
    fn production_shaped_check_is_independent_of_whose_home() {
        assert!(is_production_shaped(
            Path::new("/Users/someone-else/Library/Application Support/Boss/tmux.sock"),
            TMUX_SOCKET_FILENAME
        ));
    }

    #[test]
    fn wrong_filename_is_not_production_shaped() {
        assert!(!is_production_shaped(
            Path::new("/Users/tester/Library/Application Support/Boss/other.sock"),
            TMUX_SOCKET_FILENAME
        ));
    }

    #[test]
    fn right_filename_wrong_parent_is_not_production_shaped() {
        assert!(!is_production_shaped(
            Path::new("/tmp/boss-test-abc123.tmux.sock"),
            TMUX_SOCKET_FILENAME
        ));
    }
}
