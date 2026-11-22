//! A worker may use its own conversation's scratch and spill files, and the
//! shell analyser respects quoting. Everything else under the agy home stays
//! protected, and commands that really substitute stay denied.

use super::*;

const CONVERSATION: &str = "c1";

fn brain(fx: &Fixture) -> PathBuf {
    fx.process_home.join(".gemini/antigravity-cli/brain")
}

fn conversation_dir(fx: &Fixture) -> PathBuf {
    brain(fx).join(CONVERSATION)
}

/// Make this run's conversation directory with the files agy itself leaves
/// there: a model-written scratch script, a spilled command output, and the
/// transcript the engine reads back.
fn seed_conversation(fx: &Fixture) {
    let dir = conversation_dir(fx);
    fs::create_dir_all(dir.join("scratch")).unwrap();
    fs::create_dir_all(dir.join(".system_generated/steps/3")).unwrap();
    fs::create_dir_all(dir.join(".system_generated/tasks")).unwrap();
    fs::write(dir.join(".system_generated/tasks/task-2.log"), "build output\n").unwrap();
    fs::create_dir_all(dir.join(".system_generated/logs")).unwrap();
    fs::write(dir.join("scratch/split_modules.py"), "print('split')\n").unwrap();
    fs::write(dir.join(".system_generated/steps/3/output.txt"), "full output\n").unwrap();
    fs::write(dir.join(".system_generated/logs/transcript_full.jsonl"), "{}\n").unwrap();
}

fn payload_for(conversation: Option<&str>, tool: &str, args: Value) -> Value {
    let mut payload = json!({"hookEventName": "PreToolUse", "toolCall": {"id": "t1", "name": tool, "args": args}});
    if let Some(conversation) = conversation {
        payload["conversationId"] = json!(conversation);
    }
    payload
}

fn command_in(fx: &Fixture, conversation: Option<&str>, command: &str) -> Value {
    payload_for(
        conversation,
        "run_command",
        json!({"CommandLine": command, "Cwd": fx.workspace()}),
    )
}

fn verdict(fx: &Fixture, payload: &Value) -> (String, String) {
    let (_, body, stderr) = authorize(&fx.process_home, payload);
    (
        decision(&body).to_owned(),
        format!("{} {}", body["reason"].as_str().unwrap_or_default(), stderr),
    )
}

fn assert_allowed(fx: &Fixture, payload: &Value, what: &str) {
    let (decision, detail) = verdict(fx, payload);
    assert_eq!(decision, "auto_approve", "{what}: {detail}");
}

fn assert_denied(fx: &Fixture, payload: &Value, what: &str) -> String {
    let (decision, detail) = verdict(fx, payload);
    assert_eq!(decision, "deny", "{what}: {detail}");
    detail
}

fn assert_command_allowed(fx: &Fixture, command: &str) {
    assert_allowed(fx, &command_in(fx, Some(CONVERSATION), command), command);
}

fn assert_command_denied(fx: &Fixture, command: &str) -> String {
    assert_denied(fx, &command_in(fx, Some(CONVERSATION), command), command)
}

#[test]
fn own_conversation_scratch_and_spill_files_are_usable() {
    let fx = Fixture::new("run-own-scratch");
    fx.install(WorkerKind::Standard);
    seed_conversation(&fx);
    let dir = conversation_dir(&fx);
    let script = dir.join("scratch/split_modules.py");
    let spill = dir.join(".system_generated/steps/3/output.txt");

    // The native file tools: write the helper, read the spilled output back.
    for (tool, args) in [
        ("write_file", json!({"Path": script, "Contents": "print(1)\n"})),
        ("edit_file", json!({"Path": script})),
        ("read_file", json!({"Path": spill})),
        (
            "read_file",
            json!({"Path": dir.join(".system_generated/tasks/task-2.log")}),
        ),
        ("list_dir", json!({"Path": dir.join("scratch")})),
        (
            "grep",
            json!({"Path": dir.join(".system_generated/steps"), "Pattern": "full"}),
        ),
    ] {
        assert_allowed(
            &fx,
            &payload_for(Some(CONVERSATION), tool, args),
            &format!("{tool} on own conversation"),
        );
    }

    // Shell: run the helper script, read the spill, redirect into scratch, in
    // the absolute, `$HOME` and `~` spellings.
    let (script, spill, scratch) = (
        script.display(),
        spill.display(),
        dir.join("scratch").display().to_string(),
    );
    for command in [
        format!("python3 {script}"),
        format!("python3 \"$HOME/.gemini/antigravity-cli/brain/{CONVERSATION}/scratch/split_modules.py\""),
        format!("python3 ~/.gemini/antigravity-cli/brain/{CONVERSATION}/scratch/split_modules.py"),
        format!("cat {spill}"),
        format!("tail -n 50 {spill}"),
        format!("grep -n error {spill}"),
        format!("echo hi > {scratch}/notes.txt"),
        format!("cat {scratch}/*.py"),
        format!("cat {}/.system_generated/steps/*/output.txt", dir.display()),
        format!("cd {scratch} && python3 split_modules.py"),
        format!("ls {scratch}"),
    ] {
        assert_command_allowed(&fx, &command);
    }
}

