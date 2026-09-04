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
    run_launch_guard_with_env(bash_command, &[])
}

fn run_launch_guard_with_env(bash_command: &str, extra_env: &[(&str, &str)]) -> (String, String) {
    run_launch_guard_with_env_in_dir(bash_command, extra_env, None)
}

fn run_launch_guard_with_env_in_dir(
    bash_command: &str,
    extra_env: &[(&str, &str)],
    current_dir: Option<&std::path::Path>,
) -> (String, String) {
    run_launch_guard_with_env_dir_and_payload_cwd(bash_command, extra_env, current_dir, None)
}

/// Like [`run_launch_guard_with_env_in_dir`], but also lets the caller set
/// the hook payload's own `cwd` field independently of the guard process's
/// actual `current_dir` — this is what proves `is_workspace_checkleft`
/// resolves relative paths against the payload, not `os.getcwd()`, since
/// Codex and Grok run this same guard script from an adapter whose process
/// cwd is not guaranteed to be the workspace root.
fn run_launch_guard_with_env_dir_and_payload_cwd(
    bash_command: &str,
    extra_env: &[(&str, &str)],
    current_dir: Option<&std::path::Path>,
    payload_cwd: Option<&std::path::Path>,
) -> (String, String) {
    use std::io::Write as _;
    let mut payload = serde_json::json!({
        "tool_input": {"command": bash_command}
    });
    if let Some(payload_cwd) = payload_cwd {
        payload["cwd"] = serde_json::json!(payload_cwd.to_str().unwrap());
    }
    let stdin_payload = payload.to_string();

    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg(BOSS_LAUNCH_GUARD_COMMAND);
    if let Some(current_dir) = current_dir {
        cmd.current_dir(current_dir);
    }
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let mut child = cmd
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
        "bazel run //tools/boss/engine/core:engine",
        "bazel run //tools/boss/engine/core:engine -- --socket-path /tmp/boss-engine.sock",
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
        "bazel run //tools/boss/engine/core:engine -- --socket-path /tmp/boss-test-9d3f0f22.sock",
        "env -u BOSS_EVENTS_SOCKET bazel run //tools/boss/engine/core:engine -- --socket-path /tmp/boss-test-abc.sock",
        "bazel run //tools/boss/engine/core:engine -- --socket-path=/tmp/boss-test-abc.sock",
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

/// Isolated capture instance: non-production `BOSS_SOCKET_PATH` plus
/// `BOSS_ENGINE_AUTOSTART=0`. Mirrors the engine exemption — workers can
/// screenshot the real UI without seizing production.
#[test]
fn launch_guard_allows_isolated_app_macos_capture() {
    for command in [
        "BOSS_SOCKET_PATH=/tmp/boss-shot-9d3f.sock BOSS_ENGINE_AUTOSTART=0 bazel run //tools/boss/app-macos:Boss -- --capture-to /tmp/shot.png",
        "BOSS_ENGINE_AUTOSTART=0 BOSS_SOCKET_PATH=/tmp/boss-shot-abc.sock bazel run //tools/boss/app-macos:Boss",
        // multi-line assignment then bazel run (same pattern as the open incident)
        concat!(
            "SOCK=/tmp/boss-shot-xyz.sock\n",
            "BOSS_SOCKET_PATH=$SOCK BOSS_ENGINE_AUTOSTART=0 bazel run //tools/boss/app-macos:Boss -- --capture-to /tmp/x.png\n",
        ),
    ] {
        assert_eq!(launch_decision(command), "approve", "must allow: {command}");
    }
}

/// app-macos without both isolation signals stays blocked — missing
/// socket, production socket, Application Support path, or autostart
/// left enabled.
#[test]
fn launch_guard_blocks_app_macos_without_full_isolation() {
    for command in [
        "bazel run //tools/boss/app-macos:Boss",
        "BOSS_SOCKET_PATH=/tmp/boss-shot.sock bazel run //tools/boss/app-macos:Boss",
        "BOSS_ENGINE_AUTOSTART=0 bazel run //tools/boss/app-macos:Boss",
        "BOSS_SOCKET_PATH=/tmp/boss-engine.sock BOSS_ENGINE_AUTOSTART=0 bazel run //tools/boss/app-macos:Boss",
        "BOSS_SOCKET_PATH='/Users/x/Library/Application Support/Boss/x.sock' BOSS_ENGINE_AUTOSTART=0 bazel run //tools/boss/app-macos:Boss",
        "BOSS_SOCKET_PATH=/tmp/boss-shot.sock BOSS_ENGINE_AUTOSTART=1 bazel run //tools/boss/app-macos:Boss",
    ] {
        assert_eq!(launch_decision(command), "block", "must block: {command}");
    }
}

/// Direct binary / open of Boss.app remains unconditionally blocked —
/// the narrowing applies only to the `bazel run app-macos` case and to
/// CLI tools under `Contents/Resources/bin/` (see below).
#[test]
fn launch_guard_still_blocks_direct_boss_app_binary() {
    for command in [
        "/Applications/Boss.app/Contents/MacOS/Boss",
        "open /Applications/Boss.app",
        "open -a Boss",
        "open -b dev.spinyfin.bossmacapp",
    ] {
        assert_eq!(launch_decision(command), "block", "must block: {command}");
    }
}

/// Bundled CLI tools live inside the `.app` at `Contents/Resources/bin/`.
/// Executing them is *not* launching the production app (that is
/// `Contents/MacOS/Boss` or `open -a Boss`). The previous matcher treated
/// any path containing `Boss.app` as a launch, which forbade the only
/// version-matched `boss` a worker can use.
///
/// Pin both the production install path and a non-`/Applications` bundle
/// so a dev build's own CLI is allowed too — hardcoding `/Applications`
/// is exactly the skew this allowance must not reintroduce. `bossctl` is
/// deliberately absent here: see
/// `launch_guard_still_blocks_bundled_bossctl` below.
#[test]
fn launch_guard_allows_bundled_cli_binaries() {
    for command in [
        "/Applications/Boss.app/Contents/Resources/bin/boss",
        "/Applications/Boss.app/Contents/Resources/bin/boss pr status --json",
        "/Applications/Boss.app/Contents/Resources/bin/boss-event",
        "exec /Applications/Boss.app/Contents/Resources/bin/boss pr status",
        // Dev / non-production bundle: same layout, different install root.
        "/Users/dev/Library/Developer/Xcode/DerivedData/Boss-abc/Build/Products/Debug/Boss.app/Contents/Resources/bin/boss",
        "/tmp/scratch/boss-app-run/Boss.app/Contents/Resources/bin/boss-event --help",
        // Variable-held path that expands to a CLI, not an app open.
        concat!(
            "BIN=/Applications/Boss.app/Contents/Resources/bin/boss\n",
            "\"$BIN\" pr status\n",
        ),
    ] {
        assert_eq!(launch_decision(command), "approve", "must allow: {command}");
    }
}

/// The engine binary also lives under `Contents/Resources/bin/` in the
/// installed layout. Basename `engine` still blocks it — the CLI
/// allowance must not silently over-broaden to production-engine
/// launches.
#[test]
fn launch_guard_still_blocks_bundled_engine_binary() {
    for command in [
        "/Applications/Boss.app/Contents/Resources/bin/engine",
        "exec /tmp/boss-app-run/Boss.app/Contents/Resources/bin/engine --socket-path /tmp/s.sock",
        "/Applications/Boss.app/Contents/Resources/bin/boss-engine",
    ] {
        assert_eq!(launch_decision(command), "block", "must block: {command}");
    }
}

/// `bossctl` is the coordinator's CLI surface and stays off the worker
/// surface entirely — coordinator-only per the worker CLAUDE.md text and
/// `boss_engine_worker_bin::launcher_names::writes_only_boss_and_never_bossctl`.
/// The bundled-CLI carve-out must not accidentally widen that: `bossctl`
/// shipping in the same `Contents/Resources/bin/` directory as the
/// allowed `boss` binary must still be blocked, by basename, regardless
/// of path.
#[test]
fn launch_guard_still_blocks_bundled_bossctl() {
    for command in [
        "/Applications/Boss.app/Contents/Resources/bin/bossctl",
        "/Applications/Boss.app/Contents/Resources/bin/bossctl status",
        "exec /tmp/boss-app-run/Boss.app/Contents/Resources/bin/bossctl status",
        "/Users/dev/Library/Developer/Xcode/DerivedData/Boss-abc/Build/Products/Debug/Boss.app/Contents/Resources/bin/bossctl status",
    ] {
        assert_eq!(launch_decision(command), "block", "must block: {command}");
    }
}

/// `boss engine start` / `boss engine stop` bounce the engine out from
/// under the worker. The static bare-name deny rules in
/// `worker_setup::deny_rules` only match the literal PATH-invocation
/// text (`Bash(boss engine stop:*)`), so once the bundled-CLI carve-out
/// makes the absolute-path shape reachable, the guard itself must close
/// that gap for both bundled and PATH invocations.
#[test]
fn launch_guard_still_blocks_boss_engine_start_stop() {
    for command in [
        "/Applications/Boss.app/Contents/Resources/bin/boss engine stop",
        "/Applications/Boss.app/Contents/Resources/bin/boss engine start",
        "exec /tmp/boss-app-run/Boss.app/Contents/Resources/bin/boss engine stop --force",
        "boss engine stop",
        "boss engine start",
    ] {
        assert_eq!(launch_decision(command), "block", "must block: {command}");
    }
}

/// `is_bundle_cli` must require the *normalized* parent directory to be
/// exactly `Contents/Resources/bin` — a raw substring test would let a
/// `..`-traversal path escape the carve-out even though it resolves
/// outside `bin/`.
#[test]
fn launch_guard_still_blocks_direct_boss_app_binary_via_traversal() {
    for command in [
        "/Applications/Boss.app/Contents/Resources/bin/../MacOS/Boss",
        "/Applications/Boss.app/Contents/Resources/bin/../MacOS/SomeHelper",
    ] {
        assert_eq!(launch_decision(command), "block", "must block: {command}");
    }
}

/// The tokenizer this guard shares with the other command-splitting guards
/// (`shell_command_tokenizer_fragment!`) must still close the two bypasses
/// the previous per-guard `shlex.split` tokenizer was rewritten to close:
/// an operator written with no surrounding spaces (`shlex.split` treats
/// `x&&bossctl stop` as a single opaque token) and a `#` inside a word
/// (the old `shlex.shlex` default `commenters='#'` silently truncated the
/// rest of the line at it). Both must still block after the rewrite, and
/// an operator-adjacent `#` that is unambiguously a comment-marker inside a
/// legitimate argument (a commit `--grep` value) must still approve, so the
/// `commenters=''` change cannot regress into over-blocking ordinary
/// arguments that merely contain a `#`.
#[test]
fn launch_guard_blocks_unspaced_and_commented_chains() {
    for command in [
        "x&&bossctl stop",
        "x&&open -a Boss",
        "echo hi&&open -a Boss",
        "echo a#b && bossctl stop",
    ] {
        assert_eq!(launch_decision(command), "block", "must block: {command}");
    }
    assert_eq!(
        launch_decision("git log --grep=x#123"),
        "approve",
        "a `#` inside an ordinary argument must not be treated as a comment or otherwise misparsed"
    );
}

/// Bare `boss` / `cube` is an untrusted PATH lookup: Codex's shell snapshot
/// demotes the launcher directory, and a hit on repobin silently bazel-builds
/// the CLI. The named `"$BOSS_BIN"` / `"$CUBE_BIN"` form is the contract.
#[test]
fn launch_guard_blocks_bare_engine_owned_path_lookups() {
    for command in [
        "boss pr status --json",
        "cube pr create --branch x",
        "boss propose blocked --reason x",
    ] {
        let (decision, reason) = run_launch_guard(command);
        assert_eq!(decision, "block", "must block bare PATH lookup: {command}");
        assert!(
            reason.contains("PATH lookup") || reason.contains("repobin"),
            "reason must name the PATH/repobin failure: {reason}"
        );
        assert!(
            reason.contains("BOSS_BIN") && reason.contains("CUBE_BIN") && reason.contains("CHECKLEFT_BIN"),
            "reason must tell the worker to name the env var: {reason}"
        );
    }
}

#[test]
fn launch_guard_blocks_bare_checkleft_only_when_worker_bin_pins_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let checkleft = tmp.path().join("checkleft");
    std::fs::write(&checkleft, b"#!/bin/sh\n").unwrap();
    assert_eq!(
        run_launch_guard_with_env("checkleft run", &[("CHECKLEFT_BIN", checkleft.to_str().unwrap())]).0,
        "block",
    );
    assert_eq!(launch_decision("checkleft run"), "approve");
}

