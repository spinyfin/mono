//! Test-fixture isolation guard: keep an engine started with a non-default
//! `--socket-path` off every piece of state the production engine owns.
//!
//! ## Why this is not just "derive paths from the socket stem"
//!
//! The original guard (PR #756, 2026-05-24) derived isolated paths but stood
//! down on any field whose env override was already set, on the theory that
//! an explicit `BOSS_DB_PATH=…` is operator intent that should win.
//!
//! That theory has a hole. Every Boss **worker pane** is spawned with
//! `BOSS_EVENTS_SOCKET` pointing at the production socket (`spawn_flow.rs`),
//! so a fixture engine launched from inside a worker inherits it. The guard
//! saw an override, concluded "the operator chose this", and stood down — and
//! the fixture then unlinked and rebound production's events socket. On
//! 2026-07-23 that made the live engine deaf for ~50 minutes; the resulting
//! hook outage caused `stale_worker_sweep` to reap 6 live workers as falsely
//! stale. The same stand-down applied to the DB and pid file; they survived
//! only because that worker happened to set `BOSS_DB_PATH` itself.
//!
//! The fix is the equality test in [`IsolationPaths::derive_from`]: **an
//! override whose value is the production default is not an override.**
//! Inherited pane environment is indistinguishable from operator intent by
//! presence alone, but perfectly distinguishable by value — nobody types out
//! the production path in order to ask for isolation. A developer who points
//! `BOSS_EVENTS_SOCKET` at a private path still wins, which is why neither
//! "always derive" nor "always refuse" would do.
//!
//! [`IsolationPaths::ensure_isolated`] is the second layer: a hard pre-bind
//! check that refuses to start when any *resolved* path still lands on
//! production, whatever route it arrived by (a symlink, a `..`, a path this
//! module didn't think to derive). Layer one keeps the common case working;
//! layer two makes the failure loud instead of silent.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};

use crate::config::{DB_PATH_ENV, EVENTS_SOCKET_ENV, PID_PATH_ENV};
use crate::engine_control::TOKEN_PATH_ENV;

/// The frontend socket a production engine binds. Any other `--socket-path`
/// marks the process as a test fixture — see [`is_test_fixture_socket`].
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/boss-engine.sock";

/// The pid file a production engine writes.
pub const DEFAULT_PID_PATH: &str = "/tmp/boss-engine.pid";

/// Bare filename of [`DEFAULT_PID_PATH`], for the value-shape check in
/// [`IsolationPaths::derive_from`].
const DEFAULT_PID_FILENAME: &str = "boss-engine.pid";

/// Directory production's state-root files (db, events socket, control
/// token) live under, relative to `$HOME` — shared by [`is_production_shaped`]
/// so it can recognize production's shape without depending on *this*
/// process's `$HOME`.
const STATE_ROOT_SUFFIX: &str = "Library/Application Support/Boss";

/// The four pieces of engine-owned state a test fixture could collide with.
///
/// One shape serves three roles: where production keeps each file
/// ([`EnginePaths::production`]), what a fixture derived for itself
/// ([`IsolationPaths::derived`]), and what a given engine start actually
/// resolved (the argument to [`IsolationPaths::ensure_isolated`]). Comparing
/// two of them field-by-field is the whole guard.
///
/// A `None` field means "not applicable here": no production location because
/// `HOME` is unset, no derived path because a deliberate override won, or
/// nothing of that kind being opened on this run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnginePaths {
    pub db: Option<PathBuf>,
    pub events_socket: Option<PathBuf>,
    pub pid: Option<PathBuf>,
    pub control_token: Option<PathBuf>,
}

