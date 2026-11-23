use super::*;
use std::os::unix::fs::PermissionsExt;

fn raw_authorize(fx: &Fixture, raw: &str, run_id: Option<&str>) -> Value {
    let mut command = Command::new("python3");
    command
        .arg(adapter_path(&fx.process_home))
        .env("BOSS_AGY_POLICY", policy_path(&fx.process_home))
        .env(
            "BOSS_AGY_FORWARDER",
            gemini_config_dir(&fx.process_home).join("boss-event"),
        )
        .env_remove("BOSS_RUN_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(run_id) = run_id {
        command.env("BOSS_RUN_ID", run_id);
    }
    let mut child = command.spawn().expect("python3 is required for authorization tests");
    child.stdin.take().unwrap().write_all(raw.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_deny(body: &Value) {
    assert_eq!(decision(body), "deny", "{body}");
    assert!(!body["reason"].as_str().unwrap_or_default().is_empty(), "{body}");
}

pub(super) fn change_policy(fx: &Fixture, update: impl FnOnce(&mut Value)) {
    let path = policy_path(&fx.process_home);
    let mut policy: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    update(&mut policy);
    fs::write(&path, policy.to_string()).unwrap();
    let path = attestation_path(&fx.process_home);
    let mut binding: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for entry in binding["entries"].as_array_mut().unwrap() {
        if entry["name"] == "policy" {
            entry["sha256"] = json!(hash_file(&policy_path(&fx.process_home)).unwrap());
        }
    }
    fs::write(path, binding.to_string()).unwrap();
}

#[test]
fn malformed_input_and_identity_fail_closed() {
    let fx = Fixture::new("run-malformed");
    fx.install(WorkerKind::Standard);
    for raw in [
        "",
        "garbage",
        "[]",
        "null",
        r#"{"toolCall":{"name":"read_file","args":[]}}"#,
        r#"{"toolCall":{"name":"read_file"}}"#,
        r#"{"toolCall":{"name":"write_file","args":{"Path":123}}}"#,
        r#"{"toolCall":{"name":"run_command","args":{"CommandLine":false}}}"#,
        r#"{"toolCall":{"name":"run_command","args":{}}}"#,
        r#"{"toolCall":{"name":"read_file","args":{"Path":"a","path":"b"}}}"#,
    ] {
        assert_deny(&raw_authorize(&fx, raw, Some(&fx.run_id)));
    }
    for run in [None, Some("other-run")] {
        assert_deny(&raw_authorize(&fx, &run_command_payload("echo ok").to_string(), run));
    }
    let policy_bytes = fs::read(policy_path(&fx.process_home)).unwrap();
    for (field, value) in [
        ("worker_kind", json!("unknown")),
        ("guards", json!([])),
        ("guards", json!([{"command":"true", "matcher":"unknown"}])),
        ("protected_paths", json!(false)),
        ("workspace_path", json!(12)),
        ("command_grants", json!("gh pr view")),
        ("command_grants", json!([1])),
        ("command_grants", json!([" "])),
        // A kind answered `auto_approve` has no allowlist to grant from.
        ("command_grants", json!(["gh pr view"])),
    ] {
        let mut policy: Value = serde_json::from_slice(&policy_bytes).unwrap();
        policy[field] = value;
        fs::write(policy_path(&fx.process_home), policy.to_string()).unwrap();
        let body = raw_authorize(&fx, &run_command_payload("echo ok").to_string(), Some(&fx.run_id));
        assert_deny(&body);
        assert!(body["reason"].as_str().unwrap().contains("policy"), "{body}");
    }
    fs::write(policy_path(&fx.process_home), "garbage").unwrap();
    assert_deny(&raw_authorize(&fx, "{}", Some(&fx.run_id)));
    fs::remove_file(policy_path(&fx.process_home)).unwrap();
    assert_deny(&raw_authorize(&fx, "{}", Some(&fx.run_id)));
}

#[test]
fn guard_failures_are_explicit_and_bounded() {
    let fx = Fixture::new("run-guard-failures");
    for body in [
        "",
        "print('not json')",
        "print('{}')",
        "print('{\"decision\":\"mystery\"}')",
        "raise RuntimeError('crash')",
        "import sys; sys.exit(1)",
        "print('{\"decision\":\"approve\"}'); import sys; sys.exit(1)",
        "import os, signal; os.kill(os.getpid(), signal.SIGTERM)",
        "import time; time.sleep(60)",
    ] {
        write_exec(&fx.path_guard(), &format!("#!/usr/bin/env python3\n{body}\n"));
        fx.install(WorkerKind::Standard);
        change_policy(&fx, |p| p["guards"][0]["timeout_sec"] = json!(0.2));
        let start = std::time::Instant::now();
        let result = raw_authorize(&fx, &run_command_payload("echo ok").to_string(), Some(&fx.run_id));
        assert_deny(&result);
        assert!(result["reason"].as_str().unwrap().contains("guard"), "{result}");
        assert!(
            start.elapsed().as_secs() < 10,
            "guard did not complete within its bound"
        );
    }
}

#[test]
fn rewrites_emit_native_keys_and_recheck_policy() {
    let fx = Fixture::new("run-rewrites");
    for (replacement, expected) in [
        ("echo rewritten", "auto_approve"),
        ("sudo id", "deny"),
        ("echo ok;rm -rf /tmp/x", "deny"),
    ] {
        let body = format!(
            "#!/usr/bin/env python3\nimport json\nprint(json.dumps({{'hookSpecificOutput':{{'updatedInput':{{'command':{replacement:?}}}}}}}))\n"
        );
        write_exec(&fx.path_guard(), &body);
        fx.install(WorkerKind::Standard);
        let (_, result, stderr) = authorize(&fx.process_home, &run_command_payload("echo original"));
        assert_eq!(decision(&result), expected, "{result} {stderr}");
        if expected == "auto_approve" {
            assert_eq!(result["overwrite"], json!({"CommandLine":"echo rewritten"}));
        }
    }
    let target = fx.process_home.join(".gemini/config/hooks.json");
    write_exec(
        &fx.path_guard(),
        &format!(
            "#!/usr/bin/env python3\nimport json\nprint(json.dumps({{'decision':'approve','hookSpecificOutput':{{'updatedInput':{{'file_path':{:?}}}}}}}))\n",
            target.display().to_string()
        ),
    );
    fx.install(WorkerKind::Standard);
    let payload = json!({"toolCall":{"name":"write_file","args":{"Path":"safe.txt","Contents":"hello"}}});
    assert_deny(&authorize(&fx.process_home, &payload).1);
}

#[test]
fn shared_tokenizer_and_reviewer_reads_and_mutations() {
    let fx = Fixture::new("run-shell-regressions");
    fx.install(WorkerKind::Standard);
    for command in [
        "echo hi\nsudo id",
        "echo ok;rm -rf /tmp/x",
        "echo $(sudo id)",
        "echo `sudo id`",
        "bash -lc 'sudo id'",
        "echo ok;bossctl status",
        "env sudo id",
        "env -i sudo id",
        "env -u SECRET sudo id",
        "nice -n 10 sudo id",
        "timeout --signal TERM 5 sudo id",
        "bash --noprofile -lc 'sudo id'",
    ] {
        assert_deny(&authorize(&fx.process_home, &run_command_payload(command)).1);
    }
    fx.install_with(WorkerKind::Reviewer, |c| {
        c.is_standard_worker = false;
        c.is_reviewer = true;
    });
    for command in [
        "echo ok\ntouch f",
        "gh pr comment 1 --body x",
        "gh --repo o/r pr comment 1 --body x",
        "git -C /tmp push",
        "sed -i s/a/b/ f",
        "awk -i inplace '{print}' f",
        "patch -p1",
        "jj describe -m x",
        "jj -R /tmp new",
        "git reset --hard",
        "git restore f",
        "gh api -X POST repos/o/r/issues",
    ] {
        assert_deny(&authorize(&fx.process_home, &run_command_payload(command)).1);
    }
    for command in [
        "gh pr view 1 --repo o/r",
        "gh pr diff 1 --repo o/r",
        "gh pr checks 1 --repo o/r",
        "git log --grep push",
        "ls >/dev/null",
        "grep 'a->b' f",
        "gh api -X GET repos/o/r",
    ] {
        let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(command));
        assert_eq!(decision(&body), "auto_approve", "{command}: {body} {stderr}");
    }
}

#[test]
fn protected_state_reads_expansions_and_symlinks_are_denied() {
    let fx = Fixture::new("run-path-regressions");
    fx.install(WorkerKind::Standard);
    for command in [
        r#"printf x > "$HOME/.gemini/config/hooks.json""#,
        "cd ~/.gemini && rm config/hooks.json",
        r#"cat "${HOME}/.gemini/config/boss-agy-auth-policy.json""#,
        "cat ~/.gemini/config/hooks.json",
        "cat ../process-home/.gemini/config/hooks.json",
    ] {
        let mut payload = run_command_payload(command);
        payload["toolCall"]["args"]["Cwd"] = json!(fx.process_home.parent().unwrap().join("control"));
        assert_deny(&authorize(&fx.process_home, &payload).1);
    }
    std::os::unix::fs::symlink(fx.process_home.join(".gemini"), fx.workspace().join("config-link")).unwrap();
    for (cwd, path) in [
        (fx.workspace(), "config-link/config/hooks.json"),
        (
            fx.process_home.parent().unwrap().join("control"),
            "../process-home/.gemini/config/hooks.json",
        ),
    ] {
        let payload = json!({"cwd":cwd, "toolCall":{"name":"read_file","args":{"Path":path}}});
        assert_deny(&authorize(&fx.process_home, &payload).1);
    }
    let payload = json!({"cwd":fx.workspace(), "toolCall":{"name":"read_file",
        "args":{"Path":"ordinary-file", "Cwd":fx.process_home.join(".gemini")}}});
    assert_deny(&authorize(&fx.process_home, &payload).1);
}

#[test]
fn native_searches_reach_the_path_guard_as_traversals() {
    let fx = Fixture::new("run-search-regressions");
    // Assert the consumer-side contract, including the canonical name and
    // root used by the shared path guard's traversal_roots function.
    write_exec(
        &fx.path_guard(),
        "#!/usr/bin/env python3\nimport json,sys\np=json.load(sys.stdin)\nassert p['tool_name'] in ('Grep','Glob')\nassert p['tool_input']['path']=='/'\nprint(json.dumps({'decision':'block','reason':'broad traversal root'}))\n",
    );
    fx.install(WorkerKind::Standard);
    for tool in ["grep", "glob", "list_dir"] {
        let payload = json!({"toolCall":{"name":tool,"args":{"Path":"/"}}});
        let body = authorize(&fx.process_home, &payload).1;
        assert_eq!(body["reason"], "broad traversal root", "{body}");
    }
    for tool in [
        "list_directory",
        "list_files",
        "search_codebase",
        "view_file",
        "read_url",
    ] {
        assert_deny(&authorize(&fx.process_home, &json!({"toolCall":{"name":tool,"args":{}}})).1);
    }
}

#[test]
fn probe_requires_complete_enabled_executable_configuration() {
    let fx = Fixture::new("run-probe-failures");
    fx.install(WorkerKind::Standard);
    probe_authorization(&fx.process_home).unwrap();
    let binding: Value = serde_json::from_slice(&fs::read(attestation_path(&fx.process_home)).unwrap()).unwrap();
    for entry in binding["entries"].as_array().unwrap() {
        let path = Path::new(entry["path"].as_str().unwrap());
        let bytes = fs::read(path).unwrap();
        fs::remove_file(path).unwrap();
        assert!(
            probe_authorization(&fx.process_home).is_err(),
            "missing {}",
            path.display()
        );
        fs::write(path, bytes).unwrap();
        if entry["name"] == "adapter" || entry["name"] == "forwarder" {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    for name in ["adapter", "forwarder"] {
        let path = if name == "adapter" {
            adapter_path(&fx.process_home)
        } else {
            gemini_config_dir(&fx.process_home).join("boss-event")
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(probe_authorization(&fx.process_home).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    for content in [
        "{}",
        "not JSON",
        r#"{"boss":{"PreToolUse":[{"hooks":[{"command":"true"}]}]}}"#,
    ] {
        fs::write(hooks_file_path(&fx.process_home), content).unwrap();
        assert!(probe_authorization(&fx.process_home).is_err());
    }
    fx.install(WorkerKind::Standard);
    let mut incomplete = binding.clone();
    incomplete["entries"]
        .as_array_mut()
        .unwrap()
        .retain(|e| e["name"] != "settings");
    fs::write(attestation_path(&fx.process_home), incomplete.to_string()).unwrap();
    assert!(verify_attestation(&fx.process_home).is_err());
}

#[test]
fn drift_and_policy_errors_still_forward_raw_progress() {
    let fx = Fixture::new("run-forward-drift");
    let log = fx.tmp.path().join("forwarded");
    write_exec(&fx.forwarder(), &format!("#!/bin/sh\ncat > '{}'\n", log.display()));
    fx.install(WorkerKind::Standard);
    fs::write(hooks_file_path(&fx.process_home), "{}").unwrap();
    let raw = run_command_payload("echo ok").to_string();
    assert_deny(&raw_authorize(&fx, &raw, Some(&fx.run_id)));
    assert_eq!(fs::read_to_string(&log).unwrap(), raw);
    fs::write(policy_path(&fx.process_home), "not JSON").unwrap();
    assert_deny(&raw_authorize(&fx, &raw, Some(&fx.run_id)));
    assert_eq!(fs::read_to_string(log).unwrap(), raw);
}

#[test]
fn missing_declared_artifacts_cannot_be_omitted_at_install() {
    let fx = Fixture::new("run-install-missing");
    let settings = super::super::super::home::session_settings_path_for_run(&fx.run_id).unwrap();
    let bytes = fs::read(&settings).unwrap();
    fs::remove_file(&settings).unwrap();
    assert!(
        install_authorization(
            &fx.process_home,
            WorkerKind::Standard,
            &fx.obs(),
            &fx.interception(|_| {})
        )
        .is_err()
    );
    fs::write(settings, bytes).unwrap();
    fs::remove_file(fx.path_guard()).unwrap();
    assert!(
        install_authorization(
            &fx.process_home,
            WorkerKind::Standard,
            &fx.obs(),
            &fx.interception(|_| {})
        )
        .is_err()
    );
}

#[test]
fn probe_tests_the_installed_command_and_reused_guard_deny() {
    let fx = Fixture::new("run-installed-probe");
    fx.install(WorkerKind::Standard);
    let hooks_path = hooks_file_path(&fx.process_home);
    let original = fs::read(&hooks_path).unwrap();
    let mut hooks: Value = serde_json::from_slice(&original).unwrap();
    hooks["boss"]["PreToolUse"][0]["hooks"][0]["command"] = json!(format!(
        "missing-hook-runner '{}'",
        adapter_path(&fx.process_home).display()
    ));
    fs::write(&hooks_path, hooks.to_string()).unwrap();
    let path = attestation_path(&fx.process_home);
    let mut binding: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for entry in binding["entries"].as_array_mut().unwrap() {
        if entry["name"] == "hooks" {
            entry["sha256"] = json!(hash_file(&hooks_path).unwrap());
        }
    }
    fs::write(path, binding.to_string()).unwrap();
    assert!(
        probe_authorization(&fx.process_home)
            .unwrap_err()
            .to_string()
            .contains("PreToolUse configuration does not match")
    );
    fs::write(hooks_path, original).unwrap();
    fx.install(WorkerKind::Standard);
    change_policy(&fx, |p| {
        p["guards"]
            .as_array_mut()
            .unwrap()
            .retain(|guard| guard["command"] != crate::claude::BOSS_LAUNCH_GUARD_COMMAND);
    });
    assert!(
        probe_authorization(&fx.process_home)
            .unwrap_err()
            .to_string()
            .contains("reused_guard")
    );
}

#[test]
fn rewrites_revisit_earlier_guards_and_translate_file_keys() {
    let fx = Fixture::new("run-rewrite-chain");
    write_exec(
        &fx.path_guard(),
        "#!/usr/bin/env python3\nimport json,sys\np=json.load(sys.stdin)\nblocked=p['tool_input'].get('file_path')=='blocked.txt'\nprint(json.dumps({'decision':'block' if blocked else 'approve','reason':'earlier guard rejected rewrite'}))\n",
    );
    let payload = json!({"toolCall":{"name":"write_file","args":{"Path":"safe.txt","Contents":"original"}}});
    for (target, expected) in [("changed.txt", "auto_approve"), ("blocked.txt", "deny")] {
        fx.install(WorkerKind::Standard);
        let script = format!(
            "import json; print(json.dumps({{'hookSpecificOutput':{{'updatedInput':{{'file_path':{target:?},'content':'updated'}}}}}}))"
        );
        change_policy(&fx, |p| {
            p["guards"].as_array_mut().unwrap().push(json!({
            "command":format!("python3 -c {}", boss_ssh_transport::shell_quote(&script)), "matcher":"*", "timeout_sec":30
        }))
        });
        let body = authorize(&fx.process_home, &payload).1;
        assert_eq!(decision(&body), expected, "{body}");
        if expected == "auto_approve" {
            assert_eq!(body["overwrite"], json!({"Path":"changed.txt","Contents":"updated"}));
        } else {
            assert_eq!(body["reason"], "earlier guard rejected rewrite");
        }
    }
}
