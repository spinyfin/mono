//! Startup refusal gate: an agent-owned engine may not take over the
//! user's production state.
//!
//! The engine has two startup shapes. In **production** mode it owns the
//! real state under `~/Library/Application Support/Boss` — the events
//! socket workers deliver hooks to, `state.db`, the pid file, and the
//! engine-control token — plus the frontend socket at
//! [`DEFAULT_SOCKET_PATH`]. Exactly one such process may exist. In
//! **test-fixture** mode (`--socket-path` names anything else) every
//! runtime path is derived from that socket's directory and stem
//! (`crate::app::IsolationPaths`), so the process cannot reach
//! production state. Fixtures are the supported way to exercise a real
//! engine.
//!
//! An agent session (a worker, reviewer, or coordinator pane) that
//! starts an engine which resolves *production* paths silently
//! dispossesses the engine that is already running: hook deliveries stop
//! arriving, live workers are reaped as falsely stale, and the impostor's
//! `ControlTokenGuard::drop` removes the real control token when it
//! exits.
//!
//! ## Why this is not a command matcher
//!
//! A `PreToolUse` hook that pattern-matches the agent's shell command
//! already exists (`boss_engine_driver::claude`), and it is not
//! sufficient on its own — it can only recognise spellings. Two observed
//! evasions, neither of them deliberate:
//!
//! - The engine was run from `./bazel-bin/tools/boss/engine/core/engine`,
//!   a path with no recognisable Boss-bundle shape in it.
//! - The bundle path was assigned to a shell variable on one line and
//!   `open "$APP"` run on the next, so no single line contained both the
//!   launcher and the target.
//!
//! In the second case the process that seized the socket was not even the
//! one the agent ran: the app it launched terminated the running engine
//! and started its own as a child. Any control that inspects command
//! text is reasoning about the wrong object.
//!
//! This gate instead asks two questions of the process itself, at
//! startup, before anything is opened, bound, or written:
//!
//! 1. **Would this engine touch production state?** — the frontend
//!    socket is the production default, or a resolved runtime path lands
//!    inside the production state root.
//! 2. **Is this engine agent-owned?** — the environment carries a Boss
//!    session marker, or the binary itself lives in an agent-owned
//!    directory (a cube workspace, or a per-session agent scratchpad).
//!    The second clause survives `open`, which launches through
//!    LaunchServices and drops the caller's environment entirely.
//!
//! Both must hold. An agent running an isolated fixture is unaffected,
//! and the user's own engine — installed, launched by the app — never
//! matches clause 2 however it is invoked.

use std::path::{Path, PathBuf};

/// Environment keys that mark a process as running inside a Boss agent
/// session. `crate::spawn_flow` sets all three on every spawned pane and
/// they are inherited by everything the session executes directly.
///
/// Any one being present and non-empty is sufficient: they are set
/// together, so requiring all three would let a partially-sanitised
/// environment through.
pub(crate) const AGENT_SESSION_ENV_KEYS: &[&str] = &["BOSS_RUN_ID", "BOSS_LEASE_ID", "BOSS_WORKSPACE"];

/// Path fragments that identify a directory as agent-owned. Matched
/// against the engine binary's own path, so the classification holds
/// even when the environment has been dropped (`open`) or scrubbed.
///
/// - `.local/share/cube/workspaces` and `Documents/dev/workspaces` are
///   the two cube workspace roots.
/// - `cube-workspaces-` appears in the mangled per-session directory
///   name an agent harness derives from a workspace path.
/// - `scratchpad` is the per-session scratch directory agents are told
///   to use for temporary files; a Boss bundle unpacked for a test run
///   lands there.
///
/// Deliberately absent: bazel output roots on their own. A developer
/// building and running from `bazel-bin` in their own checkout is doing
/// something legitimate, and the two clauses of the gate mean an agent
/// doing it from a workspace is already caught by the workspace root.
const AGENT_OWNED_PATH_FRAGMENTS: &[&str] = &[
    ".local/share/cube/workspaces",
    "Documents/dev/workspaces",
    "cube-workspaces-",
    "scratchpad",
];

