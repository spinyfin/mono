use super::*;
use crate::{ProgressObservationConfig, ToolUseInterceptionConfig, WorkerKind};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

use super::super::home::{process_home_for_run, provision_agy_home_with_kind};
use super::super::test_environment::{EnvGuard, env_for_provision};
use crate::ExecutionWorkspaceKind;

struct Fixture {
    tmp: TempDir,
    _guard: EnvGuard,
    process_home: PathBuf,
    run_id: String,
}

fn write_exec(path: &Path, body: &str) {
    super::write_executable(path, body).unwrap();
}

fn approve_guard() -> &'static str {
    "#!/usr/bin/env python3\nimport json\nprint(json.dumps({'decision':'approve'}))\n"
}

impl Fixture {
    fn new(run_id: &str) -> Self {
        Self::with_workspace_kind(run_id, ExecutionWorkspaceKind::Cube)
    }

    fn with_workspace_kind(run_id: &str, kind: ExecutionWorkspaceKind) -> Self {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let guard = env_for_provision(&homes);
        provision_agy_home_with_kind(&workspace, "prompt", run_id, kind).unwrap();
        let process_home = process_home_for_run(run_id).unwrap();
        let forwarder = tmp.path().join("boss-event");
        write_exec(&forwarder, "#!/bin/sh\ncat >/dev/null\n");
        let path_guard = tmp.path().join("boss-path-guard.py");
        write_exec(&path_guard, crate::path_guard::PATH_GUARD_SCRIPT);
        write_exec(&tmp.path().join("boss-checkleft-push-guard.py"), approve_guard());
        write_exec(&tmp.path().join("boss-git-store-guard.py"), approve_guard());
        Self {
            tmp,
            _guard: guard,
            process_home,
            run_id: run_id.to_owned(),
        }
    }

    fn workspace(&self) -> PathBuf {
        self.tmp.path().join("ws")
    }

    fn forwarder(&self) -> PathBuf {
        self.tmp.path().join("boss-event")
    }

    fn path_guard(&self) -> PathBuf {
        self.tmp.path().join("boss-path-guard.py")
    }

    fn checkleft_guard(&self) -> PathBuf {
        self.tmp.path().join("boss-checkleft-push-guard.py")
    }

    fn git_store_guard(&self) -> PathBuf {
        self.tmp.path().join("boss-git-store-guard.py")
    }

    fn input(&self, kind: WorkerKind) -> crate::PermissionInput {
        self.input_with(kind, |_| {})
    }

    fn input_with(
        &self,
        kind: WorkerKind,
        overrides: impl FnOnce(&mut crate::PermissionInput),
    ) -> crate::PermissionInput {
        let mut input = crate::PermissionInput::builder()
            .worker_kind(kind)
            .workspace_path(self.workspace())
            .events_socket_path(self.obs().events_socket_path)
            .boss_event_path(self.forwarder())
            .run_id(self.run_id.clone())
            .lease_id("lease-1")
            .execution_kind("task_implementation")
            .is_remote(false)
            .path_guard_script(self.path_guard())
            .build();
        overrides(&mut input);
        input
    }

    fn complete_input(&self, kind: WorkerKind) -> crate::PermissionInput {
        self.input_with(kind, |input| {
            input.checkleft_guard_script = Some(self.checkleft_guard());
            input.git_store_guard_script = Some(self.git_store_guard());
        })
    }

    fn complete_install(&self, kind: WorkerKind) -> Vec<PathBuf> {
        self.install_with(kind, |c| {
            c.checkleft_guard_script = Some(self.checkleft_guard());
            c.git_store_guard_script = Some(self.git_store_guard());
        })
    }

    fn obs(&self) -> ProgressObservationConfig {
        ProgressObservationConfig {
            events_socket_path: self.tmp.path().join("boss-data").join("events.sock"),
            lease_id: "lease-1".into(),
            run_id: self.run_id.clone(),
            workspace_path: self.workspace(),
            forwarder_binary: self.forwarder(),
        }
    }

