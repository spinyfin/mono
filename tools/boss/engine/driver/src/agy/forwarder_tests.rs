use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;
use tempfile::TempDir;

use super::*;
use crate::agy::home::provision_agy_home;
use crate::agy::test_environment::env_for_provision;
use crate::{AgentDriver, PermissionInput, WorkerKind};

fn config(forwarder: &Path) -> ProgressObservationConfig {
    ProgressObservationConfig {
        events_socket_path: PathBuf::from("/tmp/boss test/events.sock"),
        lease_id: "lease-1".into(),
        run_id: "run-1".into(),
        workspace_path: PathBuf::from("/tmp/ws one"),
        forwarder_binary: forwarder.to_path_buf(),
    }
}

/// A stand-in for the `boss-event` shim that records what it was given next
/// to itself and exits with the status in `exit-status` (default 0).
fn stub_forwarder(dir: &Path) -> PathBuf {
    let path = dir.join("boss event stub");
    fs::write(
        &path,
        r#"#!/bin/sh
here=$(dirname "$0")
printf '%s\n' "$BOSS_EVENTS_SOCKET" "$BOSS_LEASE_ID" "$BOSS_RUN_ID" "$BOSS_WORKSPACE" "$#" "$@" > "$here/args.txt"
cat > "$here/payload.bin"
exit $(cat "$here/exit-status" 2>/dev/null || echo 0)
"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn run_hook_command(command: &str, payload: &[u8]) -> std::process::Output {
    use std::io::Write;
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let payload = payload.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&payload));
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    output
}

fn command_of(entry: &Value) -> &str {
    // Matcher events nest the handler; lifecycle events are bare handlers.
    let handler = entry.get("hooks").map_or(entry, |hooks| &hooks[0]);
    handler["command"].as_str().unwrap()
}

#[test]
fn every_event_is_wired_to_one_forwarder_entry_with_a_timeout() {
    let map = forwarder_hooks_map(&config(Path::new("/opt/boss/boss-event")));
    let events: Vec<&str> = map.keys().map(String::as_str).collect();
    let mut expected = AGY_HOOK_EVENTS.to_vec();
    expected.sort_unstable();
    let mut actual = events.clone();
    actual.sort_unstable();
    assert_eq!(actual, expected);
    assert_eq!(
        expected,
        [
            "PostInvocation",
            "PostToolUse",
            "PreInvocation",
            "PreToolUse",
            "SessionStart",
            "Stop"
        ]
    );

    for (event, entries) in &map {
        let entries = entries.as_array().unwrap();
        // One entry per event; for PreToolUse that is the single entry the
        // hook adapter later takes over (combining several is unmeasured).
        assert_eq!(entries.len(), 1, "{event}");
        let entry = &entries[0];
        let handler = if MATCHER_EVENTS.contains(&event.as_str()) {
            assert_eq!(entry["matcher"], "*", "{event}");
            &entry["hooks"][0]
        } else {
            assert!(entry.get("matcher").is_none(), "{event}");
            entry
        };
        assert_eq!(handler["type"], "command", "{event}");
        assert_eq!(handler["timeout"], FORWARDER_HOOK_TIMEOUT_SEC, "{event}");
        let command = handler["command"].as_str().unwrap();
        assert!(
            command.starts_with("BOSS_EVENTS_SOCKET='/tmp/boss test/events.sock' "),
            "{command}"
        );
        for fragment in [
            "BOSS_LEASE_ID='lease-1'",
            "BOSS_RUN_ID='run-1'",
            "BOSS_WORKSPACE='/tmp/ws one'",
        ] {
            assert!(command.contains(fragment), "{event}: {command}");
        }
        assert!(
            command.ends_with(&format!("'/opt/boss/boss-event' '{event}'")),
            "{command}"
        );
    }
}

#[test]
fn forwarder_command_matches_the_shared_claude_and_grok_shape() {
    let cfg = config(Path::new("/opt/boss/boss-event"));
    let map = forwarder_hooks_map(&cfg);
    let session_start = command_of(&map["SessionStart"][0]);
    assert_eq!(
        session_start,
        format!("{} 'SessionStart'", crate::hook_settings::forwarder_command(&cfg))
    );
}