/// Why this process counts as agent-owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentOwnership {
    /// A Boss session marker was set in the environment.
    SessionEnv(&'static str),
    /// The engine binary lives under an agent-owned directory. Carries
    /// the fragment that matched and the binary's own path.
    BinaryLocation { fragment: &'static str, exe: PathBuf },
}

impl std::fmt::Display for AgentOwnership {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentOwnership::SessionEnv(key) => write!(f, "{key} is set in this environment"),
            AgentOwnership::BinaryLocation { fragment, exe } => {
                write!(
                    f,
                    "this binary lives under an agent-owned path ({fragment}): {}",
                    exe.display()
                )
            }
        }
    }
}

/// One resolved runtime path that would land on production state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Collision {
    /// What the path is for, e.g. `events socket`.
    pub label: &'static str,
    /// The resolved path this engine would have used.
    pub path: PathBuf,
    /// The environment variable that produced it, when one did. Named in
    /// the message so the reader knows which value to change.
    pub via_env: Option<&'static str>,
}

/// A refused start, with everything needed to explain it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentLaunchRefusal {
    pub ownership: AgentOwnership,
    pub collisions: Vec<Collision>,
}

impl std::fmt::Display for AgentLaunchRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "refusing to start: this engine would take over production Boss state, and it is \
             agent-owned ({ownership}).",
            ownership = self.ownership,
        )?;
        writeln!(f)?;
        writeln!(f, "Resolved paths that belong to production:")?;
        for collision in &self.collisions {
            match collision.via_env {
                Some(key) => writeln!(f, "  - {}: {} (from {key})", collision.label, collision.path.display())?,
                None => writeln!(f, "  - {}: {}", collision.label, collision.path.display())?,
            }
        }
        writeln!(f)?;
        writeln!(
            f,
            "Starting a second engine on those paths dispossesses the one already running: hook \
             deliveries stop arriving, live workers are reaped as falsely stale, and this process \
             would delete the real engine-control token when it exits."
        )?;
        writeln!(f)?;
        writeln!(f, "To exercise a real engine, start an isolated one:")?;
        writeln!(f)?;
        writeln!(
            f,
            "\x20   env -u BOSS_EVENTS_SOCKET bazel run //tools/boss/engine:engine -- --socket-path /tmp/boss-test-<id>.sock"
        )?;
        writeln!(f)?;
        writeln!(
            f,
            "Any --socket-path other than {default} puts the engine in test-fixture mode, where the \
             db, events socket, pid file and control token are all derived from that socket's path. \
             Unsetting BOSS_EVENTS_SOCKET matters because every agent pane inherits one pointing at \
             production, and an inherited value is otherwise treated as a deliberate override. Point \
             a client at the same --socket-path to drive it.",
            default = super::DEFAULT_SOCKET_PATH,
        )?;
        writeln!(f)?;
        write!(
            f,
            "Building and testing are unaffected: `bazel build //tools/boss/...` and `bazel test \
             //tools/boss/...` start no production engine. Launching the Boss app has the same \
             effect as launching an engine — the app terminates the running engine and starts its \
             own — so it is not a way around this. Verifying the GUI is not something an agent \
             session can do; hand that to a human."
        )
    }
}

impl std::error::Error for AgentLaunchRefusal {}

/// The runtime paths this engine has resolved, as passed to `serve`.
///
/// Borrowed rather than owned so the caller can hand over what it
/// already computed without cloning, and so the whole decision is a pure
/// function of its inputs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResolvedPaths<'a> {
    pub socket_path: &'a Path,
    pub events_socket: &'a Path,
    pub db_path: &'a Path,
    pub pid_path: &'a Path,
    pub token_path: Option<&'a Path>,
}

/// Decide whether to refuse this start.
///
/// `production_root` is the directory production state lives in
/// (`~/Library/Application Support/Boss`); `None` when it cannot be
/// resolved, in which case only the frontend-socket check applies.
/// `ownership` is the outcome of the agent-ownership classification —
/// `None` means this is not an agent-owned process and nothing is
/// refused.
pub(crate) fn evaluate(
    paths: ResolvedPaths<'_>,
    production_root: Option<&Path>,
    ownership: Option<AgentOwnership>,
) -> Result<(), AgentLaunchRefusal> {
    let Some(ownership) = ownership else {
        return Ok(());
    };
    let collisions = production_collisions(paths, production_root);
    if collisions.is_empty() {
        return Ok(());
    }
    Err(AgentLaunchRefusal { ownership, collisions })
}