    fn interception(&self, overrides: impl FnOnce(&mut ToolUseInterceptionConfig)) -> ToolUseInterceptionConfig {
        let mut config = ToolUseInterceptionConfig {
            data_dir: Some(self.tmp.path().join("boss-data")),
            path_guard_script: Some(self.path_guard()),
            checkleft_guard_script: None,
            git_store_guard_script: None,
            is_revision: false,
            is_standard_worker: true,
            is_reviewer: false,
            is_review_guide: false,
            run_id: Some(self.run_id.clone()),
            workspace_path: Some(self.workspace()),
        };
        overrides(&mut config);
        config
    }

    fn install(&self, kind: WorkerKind) -> Vec<PathBuf> {
        self.install_with(kind, |_| {})
    }

    fn install_with(&self, kind: WorkerKind, overrides: impl FnOnce(&mut ToolUseInterceptionConfig)) -> Vec<PathBuf> {
        super::super::test_environment::write_final_settings(
            &self.run_id,
            &self.workspace(),
            kind,
            &self.obs().events_socket_path,
        );
        install_authorization(&self.process_home, kind, &self.obs(), &self.interception(overrides)).unwrap()
    }
}

fn run_command_payload(command: &str) -> Value {
    json!({
        "hookEventName": "PreToolUse",
        "conversationId": "c1",
        "toolCall": {
            "id": "t1",
            "name": "run_command",
            "args": {"CommandLine": command, "Cwd": "/tmp"},
        },
    })
}

fn authorize(process_home: &Path, payload: &Value) -> (i32, Value, String) {
    authorize_with_env(process_home, payload, &[])
}

fn authorize_with_env(process_home: &Path, payload: &Value, env: &[(&str, &str)]) -> (i32, Value, String) {
    let adapter = adapter_path(process_home);
    let policy = policy_path(process_home);
    let policy_doc: Value = serde_json::from_str(&fs::read_to_string(&policy).unwrap()).unwrap();
    let run_id = policy_doc["run_id"].as_str().unwrap();
    let mut cmd = Command::new("python3");
    cmd.arg(&adapter)
        .env("BOSS_AGY_POLICY", &policy)
        .env("BOSS_AGY_FORWARDER", policy_doc["forwarder"].as_str().unwrap())
        .env("BOSS_RUN_ID", run_id)
        .env_remove("BOSS_STRUCTURED_OUTPUT")
        .env("HOME", process_home)
        .env("BOSS_BIN", "/opt/boss-tools/boss")
        .env("CUBE_BIN", "/opt/boss-tools/cube")
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn python3 adapter");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().expect("wait adapter");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let body = serde_json::from_str(stdout.trim()).unwrap_or_else(|_| json!({"raw": stdout}));
    (output.status.code().unwrap_or(-1), body, stderr)
}

fn decision(body: &Value) -> &str {
    body["decision"].as_str().unwrap_or_default()
}

#[test]
fn install_writes_adapter_policy_attestation_and_single_pre_tool_use_entry() {
    let fx = Fixture::new("run-install-1");
    let written = fx.install(WorkerKind::Standard);
    let adapter = adapter_path(&fx.process_home);
    let policy = policy_path(&fx.process_home);
    let attestation = attestation_path(&fx.process_home);
    let hooks_path = hooks_file_path(&fx.process_home);
    assert!(written.contains(&adapter));
    assert!(written.contains(&policy));
    assert!(written.contains(&attestation));
    assert!(written.contains(&hooks_path));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&adapter).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "adapter must be executable");
    }
    let hooks_doc: Value = serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
    let pre = hooks_doc["boss"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 1, "{pre:#?}");
    assert_eq!(pre[0]["matcher"], "*");
    let command = pre[0]["hooks"][0]["command"].as_str().unwrap();
    assert!(command.contains(&adapter.display().to_string()), "{command}");
    assert!(command.contains("BOSS_AGY_POLICY="), "{command}");
    assert_eq!(
        pre[0]["hooks"][0]["timeout"].as_u64(),
        Some(AUTHORIZATION_HOOK_TIMEOUT_SEC)
    );
    assert!(
        hooks_doc["boss"].get("SessionStart").is_some(),
        "SessionStart must forward progress: {hooks_doc:#}"
    );
    let policy_doc: Value = serde_json::from_str(&fs::read_to_string(&policy).unwrap()).unwrap();
    assert_eq!(policy_doc["approval"], "auto_approve");
    assert_eq!(policy_doc["worker_kind"], "standard");
    assert!(policy_doc.get("exported_paths").is_none());
    assert!(policy_doc["artifact_write_paths"].as_array().unwrap().is_empty());
    verify_attestation(&fx.process_home).unwrap();
}

