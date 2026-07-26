//! Log-source -> on-disk path resolution, including the audit-path env
//! override and the default Boss state root.

use std::path::{Path, PathBuf};

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
    /// (trace + audit). Day-rotated and single-file sources return false.
    pub fn uses_timestamp_rotation(self) -> bool {
        matches!(self, LogSource::EngineTrace | LogSource::Audit)
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

/// The default Boss state root: `$HOME/Library/Application Support/Boss`.
/// Returns `None` when `HOME` is unset.
pub fn default_state_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Boss"))
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
        LogSource::Audit => audit_path_override().unwrap_or_else(|| state_root.join(ENGINE_AUDIT_FILENAME)),
        LogSource::EngineTrace => state_root.join(ENGINE_TRACE_FILENAME),
        LogSource::Dispatch => state_root.join(DISPATCH_EVENTS_DIR).join(DISPATCH_EVENTS_LIVE_FILENAME),
        LogSource::Spawn | LogSource::PopulationTiming => state_root.join(DIAGNOSTICS_DIR),
    }
}

/// Every file that participates in the logical stream for `source`, in
/// chronological order (oldest first). Callers scan this list as one stream;
/// they never need to know about rotation or day-dating.
///
/// - Trace / audit: rotated segments (oldest first) then the live file.
/// - Dispatch: the single `current.jsonl` (and any `<name>.<unix_s>` siblings
///   if present, for forward-compat with future rotation).
/// - Spawn: day-dated files under `diagnostics/`, sorted by date in the name.
/// - Population-timing: both engine (`engine-population-timing-*`) and app
///   (`population-timing-*`) day files, merged and sorted by date.
pub fn resolve_log_source_files(source: LogSource, state_root: &Path) -> Vec<PathBuf> {
    match source {
        LogSource::EngineTrace | LogSource::Audit => {
            let base = resolve_log_source_path(source, state_root);
            crate::segments::segments_with_live(&base)
        }
        LogSource::Dispatch => {
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
        return Some(path);
    }
    Some(default_state_root()?.join(ENGINE_AUDIT_FILENAME))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that mutate the process-global `AUDIT_PATH_ENV`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
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
}
