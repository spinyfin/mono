//! Authenticated host probe: a progression run on the real CLI must be able
//! to run the Boss verbs it is granted, and nothing else. Invoked through the
//! non-test `agy_progression_probe` binary. Evidence stays in a
//! caller-selected directory, including on failure.
//!
//! A hermetic test can pin the verdict the hook adapter returns for a
//! command. It cannot pin what the CLI then does with that verdict, and that
//! half is where a progression run was lost: every `"$BOSS_BIN" propose` was
//! refused by the CLI's own permission check, so no run could declare how it
//! ended. This probe measures both halves together on a signed-in host. It
//! provisions the production scratch layout, session settings and PreToolUse
//! authorization for [`WorkerKind::Progression`], points `BOSS_BIN` at a
//! stand-in that records its arguments, and asks the model to run two granted
//! verbs and one ungranted verb, spelled the way a progression agent is taught
//! to spell them.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, ensure};
use boss_ssh_transport::shell_quote;
use serde_json::Value;

use super::environment::{AgyProcessEnvironment, run_agy_bounded};
use super::home::{agent_rules_path, provision_agy_home_with_kind};
use super::hooks::{install_authorization, observation_from_permission, verify_startup_authorization};
use super::permissions;
use crate::hook_guards::write_executable;
use crate::{ExecutionWorkspaceKind, PermissionInput, ToolUseInterceptionConfig, WorkerKind};

const RUN_ID: &str = "progression-probe";

/// What the probe expects of one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Expectation {
    Runs,
    Refused,
}

/// What happened to one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The stand-in recorded the call.
    Ran,
    /// The CLI raised `PreToolUse` for the command and the stand-in never saw it.
    Refused,
    /// The model never issued the command, so nothing was measured.
    NotAttempted,
}

pub(super) struct Case {
    pub(super) what: &'static str,
    /// The command line, exactly as the model is asked to type it.
    pub(super) command: &'static str,
    /// The arguments the stand-in receives when the command runs.
    pub(super) argv: &'static [&'static str],
    pub(super) expect: Expectation,
}

pub(super) const CASES: [Case; 3] = [
    Case {
        what: "granted inspection verb with an argument",
        command: r#""$BOSS_BIN" task show probe-item"#,
        argv: &["task", "show", "probe-item"],
        expect: Expectation::Runs,
    },
    Case {
        what: "the declaration that ends a progression run",
        command: r#""$BOSS_BIN" propose done --outcome no-changes-needed --summary 'probe disposition recorded'"#,
        argv: &[
            "propose",
            "done",
            "--outcome",
            "no-changes-needed",
            "--summary",
            "probe disposition recorded",
        ],
        expect: Expectation::Runs,
    },
    Case {
        what: "Boss verb outside the progression allowlist",
        command: r#""$BOSS_BIN" comment reply probe-thread"#,
        argv: &["comment", "reply", "probe-thread"],
        expect: Expectation::Refused,
    },
];

/// Separates the arguments of one recorded call. A unit separator cannot
/// appear in any command the probe asks for.
const ARG_SEPARATOR: char = '\u{1f}';

/// Stand-in for the Boss CLI: one line per call, arguments joined by
/// [`ARG_SEPARATOR`]. Plain `sh`, so it does not depend on the tool shell's
/// `PATH`.
pub(super) fn boss_stand_in(calls: &Path) -> String {
    format!(
        "#!/bin/sh\nIFS=$(printf '\\037')\nprintf '%s\\n' \"$*\" >> {}\necho 'probe stand-in recorded the call'\n",
        shell_quote(&calls.display().to_string())
    )
}

/// Stand-in for `boss-event`: keep every hook payload as evidence.
fn forwarder_stand_in(events: &Path) -> String {
    format!(
        "#!/bin/sh\n{{ /bin/cat; printf '\\n'; }} >> {}\n",
        shell_quote(&events.display().to_string())
    )
}

/// Variables a Boss worker session exports for its own run.
fn is_worker_session_variable(name: &str) -> bool {
    name.starts_with("BOSS_") || name == "CUBE_BIN"
}

fn prompt() -> String {
    let mut prompt = String::from(
        "Use your shell command tool to run each of the following commands exactly as written, \
         one at a time, in the foreground. Copy every character, including the quotes and the \
         dollar sign: do not expand, rewrite, combine or wrap them. If a command is refused, do \
         not retry it and do not try another spelling; go on to the next one.\n\n",
    );
    for (index, case) in CASES.iter().enumerate() {
        prompt.push_str(&format!("{}. {}\n", index + 1, case.command));
    }
    prompt.push_str("\nThen reply with exactly PROBE_DONE.");
    prompt
}

