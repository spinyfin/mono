use super::*;

#[test]
fn shared_forwarder_updates_do_not_change_live_hooks() {
    let fx = Fixture::new("forwarder-update");
    let log = fx.tmp.path().join("forwarded.json");
    write_exec(&fx.forwarder(), &format!("#!/bin/sh\ncat > '{}'\n", log.display()));
    fx.install(WorkerKind::Standard);
    fs::write(fx.forwarder(), "broken replacement").unwrap();
    verify_attestation(&fx.process_home).unwrap();
    let payload = run_command_payload("echo ok");
    let (_, body, stderr) = authorize(&fx.process_home, &payload);
    assert_eq!(decision(&body), "auto_approve", "{body} {stderr}");
    assert_eq!(fs::read_to_string(log).unwrap(), payload.to_string());
    let hooks: Value = serde_json::from_slice(&fs::read(hooks_file_path(&fx.process_home)).unwrap()).unwrap();
    assert_eq!(hooks["boss"], json!(authorization_hooks_map(&fx.obs()).unwrap()));
    let private = gemini_config_dir(&fx.process_home).join("boss-event");
    for event in AGY_HOOK_EVENTS {
        assert!(hooks["boss"][event].to_string().contains(private.to_str().unwrap()));
    }
}

#[test]
fn redirected_push_runs_checkleft_once_on_the_complete_command() {
    let fx = Fixture::new("checkleft-once");
    let log = fx.tmp.path().join("checkleft.jsonl");
    let checkleft = fx.tmp.path().join("checkleft.py");
    write_exec(
        &checkleft,
        &format!(
            "#!/usr/bin/env python3\nimport json, sys\nwith open({:?}, 'a') as f:\n    f.write(json.dumps(json.load(sys.stdin)) + '\\n')\nprint(json.dumps({{'decision':'approve'}}))\n",
            log.to_str().unwrap()
        ),
    );
    fx.install_with(WorkerKind::Standard, |c| c.checkleft_guard_script = Some(checkleft));
    // Isolate scheduling from the independent guard that forbids bare pushes.
    super::failure_tests::change_policy(&fx, |p| {
        p["guards"].as_array_mut().unwrap().retain(|g| g["run_once"] == true);
    });
    for command in [
        "jj git push 2>&1",
        "echo ok 2>/dev/null; jj git push 2>&1 | tail",
        "sh -c 'jj git push 2>&1'",
    ] {
        let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(command));
        assert_eq!(decision(&body), "auto_approve", "{body} {stderr}");
        let lines = fs::read_to_string(&log).unwrap();
        assert_eq!(lines.lines().count(), 1, "{lines}");
        let input: Value = serde_json::from_str(lines.trim()).unwrap();
        assert!(input["tool_input"]["command"].as_str().unwrap().contains(command));
        fs::remove_file(&log).unwrap();
    }
}

#[test]
fn reviewer_can_only_write_the_exported_result_artifact() {
    let fx = Fixture::new("reviewer-result");
    fx.install_with(WorkerKind::Reviewer, |c| {
        c.is_standard_worker = false;
        c.is_reviewer = true;
    });
    let result = fx.tmp.path().join("result.json");
    let alias = fx.tmp.path().join("alias.json");
    std::os::unix::fs::symlink(&result, &alias).unwrap();
    for (tool, path, expected) in [
        ("write_file", result.clone(), "auto_approve"),
        ("write_file", alias, "auto_approve"),
        ("edit_file", result.clone(), "deny"),
        ("write_file", fx.workspace().join("source.rs"), "deny"),
        ("write_file", fx.tmp.path().join("other.json"), "deny"),
    ] {
        let payload = json!({"toolCall": {"name": tool, "args": {"Path": path, "Contents": "{}"}}});
        let (_, body, stderr) = authorize_with_env(
            &fx.process_home,
            &payload,
            &[("BOSS_STRUCTURED_OUTPUT", result.to_str().unwrap())],
        );
        assert_eq!(decision(&body), expected, "{body} {stderr}");
    }
    let payload = json!({"toolCall": {"name": "write_file", "args": {"Path": result, "Contents": "{}"}}});
    assert_eq!(decision(&authorize(&fx.process_home, &payload).1), "deny");
}

#[test]
fn command_name_queries_are_not_executions() {
    let fx = Fixture::new("command-query");
    fx.install(WorkerKind::Standard);
    for command in ["command -v cube", "command -V boss", "command -v bossctl"] {
        let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(command));
        assert_eq!(decision(&body), "auto_approve", "{command}: {body} {stderr}");
    }
    let (_, body, _) = authorize(&fx.process_home, &run_command_payload("command bossctl"));
    assert_eq!(decision(&body), "deny");
}