#[test]
fn own_conversation_reads_preserve_each_kinds_approval_posture() {
    for kind in [
        WorkerKind::Standard,
        WorkerKind::Reviewer,
        WorkerKind::Triage,
        WorkerKind::AnswerAgent,
        WorkerKind::ReviewGuide,
        WorkerKind::Progression,
    ] {
        let fx = Fixture::new("run-scratch-kinds");
        fx.install(kind);
        seed_conversation(&fx);
        let spill = conversation_dir(&fx).join(".system_generated/steps/3/output.txt");
        let payload = command_in(&fx, Some(CONVERSATION), &format!("cat {}", spill.display()));
        let (_, body, stderr) = authorize(&fx.process_home, &payload);
        let expected = match kind {
            WorkerKind::Standard | WorkerKind::Reviewer | WorkerKind::Triage => "auto_approve",
            WorkerKind::AnswerAgent | WorkerKind::ReviewGuide | WorkerKind::Progression => "deny_unless_prior_grant",
        };
        assert_eq!(decision(&body), expected, "{kind:?}: {body} {stderr}");
        let (read, detail) = verdict(
            &fx,
            &payload_for(Some(CONVERSATION), "read_file", json!({"Path": spill})),
        );
        assert_eq!(read, expected, "{kind:?} read_file: {detail}");
    }
}

#[test]
fn real_data_guard_allows_durable_scratch_but_fences_transcripts_and_state() {
    let fx = Fixture::new("run-durable-scratch");
    let data = fx.tmp.path().join("homes/.transcript-store");
    let sessions = fs::canonicalize(brain(&fx)).unwrap();
    // Use the real provisioned durable brain link, with its store protected
    // by BOSS_DATA_DIR just as the executions store is in production.
    assert!(sessions.starts_with(fs::canonicalize(&data).unwrap()));
    fx.install_with(WorkerKind::Standard, |config| config.data_dir = Some(data.clone()));
    seed_conversation(&fx);
    for dir in [conversation_dir(&fx), sessions.join(CONVERSATION)] {
        let script = dir.join("scratch/split_modules.py");
        let spill = dir.join(".system_generated/steps/3/output.txt");
        for (tool, args) in [
            ("write_file", json!({"Path": script, "Contents": "print('split')\n"})),
            ("read_file", json!({"Path": script})),
            ("read_file", json!({"Path": spill})),
            (
                "read_file",
                json!({"Path": dir.join(".system_generated/tasks/task-2.log")}),
            ),
        ] {
            assert_allowed(&fx, &payload_for(Some(CONVERSATION), tool, args), tool);
        }
        for command in [
            format!("python3 '{}'", script.display()),
            format!("cat '{}'", spill.display()),
            format!("cat '{}'", dir.join(".system_generated/tasks/task-2.log").display()),
        ] {
            assert_command_allowed(&fx, &command);
            let output = Command::new("/bin/sh").args(["-c", &command]).output().unwrap();
            assert!(output.status.success(), "{command}: {output:?}");
            assert!(!output.stdout.is_empty(), "{command}");
        }
        for ancestor in [
            dir.clone(),
            dir.join(".system_generated"),
            dir.join(".system_generated/tasks/.."),
        ] {
            for command in [
                format!("mv '{}' /tmp/moved", ancestor.display()),
                format!("rm -rf '{}'", ancestor.display()),
                format!("cp -r '{}' /tmp/copied", ancestor.display()),
                format!("grep -r secret '{}'", ancestor.display()),
            ] {
                assert_command_denied(&fx, &command);
            }
        }
    }
    for path in [
        data.join("state.db"),
        sessions.join("c2/scratch/helper.py"),
        sessions.join("c1/.system_generated/logs/transcript_full.jsonl"),
        fx.process_home.join(".gemini/antigravity-cli/settings.json"),
    ] {
        for tool in ["write_file", "read_file"] {
            assert_denied(&fx, &payload_for(Some(CONVERSATION), tool, json!({"Path": path})), tool);
        }
        assert_command_denied(&fx, &format!("cat '{}'", path.display()));
        assert_command_denied(
            &fx,
            &format!(
                "cat '{}' '{}'",
                sessions.join("c1/scratch/split_modules.py").display(),
                path.display()
            ),
        );
    }
}

