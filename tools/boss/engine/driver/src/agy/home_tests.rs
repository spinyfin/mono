use super::*;
use crate::AgentDriver;
use crate::test_support::home_override;
use tempfile::TempDir;

#[test]
fn provisioning_refuses_aliases_before_writing_host_files() {
    for alias in [
        "process-home",
        "process-home/.gemini",
        "process-home/.gemini/antigravity-cli",
        "control",
        "rules-root",
    ] {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let target = tmp.path().join("host-state");
        fs::create_dir_all(target.join("cache")).unwrap();
        fs::write(target.join("settings.json"), b"host settings").unwrap();
        fs::write(target.join("cache/onboarding.json"), b"host onboarding").unwrap();
        let _guard = env_for_provision(&homes);
        let link = homes.join("run-alias").join(alias);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error = provision_agy_home(&workspace, "prompt", "run-alias").unwrap_err();
        assert!(format!("{error:#}").contains("symlink"), "{error:#}");
        assert_eq!(fs::read(target.join("settings.json")).unwrap(), b"host settings");
        assert_eq!(
            fs::read(target.join("cache/onboarding.json")).unwrap(),
            b"host onboarding"
        );
        assert!(!target.join("config/hooks.json").exists());
    }
}

#[test]
fn spawn_posture_rejects_intermediate_alias_after_provision() {
    let tmp = TempDir::new().unwrap();
    let _guard = env_for_provision(tmp.path());
    let runtime = provision_agy_home(tmp.path(), "prompt", "run-swap").unwrap();
    let private = runtime.process_home.join(".gemini/antigravity-cli");
    let target = tmp.path().join("shared");
    fs::rename(&private, &target).unwrap();
    std::os::unix::fs::symlink(&target, &private).unwrap();
    assert!(assert_agy_posture(&runtime.process_home, tmp.path()).is_err());
}

use super::super::test_environment::{env_for_provision, env_for_provision_with_home_override};

#[test]
fn process_home_for_run_is_under_homes_root() {
    let tmp = TempDir::new().unwrap();
    let _guard = env_for_provision(tmp.path());
    let home = process_home_for_run("run-abc").unwrap();
    assert!(home.starts_with(tmp.path()));
    assert!(home.ends_with("process-home"));
}

#[test]
fn onboarding_json_skips_the_screen_without_answering_consent() {
    let parsed: serde_json::Value = serde_json::from_str(render_onboarding_json()).unwrap();
    assert_eq!(parsed["onboardingComplete"], true);
    assert_eq!(parsed["consumerOnboardingComplete"], true);
    assert!(parsed.get("enableTelemetry").is_none());
}

