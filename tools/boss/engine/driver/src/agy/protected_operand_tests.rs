use super::*;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

fn deny_at(fx: &Fixture, command: &str, cwd: &Path) -> Value {
    let mut payload = run_command_payload(command);
    payload["toolCall"]["args"]["Cwd"] = json!(cwd);
    let (_, body, stderr) = authorize(&fx.process_home, &payload);
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
        let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(command));
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
        &[("BOSS_STRUCTURED_OUTPUT", protected.to_str().unwrap())],
    );
    assert_eq!(decision(&body), "deny", "{body} {stderr}");
    assert!(body["reason"].as_str().unwrap().contains("protected"), "{body}");
}

#[test]
fn value_flags_of_other_programs_do_not_hide_protected_operands() {
    let fx = Fixture::new("run-value-flag-scope");
    fx.install(WorkerKind::Standard);
    let config = fx.process_home.join(".gemini/config");
    let protected = config.join("hooks.json");
    let (protected, config) = (protected.display(), config.display());
    for command in [
        format!("cat -t {protected}"),
        format!("ls -lt {config}"),
        format!("ls -g {protected}"),
        format!("ls -C {protected}"),
        format!("tar -xf a.tar -C {config}"),
        format!("git -C {config} status"),
    ] {
        deny_at(&fx, &command, &fx.workspace());
    }
    for command in ["grep -A 3 pat f", "rg -t rust pat", "grep -m 2 pat f"] {
        let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(command));
        assert_eq!(decision(&body), "auto_approve", "{command}: {body} {stderr}");
    }
}

#[test]
fn heredoc_delimiter_words_and_arithmetic_do_not_hide_live_lines() {
    let fx = Fixture::new("run-heredoc-words");
    fx.install(WorkerKind::Standard);
    let protected = fx.process_home.join(".gemini/config/hooks.json");
    let live = format!("echo \"$(cat {})\"", protected.display());
    for (opener, terminator) in [
        ("cat <<'true'X", "trueX"),
        ("cat <<\"E\"OF", "EOF"),
        ("cat <<E'OF'", "EOF"),
        ("(( y = 1 << 2 ))", "2"),
        ("echo $[1<<2]", "2"),
    ] {
        let command = format!("{opener}\n{terminator}\n{live}\ntrue");
        let body = deny_at(&fx, &command, &fx.workspace());
        assert_eq!(
            body["reason"], "command substitution is structurally denied",
            "{command}"
        );
    }
    let (_, body, stderr) = authorize(
        &fx.process_home,
        &run_command_payload("cat <<'E'OF\n`cat /protected/x`\nEOF"),
    );
    assert_eq!(decision(&body), "auto_approve", "{body} {stderr}");
}

#[test]
fn continuations_do_not_apply_in_comments_or_literal_heredoc_bodies() {
    let fx = Fixture::new("run-continuation-context");
    fx.install(WorkerKind::Standard);
    let protected = fx.process_home.join(".gemini/config/hooks.json");
    let protected = protected.display();
    for command in [
        format!("true # x \\\ncat {protected}"),
        format!("cat <<'true'\nbody \\\ntrue\necho \"$(cat {protected})\"\ntrue"),
    ] {
        deny_at(&fx, &command, &fx.workspace());
    }
}

#[test]
fn redirect_targets_with_unresolved_expansions_are_denied() {
    let fx = Fixture::new("run-redirect-expansion");
    fx.install(WorkerKind::Standard);
    for command in ["T=/x; echo x > \"${T}y\"", "printf x >> \"$T\"", "echo x > ~root/y"] {
        deny_at(&fx, command, &fx.workspace());
    }
    let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload("echo cost$"));
    assert_eq!(decision(&body), "auto_approve", "{body} {stderr}");
}

#[test]
fn operands_after_end_of_options_are_checked_verbatim() {
    let fx = Fixture::new("run-literal-operands");
    fx.install(WorkerKind::Standard);
    let target = fx.process_home.join(".gemini/config/hooks.json");
    for name in ["-falias", "name=alias"] {
        std::os::unix::fs::symlink(&target, fx.workspace().join(name)).unwrap();
    }
    deny_at(&fx, "grep -e . -- -falias", &fx.workspace());
    deny_at(&fx, "cat -- name=alias", &fx.workspace());
}