#[test]
fn launch_guard_allows_workspace_bin_checkleft_linked_to_repobin_shim() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    let shim_dir = workspace.join("tools/repobin/shim");
    let bin = workspace.join("bin");
    std::fs::create_dir_all(&shim_dir).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let shim = shim_dir.join("repobin-shim.sh");
    std::fs::write(&shim, b"#!/bin/sh\n").unwrap();
    let checkleft = bin.join("checkleft");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&shim, &checkleft).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(&shim, &checkleft).unwrap();
    let (decision, reason) = run_launch_guard_with_env_in_dir(
        "bin/checkleft run",
        &[("CHECKLEFT_BIN", checkleft.to_str().unwrap())],
        Some(&workspace),
    );
    assert_eq!(decision, "approve", "workspace bin/checkleft must be allowed: {reason}");
}

/// The companion the prior review flagged as missing: the guard process's
/// own working directory is NOT the workspace root (e.g. Codex's own hook
/// adapter invoking this script from elsewhere), but the hook payload's
/// `cwd` field names the workspace. `is_workspace_checkleft` must resolve
/// `bin/checkleft` against that payload `cwd`, not `os.getcwd()`, or this
/// sanctioned invocation is wrongly blocked as an arbitrary repobin shim.
#[test]
fn launch_guard_allows_workspace_bin_checkleft_via_payload_cwd_when_process_cwd_differs() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    let elsewhere = tmp.path().join("elsewhere");
    let shim_dir = workspace.join("tools/repobin/shim");
    let bin = workspace.join("bin");
    std::fs::create_dir_all(&shim_dir).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    let shim = shim_dir.join("repobin-shim.sh");
    std::fs::write(&shim, b"#!/bin/sh\n").unwrap();
    let checkleft = bin.join("checkleft");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&shim, &checkleft).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(&shim, &checkleft).unwrap();
    let (decision, reason) = run_launch_guard_with_env_dir_and_payload_cwd(
        "bin/checkleft run",
        &[("CHECKLEFT_BIN", checkleft.to_str().unwrap())],
        Some(&elsewhere),
        Some(&workspace),
    );
    assert_eq!(
        decision, "approve",
        "workspace bin/checkleft must be allowed via payload cwd even when the guard process's \
         own cwd differs: {reason}"
    );
}

