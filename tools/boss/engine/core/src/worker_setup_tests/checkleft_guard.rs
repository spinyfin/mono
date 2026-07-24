use super::super::*;
use super::helpers::*;

// ── checkleft pre-push guard ──────────────────────────────────────────

/// Locate the checkleft pre-push guard PreToolUse hook command (the
/// entry that invokes the push-guard script by its filename).
fn checkleft_push_guard_command(parsed: &serde_json::Value) -> Option<String> {
    parsed["hooks"]["PreToolUse"]
        .as_array()?
        .iter()
        .filter_map(|e| e["hooks"][0]["command"].as_str())
        .find(|c| c.contains(CHECKLEFT_PUSH_GUARD_SCRIPT_NAME))
        .map(str::to_owned)
}

#[test]
fn standard_worker_gets_checkleft_push_guard() {
    // A standard (implementation) worker must carry the deterministic
    // pre-push checkleft gate as a Bash-matched PreToolUse hook.
    let input = sample_input(); // Standard chore worker
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let cmd = checkleft_push_guard_command(&parsed)
        .expect("standard worker PreToolUse must include the checkleft push guard");
    assert!(cmd.contains("python3"), "guard must run via python3: {cmd}");
    let script = checkleft_push_guard_script_path();
    assert!(
        cmd.contains(&shell_quote(&script.display().to_string())),
        "guard must invoke the absolute push-guard script path: {cmd}",
    );
    // The guard is Bash-matched (it inspects the command string).
    let entry = parsed["hooks"]["PreToolUse"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            e["hooks"][0]["command"]
                .as_str()
                .unwrap_or("")
                .contains(CHECKLEFT_PUSH_GUARD_SCRIPT_NAME)
        })
        .unwrap();
    assert_eq!(entry["matcher"], "Bash", "push guard must match the Bash tool");
}

#[test]
fn reviewer_and_triage_workers_omit_checkleft_push_guard() {
    // Reviewer / triage workers cannot push (their deny rules block it),
    // so the push guard would never fire — it must be omitted.
    for kind in [WorkerKind::Reviewer, WorkerKind::Triage] {
        let mut input = sample_input();
        input.worker_kind = kind.clone();
        let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
        assert!(
            checkleft_push_guard_command(&parsed).is_none(),
            "{kind:?} worker must not carry the checkleft push guard",
        );
    }
}

#[test]
fn remote_workers_omit_checkleft_push_guard() {
    // Remote SSH workers skip the push guard: the gate script is never
    // shipped to the remote host (same reason the path guard is dropped).
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_remote_settings_json(&input)).unwrap();
    assert!(
        checkleft_push_guard_command(&parsed).is_none(),
        "remote workers must not carry the checkleft push guard",
    );
}

#[test]
fn checkleft_push_guard_script_has_the_load_bearing_logic() {
    // Guard against an accidental edit that guts the script. The gate
    // hinges on: detecting a push command, resolving checkleft (env
    // override → bin/checkleft → PATH), running `checkleft run`, gating
    // on the exit code, and surfacing the BYPASS_ guidance on a block.
    let s = CHECKLEFT_PUSH_GUARD_SCRIPT;
    assert!(s.contains("is_push_command"), "must detect push commands");
    assert!(s.contains("BOSS_CHECKLEFT_BIN"), "must honour the binary override");
    assert!(
        s.contains("bin") && s.contains("checkleft"),
        "must resolve the repobin-installed checkleft path",
    );
    assert!(s.contains("returncode"), "must gate on checkleft's exit code");
    assert!(
        s.contains("\"decision\"") && s.contains("\"block\"") || s.contains("'block'"),
        "must be able to emit a block decision",
    );
    assert!(s.contains("BYPASS_"), "block message must surface the bypass guidance");
}