#[test]
fn install_replaces_provisioned_hooks_with_the_boss_group() {
    let fx = Fixture::new("run-merge-1");
    let hooks_path = hooks_file_path(&fx.process_home);
    fs::write(
        &hooks_path,
        r#"{
  "hooks": {
    "SessionStart": [{"matcher": "*", "hooks": [{"type": "command", "command": "true"}]}],
    "PostToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": "/tmp/boss-event"}]}],
    "Stop": [{"matcher": "*", "hooks": [{"type": "command", "command": "/tmp/boss-event"}]}]
  }
}
"#,
    )
    .unwrap();
    fx.install(WorkerKind::Standard);
    let hooks_doc: Value = serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
    assert!(hooks_doc["boss"].get("SessionStart").is_some());
    assert!(
        hooks_doc["boss"]["PostToolUse"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    );
    assert!(hooks_doc["boss"]["Stop"].as_array().is_some_and(|a| !a.is_empty()));
    assert_eq!(hooks_doc["boss"]["PreToolUse"].as_array().unwrap().len(), 1);
}

#[test]
fn install_takes_over_pre_tool_use_inside_an_existing_boss_group() {
    let fx = Fixture::new("run-merge-boss");
    let hooks_path = hooks_file_path(&fx.process_home);
    fs::write(
        &hooks_path,
        r#"{
  "boss": {
    "SessionStart": [{"type": "command", "command": "/tmp/boss-event", "timeout": 15}],
    "PreToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": "/tmp/boss-event"}]}],
    "PostToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": "/tmp/boss-event"}]}],
    "Stop": [{"type": "command", "command": "/tmp/boss-event", "timeout": 15}]
  }
}
"#,
    )
    .unwrap();
    fx.install(WorkerKind::Standard);
    let hooks_doc: Value = serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
    assert!(hooks_doc.get("hooks").is_none());
    assert_eq!(hooks_doc["boss"].as_object().unwrap().len(), 6);
    assert!(
        hooks_doc["boss"]["SessionStart"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    );
    assert!(
        hooks_doc["boss"]["PostToolUse"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    );
    assert!(hooks_doc["boss"]["Stop"].as_array().is_some_and(|a| !a.is_empty()));
    let command = hooks_doc["boss"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(command.contains("boss-agy-hook-adapter.py"), "{command}");
    assert!(command.contains("BOSS_AGY_POLICY="), "{command}");
}

#[test]
fn checkleft_raises_the_outer_hook_timeout() {
    let fx = Fixture::new("run-timeout-1");
    let checkleft = fx.tmp.path().join("boss-checkleft-push-guard.py");
    write_exec(&checkleft, approve_guard());
    fx.install_with(WorkerKind::Standard, |c| {
        c.checkleft_guard_script = Some(checkleft);
    });
    let hooks_doc: Value =
        serde_json::from_str(&fs::read_to_string(hooks_file_path(&fx.process_home)).unwrap()).unwrap();
    assert_eq!(
        hooks_doc["boss"]["PreToolUse"][0]["hooks"][0]["timeout"].as_u64(),
        Some(crate::CHECKLEFT_GUARD_HOOK_TIMEOUT_SECS)
    );
}

#[test]
fn probe_attests_positive_and_denied_sentinels_and_content_binding() {
    assert!(
        super::python3_available(),
        "python3 is required for adapter acceptance tests"
    );
    let fx = Fixture::new("run-probe-1");
    fx.install(WorkerKind::Standard);
    let report = probe_authorization(&fx.process_home).expect("probe");
    assert!(report.ok);
    assert_eq!(report.positive_decision, "auto_approve");
    assert_eq!(report.denied_decision, "deny");
    assert!(
        report
            .denied_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("sudo") || reason.contains("recursive")),
        "{:?}",
        report.denied_reason
    );
    let attestation = verify_attestation(&fx.process_home).unwrap();
    let names: Vec<_> = attestation.entries.iter().map(|e| e.name.as_str()).collect();
    for required in ["adapter", "hooks", "policy", "settings", "guard:path"] {
        assert!(names.contains(&required), "missing {required} in {names:?}");
    }
}

#[test]
fn mutated_attested_artifacts_fail_the_probe() {
    assert!(
        super::python3_available(),
        "python3 is required for adapter acceptance tests"
    );
    let fx = Fixture::new("run-probe-mutate");
    fx.install(WorkerKind::Standard);
    probe_authorization(&fx.process_home).unwrap();

    let mutate = |path: &Path| {
        let mut body = fs::read_to_string(path).unwrap();
        body.push('\n');
        fs::write(path, body).unwrap();
    };

    mutate(&adapter_path(&fx.process_home));
    let err = probe_authorization(&fx.process_home).unwrap_err();
    assert!(
        format!("{err:#}").contains("binding") || format!("{err:#}").contains("changed"),
        "{err:#}"
    );
}

#[test]
fn mutated_hooks_policy_settings_and_guard_fail_binding() {
    let fx = Fixture::new("run-bind-mutate");
    fx.install(WorkerKind::Standard);
    verify_attestation(&fx.process_home).unwrap();

    let original_hooks = fs::read_to_string(hooks_file_path(&fx.process_home)).unwrap();
    fs::write(hooks_file_path(&fx.process_home), format!("{original_hooks}\n")).unwrap();
    assert!(verify_attestation(&fx.process_home).is_err());
    fs::write(hooks_file_path(&fx.process_home), original_hooks).unwrap();
    verify_attestation(&fx.process_home).unwrap();

    let original_policy = fs::read_to_string(policy_path(&fx.process_home)).unwrap();
    fs::write(policy_path(&fx.process_home), format!("{original_policy}\n")).unwrap();
    assert!(verify_attestation(&fx.process_home).is_err());
    fs::write(policy_path(&fx.process_home), original_policy).unwrap();

    let settings = super::super::home::session_settings_path_for_run(&fx.run_id).unwrap();
    let original_settings = fs::read_to_string(&settings).unwrap();
    fs::write(&settings, format!("{original_settings}\n")).unwrap();
    assert!(verify_attestation(&fx.process_home).is_err());
    fs::write(&settings, original_settings).unwrap();

    let original_guard = fs::read_to_string(fx.path_guard()).unwrap();
    fs::write(fx.path_guard(), format!("{original_guard}\n")).unwrap();
    assert!(verify_attestation(&fx.process_home).is_err());
}

#[test]
fn camelcase_toolcall_payload_translates_to_auto_approve() {
    assert!(
        super::python3_available(),
        "python3 is required for adapter acceptance tests"
    );
    let fx = Fixture::new("run-translate-1");
    fx.install(WorkerKind::Standard);
    let (code, body, stderr) = authorize(&fx.process_home, &run_command_payload("echo ok"));
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "auto_approve", "{body} stderr={stderr}");
}

