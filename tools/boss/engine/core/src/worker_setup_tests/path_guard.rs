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
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
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
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
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
