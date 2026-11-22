//! A run with no cube checkout (a progression run) works in an engine-owned
//! scratch directory. These tests pin that the CLI is given that directory as
//! its only workspace directory, that commands run from it reach the guards,
//! and that the run's Boss-owned directories stay protected.

use super::*;

use super::super::super::environment::AgyProcessEnvironment;
use super::super::super::home::{
    agent_rules_path, agent_rules_path_for_run, control_dir_for_run, log_file_for_run, read_workspace_kind,
    read_workspace_path_stamp, rules_add_dir, session_settings_path_for_run,
};

const PROPOSE: &str = "\"$BOSS_BIN\" propose unblock";

fn decide_at(fx: &Fixture, command: &str, cwd: &Path) -> Value {
    let mut payload = run_command_payload(command);
    payload["toolCall"]["args"]["Cwd"] = json!(cwd);
    let (_, body, stderr) = authorize(&fx.process_home, &payload);
    assert!(!decision(&body).is_empty(), "{command}: {body} {stderr}");
    body
}

fn launch_args(fx: &Fixture) -> Vec<String> {
    AgyProcessEnvironment::resolve(
        &fx.process_home,
        &log_file_for_run(&fx.run_id).unwrap(),
        &session_settings_path_for_run(&fx.run_id).unwrap(),
        PathBuf::from("/run/bin/agy"),
    )
    .unwrap()
    .launch_args()
    .into_iter()
    .map(|arg| arg.to_string_lossy().into_owned())
    .collect()
}

#[test]
fn a_scratch_run_has_the_scratch_directory_as_its_only_workspace_directory() {
    let fx = Fixture::with_workspace_kind("run-scratch-launch", ExecutionWorkspaceKind::Scratch);
    let control = control_dir_for_run(&fx.run_id).unwrap();
    assert_eq!(read_workspace_kind(&control), ExecutionWorkspaceKind::Scratch);

    // The launch directory is the scratch directory dispatch supplied.
    let launch_dir = read_workspace_path_stamp(&control).unwrap();
    assert_eq!(launch_dir, fs::canonicalize(fx.workspace()).unwrap());

    // Nothing else is added to the CLI workspace: `--add-dir` would make the
    // rules root a second workspace directory the agent can choose to work in.
    assert_eq!(rules_add_dir(&fx.process_home), None);
    let args = launch_args(&fx);
    assert!(!args.iter().any(|arg| arg == "--add-dir"), "{args:?}");
    assert!(!args.iter().any(|arg| arg.contains("rules-root")), "{args:?}");

    // The rules are delivered at the root of that one workspace directory.
    assert_eq!(
        agent_rules_path(&fx.workspace(), &fx.run_id).unwrap(),
        fx.workspace().join("AGENTS.md")
    );
}

#[test]
fn a_cube_run_keeps_its_rules_out_of_the_checkout() {
    let fx = Fixture::new("run-cube-launch");
    let control = control_dir_for_run(&fx.run_id).unwrap();
    assert_eq!(read_workspace_kind(&control), ExecutionWorkspaceKind::Cube);
    let rules_root = control.parent().unwrap().join("rules-root");
    assert_eq!(rules_add_dir(&fx.process_home), Some(rules_root.clone()));
    let args = launch_args(&fx);
    let add_dir = args.iter().position(|arg| arg == "--add-dir").expect("--add-dir");
    assert_eq!(args[add_dir + 1], rules_root.display().to_string());
    assert_eq!(
        agent_rules_path(&fx.workspace(), &fx.run_id).unwrap(),
        rules_root.join("AGENTS.md")
    );
}

#[test]
fn an_unstamped_run_is_treated_as_a_cube_checkout() {
    let fx = Fixture::new("run-unstamped");
    let control = control_dir_for_run(&fx.run_id).unwrap();
    fs::remove_file(control.join("boss-workspace-kind")).unwrap();
    assert_eq!(read_workspace_kind(&control), ExecutionWorkspaceKind::Cube);
    fs::write(control.join("boss-workspace-kind"), "something else\n").unwrap();
    assert_eq!(read_workspace_kind(&control), ExecutionWorkspaceKind::Cube);
    assert_eq!(
        agent_rules_path(&fx.workspace(), &fx.run_id).unwrap(),
        agent_rules_path_for_run(&fx.run_id).unwrap()
    );
}

#[test]
fn a_progression_run_can_propose_from_its_scratch_directory() {
    let fx = Fixture::with_workspace_kind("run-scratch-propose", ExecutionWorkspaceKind::Scratch);
    fx.install(WorkerKind::Progression);
    let control = control_dir_for_run(&fx.run_id).unwrap();
    let launch_dir = read_workspace_path_stamp(&control).unwrap();

    // Both spellings of the launch directory: the one stamped at provisioning
    // and the one dispatch passed.
    for cwd in [launch_dir.as_path(), fx.workspace().as_path()] {
        for command in [PROPOSE, "\"$BOSS_BIN\" task show", "\"$BOSS_BIN\" pr status --json"] {
            let body = decide_at(&fx, command, cwd);
            // The hook passes the command on to the session allowlist, which
            // is what grants a progression run its `boss` verbs.
            assert_eq!(
                decision(&body),
                "deny_unless_prior_grant",
                "{command} from {}: {body}",
                cwd.display()
            );
        }
    }
}

#[test]
fn a_progression_run_is_still_kept_out_of_antigravity_and_boss_state() {
    let fx = Fixture::with_workspace_kind("run-scratch-protected", ExecutionWorkspaceKind::Scratch);
    fx.install(WorkerKind::Progression);
    let container = fx.process_home.parent().unwrap().to_path_buf();
    let rules_root = container.join("rules-root");
    let boss_data = fx.tmp.path().join("boss-data");
    fs::create_dir_all(&boss_data).unwrap();

    // Working from a protected directory is refused whatever the command.
    for cwd in [
        rules_root.clone(),
        container.join("control"),
        fx.process_home.join(".gemini/config"),
        boss_data.clone(),
    ] {
        let body = decide_at(&fx, PROPOSE, &cwd);
        assert_eq!(decision(&body), "deny", "cwd {}: {body}", cwd.display());
        assert_eq!(
            body["reason"].as_str().unwrap_or_default(),
            "working directory accesses protected Antigravity or Boss state",
            "cwd {}: {body}",
            cwd.display()
        );
    }

    // So is reaching into one from the scratch directory.
    for target in [
        rules_root.join("AGENTS.md"),
        container.join("control/initial-prompt.txt"),
        fx.process_home.join(".gemini/config/hooks.json"),
        boss_data.join("state.db"),
    ] {
        for command in [
            format!("cat {}", target.display()),
            format!("printf x > {}", target.display()),
            format!("cd {} && ls", target.parent().unwrap().display()),
        ] {
            let body = decide_at(&fx, &command, &fx.workspace());
            assert_eq!(decision(&body), "deny", "{command}: {body}");
        }
        let payload = json!({
            "hookEventName": "PreToolUse",
            "conversationId": "c1",
            "toolCall": {"id": "t1", "name": "read_file", "args": {"Path": target, "Cwd": fx.workspace()}},
        });
        let (_, body, stderr) = authorize(&fx.process_home, &payload);
        assert_eq!(
            decision(&body),
            "deny",
            "read_file {}: {body} {stderr}",
            target.display()
        );
    }
}
