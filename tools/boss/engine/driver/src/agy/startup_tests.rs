use super::*;
use std::path::Path;

fn assert_startup_err(input: &crate::PermissionInput, needle: &str, label: &str) {
    let error = verify_startup_authorization(input).unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains(needle), "{label}: expected {needle:?} in {text}");
}

fn reattest_from_policy(process_home: &Path) {
    let policy: Value = serde_json::from_slice(&fs::read(policy_path(process_home)).unwrap()).unwrap();
    let required = policy["required_artifacts"].as_object().unwrap();
    let path = attestation_path(process_home);
    let mut binding: AuthorizationAttestation = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    binding.entries = required
        .iter()
        .map(|(name, expected)| {
            let artifact = expected.as_str().unwrap();
            AttestationEntry {
                name: name.clone(),
                path: artifact.to_owned(),
                sha256: hash_file(Path::new(artifact)).unwrap(),
            }
        })
        .collect();
    fs::write(&path, serde_json::to_vec(&binding).unwrap()).unwrap();
}

#[test]
fn startup_accepts_attested_chain_without_session_start() {
    let fx = Fixture::new("startup-positive");
    fx.complete_install(WorkerKind::Standard);
    let policy: Value = serde_json::from_slice(&fs::read(policy_path(&fx.process_home)).unwrap()).unwrap();
    for name in ["guard:path", "guard:git_store", "guard:checkleft"] {
        assert!(
            policy["required_artifacts"].as_object().unwrap().contains_key(name),
            "complete positive case must attest {name}: {policy:#}"
        );
    }
    let report = verify_startup_authorization(&fx.complete_input(WorkerKind::Standard)).unwrap();
    assert!(report.ok);
    assert_eq!(report.positive_decision, "auto_approve");
    assert_eq!(report.denied_decision, "deny");
    assert!(crate::AgentDriver::launch_implemented(&crate::agy::AgyDriver::default()));
}

#[test]
fn startup_refuses_provisioned_canary_without_authorization() {
    let fx = Fixture::new("startup-canary");
    assert_startup_err(&fx.input(WorkerKind::Standard), "authorization binding", "canary");
}

#[test]
fn startup_refuses_missing_attestation_and_disabled_adapter() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fixture::new("startup-disabled");
    fx.install(WorkerKind::Standard);
    fs::remove_file(attestation_path(&fx.process_home)).unwrap();
    assert_startup_err(&fx.input(WorkerKind::Standard), "attestation", "missing attestation");
    fx.install(WorkerKind::Standard);
    fs::set_permissions(adapter_path(&fx.process_home), fs::Permissions::from_mode(0o600)).unwrap();
    assert_startup_err(
        &fx.input(WorkerKind::Standard),
        "adapter is not executable",
        "disabled adapter",
    );
}

#[test]
fn startup_refuses_missing_disabled_malformed_and_changed_configuration() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fixture::new("startup-invalid");
    for artifact in ["hooks", "adapter", "policy", "settings", "forwarder", "guard:path"] {
        for mutation in ["missing", "malformed", "changed"] {
            // Restore the fixture's external artifacts before reinstalling.
            write_exec(&fx.forwarder(), "#!/bin/sh\ncat >/dev/null\n");
            write_exec(&fx.path_guard(), approve_guard());
            fx.install(WorkerKind::Standard);
            let attestation = verify_attestation(&fx.process_home).unwrap();
            let path = &attestation.entries.iter().find(|e| e.name == artifact).unwrap().path;
            match mutation {
                "missing" => fs::remove_file(path).unwrap(),
                "malformed" => fs::write(path, "{broken").unwrap(),
                _ => {
                    let mut bytes = fs::read(path).unwrap();
                    bytes.push(b' ');
                    fs::write(path, bytes).unwrap();
                }
            }
            let error = verify_startup_authorization(&fx.input(WorkerKind::Standard)).unwrap_err();
            let text = format!("{error:#}");
            assert!(
                text.contains(artifact),
                "{artifact}: {mutation} should name the artifact, got {text}"
            );
            let kind_ok = match mutation {
                "missing" => text.contains("missing") || text.contains("reading"),
                "malformed" => {
                    text.contains("malformed")
                        || text.contains("changed")
                        || text.contains("parsing")
                        || text.contains("binding")
                }
                _ => text.contains("changed") || text.contains("malformed") || text.contains("guard chain"),
            };
            assert!(
                kind_ok,
                "{artifact}: {mutation} should name the failure kind, got {text}"
            );
        }
    }
    write_exec(&fx.forwarder(), "#!/bin/sh\ncat >/dev/null\n");
    write_exec(&fx.path_guard(), approve_guard());
    fx.install(WorkerKind::Standard);
    // Install binds a run-private copy of the forwarder; disable that copy.
    let private_forwarder = private_forwarder_config(&fx.process_home, &fx.obs()).forwarder_binary;
    fs::set_permissions(private_forwarder, fs::Permissions::from_mode(0o600)).unwrap();
    assert_startup_err(
        &fx.input(WorkerKind::Standard),
        "forwarder is not executable",
        "disabled forwarder",
    );
    write_exec(&fx.forwarder(), "#!/bin/sh\ncat >/dev/null\n");
    fx.install(WorkerKind::Standard);
    let path = hooks_file_path(&fx.process_home);
    let mut hooks: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    hooks["boss"]["PreToolUse"][0]["hooks"][0]["command"] = json!("true");
    fs::write(&path, hooks.to_string()).unwrap();
    assert_startup_err(
        &fx.input(WorkerKind::Standard),
        "PreToolUse",
        "PreToolUse replaced by true",
    );
    write_exec(&fx.forwarder(), "#!/bin/sh\ncat >/dev/null\n");
    write_exec(&fx.path_guard(), approve_guard());
    fx.install(WorkerKind::Standard);
    let path = hooks_file_path(&fx.process_home);
    let mut hooks: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    hooks["boss"]["PreToolUse"] = json!([]);
    fs::write(path, hooks.to_string()).unwrap();
    assert_startup_err(&fx.input(WorkerKind::Standard), "PreToolUse", "emptied PreToolUse");
}