/// `"$BOSS_BIN"` / `"$CUBE_BIN"` pointing at a real non-shim binary is the
/// sanctioned form and must still be approved.
#[test]
fn launch_guard_allows_named_boss_bin_and_cube_bin() {
    let tmp = tempfile::TempDir::new().unwrap();
    let boss = tmp.path().join("boss");
    let cube = tmp.path().join("cube");
    std::fs::write(&boss, b"#!/bin/sh\n").unwrap();
    std::fs::write(&cube, b"#!/bin/sh\n").unwrap();
    let env = [
        ("BOSS_BIN", boss.to_str().unwrap()),
        ("CUBE_BIN", cube.to_str().unwrap()),
    ];
    for command in ["\"$BOSS_BIN\" pr status --json", "\"$CUBE_BIN\" pr create --branch x"] {
        let (decision, reason) = run_launch_guard_with_env(command, &env);
        assert_eq!(
            decision, "approve",
            "named binary must be allowed: {command} reason={reason}"
        );
    }
}

/// A `"$BOSS_BIN"` that itself resolves to repobin is still a shim and
/// must fail closed — never rewritten, never passed.
#[test]
fn launch_guard_blocks_named_bin_that_is_a_repobin_shim() {
    let tmp = tempfile::TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let repobin = bin.join("repobin");
    std::fs::write(&repobin, b"#!/bin/sh\n").unwrap();
    let boss = bin.join("boss");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repobin, &boss).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(&repobin, &boss).unwrap();
    let (decision, reason) = run_launch_guard_with_env(
        "\"$BOSS_BIN\" pr status --json",
        &[("BOSS_BIN", boss.to_str().unwrap())],
    );
    assert_eq!(decision, "block", "shim via BOSS_BIN must be blocked: {reason}");
    assert!(
        reason.contains("repobin") || reason.contains("shim"),
        "reason must name the shim: {reason}"
    );
}

