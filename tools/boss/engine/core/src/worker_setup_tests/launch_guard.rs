use super::super::*;

// ── BOSS_LAUNCH_GUARD_COMMAND execution tests ─────────────────────────
//
// These run the guard through `sh -c`, exactly as claude does, so the
// shell quoting of the inline python is covered alongside its matching.
//
// The guard is the advisory layer; `boss_engine::app::agent_launch_guard`
// is the control. Both incident commands below are covered by both
// layers deliberately — the guard so the worker fails fast with an
// explanation, the engine gate so the outcome does not depend on the
// command's spelling.

/// Run the Boss-launch guard against a simulated Bash `tool_input`
/// payload and return its decision plus reason.
fn run_launch_guard(bash_command: &str) -> (String, String) {
    use std::io::Write as _;
    let stdin_payload = serde_json::json!({
        "tool_input": {"command": bash_command}
    })
    .to_string();

    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(BOSS_LAUNCH_GUARD_COMMAND)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("sh must be available");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin_payload.as_bytes())
        .unwrap();
    drop(child.stdin.take());

    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "launch guard produced invalid JSON for command {:?}: {e}\nstdout={stdout}\nstderr={}",
            bash_command,
            String::from_utf8_lossy(&out.stderr),
        )
    });
    (
        parsed["decision"].as_str().unwrap_or("missing").to_owned(),
        parsed["reason"].as_str().unwrap_or_default().to_owned(),
    )
}

fn launch_decision(bash_command: &str) -> String {
    run_launch_guard(bash_command).0
}

// --- the observed launches that must be blocked ---

/// The first incident, verbatim. `./bazel-bin/...` carries no bundle
/// shape at all; the program's basename is the whole signal.
#[test]
fn launch_guard_blocks_engine_run_from_bazel_bin() {
    let command = concat!(
        "SP=/private/tmp/claude-501/-Users-dev--local-share-cube-workspaces-mono-agent-123/scratchpad\n",
        "SOCK=/tmp/boss-dsgn.sock\n",
        "export BOSS_DB_PATH=\"$SP/bosshome/state.db\"\n",
        "rm -f \"$SOCK\" \"$BOSS_DB_PATH\"\n",
        "nohup ./bazel-bin/tools/boss/engine/core/engine --socket-path \"$SOCK\" > \"$SP/engine.log\" 2>&1 &",
    );
    assert_eq!(launch_decision(command), "block");
}

/// The second incident, verbatim. The bundle path is assigned on one
/// line and opened on the next, so no single line carries both — the
/// previous regex could not span it, and variable resolution is what
/// closes it.
#[test]
fn launch_guard_blocks_open_of_a_bundle_held_in_a_shell_variable() {
    let command = concat!(
        "SCRATCH=/private/tmp/claude-501/-Users-dev--local-share-cube-workspaces-mono-agent-135/scratchpad\n",
        "APP=\"$SCRATCH/boss-app-run/Boss.app\"\n",
        "open \"$APP\" 2>&1\n",
        "sleep 3\n",
        "pgrep -fl \"Boss.app\"",
    );
    assert_eq!(launch_decision(command), "block");
}

/// The same launcher without the indirection, and the other spellings
/// the app can be started with.
#[test]
fn launch_guard_blocks_direct_app_launches() {
    for command in [
        "open /tmp/x/Boss.app",
        "open -a Boss",
        "open -b dev.spinyfin.bossmacapp",
        "/Applications/Boss.app/Contents/MacOS/Boss",
        "cd /tmp/x/Boss.app/Contents/MacOS && ./Boss",
        "bazel run //tools/boss/app-macos:Boss",
        "swift run Boss",
    ] {
        assert_eq!(launch_decision(command), "block", "must block: {command}");
    }
}

/// A launcher prefix must not hide the program being started.
#[test]
fn launch_guard_blocks_engine_behind_a_launcher_prefix() {
    for command in [
        "nohup ./engine --socket-path /tmp/s.sock &",
        "env FOO=1 ./bazel-bin/tools/boss/engine/core/engine",
        "timeout 60 ./bazel-bin/tools/boss/engine/core/engine --socket-path /tmp/s.sock",
        "exec /tmp/boss-app-run/Boss.app/Contents/Resources/bin/engine",
    ] {
        assert_eq!(launch_decision(command), "block", "must block: {command}");
    }
}

/// `bazel run` of an engine target with no `--socket-path` starts a
/// production engine, so it is blocked — with the isolating form named
/// in the reason rather than a bare refusal.
#[test]
fn launch_guard_blocks_bazel_run_engine_without_an_isolating_socket() {
    for command in [
        "bazel run //tools/boss/engine:engine",
        "bazel run //tools/boss/engine:engine -- --socket-path /tmp/boss-engine.sock",
    ] {
        assert_eq!(launch_decision(command), "block", "must block: {command}");
    }
}

// --- launches and inspections that must be allowed ---

/// The supported isolated engine. Blocking this is what drives a worker
/// to unpack a bundle and run the binary by hand, which is how both
/// incidents started.
#[test]
fn launch_guard_allows_an_isolated_bazel_run_engine() {
    for command in [
        "bazel run //tools/boss/engine:engine -- --socket-path /tmp/boss-test-9d3f0f22.sock",
        "env -u BOSS_EVENTS_SOCKET bazel run //tools/boss/engine:engine -- --socket-path /tmp/boss-test-abc.sock",
        "bazel run //tools/boss/engine:engine -- --socket-path=/tmp/boss-test-abc.sock",
    ] {
        assert_eq!(launch_decision(command), "approve", "must allow: {command}");
    }
}

/// Building, testing, and looking at a bundle are all untouched.
#[test]
fn launch_guard_allows_build_test_and_inspection() {
    for command in [
        "bazel build //tools/boss/app-macos:Boss",
        "bazel test //tools/boss/... --test_output=errors",
        "unzip -oq bazel-bin/tools/boss/app-macos/Boss.zip -d /tmp/scratch/boss-app-run",
        "find /tmp/scratch/boss-app-run -maxdepth 1",
        "ls -la /tmp/scratch/boss-app-run/Boss.app/Contents/MacOS",
        r#"jj describe -m "block workers from running open -a Boss""#,
    ] {
        assert_eq!(launch_decision(command), "approve", "must allow: {command}");
    }
}

/// The reason has to hand the worker the supported command; a refusal
/// with no alternative produces a worker that finds its own.
#[test]
fn launch_guard_reason_names_the_isolated_alternative() {
    let (decision, reason) = run_launch_guard("open -a Boss");
    assert_eq!(decision, "block");
    for expected in [
        "--socket-path",
        "//tools/boss/engine:engine",
        "BOSS_EVENTS_SOCKET",
        "bazel build",
        "bazel test",
    ] {
        assert!(reason.contains(expected), "reason must mention {expected}: {reason}");
    }
}