#[test]
fn everything_else_under_the_agy_home_stays_protected() {
    let fx = Fixture::new("run-scratch-fences");
    fx.install(WorkerKind::Standard);
    seed_conversation(&fx);
    let other = brain(&fx).join("c2");
    fs::create_dir_all(other.join("scratch")).unwrap();
    fs::write(other.join("scratch/x.py"), "").unwrap();
    let dir = conversation_dir(&fx);
    let config = fx.process_home.join(".gemini/config");
    let control = fx.process_home.parent().unwrap().join("control");
    let settings = fx.process_home.join(".gemini/antigravity-cli/settings.json");
    let transcript = dir.join(".system_generated/logs/transcript_full.jsonl");

    for path in [
        other.join("scratch/x.py"),
        brain(&fx),
        dir.join("../c2/scratch/x.py"),
        dir.join("scratch/../../c2/scratch/x.py"),
        settings.clone(),
        config.join("hooks.json"),
        config.join("boss-agy-hook-adapter.py"),
        control.join("session-settings.json"),
        transcript.clone(),
        dir.join(".system_generated/logs"),
        dir.clone(),
        dir.join(".system_generated"),
    ] {
        let what = path.display().to_string();
        assert_denied(
            &fx,
            &payload_for(Some(CONVERSATION), "read_file", json!({"Path": path})),
            &format!("read_file {what}"),
        );
        assert_denied(
            &fx,
            &payload_for(Some(CONVERSATION), "write_file", json!({"Path": path, "Contents": "x"})),
            &format!("write_file {what}"),
        );
        assert_command_denied(&fx, &format!("cat {what}"));
        assert_command_denied(&fx, &format!("echo x > {what}"));
    }
    for command in [
        format!("mv {}/.system_generated {}/sg", dir.display(), dir.display()),
        format!("rm -rf {}/.system_generated", dir.display()),
        format!("cp -r {} /tmp/conversation-copy", dir.display()),
        format!("grep -r secret {}", dir.display()),
        format!("mv {} /tmp/conversation-moved", dir.display()),
        format!("rm -rf {}", dir.display()),
        format!("cp -r {}/.system_generated /tmp/generated-copy", dir.display()),
        format!("grep -r secret {}/.system_generated", dir.display()),
        format!("python3 {}", other.join("scratch/x.py").display()),
        format!("cat {}/*/scratch/x.py", brain(&fx).display()),
        format!("cat {}/c?/scratch/x.py", brain(&fx).display()),
        // A wildcard directly under the conversation directory or its
        // `.system_generated` directory could match the transcript.
        format!("cat {}/.system_generated/*/transcript_full.jsonl", dir.display()),
        format!("cat {}/.sys*/logs/transcript_full.jsonl", dir.display()),
        format!(
            "cat {}/*/logs/transcript_full.jsonl",
            dir.join(".system_generated").display()
        ),
        format!("cat {}/scratch/../../../config/hooks.json", dir.display()),
        format!("cat {}/scratch/*/../../../../config/hooks.json", dir.display()),
        "cat ~/.gemini/config/hooks.json".to_owned(),
        "cat \"$HOME/.gemini/antigravity-cli/settings.json\"".to_owned(),
        format!("cd {} && ls", brain(&fx).display()),
        format!("cd {} && ls", other.display()),
    ] {
        assert_command_denied(&fx, &command);
    }

    // Another conversation's id, no id, and ids that are not plain names do
    // not unlock this one.
    let own_script = dir.join("scratch/split_modules.py");
    for conversation in [
        Some("c2"),
        Some("c1/../c2"),
        Some("../config"),
        Some(".."),
        Some(""),
        None,
    ] {
        assert_denied(
            &fx,
            &payload_for(conversation, "read_file", json!({"Path": own_script})),
            &format!("conversation {conversation:?}"),
        );
        assert_denied(
            &fx,
            &command_in(&fx, conversation, &format!("cat {}", own_script.display())),
            &format!("conversation {conversation:?} shell"),
        );
    }
}

