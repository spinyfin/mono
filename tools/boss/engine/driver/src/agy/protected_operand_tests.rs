use super::*;
use std::io::Write;
use std::process::{Command, Stdio};

fn deny_at(fx: &Fixture, command: &str, cwd: &Path) -> Value {
    let mut payload = run_command_payload(command);
    payload["toolCall"]["args"]["Cwd"] = json!(cwd);
    let (_, body, stderr) = authorize(&fx.process_home, &payload, true);
    assert_eq!(decision(&body), "deny", "{command}: {body} {stderr}");
    body
}

#[test]
fn protected_cwds_bare_operands_and_directory_changes_are_denied() {
    for kind in [
        WorkerKind::Standard,
        WorkerKind::Reviewer,
        WorkerKind::Triage,
        WorkerKind::AnswerAgent,
        WorkerKind::ReviewGuide,
        WorkerKind::Progression,
    ] {
        let fx = Fixture::new("run-protected-operands");
        fx.install(kind);
        let config = fx.process_home.join(".gemini/config");
        for command in ["cat hooks.json", "printf x > hooks.json", "ls"] {
            deny_at(&fx, command, &config);
        }
        let control = fx.process_home.parent().unwrap().join("control");
        deny_at(&fx, "cat initial-prompt.txt", &control);
        for directory in [&config, &control] {
            deny_at(&fx, &format!("cd {} && ls", directory.display()), &fx.workspace());
            deny_at(&fx, &format!("pushd {} && ls", directory.display()), &fx.workspace());
        }
        std::os::unix::fs::symlink(config.join("hooks.json"), fx.workspace().join("alias")).unwrap();
        deny_at(&fx, "cat alias", &fx.workspace());
        deny_at(&fx, "printf x > alias", &fx.workspace());
    }
}

#[test]
fn normalized_relative_parent_and_tilde_globs_are_denied() {
    let fx = Fixture::new("run-normalized-globs");
    fx.install(WorkerKind::Standard);
    let tmp = fx.tmp.path().display();
    let leaf = format!("{}/process-home/.gemini/config/hooks.json", fx.run_id);
    for (command, cwd) in [
        (format!("cat hom?s/{leaf}"), fx.tmp.path().to_path_buf()),
        (format!("cat ../hom?s/{leaf}"), fx.workspace()),
        (format!("cat {tmp}/ws/../hom?s/{leaf}"), fx.workspace()),
        (format!("cat ~/../../../hom?s/{leaf}"), fx.workspace()),
    ] {
        let body = deny_at(&fx, &command, &cwd);
        assert!(body["reason"].as_str().unwrap().contains("protected"), "{body}");
    }
}

#[test]
fn identical_heredoc_text_does_not_mask_a_live_substitution() {
    let fx = Fixture::new("run-heredoc-offsets");
    fx.install(WorkerKind::Standard);
    let command = "echo \"'`cat /protected/x`\n\"\ncat <<'EOF'\n'`cat /protected/x`\nEOF";
    let body = deny_at(&fx, command, &fx.workspace());
    assert_eq!(body["reason"], "command substitution is structurally denied");
}

