use super::*;

#[test]
fn standard_shell_execution_positions_preserve_denials() {
    let fx = Fixture::new("run-execution-edges");
    fx.install(WorkerKind::Standard);
    for command in [
        "2>/dev/null sudo id",
        "2>/dev/null rm -rf /tmp/x",
        "2>/dev/null bossctl status",
        "2>/dev/null sh -c 'sudo id'",
        "2> /dev/null sudo id",
        "2>& 1 sudo id",
        "2>|/dev/null sudo id",
        "sudo>/dev/null id",
        "2>/dev/null git push",
        "2>/dev/null sh -c 'git push'",
        "2>/dev/null gh pr create --title x --body y",
        "sh -c 'echo `rm -rf /tmp/x`'",
        "sh -c 'echo $(sudo id)'",
        "sh -c 'echo $(git push)'",
        "sh -c \"sh -c 'echo \\`sudo id\\`'\"",
        "sh -c \"sh -c 'echo \\$(rm -rf /tmp/x)'\"",
        "sh -c \"sh -c 'echo \\$(git push)'\"",
        "P=\"$HOME/.gemini/config/hooks.json\"; cat \"$P\"",
        "P=\"$HOME/.gemini/config/hooks.json\"; mv \"$P\" x",
        "cat \"$HOME_CONFIG\"",
        "cat ~/../proc*/.gemini/config/boss-agy-auth-policy.json",
        "cat ~/../cont*/session-settings.json",
        "mv ~/../proc*/.gemini /tmp/g",
        "cd ~ && cd /tmp && cd - && mv .gemini x",
        "cd && ls",
        "cd ~- && ls",
        "cd ~+ && ls",
        "if true; then sudo id; fi",
        "if true; then rm -rf /tmp/x; fi",
        "echo 'sudo id' | bash",
        "bash <<'EOF'\nsudo id\nEOF",
        "eval 'sudo id'",
        "source /tmp/script",
        "find . -exec sudo id +",
        "printf 'rm -rf /tmp/x' | sh",
        "sh -c 'gh pr create --title x --body y'",
        "bash -lc 'gh pr create --title x --body y'",
        "sh -c 'git push'",
        "bash -lc 'git push'",
        "sh -c 'jj git push'",
        "bash -lc 'jj git push'",
        "cd ~ && mv .gemini x",
        "cd ~ && sh -c 'mv .gemini x'",
        "cd ~ && rm .gemini/config/hooks.*",
        "cat ~/.gem*/config/*",
    ] {
        let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(command));
        assert_eq!(decision(&body), "deny", "{command}: {body} {stderr}");
    }
    for command in [
        "rg hooks.json tools/boss",
        "cat some/other/hooks.json",
        "cd tools 2>/dev/null && ls",
        "echo ok >/dev/null",
        "echo ok 2>&1",
    ] {
        let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(command));
        assert_eq!(decision(&body), "auto_approve", "{command}: {body} {stderr}");
    }
}

#[test]
fn substitutions_are_denied_at_each_shell_depth() {
    let fx = Fixture::new("run-nested-substitutions");
    fx.install(WorkerKind::Standard);
    for forbidden in ["rm -rf /tmp/x", "sudo id", "git push"] {
        for substitution in [format!("`{forbidden}`"), format!("$({forbidden})")] {
            let mut command = format!("echo {substitution}");
            for _ in 0..2 {
                command = format!("sh -c {}", shell_quote(&command));
                let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(&command));
                assert_eq!(decision(&body), "deny", "{command}: {body} {stderr}");
            }
        }
    }
}