#[test]
fn links_cannot_carry_the_allowance_out_of_the_conversation_directory() {
    let fx = Fixture::new("run-scratch-links");
    fx.install(WorkerKind::Standard);
    seed_conversation(&fx);
    let dir = conversation_dir(&fx);
    let hooks = fx.process_home.join(".gemini/config/hooks.json");

    // A link inside the directory to protected state resolves to that state.
    std::os::unix::fs::symlink(&hooks, dir.join("scratch/alias.json")).unwrap();
    std::os::unix::fs::symlink(fx.process_home.join(".gemini/config"), dir.join("scratch/config")).unwrap();
    let alias = dir.join("scratch/alias.json");
    assert_denied(
        &fx,
        &payload_for(Some(CONVERSATION), "read_file", json!({"Path": alias})),
        "link file",
    );
    assert_command_denied(&fx, &format!("cat {}", alias.display()));
    assert_command_denied(&fx, &format!("cat {}/scratch/config/hooks.json", dir.display()));
    assert_command_denied(&fx, &format!("cat {}/scratch/conf*/hooks.json", dir.display()));

    // A conversation directory that is itself a link to protected state is not
    // this run's conversation directory.
    std::os::unix::fs::symlink(fx.process_home.join(".gemini/config"), brain(&fx).join("c9")).unwrap();
    let through = brain(&fx).join("c9/hooks.json");
    assert_denied(
        &fx,
        &payload_for(Some("c9"), "read_file", json!({"Path": through})),
        "linked conversation directory",
    );
    assert_denied(
        &fx,
        &command_in(&fx, Some("c9"), &format!("cat {}", through.display())),
        "linked conversation directory shell",
    );
}

// --- Quoting -----------------------------------------------------------------

/// The Python body from a real run: f-strings, `!r`, a slice with spaces
/// around its colon, an apostrophe in a comment, and the backticks and `$(`
/// that Rust doc comments and shell snippets carry.
fn python_body(scratch: &str) -> String {
    format!(
        "import re, sys\n\
         # don't split on the `pub fn` lines inside `impl` blocks, nor $(these)\n\
         lines = open(\"{scratch}/lib.rs\").read().splitlines()\n\
         for number, line in enumerate(lines[start_line - 1 : end_line]):\n\
         \x20   if line.startswith(\"\\t\"):\n\
         \x20       sys.exit(f\"Unexpected indentation in {{filename}}: {{line!r}}\")\n\
         print(f\"{{len(lines)}} lines; first={{lines[0]!r}}; \\`literal\\`\")\n"
    )
}

#[test]
fn quoted_heredoc_and_single_quoted_python_bodies_are_data() {
    let fx = Fixture::new("run-quoted-python");
    fx.install(WorkerKind::Standard);
    seed_conversation(&fx);
    fs::write(fx.workspace().join("script.py"), "print(1)\n").unwrap();
    fs::write(fx.workspace().join("script<name.py"), "print(1)\n").unwrap();
    let scratch = conversation_dir(&fx).join("scratch");
    let scratch = scratch.display().to_string();
    let body = python_body(&scratch);

    for command in [
        // A quoted heredoc in the Python body shape from the report.
        format!("python3 - <<'PY'\n{body}PY"),
        format!("python3 - <<'PY'\n{body}PY\necho done"),
        format!("python3 - 2>/dev/null <<'PY'\n{body}PY"),
        format!("python3 - <<'PY' 2>&1\n{body}PY"),
        "node - <<'JS' >&2\nconsole.log(1)\nJS".to_owned(),
        "python3 - < script.py 2>&1".to_owned(),
        "python3 - < script.py >&2".to_owned(),
        "python3 - < script.py &>/dev/null".to_owned(),
        "python3 - <<'PY' &>>log\nprint(1)\nPY".to_owned(),
        "python3 - < 'script<name.py'".to_owned(),
        format!("python3 - <<\"PY\"\n{body}PY"),
        format!("python3 - <<-'PY'\n{body}PY"),
        format!("cd {scratch} && python3 - <<'PY'\n{body}PY"),
        // The same body as a single-quoted `-c` string. A single quote cannot
        // occur inside one, so its apostrophe is spelled out.
        format!("python3 -c '{}'", body.replace('\'', "")),
        format!("python3 -c '{}' && echo done", body.replace('\'', "")),
        // Single-quoted data with a substitution in it, and no live arithmetic.
        "echo '$(id) `id` ${x:-1} lines[a - 1 : b]'".to_owned(),
        "grep -n '`' src/lib.rs".to_owned(),
        "grep -n 'x$(y)' src/lib.rs".to_owned(),
    ] {
        assert_command_allowed(&fx, &command);
    }
}