#[test]
fn exported_path_variables_cannot_be_rebound_then_expanded() {
    let fx = Fixture::new("run-exported-rebind");
    fx.install(WorkerKind::Standard);
    let protected = fx.process_home.join(".gemini/config/boss-agy-auth-policy.json");
    let protected = protected.to_str().unwrap();
    let split = protected.len() / 2;
    let prefix = &protected[..split];
    let suffix = &protected[split..];
    let hooks = fx.process_home.join(".gemini/config/hooks.json");
    let hooks = hooks.to_str().unwrap();
    let hooks_split = hooks.len() / 2;
    let hooks_prefix = &hooks[..hooks_split];
    let hooks_suffix = &hooks[hooks_split..];
    let data = fx.tmp.path().join("boss-data");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("secret"), "x").unwrap();
    let data_str = data.to_str().unwrap();
    let data_split = data_str.len() / 2;
    let data_prefix = &data_str[..data_split];
    let data_suffix = &data_str[data_split..];
    let env = [
        ("BOSS_STRUCTURED_OUTPUT", "/artifacts/out.json"),
        ("BOSS_BIN_DIR", "/opt/boss-tools"),
    ];
    for command in [
        format!("BOSS_STRUCTURED_OUTPUT={prefix}; cat \"${{BOSS_STRUCTURED_OUTPUT}}{suffix}\""),
        format!("BOSS_STRUCTURED_OUTPUT={hooks_prefix}; printf '{{}}' > \"${{BOSS_STRUCTURED_OUTPUT}}{hooks_suffix}\""),
        "read BOSS_STRUCTURED_OUTPUT < f".into(),
        "builtin read BOSS_STRUCTURED_OUTPUT < f".into(),
        format!("builtin export BOSS_STRUCTURED_OUTPUT={prefix}; cat \"${{BOSS_STRUCTURED_OUTPUT}}{suffix}\""),
        format!("builtin command export BOSS_STRUCTURED_OUTPUT={prefix}; cat \"${{BOSS_STRUCTURED_OUTPUT}}{suffix}\""),
        format!(
            "command builtin command export BOSS_STRUCTURED_OUTPUT={prefix}; cat \"${{BOSS_STRUCTURED_OUTPUT}}{suffix}\""
        ),
        format!("BOSS_STRUCTURED_OUTPUT[0]={prefix}; cat \"${{BOSS_STRUCTURED_OUTPUT}}{suffix}\""),
        format!("BOSS_BIN_DIR+={data_prefix}; cat \"$BOSS_BIN_DIR{data_suffix}/secret\""),
        "export BOSS_STRUCTURED_OUTPUT=/tmp/agy-ho".into(),
        "printf -v BOSS_STRUCTURED_OUTPUT %s /tmp/agy-ho".into(),
        format!("BOSS_BIN_DIR={data_prefix}; cat \"$BOSS_BIN_DIR{data_suffix}/secret\""),
    ] {
        let (_, body, stderr) = authorize_with_env(&fx.process_home, &run_command_payload(&command), &env);
        assert_eq!(decision(&body), "deny", "{command}: {body} {stderr}");
    }
}

#[test]
fn unquoted_grep_pattern_globs_and_expansions_are_denied() {
    let fx = Fixture::new("run-grep-pattern-slots");
    fx.install(WorkerKind::Standard);
    let config = fx.process_home.join(".gemini/config");
    let config = config.display();
    for command in ["grep $X f".into(), "grep -e $X f".into(), format!("grep -v {config}/*")] {
        deny_at(&fx, &command, &fx.workspace());
    }
    for command in ["grep \"$X\" f", "grep -n 'x$' f"] {
        let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(command));
        assert_eq!(decision(&body), "auto_approve", "{command}: {body} {stderr}");
    }
}

#[test]
fn redirect_targets_matching_argv_words_keep_redirect_role() {
    let fx = Fixture::new("run-redirect-same-word");
    fx.install(WorkerKind::Standard);
    for command in [
        r#"echo "$P" > "$P""#,
        r#"printf %s "$P" > "$P""#,
        r#"echo -- "$P" > "$P""#,
    ] {
        deny_at(&fx, command, &fx.workspace());
    }
}

#[test]
fn ansi_c_quoting_cannot_hide_a_later_live_line() {
    let fx = Fixture::new("run-ansi-c-heredoc");
    fx.install(WorkerKind::Standard);
    let protected = fx.process_home.join(".gemini/config/hooks.json");
    let command = format!("echo $'\\' <<X #'\ncat {}\nX", protected.display());
    deny_at(&fx, &command, &fx.workspace());
}

