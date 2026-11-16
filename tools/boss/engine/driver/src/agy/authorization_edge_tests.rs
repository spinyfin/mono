use super::*;

#[test]
fn standard_shell_execution_positions_preserve_denials() {
    let fx = Fixture::new("run-execution-edges");
    fx.install(WorkerKind::Standard);
    for command in [
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
        let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(command), false);
        assert_eq!(decision(&body), "deny", "{command}: {body} {stderr}");
    }
    for command in ["rg hooks.json tools/boss", "cat some/other/hooks.json"] {
        let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(command), false);
        assert_eq!(decision(&body), "auto_approve", "{command}: {body} {stderr}");
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
            let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(command), false);
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
