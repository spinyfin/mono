//! Wrapper-script source + version stamping.
//!
//! The wrapper script (`tools/boss/engine/remote/boss-remote-run.sh`)
//! is the engine's contract with remote workers: env vars in, exec
//! shape out. The engine bundles the source verbatim via `include_str!`
//! and stamps the canonical version string into it before pushing to a
//! remote host. The pushed file is what the remote actually runs.
//!
//! Version policy (per the distributed-agent-execution design,
//! "Wrapper Distribution"):
//!
//! - The wrapper carries a `BOSS_REMOTE_RUN_VERSION` constant near the
//!   top, replaced at push time with a value derived from the running
//!   engine binary's content fingerprint (e.g. `eng-7a3f2c1b9e04`).
//! - `--version` prints exactly that string and exits zero.
//! - The engine's expected version is computed from the same binary at
//!   runtime; comparison is exact-equality, not semver.
//! - Any mismatch triggers a re-push.
//!
//! The version used to derive from the engine's stamped git SHA, but
//! stamping the SHA into the engine crate busted the build cache on
//! every commit (see `installer/pkg.bzl`'s `build_info_rs`). The binary
//! fingerprint is a strictly better discriminator anyway: it changes iff
//! the engine bytes change — and because the wrapper source is bundled
//! into the engine via `include_str!`, any edit to the wrapper changes
//! those bytes and therefore the fingerprint, preserving the contract.

/// Verbatim wrapper script source. Bundled at compile time so the
/// engine has one source of truth and no separate distribution path.
const WRAPPER_SOURCE: &str = include_str!("../remote/boss-remote-run.sh");

/// Sentinel string in the wrapper source that the engine replaces with
/// the canonical version string at push time. Defined once so a typo
/// in either side fails the unit test below at build time.
const VERSION_PLACEHOLDER: &str = "__BOSS_REMOTE_RUN_VERSION__";

/// The canonical wrapper version string, derived from the running
/// engine binary's content fingerprint (e.g. `eng-7a3f2c1b9e04`). Falls
/// back to `eng-unknown` only if the engine cannot read its own binary
/// (extremely rare; see [`crate::build_info::binary_fingerprint`]).
///
/// Exact-equality is the engine ↔ wrapper version contract. The wrapper
/// source is bundled into the engine via `include_str!`, so any change
/// to it produces a different engine binary, a different fingerprint,
/// and therefore a re-push — which is exactly the contract we want.
pub fn expected_version() -> String {
    format!("eng-{}", crate::build_info::binary_fingerprint())
}

/// Return the wrapper source ready to push to a remote host, with the
/// `__BOSS_REMOTE_RUN_VERSION__` placeholder replaced by [`expected_version`].
///
/// Panics if the placeholder isn't present in the source — that means
/// the wrapper script was edited in a way that broke the contract. The
/// unit test `placeholder_present_in_source` catches the same problem
/// at build time so a panic in production is unlikely.
pub fn rendered_wrapper() -> String {
    let version = expected_version();
    debug_assert!(
        WRAPPER_SOURCE.contains(VERSION_PLACEHOLDER),
        "wrapper source missing __BOSS_REMOTE_RUN_VERSION__ placeholder"
    );
    WRAPPER_SOURCE.replacen(VERSION_PLACEHOLDER, &version, 1)
}

/// Remote install path (per the design's "Install location on the remote").
pub const REMOTE_WRAPPER_DIR: &str = ".boss-remote/bin";

/// Filename of the wrapper on the remote.
pub const REMOTE_WRAPPER_NAME: &str = "boss-remote-run";

