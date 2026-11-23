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

fn relative_from(from_dir: &Path, to: &Path) -> std::path::PathBuf {
    crate::test_support::relative_from(from_dir, to)
}

fn run_approved(data: &Path, brain: &Path, command: &str) {
    assert_eq!(
        verdict(
            data,
            brain,
            json!({
                "session_id": "c1", "tool_name": "Bash",
                "tool_input": {"command": command}
            })
        ),
        "approve",
        "{command}"
    );
    let status = Command::new("bash").arg("-c").arg(command).status().unwrap();
    assert!(status.success(), "{command}");
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
    let scratch = brain.join("c1/scratch");
    // Relative target that, from tmp, stays outside data, but after a move
    // into scratch resolves at data. Destination names are absent so the
    // skip cannot lean on a pre-existing scratch/up link.
    let to_data = relative_from(&scratch, &data);
    std::os::unix::fs::symlink(&to_data, tmp.path().join("prepared-up")).unwrap();
    std::os::unix::fs::symlink(&to_data, tmp.path().join("prepared-link")).unwrap();
    std::fs::create_dir_all(sessions.join("c1/scratch/d")).unwrap();
    std::fs::write(sessions.join("c1/scratch/d/state.db"), "decoy\n").unwrap();
    std::fs::write(sessions.join("c1/scratch/file"), "decoy\n").unwrap();
    let prepared_up = tmp.path().join("prepared-up");
    let prepared_link = tmp.path().join("prepared-link");
    let old_dir = tmp.path().join("old-d");
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
            "mv {} {}/planted && cat {}/planted/state.db",
            prepared_up.display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "cp -a {} {}/planted-cp && cat {}/planted-cp/state.db",
            prepared_link.display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "mv {} {} && mv {} {}/d && cat {}/d/state.db",
            scratch.join("d").display(),
            old_dir.display(),
            prepared_up.display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "mv -f {} {}/file && cat {}/file",
            prepared_up.display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "cp -a {} {}/file && cat {}/file",
            prepared_link.display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "ditto {} {}/planted-ditto && cat {}/planted-ditto/state.db",
            prepared_up.display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "bash -c 'mv {} {}/planted-bash' && cat {}/planted-bash/state.db",
            prepared_up.display(),
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

#[test]
fn same_command_mv_cd_rg_and_nested_bodies_do_not_skip_scratch() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("Boss state");
    let sessions = data.join("executions/run/transcripts/agy/sessions");
    std::fs::create_dir_all(sessions.join("c1/scratch")).unwrap();
    std::fs::write(data.join("state.db"), "secret\n").unwrap();
    let brain = tmp.path().join("brain");
    std::os::unix::fs::symlink(&sessions, &brain).unwrap();
    let scratch = brain.join("c1/scratch");
    let hooks_dir = tmp.path().join("gemini-config");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    std::fs::write(hooks_dir.join("hooks.json"), "{}\n").unwrap();
    let to_data = relative_from(&scratch, &data);
    let to_hooks = relative_from(&scratch, &hooks_dir);
    let prepared_data = tmp.path().join("prepared-up");
    let prepared_hooks = tmp.path().join("prepared-hooks");
    run_approved(
        &data,
        &brain,
        &format!("ln -s {} {}", to_data.display(), prepared_data.display()),
    );
    run_approved(
        &data,
        &brain,
        &format!("ln -s {} {}", to_hooks.display(), prepared_hooks.display()),
    );
    for command in [
        format!(
            "mv {} {}/up\ncd {}/up\nrg -a --hidden --no-ignore .",
            prepared_data.display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "mv {} {}/up && cd {}/up && rg -a --hidden --no-ignore .",
            prepared_data.display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "bash -c 'mv {} {}/up'; bash -c 'cat {}/up/state.db'",
            prepared_data.display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "true\nbash -c 'true\nmv {} {}/up'; bash -c 'cat {}/up/state.db'",
            prepared_data.display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "mv {} {}/hooks\ncd {}/hooks\nrg -a --hidden --no-ignore .",
            prepared_hooks.display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "bash -c 'mv {} {}/hooks'; bash -c 'cat {}/hooks/hooks.json'",
            prepared_hooks.display(),
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

#[test]
fn posix_class_later_in_bracket_does_not_skip_scratch_after_mv() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("Boss state");
    let sessions = data.join("executions/run/transcripts/agy/sessions");
    std::fs::create_dir_all(sessions.join("c1/scratch")).unwrap();
    std::fs::write(data.join("state.db"), "secret\n").unwrap();
    let brain = tmp.path().join("brain");
    std::os::unix::fs::symlink(&sessions, &brain).unwrap();
    let scratch = brain.join("c1/scratch");
    let hooks_dir = tmp.path().join("gemini-config");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    std::fs::write(hooks_dir.join("hooks.json"), "{}\n").unwrap();
    let to_data = relative_from(&scratch, &data);
    let to_hooks = relative_from(&scratch, &hooks_dir);
    let prepared_data = tmp.path().join("prepared-up");
    let prepared_hooks = tmp.path().join("prepared-hooks");
    run_approved(
        &data,
        &brain,
        &format!("ln -s {} {}", to_data.display(), prepared_data.display()),
    );
    run_approved(
        &data,
        &brain,
        &format!("mv {} {}/up", prepared_data.display(), scratch.display()),
    );
    run_approved(
        &data,
        &brain,
        &format!("ln -s {} {}", to_hooks.display(), prepared_hooks.display()),
    );
    run_approved(
        &data,
        &brain,
        &format!("mv {} {}/hooks.json", prepared_hooks.display(), scratch.display()),
    );
    for command in [
        format!("cat {}/[a[:lower:]]p/state.db", scratch.display()),
        format!("cat {}/[x[=u=]]p/state.db", scratch.display()),
        format!("cat {}/[x[.u.]]p/state.db", scratch.display()),
        format!("cat {}/[a[:lower:]]ooks.json", scratch.display()),
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

fn bash_verdict(data: &Path, brain: &Path, command: &str) -> String {
    verdict(
        data,
        brain,
        json!({
            "session_id": "c1", "tool_name": "Bash",
            "tool_input": {"command": command}
        }),
    )
}

#[test]
fn wildcard_candidates_are_expanded_through_scratch_links() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("Boss state");
    let sessions = data.join("executions/run/transcripts/agy/sessions");
    std::fs::create_dir_all(sessions.join("c1/scratch/a")).unwrap();
    std::fs::write(data.join("state.db"), "secret\n").unwrap();
    std::fs::write(sessions.join("c1/scratch/helper.py"), "print(1)\n").unwrap();
    let brain = tmp.path().join("brain");
    std::os::unix::fs::symlink(&sessions, &brain).unwrap();
    let scratch = brain.join("c1/scratch");
    let command = format!("cat {}/*.py", scratch.display());
    assert_eq!(bash_verdict(&data, &brain, &command), "approve", "{command}");
    for (name, from) in [("up", scratch.clone()), ("a/up", scratch.join("a"))] {
        let prepared = tmp.path().join(format!("prepared-{}", name.replace('/', "-")));
        run_approved(
            &data,
            &brain,
            &format!("ln -s {} {}", relative_from(&from, &data).display(), prepared.display()),
        );
        run_approved(
            &data,
            &brain,
            &format!("mv {} {}/{name}", prepared.display(), scratch.display()),
        );
    }
    let s = scratch.display();
    for command in [
        format!("cat {s}/[u]p/state.db"),
        format!("cat {s}/[]a[:lower:]]p/state.db"),
        format!("cat {s}/[][:lower:]]p/state.db"),
        format!("cat {s}/[a]/[]a[:lower:]]p/state.db"),
        format!("cat {s}/[a]/[u]p/state.db"),
        format!("cat {s}/*/state.db"),
    ] {
        assert_eq!(bash_verdict(&data, &brain, &command), "block", "{command}");
    }
}

#[test]
fn executable_heredoc_bodies_are_inspected_but_data_heredocs_are_not() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("Boss state");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("state.db"), "secret\n").unwrap();
    let brain = tmp.path().join("brain");
    std::fs::create_dir_all(&brain).unwrap();
    let alias = tmp.path().join("boss-alias");
    std::os::unix::fs::symlink(&data, &alias).unwrap();
    let target = alias.join("state.db");
    for command in [
        format!("bash <<'EOF'\ncat {}\nEOF", target.display()),
        format!("sh -s <<EOF\ncat {}\nEOF", target.display()),
        format!("python3 - <<'EOF'\nprint(open(\"{}\").read())\nEOF", target.display()),
        format!("bash <<'EOF'\nbash -c 'cat {}'\nEOF", target.display()),
    ] {
        assert_eq!(bash_verdict(&data, &brain, &command), "block", "{command}");
    }
    let command = format!("cat <<'EOF'\n{}\nEOF", target.display());
    assert_eq!(bash_verdict(&data, &brain, &command), "approve", "{command}");
    assert_eq!(bash_verdict(&data, &brain, "bash <<'EOF'\necho hi\nEOF"), "approve");
}

fn data_fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("Boss state");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("state.db"), "secret\n").unwrap();
    let brain = tmp.path().join("brain");
    std::fs::create_dir_all(&brain).unwrap();
    (tmp, data, brain)
}