impl EnginePaths {
    /// Production's locations, resolved from the current process environment
    /// (really just `$HOME`, plus the hard-coded `/tmp` pid default). These
    /// are what the engine resolves with **no** overrides in play.
    ///
    /// **This models production's *default* location, not its actual one.**
    /// If the live production engine itself runs with `BOSS_EVENTS_SOCKET`
    /// (or `BOSS_DB_PATH`/`BOSS_ENGINE_CONTROL_TOKEN_PATH`) pointed somewhere
    /// non-default — a documented, supported way to relocate engine state —
    /// this struct still reports the default path, not the one the live
    /// engine actually bound. A fixture that inherits that same relocated
    /// override is invisible to both [`IsolationPaths::derive_from`]'s
    /// equality rule and [`IsolationPaths::ensure_isolated`]'s gate: both
    /// compare against this (wrong) model and report no collision. This is a
    /// known limitation of the guard, not something either layer can close —
    /// closing it would require the fixture to ask the *actual* running
    /// engine where its state lives, which the guard has no channel for.
    pub fn production() -> Self {
        Self {
            db: boss_log_files::default_state_db_path(),
            events_socket: boss_log_files::default_events_socket_path(),
            pid: Some(PathBuf::from(DEFAULT_PID_PATH)),
            control_token: boss_log_files::default_control_token_path(),
        }
    }

    /// Production's locations if the state root were `root`. Tests use this
    /// to model production without depending on the real `$HOME`.
    pub fn under_state_root(root: &Path, pid: &Path) -> Self {
        Self {
            db: Some(root.join(boss_log_files::STATE_DB_FILENAME)),
            events_socket: Some(root.join(boss_log_files::EVENTS_SOCKET_FILENAME)),
            pid: Some(pid.to_path_buf()),
            control_token: Some(root.join(boss_log_files::CONTROL_TOKEN_FILENAME)),
        }
    }

    /// The four fields, paired with a label and the env var an operator would
    /// change to move that file. Drives both the derivation and the gate, so
    /// neither can quietly forget a field.
    fn fields(&self) -> [(&'static str, &'static str, Option<&Path>); 4] {
        [
            ("state database", DB_PATH_ENV, self.db.as_deref()),
            ("events socket", EVENTS_SOCKET_ENV, self.events_socket.as_deref()),
            ("pid file", PID_PATH_ENV, self.pid.as_deref()),
            ("engine-control token", TOKEN_PATH_ENV, self.control_token.as_deref()),
        ]
    }
}

/// The env overrides [`IsolationPaths::derive_from`] consults, captured as
/// plain data so the derivation is a pure function — the unit tests drive it
/// without mutating process-global environment state (which is what let the
/// original guard ship with zero coverage of its own derivation).
#[derive(Debug, Clone, Default)]
pub struct IsolationOverrides {
    pub db_path: Option<PathBuf>,
    pub events_socket: Option<PathBuf>,
    pub pid_path: Option<PathBuf>,
    pub control_token_path: Option<PathBuf>,
}

impl IsolationOverrides {
    pub fn from_process() -> Self {
        Self {
            db_path: std::env::var_os(DB_PATH_ENV).map(PathBuf::from),
            events_socket: std::env::var_os(EVENTS_SOCKET_ENV).map(PathBuf::from),
            pid_path: std::env::var_os(PID_PATH_ENV).map(PathBuf::from),
            control_token_path: std::env::var_os(TOKEN_PATH_ENV).map(PathBuf::from),
        }
    }
}

