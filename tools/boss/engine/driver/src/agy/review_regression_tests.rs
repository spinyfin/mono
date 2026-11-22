use super::*;

#[test]
fn python_scratch_traversal_and_clustered_programs_are_inspected() {
    let fx = Fixture::new("python-path-options");
    fx.install(WorkerKind::Standard);
    let home = fx.process_home.display();
    let scratch = format!("{home}/.gemini/antigravity-cli/brain/c1/scratch");
    fs::create_dir_all(&scratch).unwrap();
    let run_command_payload = |command: &str| {
        let mut payload = super::run_command_payload(command);
        payload["conversationId"] = json!("c1");
        payload
    };
    for path in [
        format!("{scratch}/../../../settings.json"),
        format!("{scratch}/../../c2/scratch/x.py"),
        format!("{scratch}/../.system_generated/logs/transcript_full.jsonl"),
        format!("{home}/.gemini/antigravity-cli/brain/c1/.system_generated/steps/../../logs/transcript_full.jsonl"),
        format!("{scratch}/../../../../config/hooks.json"),
        format!("{home}/.gemini/antigravity-cli/settings.json"),
        format!("{home}/.gemini/config/hooks.json"),
        format!("{home}/.gemini/antigravity-cli/brain/c2/scratch/x.py"),
    ] {
        for prefix in [
            "python3 -c",
            "python3 -Ic",
            "python3 -Bc",
            "python3 -uc",
            "python3 -sc",
            "python3 -Sc",
            "uv run python3 -c",
            "poetry run python3 -c",
            "pipx run python3 -c",
            "python3 -W ignore -X dev -Ic",
        ] {
            let command = format!("{prefix} 'print(open(\"{path}\").read())'");
            let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(&command));
            assert_eq!(decision(&body), "deny", "{command}: {body} {stderr}");
        }
        for command in [
            format!("python3 - <<'PY'\nprint(open(\"{path}\").read())\nPY"),
            format!("python3 -Ic'print(open(\"{path}\").read())'"),
        ] {
            let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(&command));
            assert_eq!(decision(&body), "deny", "{command}: {body} {stderr}");
        }
    }
    for command in [
        "python3 -Ic 'print(1)'",
        "python3 -Bc 'print(1)'",
        "python3 -uc 'print(1)'",
        "uv run python3 -c 'print(1)'",
    ] {
        let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(command));
        assert_eq!(decision(&body), "auto_approve", "{command}: {body} {stderr}");
    }
    let command = format!("python3 -Ic 'print(open(\"{scratch}/lib.rs\").read())'");
    let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(&command));
    assert_eq!(decision(&body), "auto_approve", "{command}: {body} {stderr}");
}

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
    let log = fx.tmp.path().join("checkleft.log");
    // The real push guard, with a counting stub standing in for checkleft.
    let guard = fx.tmp.path().join("boss-checkleft-push-guard.py");
    write_exec(&guard, crate::CHECKLEFT_PUSH_GUARD_SCRIPT);
    let stub = fx.tmp.path().join("checkleft-stub");
    write_exec(&stub, &format!("#!/bin/sh\necho run >> '{}'\nexit 0\n", log.display()));
    fx.install_with(WorkerKind::Standard, |c| c.checkleft_guard_script = Some(guard));
    // Isolate scheduling from the independent guard that forbids bare pushes.
    super::failure_tests::change_policy(&fx, |p| {
        p["guards"].as_array_mut().unwrap().retain(|g| g["run_once"] == true);
    });
    let runs = |command: &str| -> usize {
        let _ = fs::remove_file(&log);
        let (_, body, stderr) = authorize_with_env(
            &fx.process_home,
            &run_command_payload(command),
            &[("BOSS_CHECKLEFT_BIN", stub.to_str().unwrap())],
        );
        assert_eq!(decision(&body), "auto_approve", "{command}: {body} {stderr}");
        fs::read_to_string(&log).map(|l| l.lines().count()).unwrap_or(0)
    };
    for command in [
        "jj git push 2>&1",
        "jj git push --bookmark x && echo ok 2>&1",
        "echo ok 2>/dev/null; jj git push 2>&1 | tail",
        "echo ok 2>/dev/null && sh -c 'git -C /tmp/repo push 2>&1'",
        "sh -c 'jj git push 2>&1'",
    ] {
        assert_eq!(runs(command), 1, "{command}");
    }
    assert_eq!(runs("echo ok 2>&1"), 0);
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
