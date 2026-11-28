use super::*;
use crate::{ProgressObservationConfig, ToolUseInterceptionConfig};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

struct Fixture {
    dir: tempfile::TempDir,
    command: String,
    run_id: String,
}

impl Fixture {
    fn new(reviewer: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let run_id = format!("grok-reviewer-{}", std::process::id());
        let obs = ProgressObservationConfig {
            events_socket_path: dir.path().join("events.sock"),
            lease_id: "lease".into(),
            run_id: run_id.clone(),
            workspace_path: dir.path().to_owned(),
            forwarder_binary: dir.path().join("boss-event"),
        };
        let config = ToolUseInterceptionConfig {
            data_dir: None,
            path_guard_script: None,
            checkleft_guard_script: None,
            git_store_guard_script: None,
            is_revision: false,
            is_standard_worker: !reviewer,
            is_reviewer: reviewer,
            is_review_guide: false,
            run_id: Some(run_id.clone()),
            workspace_path: Some(dir.path().to_owned()),
        };
        let files = super::super::hooks::write_hooks(dir.path(), &obs, &config, false).unwrap();
        let hooks: Value = serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
        let entries = hooks["hooks"]["PreToolUse"].as_array().unwrap();
        let reviewer_hook = entries.iter().find(|entry| {
            entry["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("boss-grok-reviewer-guard.py")
        });
        assert_eq!(reviewer_hook.is_some(), reviewer);
        let command = reviewer_hook
            .map(|entry| {
                assert_eq!(entry["matcher"], "*", "native writes must not miss a Bash-only matcher");
                entry["hooks"][0]["command"].as_str().unwrap().to_owned()
            })
            .unwrap_or_default();
        Self { dir, command, run_id }
    }

    fn decide(&self, tool: &str, input: Value) -> String {
        self.payload(&json!({
            "hookEventName": "pre_tool_use",
            "permissionMode": "bypassPermissions",
            "toolName": tool,
            "toolInput": input,
            "cwd": self.dir.path(),
        }))
    }

    fn payload(&self, payload: &Value) -> String {
        let mut child = Command::new("/bin/sh")
            .args(["-c", &self.command])
            .env("GROK_HOOK_EVENT", "pre_tool_use")
            .env("GROK_HOOK_NAME", "reviewer")
            .env("GROK_SESSION_ID", "session")
            .env("GROK_WORKSPACE_ROOT", self.dir.path())
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
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        result["decision"].as_str().unwrap().into()
    }

    fn bash(&self, command: &str) -> String {
        self.decide("run_terminal_command", json!({"command": command}))
    }
}

#[test]
fn reviewer_hook_denies_native_and_unknown_write_routes() {
    let f = Fixture::new(true);
    for tool in [
        "write",
        "search_replace",
        "apply_patch",
        "notebook_edit",
        "mcp__write",
        "spawn_subagent",
        "run_new_tool",
        "send_input",
        "manage_task",
    ] {
        assert_eq!(
            f.decide(tool, json!({"file_path": "source.rs", "content": "changed"})),
            "deny",
            "{tool}"
        );
    }
    for command in [
        "printf changed > source.rs",
        "echo changed >> source.rs",
        "touch source.rs",
        "cp old source.rs",
        "mv old source.rs",
        "rm source.rs",
        "tee source.rs",
        "sed -i 's/a/b/' source.rs",
        "sed -n '1w source.rs' old",
        "sed -n '1p' -e 'w source.rs' old",
        "awk 'BEGIN {print 1 > \"source.rs\"}'",
        "python3 -c 'open(\"source.rs\",\"w\").write(\"changed\")'",
        "sh -c 'echo changed > source.rs'",
        "./write-source",
        "find . -exec touch source.rs ';'",
        "cat old; touch source.rs",
        "cat $(touch source.rs)",
        "cat `touch source.rs`",
        "cat <(touch source.rs)",
        "cat <<EOF\n$(touch source.rs)\nEOF",
        "rg --pre ./writer pattern .",
        "rg --no-config --pre ./writer pattern .",
        "git --no-pager diff --no-ext-diff --no-textconv --output source.rs",
        "git --no-pager show --textconv HEAD:source.rs",
        "git --no-pager show :/needle",
        "git --no-pager show --no-ext-diff --no-textconv HEAD",
        "GUIDE_LITERAL_0 'cat' source.rs",
        "jj diff",
        "jj diff --git -- --ignore-working-copy --no-pager",
        "jj --ignore-working-copy --no-pager diff --tool ./writer",
    ] {
        assert_eq!(f.bash(command), "deny", "{command}");
    }
}

#[test]
fn reviewer_hook_denies_publishing_including_wrappers_and_flag_forms() {
    let f = Fixture::new(true);
    let mut commands = vec![
        "git push".to_owned(),
        "git -C /repo push".into(),
        "jj git push".into(),
        "jj -R /repo git push".into(),
        "cube pr create".into(),
        "cube pr update".into(),
        "cube pr arbitrary-verb".into(),
        "gh api -X POST repos/o/r/issues".into(),
        "\"$CUBE_BIN\" pr update".into(),
        "gh --repo o/r pr edit".into(),
    ];
    for verb in [
        "create", "merge", "close", "edit", "comment", "review", "reopen", "ready",
    ] {
        commands.push(format!("gh pr {verb}"));
    }
    for verb in ["create", "comment", "close", "edit", "reopen", "delete"] {
        commands.push(format!("gh issue {verb}"));
    }
    for command in commands {
        for prefix in ["", "env ", "sudo ", "timeout 60 ", "nohup ", "cat source.rs && "] {
            let command = format!("{prefix}{command}");
            assert_eq!(f.bash(&command), "deny", "{command}");
        }
    }
}

#[test]
fn reviewer_retains_source_and_pull_request_inspection() {
    let f = Fixture::new(true);
    for tool in [
        "read_file",
        "grep",
        "list_dir",
        "glob",
        "get_command_or_subagent_output",
    ] {
        assert_eq!(f.decide(tool, json!({"target_file": "source.rs"})), "allow", "{tool}");
    }
    for command in [
        "cat source.rs",
        "head -n 40 source.rs",
        "tail -n 20 source.rs",
        "nl -ba source.rs",
        "wc -l source.rs",
        "sed -n '1,30p;50,60p' source.rs",
        "rg --no-config -n 'unsafe|unwrap' .",
        "rg --no-config --files .",
        "cat source.rs | head -n 10",
        "git --no-pager show aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:source.rs",
        "git --no-pager diff --no-ext-diff --no-textconv aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "git --no-pager --git-dir /store/.git show aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:source.rs",
        "git --no-pager log --no-patch --no-show-signature --oneline -n 10",
        "jj --ignore-working-copy --no-pager diff --git -r @",
        "jj --ignore-working-copy --no-pager show --git -r @",
        "jj --ignore-working-copy --no-pager log -n 10",
        "jj --ignore-working-copy --no-pager file show -r @ source.rs",
        "jj --ignore-working-copy --no-pager status",
        "gh -R owner/repo pr view 1 --json title,body",
        "gh pr diff 1",
        "gh pr checks 1",
        "gh issue view 1",
    ] {
        assert_eq!(f.bash(command), "allow", "{command}");
    }
}

#[test]
fn only_this_runs_native_review_result_write_is_allowed() {
    let f = Fixture::new(true);
    let artifact = path_for(&default_dir(), &f.run_id, StructuredOutputKind::ReviewResult);
    let data = json!({"file_path": artifact, "content": "{}"});
    assert_eq!(f.decide("write", data.clone()), "allow");
    assert_eq!(f.decide("search_replace", data), "deny");
    for path in [
        artifact.with_extension("other.json"),
        f.dir.path().join("review-result.json"),
    ] {
        assert_eq!(f.decide("write", json!({"file_path": path, "content": "{}"})), "deny");
    }
    assert_eq!(f.bash(&format!("printf '{{}}' > {}", artifact.display())), "deny");
}

#[test]
fn malformed_or_truncated_calls_fail_closed() {
    let f = Fixture::new(true);
    for payload in [
        json!(null),
        json!({}),
        json!({"toolName": "run_terminal_command", "toolInput": {}}),
        json!({"toolName": "write", "toolInput": null}),
        json!({"toolName": "read_file", "toolInput": {}, "toolInputTruncated": true}),
    ] {
        assert_eq!(f.payload(&payload), "deny", "{payload}");
    }
    assert!(script(Path::new("relative"), "run").is_err());
}

#[test]
fn standard_workers_do_not_get_the_reviewer_fence() {
    Fixture::new(false);
}

#[cfg(unix)]
#[test]
fn review_result_exception_rejects_symlinks_and_hardlinks() {
    let f = Fixture::new(true);
    let artifact = f.dir.path().join("result.json");
    let source = f.dir.path().join("source.rs");
    std::fs::write(&source, "original").unwrap();
    // Keep this adversarial filesystem fixture entirely inside the test's
    // private directory while exercising the same generated artifact check.
    let production_artifact = path_for(&default_dir(), &f.run_id, StructuredOutputKind::ReviewResult);
    let guard = f.dir.path().join("hooks/boss-grok-reviewer-guard.py");
    let body = std::fs::read_to_string(&guard).unwrap().replace(
        &serde_json::to_string(&production_artifact).unwrap(),
        &serde_json::to_string(&artifact).unwrap(),
    );
    std::fs::write(guard, body).unwrap();
    let data = json!({"file_path": artifact, "content": "changed"});
    std::os::unix::fs::symlink(&source, &artifact).unwrap();
    assert_eq!(f.decide("write", data.clone()), "deny");
    std::fs::remove_file(&artifact).unwrap();
    std::fs::hard_link(&source, &artifact).unwrap();
    assert_eq!(f.decide("write", data), "deny");
    assert_eq!(std::fs::read_to_string(source).unwrap(), "original");
}

#[test]
fn rendered_batch_submission_is_the_only_permitted_proposal() {
    use boss_pr_review::*;
    let f = Fixture::new(true);
    let artifact = path_for(&default_dir(), &f.run_id, StructuredOutputKind::ReviewResult);
    let destination = ReviewerReportDestination::builder()
        .batch_id("batch-review")
        .pr_url("https://github.com/owner/repo/pull/1")
        .target_sha("a".repeat(40))
        .phase(boss_protocol::ReviewBatchPhase::PreMerge)
        .body_path(artifact.to_str().unwrap())
        .build();
    let prompt = render_batch_reviewer_initial_prompt(
        &ReviewBriefPacket::builder().task_name("Review guard").build(),
        &destination,
        ReviewScope::Code,
        None,
        "owner/repo",
    );
    let command = prompt
        .lines()
        .find(|line| line.starts_with("\"$BOSS_BIN\" propose review-report "))
        .unwrap();
    assert_eq!(f.bash(command), "allow", "{command}");
    for bad in [
        command.replace("review-report", "done"),
        command.replace("review-report", "review-guide"),
        command.replace(artifact.to_str().unwrap(), "/tmp/other.json"),
        format!("{command} --extra"),
        format!("{command} && touch source.rs"),
        format!("{command} > source.rs"),
        format!("{command}\ncat source.rs"),
        command.replace("batch-review", "$(touch source.rs)"),
    ] {
        assert_eq!(f.bash(&bad), "deny", "{bad}");
    }
}

#[test]
fn rendered_inspection_commands_are_permitted() {
    use boss_pr_review::*;
    let f = Fixture::new(true);
    let workspace = f.dir.path().to_str().unwrap();
    let guide = render_reviewer_claude_md("lease", workspace, "", "");
    for command in [
        format!("jj log -R {workspace} --ignore-working-copy --no-pager"),
        format!("jj show -R {workspace} --ignore-working-copy --no-pager --git"),
        format!("jj diff -R {workspace} --ignore-working-copy --no-pager --git"),
    ] {
        assert!(guide.contains(&command));
        assert_eq!(f.bash(&command), "allow", "{command}");
    }
    let prompt = render_reviewer_initial_prompt(
        &ReviewBriefPacket::builder().task_name("Review guard").build(),
        "https://github.com/owner/repo/pull/1",
        "/tmp/result.json",
        ReviewScope::Code,
        None,
        "owner/repo",
    );
    for command in [
        "gh pr diff https://github.com/owner/repo/pull/1 --repo owner/repo",
        "gh pr view https://github.com/owner/repo/pull/1 --repo owner/repo",
    ] {
        assert!(prompt.contains(command));
        assert_eq!(f.bash(command), "allow", "{command}");
        assert_eq!(f.bash(&command.replace("--repo", "-R")), "allow");
    }
    for command in [
        format!("jj diff -R {workspace} --no-pager --git"),
        format!("jj show -R {workspace} --ignore-working-copy --no-pager --tool writer"),
        "gh pr edit 1 --repo owner/repo".into(),
    ] {
        assert_eq!(f.bash(&command), "deny", "{command}");
    }
}

#[test]
fn git_object_inspection_cannot_execute_clean_or_process_filters() {
    let f = Fixture::new(true);
    let repo = f.dir.path();
    let git = |args: &[&str]| {
        let output = Command::new("git").arg("-C").arg(repo).args(args).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init"]);
    git(&["config", "user.name", "Reviewer fixture"]);
    git(&["config", "user.email", "reviewer@example.invalid"]);
    std::fs::write(repo.join(".gitattributes"), "source.rs filter=sentinel\n").unwrap();
    std::fs::write(repo.join("source.rs"), "original\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-m", "Create filter fixture"]);
    let sha = git(&["rev-parse", "HEAD"]);
    // A filter has an observable side effect before attempting any protocol.
    // The process variant deliberately fails, but Git still executes it.
    for (kind, script) in [("clean", "touch sentinel; cat"), ("process", "touch sentinel; exit 1")] {
        git(&["config", &format!("filter.sentinel.{kind}"), script]);
        std::fs::write(repo.join("source.rs"), "modified and different length\n").unwrap();
        for command in [
            format!("git --no-pager -C {} diff --no-ext-diff --no-textconv", repo.display()),
            format!(
                "git --no-pager -C {} diff --no-ext-diff --no-textconv {sha}",
                repo.display()
            ),
            format!(
                "git --no-pager -C {} diff --no-ext-diff --no-textconv --cached {sha}",
                repo.display()
            ),
        ] {
            assert_eq!(f.bash(&command), "deny", "{command}");
        }
        for command in [
            format!(
                "git --no-pager -C {} diff --no-ext-diff --no-textconv {sha} {sha}",
                repo.display()
            ),
            format!(
                "git --no-pager -C {} show --no-ext-diff --no-textconv --no-show-signature {sha}",
                repo.display()
            ),
            format!("git --no-pager -C {} show {sha}:source.rs", repo.display()),
        ] {
            assert_eq!(f.bash(&command), "allow", "{command}");
            let output = Command::new("/bin/sh").args(["-c", &command]).output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert!(!repo.join("sentinel").exists(), "{command} executed a filter");
        }
        // Prove the rejected working-tree form actually runs this filter.
        let _ = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["--no-pager", "diff", "--no-ext-diff", "--no-textconv"])
            .output()
            .unwrap();
        assert!(repo.join("sentinel").exists(), "{kind} control did not execute");
        std::fs::remove_file(repo.join("sentinel")).unwrap();
        git(&["config", "--unset", &format!("filter.sentinel.{kind}")]);
    }
}