#[test]
fn substitutions_that_really_run_are_still_denied() {
    let fx = Fixture::new("run-live-substitution");
    fx.install(WorkerKind::Standard);
    seed_conversation(&fx);
    let scratch = conversation_dir(&fx).join("scratch");
    let scratch = scratch.display().to_string();
    let protected = fx.process_home.join(".gemini/config/hooks.json");
    let protected = protected.display();

    for command in [
        // An unquoted heredoc is expanded by the shell.
        format!("python3 - <<PY\nprint('$(cat {protected})')\nPY"),
        format!("python3 - <<PY\nprint('`cat {protected}`')\nPY"),
        "python3 - <<PY\nx = $(id)\nPY".to_owned(),
        // Quote characters are plain text inside an unquoted heredoc, so a
        // `$(` after an apostrophe still runs.
        "cat <<EOF\ndon't\nEOF\necho $(id)".to_owned(),
        // Double quotes expand.
        format!("python3 -c \"print('$(cat {protected})')\""),
        format!("python3 -c \"print('`cat {protected}`')\""),
        format!("echo \"$(cat {protected})\""),
        format!("python3 -c 'print(1)' \"$(cat {protected})\""),
        // Substitution outside any quoting, beside quoted data.
        format!("echo '{{x}}' $(cat {protected})"),
        format!("python3 - <<'PY'\nprint(1)\nPY\necho $(cat {protected})"),
        // The quoted heredoc ends where its delimiter does; what follows runs.
        "python3 - <<'PY'\nprint(1)\nPY\necho \"`id`\"".to_string(),
    ] {
        let reason = assert_command_denied(&fx, &command);
        assert!(
            reason.contains("command substitution is structurally denied") || reason.contains("protected"),
            "{command}: {reason}"
        );
    }

    // Data that carries a substitution becomes live when an arithmetic
    // context, `[[`, `let` or a subscript in the shell code evaluates it.
    for command in [
        "x='a[$(id)]'; (( x ))".to_owned(),
        "x='a[$(id)]'; [[ $x -eq 1 ]]".to_owned(),
        "x='a[`id`]'; let y=x".to_owned(),
        "x='a[$(id)]'; declare -i x".to_owned(),
        "read x <<'E'\na[$(id)]\nE\n(( x ))".to_owned(),
        "python3 - <<'PY'\nlines[a - 1 : b] = `x`\nPY\n(( y = 1 ))".to_owned(),
    ] {
        let reason = assert_command_denied(&fx, &command);
        assert!(
            reason.contains("cannot be inspected") || reason.contains("denied"),
            "{command}: {reason}"
        );
    }

    // Removing a redirection does not hide a command that opens a stdin channel.
    for command in [
        "bash 2>/dev/null",
        "python3 - </dev/tty",
        "python3 - < //dev/tty",
        "python3 - < /./dev/tty",
        "python3 - < ../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../dev/tty",
        "cd /dev && python3 - < tty",
        "python3 - < script.py &>log</dev/tty",
        "python3 - <<'PY' &>>log</dev/tty\nprint(1)\nPY",
        "python3 - <script.py</dev/tty",
        "python3 - < script.py</dev/tty",
        "python3 - <<'PY' | python3 -\nprint(1)\nPY",
    ] {
        assert_command_denied(&fx, command);
    }

    // Shell that cannot be parsed stays denied.
    for command in [
        format!("python3 -c 'print({scratch:?})"),
        "python3 - <<'PY'\nprint(1)".to_owned(),
        "python3 -c \"print(1)".to_owned(),
    ] {
        assert_command_denied(&fx, &command);
    }
}