#[test]
fn heredocs_piped_into_executors_or_written_then_run_are_inspected() {
    let (tmp, data, brain) = data_fixture();
    let alias = tmp.path().join("boss-alias");
    std::os::unix::fs::symlink(&data, &alias).unwrap();
    let target = alias.join("state.db");
    let t = target.display();
    for command in [
        format!("cat <<'EOF' | bash\ncat {t}\nEOF"),
        format!("cat <<EOF | sh\ncat {t}\nEOF"),
        format!("tee /dev/null <<'EOF' | bash\ncat {t}\nEOF"),
        format!("cat <<'EOF' | python3 -\nprint(open(\"{t}\").read())\nEOF"),
        format!("cat <<'EOF' > s.sh\ncat {t}\nEOF\nbash s.sh"),
    ] {
        assert_eq!(bash_verdict(&data, &brain, &command), "block", "{command}");
    }
    for command in [
        format!("cat <<'EOF' | grep x\n{t}\nEOF"),
        "cat <<'EOF' | bash\necho hi\nEOF".to_owned(),
    ] {
        assert_eq!(bash_verdict(&data, &brain, &command), "approve", "{command}");
    }
}

#[test]
fn heredocs_for_unlisted_consumers_and_spaced_aliases_are_inspected() {
    let (tmp, data, brain) = data_fixture();
    let alias = tmp.path().join("boss-alias");
    std::os::unix::fs::symlink(&data, &alias).unwrap();
    let spaced = tmp.path().join("boss alias");
    std::os::unix::fs::symlink(&data, &spaced).unwrap();
    let t = alias.join("state.db");
    let t = t.display();
    let st = spaced.join("state.db");
    let st = st.display();
    for command in [
        format!("xargs cat <<'EOF'\n{t}\nEOF"),
        format!("cat <<'EOF' | xargs cat\n{t}\nEOF"),
        format!("cat <<'EOF' | awk '{{system($0)}}'\ncat {t}\nEOF"),
        format!("cat <<'EOF' | sqlite3\n.open {t}\nEOF"),
        format!("cat <<'EOF' > s.sh\ncat {t}\nEOF\n./s.sh"),
        format!("cat <<'EOF' | python3 -\nprint(open(\"{st}\").read())\nEOF"),
        format!("cat <<'EOF' > s.sh\ncat '{st}'\nEOF\nbash s.sh"),
        format!(
            "cat <<'EOF' > s.sh\ncat {}\nEOF\nbash s.sh",
            st.to_string().replace(' ', "\\ ")
        ),
    ] {
        assert_eq!(bash_verdict(&data, &brain, &command), "block", "{command}");
    }
    for command in [
        format!("cat <<'EOF' > notes.txt\n{t}\nEOF"),
        format!("cat <<'EOF' | grep x\n{st}\nEOF"),
    ] {
        assert_eq!(bash_verdict(&data, &brain, &command), "approve", "{command}");
    }
}