#[test]
fn startup_binds_expected_kind_and_delivery_identity() {
    let fx = Fixture::new("startup-identity");
    fx.install(WorkerKind::Standard);
    assert_startup_err(&fx.input(WorkerKind::Reviewer), "guard chain", "other kind");
    let mut input = fx.input(WorkerKind::Standard);
    input.lease_id = "other-lease".into();
    assert_startup_err(&input, "guard chain", "other lease");
    let mut input = fx.input(WorkerKind::Standard);
    input.events_socket_path = fx.tmp.path().join("other.sock");
    assert_startup_err(&input, "guard chain", "other socket");
}

#[test]
fn startup_refuses_attested_but_incomplete_lifecycle_wiring() {
    let fx = Fixture::new("startup-lifecycle");
    fx.install(WorkerKind::Standard);
    let path = hooks_file_path(&fx.process_home);
    let mut hooks: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    hooks["boss"]["Stop"] = json!([]);
    fs::write(&path, hooks.to_string()).unwrap();
    let attestation_path = attestation_path(&fx.process_home);
    let mut binding: AuthorizationAttestation = serde_json::from_slice(&fs::read(&attestation_path).unwrap()).unwrap();
    binding.entries.iter_mut().find(|e| e.name == "hooks").unwrap().sha256 = hash_file(&path).unwrap();
    fs::write(attestation_path, serde_json::to_vec(&binding).unwrap()).unwrap();
    verify_attestation(&fx.process_home).unwrap();
    assert_startup_err(
        &fx.input(WorkerKind::Standard),
        "complete expected lifecycle wiring",
        "incomplete lifecycle",
    );
}

#[test]
fn startup_requires_live_probe_even_with_valid_binding() {
    let fx = Fixture::new("startup-liveness");
    write_exec(
        &fx.path_guard(),
        "#!/bin/sh\necho '{\"decision\":\"block\",\"reason\":\"deny every call\"}'\n",
    );
    fx.install(WorkerKind::Standard);
    verify_attestation(&fx.process_home).unwrap();
    assert_startup_err(&fx.input(WorkerKind::Standard), "authorization liveness", "dead probe");
}

#[test]
fn startup_refuses_consistently_attested_install_that_omits_a_required_guard() {
    let fx = Fixture::new("startup-omit-guard");
    fx.complete_install(WorkerKind::Standard);
    let path = policy_path(&fx.process_home);
    let mut policy: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let checkleft = fx.checkleft_guard().display().to_string();
    policy["guards"]
        .as_array_mut()
        .unwrap()
        .retain(|guard| !guard["command"].as_str().unwrap_or_default().contains(&checkleft));
    policy["required_artifacts"]
        .as_object_mut()
        .unwrap()
        .remove("guard:checkleft");
    fs::write(&path, serde_json::to_string_pretty(&policy).unwrap()).unwrap();
    reattest_from_policy(&fx.process_home);
    verify_attestation(&fx.process_home).unwrap();
    assert_startup_err(
        &fx.complete_input(WorkerKind::Standard),
        "expected guard chain",
        "omitted checkleft",
    );
}

#[test]
fn startup_refuses_consistently_attested_install_with_another_kind_settings() {
    let fx = Fixture::new("startup-wrong-settings");
    fx.complete_install(WorkerKind::Standard);
    super::super::super::test_environment::write_final_settings(
        &fx.run_id,
        &fx.workspace(),
        WorkerKind::Reviewer,
        &fx.obs().events_socket_path,
    );
    reattest_from_policy(&fx.process_home);
    verify_attestation(&fx.process_home).unwrap();
    assert_startup_err(
        &fx.complete_input(WorkerKind::Standard),
        "per-kind render",
        "reviewer settings on standard",
    );
}

#[test]
fn startup_refuses_wrong_revision_flag() {
    let fx = Fixture::new("startup-revision");
    fx.complete_install(WorkerKind::Standard);
    verify_attestation(&fx.process_home).unwrap();
    let mut input = fx.complete_input(WorkerKind::Standard);
    input.execution_kind = "revision_implementation".into();
    assert_startup_err(&input, "expected guard chain", "wrong revision flag");
}