/// Commands the CLI raised a `PreToolUse` hook for, from the recorded payloads
/// (a stream of JSON values, one per hook event).
fn attempted_commands(events: &str) -> anyhow::Result<Vec<String>> {
    let mut commands = Vec::new();
    for value in serde_json::Deserializer::from_str(events).into_iter::<Value>() {
        let value = value.context("recorded hook payload is not JSON")?;
        if value.get("hookEventName").and_then(Value::as_str) != Some("PreToolUse") {
            continue;
        }
        if let Some(command) = value.pointer("/toolCall/args/CommandLine").and_then(Value::as_str) {
            commands.push(command.to_owned());
        }
    }
    Ok(commands)
}

/// The argument vectors the stand-in recorded, one per call.
pub(super) fn recorded_calls(calls: &str) -> Vec<Vec<&str>> {
    calls.lines().map(|line| line.split(ARG_SEPARATOR).collect()).collect()
}

fn outcome(case: &Case, attempted: &[String], recorded: &[Vec<&str>]) -> Outcome {
    if recorded.iter().any(|call| call == case.argv) {
        Outcome::Ran
    } else if attempted.iter().any(|command| command == case.command) {
        Outcome::Refused
    } else {
        Outcome::NotAttempted
    }
}

/// One line per case, and whether every case met its expectation.
fn verdict(attempted: &[String], recorded: &[Vec<&str>]) -> (String, bool) {
    let mut report = String::new();
    let mut pass = true;
    for case in &CASES {
        let outcome = outcome(case, attempted, recorded);
        let met = matches!(
            (case.expect, outcome),
            (Expectation::Runs, Outcome::Ran) | (Expectation::Refused, Outcome::Refused)
        );
        pass &= met;
        report.push_str(&format!(
            "{}: {}: expected {:?}, observed {outcome:?}: {}\n",
            if met { "ok" } else { "FAIL" },
            case.what,
            case.expect,
            case.command,
        ));
    }
    (report, pass)
}

