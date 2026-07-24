use super::super::*;

// ── REVISION_PR_GUARD_COMMAND execution tests ─────────────────────────
//
// These tests actually run the guard script (via `sh -c`) to verify
// its behaviour end-to-end, including the shlex tokenisation fix.

/// Run the revision PR guard against a simulated Bash tool_input payload
/// and return the `decision` field from its JSON output.
fn run_revision_guard(bash_command: &str) -> String {
    use std::io::Write as _;
    let stdin_payload = serde_json::json!({
        "tool_input": {"command": bash_command}
    })
    .to_string();

    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(REVISION_PR_GUARD_COMMAND)
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
            "guard produced invalid JSON for command {:?}: {e}\nstdout={stdout}\nstderr={}",
            bash_command,
            String::from_utf8_lossy(&out.stderr),
        )
    });
    parsed["decision"].as_str().unwrap_or("missing").to_owned()
}

// --- false-positive regression tests (must APPROVE) ---

#[test]
fn guard_approves_jj_describe_with_cube_pr_ensure_in_message() {
    // The bug: `jj describe -m "...cube pr ensure..."` was blocked
    // because the phrase appeared inside the quoted commit message.
    let decision =
        run_revision_guard(r#"jj describe -m "fix(boss-engine): extend editorial hook to intercept cube pr ensure""#);
    assert_eq!(
        decision, "approve",
        "guard must NOT block jj describe when the phrase is in the commit message",
    );
}

#[test]
fn guard_approves_git_commit_with_gh_pr_create_in_message() {
    let decision = run_revision_guard(r#"git commit -m "docs: explain how gh pr create interacts with the hook""#);
    assert_eq!(
        decision, "approve",
        "guard must NOT block git commit when the phrase is in the commit message",
    );
}

#[test]
fn guard_approves_echo_with_pr_creation_phrase() {
    let decision = run_revision_guard(r#"echo "cube pr ensure is documented here""#);
    assert_eq!(decision, "approve", "echo must not be blocked");
}

#[test]
fn guard_approves_jj_describe_with_gh_pr_create_in_message() {
    let decision = run_revision_guard(r#"jj describe -m "fix: the gh pr create story in this branch""#);
    assert_eq!(decision, "approve", "jj describe must not be blocked");
}

// --- true-positive tests (must BLOCK) ---

#[test]
fn guard_blocks_gh_pr_create() {
    let decision = run_revision_guard("gh pr create --title 'My PR' --body 'content'");
    assert_eq!(decision, "block", "guard must block a bare gh pr create",);
}

#[test]
fn guard_blocks_cube_pr_ensure() {
    let decision = run_revision_guard("cube pr ensure --branch feat/foo --title 'My PR'");
    assert_eq!(decision, "block", "guard must block a bare cube pr ensure",);
}

#[test]
fn guard_blocks_gh_pr_create_with_git_dir_prefix() {
    let decision = run_revision_guard("GIT_DIR=.jj/repo/store/git gh pr create --title 'x' --body 'y'");
    assert_eq!(
        decision, "block",
        "guard must block gh pr create even with a GIT_DIR= prefix",
    );
}

#[test]
fn guard_blocks_cube_pr_ensure_in_compound_command() {
    // A compound command: benign `jj describe` first, then a real
    // `cube pr ensure` — the guard must catch the second part.
    let decision = run_revision_guard(r#"jj describe -m "push changes" && cube pr ensure --branch feat/x"#);
    assert_eq!(
        decision, "block",
        "guard must block cube pr ensure in a compound command",
    );
}

#[test]
fn guard_blocks_cube_pr_create() {
    let decision = run_revision_guard("cube pr create --branch feat/foo --title 'My PR'");
    assert_eq!(decision, "block", "guard must block a bare cube pr create",);
}

#[test]
fn guard_blocks_cube_pr_create_in_compound_command() {
    let decision = run_revision_guard(r#"jj describe -m "push changes" && cube pr create --branch feat/x"#);
    assert_eq!(
        decision, "block",
        "guard must block cube pr create in a compound command",
    );
}

// --- allowed-update tests (must APPROVE) ---

#[test]
fn guard_approves_cube_pr_update() {
    // Revision workers advance the existing PR with `cube pr update` — this
    // is the sanctioned verb and must NOT be blocked.
    let decision = run_revision_guard("cube pr update --branch feat/foo");
    assert_eq!(decision, "approve", "guard must allow cube pr update",);
}

#[test]
fn guard_approves_cube_pr_update_in_compound_command() {
    let decision = run_revision_guard(r#"jj describe -m "push" && cube pr update --branch feat/x"#);
    assert_eq!(
        decision, "approve",
        "guard must allow cube pr update in a compound command",
    );
}

/// Run the guard and return the full `reason` string from a block decision.
fn run_revision_guard_reason(bash_command: &str) -> String {
    use std::io::Write as _;
    let stdin_payload = serde_json::json!({
        "tool_input": {"command": bash_command}
    })
    .to_string();

    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(REVISION_PR_GUARD_COMMAND)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin_payload.as_bytes())
        .unwrap();
    drop(child.stdin.take());

    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    parsed["reason"].as_str().unwrap_or("").to_owned()
}

#[test]
fn guard_block_message_names_matched_command() {
    let reason = run_revision_guard_reason("cube pr ensure --branch b");
    assert!(
        reason.contains("cube pr ensure"),
        "block reason must name the matched command, got: {reason}",
    );
}

#[test]
fn guard_block_message_suggests_cube_pr_update_with_branch() {
    // The block message must hand the worker the exact recovery command,
    // reusing the --branch value from the blocked invocation — no jj
    // forensics required.
    let reason = run_revision_guard_reason("cube pr create --branch boss/exec_abc123_01 --title 'x'");
    assert!(
        reason.contains("cube pr update --branch boss/exec_abc123_01"),
        "block reason must suggest `cube pr update --branch <bookmark>`, got: {reason}",
    );
}

#[test]
fn guard_block_message_reuses_head_branch_from_gh_pr_create() {
    // `gh pr create --head <branch>` should also surface the concrete branch
    // in the `cube pr update` suggestion.
    let reason = run_revision_guard_reason("gh pr create --head boss/exec_abc123_01 --title 'x'");
    assert!(
        reason.contains("cube pr update --branch boss/exec_abc123_01"),
        "block reason must reuse the --head branch in the update suggestion, got: {reason}",
    );
}
