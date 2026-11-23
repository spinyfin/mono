//! A run with no cube checkout (a progression run) works in an engine-owned
//! scratch directory. These tests pin that the CLI is given that directory as
//! its only workspace directory, that commands run from it reach the guards,
//! that the run can execute the Boss verbs it is granted (the declaration
//! that ends it above all), and that the run's Boss-owned directories stay
//! protected.

use super::*;

use super::super::super::environment::AgyProcessEnvironment;
use super::super::super::home::{
    agent_rules_path, agent_rules_path_for_run, control_dir_for_run, log_file_for_run, read_workspace_kind,
    read_workspace_path_stamp, rules_add_dir, session_settings_path_for_run,
};
use super::super::super::{permissions, progression_probe};

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
            // The hook itself grants a progression run its `boss` verbs. It
            // does not pass them on to the session allowlist: the CLI cannot
            // match a rule whose program is `"$BOSS_BIN"` against a command
            // that carries an argument.
            assert_eq!(
                decision(&body),
                "auto_approve",
                "{command} from {}: {body}",
                cwd.display()
            );
        }
    }
}

/// The CLI's half of a `run_command` call in a deny-by-default session: what
/// it does with the hook's verdict and the session's `permissions.allow`.
///
/// This is a model, written from two measurements, and it is here so a test
/// can follow a command past the hook. `auto_approve` runs the command and
/// `deny` refuses it (permission study, section 7). Under
/// `deny_unless_prior_grant` the command runs only when a `command(...)` allow
/// rule covers it, and how a rule covers a command was measured on 1.5.14 in
/// the progression runs [`the_cli_model_reproduces_the_measured_run`] replays:
/// a rule of plain words is matched word by word as a prefix, and a rule whose
/// program is a quoted shell variable matches only the exact command text.
fn cli_runs(verdict: &str, command: &str, allow: &[String]) -> bool {
    match verdict {
        "auto_approve" => true,
        "deny_unless_prior_grant" => allow
            .iter()
            .filter_map(|rule| rule.strip_prefix("command(")?.strip_suffix(')'))
            .any(|target| {
                let plain_words = target.chars().all(|c| c.is_ascii_alphanumeric() || " -_./".contains(c));
                if plain_words {
                    let mut typed = command.split_whitespace();
                    target.split_whitespace().all(|word| typed.next() == Some(word))
                } else {
                    command == target
                }
            }),
        _ => false,
    }
}

/// The verdict the hook gave every command of those runs, before command
/// grants moved into the adapter.
const PASSED_TO_THE_ALLOWLIST: &str = "deny_unless_prior_grant";

/// The session allowlist those runs were started with: the progression table
/// rendered as `command(...)` rules.
fn allowlist_of_the_measured_run() -> Vec<String> {
    crate::capability_rules::progression_allow_rules()
        .iter()
        .filter_map(|rule| rule.strip_prefix("Bash(")?.strip_suffix(":*)"))
        .map(|prefix| format!("command({prefix})"))
        .collect()
}

/// The declaration a measured run tried to end with. Its summary is quoted
/// text with spaces, as every real one is.
const DECLARATION: &str = r#""$BOSS_BIN" propose done --outcome no-changes-needed --summary 'PR 3338 still has conflicts and revision task_2 is waiting_dependency on PR 3809.'"#;

/// The arguments the Boss CLI receives when [`DECLARATION`] runs.
const DECLARATION_ARGV: [&str; 6] = [
    "propose",
    "done",
    "--outcome",
    "no-changes-needed",
    "--summary",
    "PR 3338 still has conflicts and revision task_2 is waiting_dependency on PR 3809.",
];

/// A `gh` inspection the progression table names.
const GH_VIEW: &str =
    "gh pr view 3338 --repo spinyfin/mono --json state,mergeable,mergeStateStatus,headRefOid,baseRefOid,title,url";

/// A `gh` verb the table does not name.
const GH_LIST: &str =
    "gh pr list --repo spinyfin/mono --state all --limit 30 --json number,title,state,url,mergedAt,headRefName";