#[test]
fn write_workspace_files_writes_checkleft_push_guard_script_outside_workspace() {
    let _shared = lock_shared_settings_dir();
    let _home = HomeGuard::new();
    let dir = TempDir::new().unwrap();
    let input = WorkerSetupInput {
        run_id: "run-clguard".into(),
        lease_id: "lease-clguard".into(),
        workspace_path: dir.path().to_path_buf(),
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        boss_event_path: PathBuf::from("/tmp/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
    };
    write_workspace_files(&input, &ClaudeDriver).unwrap();

    let script = checkleft_push_guard_script_path();
    assert!(
        script.exists(),
        "push-guard script must be written: {}",
        script.display()
    );
    assert!(
        !script.starts_with(dir.path()),
        "push-guard script must live outside the workspace: {}",
        script.display(),
    );
    let body = std::fs::read_to_string(&script).unwrap();
    assert_eq!(
        body, CHECKLEFT_PUSH_GUARD_SCRIPT,
        "written script must match the source"
    );
    assert!(
        !dir.path()
            .join(".claude")
            .join(CHECKLEFT_PUSH_GUARD_SCRIPT_NAME)
            .exists(),
        "push-guard script must not be written into the workspace .claude/ dir",
    );
}

#[test]
fn heal_worker_settings_json_refreshes_checkleft_push_guard_script() {
    let settings_dir = TempDir::new().unwrap();
    std::fs::write(settings_dir.path().join("ws.json"), "{}").unwrap();

    heal_worker_settings_json(settings_dir.path(), &PathBuf::from("/stable/boss-event"));

    let script = settings_dir.path().join(CHECKLEFT_PUSH_GUARD_SCRIPT_NAME);
    assert!(script.exists(), "heal must refresh the push-guard script");
    assert_eq!(std::fs::read_to_string(&script).unwrap(), CHECKLEFT_PUSH_GUARD_SCRIPT);
}

// ── checkleft pre-push guard execution tests ──────────────────────────
//
// These run the actual guard script (via python3) against a simulated
// Bash tool_input payload, using a fake checkleft binary so the gate's
// block/approve behaviour is verified end-to-end and deterministically.

/// Write an executable fake `checkleft` that prints `stdout` and exits
/// with `exit_code`. Returns its path.
fn write_fake_checkleft(dir: &Path, name: &str, exit_code: i32, stdout: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    let script = format!("#!/bin/sh\ncat <<'CHECKLEFT_EOF'\n{stdout}\nCHECKLEFT_EOF\nexit {exit_code}\n");
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// Write a fake checkleft that emits `stderr_msg` on stderr only (empty
/// stdout) and exits with `exit_code`. Models a parser/internal crash.
fn write_fake_checkleft_stderr_only(dir: &Path, name: &str, exit_code: i32, stderr_msg: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    let script = format!("#!/bin/sh\necho '{stderr_msg}' >&2\nexit {exit_code}\n");
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// Run the checkleft push guard against a simulated Bash command and
/// return the decision JSON. `checkleft_bin` is passed via
/// `BOSS_CHECKLEFT_BIN` (a nonexistent path simulates "no checkleft").
fn run_push_guard(command: &str, cwd: &Path, checkleft_bin: &Path) -> serde_json::Value {
    use std::io::Write as _;
    let script_dir = TempDir::new().unwrap();
    let script_path = script_dir.path().join(CHECKLEFT_PUSH_GUARD_SCRIPT_NAME);
    std::fs::write(&script_path, CHECKLEFT_PUSH_GUARD_SCRIPT).unwrap();

    let payload = serde_json::json!({
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": cwd.display().to_string(),
    })
    .to_string();

    let mut child = std::process::Command::new("python3")
        .arg(&script_path)
        .env("BOSS_CHECKLEFT_BIN", checkleft_bin)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("python3 must be available");
    child.stdin.as_mut().unwrap().write_all(payload.as_bytes()).unwrap();
    drop(child.stdin.take());

    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "push guard produced invalid JSON for {command:?}: {e}\nstdout={stdout}\nstderr={}",
            String::from_utf8_lossy(&out.stderr),
        )
    })
}