#[test]
fn write_file_is_not_fail_closed_by_bash_only_guards() {
    assert!(
        super::python3_available(),
        "python3 is required for adapter acceptance tests"
    );
    let fx = Fixture::new("run-write-1");
    fx.install_with(WorkerKind::Standard, |c| {
        c.path_guard_script = None;
        c.data_dir = None;
    });
    let payload = json!({
        "toolCall": {
            "name": "write_file",
            "args": {"Path": fx.workspace().join("hello.txt").display().to_string(), "Contents": "hi"},
        }
    });
    let (code, body, stderr) = authorize(&fx.process_home, &payload);
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "auto_approve", "{body} stderr={stderr}");
}

#[test]
fn structural_denies_cover_bypass_mcp_stdin_sudo_and_unknown_tools() {
    assert!(
        super::python3_available(),
        "python3 is required for adapter acceptance tests"
    );
    let fx = Fixture::new("run-struct-1");
    fx.install(WorkerKind::Standard);
    let cases = [
        (
            json!({"toolCall": {"name": "run_command", "args": {"CommandLine": "echo hi", "BypassSandbox": true}}}),
            "BypassSandbox",
        ),
        (
            json!({"toolCall": {"name": "call_mcp_tool", "args": {"ServerName": "browser", "ToolName": "click"}}}),
            "MCP",
        ),
        (json!({"toolCall": {"name": "mcp_github", "args": {}}}), "MCP"),
        (
            json!({"toolCall": {"name": "manage_task", "args": {"Action": "send_input", "Input": "rm -rf /"}}}),
            "send_input",
        ),
        (
            json!({"toolCall": {"name": "manage_task", "args": {"Action": "list"}}}),
            "manage_task",
        ),
        (
            json!({"toolCall": {"name": "run_command", "args": {"CommandLine": "sudo echo hi"}}}),
            "sudo",
        ),
        (
            json!({"toolCall": {"name": "run_command", "args": {"CommandLine": "rm -rf /tmp/x"}}}),
            "recursive",
        ),
        (
            json!({"toolCall": {"name": "browser_subagent", "args": {}}}),
            "browser_subagent",
        ),
        (json!({"toolCall": {"name": "mystery_tool", "args": {}}}), "unknown"),
    ];
    for (payload, needle) in cases {
        let (code, body, stderr) = authorize(&fx.process_home, &payload);
        assert_eq!(code, 0, "{needle} stderr={stderr}");
        assert_eq!(decision(&body), "deny", "{needle}: {body}");
        let reason = body["reason"].as_str().unwrap_or_default();
        assert!(
            reason.to_lowercase().contains(&needle.to_lowercase())
                || reason.contains("MCP")
                || reason.contains("stdin")
                || reason.contains("destructive")
                || reason.contains("structurally"),
            "{needle}: {reason}"
        );
    }
}

