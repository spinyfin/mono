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