fn tokenizer_records(command: &str) -> Value {
    let script = format!(
        "import re,shlex,os,json,sys\n{}\nprint(json.dumps([list(g) for g in command_records(sys.stdin.read()) if list(g)]))\n",
        crate::guard_python::CODEX_COMMAND_TOKENIZER_PY
    );
    let mut child = Command::new("python3")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 is required for tokenizer tests");
    child.stdin.take().unwrap().write_all(command.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "tokenizer crashed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn tokenizer_keeps_quoted_punctuation_nbsp_and_escaped_parens() {
    for command in ["echo '()'", r"echo \(\)", "echo foo\u{00a0}bar"] {
        let groups = tokenizer_records(command);
        assert!(
            groups.as_array().unwrap().iter().any(|g| g[0] == "echo"),
            "{command}: {groups}"
        );
    }
}

#[test]
fn attached_grep_options_end_of_options_and_xargs_are_denied() {
    let fx = Fixture::new("run-operand-classifier");
    fx.install(WorkerKind::Standard);
    let protected = fx.process_home.join(".gemini/config/hooks.json");
    let protected = protected.display();
    std::os::unix::fs::symlink(
        fx.process_home.join(".gemini/config/hooks.json"),
        fx.workspace().join("-alias"),
    )
    .unwrap();
    for command in [
        format!("grep -e. {protected}"),
        format!("rg -e. {protected}"),
        format!("grep --file={protected} f"),
        format!("grep -fX {protected}"),
        "cat -- -alias".into(),
        format!("echo {protected} | xargs cat"),
    ] {
        deny_at(&fx, &command, &fx.workspace());
    }
}

#[test]
fn quoted_text_is_not_a_heredoc_and_continuations_keep_span() {
    let fx = Fixture::new("run-heredoc-lex");
    fx.install(WorkerKind::Standard);
    let protected = fx.process_home.join(".gemini/config/hooks.json");
    let attack = format!("echo \"<<'true'\"\necho \"$(cat {})\"\ntrue", protected.display());
    let body = deny_at(&fx, &attack, &fx.workspace());
    assert_eq!(body["reason"], "command substitution is structurally denied");
    let continuation = format!(
        "echo foo\\\\\n\ncat <<'EOF'\nbody\nEOF\necho `cat {}`",
        protected.display()
    );
    let body = deny_at(&fx, &continuation, &fx.workspace());
    assert_eq!(body["reason"], "command substitution is structurally denied");
    let (_, body, stderr) = authorize(
        &fx.process_home,
        &run_command_payload("cat <<'EOF'\n`cat /protected/x`\nEOF"),
        true,
    );
    assert_eq!(decision(&body), "auto_approve", "{body} {stderr}");
}

#[test]
fn quote_segments_preserve_literal_dollars_in_path_operands() {
    let fx = Fixture::new("run-quote-segments");
    fx.install(WorkerKind::Standard);
    for command in [
        "cat file'$'",
        "cat file\\$",
        "cat \"file\\$\"",
        "cat '~user'",
        "grep -n x'$' f",
        "printf '%s' a$",
        "echo cost$",
        "echo '()'",
        r"echo \(\)",
        "echo foo\u{00a0}bar",
    ] {
        let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(command), true);
        assert_eq!(decision(&body), "auto_approve", "{command}: {body} {stderr}");
    }
    deny_at(&fx, "cat \"$HOME_CONFIG\"", &fx.workspace());
    deny_at(&fx, "cat file'$'\"$HOME_CONFIG\"", &fx.workspace());
}

#[test]
fn guards_receive_the_workers_exact_environment() {
    let fx = Fixture::new("run-exact-env");
    write_exec(
        &fx.path_guard(),
        "#!/usr/bin/env python3\nimport json,os\nexpected={'BOSS_BIN':'/workspace/bin/boss','CUBE_BIN':'/workspace/bin/cube','BOSS_BIN_DIR':'/bundle','BOSS_STRUCTURED_OUTPUT':'/artifacts/actual.json'}\nassert all(os.environ.get(k)==v for k,v in expected.items())\nprint(json.dumps({'decision':'approve'}))\n",
    );
    fx.install(WorkerKind::Standard);
    let (_, body, stderr) = authorize_with_env(
        &fx.process_home,
        &run_command_payload("\"$BOSS_BIN\" pr status"),
        true,
        &[
            ("BOSS_BIN", "/workspace/bin/boss"),
            ("CUBE_BIN", "/workspace/bin/cube"),
            ("BOSS_BIN_DIR", "/bundle"),
            ("BOSS_STRUCTURED_OUTPUT", "/artifacts/actual.json"),
        ],
    );
    assert_eq!(decision(&body), "auto_approve", "{body} {stderr}");
    let protected = fx.process_home.join(".gemini/config/hooks.json");
    let (_, body, stderr) = authorize_with_env(
        &fx.process_home,
        &run_command_payload("cat \"$BOSS_STRUCTURED_OUTPUT\""),
        true,
        &[("BOSS_STRUCTURED_OUTPUT", protected.to_str().unwrap())],
    );
    assert_eq!(decision(&body), "deny", "{body} {stderr}");
    assert!(body["reason"].as_str().unwrap().contains("protected"), "{body}");
}