#[test]
fn protected_configuration_is_denied() {
    assert!(
        super::python3_available(),
        "python3 is required for adapter acceptance tests"
    );
    let fx = Fixture::new("run-protect-1");
    fx.install(WorkerKind::Standard);
    let gemini = fx.process_home.join(".gemini/config/hooks.json");
    let payload = json!({
        "toolCall": {
            "name": "write_file",
            "args": {"Path": gemini.display().to_string(), "Contents": "owned"},
        }
    });
    let (code, body, stderr) = authorize(&fx.process_home, &payload);
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "deny", "{body}");
    assert!(
        body["reason"].as_str().unwrap_or_default().contains("protected"),
        "{body}"
    );
}

#[test]
fn reviewer_native_and_shell_writes_are_denied() {
    assert!(
        super::python3_available(),
        "python3 is required for adapter acceptance tests"
    );
    let fx = Fixture::new("run-reviewer-1");
    fx.install_with(WorkerKind::Reviewer, |c| {
        c.is_standard_worker = false;
        c.is_reviewer = true;
    });
    let write = json!({
        "toolCall": {
            "name": "write_file",
            "args": {"Path": fx.workspace().join("x.rs").display().to_string(), "Contents": "x"},
        }
    });
    let (code, body, stderr) = authorize(&fx.process_home, &write);
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "deny", "{body}");
    let redirect = run_command_payload("echo hi > /tmp/out");
    let (code, body, stderr) = authorize(&fx.process_home, &redirect);
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "deny", "{body}");
    let touch = run_command_payload("touch /tmp/out");
    let (code, body, stderr) = authorize(&fx.process_home, &touch);
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "deny", "{body}");
}