/// Commands progression agents typed in two measured runs, with whether the
/// CLI ran them. Item ids are shortened; nothing else is changed.
const MEASURED_RUN: [(&str, bool); 12] = [
    (r#""$BOSS_BIN" task show task_1"#, false),
    (r#""$BOSS_BIN" pr status --json"#, false),
    (r#""$BOSS_BIN" propose --help"#, false),
    // The Boss verbs that ran: each command is exactly a rule's text.
    (r#""$BOSS_BIN" task list"#, true),
    (r#""$BOSS_BIN" task show"#, true),
    (r#""$BOSS_BIN" pr status"#, true),
    (r#""$BOSS_BIN" propose"#, true),
    (r#""$BOSS_BIN" task list --product boss"#, false),
    (DECLARATION, false),
    (GH_VIEW, true),
    ("gh pr checks 3338 --repo spinyfin/mono", true),
    // No rule names it.
    (GH_LIST, false),
];

/// How a progression agent is taught to name the Boss CLI.
const BOSS: &str = "\"$BOSS_BIN\" ";

#[test]
fn the_cli_model_reproduces_the_measured_run() {
    let allow = allowlist_of_the_measured_run();
    assert!(
        allow.contains(&r#"command("$BOSS_BIN" propose)"#.to_owned()),
        "{allow:?}"
    );
    for (command, ran) in MEASURED_RUN {
        assert_eq!(cli_runs(PASSED_TO_THE_ALLOWLIST, command, &allow), ran, "{command}");
    }
    // The declaration that ends a run always carries arguments, so under that
    // posture no progression run could record how it ended.
    assert!(!cli_runs(
        PASSED_TO_THE_ALLOWLIST,
        r#""$BOSS_BIN" propose done --outcome delivered --summary ok"#,
        &allow
    ));
}

/// A provisioned progression run whose `BOSS_BIN` names a stand-in for the
/// Boss CLI that records its arguments.
struct ProgressionRun {
    fx: Fixture,
    boss: PathBuf,
    calls: PathBuf,
}

impl ProgressionRun {
    fn start(run_id: &str) -> Self {
        let fx = Fixture::with_workspace_kind(run_id, ExecutionWorkspaceKind::Scratch);
        fx.install_with(WorkerKind::Progression, |c| c.is_standard_worker = false);
        let tools = fx.tmp.path().join("tools");
        fs::create_dir_all(&tools).unwrap();
        let (boss, calls) = (tools.join("boss"), fx.tmp.path().join("boss-calls.txt"));
        write_exec(&boss, &progression_probe::boss_stand_in(&calls));
        Self { fx, boss, calls }
    }

    /// The installed hook's verdict for a command typed in the scratch
    /// directory.
    fn hook_verdict(&self, command: &str) -> String {
        let mut payload = run_command_payload(command);
        payload["toolCall"]["args"]["Cwd"] = json!(self.fx.workspace());
        let boss = self.boss.to_str().unwrap();
        let (_, body, stderr) = authorize_with_env(&self.fx.process_home, &payload, &[("BOSS_BIN", boss)]);
        assert!(!decision(&body).is_empty(), "{command}: {body} {stderr}");
        decision(&body).to_owned()
    }

    /// The session allowlist the run is launched with.
    fn session_allowlist(&self) -> Vec<String> {
        let settings = fs::read_to_string(session_settings_path_for_run(&self.fx.run_id).unwrap()).unwrap();
        permissions::parse_rendered(&settings).unwrap().allow_rules().to_vec()
    }

    /// Whether the command gets past the hook and the CLI.
    fn runs(&self, command: &str) -> bool {
        cli_runs(&self.hook_verdict(command), command, &self.session_allowlist())
    }

    /// Follow one Boss verb the whole way: the hook's verdict, what the CLI
    /// does with it, and, when the CLI runs it, the tool shell and the
    /// stand-in. Returns whether it ran.
    fn type_boss_verb(&self, command: &str) -> bool {
        assert!(command.starts_with(BOSS), "{command}");
        let runs = self.runs(command);
        if runs {
            let status = Command::new("/bin/sh")
                .args(["-c", command])
                .env("BOSS_BIN", &self.boss)
                .current_dir(self.fx.workspace())
                .stdout(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "{command}");
        }
        runs
    }

    /// The argument vectors the stand-in has received, in order.
    fn recorded(&self) -> Vec<Vec<String>> {
        let text = fs::read_to_string(&self.calls).unwrap_or_default();
        progression_probe::recorded_calls(&text)
            .into_iter()
            .map(|call| call.into_iter().map(str::to_owned).collect())
            .collect()
    }
}

#[test]
fn a_progression_run_records_its_disposition() {
    let run = ProgressionRun::start("run-scratch-disposition");
    // Nothing a progression run needs depends on the CLI's command matcher.
    let allow = run.session_allowlist();
    assert!(allow.iter().all(|rule| rule.starts_with("read_file(")), "{allow:?}");

    // The run a progression agent attempted: inspect the item, inspect its
    // PR, declare. Each verb the CLI refused now reaches the Boss CLI.
    let inspect_item = r#""$BOSS_BIN" task show task_1"#;
    let inspect_pr = r#""$BOSS_BIN" pr status --json"#;
    for command in [inspect_item, inspect_pr, DECLARATION] {
        assert!(MEASURED_RUN.contains(&(command, false)), "{command}");
        assert!(run.type_boss_verb(command), "{command}");
    }
    let recorded = run.recorded();
    assert_eq!(recorded.len(), 3, "{recorded:#?}");
    assert_eq!(recorded[0], ["task", "show", "task_1"], "{recorded:#?}");
    assert_eq!(recorded[1], ["pr", "status", "--json"], "{recorded:#?}");
    assert_eq!(
        recorded[2], DECLARATION_ARGV,
        "the declaration reaches the CLI with its summary intact"
    );

    // The `gh` inspection the table names still runs, and what it does not
    // name still does not.
    assert!(run.runs(GH_VIEW));
    assert!(!run.runs(GH_LIST));
}

#[test]
fn the_live_probes_cases_hold_at_the_hook() {
    // The authenticated probe (`agy_progression_probe`) measures the same
    // commands against the real CLI. This is the half of it that can run
    // here: the hook's verdict, and the stand-in receiving the arguments.
    let run = ProgressionRun::start("run-scratch-probe-cases");
    for case in &progression_probe::CASES {
        let ran = run.type_boss_verb(case.command);
        assert_eq!(
            ran,
            case.expect == progression_probe::Expectation::Runs,
            "{}: {}",
            case.what,
            case.command
        );
        let recorded = run.recorded().iter().any(|call| call == case.argv);
        assert_eq!(recorded, ran, "{}", case.command);
    }
}

#[test]
fn a_grant_never_overrides_a_refusal_or_reaches_past_one_command() {
    let run = ProgressionRun::start("run-scratch-grant-bounds");
    // Refused by a structural check before any grant is consulted.
    for command in [
        r#""$BOSS_BIN" pr status > out.txt"#,
        r#""$BOSS_BIN" propose done --summary "$(cat /etc/passwd)""#,
        r#"sudo "$BOSS_BIN" propose unblock"#,
        r#"BOSS_BIN=/tmp/evil "$BOSS_BIN" propose unblock"#,
    ] {
        assert_eq!(run.hook_verdict(command), "deny", "{command}");
    }
    // A granted verb inside something larger, a verb the table does not name,
    // and a spelling that is not the program `BOSS_BIN` holds: none of these
    // is the granted command, whichever layer says so. The matcher's own
    // matrix (`command_grant_tests`) covers the rest of these shapes.
    let allow = run.session_allowlist();
    for command in [
        r#""$BOSS_BIN" propose unblock; gh pr merge 1"#,
        r#"'$BOSS_BIN' propose unblock"#,
        r#""$BOSS_BIN" comment reply thread"#,
    ] {
        let verdict = run.hook_verdict(command);
        assert_ne!(verdict, "auto_approve", "{command}");
        assert!(!cli_runs(&verdict, command, &allow), "{command}");
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