#[test]
fn ansi_c_escaped_quote_cannot_hide_a_live_command_or_substitution() {
    let fx = Fixture::new("run-ansi-c-escaped-quote");
    fx.install(WorkerKind::Standard);
    let protected = fx.process_home.join(".gemini/config/hooks.json");
    deny_at(
        &fx,
        &format!("echo $'\\'' ; cat {} ; echo \\'", protected.display()),
        &fx.workspace(),
    );
    deny_at(&fx, "echo $'\\'' $(jj describe -m x) \\'", &fx.workspace());
}

#[test]
fn multi_word_expansions_option_bundles_and_braces_are_denied() {
    let fx = Fixture::new("run-multi-word-expansions");
    fx.install(WorkerKind::Standard);
    let config = fx.process_home.join(".gemini/config");
    let hooks = config.join("hooks.json");
    let hooks = hooks.display();
    for command in [
        "grep \"$@\" f".to_string(),
        "grep -e \"${a[@]}\" f".to_string(),
        "set -- x /tmp/y; grep \"$@mes/z\"".to_string(),
        "grep -v$X f".to_string(),
        "grep -e . -A$X f".to_string(),
        "grep -${X}e f".to_string(),
        "grep -${X}A f".to_string(),
        format!("grep {{x,{hooks}}}"),
        format!("cat {{{hooks},}}"),
    ] {
        deny_at(&fx, &command, &fx.workspace());
    }
    for command in ["grep \"$X\" f", "grep -n 'x$' f"] {
        let (_, body, stderr) = authorize(&fx.process_home, &run_command_payload(command));
        assert_eq!(decision(&body), "auto_approve", "{command}: {body} {stderr}");
    }
}

#[test]
fn symlink_parent_glob_is_denied_after_physical_dotdot() {
    let fx = Fixture::new("run-symlink-dotdot-glob");
    fx.install(WorkerKind::Standard);
    let sibling = fx.tmp.path().join("other");
    fs::create_dir_all(&sibling).unwrap();
    std::os::unix::fs::symlink(&sibling, fx.workspace().join("link")).unwrap();
    let leaf = format!("{}/process-home/.gemini/config/hooks.json", fx.run_id);
    let body = deny_at(&fx, &format!("cat link/../hom?s/{leaf}"), &fx.workspace());
    assert!(body["reason"].as_str().unwrap().contains("protected"), "{body}");
}

#[test]
fn builtin_and_trap_cannot_hide_eval_source_cd_or_export() {
    {
        let fx = Fixture::new("run-builtin-wrappers");
        fx.install(WorkerKind::Standard);
        let protected = fx.process_home.join(".gemini/config/hooks.json");
        let protected = protected.display();
        let relative = format!("homes/{}/process-home/.gemini/config/hooks.json", fx.run_id);
        for command in [
            format!("builtin eval 'cat {protected}'"),
            "builtin source ./f".into(),
            "builtin . ./f".into(),
            format!("builtin cd {}; cat {relative}", fx.tmp.path().display()),
            format!("trap 'cat {protected}' EXIT"),
            "trap 'rm -rf ~' EXIT".into(),
        ] {
            deny_at(&fx, &command, &fx.workspace());
        }
    }
    let reviewer = Fixture::new("run-builtin-reviewer");
    reviewer.install_with(WorkerKind::Reviewer, |c| {
        c.is_standard_worker = false;
        c.is_reviewer = true;
    });
    deny_at(&reviewer, "builtin eval 'jj describe -m x'", &reviewer.workspace());
}

#[test]
fn option_bundle_expansions_before_a_recognized_flag_are_denied() {
    let fx = Fixture::new("run-option-bundle-prefix");
    fx.install(WorkerKind::Standard);
    let hooks = fx.process_home.join(".gemini/config/hooks.json");
    fs::write(
        fx.workspace().join("input"),
        format!("v NEVERMATCH {} junk", hooks.display()),
    )
    .unwrap();
    for command in [
        "grep -${X}e f".to_string(),
        "grep -${X}A f".to_string(),
        "read X < input; grep -${X}e f".to_string(),
        "read X < input; grep -${X}A f".to_string(),
    ] {
        deny_at(&fx, &command, &fx.workspace());
    }
}