#[test]
fn reviewer_may_write_the_engine_owned_review_result_artifact() {
    assert!(
        super::python3_available(),
        "python3 is required for adapter acceptance tests"
    );
    let fx = Fixture::new("run-reviewer-artifact");
    fx.install_with(WorkerKind::Reviewer, |c| {
        c.is_standard_worker = false;
        c.is_reviewer = true;
    });
    let policy: Value = serde_json::from_str(&fs::read_to_string(policy_path(&fx.process_home)).unwrap()).unwrap();
    let artifact = policy["artifact_write_paths"][0]
        .as_str()
        .expect("reviewer artifact path");
    assert!(Path::new(artifact).is_absolute(), "{artifact}");
    let write = json!({
        "toolCall": {
            "name": "write_file",
            "args": {"Path": artifact, "Contents": "{\"findings\":[]}"},
        }
    });
    let (code, body, stderr) = authorize(&fx.process_home, &write);
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "auto_approve", "{body} stderr={stderr}");

    let sibling = Path::new(artifact).with_file_name("sibling.review-result.json");
    let sibling_write = json!({
        "toolCall": {
            "name": "write_file",
            "args": {"Path": sibling.display().to_string(), "Contents": "{}"},
        }
    });
    let (code, body, stderr) = authorize(&fx.process_home, &sibling_write);
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "deny", "{body}");
    assert!(
        body["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("must not modify files"),
        "{body}"
    );

    let workspace_write = json!({
        "toolCall": {
            "name": "write_file",
            "args": {"Path": fx.workspace().join("lib.rs").display().to_string(), "Contents": "x"},
        }
    });
    let (code, body, stderr) = authorize(&fx.process_home, &workspace_write);
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "deny", "{body}");
}

#[test]
fn allowlist_kinds_return_deny_unless_prior_grant() {
    assert!(
        super::python3_available(),
        "python3 is required for adapter acceptance tests"
    );
    let fx = Fixture::new("run-allowlist-1");
    fx.install_with(WorkerKind::AnswerAgent, |c| {
        c.is_standard_worker = false;
    });
    let (code, body, stderr) = authorize(&fx.process_home, &run_command_payload("echo ok"));
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "deny_unless_prior_grant", "{body}");
}

#[test]
fn child_calls_share_the_same_authorization_entry() {
    assert!(
        super::python3_available(),
        "python3 is required for adapter acceptance tests"
    );
    let fx = Fixture::new("run-child-1");
    fx.install(WorkerKind::Standard);
    let invoke = json!({"toolCall": {"name": "invoke_subagent", "args": {"prompt": "hello"}}});
    let (code, body, stderr) = authorize(&fx.process_home, &invoke);
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "auto_approve", "{body}");
    let denied_child = json!({
        "conversationId": "child-1",
        "toolCall": {"name": "run_command", "args": {"CommandLine": "sudo echo hi"}},
    });
    let (code, body, stderr) = authorize(&fx.process_home, &denied_child);
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "deny", "{body}");
}

#[test]
fn measured_parent_child_subagent_flow_is_approved() {
    assert!(
        super::python3_available(),
        "python3 is required for adapter acceptance tests"
    );
    let fx = Fixture::new("run-subagent-flow");
    fx.install(WorkerKind::Standard);
    for payload in [
        json!({"toolCall": {"name": "define_subagent", "args": {"name": "helper", "prompt": "work"}}}),
        json!({"toolCall": {"name": "invoke_subagent", "args": {"prompt": "hello"}}}),
        json!({
            "conversationId": "child-1",
            "toolCall": {"name": "run_command", "args": {"CommandLine": "echo ok"}},
        }),
        json!({
            "conversationId": "child-1",
            "toolCall": {"name": "send_message", "args": {"text": "done"}},
        }),
    ] {
        let (code, body, stderr) = authorize(&fx.process_home, &payload);
        assert_eq!(code, 0, "{payload} stderr={stderr}");
        assert_eq!(decision(&body), "auto_approve", "{payload}: {body} {stderr}");
    }
}

#[test]
fn silent_forward_discards_forwarder_stdout() {
    assert!(
        super::python3_available(),
        "python3 is required for adapter acceptance tests"
    );
    let fx = Fixture::new("run-forward-1");
    write_exec(
        &fx.forwarder(),
        "#!/bin/sh\necho '{\"decision\":\"allow\"}'\ncat >/dev/null\n",
    );
    fx.install(WorkerKind::Standard);
    let (code, body, stderr) = authorize(&fx.process_home, &run_command_payload("echo ok"));
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(
        decision(&body),
        "auto_approve",
        "forwarder stdout must not authorize: {body}"
    );
}

