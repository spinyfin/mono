use super::PATH_GUARD_SCRIPT;
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn verdict(data: &Path, brain: &Path, payload: Value) -> String {
    let mut child = Command::new("python3")
        .args(["-c", PATH_GUARD_SCRIPT])
        .env("BOSS_DATA_DIR", data)
        .env("BOSS_AGY_SCRATCH_ROOT", brain)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    result["decision"].as_str().unwrap().to_owned()
}

#[test]
fn conversation_allowance_is_scoped_in_both_path_and_substring_checks() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("Boss state");
    let sessions = data.join("executions/run/transcripts/agy/sessions");
    std::fs::create_dir_all(sessions.join("c1/scratch")).unwrap();
    std::fs::create_dir_all(sessions.join("c1/.system_generated/steps/3")).unwrap();
    let brain = tmp.path().join("brain");
    std::os::unix::fs::symlink(&sessions, &brain).unwrap();
    for base in [&brain, &sessions] {
        let safe = base.join("c1/scratch/helper.py");
        let spill = base.join("c1/.system_generated/steps/3/output.txt");
        let task = base.join("c1/.system_generated/tasks/task-2.log");
        for path in [&safe, &spill, &task] {
            for tool in ["Read", "Write"] {
                assert_eq!(
                    verdict(
                        &data,
                        &brain,
                        json!({
                            "session_id": "c1", "tool_name": tool, "tool_input": {"file_path": path}
                        })
                    ),
                    "approve"
                );
            }
            assert_eq!(
                verdict(
                    &data,
                    &brain,
                    json!({
                        "session_id": "c1", "tool_name": "Bash",
                        "tool_input": {"command": format!("cat '{}'", path.display())}
                    })
                ),
                "approve"
            );
        }
        for path in [
            base.join("c1"),
            base.join("c1/.system_generated"),
            base.join("c1/.system_generated/tasks/.."),
            base.join("c1/.system_generated/logs/transcript_full.jsonl"),
            base.join("c2/scratch/helper.py"),
            data.join("settings.json"),
            base.join("c1/scratch/../.system_generated/logs/transcript_full.jsonl"),
        ] {
            let physical = path
                .strip_prefix(&brain)
                .map_or_else(|_| path.clone(), |suffix| sessions.join(suffix));
            for command in [
                format!("cat '{}'", path.display()),
                format!("cat '{}' '{}'", safe.display(), path.display()),
                format!("cat '{}'; P='{}'; cat \"$P\"", safe.display(), physical.display()),
            ] {
                assert_eq!(
                    verdict(
                        &data,
                        &brain,
                        json!({
                            "session_id": "c1", "tool_name": "Bash", "tool_input": {"command": command}
                        })
                    ),
                    "block",
                    "{command}"
                );
            }
        }
        for session in ["c2", "../c1", "c1/../c1", ""] {
            assert_eq!(
                verdict(
                    &data,
                    &brain,
                    json!({
                        "session_id": session, "tool_name": "Read", "tool_input": {"file_path": safe}
                    })
                ),
                "block"
            );
        }
        // A safe operand elsewhere in the call must not mask the prefix of
        // a quoted assignment whose concatenated suffix escapes scratch.
        if base == &sessions {
            let command = format!(
                "cat '{}'; P='{}'/../../.system_generated/logs/transcript_full.jsonl; cat \"$P\"",
                safe.display(),
                safe.display()
            );
            assert_eq!(
                verdict(
                    &data,
                    &brain,
                    json!({
                        "session_id": "c1", "tool_name": "Bash", "tool_input": {"command": command}
                    })
                ),
                "block"
            );
        }
    }
    std::os::unix::fs::symlink(data.join("settings.json"), sessions.join("c1/scratch/alias")).unwrap();
    assert_eq!(
        verdict(
            &data,
            &brain,
            json!({
                "session_id": "c1", "tool_name": "Read",
                "tool_input": {"file_path": brain.join("c1/scratch/alias")}
            })
        ),
        "block"
    );
}

#[test]
fn link_creation_and_unmodeled_bracket_globs_do_not_skip_scratch() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("Boss state");
    let sessions = data.join("executions/run/transcripts/agy/sessions");
    std::fs::create_dir_all(sessions.join("c1/scratch")).unwrap();
    std::fs::write(data.join("state.db"), "secret\n").unwrap();
    let brain = tmp.path().join("brain");
    std::os::unix::fs::symlink(&sessions, &brain).unwrap();
    std::os::unix::fs::symlink(&data, sessions.join("c1/scratch/up")).unwrap();
    std::os::unix::fs::symlink(&data, tmp.path().join("prepared-up")).unwrap();
    std::os::unix::fs::symlink(&data, tmp.path().join("prepared-link")).unwrap();
    let scratch = brain.join("c1/scratch");
    for command in [
        format!("ln -s ../../../../../../.. {}/climb", scratch.display()),
        format!("echo ok\nln -s ../../../../../../.. {}/climb", scratch.display()),
        format!("FOO=1 ln -s ../../../../../../.. {}/climb", scratch.display()),
        format!("env ln -s ../../../../../../.. {}/climb", scratch.display()),
        format!("command ln -s ../../../../../../.. {}/climb", scratch.display()),
        format!("(ln -s ../../../../../../.. {}/climb)", scratch.display()),
        format!("env cp -s ../../../../../../.. {}/climb", scratch.display()),
        format!("cat {}/[[:lower:]]p/state.db", scratch.display()),
        format!("cat {}/[[=u=]]p/state.db", scratch.display()),
        format!("cat {}/[[.u.]]p/state.db", scratch.display()),
        format!("cat {}/[!x]p/state.db", scratch.display()),
        format!(
            "ln -s ../../../../../../.. {}/climb && cat {}/climb/state.db",
            scratch.display(),
            scratch.display()
        ),
        format!(
            "python3 -c 'import os; os.symlink(\"../../../../../../..\", \"{}/climb\")' && cat {}/climb/state.db",
            scratch.display(),
            scratch.display()
        ),
        format!(
            "mv {} {}/up && cat {}/up/state.db",
            tmp.path().join("prepared-up").display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "cp -a {} {}/up && cat {}/up/state.db",
            tmp.path().join("prepared-link").display(),
            scratch.display(),
            scratch.display()
        ),
    ] {
        assert_eq!(
            verdict(
                &data,
                &brain,
                json!({
                    "session_id": "c1", "tool_name": "Bash",
                    "tool_input": {"command": command}
                })
            ),
            "block",
            "{command}"
        );
    }
}