#[test]
fn arithmetic_contexts_cannot_rebind_trusted_path_variables() {
    let fx = Fixture::new("run-arithmetic-rebind");
    fx.install(WorkerKind::Standard);
    let relative = format!("homes/{}/process-home/.gemini/config/hooks.json", fx.run_id);
    let env = [
        ("BOSS_STRUCTURED_OUTPUT", "/artifacts/out.json"),
        ("BOSS_BIN_DIR", "/opt/boss-tools"),
    ];
    for command in [
        format!(r#"(( BOSS_STRUCTURED_OUTPUT = 0 )); cat "${{BOSS_STRUCTURED_OUTPUT}}/../T/{relative}""#),
        format!(r#"$[BOSS_STRUCTURED_OUTPUT=0]; cat "${{BOSS_STRUCTURED_OUTPUT}}/../T/{relative}""#),
        format!(r#"echo "${{a[BOSS_STRUCTURED_OUTPUT=0]}}"; cat "${{BOSS_STRUCTURED_OUTPUT}}/../T/{relative}""#),
        format!(r#"[[ 1 -eq BOSS_STRUCTURED_OUTPUT=0 ]]; cat "${{BOSS_STRUCTURED_OUTPUT}}/../T/{relative}""#),
        format!(r#"(( BOSS_STRUCTURED_OUTPUT = 0 )); echo x > "${{BOSS_STRUCTURED_OUTPUT}}/../T/{relative}""#),
        format!(r#"(( HOME = 0 )); cat "${{HOME}}/../T/{relative}""#),
        format!(r#"(( BOSS_BIN_DIR = 0 )); cat "${{BOSS_BIN_DIR}}/../T/{relative}""#),
    ] {
        let (_, body, stderr) = authorize_with_env(&fx.process_home, &run_command_payload(&command), &env);
        assert_eq!(decision(&body), "deny", "{command}: {body} {stderr}");
    }
}

#[test]
fn attached_file_flag_values_behind_a_trusted_expansion_are_checked() {
    for kind in [WorkerKind::Standard, WorkerKind::Reviewer] {
        let fx = Fixture::new("run-attached-dynamic-flag");
        fx.install_with(kind, |c| {
            c.is_standard_worker = kind == WorkerKind::Standard;
            c.is_reviewer = kind == WorkerKind::Reviewer;
        });
        let home = fx.process_home.display().to_string();
        let env = [("HOME", home.as_str())];
        for command in [
            "grep -f$HOME/.gemini/config/hooks.json f",
            "grep -vf$HOME/.gemini/config/hooks.json f",
            "grep -f${HOME}/.gemini/config/hooks.json f",
        ] {
            let (_, body, stderr) = authorize_with_env(&fx.process_home, &run_command_payload(command), &env);
            assert_eq!(decision(&body), "deny", "{kind:?} {command}: {body} {stderr}");
        }
    }
}

#[test]
fn split_or_composed_trusted_names_cannot_rebind_in_arithmetic() {
    let fx = Fixture::new("run-composed-rebind");
    fx.install(WorkerKind::Standard);
    let relative = format!("homes/{}/process-home/.gemini/config/hooks.json", fx.run_id);
    let env = [("BOSS_STRUCTURED_OUTPUT", "/artifacts/out.json")];
    let read = format!(r#"cat "${{BOSS_STRUCTURED_OUTPUT}}/../T/{relative}""#);
    for command in [
        format!("(( BOSS_STRUC\\\nTURED_OUTPUT = 0 )); {read}"),
        format!(r#"n=BOSS_STRUCTURED; echo "${{a[${{n}}_OUTPUT=0]}}"; {read}"#),
        format!(r#"n=BOSS_STRUCTURED; echo $[${{n}}_OUTPUT=0]; {read}"#),
        format!(r#"n=BOSS_STRUCTURED; printf '%s' "${{a[${{n}}_OUTPUT=0]}}"; {read}"#),
        format!(
            r#"n=BOSS_STRUCTURED; echo $[${{n}}_OUTPUT=0]; echo x > "${{BOSS_STRUCTURED_OUTPUT}}/../T/{relative}""#
        ),
    ] {
        let (_, body, stderr) = authorize_with_env(&fx.process_home, &run_command_payload(&command), &env);
        assert_eq!(decision(&body), "deny", "{command}: {body} {stderr}");
    }
}

#[test]
fn mention_of_a_trusted_name_outside_arithmetic_is_not_a_rebinding() {
    let fx = Fixture::new("run-mention-scope");
    fx.install(WorkerKind::Standard);
    let env = [("BOSS_BIN", "/opt/boss/boss")];
    for command in [
        r#""$BOSS_BIN" propose done --summary "read HOME before exporting""#,
        r#"cat "$BOSS_BIN" ; rg BOSS_BIN_DIR ."#,
    ] {
        let (_, body, stderr) = authorize_with_env(&fx.process_home, &run_command_payload(command), &env);
        assert_ne!(decision(&body), "deny", "{command}: {body} {stderr}");
    }
}

fn install_reviewer(run_id: &str) -> Fixture {
    let fx = Fixture::new(run_id);
    fx.install_with(WorkerKind::Reviewer, |c| {
        c.is_standard_worker = false;
        c.is_reviewer = true;
    });
    fx
}

#[test]
fn reviewer_denies_trusted_names_composed_across_quote_or_escape() {
    let fx = install_reviewer("run-quote-boundary");
    let env = [("BOSS_STRUCTURED_OUTPUT", "/artifacts/out.json")];
    let relative = format!("homes/{}/process-home/.gemini/config/hooks.json", fx.run_id);
    for command in [
        format!(r#"BOSS_STRUCTURED_OUTPU=/tmp/; cat "$BOSS_STRUCTURED_OUTPU"T/{relative}"#),
        format!(r#"BOSS_STRUCTURED_OUTPU=/tmp/; cat "$BOSS_STRUCTURED_OUTPU""T"/{relative}"#),
        format!(r#"BOSS_STRUCTURED_OUTPU=/tmp/; cat $BOSS_STRUCTURED_OUTPU\T/{relative}"#),
        "cat $'BOSS_STRUCTURED_OUTPUT'/x".into(),
    ] {
        let (_, body, stderr) = authorize_with_env(&fx.process_home, &run_command_payload(&command), &env);
        assert_eq!(decision(&body), "deny", "{command}: {body} {stderr}");
    }
}

#[test]
fn reviewer_denies_env_exec_substring_subscript_and_wait_rebinding() {
    let fx = install_reviewer("run-rebind-forms");
    let env = [("BOSS_STRUCTURED_OUTPUT", "/artifacts/out.json")];
    let relative = format!("homes/{}/process-home/.gemini/config/hooks.json", fx.run_id);
    let read = format!(r#"cat "${{BOSS_STRUCTURED_OUTPUT}}/../T/{relative}""#);
    for command in [
        format!(r#"env -u BOSS_STRUCTURED_OUTPUT bash -c '{read}'"#),
        format!(r#"env --unset=BOSS_STRUCTURED_OUTPUT bash -c '{read}'"#),
        format!(r#"env -i bash -c '{read}'"#),
        format!(r#"exec -c bash -c '{read}'"#),
        format!(r#"x=a; echo ${{x:BOSS_STRUCTURED_OUTPUT=0}}; {read}"#),
        format!(r#"a[BOSS_STRUCTURED_OUTPUT=0]=v; {read}"#),
        format!(r#"coproc BOSS_STRUCTURED_OUTPUT (echo 1); {read}"#),
        format!(r#"wait -p BOSS_STRUCTURED_OUTPUT; {read}"#),
    ] {
        let (_, body, stderr) = authorize_with_env(&fx.process_home, &run_command_payload(&command), &env);
        assert_eq!(decision(&body), "deny", "{command}: {body} {stderr}");
    }
}

#[test]
fn reviewer_denies_alias_mapfile_and_compgen_callbacks() {
    let fx = install_reviewer("run-dynamic-exec");
    let protected = fx.process_home.join(".gemini/config/hooks.json");
    let protected = protected.display();
    fs::write(fx.workspace().join("README.md"), "x").unwrap();
    for command in [
        format!("shopt -s expand_aliases\nalias c='cat {protected}'\nc"),
        "shopt -s expand_aliases\nalias j='jj describe -m x'\nj".into(),
        format!("mapfile -C 'cat {protected}' -c 1 a < README.md"),
        format!("readarray -C 'cat {protected}' -c 1 a < README.md"),
        format!("compgen -C 'cat {protected}' x"),
    ] {
        deny_at(&fx, &command, &fx.workspace());
    }
}
