use super::probe_command;
use std::ffi::OsStr;

#[tokio::test]
async fn explicit_ssh_environment_is_not_overridden() {
    // No repository is needed: explicit environment settings must also skip
    // the core.sshCommand lookup entirely.
    let dir = tempfile::tempdir().unwrap();
    for key in ["GIT_SSH_COMMAND", "GIT_SSH"] {
        let command = probe_command(&["fetch"], dir.path(), &dir.path().join("missing"), |name| {
            (name == key).then(|| "custom-ssh -i key".into())
        })
        .await
        .unwrap();
        assert!(
            command.as_std().get_envs().all(|(name, _)| name != "GIT_SSH_COMMAND"),
            "{key} must be inherited without a higher-precedence override"
        );
    }
}

#[tokio::test]
async fn batch_mode_preserves_configured_key_and_options() {
    let dir = tempfile::tempdir().unwrap();
    boss_engine_test_git::init_repo_with_branch(dir.path(), "main");
    let configured = "ssh -i 'key with spaces' -o IdentitiesOnly=yes";
    let status = tokio::process::Command::new("git")
        .args(["config", "core.sshCommand", configured])
        .current_dir(dir.path())
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let command = probe_command(&["fetch"], dir.path(), &dir.path().join(".git"), |_| None)
        .await
        .unwrap();
    let ssh = command
        .as_std()
        .get_envs()
        .find(|(name, _)| *name == "GIT_SSH_COMMAND")
        .unwrap()
        .1;
    let expected = format!("{configured} -o BatchMode=yes");
    assert_eq!(ssh, Some(OsStr::new(&expected)));
}

#[tokio::test]
async fn batch_mode_defaults_to_ssh() {
    let dir = tempfile::tempdir().unwrap();
    boss_engine_test_git::init_repo_with_branch(dir.path(), "main");
    let command = probe_command(&["fetch"], dir.path(), &dir.path().join(".git"), |_| None)
        .await
        .unwrap();
    let ssh = command
        .as_std()
        .get_envs()
        .find(|(name, _)| *name == "GIT_SSH_COMMAND")
        .unwrap()
        .1;
    assert_eq!(ssh, Some(OsStr::new("ssh -o BatchMode=yes")));
}
