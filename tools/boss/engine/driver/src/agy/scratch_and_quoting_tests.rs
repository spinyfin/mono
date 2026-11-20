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
fn own_conversation_scratch_is_usable_by_every_kind_for_reading() {
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
        assert_ne!(decision(&body), "deny", "{kind:?}: {body} {stderr}");
        let (read, detail) = verdict(
            &fx,
            &payload_for(Some(CONVERSATION), "read_file", json!({"Path": spill})),
        );
        assert_ne!(read, "deny", "{kind:?} read_file: {detail}");
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
    let scratch = conversation_dir(&fx).join("scratch");
    let scratch = scratch.display().to_string();
    let body = python_body(&scratch);

    for command in [
        // A quoted heredoc in the Python body shape from the report.
        format!("python3 - <<'PY'\n{body}PY"),
        format!("python3 - <<'PY'\n{body}PY\necho done"),
        format!("python3 - 2>/dev/null <<'PY'\n{body}PY"),
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