#[test]
fn hooks_json_has_one_named_group_and_no_permission_reply_fields() {
    let body = render_hooks_json(&config(Path::new("/opt/boss/boss-event"))).unwrap();
    assert!(body.ends_with('\n'));
    let parsed: Value = serde_json::from_str(&body).unwrap();
    let groups = parsed.as_object().unwrap();
    assert_eq!(groups.keys().collect::<Vec<_>>(), [HOOK_GROUP]);
    assert_eq!(groups[HOOK_GROUP].as_object().unwrap().len(), AGY_HOOK_EVENTS.len());
    // The forwarder carries no verdict of its own.
    for word in ["auto_approve", "deny", "allow", "decision"] {
        assert!(!body.contains(word), "forwarder wiring must not mention {word:?}");
    }
}

#[test]
fn forwarded_payload_is_unchanged_stdout_is_silent_and_event_name_is_argv() {
    let tmp = TempDir::new().unwrap();
    let stub = stub_forwarder(tmp.path());
    let map = forwarder_hooks_map(&config(&stub));
    // A ~100 KB payload: every agy payload repeats the whole prompt.
    let payload: Vec<u8> = (0..100_000u32).map(|i| b"abcdefghij\n"[(i % 11) as usize]).collect();
    for event in AGY_HOOK_EVENTS {
        let output = run_hook_command(command_of(&map[*event][0]), &payload);
        assert_eq!(output.status.code(), Some(0), "{event}");
        assert!(output.stdout.is_empty(), "{event} must not reply on stdout");
        assert_eq!(fs::read(tmp.path().join("payload.bin")).unwrap(), payload, "{event}");
        let args = fs::read_to_string(tmp.path().join("args.txt")).unwrap();
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            [
                "/tmp/boss test/events.sock",
                "lease-1",
                "run-1",
                "/tmp/ws one",
                "1",
                event
            ],
            "{event}"
        );
    }
}

/// A `PreToolUse` hook that exits non-zero denies the tool call it reports,
/// so a shim that could neither deliver nor buffer the event blocks that
/// call: delivery failure is observable and never turns into an approval.
/// The command string must therefore not mask the shim's exit status.
#[test]
fn forwarder_exit_status_is_not_masked() {
    let tmp = TempDir::new().unwrap();
    let stub = stub_forwarder(tmp.path());
    fs::write(tmp.path().join("exit-status"), "3").unwrap();
    let map = forwarder_hooks_map(&config(&stub));
    for event in AGY_HOOK_EVENTS {
        let output = run_hook_command(command_of(&map[*event][0]), b"{}");
        assert_eq!(output.status.code(), Some(3), "{event} must surface the shim's failure");
        assert!(output.stdout.is_empty(), "{event} must not turn failure into a reply");
    }
}

#[test]
fn write_hooks_replaces_the_provisioned_canary_and_leaves_trust_alone() {
    let tmp = TempDir::new().unwrap();
    let _guard = env_for_provision(&tmp.path().join("homes"));
    let workspace = tmp.path().join("ws");
    fs::create_dir_all(&workspace).unwrap();
    let runtime = provision_agy_home(&workspace, "prompt", "run-hooks").unwrap();
    let path = hooks_file_path(&runtime.process_home);
    assert!(fs::read_to_string(&path).unwrap().contains("\"command\": \"true\""));
    let settings_before = fs::read(runtime.process_home.join(".gemini/antigravity-cli/settings.json")).unwrap();

    let cfg = config(Path::new("/opt/boss/boss-event"));
    let written = write_hooks(&runtime.process_home, &cfg).unwrap();

    assert_eq!(written, path);
    assert_eq!(fs::read_to_string(&path).unwrap(), render_hooks_json(&cfg).unwrap());
    assert!(!path.with_extension("json.tmp").exists());
    // The control directory is not added to trustedWorkspaces.
    assert_eq!(
        fs::read(runtime.process_home.join(".gemini/antigravity-cli/settings.json")).unwrap(),
        settings_before
    );
    let trusted = fs::read_to_string(runtime.process_home.join(".gemini/antigravity-cli/settings.json")).unwrap();
    assert!(
        !trusted.contains(&runtime.control_dir.display().to_string()),
        "{trusted}"
    );
}