#[test]
fn readonly_redirects_distinguish_files_from_descriptors() {
    for kind in [WorkerKind::Reviewer, WorkerKind::Triage] {
        let fx = Fixture::new("run-redirect-edges");
        fx.install_with(kind, |c| {
            c.is_standard_worker = false;
            c.is_reviewer = kind == WorkerKind::Reviewer;
        });
        for (command, expected) in [
            ("2>/dev/null touch f", "deny"),
            ("awk '{print > \"f\"}' x", "deny"),
            ("sed -n 'w out' file", "deny"),
            ("find . -delete", "deny"),
            ("find . -fprint out", "deny"),
            ("find . -fprint0 out", "deny"),
            ("find . -fls out", "deny"),
            ("sort -o out file", "deny"),
            ("sort --output=out file", "deny"),
            ("python3.12 -c \"open('f','w')\"", "deny"),
            ("pypy3 -c \"open('f','w')\"", "deny"),
            ("php -r 'file_put_contents(\"f\", \"x\");'", "deny"),
            ("lua -e 'io.open(\"f\", \"w\")'", "deny"),
            ("deno eval 'Deno.writeTextFileSync(\"f\", \"x\")'", "deny"),
            ("bun -e 'require(\"fs\").writeFileSync(\"f\", \"x\")'", "deny"),
            ("osascript -e 'do shell script \"touch f\"'", "deny"),
            ("cd tools 2>/dev/null && ls", "auto_approve"),
            ("echo ok >/dev/null", "auto_approve"),
            ("rg x . 2>&1 | head", "auto_approve"),
            ("echo m >&2", "auto_approve"),
            ("echo m >&-", "auto_approve"),
            ("echo '>'", "auto_approve"),
            ("echo x > '&1'", "deny"),
            ("printf x 1<>tracked.txt", "deny"),
            (": <>newfile", "deny"),
            ("echo x >| f", "deny"),
            ("echo x &>> f", "deny"),
            ("sh -c 'echo x > f'", "deny"),
            ("python3 -c \"open('f','w')\"", "deny"),
            ("if true; then touch f; fi", "deny"),
        ] {
            let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(command));
            assert_eq!(decision(&body), expected, "{kind:?} {command}: {body} {stderr}");
        }
    }
}

fn rebind(fx: &Fixture) {
    let path = attestation_path(&fx.process_home);
    let mut attestation: AuthorizationAttestation = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for entry in &mut attestation.entries {
        entry.sha256 = hash_file(Path::new(&entry.path)).unwrap();
    }
    write_json_atomic(&path, &serde_json::to_value(attestation).unwrap()).unwrap();
}

#[test]
fn final_permission_settings_install_verify_and_probe_for_every_kind() {
    // The fixture installs the real renderer output for each kind, with the
    // kind flags set exactly as `ToolUseInterceptionConfig::from_permission_input`
    // sets them, so the review-guide guard is part of that kind's chain.
    for kind in ALL_KINDS {
        let fx = Fixture::new("run-final-settings");
        fx.install_with(kind, |c| {
            c.is_standard_worker = kind == WorkerKind::Standard;
            c.is_reviewer = kind == WorkerKind::Reviewer;
            c.is_review_guide = kind == WorkerKind::ReviewGuide;
        });
        if kind == WorkerKind::ReviewGuide {
            let policy: Value = serde_json::from_slice(&fs::read(policy_path(&fx.process_home)).unwrap()).unwrap();
            let chain = policy["guards"].to_string();
            assert!(
                chain.contains("review-guide"),
                "guide guard missing from chain: {chain}"
            );
        }
        verify_attestation(&fx.process_home).unwrap_or_else(|e| panic!("{kind:?}: {e:#}"));
        probe_authorization(&fx.process_home).unwrap_or_else(|e| panic!("{kind:?}: {e:#}"));
    }
}

const ALL_KINDS: [WorkerKind; 6] = [
    WorkerKind::Standard,
    WorkerKind::Reviewer,
    WorkerKind::Triage,
    WorkerKind::AnswerAgent,
    WorkerKind::ReviewGuide,
    WorkerKind::Progression,
];

const BOSS_BIN: &str = "/opt/boss-tools/boss";
const CUBE_BIN: &str = "/opt/boss-tools/cube";

fn authorize_tools(fx: &Fixture, command: &str) -> (Value, String) {
    let (_, body, stderr) = authorize_with_env(
        &fx.process_home,
        &run_command_payload(command),
        &[("BOSS_BIN", BOSS_BIN), ("CUBE_BIN", CUBE_BIN)],
    );
    (body, stderr)
}

