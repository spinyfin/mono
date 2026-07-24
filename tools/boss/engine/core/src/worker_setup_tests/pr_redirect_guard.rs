use super::super::*;

// ── PR_REDIRECT_GUARD_COMMAND execution tests ─────────────────────────
//
// These tests actually run the PR redirect guard script (via `sh -c`) to
// verify that it blocks direct VCS push commands and GH PR creation while
// allowing cube pr create/update.

/// Run the PR redirect guard against a simulated Bash tool_input payload
/// and return the `decision` field from its JSON output.
fn run_pr_redirect_guard(bash_command: &str) -> String {
    use std::io::Write as _;
    let stdin_payload = serde_json::json!({
        "tool_input": {"command": bash_command}
    })
    .to_string();

    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(PR_REDIRECT_GUARD_COMMAND)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("sh must be available");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin_payload.as_bytes())
        .unwrap();
    drop(child.stdin.take());

    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "PR redirect guard produced invalid JSON for command {:?}: {e}\nstdout={stdout}\nstderr={}",
            bash_command,
            String::from_utf8_lossy(&out.stderr),
        )
    });
    parsed["decision"].as_str().unwrap_or("missing").to_owned()
}

// --- PR redirect guard: must BLOCK ---

#[test]
fn pr_redirect_blocks_gh_pr_create() {
    let decision = run_pr_redirect_guard("gh pr create --title 'My PR' --body 'content'");
    assert_eq!(decision, "block", "guard must block gh pr create");
}

#[test]
fn pr_redirect_blocks_gh_pr_create_with_git_dir_prefix() {
    let decision = run_pr_redirect_guard("GIT_DIR=.jj/repo/store/git gh pr create --title 'x'");
    assert_eq!(
        decision, "block",
        "guard must block gh pr create even with a GIT_DIR= env prefix"
    );
}

#[test]
fn pr_redirect_blocks_jj_git_push_with_allow_new() {
    let decision = run_pr_redirect_guard("jj git push -b boss/exec_abc --allow-new");
    assert_eq!(decision, "block", "guard must block jj git push --allow-new");
}

#[test]
fn pr_redirect_blocks_jj_git_push_without_allow_new() {
    let decision = run_pr_redirect_guard("jj git push -b boss/exec_abc");
    assert_eq!(
        decision, "block",
        "guard must block jj git push even without --allow-new"
    );
}

#[test]
fn pr_redirect_blocks_git_push() {
    let decision = run_pr_redirect_guard("git push origin boss/exec_abc");
    assert_eq!(decision, "block", "guard must block bare git push");
}

#[test]
fn pr_redirect_blocks_gh_pr_create_in_compound_command() {
    let decision = run_pr_redirect_guard(r#"jj describe -m "my change" && gh pr create --title 'x'"#);
    assert_eq!(decision, "block", "guard must block gh pr create in a compound command");
}

#[test]
fn pr_redirect_blocks_jj_git_push_in_compound_command() {
    let decision = run_pr_redirect_guard(r#"jj describe -m "my change" && jj git push -b boss/exec_abc --allow-new"#);
    assert_eq!(decision, "block", "guard must block jj git push in a compound command");
}

// --- PR redirect guard: must APPROVE ---

#[test]
fn pr_redirect_approves_cube_pr_create() {
    let decision = run_pr_redirect_guard("cube pr create --branch boss/exec_abc --title 'x'");
    assert_eq!(decision, "approve", "guard must allow cube pr create");
}

#[test]
fn pr_redirect_approves_cube_pr_update() {
    let decision = run_pr_redirect_guard("cube pr update --branch boss/exec_abc");
    assert_eq!(decision, "approve", "guard must allow cube pr update");
}

#[test]
fn pr_redirect_approves_jj_git_fetch() {
    let decision = run_pr_redirect_guard("jj git fetch");
    assert_eq!(decision, "approve", "guard must allow jj git fetch");
}

#[test]
fn pr_redirect_approves_jj_describe_with_jj_git_push_in_message() {
    let decision = run_pr_redirect_guard(r#"jj describe -m "fix: stop using jj git push for PRs""#);
    assert_eq!(
        decision, "approve",
        "guard must NOT block jj describe when the phrase is in the commit message",
    );
}

#[test]
fn pr_redirect_approves_jj_bookmark_create() {
    let decision = run_pr_redirect_guard("jj bookmark create boss/exec_abc -r @");
    assert_eq!(decision, "approve", "guard must allow jj bookmark create");
}

#[test]
fn pr_redirect_approves_echo_with_jj_git_push_phrase() {
    let decision = run_pr_redirect_guard(r#"echo "jj git push is now blocked""#);
    assert_eq!(decision, "approve", "echo with jj git push phrase must not be blocked");
}