#[test]
fn stdin_redirects_resolve_workspace_symlinks_and_reject_special_files() {
    let fx = Fixture::new("run-stdin-paths");
    fx.install(WorkerKind::Standard);
    std::os::unix::fs::symlink("/dev/tty", fx.workspace().join("terminal")).unwrap();
    fs::write(fx.workspace().join("regular.py"), "print(1)\n").unwrap();
    std::os::unix::fs::symlink("regular.py", fx.workspace().join("script-link")).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(fx.workspace().join("fifo"))
            .status()
            .unwrap()
            .success()
    );
    for target in ["terminal", "fifo", "."] {
        let reason = assert_command_denied(&fx, &format!("python3 - < {target}"));
        assert!(reason.contains("stdin"), "{target}: {reason}");
    }
    assert_command_allowed(&fx, "python3 - < script-link");
}

#[test]
fn shell_bodies_are_still_inspected_and_python_bodies_are_not_run_as_shell() {
    let fx = Fixture::new("run-shell-bodies");
    fx.install(WorkerKind::Standard);
    for command in [
        "sh -c 'sudo id'",
        "bash -c 'echo $(sudo id)'",
        "bash -c 'git push'",
        "bash <<'EOF'\nsudo id\nEOF",
        "find . -exec sudo id ';'",
    ] {
        assert_command_denied(&fx, command);
    }
    assert_command_allowed(&fx, "python3 -c 'print(`1`)  # not shell'");
}

fn relative_from(from_dir: &Path, to: &Path) -> PathBuf {
    let from = fs::canonicalize(from_dir).unwrap();
    let to = if to.exists() {
        fs::canonicalize(to).unwrap()
    } else {
        to.to_path_buf()
    };
    let mut cursor = from.as_path();
    let mut up = PathBuf::new();
    loop {
        if to.starts_with(cursor) {
            return up.join(to.strip_prefix(cursor).unwrap());
        }
        match cursor.parent() {
            Some(parent) => {
                up.push("..");
                cursor = parent;
            }
            None => return up.join(&to),
        }
    }
}

#[test]
fn same_command_symlink_in_scratch_cannot_reach_protected_state() {
    let fx = Fixture::new("run-scratch-ln-same");
    let data = fx.tmp.path().join("homes/.transcript-store");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("state.db"), "secret\n").unwrap();
    fx.install_with(WorkerKind::Standard, |config| config.data_dir = Some(data.clone()));
    seed_conversation(&fx);
    let scratch = conversation_dir(&fx).join("scratch");
    let hooks = fx.process_home.join(".gemini/config/hooks.json");
    let to_data = relative_from(&scratch, &data);
    let to_hooks = relative_from(&scratch, &hooks);
    std::os::unix::fs::symlink(&to_data, fx.workspace().join("prepared-up")).unwrap();
    std::os::unix::fs::symlink(&to_data, fx.workspace().join("prepared-link")).unwrap();
    fs::create_dir_all(scratch.join("d")).unwrap();
    fs::write(scratch.join("d/state.db"), "decoy\n").unwrap();
    fs::write(scratch.join("file"), "decoy\n").unwrap();
    let home_scratch = format!("$HOME/.gemini/antigravity-cli/brain/{CONVERSATION}/scratch");
    for command in [
        format!(
            "ln -s {} {}/up && cat {}/up/state.db",
            to_data.display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "ln -s {} {}/hooks && cat {}/hooks",
            to_hooks.display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "ln -s {} {home_scratch}/up && cat {home_scratch}/up/state.db",
            to_data.display(),
        ),
        format!("ln -s {} {}/up", to_data.display(), scratch.display()),
        format!("cp -s {} {}/up", to_data.display(), scratch.display()),
        format!(
            "python3 -c 'import os; os.symlink({:?}, {:?})' && cat {}/up/state.db",
            to_data.to_string_lossy(),
            scratch.join("up").to_string_lossy(),
            scratch.display()
        ),
        format!(
            "mv {} {}/up && cat {}/up/state.db",
            fx.workspace().join("prepared-up").display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "cp -a {} {}/up && cat {}/up/state.db",
            fx.workspace().join("prepared-link").display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "mv {} {}/old-d && mv {} {}/d && cat {}/d/state.db",
            scratch.join("d").display(),
            fx.workspace().display(),
            fx.workspace().join("prepared-up").display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "mv -f {} {}/file && cat {}/file",
            fx.workspace().join("prepared-up").display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "cp -a {} {}/file && cat {}/file",
            fx.workspace().join("prepared-link").display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "ditto {} {}/planted && cat {}/planted/state.db",
            fx.workspace().join("prepared-up").display(),
            scratch.display(),
            scratch.display()
        ),
        format!(
            "bash -c 'mv {} {}/planted-bash' && cat {}/planted-bash/state.db",
            fx.workspace().join("prepared-up").display(),
            scratch.display(),
            scratch.display()
        ),
    ] {
        assert_command_denied(&fx, &command);
    }
}