#[test]
fn engine_executable_variables_are_resolved_before_the_guards() {
    // Fixtures hold the process-wide environment lock, so drop one before
    // creating the next.
    {
        let standard = Fixture::new("run-tool-vars-standard");
        standard.install(WorkerKind::Standard);
        for command in [
            "\"$BOSS_BIN\" propose done --outcome delivered --summary ok",
            "\"${BOSS_BIN}\" propose done --outcome delivered --summary ok",
            "\"$CUBE_BIN\" pr update --branch my-feature",
            "\"${CUBE_BIN}\" pr update --branch my-feature",
            "$BOSS_BIN pr status --json",
        ] {
            let (body, stderr) = authorize_tools(&standard, command);
            assert_eq!(decision(&body), "auto_approve", "{command}: {body} {stderr}");
        }
    }
    // A progression run's grant names the program `BOSS_BIN` holds, so every
    // spelling that resolves to it is the granted verb.
    let progression = Fixture::new("run-tool-vars-progression");
    progression.install(WorkerKind::Progression);
    for command in [
        "\"$BOSS_BIN\" propose done --outcome delivered --summary ok",
        "\"${BOSS_BIN}\" propose done --outcome delivered --summary ok",
    ] {
        let (body, stderr) = authorize_tools(&progression, command);
        assert_eq!(decision(&body), "auto_approve", "{command}: {body} {stderr}");
    }
}

#[test]
fn engine_executable_variables_cannot_evade_structural_denies() {
    let fx = Fixture::new("run-tool-vars-negative");
    fx.install(WorkerKind::Standard);
    for command in [
        "BOSS_BIN=/tmp/evil \"$BOSS_BIN\" propose done",
        "export CUBE_BIN=/tmp/evil; \"$CUBE_BIN\" pr update",
        "unset BOSS_BIN; \"$BOSS_BIN\" propose done",
        "env BOSS_BIN=/tmp/evil sh -c '\"$BOSS_BIN\" propose'",
        "read BOSS_BIN; \"$BOSS_BIN\" propose",
        "\"${BOSS_BIN:-sudo}\" id",
        "\"${BOSS_BIN/boss/sudo}\" id",
        "X=sudo; \"$X\" id",
        "\"$BOSS_BIN_OTHER\" propose",
        "\"$BOSS_BIN\" propose; sudo id",
        "\"$BOSS_BIN\" propose && git push",
        "\"$CUBE_BIN\" pr update $(sudo id)",
        "sh -c '\"$BOSS_BIN\" propose; sudo id'",
        "export BOSS_{BIN,X}=/bin/sh; \"$BOSS_BIN\" -c 'sudo id'",
        "declare -n r=BOSS_BIN; r=/bin/sh; \"$BOSS_BIN\" -c 'sudo id'",
        "printf -v BOSS_BIN %s /bin/sh; \"$BOSS_BIN\" -c 'sudo id'",
        "export \"$NAME=/bin/sh\"; \"$BOSS_BIN\" -c 'sudo id'",
    ] {
        let (body, stderr) = authorize_tools(&fx, command);
        assert_eq!(decision(&body), "deny", "{command}: {body} {stderr}");
    }
    for value in ["", "boss"] {
        let (_, body, stderr) = authorize_with_env(
            &fx.process_home,
            &run_command_payload("\"$BOSS_BIN\" propose done"),
            &[("BOSS_BIN", value)],
        );
        assert_eq!(decision(&body), "deny", "{body} {stderr}");
    }
}