/// Paths derived from a non-default `--socket-path` so a test-fixture engine
/// never touches production state.
///
/// When `socket_path` resolves to [`DEFAULT_SOCKET_PATH`] this is the
/// production engine: `is_test_fixture` is false, every derived field is
/// `None`, and the caller resolves paths through its normal env / home-dir
/// logic.
///
/// Otherwise each derived field is `Some(path)` **unless** the corresponding
/// env override names somewhere that is not production — see the module doc.
#[derive(Debug, Clone)]
pub struct IsolationPaths {
    /// True when the engine is operating as a test fixture (non-default socket).
    pub is_test_fixture: bool,
    /// The isolated paths derived from the socket stem. A field is `None`
    /// when a deliberate env override won for it, in which case the caller
    /// keeps whatever that override named.
    ///
    /// `control_token` is here because it used to be resolved outside the
    /// guard entirely: a fixture overwrote production's token file and
    /// `ControlTokenGuard::drop` deleted it on shutdown, leaving every
    /// engine-control path broken until the next production restart.
    pub derived: EnginePaths,
    /// Directory the derived paths live in; also the fixture's state root for
    /// trace / audit logs.
    pub state_root: Option<PathBuf>,
    /// Stem shared by every derived filename (`boss-test-UUID`).
    stem: String,
    /// Production's locations, retained so [`Self::ensure_isolated`] can run
    /// the pre-bind collision check without re-reading the environment.
    production: EnginePaths,
}

impl IsolationPaths {
    /// Derive isolation paths from `socket_path`, reading overrides and
    /// production defaults from the process environment.
    pub fn derive(socket_path: &str) -> Self {
        Self::derive_from(
            socket_path,
            &IsolationOverrides::from_process(),
            &EnginePaths::production(),
        )
    }

    /// Pure derivation. See the module doc for the override-equality rule.
    ///
    /// Non-default socket → derive paths from the socket's directory and
    /// file-stem (e.g. `/tmp/boss-test-UUID.sock` → `/tmp/boss-test-UUID.db`,
    /// `/tmp/boss-test-UUID.events.sock`, `/tmp/boss-test-UUID.pid`,
    /// `/tmp/boss-test-UUID.control-token`).
    pub fn derive_from(socket_path: &str, overrides: &IsolationOverrides, production: &EnginePaths) -> Self {
        if !is_test_fixture_socket(socket_path) {
            return Self {
                is_test_fixture: false,
                derived: EnginePaths::default(),
                state_root: None,
                stem: String::new(),
                production: production.clone(),
            };
        }

        let path = Path::new(socket_path);
        let dir = path.parent().unwrap_or(Path::new("/tmp")).to_path_buf();
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "boss-test".to_owned());

        // Honour explicit env overrides — but only when the override actually
        // points somewhere private. An override whose value IS the production
        // default is inherited pane environment, not intent, so we derive over
        // it. Applied identically to all four fields: the 2026-07-23 incident
        // hit the events socket only because that worker happened to set
        // `BOSS_DB_PATH` itself; the stand-down logic was the same for each.
        //
        // Two independent tests decide "is this really production", because
        // either alone has a blind spot:
        //   - `same_path` against `production` (this process's own $HOME):
        //     catches the common case, but `production`'s field is `None`
        //     when `HOME` is unset, and `same_path(_, None)` is unconditionally
        //     false — so with no `HOME` this test alone would treat every
        //     inherited production path as a deliberate override.
        //   - `is_production_shaped`: a *structural* check (right filename,
        //     parent ends `Library/Application Support/Boss`) that doesn't
        //     depend on this process's own `$HOME` at all. It catches an
        //     override inherited from a production engine running under a
        //     *different* `$HOME` than this process (a wrapper, a launchd
        //     job, a `rust_test` with `HOME` pinned to `/tmp`) — a case the
        //     `same_path` test alone stands down on, because it compares
        //     against this process's own (different) production model.
        let derive_field =
            |override_value: &Option<PathBuf>, production: &Option<PathBuf>, filename: &str, suffix: &str| {
                let stands_down = match override_value {
                    None => false,
                    Some(value) => !same_path(value, production.as_deref()) && !is_production_shaped(value, filename),
                };
                (!stands_down).then(|| dir.join(format!("{stem}.{suffix}")))
            };

