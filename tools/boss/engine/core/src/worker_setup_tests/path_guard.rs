use super::super::*;
use super::helpers::*;

/// Locate the deterministic path-guard PreToolUse hook command (the
/// one that invokes the gate script), if present.
fn path_guard_command(parsed: &serde_json::Value) -> Option<String> {
    parsed["hooks"]["PreToolUse"]
        .as_array()?
        .iter()
        .filter_map(|e| e["hooks"][0]["command"].as_str())
        .find(|c| c.contains(PATH_GUARD_SCRIPT_NAME))
        .map(str::to_owned)
}

#[test]
fn settings_json_adds_deterministic_path_guard_hook() {
    // Every session must carry the deterministic Boss-data-dir gate
    // as a PreToolUse hook. The hook invokes the gate script with the
    // Boss data dir passed via BOSS_DATA_DIR so the script resolves
    // candidate paths against the right boundary.
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input, &ClaudeDriver)).unwrap();
    let cmd = path_guard_command(&parsed).expect("PreToolUse must include the deterministic path-guard hook");
    assert!(cmd.contains("python3"), "guard must run via python3: {cmd}");
    // The data dir is the Boss state dir (events socket parent),
    // single-quoted because of the space in "Application Support".
    assert!(
        cmd.contains("BOSS_DATA_DIR='/Users/brianduff/Library/Application Support/Boss'"),
        "guard must pass the Boss data dir via BOSS_DATA_DIR: {cmd}",
    );
    // The script path lives outside any workspace, in the shared
    // worker-settings dir.
    let script = path_guard_script_path();
    assert!(
        cmd.contains(&shell_quote(&script.display().to_string())),
        "guard must invoke the absolute gate-script path: {cmd}",
    );
}

#[test]
fn path_guard_present_for_revision_sessions_too() {
    // The gate is session-kind-agnostic: revision sessions get it
    // alongside their gh-pr-create guard.
    let mut input = sample_input();
    input.execution_kind = "revision_implementation".into();
    input.task_kind = Some("revision".into());
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input, &ClaudeDriver)).unwrap();
    assert!(
        path_guard_command(&parsed).is_some(),
        "revision sessions must also carry the deterministic path guard",
    );
}

#[test]
fn path_guard_script_has_the_load_bearing_logic() {
    // Guard against an accidental edit that guts the script. The
    // deterministic gate hinges on: reading BOSS_DATA_DIR, resolving
    // symlinks/.. via realpath, a component-wise prefix test, emitting
    // a block decision, and pointing at the sanctioned recovery path.
    let s = PATH_GUARD_SCRIPT;
    assert!(s.contains("BOSS_DATA_DIR"), "must read the data dir from env");
    assert!(s.contains("realpath"), "must canonicalise paths via realpath");
    assert!(
        s.contains("expanduser") && s.contains("expandvars"),
        "must expand ~ and $VAR indirection"
    );
    assert!(
        s.contains("\"decision\"") && s.contains("\"block\""),
        "must be able to emit a block decision"
    );
    assert!(
        s.contains("boss task restore") || s.contains("boss shake"),
        "block message must point at the sanctioned recovery surface"
    );
}