#[test]
fn forwarder_failure_denies() {
    assert!(
        super::python3_available(),
        "python3 is required for adapter acceptance tests"
    );
    let fx = Fixture::new("run-forward-fail");
    write_exec(&fx.forwarder(), "#!/bin/sh\nexit 1\n");
    fx.install(WorkerKind::Standard);
    let (code, body, stderr) = authorize(&fx.process_home, &run_command_payload("echo ok"));
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "deny", "{body}");
    assert!(
        body["reason"].as_str().unwrap_or_default().contains("forwarder"),
        "{body}"
    );
}

#[test]
fn pr_redirect_guard_denies_gh_pr_create_through_the_adapter() {
    assert!(
        super::python3_available(),
        "python3 is required for adapter acceptance tests"
    );
    let fx = Fixture::new("run-pr-1");
    fx.install(WorkerKind::Standard);
    let (code, body, stderr) = authorize(
        &fx.process_home,
        &run_command_payload("gh pr create --title x --body y"),
    );
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "deny", "{body}");
    let reason = body["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("CUBE_BIN") || reason.contains("pr create") || reason.contains("gh"),
        "{reason}"
    );
}

#[test]
fn git_store_guard_refuses_store_writes_via_agy_payload() {
    assert!(
        super::python3_available(),
        "python3 is required for adapter acceptance tests"
    );
    let layout = crate::git_store_guard::tests::cube_layout();
    let fx = Fixture::new("run-git-store-1");
    fx.install_with(WorkerKind::Standard, |c| {
        c.git_store_guard_script = Some(layout.script.clone());
        c.workspace_path = Some(layout.workspace.clone());
    });
    let store = layout.store.display();
    let (code, body, stderr) = authorize(
        &fx.process_home,
        &run_command_payload(&format!("git -C {store} fetch --depth=200 origin main")),
    );
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "deny", "{body}");
}

#[test]
fn guard_commands_always_includes_boss_launch_guard() {
    let config = ToolUseInterceptionConfig {
        data_dir: None,
        path_guard_script: None,
        checkleft_guard_script: None,
        git_store_guard_script: None,
        is_revision: false,
        is_standard_worker: false,
        is_reviewer: false,
        is_review_guide: false,
        run_id: Some("run-1".into()),
        workspace_path: Some(PathBuf::from("/tmp/ws")),
    };
    let guards = super::guard_commands(&config);
    assert_eq!(guards.len(), 1, "{config:?}");
    assert_eq!(guards[0].command, crate::claude::BOSS_LAUNCH_GUARD_COMMAND);
    assert_eq!(guards[0].matcher, "Bash");
}

#[test]
fn never_emits_allow_ask_or_force_ask() {
    assert!(
        super::python3_available(),
        "python3 is required for adapter acceptance tests"
    );
    let fx = Fixture::new("run-vocab-1");
    fx.install(WorkerKind::Standard);
    let (code, body, _) = authorize(&fx.process_home, &run_command_payload("echo ok"));
    assert_eq!(code, 0);
    let decision = decision(&body);
    assert!(
        matches!(decision, "auto_approve" | "deny" | "deny_unless_prior_grant"),
        "{decision}"
    );
    assert_ne!(decision, "allow");
    assert_ne!(decision, "ask");
    assert_ne!(decision, "force_ask");
}

#[path = "adapter_failure_tests.rs"]
mod failure_tests;

#[path = "authorization_edge_tests.rs"]
mod edge_tests;

#[path = "review_regression_tests.rs"]
mod review_regression_tests;

#[path = "startup_tests.rs"]
mod startup_tests;

#[path = "protected_operand_tests.rs"]
mod protected_operand_tests;

#[path = "scratch_and_quoting_tests.rs"]
mod scratch_and_quoting_tests;

#[path = "scratch_workspace_tests.rs"]
mod scratch_workspace_tests;