        Self {
            is_test_fixture: true,
            derived: EnginePaths {
                db: derive_field(
                    &overrides.db_path,
                    &production.db,
                    boss_log_files::STATE_DB_FILENAME,
                    "db",
                ),
                events_socket: derive_field(
                    &overrides.events_socket,
                    &production.events_socket,
                    boss_log_files::EVENTS_SOCKET_FILENAME,
                    "events.sock",
                ),
                pid: derive_field(&overrides.pid_path, &production.pid, DEFAULT_PID_FILENAME, "pid"),
                control_token: derive_field(
                    &overrides.control_token_path,
                    &production.control_token,
                    boss_log_files::CONTROL_TOKEN_FILENAME,
                    "control-token",
                ),
            },
            state_root: Some(dir),
            stem,
            production: production.clone(),
        }
    }

    /// A fixture-scoped filename under the fixture's state root, for state
    /// this struct does not own outright (trace log, audit log, text log).
    /// `None` for a production engine.
    pub fn scoped_path(&self, suffix: &str) -> Option<PathBuf> {
        let root = self.state_root.as_ref()?;
        Some(root.join(format!("{}.{suffix}", self.stem)))
    }

    /// Apply the same override-equality rule as [`Self::derive_from`] to a
    /// file this struct does not own outright — the trace, audit, and text
    /// logs, which each resolve through their own env var and are not part of
    /// [`EnginePaths`].
    ///
    /// - Not a fixture → `resolved` unchanged. Production owns its files.
    /// - Fixture, and `resolved` is production's location → the stem-scoped
    ///   name, so the fixture cannot write into production's log.
    /// - Fixture, and `resolved` is somewhere else → `resolved` unchanged: the
    ///   caller deliberately chose that path, exactly as with
    ///   `BOSS_EVENTS_SOCKET`.
    pub fn scope_if_production(
        &self,
        resolved: Option<PathBuf>,
        production: Option<&Path>,
        suffix: &str,
    ) -> Option<PathBuf> {
        if !self.is_test_fixture {
            return resolved;
        }
        match &resolved {
            Some(path) if !same_path(path, production) => resolved,
            _ => self.scoped_path(suffix).or(resolved),
        }
    }

    /// Hard pre-bind gate: refuse to start a fixture whose *resolved* paths
    /// still land on production state.
    ///
    /// The derivation above is the policy that makes the common case work;
    /// this is the assertion that it did. It runs on fully-resolved paths, so
    /// it also catches routes the derivation never sees — an override that
    /// reaches production via a symlink or `..`, or a fallback added later
    /// that forgets to consult this struct. A production engine passes
    /// trivially: production is *supposed* to use production's paths.
    pub fn ensure_isolated(&self, resolved: &EnginePaths) -> Result<()> {
        if !self.is_test_fixture {
            return Ok(());
        }

        // A gate that cannot compute what it is guarding must not report
        // success. `same_path(_, None)` is unconditionally false, so if any
        // of production's fields are unresolved (only possible with `HOME`
        // unset — the pid field always has a value), the collision check
        // below would silently find zero collisions for that field no matter
        // what `resolved` actually names. Refuse to start instead.
        let unknown_production: Vec<&str> = self
            .production
            .fields()
            .into_iter()
            .filter(|(_, _, path)| path.is_none())
            .map(|(what, _, _)| what)
            .collect();
        if !unknown_production.is_empty() {
            bail!(
                "test-fixture engine refused to start: could not resolve production's {} (is $HOME set?), \
                 so the isolation gate cannot verify this fixture does not collide with it. Set HOME and retry.",
                unknown_production.join(", ")
            );
        }

        let collisions: Vec<String> = resolved
            .fields()
            .into_iter()
            .zip(self.production.fields())
            .filter_map(|((what, env, actual), (_, _, production))| {
                let actual = actual?;
                same_path(actual, production)
                    .then(|| format!("{what} ({env}) resolved to the production path {}", actual.display()))
            })
            .collect();

        if collisions.is_empty() {
            return Ok(());
        }

        bail!(
            "test-fixture engine refused to start: {}. A fixture (non-default --socket-path) must not \
             share state with the production engine — binding these would unlink production's socket \
             and overwrite its token. Unset the named environment variable, or point it at a private \
             path, and retry.",
            collisions.join("; ")
        )
    }
}

