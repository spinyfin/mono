//! Exercise Codex's semantic config loader, not just TOML deserialization.
//! Bazel supplies the checksum-pinned deployed version on every CI platform.

use super::render_review_guide_config;
use boss_engine_codex_hook_trust::{ArmRequest, CommandHookSpec, HookEvent, arm_and_attest};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

#[test]
fn review_guide_config_arms_hooks_on_pinned_codex() {
    let binary = PathBuf::from(std::env::var_os("BOSS_TEST_CODEX").expect("Bazel must provide pinned Codex"))
        .canonicalize()
        .unwrap();
    let version = Command::new(&binary).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("codex-cli {}", env!("CODEX_CLI_VERSION")),
    );

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let home = root.join("home");
    let workspace = root.join("workspace");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let guard = root.join("guard.sh");
    fs::write(&guard, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&guard, fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = render_review_guide_config(&workspace, &root.join("guide.sock"));
    config.push_str(&format!(
        "\n[[hooks.PreToolUse]]\nmatcher = \".*\"\nhooks = [{{ type = \"command\", command = {} }}]\n",
        super::toml_basic_string(&guard.display().to_string()),
    ));
    let request = ArmRequest {
        codex_home: home.clone(),
        config_path: home.join("config.toml"),
        cwd: workspace,
        hooks: vec![
            CommandHookSpec::builder()
                .event(HookEvent::PreToolUse)
                .matcher(".*")
                .command(guard)
                .build(),
        ],
        codex_bin: binary,
    };

    // Reproduce the pre-fix failure through the unchanged production gate.
    // No CLI overrides: this is exactly how the pre-spawn observer loads it.
    fs::write(
        &request.config_path,
        config.replace("default_permissions = \"review-guide\"\n", ""),
    )
    .unwrap();
    let error = arm_and_attest(&request).expect_err("missing profile selection must refuse the worker");
    assert!(
        error.to_string().contains("hooks/list returned no hook entries"),
        "{error}"
    );

    fs::write(&request.config_path, config).unwrap();
    let attestation = arm_and_attest(&request).expect("rendered guide config must load and arm its guard");
    assert_eq!(attestation.hooks.len(), 1);
    assert_eq!(attestation.hooks[0].observed_trust_status, "trusted");
    assert!(attestation.hooks[0].guard_content_sha256.is_some());
}