/// Every resolved path that belongs to production, in report order.
fn production_collisions(paths: ResolvedPaths<'_>, production_root: Option<&Path>) -> Vec<Collision> {
    let mut collisions = Vec::new();

    if same_path(paths.socket_path, Path::new(super::DEFAULT_SOCKET_PATH)) {
        collisions.push(Collision {
            label: "frontend socket",
            path: paths.socket_path.to_path_buf(),
            via_env: None,
        });
    }

    let Some(root) = production_root else {
        return collisions;
    };
    for (label, path, via_env) in [
        ("events socket", paths.events_socket, Some("BOSS_EVENTS_SOCKET")),
        ("state db", paths.db_path, Some("BOSS_DB_PATH")),
        ("pid file", paths.pid_path, Some("BOSS_ENGINE_PID_PATH")),
    ] {
        if is_inside(path, root) {
            collisions.push(Collision {
                label,
                path: path.to_path_buf(),
                via_env,
            });
        }
    }
    if let Some(token) = paths.token_path
        && is_inside(token, root)
    {
        collisions.push(Collision {
            label: "engine-control token",
            path: token.to_path_buf(),
            via_env: Some(crate::engine_control::TOKEN_PATH_ENV),
        });
    }

    collisions
}

/// Classify this process as agent-owned, or not.
///
/// `env` looks up the process environment and `current_exe` yields the
/// running binary's path; both are injected so the decision is testable
/// without mutating process-global state. The environment is checked
/// first because it is the precise signal; the binary location is the
/// fallback that survives an environment being dropped.
pub(crate) fn classify_ownership(
    env: impl Fn(&str) -> Option<String>,
    current_exe: Option<PathBuf>,
) -> Option<AgentOwnership> {
    if let Some(key) = AGENT_SESSION_ENV_KEYS
        .iter()
        .copied()
        .find(|key| env(key).is_some_and(|value| !value.trim().is_empty()))
    {
        return Some(AgentOwnership::SessionEnv(key));
    }
    let exe = current_exe?;
    let as_text = exe.to_string_lossy();
    let fragment = AGENT_OWNED_PATH_FRAGMENTS
        .iter()
        .copied()
        .find(|fragment| as_text.contains(fragment))?;
    Some(AgentOwnership::BinaryLocation { fragment, exe })
}

/// Whether `child` is `parent` or sits underneath it, comparing
/// normalised paths so a `.` segment or a `..` cannot slip past.
fn is_inside(child: &Path, parent: &Path) -> bool {
    let child = normalize(child);
    let parent = normalize(parent);
    child.starts_with(&parent)
}

/// Whether two paths name the same location after normalisation.
fn same_path(left: &Path, right: &Path) -> bool {
    normalize(left) == normalize(right)
}

/// Canonicalise the parent directory and re-attach the file name,
/// dropping `.` segments and resolving `..` lexically first.
///
/// The leaf is deliberately not canonicalised: at this point in startup
/// sockets and token files do not exist yet, which is exactly when the
/// comparison matters. Canonicalising the parent still resolves the
/// macOS `/tmp` → `/private/tmp` symlink, so the two spellings of the
/// production socket compare equal. Falls back to the lexical form when
/// the parent cannot be canonicalised, keeping the comparison total.
fn normalize(path: &Path) -> PathBuf {
    let lexical = lexically_normalize(path);
    let (Some(parent), Some(name)) = (lexical.parent(), lexical.file_name()) else {
        return lexical;
    };
    match parent.canonicalize() {
        Ok(real_parent) => real_parent.join(name),
        Err(_) => lexical,
    }
}