/// Is `socket_path` a test-fixture socket — i.e. anything other than the
/// production `/tmp/boss-engine.sock`?
///
/// Compared after normalization rather than by raw string equality: on macOS
/// `/tmp` is a symlink to `/private/tmp`, so a launcher that resolved the
/// path before passing it would otherwise be misclassified as a fixture (and,
/// with the refusal gate below, would fail to start). `/tmp/./boss-engine.sock`
/// is the same class of near-miss.
fn is_test_fixture_socket(socket_path: &str) -> bool {
    !same_path(Path::new(socket_path), Some(Path::new(DEFAULT_SOCKET_PATH)))
}

/// Do these two paths name the same file?
///
/// Lexical normalization first (collapse `.`, resolve `..`, drop repeated
/// separators), then — because the interesting paths are sockets and token
/// files that do not exist yet, so `canonicalize` on the path itself would
/// fail exactly when it matters — canonicalize the *parent directory* when it
/// exists and re-attach the filename. That resolves the `/tmp` →
/// `/private/tmp` symlink and any other symlinked ancestor.
fn same_path(a: &Path, b: Option<&Path>) -> bool {
    let Some(b) = b else { return false };
    resolve_for_compare(a) == resolve_for_compare(b)
}

/// Does `path` have production's *shape* for `filename` — i.e. is it named
/// exactly `filename` and does its parent end with production's state-root
/// suffix (`Library/Application Support/Boss`)?
///
/// This is deliberately independent of *whose* `$HOME` produced `path`: it
/// exists to catch an override inherited from a production engine running
/// under a different `$HOME` than this process (or with `HOME` unset here
/// entirely), which [`same_path`] cannot — `same_path` only knows *this*
/// process's own production model. See [`EnginePaths::production`]'s doc for
/// what this still cannot catch (production relocated by env var to a path
/// that doesn't have this shape at all).
fn is_production_shaped(path: &Path, filename: &str) -> bool {
    if path.file_name().and_then(|n| n.to_str()) != Some(filename) {
        return false;
    }
    path.parent().is_some_and(|parent| parent.ends_with(STATE_ROOT_SUFFIX))
}

fn resolve_for_compare(path: &Path) -> PathBuf {
    let lexical = lexically_normalize(path);
    let (Some(parent), Some(name)) = (lexical.parent(), lexical.file_name()) else {
        return lexical;
    };
    match parent.canonicalize() {
        Ok(real_parent) => real_parent.join(name),
        Err(_) => lexical,
    }
}