#[test]
fn write_workspace_files_writes_path_guard_script_outside_workspace() {
    let _shared = lock_shared_settings_dir();
    let _home = HomeGuard::new();
    let dir = TempDir::new().unwrap();
    let input = WorkerSetupInput {
        run_id: "run-guard".into(),
        lease_id: "lease-guard".into(),
        workspace_path: dir.path().to_path_buf(),
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        boss_event_path: PathBuf::from("/tmp/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
    };
    write_workspace_files(&input, &ClaudeDriver).unwrap();

    let script = path_guard_script_path();
    assert!(script.exists(), "gate script must be written: {}", script.display());
    // Must live outside the workspace tree (same rule as the
    // settings file — never shipped into a worker PR).
    assert!(
        !script.starts_with(dir.path()),
        "gate script must live outside the workspace: {}",
        script.display(),
    );
    let body = std::fs::read_to_string(&script).unwrap();
    assert_eq!(body, PATH_GUARD_SCRIPT, "written script must match the source");
    // And the engine must never drop the gate script into the
    // workspace's .claude/ where VCS could pick it up.
    assert!(
        !dir.path().join(".claude").join(PATH_GUARD_SCRIPT_NAME).exists(),
        "gate script must not be written into the workspace .claude/ dir",
    );
}

#[test]
fn heal_worker_settings_json_refreshes_path_guard_script() {
    // On engine restart the heal sweep must (re)materialise the gate
    // script so a live worker whose settings reference it still has a
    // working PreToolUse gate even after TMPDIR churn.
    let settings_dir = TempDir::new().unwrap();
    // A settings file must exist for the dir to be considered live.
    std::fs::write(settings_dir.path().join("ws.json"), "{}").unwrap();

    heal_worker_settings_json(settings_dir.path(), &PathBuf::from("/stable/boss-event"));

    let script = settings_dir.path().join(PATH_GUARD_SCRIPT_NAME);
    assert!(script.exists(), "heal must refresh the gate script");
    assert_eq!(std::fs::read_to_string(&script).unwrap(), PATH_GUARD_SCRIPT);
}

// ── PATH_GUARD_SCRIPT execution tests ─────────────────────────────────
//
// These run the actual gate script against simulated PreToolUse payloads,
// with BOSS_DATA_DIR pointed at a temp directory so no test ever needs the
// real Boss data dir on the host. The Codex cases matter because Codex's
// file-edit tool is `apply_patch`, whose payload carries the whole patch
// body in `tool_input.command` and no `file_path` key at all — before the
// script learned that shape every Codex file edit was approved unread.

/// Run the gate against `payload` with `data_dir` as the boundary and return
/// `(decision, reason)`.
fn run_path_guard(data_dir: &std::path::Path, payload: serde_json::Value) -> (String, String) {
    run_path_guard_raw(data_dir, &payload.to_string())
}

/// As [`run_path_guard`], but writes `stdin` verbatim — so a payload that is
/// not JSON at all can be exercised.
fn run_path_guard_raw(data_dir: &std::path::Path, stdin: &str) -> (String, String) {
    use std::io::Write as _;
    let script_dir = TempDir::new().unwrap();
    let script = script_dir.path().join(PATH_GUARD_SCRIPT_NAME);
    std::fs::write(&script, PATH_GUARD_SCRIPT).unwrap();

    let mut child = std::process::Command::new("python3")
        .arg(&script)
        .env("BOSS_DATA_DIR", data_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("python3 must be available");
    child.stdin.as_mut().unwrap().write_all(stdin.as_bytes()).unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
        panic!(
            "path guard produced invalid JSON: {err}\nstdout={stdout}\nstderr={}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (
        parsed["decision"].as_str().unwrap_or("missing").to_owned(),
        parsed["reason"].as_str().unwrap_or_default().to_owned(),
    )
}

#[test]
fn path_guard_blocks_a_relative_read_whose_cwd_is_the_data_dir() {
    // This is the case the data-dir fence now leans on the hook for.
    //
    // The `Read(<data dir>/**)` deny globs were removed from the worker
    // settings (any `Read()` deny rule arms Claude Code's "compound command
    // contains `cd` with a relative file read" permission prompt and stalls
    // an unattended worker). Those globs never covered this shape anyway: a
    // literal-path glob cannot match `state.db` — only the hook can, because
    // it canonicalises the candidate path against the call's own `cwd`.
    let data = TempDir::new().unwrap();
    std::fs::write(data.path().join("state.db"), b"x").unwrap();

    let (decision, reason) = run_path_guard(
        data.path(),
        serde_json::json!({
            "tool_name": "Read",
            "tool_input": {"file_path": "state.db"},
            "cwd": data.path().display().to_string(),
        }),
    );
    assert_eq!(
        decision, "block",
        "a Read of a relative path resolving into the data dir must be blocked: {reason}",
    );
    assert!(reason.contains("engine-owned"), "{reason}");

    // Same shape one level deeper, and via `..` — canonicalisation, not
    // string matching, is what makes the boundary hold.
    let nested = data.path().join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let (decision, reason) = run_path_guard(
        data.path(),
        serde_json::json!({
            "tool_name": "Read",
            "tool_input": {"file_path": "../state.db"},
            "cwd": nested.display().to_string(),
        }),
    );
    assert_eq!(
        decision, "block",
        "a `..` escape back into the data dir must be blocked"
    );
    assert!(reason.contains("engine-owned"), "{reason}");
}

#[test]
fn path_guard_approves_a_relative_read_outside_the_data_dir() {
    // The belt above must not become a blanket refusal of relative reads:
    // an ordinary relative `Read` inside the workspace still approves.
    let data = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let (decision, reason) = run_path_guard(
        data.path(),
        serde_json::json!({
            "tool_name": "Read",
            "tool_input": {"file_path": "src/main.rs"},
            "cwd": workspace.path().display().to_string(),
        }),
    );
    assert_eq!(
        decision, "approve",
        "ordinary relative reads must not be disturbed: {reason}"
    );
}

#[test]
fn path_guard_blocks_a_bash_cd_then_relative_read_of_the_data_dir() {
    // The compound shape Claude Code's own classifier reacts to: `cd <data
    // dir> && cat state.db`. The literal-path deny globs could never see the
    // relative `state.db`; the hook resolves the `cd` target and blocks.
    let data = TempDir::new().unwrap();
    let command = format!("cd {} && cat state.db", data.path().display());
    let (decision, reason) = run_path_guard(
        data.path(),
        serde_json::json!({"tool_name": "Bash", "tool_input": {"command": command}}),
    );
    assert_eq!(
        decision, "block",
        "cd-into-data-dir then read must be blocked: {reason}"
    );
    assert!(reason.contains("engine-owned"), "{reason}");
}

#[test]
fn path_guard_blocks_apply_patch_writing_into_the_data_dir() {
    let data = TempDir::new().unwrap();
    let target = data.path().join("state.db");
    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n+tampered\n*** End Patch",
        target.display()
    );
    let (decision, reason) = run_path_guard(
        data.path(),
        serde_json::json!({"tool_name": "apply_patch", "tool_input": {"command": patch}}),
    );
    assert_eq!(decision, "block", "apply_patch into the data dir must be blocked");
    assert!(reason.contains("engine-owned"), "{reason}");
}

#[test]
fn path_guard_blocks_apply_patch_add_and_move_headers() {
    let data = TempDir::new().unwrap();
    for header in ["*** Add File:", "*** Delete File:", "*** Move to:"] {
        let patch = format!(
            "*** Begin Patch\n{header} {}/leak.txt\n+x\n*** End Patch",
            data.path().display()
        );
        let (decision, _) = run_path_guard(
            data.path(),
            serde_json::json!({"tool_name": "apply_patch", "tool_input": {"command": patch}}),
        );
        assert_eq!(decision, "block", "{header} must be read for its target path");
    }
}

#[test]
fn path_guard_approves_apply_patch_outside_the_data_dir() {
    let data = TempDir::new().unwrap();
    let patch = "*** Begin Patch\n*** Add File: src/main.rs\n+fn main() {}\n*** End Patch";
    let (decision, reason) = run_path_guard(
        data.path(),
        serde_json::json!({"tool_name": "apply_patch", "tool_input": {"command": patch}}),
    );
    assert_eq!(decision, "approve", "ordinary edits must not be disturbed: {reason}");
}

#[test]
fn path_guard_fails_closed_on_an_unreadable_payload_for_a_tool_it_reads() {
    // The hazard this exists for: a Codex payload shape Boss did not
    // anticipate must block, not fall through to approve.
    let data = TempDir::new().unwrap();
    for payload in [
        serde_json::json!({"tool_name": "Bash", "tool_input": "const r = await tools.exec_command({})"}),
        serde_json::json!({"tool_name": "apply_patch", "tool_input": {"command": 42}}),
        serde_json::json!({"tool_name": "Bash", "tool_input": {}}),
    ] {
        let (decision, reason) = run_path_guard(data.path(), payload.clone());
        assert_eq!(decision, "block", "must fail closed for {payload}");
        assert!(reason.contains("fail-closed"), "{reason}");
    }

    // A payload the gate cannot parse at all is the same hazard one level up:
    // it cannot read the tool name, so it is in no position to conclude it has
    // nothing to say about this call.
    let (decision, reason) = run_path_guard_raw(data.path(), "const r = await tools.exec_command({})");
    assert_eq!(decision, "block", "non-JSON hook stdin must fail closed: {reason}");
    assert!(
        reason.contains("fail-closed") && reason.contains("not JSON"),
        "{reason}"
    );

    let (decision, reason) = run_path_guard(data.path(), serde_json::json!([{"tool_name": "Bash"}]));
    assert_eq!(decision, "block", "a non-object payload must fail closed: {reason}");
    assert!(
        reason.contains("fail-closed") && reason.contains("not a JSON object"),
        "{reason}"
    );
}

#[test]
fn path_guard_with_no_data_dir_configured_approves() {
    // BOSS_DATA_DIR unset is Boss's own configuration (a remote worker, where
    // the gate is deliberately not armed), not an agent payload the gate failed
    // to read — so it stays an approve while the payload cases above block.
    use std::io::Write as _;
    let script_dir = TempDir::new().unwrap();
    let script = script_dir.path().join(PATH_GUARD_SCRIPT_NAME);
    std::fs::write(&script, PATH_GUARD_SCRIPT).unwrap();
    let mut child = std::process::Command::new("python3")
        .arg(&script)
        .env_remove("BOSS_DATA_DIR")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("python3 must be available");
    child.stdin.as_mut().unwrap().write_all(b"not json at all").unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(parsed["decision"], "approve");
}

#[test]
fn path_guard_still_approves_tools_it_has_nothing_to_say_about() {
    // Armed with `.*` on the Codex path, the gate sees every tool. Silence
    // for a tool with no candidate path is correct — that is not the same as
    // failing open on a payload it should have read.
    let data = TempDir::new().unwrap();
    for tool in ["view_image", "update_plan", "web__run"] {
        let (decision, _) = run_path_guard(data.path(), serde_json::json!({"tool_name": tool, "tool_input": {}}));
        assert_eq!(decision, "approve", "{tool} must be approved");
    }
}

// ── Broad-traversal boundary ──────────────────────────────────────────
//
// The second boundary the gate enforces: a tool call may not *start* a
// recursive walk rooted at the whole machine. This is what let a worker's
// claude process walk `/` and read into ~/Desktop, ~/Documents, ~/Pictures
// and three other users' home directories in one run — raising a macOS
// privacy prompt attributed to Boss for each protected category. Evidence:
// tools/boss/docs/investigations/worker-filesystem-traversal-tcc-prompts-2026-08-19.md
//
// `cwd` is passed explicitly so relative roots resolve against a known
// directory rather than whatever cwd the test binary inherited.

/// Run the gate with an explicit `cwd` in the payload.
fn run_guard_in(data_dir: &std::path::Path, cwd: &std::path::Path, mut payload: serde_json::Value) -> (String, String) {
    payload["cwd"] = serde_json::json!(cwd.display().to_string());
    run_path_guard(data_dir, payload)
}

fn bash(command: &str) -> serde_json::Value {
    serde_json::json!({"tool_name": "Bash", "tool_input": {"command": command}})
}

#[test]
fn path_guard_blocks_a_machine_wide_recursive_walk() {
    let data = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    for command in [
        // The exact shape observed in the incident.
        "find / -name foo",
        "du -sh /",
        "rg needle /Users",
        "grep -rn needle /Volumes",
        "ls -R /System",
        "find /Library -name x",
        // autofs maps: descending these mounts a network volume on demand,
        // which is what raised the "access files on a network volume" prompt.
        "find /net -name x",
        "find /home -name x",
        // A sibling user's home is as far out of scope as the operator's.
        "find /Users/someone-else -name x",
        // The root can arrive via a `cd` earlier in the same command line.
        "cd / && find . -name x",
        "cd /;find . -name x",
        "(cd / && find . -name x)",
        "{ cd / ; find . -name x ; }",
        // POSIX -H/-L/-P precede path operands and must not hide the root.
        "find -L / -name x",
        "find -H /Users -name x",
        // A shell glob still denotes every user's protected Desktop.
        "find /Users/*/Desktop -name x",
        // Clustered short flags request recursion just as `-r` does.
        "grep -rn needle /Users",
        // Pattern-supplying flags leave no positional pattern; the next
        // non-flag is a real traversal root, not a regex to skip.
        "rg -e needle /",
        "grep -r -e needle /",
        "rg --regexp=needle /",
        "rg -f patterns.txt /",
        // Metadata searches are global unless mdfind receives a narrow root.
        "mdfind 'kMDItemDisplayName == foo'",
        "locate foo",
        // Whole-tree archivers must judge their input tree, not their output.
        "tar -cf out.tar /",
        "zip -r out.zip /",
        "ditto / /tmp/copy",
    ] {
        let (decision, reason) = run_guard_in(data.path(), cwd.path(), bash(command));
        assert_eq!(decision, "block", "must block a machine-wide walk: {command}");
        assert!(
            reason.contains("recursive directory walk"),
            "block reason must name the hazard for {command}: {reason}",
        );
    }
}

#[test]
fn path_guard_blocks_broad_roots_in_their_resolved_spelling_too() {
    // A root must be recognised in either spelling, or the boundary is one
    // symlink away from being bypassed:
    //
    //  - `/home` is an autofs map; realpath() turns it into
    //    `/System/Volumes/Data/home`, which is not literally in the broad-root
    //    set. Descending it is what mounts a network volume on demand, so this
    //    is precisely the case behind the "network volume" prompt.
    //  - macOS resolves user paths through the data-volume firmlink, so
    //    `/Users/x` also arrives as `/System/Volumes/Data/Users/x`.
    let data = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    for command in [
        "find /home -name x",
        "find /System/Volumes/Data/home -name x",
        "rg needle /System/Volumes/Data/Users/someone-else",
        "du -sh /System/Volumes/Data",
    ] {
        let (decision, reason) = run_guard_in(data.path(), cwd.path(), bash(command));
        assert_eq!(decision, "block", "must block either spelling: {command} → {reason}");
    }
}

#[test]
fn path_guard_blocks_broad_roots_from_glob_and_grep_tools() {
    // Claude Code's Glob runs in-process — no Bash command to tokenise — so
    // the root arrives as `path`, or as the literal prefix of an absolute
    // `pattern`. Both must be judged, or the boundary only covers shell-outs.
    let data = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    for payload in [
        serde_json::json!({"tool_name": "Glob", "tool_input": {"path": "/", "pattern": "**/*.rs"}}),
        serde_json::json!({"tool_name": "Glob", "tool_input": {"pattern": "/**/*.rs"}}),
        serde_json::json!({"tool_name": "Grep", "tool_input": {"pattern": "x", "path": "/Users/someone-else"}}),
        serde_json::json!({"tool_name": "Grep", "tool_input": {"pattern": "x", "path": "/Volumes"}}),
    ] {
        let (decision, reason) = run_guard_in(data.path(), cwd.path(), payload.clone());
        assert_eq!(decision, "block", "must block a broad root for {payload}");
        assert!(reason.contains("recursive directory walk"), "{reason}");
    }
}

#[test]
fn path_guard_approves_scoped_searches_and_specific_external_files() {
    // The defect is the breadth of the traversal, not the fact that a path is
    // outside the workspace. A scoped search, and a read of one named file
    // outside the workspace, must both stay untouched — narrowing the scan is
    // the fix; fencing every external read would break ordinary worker work.
    let data = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let scoped = cwd.path().join("src");
    std::fs::create_dir_all(&scoped).unwrap();
    for payload in [
        bash("find . -name '*.rs'"),
        bash("rg needle src/"),
        // The first non-flag grep-family operand is a regex pattern, not a
        // traversal root, and `find` expressions carry values not paths.
        bash("rg '/Users' tools/boss"),
        bash("rg -e '/Users' tools/boss"),
        bash("rg -A 3 '/Users' tools/boss"),
        bash("rg --glob '*.rs' '/System' tools"),
        bash("find tools -name '*.rs' -newer /Users"),
        bash(&format!("rg needle {}", scoped.display())),
        // A cargo/bazel cache under the home directory: a real root, but a
        // named one, not the home directory itself.
        bash("find ~/.cargo/registry -name x"),
        // Not recursive: listing a directory is not walking its tree.
        bash("ls /"),
        // Changing directories alone is not a traversal.
        bash("cd /; echo scoped-work"),
        bash("echo /"),
        bash("cat /etc/hosts"),
        bash("bazel test //tools/boss/engine/..."),
        // `git -C <home>` is not a traversal program at all.
        bash("git -C /Users/someone-else status"),
        bash("mdfind -onlyin tools/boss 'kMDItemDisplayName == foo'"),
        serde_json::json!({"tool_name": "Read", "tool_input": {"file_path": "/etc/hosts"}}),
        serde_json::json!({"tool_name": "Glob", "tool_input": {"pattern": "**/*.rs"}}),
        // Grep/Search patterns are regexes; only their `path` is a root.
        serde_json::json!({"tool_name": "Grep", "tool_input": {"pattern": "/Users/brianduff", "path": "tools/boss"}}),
    ] {
        let (decision, reason) = run_guard_in(data.path(), cwd.path(), payload.clone());
        assert_eq!(
            decision, "approve",
            "must not disturb scoped work: {payload} → {reason}"
        );
    }
}

#[test]
fn path_guard_script_has_the_traversal_boundary_logic() {
    // Guard against an accidental edit that guts the second boundary, in the
    // same shape as the data-dir assertions above.
    let s = PATH_GUARD_SCRIPT;
    assert!(s.contains("STATIC_BROAD_ROOTS"), "must define the broad-root set");
    assert!(
        s.contains("ALWAYS_RECURSIVE") && s.contains("RECURSIVE_WITH_FLAG"),
        "must know which programs recurse",
    );
    assert!(
        s.contains("\"/net\"") && s.contains("\"/home\""),
        "autofs maps must be broad roots — descending them mounts a network volume",
    );
    assert!(
        s.contains("is_broad_root") && s.contains("traversal_roots"),
        "must judge the root of a recursive walk",
    );
    assert!(
        s.contains("TRAVERSAL_RECOVERY"),
        "must explain how to re-root the search",
    );
}
