//! Integration test for the editorial PreToolUse handler against a stub
//! `gh` shim.
//!
//! The acceptance criterion for the editorial PreToolUse chore is an
//! end-to-end test that exercises the handler's three real outcomes
//! against a fake `gh` executable:
//!
//! 1. **bad body → deny** — a body that trips a `Block`-kind rule is
//!    denied; the `gh` shim is never run.
//! 2. **redactable identifiers → allow-with-rewrite, substituted body
//!    lands** — an inline `--body` carrying a Boss id is rewritten; when
//!    the *mutated* command is run through the stub `gh`, the captured
//!    body has the id stripped.
//! 3. **`--body-file` rewrite** — the handler overwrites the worker's
//!    body file on disk; the stub `gh` reading that file sees redacted
//!    content.
//! 4. **three-deny → allow + attention item** — a body the worker keeps
//!    re-submitting is denied twice and allowed (with a flagged
//!    attention item) on the third attempt.
//!
//! The stub `gh` is a tiny shell script that records whatever body it
//! received (inline or from a file) to `$GH_CAPTURE`, so the test can
//! assert what *would* have reached GitHub.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use boss_editorial::CompiledRules;
use boss_engine::editorial_hook::{
    DenyTracker, EditorialActionKind, PreToolUseDecision, evaluate_gh_pretooluse,
};
use boss_protocol::EditorialRules;
use tempfile::TempDir;

const EXEC_ID: &str = "exec_18b07a506d2518d0_1b";

fn default_rules() -> CompiledRules {
    CompiledRules::compile(EditorialRules::default()).unwrap()
}

/// Write a stub `gh` executable into `dir` that captures the body it was
/// given (inline `--body`/`-b` or `--body-file`/`-F`) to `$GH_CAPTURE`.
/// Returns the directory path to prepend to `PATH`.
fn install_stub_gh(dir: &Path) {
    let script = r#"#!/bin/sh
# stub gh: record the body we were asked to publish.
body=""
while [ $# -gt 0 ]; do
  case "$1" in
    --body|-b) body="$2"; shift 2 ;;
    --body-file|-F) body="$(cat "$2")"; shift 2 ;;
    *) shift ;;
  esac
done
printf '%s' "$body" > "$GH_CAPTURE"
echo "https://github.com/foo/bar/pull/1"
"#;
    let gh = dir.join("gh");
    fs::write(&gh, script).unwrap();
    let mut perms = fs::metadata(&gh).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&gh, perms).unwrap();
}

/// Run `command` through the stub `gh`, capturing the published body.
/// Returns the captured body text.
fn run_through_stub_gh(command: &str, workspace: &Path, stub_dir: &Path) -> String {
    let capture = workspace.join("gh-capture.txt");
    let existing_path = std::env::var("PATH").unwrap_or_default();
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(workspace)
        .env("PATH", format!("{}:{}", stub_dir.display(), existing_path))
        .env("GH_CAPTURE", &capture)
        .status()
        .expect("run stub gh");
    assert!(status.success(), "stub gh exited non-zero");
    fs::read_to_string(&capture).unwrap_or_default()
}

#[test]
fn bad_body_is_denied_and_gh_never_runs() {
    let ws = TempDir::new().unwrap();
    let stub = TempDir::new().unwrap();
    install_stub_gh(stub.path());

    let cmd = "gh pr create --title t --body 'Opened by a Boss worker for you.'";
    let outcome = evaluate_gh_pretooluse(
        cmd,
        ws.path(),
        &default_rules(),
        None,
        EXEC_ID,
        &DenyTracker::new(),
    );

    match &outcome.decision {
        PreToolUseDecision::Deny { reason } => {
            assert!(reason.contains("Boss worker"), "reason: {reason}");
        }
        other => panic!("expected Deny, got {other:?}"),
    }
    assert_eq!(outcome.action, EditorialActionKind::Deny);
    // A denied call must never reach the stub: no capture file was created.
    assert!(!ws.path().join("gh-capture.txt").exists());
}