/// Collapse `.` components and resolve `..` against the preceding component.
/// Deliberately lexical — it must work on paths that do not exist.
fn lexically_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                // Pop a real directory name…
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // …swallow it at the root (`/..` is `/`, per POSIX)…
                Some(Component::RootDir | Component::Prefix(_)) => {}
                // …and keep a leading `..` in a relative path, which has
                // nothing to resolve against.
                _ => out.push(component.as_os_str()),
            },
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_SOCKET: &str = "/tmp/boss-test-abc123.sock";

    /// A model of production that does not depend on the real `$HOME`.
    fn production() -> EnginePaths {
        EnginePaths::under_state_root(
            Path::new("/Users/tester/Library/Application Support/Boss"),
            Path::new(DEFAULT_PID_PATH),
        )
    }

    fn derive(overrides: IsolationOverrides) -> IsolationPaths {
        IsolationPaths::derive_from(FIXTURE_SOCKET, &overrides, &production())
    }

    /// What the fixture above derives when nothing stands in its way.
    fn all_derived() -> EnginePaths {
        EnginePaths {
            db: Some(PathBuf::from("/tmp/boss-test-abc123.db")),
            events_socket: Some(PathBuf::from("/tmp/boss-test-abc123.events.sock")),
            pid: Some(PathBuf::from("/tmp/boss-test-abc123.pid")),
            control_token: Some(PathBuf::from("/tmp/boss-test-abc123.control-token")),
        }
    }

    // -- fixture classification ------------------------------------------

    #[test]
    fn default_socket_is_not_a_fixture() {
        let paths = IsolationPaths::derive_from(DEFAULT_SOCKET_PATH, &IsolationOverrides::default(), &production());
        assert!(!paths.is_test_fixture);
        assert_eq!(paths.derived, EnginePaths::default());
    }

    /// A cosmetically different spelling of the production socket must not be
    /// misclassified as a fixture — with the refusal gate armed, that would
    /// stop the production engine from starting at all.
    #[test]
    fn cosmetically_different_production_socket_is_not_a_fixture() {
        for spelling in [
            "/tmp/./boss-engine.sock",
            "/tmp/sub/../boss-engine.sock",
            "//tmp/boss-engine.sock",
        ] {
            let paths = IsolationPaths::derive_from(spelling, &IsolationOverrides::default(), &production());
            assert!(!paths.is_test_fixture, "{spelling} must classify as production");
        }
    }

    #[test]
    fn non_default_socket_is_a_fixture() {
        assert!(derive(IsolationOverrides::default()).is_test_fixture);
    }

    // -- derivation with no overrides set --------------------------------

    #[test]
    fn unset_env_derives_every_path_from_the_socket_stem() {
        assert_eq!(derive(IsolationOverrides::default()).derived, all_derived());
    }

    // -- a deliberate override wins --------------------------------------

    #[test]
    fn private_override_suppresses_derivation_for_that_field_only() {
        let paths = derive(IsolationOverrides {
            events_socket: Some(PathBuf::from("/tmp/my-private-events.sock")),
            ..IsolationOverrides::default()
        });
        assert_eq!(
            paths.derived,
            EnginePaths {
                events_socket: None,
                ..all_derived()
            },
            "the developer's explicit socket is left alone; every other field still derives"
        );
    }

    #[test]
    fn private_override_wins_for_each_field_independently() {
        let private = PathBuf::from("/tmp/private-thing");
        let paths = derive(IsolationOverrides {
            db_path: Some(private.clone()),
            events_socket: Some(private.clone()),
            pid_path: Some(private.clone()),
            control_token_path: Some(private),
        });
        assert_eq!(paths.derived, EnginePaths::default());
    }

    // -- the regression: an override that IS production ------------------

    /// The 2026-07-23 incident. A fixture started from inside a worker pane
    /// inherits `BOSS_EVENTS_SOCKET=<production socket>`; the guard must
    /// derive over it rather than standing down.
    #[test]
    fn inherited_production_events_socket_is_not_treated_as_an_override() {
        let paths = derive(IsolationOverrides {
            events_socket: production().events_socket,
            ..IsolationOverrides::default()
        });
        assert_eq!(
            paths.derived.events_socket.as_deref(),
            Some(Path::new("/tmp/boss-test-abc123.events.sock")),
            "an override equal to the production default is inherited env, not intent"
        );
    }

    #[test]
    fn inherited_production_defaults_are_overridden_on_every_field() {
        let prod = production();
        let paths = derive(IsolationOverrides {
            db_path: prod.db,
            events_socket: prod.events_socket,
            pid_path: prod.pid,
            control_token_path: prod.control_token,
        });
        assert_eq!(paths.derived, all_derived());
    }

    /// Same rule, reached by a non-literal spelling of production's path.
    #[test]
    fn production_path_reached_via_dot_segments_is_still_not_an_override() {
        let noisy = PathBuf::from("/Users/tester/Library/Application Support/Boss/./sub/../events.sock");
        assert_eq!(
            production().events_socket.as_deref(),
            Some(lexically_normalize(&noisy).as_path())
        );
        let paths = derive(IsolationOverrides {
            events_socket: Some(noisy),
            ..IsolationOverrides::default()
        });
        assert_eq!(
            paths.derived.events_socket.as_deref(),
            Some(Path::new("/tmp/boss-test-abc123.events.sock"))
        );
    }

    /// An override that has production's *shape* (right filename, parent
    /// ends `Library/Application Support/Boss`) must be treated as inherited
    /// environment even when it names a *different* `$HOME` than this
    /// process's own production model — e.g. a fixture launched under a
    /// production engine running with `HOME=/Users/other`, or a bazel test
    /// that pins `HOME=/tmp` while inheriting `BOSS_EVENTS_SOCKET` from a
    /// real `/Users/tester/...` production engine.
    #[test]
    fn production_shaped_override_is_not_treated_as_an_override_even_under_a_different_home() {
        let differently_homed_production =
            PathBuf::from("/Users/someone-else/Library/Application Support/Boss/events.sock");
        let paths = derive(IsolationOverrides {
            events_socket: Some(differently_homed_production),
            ..IsolationOverrides::default()
        });
        assert_eq!(
            paths.derived.events_socket.as_deref(),
            Some(Path::new("/tmp/boss-test-abc123.events.sock")),
            "an override with production's shape is inherited env, not intent, regardless of whose $HOME it names"
        );
    }

    /// The same shape check must stand in when this process cannot resolve
    /// its own production model at all (`HOME` unset): `same_path` alone is
    /// unconditionally false against `None`, so without the shape check every
    /// inherited production override would be (wrongly) treated as intent.
    #[test]
    fn production_shaped_override_is_not_treated_as_an_override_when_home_is_unset() {
        let paths = IsolationPaths::derive_from(
            FIXTURE_SOCKET,
            &IsolationOverrides {
                db_path: Some(PathBuf::from(
                    "/Users/someone/Library/Application Support/Boss/state.db",
                )),
                ..IsolationOverrides::default()
            },
            &EnginePaths::default(), // models HOME unset: every field None
        );
        assert_eq!(paths.derived.db.as_deref(), Some(Path::new("/tmp/boss-test-abc123.db")),);
    }

    /// The gate must fail closed, not open, when it cannot resolve
    /// production's own paths — it cannot prove there is no collision.
    #[test]
    fn gate_refuses_to_start_when_production_cannot_be_resolved() {
        let paths =
            IsolationPaths::derive_from(FIXTURE_SOCKET, &IsolationOverrides::default(), &EnginePaths::default());
        let err = paths
            .ensure_isolated(&all_derived())
            .expect_err("must refuse when production's own state can't be resolved");
        assert!(format!("{err}").contains("could not resolve production's"));
    }

    // -- the refusal gate -------------------------------------------------

    #[test]
    fn isolated_fixture_passes_the_gate() {
        derive(IsolationOverrides::default())
            .ensure_isolated(&all_derived())
            .expect("fully isolated");
    }

    #[test]
    fn fixture_refuses_to_start_when_a_resolved_path_is_production() {
        let prod = production();
        let paths = derive(IsolationOverrides::default());

        let cases: Vec<(&str, EnginePaths)> = vec![
            (
                EVENTS_SOCKET_ENV,
                EnginePaths {
                    events_socket: prod.events_socket.clone(),
                    ..all_derived()
                },
            ),
            (
                DB_PATH_ENV,
                EnginePaths {
                    db: prod.db.clone(),
                    ..all_derived()
                },
            ),
            (
                PID_PATH_ENV,
                EnginePaths {
                    pid: prod.pid.clone(),
                    ..all_derived()
                },
            ),
            (
                TOKEN_PATH_ENV,
                EnginePaths {
                    control_token: prod.control_token.clone(),
                    ..all_derived()
                },
            ),
        ];

        for (env, resolved) in cases {
            let err = paths
                .ensure_isolated(&resolved)
                .expect_err("collision with production must refuse the start");
            let msg = format!("{err}");
            assert!(
                msg.contains(env),
                "error must name the offending env var {env}; got: {msg}"
            );
            assert!(msg.contains("refused to start"), "error must be a refusal; got: {msg}");
        }
    }

    /// The gate reports every colliding field at once, so one failed start
    /// shows the operator the whole problem.
    #[test]
    fn gate_reports_all_collisions_together() {
        let paths = derive(IsolationOverrides::default());
        let err = paths.ensure_isolated(&production()).expect_err("all four collide");
        let msg = format!("{err}");
        for env in [DB_PATH_ENV, EVENTS_SOCKET_ENV, PID_PATH_ENV, TOKEN_PATH_ENV] {
            assert!(msg.contains(env), "error must name {env}; got: {msg}");
        }
    }

    /// The production engine's paths are production's paths — the gate must
    /// never fire for it.
    #[test]
    fn production_engine_is_never_refused() {
        let prod = production();
        IsolationPaths::derive_from(DEFAULT_SOCKET_PATH, &IsolationOverrides::default(), &prod)
            .ensure_isolated(&prod)
            .expect("production owns production");
    }

    // -- scoped_path ------------------------------------------------------

    /// Logs follow the same rule as sockets: production's location gets
    /// redirected, a deliberately-chosen one is left alone, and a production
    /// engine is never touched.
    #[test]
    fn scope_if_production_matches_the_derivation_rule() {
        let prod_audit = PathBuf::from("/Users/tester/Library/Application Support/Boss/engine-audit.log");
        let fixture = derive(IsolationOverrides::default());

        assert_eq!(
            fixture.scope_if_production(Some(prod_audit.clone()), Some(&prod_audit), "engine-audit.log"),
            Some(PathBuf::from("/tmp/boss-test-abc123.engine-audit.log")),
            "a fixture must not append to production's audit log"
        );
        assert_eq!(
            fixture.scope_if_production(
                Some(PathBuf::from("/tmp/my-audit.log")),
                Some(&prod_audit),
                "engine-audit.log"
            ),
            Some(PathBuf::from("/tmp/my-audit.log")),
            "an explicitly chosen audit path still wins"
        );

        let production =
            IsolationPaths::derive_from(DEFAULT_SOCKET_PATH, &IsolationOverrides::default(), &production());
        assert_eq!(
            production.scope_if_production(Some(prod_audit.clone()), Some(&prod_audit), "engine-audit.log"),
            Some(prod_audit),
            "the production engine keeps production's log"
        );
    }

    #[test]
    fn scoped_path_uses_the_socket_stem_and_dir() {
        assert_eq!(
            derive(IsolationOverrides::default())
                .scoped_path("engine-trace.jsonl")
                .as_deref(),
            Some(Path::new("/tmp/boss-test-abc123.engine-trace.jsonl"))
        );
        let prod = IsolationPaths::derive_from(DEFAULT_SOCKET_PATH, &IsolationOverrides::default(), &production());
        assert_eq!(prod.scoped_path("engine-trace.jsonl"), None);
    }

    // -- path normalization ----------------------------------------------

    #[test]
    fn lexical_normalization_collapses_dot_segments() {
        assert_eq!(
            lexically_normalize(Path::new("/tmp/./a.sock")),
            PathBuf::from("/tmp/a.sock")
        );
        assert_eq!(
            lexically_normalize(Path::new("/tmp/b/../a.sock")),
            PathBuf::from("/tmp/a.sock")
        );
        assert_eq!(lexically_normalize(Path::new("/../a.sock")), PathBuf::from("/a.sock"));
        assert_eq!(lexically_normalize(Path::new("../a.sock")), PathBuf::from("../a.sock"));
    }

    #[test]
    fn same_path_is_false_against_a_missing_production_default() {
        assert!(!same_path(Path::new("/tmp/a.sock"), None));
    }
}