#[test]
fn two_command_bracket_glob_cannot_read_through_scratch_link() {
    let fx = Fixture::new("run-scratch-ln-two");
    let data = fx.tmp.path().join("homes/.transcript-store");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("state.db"), "secret\n").unwrap();
    fx.install_with(WorkerKind::Standard, |config| config.data_dir = Some(data.clone()));
    seed_conversation(&fx);
    let scratch = conversation_dir(&fx).join("scratch");
    let hooks = fx.process_home.join(".gemini/config/hooks.json");
    std::os::unix::fs::symlink(&data, scratch.join("up")).unwrap();
    std::os::unix::fs::symlink(&hooks, scratch.join("hooks.json")).unwrap();
    for command in [
        format!("cat {}/[[:lower:]]p/state.db", scratch.display()),
        format!("cat {}/[[=u=]]p/state.db", scratch.display()),
        format!("cat {}/[[.u.]]p/state.db", scratch.display()),
        format!("cat {}/[!x]p/state.db", scratch.display()),
        format!("cat {}/[[:lower:]]ooks.json", scratch.display()),
        format!("cat $HOME/.gemini/antigravity-cli/brain/{CONVERSATION}/scratch/[[:lower:]]p/state.db"),
        format!("cat ~/.gemini/antigravity-cli/brain/{CONVERSATION}/scratch/[[:lower:]]ooks.json"),
    ] {
        assert_command_denied(&fx, &command);
    }
}