#[test]
fn launch_guard_blocks_workspace_cube_linked_to_repobin_shim() {
    let tmp = tempfile::TempDir::new().unwrap();
    let shim_dir = tmp.path().join("tools/repobin/shim");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&shim_dir).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let shim = shim_dir.join("repobin-shim.sh");
    std::fs::write(&shim, b"#!/bin/sh\n").unwrap();
    let cube = bin.join("cube");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&shim, &cube).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(&shim, &cube).unwrap();
    let (decision, reason) = run_launch_guard_with_env(&format!("{} pr create --branch x", cube.display()), &[]);
    assert_eq!(decision, "block", "workspace shim must be blocked: {reason}");
    assert!(reason.contains("repobin shim"), "reason must name the shim: {reason}");
}

/// The Codex driver wraps every tool call as `/bin/zsh -lc '<payload>'`, so
/// the guard's WATCH check must peel that envelope rather than approving
/// because the program token is `/bin/zsh`, not the bare `cube`/`boss`
/// inside the payload.
#[test]
fn launch_guard_blocks_bare_cube_inside_a_zsh_lc_envelope() {
    let (decision, reason) = run_launch_guard("/bin/zsh -lc 'cube pr create --branch x'");
    assert_eq!(decision, "block", "must peel the zsh -lc envelope: reason={reason}");
    assert!(
        reason.contains("BOSS_BIN") && reason.contains("CUBE_BIN"),
        "reason must tell the worker to name the env var: {reason}"
    );
}