fn permission_input(run_id: &str, workspace: &Path) -> PermissionInput {
    let forwarder = stub_forwarder(workspace.parent().unwrap());
    PermissionInput::builder()
        .worker_kind(WorkerKind::Standard)
        .workspace_path(workspace.to_path_buf())
        .events_socket_path(PathBuf::from("/tmp/boss/events.sock"))
        .boss_event_path(forwarder)
        .run_id(run_id.to_owned())
        .lease_id("lease-9".to_owned())
        .execution_kind("task_implementation".to_owned())
        .is_remote(false)
        .build()
}

#[tokio::test]
async fn write_permission_config_installs_the_forwarder_under_the_per_run_home() {
    let tmp = TempDir::new().unwrap();
    let _guard = env_for_provision(&tmp.path().join("homes"));
    let workspace = tmp.path().join("ws");
    fs::create_dir_all(&workspace).unwrap();
    let driver = crate::agy::AgyDriver::default();
    let state = driver
        .provision_workspace(&workspace, "prompt", "run-wpc")
        .await
        .unwrap()
        .unwrap();
    let runtime = crate::agy::home::AgyRuntimeState::from_driver_runtime_state(&state).unwrap();

    crate::agy::test_environment::write_final_settings("run-wpc", &workspace, WorkerKind::Standard);
    let artifacts = driver
        .write_permission_config(&permission_input("run-wpc", &workspace), tmp.path())
        .await
        .unwrap();

    let expected = hooks_file_path(&runtime.process_home);
    assert!(artifacts.config_files.contains(&expected));
    assert!(artifacts.config_files.contains(&adapter_path(&runtime.process_home)));
    assert!(artifacts.extra_args.is_empty());
    assert!(artifacts.env.is_empty());
    let parsed: Value = serde_json::from_str(&fs::read_to_string(&expected).unwrap()).unwrap();
    let session_start = command_of(&parsed[HOOK_GROUP]["SessionStart"][0]);
    assert!(session_start.contains("BOSS_RUN_ID='run-wpc'"), "{session_start}");
    assert!(session_start.contains("BOSS_LEASE_ID='lease-9'"), "{session_start}");
    assert!(
        session_start.contains(&format!("BOSS_WORKSPACE='{}'", workspace.display())),
        "{session_start}"
    );
    // The declared wiring is exactly what was installed.
    let declared = driver.progress_observation_wiring(&ProgressObservationConfig {
        events_socket_path: PathBuf::from("/tmp/boss/events.sock"),
        lease_id: "lease-9".into(),
        run_id: "run-wpc".into(),
        workspace_path: workspace.clone(),
        forwarder_binary: tmp.path().join("boss event stub"),
    });
    let crate::ProgressIngress::HookCallback(wiring) = declared else {
        panic!("agy declares HookCallback ingress");
    };
    assert_eq!(Value::Object(wiring.hooks), parsed[HOOK_GROUP]);
}

#[tokio::test]
async fn write_permission_config_requires_a_provisioned_home() {
    let tmp = TempDir::new().unwrap();
    let _guard = env_for_provision(&tmp.path().join("homes"));
    let error = crate::agy::AgyDriver::default()
        .write_permission_config(&permission_input("run-missing", tmp.path()), tmp.path())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("call provision_workspace first"),
        "{error:#}"
    );
}

#[tokio::test]
async fn write_permission_config_refuses_a_swapped_hooks_symlink() {
    let tmp = TempDir::new().unwrap();
    let _guard = env_for_provision(&tmp.path().join("homes"));
    let workspace = tmp.path().join("ws");
    fs::create_dir_all(&workspace).unwrap();
    let runtime = provision_agy_home(&workspace, "prompt", "run-link").unwrap();
    let victim = tmp.path().join("victim.json");
    fs::write(&victim, "untouched").unwrap();
    let path = hooks_file_path(&runtime.process_home);
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&victim, &path).unwrap();

    let error = crate::agy::AgyDriver::default()
        .write_permission_config(&permission_input("run-link", &workspace), tmp.path())
        .await
        .unwrap_err();

    assert!(format!("{error:#}").contains("symlink"), "{error:#}");
    assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
}