#[test]
fn provision_creates_private_tree_and_keychain_bridge() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let workspace = tmp.path().join("ws");
    fs::create_dir_all(&workspace).unwrap();
    let real_home = tmp.path().join("real-home");
    let keychain_dir = real_home.join("Library/Keychains");
    fs::create_dir_all(&keychain_dir).unwrap();
    let keychain_file = keychain_dir.join("login.keychain-db");
    fs::write(&keychain_file, b"dummy-keychain").unwrap();
    fs::create_dir_all(real_home.join(".gemini/antigravity-cli")).unwrap();
    fs::write(
        real_home.join(".gemini/antigravity-cli/settings.json"),
        "{\"enableTelemetry\": true}\n",
    )
    .unwrap();
    let _home = home_override(&real_home);
    let _guard = env_for_provision_with_home_override(&homes);

    let runtime = provision_agy_home(&workspace, "hello prompt", "run-prov-1").unwrap();
    assert!(runtime.process_home.starts_with(&homes));
    assert_ne!(runtime.process_home, real_home);

    let gemini = runtime.process_home.join(".gemini");
    assert!(gemini.join("antigravity-cli/cache/onboarding.json").is_file());
    assert!(gemini.join("antigravity-cli/settings.json").is_file());
    assert!(gemini.join("config/hooks.json").is_file());
    let rules_dir = homes.join("run-prov-1/rules-root/.agents/rules");
    assert!(rules_dir.is_dir());
    assert_eq!(
        agent_rules_path_for_run("run-prov-1").unwrap().parent(),
        Some(rules_dir.as_path())
    );
    assert_eq!(
        super::super::AgyDriver::default()
            .agent_rules_destination(&workspace, "run-prov-1")
            .parent(),
        Some(rules_dir.as_path())
    );
    assert!(!gemini.join("AGENTS.md").exists());
    assert!(!gemini.join("settings.json").exists());
    assert!(!gemini.join("trustedFolders.json").exists());
    assert!(runtime.control_dir.join("initial-prompt.txt").is_file());
    assert_eq!(
        fs::read_to_string(runtime.control_dir.join("initial-prompt.txt")).unwrap(),
        "hello prompt"
    );
    assert_eq!(
        read_workspace_path_stamp(&runtime.control_dir).unwrap(),
        fs::canonicalize(&workspace).unwrap()
    );

    let bridged = runtime.process_home.join(LOGIN_KEYCHAIN_LEAF);
    let meta = fs::symlink_metadata(&bridged).expect("login keychain bridge");
    assert!(meta.file_type().is_symlink());
    assert_eq!(fs::read_link(&bridged).unwrap(), keychain_file);

    // Host ~/.gemini must not be the run's .gemini.
    let host_gemini = fs::canonicalize(real_home.join(".gemini")).unwrap();
    let run_gemini = fs::canonicalize(&gemini).unwrap();
    assert_ne!(host_gemini, run_gemini);

    let settings = fs::read_to_string(gemini.join("antigravity-cli/settings.json")).unwrap();
    assert!(!settings.contains("enableTelemetry"));
    assert!(settings.contains("trustedWorkspaces"));

    assert_agy_posture(&runtime.process_home, &workspace).unwrap();
}

#[test]
fn posture_fails_when_private_state_is_symlinked_out() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let workspace = tmp.path().join("ws");
    fs::create_dir_all(&workspace).unwrap();
    let real_home = tmp.path().join("real-home");
    fs::create_dir_all(&real_home).unwrap();
    let _home = home_override(&real_home);
    let _guard = env_for_provision_with_home_override(&homes);

    let runtime = provision_agy_home(&workspace, "p", "run-leak-1").unwrap();
    let dest = runtime
        .process_home
        .join(".gemini/antigravity-cli/conversation_summaries.db");
    let outside = tmp.path().join("shared-db");
    fs::write(&outside, b"shared").unwrap();
    let _ = fs::remove_file(&dest);
    std::os::unix::fs::symlink(&outside, &dest).unwrap();

    let err = assert_agy_posture(&runtime.process_home, &workspace)
        .unwrap_err()
        .to_string();
    assert!(err.contains("symlink"), "{err}");
    assert!(err.contains("conversation_summaries.db"), "{err}");
}

#[test]
fn two_runs_do_not_share_private_state_paths() {
    let tmp = TempDir::new().unwrap();
    let homes = tmp.path().join("homes");
    let a = process_home_for_run_with(&homes, "run-a");
    let b = process_home_for_run_with(&homes, "run-b");
    assert_ne!(a, b);
    assert_ne!(
        a.join(".gemini/antigravity-cli/settings.json"),
        b.join(".gemini/antigravity-cli/settings.json")
    );
}

fn process_home_for_run_with(homes: &Path, run_id: &str) -> PathBuf {
    let _guard = env_for_provision(homes);
    process_home_for_run(run_id).unwrap()
}

#[test]
fn safe_to_delete_refuses_host_home() {
    let tmp = TempDir::new().unwrap();
    let _guard = env_for_provision(&tmp.path().join("homes"));
    let err = assert_agy_home_safe_to_delete(Path::new("/Users/someone/.gemini"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("outside"), "{err}");
}