/// Drop `.` components and collapse `..` against the preceding
/// component, without touching the filesystem.
fn lexically_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROD_ROOT: &str = "/Users/dev/Library/Application Support/Boss";

    fn prod_root() -> PathBuf {
        PathBuf::from(PROD_ROOT)
    }

    /// Paths as a production engine resolves them.
    fn production_paths() -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
        (
            PathBuf::from(super::super::DEFAULT_SOCKET_PATH),
            prod_root().join("events.sock"),
            prod_root().join("state.db"),
            PathBuf::from("/tmp/boss-engine.pid"),
            prod_root().join("engine-control.token"),
        )
    }

    /// Paths as an isolated fixture resolves them from
    /// `--socket-path /tmp/boss-test-<id>.sock`.
    fn fixture_paths(stem: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
        let base = PathBuf::from("/tmp");
        (
            base.join(format!("{stem}.sock")),
            base.join(format!("{stem}.events.sock")),
            base.join(format!("{stem}.db")),
            base.join(format!("{stem}.pid")),
            base.join(format!("{stem}.token")),
        )
    }

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |key: &str| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| (*v).to_owned())
    }

    const NO_ENV: &[(&str, &str)] = &[];
    const WORKER_ENV: &[(&str, &str)] = &[
        ("BOSS_RUN_ID", "exec_18c50ef667d22270_13b"),
        ("BOSS_LEASE_ID", "5620e9e8-96e8-4bdc-bf37-3551f54b1b06"),
        (
            "BOSS_WORKSPACE",
            "/Users/dev/.local/share/cube/workspaces/mono-agent-135",
        ),
    ];

    // -- ownership classification ------------------------------------

    #[test]
    fn session_env_marks_a_process_agent_owned() {
        let ownership = classify_ownership(env_of(WORKER_ENV), Some(PathBuf::from("/Applications/Boss.app/x")));
        assert_eq!(ownership, Some(AgentOwnership::SessionEnv("BOSS_RUN_ID")));
    }

    #[test]
    fn each_session_marker_is_independently_sufficient() {
        for key in AGENT_SESSION_ENV_KEYS {
            let owned_key = (*key).to_owned();
            let env = move |lookup: &str| (lookup == owned_key).then(|| "set".to_owned());
            assert_eq!(
                classify_ownership(env, None),
                Some(AgentOwnership::SessionEnv(key)),
                "{key} alone must mark the process agent-owned",
            );
        }
    }

    #[test]
    fn blank_session_markers_are_not_ownership() {
        let env = env_of(&[("BOSS_RUN_ID", ""), ("BOSS_LEASE_ID", "   ")]);
        assert_eq!(classify_ownership(env, Some(PathBuf::from("/Applications/Boss"))), None);
    }

    /// `open` launches through LaunchServices, which drops the caller's
    /// environment — so an app started that way carries no session
    /// markers and the binary's own location has to carry the signal.
    #[test]
    fn scratchpad_bundle_is_agent_owned_without_any_env() {
        let exe = PathBuf::from(
            "/private/tmp/claude-501/-Users-dev--local-share-cube-workspaces-mono-agent-135/\
             97a79896-f11c-4b4a-8eb4-4b070f5707fb/scratchpad/boss-app-run/Boss.app/Contents/Resources/bin/engine",
        );
        let ownership = classify_ownership(env_of(NO_ENV), Some(exe.clone()));
        assert!(
            matches!(ownership, Some(AgentOwnership::BinaryLocation { .. })),
            "an engine inside an agent scratchpad must be agent-owned: {ownership:?}",
        );
    }

    #[test]
    fn cube_workspace_binary_is_agent_owned_without_any_env() {
        let exe = PathBuf::from(
            "/Users/dev/.local/share/cube/workspaces/mono-agent-123/bazel-bin/tools/boss/engine/core/engine",
        );
        assert!(matches!(
            classify_ownership(env_of(NO_ENV), Some(exe)),
            Some(AgentOwnership::BinaryLocation { .. }),
        ));
    }

    /// The user's own engine — installed, or built in their own
    /// checkout — is never agent-owned, so the gate can never refuse it.
    #[test]
    fn installed_and_developer_binaries_are_not_agent_owned() {
        for exe in [
            "/Applications/Boss.app/Contents/Resources/bin/engine",
            "/Users/dev/Documents/dev/mono/bazel-bin/tools/boss/engine/core/engine",
            "/opt/homebrew/bin/engine",
        ] {
            assert_eq!(
                classify_ownership(env_of(NO_ENV), Some(PathBuf::from(exe))),
                None,
                "{exe} must not be classified as agent-owned",
            );
        }
    }

    // -- the two incidents -------------------------------------------

    /// Incident 1: a fixture on its own `--socket-path`, with
    /// `BOSS_DB_PATH` pointed at scratch, but the events socket
    /// inherited from the worker pane still resolving production.
    #[test]
    fn refuses_fixture_whose_events_socket_is_the_inherited_production_one() {
        let socket = PathBuf::from("/tmp/boss-dsgn.sock");
        let events = prod_root().join("events.sock");
        let db = PathBuf::from("/tmp/scratch/bosshome/state.db");
        let pid = PathBuf::from("/tmp/boss-dsgn.pid");
        let token = PathBuf::from("/tmp/boss-dsgn.token");
        let refusal = evaluate(
            ResolvedPaths {
                socket_path: &socket,
                events_socket: &events,
                db_path: &db,
                pid_path: &pid,
                token_path: Some(&token),
            },
            Some(&prod_root()),
            classify_ownership(env_of(WORKER_ENV), None),
        )
        .expect_err("an inherited production events socket must be refused");
        assert_eq!(refusal.collisions.len(), 1, "{:?}", refusal.collisions);
        assert_eq!(refusal.collisions[0].label, "events socket");
        assert_eq!(refusal.collisions[0].via_env, Some("BOSS_EVENTS_SOCKET"));
    }

    /// Incident 2: an app unpacked into an agent scratchpad started a
    /// bundled engine on the production defaults. No session markers
    /// reached it — `open` dropped them — so ownership comes from the
    /// binary's own path, and every production path collides.
    #[test]
    fn refuses_scratchpad_bundle_engine_on_production_defaults() {
        let exe = PathBuf::from(
            "/private/tmp/claude-501/-Users-dev--local-share-cube-workspaces-mono-agent-135/\
             97a79896/scratchpad/boss-app-run/Boss.app/Contents/Resources/bin/engine",
        );
        let (socket, events, db, pid, token) = production_paths();
        let refusal = evaluate(
            ResolvedPaths {
                socket_path: &socket,
                events_socket: &events,
                db_path: &db,
                pid_path: &pid,
                token_path: Some(&token),
            },
            Some(&prod_root()),
            classify_ownership(env_of(NO_ENV), Some(exe)),
        )
        .expect_err("a scratchpad bundle on production defaults must be refused");
        let labels: Vec<&str> = refusal.collisions.iter().map(|c| c.label).collect();
        assert_eq!(
            labels,
            vec!["frontend socket", "events socket", "state db", "engine-control token"],
            "every production path must be reported, not just the first",
        );
        assert!(matches!(refusal.ownership, AgentOwnership::BinaryLocation { .. }));
    }

    // -- launches that must keep working -----------------------------

    /// The supported isolated fixture: every path derived from a
    /// non-default `--socket-path`, nothing resolving into production.
    /// Run from a worker session, which is where fixtures normally run.
    #[test]
    fn allows_isolated_fixture_from_a_worker_session() {
        let (socket, events, db, pid, token) = fixture_paths("boss-test-9d3f0f22");
        assert!(
            evaluate(
                ResolvedPaths {
                    socket_path: &socket,
                    events_socket: &events,
                    db_path: &db,
                    pid_path: &pid,
                    token_path: Some(&token),
                },
                Some(&prod_root()),
                classify_ownership(env_of(WORKER_ENV), None),
            )
            .is_ok(),
            "an isolated fixture must run from a worker session",
        );
    }

    /// The isolation-demo shape: an engine binary inside a cube
    /// workspace, run with `HOME` repointed at a throwaway directory so
    /// every resolved path lands there instead of production.
    #[test]
    fn allows_agent_owned_binary_with_every_path_repointed_away_from_production() {
        let demo = PathBuf::from("/tmp/boss-iso-demo/home/Library/Application Support/Boss");
        let socket = PathBuf::from("/tmp/boss-iso-demo/engine.sock");
        let events = demo.join("events.sock");
        let db = demo.join("state.db");
        let pid = PathBuf::from("/tmp/boss-iso-demo/engine.pid");
        let token = demo.join("engine-control.token");
        let exe = PathBuf::from("/Users/dev/.local/share/cube/workspaces/mono-agent-131/bazel-bin/engine");
        assert!(
            evaluate(
                ResolvedPaths {
                    socket_path: &socket,
                    events_socket: &events,
                    db_path: &db,
                    pid_path: &pid,
                    token_path: Some(&token),
                },
                Some(&prod_root()),
                classify_ownership(env_of(NO_ENV), Some(exe)),
            )
            .is_ok(),
            "repointing HOME away from production is a legitimate isolated launch",
        );
    }

    /// The user's own engine on the production defaults — the case the
    /// gate must never touch, however the paths are spelled.
    #[test]
    fn allows_production_engine_that_is_not_agent_owned() {
        let (socket, events, db, pid, token) = production_paths();
        assert!(
            evaluate(
                ResolvedPaths {
                    socket_path: &socket,
                    events_socket: &events,
                    db_path: &db,
                    pid_path: &pid,
                    token_path: Some(&token),
                },
                Some(&prod_root()),
                classify_ownership(env_of(NO_ENV), Some(PathBuf::from("/Applications/Boss.app/x/engine"))),
            )
            .is_ok(),
            "the user's own production engine must always start",
        );
    }

    /// `bazel test` sandboxes repoint `HOME`, so nothing resolves into
    /// production even though the test binary sits in a workspace.
    #[test]
    fn allows_a_test_binary_with_no_production_paths() {
        let (socket, events, db, pid, token) = fixture_paths("boss-unit-test");
        assert!(
            evaluate(
                ResolvedPaths {
                    socket_path: &socket,
                    events_socket: &events,
                    db_path: &db,
                    pid_path: &pid,
                    token_path: Some(&token),
                },
                None,
                classify_ownership(env_of(WORKER_ENV), None),
            )
            .is_ok(),
        );
    }

    // -- path handling -----------------------------------------------

    /// Alternate spellings of the production socket resolve to the same
    /// decision, so a `.` or `..` segment is not a way through.
    #[test]
    fn production_socket_spellings_all_collide() {
        for socket in [
            "/tmp/boss-engine.sock",
            "/tmp/./boss-engine.sock",
            "/tmp/x/../boss-engine.sock",
        ] {
            let socket = PathBuf::from(socket);
            let (_, events, db, pid, token) = fixture_paths("boss-test-spelling");
            let refusal = evaluate(
                ResolvedPaths {
                    socket_path: &socket,
                    events_socket: &events,
                    db_path: &db,
                    pid_path: &pid,
                    token_path: Some(&token),
                },
                Some(&prod_root()),
                classify_ownership(env_of(WORKER_ENV), None),
            )
            .expect_err("{socket} names the production socket");
            assert_eq!(refusal.collisions[0].label, "frontend socket");
        }
    }

    /// Nothing is refused when the process is not agent-owned, no matter
    /// what it would touch — the gate is about agents, not about
    /// second-guessing the user.
    #[test]
    fn ownership_is_required_for_any_refusal() {
        let (socket, events, db, pid, token) = production_paths();
        assert!(
            evaluate(
                ResolvedPaths {
                    socket_path: &socket,
                    events_socket: &events,
                    db_path: &db,
                    pid_path: &pid,
                    token_path: Some(&token),
                },
                Some(&prod_root()),
                None,
            )
            .is_ok(),
        );
    }

    // -- the message -------------------------------------------------

    /// A refusal with no alternative produces a session that looks for
    /// one; the message has to carry the supported command and name the
    /// paths it matched on.
    #[test]
    fn refusal_message_is_actionable() {
        let refusal = AgentLaunchRefusal {
            ownership: AgentOwnership::SessionEnv("BOSS_RUN_ID"),
            collisions: vec![Collision {
                label: "events socket",
                path: prod_root().join("events.sock"),
                via_env: Some("BOSS_EVENTS_SOCKET"),
            }],
        };
        let message = refusal.to_string();
        for expected in [
            "--socket-path",
            "//tools/boss/engine:engine",
            "BOSS_EVENTS_SOCKET",
            "BOSS_RUN_ID",
            "events socket",
            "bazel test",
        ] {
            assert!(message.contains(expected), "message must mention {expected}: {message}");
        }
    }
}