#[test]
fn non_writing_kinds_deny_shell_mediated_writes_on_allowed_prefixes() {
    for kind in [
        WorkerKind::Reviewer,
        WorkerKind::Triage,
        WorkerKind::AnswerAgent,
        WorkerKind::ReviewGuide,
        WorkerKind::Progression,
    ] {
        let fx = Fixture::new("run-nonwriting-redirects");
        fx.install_with(kind, |c| {
            c.is_standard_worker = false;
            c.is_reviewer = kind == WorkerKind::Reviewer;
        });
        for command in [
            "gh pr view 1 --repo spinyfin/mono > ./out.txt",
            "gh pr view 1 --repo spinyfin/mono >> ./out.txt",
            "gh pr view 1 --repo spinyfin/mono >| ./out.txt",
            "\"$BOSS_BIN\" pr status > ./out.txt",
            "gh pr view 1 | tee out.txt",
            "gh pr view 1; touch out.txt",
            "sh -c 'gh pr view 1 > out.txt'",
            "sed -n '1,5p' f > out.txt",
            "sed -i s/a/b/ f",
            "sed -n 'w out' f",
            "find . -fprint out",
            "sort -o out f",
        ] {
            let (body, stderr) = authorize_tools(&fx, command);
            assert_eq!(decision(&body), "deny", "{kind:?} {command}: {body} {stderr}");
        }
    }
    // The same commands without a write pass the structural checks. Whether
    // one then runs is the kind's own command grants: a covered command is
    // approved, and the rest keep the deny-by-default answer.
    const GRANTED: &str = "auto_approve";
    const UNGRANTED: &str = "deny_unless_prior_grant";
    for (kind, gh, boss, sed) in [
        (WorkerKind::AnswerAgent, UNGRANTED, UNGRANTED, UNGRANTED),
        (WorkerKind::ReviewGuide, UNGRANTED, UNGRANTED, GRANTED),
        (WorkerKind::Progression, GRANTED, GRANTED, UNGRANTED),
    ] {
        let fx = Fixture::new("run-nonwriting-reads");
        fx.install_with(kind, |c| {
            c.is_standard_worker = false;
        });
        for (command, expected) in [
            ("gh pr view 1 --repo spinyfin/mono 2>&1", gh),
            ("gh pr view 1 > /dev/null", gh),
            ("\"$BOSS_BIN\" pr status", boss),
            ("sed -n '1,5p' f", sed),
        ] {
            let (body, stderr) = authorize_tools(&fx, command);
            assert_eq!(decision(&body), expected, "{kind:?} {command}: {body} {stderr}");
        }
    }
}

#[test]
fn invalid_permission_posture_is_rejected_with_consistent_hashes() {
    for (key, value) in [
        ("agentMode", None),
        ("agentMode", Some(json!("auto"))),
        ("toolPermission", None),
        ("toolPermission", Some(json!("always-proceed"))),
        (
            "permissions",
            Some(json!({"allow": [], "deny": [], "ask": ["command(ls)"]})),
        ),
        (
            "permissions",
            Some(json!({"allow": [1], "deny": ["write_file(/tmp/fence)"]})),
        ),
        ("permissions", Some(json!({"allow": [], "deny": ["command(rm)"]}))),
        (
            "permissions",
            Some(json!({"allow": ["command(gh pr view)"], "deny": ["write_file(/tmp/fence)"]})),
        ),
        (
            "permissions",
            Some(json!({"allow": ["invalid"], "deny": ["write_file(/tmp/fence)"]})),
        ),
        ("permissions", Some(json!({"allow": [], "deny": []}))),
        ("enableTelemetry", Some(json!(true))),
    ] {
        let fx = Fixture::new("run-invalid-posture");
        fx.install(WorkerKind::Standard);
        let path = session_settings_path_for_run(&fx.run_id).unwrap();
        let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        if let Some(value) = value {
            settings[key] = value;
        } else {
            settings.as_object_mut().unwrap().remove(key);
        }
        write_json_atomic(&path, &settings).unwrap();
        rebind(&fx);
        assert!(verify_attestation(&fx.process_home).is_err(), "{settings}");
        assert!(probe_authorization(&fx.process_home).is_err(), "{settings}");
        assert!(
            install_authorization(
                &fx.process_home,
                WorkerKind::Standard,
                &fx.obs(),
                &fx.interception(|_| {})
            )
            .is_err(),
            "{settings}"
        );
    }
}