/// Absolute install path on the remote relative to `$HOME`. The remote
/// expansion happens via the wrapper invocation itself; the engine
/// always invokes with `~/.boss-remote/bin/boss-remote-run` so it
/// doesn't need to know the remote's `$HOME` value.
pub fn remote_wrapper_path() -> String {
    format!("~/{REMOTE_WRAPPER_DIR}/{REMOTE_WRAPPER_NAME}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh_spawn::WORKER_EXIT_STATUS_PREFIX;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::time::Duration;

    #[test]
    fn placeholder_present_in_source() {
        assert!(
            WRAPPER_SOURCE.contains(VERSION_PLACEHOLDER),
            "wrapper source must contain the version placeholder so the engine \
             can stamp a real version before push; the build-time `include_str!` \
             would otherwise ship un-versioned bytes"
        );
    }

    #[test]
    fn rendered_wrapper_replaces_placeholder() {
        let rendered = rendered_wrapper();
        assert!(
            !rendered.contains(VERSION_PLACEHOLDER),
            "rendered wrapper still contains the placeholder; replacen failed"
        );
        let expected = expected_version();
        assert!(
            rendered.contains(&expected),
            "rendered wrapper should contain `{expected}` but did not"
        );
    }

    #[test]
    fn expected_version_has_eng_prefix() {
        let v = expected_version();
        assert!(
            v.starts_with("eng-"),
            "expected_version should start with `eng-`, got {v}"
        );
    }

    #[test]
    fn wrapper_passes_settings_file_through_to_driver() {
        // The engine ships the worker's `--settings` JSON outside the
        // workspace tree and points the resolved driver at it via
        // BOSS_SETTINGS_FILE; the wrapper must consume that env var and
        // forward `--settings`. A refactor that dropped either side would
        // silently strip the boss-event hooks from remote workers, pinning
        // their lifecycle.
        assert!(
            WRAPPER_SOURCE.contains("BOSS_SETTINGS_FILE"),
            "wrapper must read BOSS_SETTINGS_FILE so the engine can wire boss-event hooks remotely"
        );
        assert!(
            WRAPPER_SOURCE.contains("--settings"),
            "wrapper must pass `--settings` to the driver when BOSS_SETTINGS_FILE is set"
        );
    }

    #[test]
    fn wrapper_launches_resolved_driver_not_hardcoded_claude() {
        // A row allocated to codex must not silently exec `claude` on a
        // remote host. The wrapper reads BOSS_DRIVER and execs that binary.
        assert!(
            WRAPPER_SOURCE.contains("BOSS_DRIVER"),
            "wrapper must read BOSS_DRIVER so the engine can launch the resolved driver"
        );
        assert!(
            WRAPPER_SOURCE.contains("\"$BOSS_DRIVER\""),
            "wrapper must exec \"$BOSS_DRIVER\", not a hardcoded claude binary"
        );
        // The old hardcoded form must not reappear as the launch target.
        assert!(
            !WRAPPER_SOURCE.contains("nohup claude "),
            "wrapper must not hardcode `nohup claude`; launch the resolved driver"
        );
    }

    #[test]
    fn wrapper_records_the_workers_observed_exit_status() {
        assert!(
            WRAPPER_SOURCE.contains(&format!("{WORKER_EXIT_STATUS_PREFIX}%s")),
            "the detached worker supervisor must append its observed exit status to worker.log"
        );
    }

    #[test]
    fn wrapper_handshake_reports_the_direct_claude_pid() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir(&bin_dir).unwrap();

        let wrapper = temp.path().join("boss-remote-run");
        std::fs::write(&wrapper, rendered_wrapper()).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

        let claude_pid = temp.path().join("claude.pid");
        for (name, body) in [
            (
                "claude",
                "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$CLAUDE_PID_FILE\"\nsleep 0.1\n",
            ),
            ("cube", "#!/bin/sh\nexit 0\n"),
            ("gh", "#!/bin/sh\nexit 0\n"),
            ("boss-event", "#!/bin/sh\nexit 0\n"),
            // Bazel's macOS sandbox denies the platform `nohup` binary, so
            // model its argument-forwarding behavior locally.
            ("nohup", "#!/bin/sh\nexec \"$@\"\n"),
        ] {
            let path = bin_dir.join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let path = format!(
            "{}:{}:/usr/bin:/bin",
            bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let output = Command::new("/bin/sh")
            .arg(&wrapper)
            .env("PATH", path)
            .env("CLAUDE_PID_FILE", &claude_pid)
            .env("BOSS_RUN_ID", "run-test")
            .env("BOSS_EVENTS_SOCKET", "/tmp/boss-events-test.sock")
            .env("BOSS_LEASE_ID", "lease-test")
            .env("BOSS_WORKSPACE", &workspace)
            .output()
            .unwrap();

        let worker_log = std::fs::read_to_string(workspace.join(".boss/worker.log")).unwrap_or_default();
        assert!(
            output.status.success(),
            "wrapper stderr: {}; worker log: {worker_log}",
            String::from_utf8_lossy(&output.stderr)
        );
        let handshake_pid = crate::ssh_spawn::parse_remote_pid(&String::from_utf8_lossy(&output.stderr)).unwrap();
        // The wrapper has successfully received the direct child PID at this
        // point, but a loaded CI shard can still delay the child before it
        // writes its test marker. Wait long enough to observe that asynchronous
        // handoff instead of treating scheduler latency as a bad PID.
        for _ in 0..1_000 {
            if claude_pid.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let direct_claude_pid = std::fs::read_to_string(claude_pid)
            .unwrap()
            .trim()
            .parse::<i64>()
            .unwrap();
        assert_eq!(handshake_pid, direct_claude_pid);
    }

    #[test]
    fn wrapper_source_has_shebang() {
        // The remote ends up running the file directly via
        // `~/.boss-remote/bin/boss-remote-run`, so the shebang is
        // load-bearing. A refactor that strips the first line would
        // produce a wrapper that fails with "exec format error".
        assert!(
            WRAPPER_SOURCE.starts_with("#!/bin/sh\n"),
            "wrapper must start with `#!/bin/sh` so the kernel runs it via /bin/sh"
        );
    }
}