#[test]
fn push_guard_blocks_jj_git_push_when_checkleft_fails() {
    let dir = TempDir::new().unwrap();
    let checkleft = write_fake_checkleft(dir.path(), "checkleft", 1, "error[rustfmt]: needs formatting");
    let decision = run_push_guard("jj git push -b boss/foo --allow-new", dir.path(), &checkleft);
    assert_eq!(
        decision["decision"], "block",
        "a failing checkleft must block the push: {decision}"
    );
    let reason = decision["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("error[rustfmt]"),
        "block reason must echo the findings: {reason}"
    );
    assert!(
        reason.contains("BYPASS_"),
        "block reason must include bypass guidance: {reason}"
    );
}

#[test]
fn push_guard_blocks_git_push_when_checkleft_fails() {
    let dir = TempDir::new().unwrap();
    let checkleft = write_fake_checkleft(dir.path(), "checkleft", 1, "error[clippy]: bad");
    let decision = run_push_guard("git push --force-with-lease github my-branch", dir.path(), &checkleft);
    assert_eq!(
        decision["decision"], "block",
        "git push with a failing checkleft must block: {decision}"
    );
}

#[test]
fn push_guard_allows_push_when_checkleft_passes() {
    let dir = TempDir::new().unwrap();
    let checkleft = write_fake_checkleft(dir.path(), "checkleft", 0, "checks: no findings");
    let decision = run_push_guard("jj git push -b boss/foo", dir.path(), &checkleft);
    assert_eq!(
        decision["decision"], "approve",
        "a clean checkleft must allow the push: {decision}"
    );
}

#[test]
fn push_guard_approves_non_push_command_without_running_checkleft() {
    let dir = TempDir::new().unwrap();
    // checkleft would fail if invoked — but a non-push command must never
    // invoke it, so the decision is approve.
    let checkleft = write_fake_checkleft(dir.path(), "checkleft", 1, "error: would block");
    let decision = run_push_guard("jj describe -m 'wip'", dir.path(), &checkleft);
    assert_eq!(
        decision["decision"], "approve",
        "non-push command must approve: {decision}"
    );
}

#[test]
fn push_guard_approves_describe_with_push_phrase_in_message() {
    let dir = TempDir::new().unwrap();
    let checkleft = write_fake_checkleft(dir.path(), "checkleft", 1, "error: would block");
    // "git push" is inside the quoted commit message — shlex keeps it as a
    // single token, so it must NOT be treated as a push.
    let decision = run_push_guard(r#"jj describe -m "git push the fix to prod""#, dir.path(), &checkleft);
    assert_eq!(
        decision["decision"], "approve",
        "push phrase in a commit message must not block: {decision}"
    );
}

#[test]
fn push_guard_approves_when_no_checkleft_binary() {
    let dir = TempDir::new().unwrap();
    // A nonexistent override path → resolve returns None → fail open.
    let missing = dir.path().join("does-not-exist-checkleft");
    let decision = run_push_guard("jj git push -b boss/foo --allow-new", dir.path(), &missing);
    assert_eq!(
        decision["decision"], "approve",
        "a repo without a checkleft binary must allow the push (fail open): {decision}",
    );
}

#[test]
fn push_guard_reports_internal_error_when_only_stderr() {
    // When checkleft exits non-zero with nothing on stdout but an error on
    // stderr (e.g. a parser crash on an unknown VCS status code), the guard
    // must use the "internal error" message rather than "errors that must be
    // fixed". This prevents workers from thinking they have policy violations
    // to fix or from reaching for BYPASS_ directives unnecessarily.
    let dir = TempDir::new().unwrap();
    let checkleft = write_fake_checkleft_stderr_only(
        dir.path(),
        "checkleft",
        1,
        "error: unsupported jj diff summary line: X some/file.rs",
    );
    let decision = run_push_guard("jj git push -b boss/foo --allow-new", dir.path(), &checkleft);
    assert_eq!(
        decision["decision"], "block",
        "a crashing checkleft must still block the push: {decision}",
    );
    let reason = decision["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("internal error"),
        "block reason must say 'internal error', not 'errors that must be fixed': {reason}",
    );
    assert!(
        !reason.contains("BYPASS_"),
        "internal error message must NOT include bypass guidance: {reason}",
    );
    assert!(
        reason.contains("unsupported jj diff summary line"),
        "block reason must include the stderr detail: {reason}",
    );
}