/// Provision a progression run exactly as dispatch does and measure which of
/// its commands the CLI lets through. The caller must select a fresh homes
/// root before starting any threads.
pub fn run(evidence: &Path) -> anyhow::Result<()> {
    let workspace = evidence.join("scratch");
    fs::create_dir(&workspace)?;
    let runtime = provision_agy_home_with_kind(&workspace, "unused", RUN_ID, ExecutionWorkspaceKind::Scratch)?;
    fs::write(
        agent_rules_path(&workspace, RUN_ID)?,
        "Run only the commands you are asked to run, exactly as they are written.\n",
    )?;

    let tools = evidence.join("tools");
    fs::create_dir(&tools)?;
    let calls = evidence.join("boss-calls.txt");
    let events = evidence.join("hook-events.jsonl");
    for file in [&calls, &events] {
        fs::write(file, "")?;
    }
    let boss = tools.join("boss");
    write_executable(&boss, &boss_stand_in(&calls))?;
    let forwarder = tools.join("boss-event");
    write_executable(&forwarder, &forwarder_stand_in(&events))?;
    let path_guard = tools.join("boss-path-guard.py");
    write_executable(&path_guard, crate::path_guard::PATH_GUARD_SCRIPT)?;

    // The same three steps `write_permission_config` takes for a real run.
    let input = PermissionInput::builder()
        .worker_kind(WorkerKind::Progression)
        .workspace_path(workspace.clone())
        .events_socket_path(evidence.join("boss-data").join("events.sock"))
        .boss_event_path(forwarder)
        .run_id(RUN_ID)
        .lease_id(RUN_ID)
        .execution_kind("progression")
        .is_remote(false)
        .path_guard_script(path_guard)
        .build();
    let artifacts = permissions::write_session_settings(&input)?;
    install_authorization(
        &runtime.process_home,
        input.worker_kind,
        &observation_from_permission(&input),
        &ToolUseInterceptionConfig::from_permission_input(&input),
    )?;
    verify_startup_authorization(&input).context("startup authorization for the probe run")?;

    let environment = AgyProcessEnvironment::resolve(
        &runtime.process_home,
        &runtime.log_file,
        &runtime.session_settings_path,
        runtime.agy_binary.clone(),
    )?;
    let mut version = Command::new(&runtime.agy_binary);
    environment.apply_to_command(&mut version);
    let version = run_agy_bounded(version.arg("--version"), Duration::from_secs(10))?;
    ensure!(version.status.success(), "agy --version failed");
    fs::write(evidence.join("version.txt"), &version.stdout)?;

    let mut command = Command::new(&runtime.agy_binary);
    // A shell that is itself a Boss worker exports its own run's variables.
    // None of them may reach the probe session: the hook commands carry the
    // probe's identity inline, and `BOSS_BIN` must be the stand-in.
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(is_worker_session_variable) {
            command.env_remove(name);
        }
    }
    environment.apply_to_command(&mut command);
    command
        .env("BOSS_BIN", &boss)
        .args(environment.launch_args())
        .args(&artifacts.extra_args)
        .args(["-p", &prompt()])
        .current_dir(&workspace);
    fs::write(evidence.join("command.txt"), format!("{command:?}\n"))?;
    let output =
        run_agy_bounded(&mut command, Duration::from_secs(300)).context("live progression run; evidence retained")?;
    fs::write(evidence.join("stdout.txt"), &output.stdout)?;
    fs::write(evidence.join("stderr.txt"), &output.stderr)?;

    let log_check = permissions::verify_settings_log_files(&runtime.session_settings_path, &runtime.log_file);
    fs::write(evidence.join("settings-log-check.txt"), format!("{log_check:#?}\n"))?;
    ensure!(
        log_check? == permissions::SettingsLogCheck::ConsistentSoFar,
        "the CLI did not initialise the rendered settings and hooks"
    );

    let events = fs::read_to_string(&events)?;
    let calls = fs::read_to_string(&calls)?;
    let (report, pass) = verdict(&attempted_commands(&events)?, &recorded_calls(&calls));
    let version = String::from_utf8_lossy(&version.stdout);
    let result = format!(
        "{}agy {}\n{report}",
        if pass { "PASS\n" } else { "FAIL\n" },
        version.trim()
    );
    fs::write(evidence.join("result.txt"), &result)?;
    print!("{result}");
    println!("evidence: {}", evidence.display());
    ensure!(
        pass,
        "a progression run could not run exactly its granted verbs; see result.txt and the transcript under {}",
        evidence.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pre_tool_use(command: &str) -> String {
        serde_json::json!({
            "hookEventName": "PreToolUse",
            "toolCall": {"id": "t", "name": "run_command", "args": {"CommandLine": command}},
        })
        .to_string()
    }

    fn recorded(case: &Case) -> String {
        case.argv.join(&ARG_SEPARATOR.to_string())
    }

    fn judge(events: &str, calls: &str) -> (String, bool) {
        verdict(&attempted_commands(events).unwrap(), &recorded_calls(calls))
    }

    fn all_attempted() -> String {
        // Other lifecycle events share the file and are ignored.
        let mut events = String::from("{\"hookEventName\":\"SessionStart\"}\n");
        for case in &CASES {
            events.push_str(&pre_tool_use(case.command));
            events.push('\n');
        }
        events
    }

    #[test]
    fn passes_only_when_the_granted_verbs_ran_and_the_ungranted_one_was_refused() {
        let calls = format!("{}\n{}\n", recorded(&CASES[0]), recorded(&CASES[1]));
        let (report, pass) = judge(&all_attempted(), &calls);
        assert!(pass, "{report}");
    }

    #[test]
    fn a_refused_declaration_fails_the_probe() {
        // The measured failure: the hook saw every command and none ran.
        let (report, pass) = judge(&all_attempted(), "");
        assert!(!pass, "{report}");
        assert!(report.contains("expected Runs, observed Refused"), "{report}");
    }

    #[test]
    fn an_ungranted_verb_that_ran_fails_the_probe() {
        let calls = CASES.iter().map(recorded).collect::<Vec<_>>().join("\n");
        let (report, pass) = judge(&all_attempted(), &calls);
        assert!(!pass, "{report}");
        assert!(report.contains("expected Refused, observed Ran"), "{report}");
    }

    #[test]
    fn a_command_the_model_never_issued_is_not_counted_as_refused() {
        let calls = format!("{}\n{}\n", recorded(&CASES[0]), recorded(&CASES[1]));
        let events = format!(
            "{}\n{}\n",
            pre_tool_use(CASES[0].command),
            pre_tool_use(CASES[1].command)
        );
        let (report, pass) = judge(&events, &calls);
        assert!(!pass, "{report}");
        assert!(report.contains("observed NotAttempted"), "{report}");
    }

    #[test]
    fn the_stand_in_records_arguments_the_way_the_shell_passes_them() {
        let tmp = tempfile::tempdir().unwrap();
        let calls = tmp.path().join("calls with spaces.txt");
        let boss = tmp.path().join("boss");
        write_executable(&boss, &boss_stand_in(&calls)).unwrap();
        for case in &CASES {
            let status = Command::new("/bin/sh")
                .args(["-c", case.command])
                .env("BOSS_BIN", &boss)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "{}", case.command);
        }
        let text = fs::read_to_string(&calls).unwrap();
        let expected: Vec<Vec<&str>> = CASES.iter().map(|case| case.argv.to_vec()).collect();
        assert_eq!(recorded_calls(&text), expected);
    }

    #[test]
    fn a_worker_sessions_own_variables_are_kept_out_of_the_probe_session() {
        for name in ["BOSS_BIN", "BOSS_RUN_ID", "BOSS_EVENTS_SOCKET", "CUBE_BIN"] {
            assert!(is_worker_session_variable(name), "{name}");
        }
        for name in ["HOME", "PATH", "AGY_CLI_DISABLE_AUTO_UPDATE"] {
            assert!(!is_worker_session_variable(name), "{name}");
        }
    }

    #[test]
    fn the_prompt_lists_every_case_verbatim() {
        let prompt = prompt();
        for case in &CASES {
            assert!(prompt.contains(case.command), "{}", case.command);
        }
    }
}
