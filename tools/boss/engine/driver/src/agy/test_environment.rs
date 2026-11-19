use super::{environment, home};
use crate::scoped_home::SCOPED_HOME_ENV_TEST_LOCK;
use std::ffi::OsString;
use std::path::Path;

/// Write the real permission-renderer output for `kind` into the run's
/// control directory, exactly as `write_permission_config` does before the
/// authorization adapter is installed.
pub(crate) fn write_final_settings(run_id: &str, workspace: &Path, kind: crate::WorkerKind, events_socket: &Path) {
    let input = crate::PermissionInput::builder()
        .worker_kind(kind)
        .workspace_path(workspace.to_path_buf())
        .events_socket_path(events_socket.to_path_buf())
        .boss_event_path(events_socket.with_file_name("boss-event"))
        .run_id(run_id)
        .lease_id("lease-1")
        .execution_kind("task_implementation")
        .is_remote(false)
        .build();
    super::permissions::write_session_settings(&input).unwrap();
}

pub(crate) struct EnvGuard {
    // Declared first so it is released first: the transcript-store lock is the
    // innermost of the test environment locks (see `transcript_store_override`).
    _store: Option<crate::test_support::TranscriptStoreOverride>,
    _lock: Option<std::sync::MutexGuard<'static, ()>>,
    prior: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    pub(crate) fn set(pairs: &[(&'static str, &str)]) -> Self {
        let lock = SCOPED_HOME_ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        Self::set_with_lock(pairs, Some(lock))
    }

    pub(crate) fn set_with_lock(
        pairs: &[(&'static str, &str)],
        lock: Option<std::sync::MutexGuard<'static, ()>>,
    ) -> Self {
        let mut prior = Vec::new();
        for (key, value) in pairs {
            prior.push((*key, std::env::var_os(key)));
            // SAFETY: serialised by SCOPED_HOME_ENV_TEST_LOCK.
            unsafe { std::env::set_var(key, value) };
        }
        Self {
            _store: None,
            _lock: lock,
            prior,
        }
    }

    /// Also point the durable worker transcript store at `root`, for as long as
    /// this guard lives. Provisioning an `agy` home links its `brain/` into
    /// that store, so any test that provisions must not reach the real one.
    pub(crate) fn with_transcript_store(mut self, root: &Path) -> Self {
        self._store = Some(crate::test_support::transcript_store_override(root));
        self
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.prior {
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }
}

pub(crate) fn env_for_provision(homes: &Path) -> EnvGuard {
    let lock = SCOPED_HOME_ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    env_for_provision_with_lock(homes, Some(lock))
}

pub(crate) fn env_for_provision_with_home_override(homes: &Path) -> EnvGuard {
    env_for_provision_with_lock(homes, None)
}

/// Stand-in for the vendor CLI in provisioning tests that keep the live
/// `models` probe on. It does the two things a real start does that
/// provisioning depends on, and nothing else:
///
/// - It rewrites `$HOME/.gemini/antigravity-cli/settings.json` in the CLI's
///   sparse form, which leaves out every key whose value is a built-in
///   default. `queuedMessages: "queue"` is one. The real CLI does this in
///   `validateCliSettings` on every start, before `models` runs (read from
///   the 1.5.9 binary; see `restore_private_cli_settings_after_cli_start`).
/// - `models` needs a credential, and the only one a scoped home can hold is
///   the bridged login keychain. Without it the CLI prints its sign-in error
///   and exits 1 (pane study, scoped-home evidence). It does not sign in.
///
/// Each start appends one JSON line (`argv`, `home`, `dropped`) to
/// `<binary>.starts`; read it back with [`fake_agy_starts`].
const FAKE_AGY: &str = r#"#!/usr/bin/env python3
import json
import os
import sys

DEFAULTS = {"colorScheme": "terminal", "queuedMessages": "queue", "toolPermission": "request-review"}


def rewrite_sparse(path):
    try:
        with open(path) as handle:
            raw = handle.read()
    except FileNotFoundError:
        return None
    parsed = json.loads(raw)
    sparse = {key: value for key, value in parsed.items() if DEFAULTS.get(key) != value}
    canonical = json.dumps(sparse, indent=2) + "\n"
    if canonical != raw:
        staging = path + ".tmp"
        with open(staging, "w") as handle:
            handle.write(canonical)
        os.replace(staging, path)
    return sorted(set(parsed) - set(sparse))


home = os.environ["HOME"]
dropped = rewrite_sparse(os.path.join(home, ".gemini", "antigravity-cli", "settings.json"))
with open(os.path.abspath(sys.argv[0]) + ".starts", "a") as trace:
    trace.write(json.dumps({"argv": sys.argv[1:], "home": home, "dropped": dropped}) + "\n")
if sys.argv[-1:] != ["models"]:
    sys.exit("fake agy: only `models` is implemented")
print("Fetching available models...")
if not os.path.exists(os.path.join(home, "Library", "Keychains", "login.keychain-db")):
    sys.exit("Error: Please sign in to view available models. Launch the CLI without arguments to sign in.")
print("gemini-3.8-flash")
"#;

/// Like [`env_for_provision_with_home_override`], but the live `models` probe
/// stays on and runs [`FAKE_AGY`], installed as the Boss-owned binary under
/// `homes`. Binary identity checks stay off: the stand-in is not a signed
/// vendor binary. The caller holds a `home_override`, whose lock serialises
/// the environment and whose `HOME` decides whether a login keychain is
/// bridged into the run's home.
pub(crate) fn env_for_provision_with_live_probe(homes: &Path) -> EnvGuard {
    std::fs::create_dir_all(homes.join("bin")).unwrap();
    crate::hook_guards::write_executable(&homes.join("bin/agy"), FAKE_AGY).unwrap();
    EnvGuard::set_with_lock(
        &[
            (home::AGY_HOMES_ROOT_ENV, homes.to_str().unwrap()),
            (home::AGY_SKIP_POSTURE_ASSERT_ENV, "0"),
            (environment::AGY_SKIP_BINARY_ASSERT_ENV, "1"),
        ],
        None,
    )
    .with_transcript_store(&transcript_store_for(homes))
}

/// One entry per start of the [`FAKE_AGY`] installed under `homes`, in order.
pub(crate) fn fake_agy_starts(homes: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(homes.join("bin/agy.starts"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Where [`env_for_provision`] puts the durable transcript store: inside the
/// test's own homes directory, so it is cleaned up with it, but outside every
/// per-run container.
pub(crate) fn transcript_store_for(homes: &Path) -> std::path::PathBuf {
    homes.join(".transcript-store")
}

fn env_for_provision_with_lock(homes: &Path, lock: Option<std::sync::MutexGuard<'static, ()>>) -> EnvGuard {
    EnvGuard::set_with_lock(
        &[
            (home::AGY_HOMES_ROOT_ENV, homes.to_str().unwrap()),
            (home::AGY_SKIP_POSTURE_ASSERT_ENV, "1"),
            (environment::AGY_SKIP_BINARY_ASSERT_ENV, "1"),
        ],
        lock,
    )
    .with_transcript_store(&transcript_store_for(homes))
}