#[test]
fn semantic_configuration_errors_fail_even_with_consistent_hashes() {
    for (pointer, value) in [
        ("/boss/PreToolUse/0/matcher", json!("no-native-tool")),
        ("/boss/PreToolUse/0/hooks/0/type", json!("invalid")),
    ] {
        let fx = Fixture::new("run-invalid-hook");
        fx.install(WorkerKind::Standard);
        let path = hooks_file_path(&fx.process_home);
        let mut hooks: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        *hooks.pointer_mut(pointer).unwrap() = value;
        write_json_atomic(&path, &hooks).unwrap();
        rebind(&fx);
        assert!(probe_authorization(&fx.process_home).is_err());
    }
    for settings in ["not json", "{}", r#"{"trustedWorkspaces":[],"enableTelemetry":true}"#] {
        let fx = Fixture::new("run-invalid-settings");
        fs::write(session_settings_path_for_run(&fx.run_id).unwrap(), settings).unwrap();
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
}

#[test]
fn probe_uses_a_private_events_socket() {
    let fx = Fixture::new("run-probe-socket");
    let log = fx.tmp.path().join("forwarded-sockets");
    write_exec(
        &fx.forwarder(),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$BOSS_EVENTS_SOCKET\" >> '{}'\ncat >/dev/null\n",
            log.display()
        ),
    );
    fx.install(WorkerKind::Standard);
    probe_authorization(&fx.process_home).unwrap();
    let sockets = fs::read_to_string(log).unwrap();
    assert_eq!(sockets.lines().count(), 3);
    assert!(
        sockets
            .lines()
            .all(|line| line.contains("/agy-probe-") && line != fx.obs().events_socket_path.to_str().unwrap()),
        "{sockets}"
    );
}