/// A heredoc PR body that mentions `cube pr create` in prose (as the
/// worker's own CLAUDE.md instructs it to write) must not be treated as an
/// invocation of that command — only the actual command lines (the
/// `cat > ... << 'PRBODY'` opener) are real commands here.
#[test]
fn launch_guard_allows_heredoc_body_mentioning_cube_pr_create() {
    let command = "body=$(mktemp)\ncat > \"$body\" << 'PRBODY'\n## Summary\nUse `cube pr create --branch x` to open a PR.\nPRBODY\n";
    let (decision, reason) = run_launch_guard(command);
    assert_eq!(
        decision, "approve",
        "heredoc body must not be scanned as commands: {reason}"
    );
}

/// The reason has to hand the worker the supported commands; a refusal
/// with no alternative produces a worker that finds its own.
#[test]
fn launch_guard_reason_names_the_isolated_alternative() {
    let (decision, reason) = run_launch_guard("open -a Boss");
    assert_eq!(decision, "block");
    for expected in [
        "--socket-path",
        "//tools/boss/engine/core:engine",
        "BOSS_EVENTS_SOCKET",
        "bazel build",
        "bazel test",
        "--capture-to",
        "BOSS_SOCKET_PATH",
        "BOSS_ENGINE_AUTOSTART=0",
        "//tools/boss/app-macos:Boss",
    ] {
        assert!(reason.contains(expected), "reason must mention {expected}: {reason}");
    }
}