#[test]
fn python_bodies_cannot_open_settings_hooks_or_other_conversations() {
    let fx = Fixture::new("run-python-body-scan");
    fx.install(WorkerKind::Standard);
    seed_conversation(&fx);
    let settings = fx.process_home.join(".gemini/antigravity-cli/settings.json");
    let hooks = fx.process_home.join(".gemini/config/hooks.json");
    let other = brain(&fx).join("c2/scratch/x.py");
    fs::create_dir_all(other.parent().unwrap()).unwrap();
    fs::write(&other, "print(1)\n").unwrap();
    let scratch = conversation_dir(&fx).join("scratch");
    fs::write(scratch.join("lib.rs"), "fn main() {}\n").unwrap();
    fs::create_dir_all(scratch.join("work dir")).unwrap();
    fs::write(scratch.join("work dir/file.rs"), "fn main() {}\n").unwrap();
    for command in [
        format!("python3 -c 'print(open(\"{}\").read())'", settings.display()),
        format!("python3 -c 'print(open(\"{}\").read())'", hooks.display()),
        format!("python3 -c 'print(open(\"{}\").read())'", other.display()),
        format!("python3 - <<'PY'\nprint(open(\"{}\").read())\nPY", settings.display()),
        format!("python3 - <<'PY'\nprint(open(\"{}\").read())\nPY", other.display()),
        "python3 -c 'print(open(\"~/.gemini/antigravity-cli/settings.json\").read())'".to_owned(),
        "python3 -c 'print(open(\"$HOME/.gemini/config/hooks.json\").read())'".to_owned(),
        "python3 -c 'print(open(\"$HOME/.gemini/antigravity-cli/brain/c2/scratch/x.py\").read())'".to_owned(),
        format!(
            "python3 -c 'print(open(\"{}/work dir/../../.system_generated/logs/transcript_full.jsonl\").read())'",
            scratch.display()
        ),
        format!(
            "python3 - <<'PY'\nprint(open(\"{}/work dir/../../.system_generated/logs/transcript_full.jsonl\").read())\nPY",
            scratch.display()
        ),
        format!(
            "python3 -c 'print(open(\"{}/\\x2e\\x2e/.system_generated/logs/transcript_full.jsonl\").read())'",
            scratch.display()
        ),
        format!(
            "python3 -c 'print(open(\"{}/\" \"../../../antigravity-cli/settings.json\").read())'",
            scratch.display()
        ),
        format!(
            "python3 -c 'import os; print(open(os.path.join(\"{}\", \"..\", \"..\", \"c2\", \"scratch\", \"x.py\")).read())'",
            scratch.display()
        ),
        format!(
            "python3 -c 'from pathlib import Path; print(open(Path(\"{}\") / \"..\" / \"..\" / \"c2\" / \"scratch\" / \"x.py\").read())'",
            scratch.display()
        ),
        format!("echo 'print(open(\"{}\").read())' | python3", settings.display()),
        format!(
            "printf '%s' 'print(open(\"{}\").read())' | python3 -",
            settings.display()
        ),
        format!(
            "cat <<'PY' | python3 -\nprint(open(\"{}\").read())\nPY",
            other.display()
        ),
        format!(
            "uv run --python python3.12 python3 -c 'print(open(\"{}\").read())'",
            settings.display()
        ),
        format!("python3.13t -c 'print(open(\"{}\").read())'", settings.display()),
    ] {
        let reason = assert_command_denied(&fx, &command);
        assert!(
            reason.contains("protected") || reason.contains("denied"),
            "{command}: {reason}"
        );
    }
    assert_command_allowed(
        &fx,
        &format!("python3 -c 'print(open(\"{}/lib.rs\").read())'", scratch.display()),
    );
    assert_command_allowed(
        &fx,
        &format!(
            "python3 - <<'PY'\nprint(open(\"{}/lib.rs\").read())\nPY",
            scratch.display()
        ),
    );
    assert_command_allowed(
        &fx,
        &format!(
            "python3 -c 'print(open(\"{}/work dir/file.rs\").read())'",
            scratch.display()
        ),
    );
    assert_command_allowed(
        &fx,
        &format!(
            "python3 - <<'PY'\nprint(open(\"{}/work dir/file.rs\").read())\nPY",
            scratch.display()
        ),
    );
    for command in [
        r#"python3 -c 'print(open(os.path.expanduser("~/\x2egemini/config/hooks.json")).read())'"#.to_owned(),
        r#"python3 -c 'print(open(os.path.expanduser("~/\u002egemini/config/hooks.json")).read())'"#.to_owned(),
        r#"python3 -c 'print(open(os.path.expanduser("~/\056gemini/config/hooks.json")).read())'"#.to_owned(),
        "python3 - <<'PY'\nprint(open(os.path.expanduser(\"~/\\x2egemini/config/hooks.json\")).read())\nPY".to_owned(),
        "python3 - <<'PY'\nprint(open(os.path.expanduser(\"~/\\u002egemini/config/hooks.json\")).read())\nPY"
            .to_owned(),
        "python3 - <<'PY'\nprint(open(os.path.expanduser(\"~/\\056gemini/config/hooks.json\")).read())\nPY".to_owned(),
        format!(
            "cd {} && python3 -c 'print(open(\"../.system_generated/logs/transcript_full.jsonl\").read())'",
            scratch.display()
        ),
        format!(
            "cd {} && python3 - <<'PY'\nprint(open(\"../.system_generated/logs/transcript_full.jsonl\").read())\nPY",
            scratch.display()
        ),
        format!(
            "cd {} && python3 -c 'print(open(\"../../../../../../../state.db\").read())'",
            scratch.display()
        ),
        format!(
            "cd {} && python3 -c 'print(open(\"../../c2/scratch/x.py\").read())'",
            scratch.display()
        ),
    ] {
        let reason = assert_command_denied(&fx, &command);
        assert!(
            reason.contains("protected") || reason.contains("denied"),
            "{command}: {reason}"
        );
    }
    assert_command_allowed(&fx, "cat <<'EOF'\n~/.gemini/config/hooks.json\nsymlink\nEOF");
    assert_command_allowed(&fx, "cat <<<'symlink ~/.gemini/config/hooks.json'");
    assert_command_allowed(
        &fx,
        "python3 -c 'from pathlib import Path; print(Path(\"x\").is_symlink())'",
    );
    assert_command_allowed(&fx, "python3 -c 'import os; os.stat(\"x\", follow_symlinks=False)'");
    assert_command_allowed(
        &fx,
        &format!("grep -rn symlink src && cat {}/lib.rs", scratch.display()),
    );
}

#[test]
fn unquoted_heredoc_apostrophe_does_not_hide_arithmetic() {
    let fx = Fixture::new("run-unquoted-apostrophe");
    fx.install(WorkerKind::Standard);
    let reason = assert_command_denied(&fx, "cat <<EOF\ndon't\nEOF\nx='a''[$(id)]'\n(( x ))");
    assert!(
        reason.contains("cannot be inspected") || reason.contains("denied") || reason.contains("substitution"),
        "{reason}"
    );
}
