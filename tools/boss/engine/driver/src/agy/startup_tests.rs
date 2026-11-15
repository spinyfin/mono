use super::*;

#[test]
fn startup_accepts_attested_chain_without_session_start() {
    let fx = Fixture::new("startup-positive");
    fx.install(WorkerKind::Standard);
    let report = verify_startup_authorization(&fx.obs(), WorkerKind::Standard).unwrap();
    assert!(report.ok);
    assert_eq!(report.positive_decision, "auto_approve");
    assert_eq!(report.denied_decision, "deny");
    assert!(!crate::AgentDriver::launch_ready(&crate::agy::AgyDriver::default()));
}

#[test]
fn startup_refuses_provisioned_canary_without_authorization() {
    let fx = Fixture::new("startup-canary");
    assert!(verify_startup_authorization(&fx.obs(), WorkerKind::Standard).is_err());
}

#[test]
fn startup_refuses_missing_attestation_and_disabled_adapter() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fixture::new("startup-disabled");
    fx.install(WorkerKind::Standard);
    fs::remove_file(attestation_path(&fx.process_home)).unwrap();
    assert!(verify_startup_authorization(&fx.obs(), WorkerKind::Standard).is_err());
    fx.install(WorkerKind::Standard);
    fs::set_permissions(adapter_path(&fx.process_home), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(verify_startup_authorization(&fx.obs(), WorkerKind::Standard).is_err());
}

#[test]
fn startup_refuses_missing_disabled_malformed_and_changed_configuration() {
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
            assert!(
                verify_startup_authorization(&fx.obs(), WorkerKind::Standard).is_err(),
                "{artifact}: {mutation}"
            );
        }
    }
    write_exec(&fx.forwarder(), "#!/bin/sh\ncat >/dev/null\n");
    write_exec(&fx.path_guard(), approve_guard());
    fx.install(WorkerKind::Standard);
    let path = hooks_file_path(&fx.process_home);
    let mut hooks: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    hooks["boss"]["PreToolUse"] = json!([]);
    fs::write(path, hooks.to_string()).unwrap();
    assert!(verify_startup_authorization(&fx.obs(), WorkerKind::Standard).is_err());
}

#[test]
fn startup_binds_expected_kind_and_delivery_identity() {
    let fx = Fixture::new("startup-identity");
    fx.install(WorkerKind::Standard);
    assert!(verify_startup_authorization(&fx.obs(), WorkerKind::Reviewer).is_err());
    let mut obs = fx.obs();
    obs.lease_id = "other-lease".into();
    assert!(verify_startup_authorization(&obs, WorkerKind::Standard).is_err());
    let mut obs = fx.obs();
    obs.events_socket_path = fx.tmp.path().join("other.sock");
    assert!(verify_startup_authorization(&obs, WorkerKind::Standard).is_err());
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
    let error = verify_startup_authorization(&fx.obs(), WorkerKind::Standard).unwrap_err();
    assert!(
        error.to_string().contains("complete expected lifecycle wiring"),
        "{error:#}"
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
    let error = verify_startup_authorization(&fx.obs(), WorkerKind::Standard).unwrap_err();
    assert!(error.to_string().contains("authorization liveness"), "{error:#}");
}