#[test]
fn wildcards_keep_the_literal_check_and_native_paths_are_not_expanded() {
    let (tmp, data, brain) = data_fixture();
    let d = data.display();
    for command in [
        format!("touch {d}/x*"),
        format!("mkdir {d}/d?"),
        format!("touch {d}/[n]ew"),
    ] {
        assert_eq!(bash_verdict(&data, &brain, &command), "block", "{command}");
    }
    // Same-command plant-and-read: the expansion is empty when the guard runs.
    let sessions = data.join("executions/run/transcripts/agy/sessions");
    std::fs::create_dir_all(sessions.join("c1/scratch")).unwrap();
    std::os::unix::fs::symlink(&sessions, tmp.path().join("brain2")).unwrap();
    let brain2 = tmp.path().join("brain2");
    let s = brain2.join("c1/scratch");
    let s = s.display();
    for command in [
        format!("mv /tmp/p {s}/up; cat {s}/u*/state.db"),
        format!("mv /tmp/p {s}/up; cat {s}/[u]p/state.db"),
    ] {
        assert_eq!(bash_verdict(&data, &brain2, &command), "block", "{command}");
    }
    // A native tool's path is a literal name: a bracket is not a wildcard.
    let alias = tmp.path().join("alias1");
    std::os::unix::fs::symlink(data.join("state.db"), &alias).unwrap();
    let bracketed = tmp.path().join("alias[1]");
    std::os::unix::fs::symlink(data.join("state.db"), &bracketed).unwrap();
    let native = |tool: &str, key: &str, path: &Path| {
        verdict(
            &data,
            &brain,
            json!({"session_id": "c1", "tool_name": tool, "tool_input": {key: path}}),
        )
    };
    assert_eq!(native("Read", "file_path", &bracketed), "block");
    assert_eq!(native("Write", "file_path", &data.join("new[1].txt")), "block");
    assert_eq!(native("Write", "file_path", &data.join("new*.txt")), "block");
    assert_eq!(
        native("NotebookEdit", "notebook_path", &data.join("n[1].ipynb")),
        "block"
    );
    let patch = format!("*** Begin Patch\n*** Add File: {}/p[1].txt\n+x\n*** End Patch", d);
    assert_eq!(
        verdict(
            &data,
            &brain,
            json!({"session_id": "c1", "tool_name": "apply_patch", "tool_input": {"input": patch}}),
        ),
        "block"
    );
    assert_eq!(native("Read", "file_path", &tmp.path().join("plain[1].txt")), "approve");
}

#[test]
fn glob_expansion_past_the_limit_fails_closed() {
    let (tmp, data, brain) = data_fixture();
    let aliases = tmp.path().join("aliases");
    for index in 0..2001 {
        std::fs::create_dir_all(aliases.join(format!("d{index:04}"))).unwrap();
    }
    std::os::unix::fs::symlink(&data, aliases.join("zz")).unwrap();
    let command = format!("cat {}/*/state.db", aliases.display());
    assert_eq!(bash_verdict(&data, &brain, &command), "block", "{command}");
}