#[test]
fn probe_kills_a_hung_forwarder_at_its_deadline() {
    let fx = Fixture::new("run-hung-probe");
    write_exec(&fx.forwarder(), "#!/bin/sh\nsleep 120\n");
    fx.install(WorkerKind::Standard);
    let started = std::time::Instant::now();
    let error = probe_authorization_with_timeout(&fx.process_home, std::time::Duration::from_millis(500)).unwrap_err();
    assert!(error.to_string().contains("exceeded 500ms timeout"), "{error:#}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[test]
fn engine_executable_variable_names_are_plain_data_outside_binding_positions() {
    let fx = Fixture::new("run-tool-var-names");
    fx.install(WorkerKind::Standard);
    for command in [
        "rg BOSS_BIN tools/",
        "grep -rn CUBE_BIN .",
        "rg 'BOSS_BIN=' tools/",
        "printenv BOSS_BIN",
        "grep -E 'foo$' f",
        "awk '{print $1}' f",
        "rg 'bar$' .",
        "grep -n x'$' f",
        "grep -n x\\$ f",
        "echo cost$",
        "printf '%s' a$",
        "grep \"$X\" f",
        "sed -n '10,$p' f",
    ] {
        let (body, stderr) = authorize_tools(&fx, command);
        assert_eq!(decision(&body), "auto_approve", "{command}: {body} {stderr}");
    }
    let (body, stderr) = authorize_tools(&fx, "cat \"$HOME_CONFIG\"");
    assert_eq!(decision(&body), "deny", "{body} {stderr}");
}

#[test]
fn non_writing_kinds_deny_rebinding_and_sed_option_permutation() {
    for kind in [WorkerKind::Reviewer, WorkerKind::AnswerAgent, WorkerKind::ReviewGuide] {
        let fx = Fixture::new("run-nonwriting-rebinding");
        fx.install_with(kind, |c| {
            c.is_standard_worker = false;
            c.is_reviewer = kind == WorkerKind::Reviewer;
            c.is_review_guide = kind == WorkerKind::ReviewGuide;
        });
        for command in [
            "export BOSS_{BIN,X}=/bin/sh; \"$BOSS_BIN\" -c 'touch x'",
            "declare -n r=BOSS_BIN; r=/bin/sh; \"$BOSS_BIN\" -c 'touch x'",
            "sed -n 1p -i f",
            "sed -n 1p -e 'w out' f",
        ] {
            let (body, stderr) = authorize_tools(&fx, command);
            assert_eq!(decision(&body), "deny", "{kind:?} {command}: {body} {stderr}");
        }
    }
    for kind in [WorkerKind::AnswerAgent, WorkerKind::Progression] {
        let fx = Fixture::new("run-nonwriting-sed-range");
        fx.install_with(kind, |c| {
            c.is_standard_worker = false;
        });
        let (body, stderr) = authorize_tools(&fx, "sed -n '10,$p' f");
        assert_ne!(decision(&body), "deny", "{kind:?}: {body} {stderr}");
    }
}

#[test]
fn quoted_worker_clis_and_literal_dollar_operands_are_not_fail_closed() {
    let fx = Fixture::new("run-dollar-ok");
    fx.install(WorkerKind::Standard);
    for command in [
        r#""$BOSS_BIN" propose done --outcome completed --summary x"#,
        r#"grep -n 'x$' f"#,
        r#"awk '{print $1}' f"#,
        "cat <<'EOF'\n`whoami` $(id)\nEOF",
    ] {
        let (code, body, stderr) = authorize(&fx.process_home, &run_command_payload(command));
        assert_eq!(code, 0, "{command} stderr={stderr}");
        assert_eq!(decision(&body), "auto_approve", "{command}: {body} {stderr}");
    }
    let (code, body, stderr) = authorize(&fx.process_home, &run_command_payload(r#"cat "$HOME_CONFIG""#));
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "deny", "{body} {stderr}");
    assert!(
        body["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("unresolved shell path expansion"),
        "{body}"
    );
}

#[test]
fn cube_pr_create_reaches_pr_redirect_and_checkleft() {
    let fx = Fixture::new("run-cube-pr");
    let marker = fx.tmp.path().join("checkleft-reached");
    let checkleft = fx.tmp.path().join("boss-checkleft-push-guard.py");
    write_exec(
        &checkleft,
        &format!(
            "#!/usr/bin/env python3\nimport json,pathlib\npathlib.Path({:?}).write_text('1')\nprint(json.dumps({{'decision':'block','reason':'checkleft-reached'}}))\n",
            marker
        ),
    );
    fx.install_with(WorkerKind::Standard, |c| {
        c.checkleft_guard_script = Some(checkleft);
    });
    let (code, body, stderr) = authorize(
        &fx.process_home,
        &run_command_payload(r#""$CUBE_BIN" pr create --title x --body-file f"#),
    );
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "deny", "{body} {stderr}");
    let reason = body["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("checkleft-reached"),
        "must reach checkleft after PR-redirect: {body} {stderr}"
    );
    assert!(!reason.contains("unresolved shell path expansion"), "{body}");
    assert_eq!(fs::read_to_string(&marker).unwrap(), "1");
}

#[test]
fn protected_tilde_user_and_symlink_glob_are_denied() {
    let fx = Fixture::new("run-tilde-glob");
    fx.install(WorkerKind::Standard);
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "root".into());
    let tilde = format!("cat ~{user}/.gemini/config/hooks.json");
    let (code, body, stderr) = authorize(&fx.process_home, &run_command_payload(&tilde));
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "deny", "{tilde}: {body} {stderr}");
    assert!(
        body["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("unresolved shell path expansion"),
        "{body}"
    );

    let real = fx.tmp.path().join("real");
    fs::create_dir_all(real.join("protected")).unwrap();
    std::os::unix::fs::symlink(&real, fx.tmp.path().join("link")).unwrap();
    let policy = policy_path(&fx.process_home);
    let mut doc: Value = serde_json::from_slice(&fs::read(&policy).unwrap()).unwrap();
    doc["protected_paths"]
        .as_array_mut()
        .unwrap()
        .push(json!(real.join("protected")));
    write_json_atomic(&policy, &doc).unwrap();
    rebind(&fx);
    let globbed = format!("{}/lin?/protected/hooks.json", fx.tmp.path().display());
    let glob_cmd = format!("cat {globbed}");
    let (code, body, stderr) = authorize(&fx.process_home, &run_command_payload(&glob_cmd));
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(decision(&body), "deny", "{glob_cmd}: {body} {stderr}");
    assert!(
        body["reason"].as_str().unwrap_or_default().contains("protected"),
        "{body}"
    );
}