#[test]
fn redactable_inline_body_rewrite_lands_through_gh() {
    let ws = TempDir::new().unwrap();
    let stub = TempDir::new().unwrap();
    install_stub_gh(stub.path());

    let cmd = format!("gh pr create --title t --body 'Fixes {EXEC_ID} in prod.'");
    let outcome = evaluate_gh_pretooluse(
        &cmd,
        ws.path(),
        &default_rules(),
        None,
        EXEC_ID,
        &DenyTracker::new(),
    );

    let mutated = match &outcome.decision {
        PreToolUseDecision::AllowWithRewrite {
            updated_command: Some(c),
            ..
        } => c.clone(),
        other => panic!("expected AllowWithRewrite with mutated command, got {other:?}"),
    };
    assert_eq!(outcome.action, EditorialActionKind::Rewrite);

    // Run the *mutated* command — the body that lands at gh must be clean.
    let published = run_through_stub_gh(&mutated, ws.path(), stub.path());
    assert!(
        !published.contains(EXEC_ID),
        "published body still leaks the id: {published:?}"
    );
    assert!(published.contains("Fixes"), "published: {published:?}");
    assert!(published.contains("in prod."), "published: {published:?}");
}

#[test]
fn body_file_rewrite_lands_through_gh() {
    let ws = TempDir::new().unwrap();
    let stub = TempDir::new().unwrap();
    install_stub_gh(stub.path());

    let body_path = ws.path().join("pr-body.md");
    fs::write(
        &body_path,
        format!("## Summary\n\nResolves {EXEC_ID} that broke login.\n"),
    )
    .unwrap();

    let cmd = "gh pr create --title t --body-file pr-body.md";
    let outcome = evaluate_gh_pretooluse(
        cmd,
        ws.path(),
        &default_rules(),
        None,
        EXEC_ID,
        &DenyTracker::new(),
    );

    // A body-file rewrite leaves the command unchanged (the file on disk
    // is what changed).
    match &outcome.decision {
        PreToolUseDecision::AllowWithRewrite {
            updated_command, ..
        } => assert!(updated_command.is_none()),
        other => panic!("expected AllowWithRewrite, got {other:?}"),
    }

    // The file on disk has been scrubbed...
    let on_disk = fs::read_to_string(&body_path).unwrap();
    assert!(!on_disk.contains(EXEC_ID), "file still leaks id: {on_disk:?}");

    // ...and the stub gh reading that file sees the scrubbed content.
    let published = run_through_stub_gh(cmd, ws.path(), stub.path());
    assert!(!published.contains(EXEC_ID), "published: {published:?}");
    assert!(published.contains("broke login"), "published: {published:?}");
}

#[test]
fn three_denies_flip_to_allow_with_attention_item() {
    let ws = TempDir::new().unwrap();
    let tracker = DenyTracker::new();
    let cmd = "gh pr create --title t --body 'Authored by a Boss worker.'";

    let d1 = evaluate_gh_pretooluse(cmd, ws.path(), &default_rules(), None, EXEC_ID, &tracker);
    assert!(matches!(d1.decision, PreToolUseDecision::Deny { .. }));
    assert!(d1.attention.is_none());

    let d2 = evaluate_gh_pretooluse(cmd, ws.path(), &default_rules(), None, EXEC_ID, &tracker);
    assert!(matches!(d2.decision, PreToolUseDecision::Deny { .. }));
    assert!(d2.attention.is_none());

    let d3 = evaluate_gh_pretooluse(cmd, ws.path(), &default_rules(), None, EXEC_ID, &tracker);
    assert_eq!(d3.decision, PreToolUseDecision::Allow);
    assert_eq!(d3.action, EditorialActionKind::Allow);
    let attention = d3
        .attention
        .expect("the third deny must flip to allow with an attention item");
    assert!(
        attention.detail.contains("Boss worker"),
        "attention detail should name the unresolved finding: {}",
        attention.detail
    );

    // And the flipped-through call really does reach gh with the original
    // (non-compliant) body — the loop guard ships it rather than blocking.
    let stub = TempDir::new().unwrap();
    install_stub_gh(stub.path());
    let published = run_through_stub_gh(cmd, ws.path(), stub.path());
    assert!(published.contains("Boss worker"), "published: {published:?}");
}
