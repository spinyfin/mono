//! Isolation of content-addressed guard materialisation: one engine
//! build's bytes must never overwrite the file another build's workers
//! are attesting.
//!
//! The Codex driver content-binds the guard path at arming and re-checks
//! the digest on every tool call. A shared mutable path is what turned a
//! few seconds of engine-build divergence into a permanent brick. These
//! tests cover that collision, not just the hashing helper.

use std::io::Write as _;
use std::time::{Duration, SystemTime};

use super::super::*;
use super::helpers::*;

use crate::driver::codex::guard_trace::GUARD_TRACE_SHIM_SCRIPT;

fn sha256_prefixed(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

/// Run the production Codex guard-trace shim against `guard_path` with
/// the attested digest `expected_sha`, the same way an armed Codex
/// worker re-verifies the guard on every tool call.
fn run_armed_codex_shim(tag: &str, guard_path: &std::path::Path, expected_sha: &str) -> (String, String) {
    let dir = TempDir::new().unwrap();
    let shim = dir.path().join(format!("{tag}-shim.py"));
    std::fs::write(&shim, GUARD_TRACE_SHIM_SCRIPT).unwrap();
    let trace = dir.path().join("guard-trace.jsonl");

    let mut child = std::process::Command::new("python3")
        .arg(&shim)
        .arg(guard_path)
        .env("BOSS_GUARD_TRACE", &trace)
        .env("BOSS_GUARD_NAME", "path_guard")
        .env("BOSS_GUARD_SHA256", expected_sha)
        .env_remove("BOSS_DATA_DIR")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("python3 must be available");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{"tool_name":"Read","tool_input":{"file_path":"src/main.rs"}}"#)
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        String::from_utf8_lossy(&out.stderr).trim().to_owned(),
    )
}

#[test]
fn a_second_build_cannot_brick_an_already_armed_codex_path_guard() {
    // The production failure: build A materialises `$TMPDIR/boss-worker-settings/
    // boss-path-guard.py`, Codex attests those bytes, build B overwrites the
    // same path with different bytes, and every subsequent tool call fail-closes
    // on `guard bytes do not match the attested content hash`.
    let dir = TempDir::new().unwrap();
    let bytes_a = PATH_GUARD_SCRIPT.as_bytes();
    let bytes_b = format!("{PATH_GUARD_SCRIPT}\n# other-build\n");
    let hash_a = sha256_prefixed(bytes_a);

    let path_a = ensure_content_addressed_script(dir.path(), PATH_GUARD_KIND, PATH_GUARD_SCRIPT_NAME, bytes_a).unwrap();

    // Arm: the shim the Codex driver wraps around this path accepts the
    // attested digest and the guard itself still answers.
    let (stdout, stderr) = run_armed_codex_shim("arm-a", &path_a, &hash_a);
    assert!(
        stdout.is_empty(),
        "an armed Codex worker must allow a benign call; stdout={stdout:?} stderr={stderr:?}"
    );

    let path_b =
        ensure_content_addressed_script(dir.path(), PATH_GUARD_KIND, PATH_GUARD_SCRIPT_NAME, bytes_b.as_bytes())
            .unwrap();

    assert_ne!(
        path_a, path_b,
        "divergent builds must materialise distinct paths, not share one"
    );
    assert_eq!(
        std::fs::read(&path_a).unwrap(),
        bytes_a,
        "build B must not mutate the bytes build A attested"
    );
    assert!(
        !dir.path().join(PATH_GUARD_SCRIPT_NAME).exists(),
        "must not write the unversioned shared path that was the collision"
    );

    // After the other build materialised different bytes, the already-armed
    // worker still functions: attestation matches, guard still answers.
    let (stdout, stderr) = run_armed_codex_shim("after-b", &path_a, &hash_a);
    assert!(
        stdout.is_empty(),
        "armed worker must keep working after another build materialises; \
         stdout={stdout:?} stderr={stderr:?}"
    );

    // Fail-closed is intact: pointing A's attestation at B's bytes is still
    // a refusal, never an approval.
    let (stdout, stderr) = run_armed_codex_shim("mismatch", &path_b, &hash_a);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|err| {
        panic!("mismatch must emit a JSON block, not fall through ({err}); stdout={stdout:?} stderr={stderr:?}")
    });
    assert_eq!(parsed["decision"], "block", "hash mismatch must fail closed: {parsed}");
    let reason = parsed["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("guard bytes do not match the attested content hash"),
        "block reason must name the hash mismatch: {reason}"
    );
}

#[test]
fn ensure_path_guard_script_in_does_not_write_the_unversioned_shared_path() {
    let dir = TempDir::new().unwrap();
    let written = ensure_path_guard_script_in(dir.path()).unwrap();
    assert_eq!(written, path_guard_script_path_in(dir.path()));
    assert!(written.exists(), "current build's hashed path must be written");
    assert_eq!(std::fs::read_to_string(&written).unwrap(), PATH_GUARD_SCRIPT);
    assert!(
        !dir.path().join(PATH_GUARD_SCRIPT_NAME).exists(),
        "unversioned boss-path-guard.py is the collision; do not write it"
    );
    let parent = written.parent().expect("hashed file lives in a kind-hash directory");
    assert!(
        is_content_addressed_guard_dir_name(parent.file_name().unwrap().to_str().unwrap(), PATH_GUARD_KIND),
        "parent must be path-guard-<sha256>, got {}",
        parent.display()
    );
}

#[test]
fn writing_a_guard_script_logs_process_and_build_identity_at_info() {
    let buffer = crate::test_support::log_capture::install();
    let start = buffer.lock().len();

    let dir = TempDir::new().unwrap();
    let written = ensure_path_guard_script_in(dir.path()).unwrap();
    let hash = sha256_hex(PATH_GUARD_SCRIPT.as_bytes());

    let captured = String::from_utf8(buffer.lock()[start..].to_vec()).expect("utf8 log capture");
    let line = captured
        .lines()
        .find(|line| line.contains("wrote worker guard script") && line.contains(&written.display().to_string()))
        .unwrap_or_else(|| panic!("no write log for {}; captured: {captured}", written.display()));
    assert!(
        line.contains("INFO"),
        "guard writes must be visible in production logs, not DEBUG: {line}"
    );
    assert!(line.contains("pid="), "must record writing process identity: {line}");
    assert!(line.contains("exe="), "must record writing process identity: {line}");
    assert!(
        line.contains("version="),
        "must record the writing build/version: {line}"
    );
    assert!(line.contains("git_sha="), "must record the writing build: {line}");
    assert!(
        line.contains("binary_fingerprint="),
        "must record the writing build: {line}"
    );
    assert!(line.contains(&hash), "must record the resulting content hash: {line}");
    assert!(
        line.contains("replaced_different_bytes=false"),
        "a fresh write does not replace different bytes: {line}"
    );
}

#[test]
fn a_hash_collision_at_the_same_path_is_logged_and_fails_closed() {
    let buffer = crate::test_support::log_capture::install();
    let dir = TempDir::new().unwrap();
    let path = path_guard_script_path_in(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"not-the-guard-script\n").unwrap();

    let start = buffer.lock().len();
    let err = ensure_path_guard_script_in(dir.path())
        .expect_err("a file that doesn't hash to its own directory name must never be wired in as the PreToolUse gate");
    assert!(
        err.to_string().contains("refusing to wire an unverified file in"),
        "error must explain the fail-closed refusal: {err}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"not-the-guard-script\n",
        "must never overwrite attested bytes, even when the filename-hash disagrees"
    );

    let captured = String::from_utf8(buffer.lock()[start..].to_vec()).expect("utf8 log capture");
    let line = captured
        .lines()
        .find(|line| line.contains("already exists with different bytes") && line.contains(&path.display().to_string()))
        .unwrap_or_else(|| panic!("no refuse-overwrite log for {}; captured: {captured}", path.display()));
    assert!(
        line.contains("ERROR") || line.contains("error"),
        "the outage event must be louder than DEBUG: {line}"
    );
    assert!(line.contains("existing_bytes_differ=true"), "{line}");
    assert!(
        line.contains("replaced_different_bytes=false"),
        "must not actually replace: {line}"
    );
}

#[test]
fn unreferenced_stale_guard_dirs_are_pruned_after_the_grace_window() {
    let dir = TempDir::new().unwrap();
    let bytes_a = b"guard-bytes-a\n";
    let bytes_b = b"guard-bytes-b\n";
    let path_a = ensure_content_addressed_script(dir.path(), PATH_GUARD_KIND, PATH_GUARD_SCRIPT_NAME, bytes_a).unwrap();
    let path_b = ensure_content_addressed_script(dir.path(), PATH_GUARD_KIND, PATH_GUARD_SCRIPT_NAME, bytes_b).unwrap();
    assert!(
        path_a.exists() && path_b.exists(),
        "grace keeps both while they are young"
    );

    let old = SystemTime::now() - GUARD_SCRIPT_PRUNE_GRACE - Duration::from_secs(60);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path_a)
        .unwrap()
        .set_modified(old)
        .unwrap();

    // Re-materialising B runs prune. A is unreferenced and past grace.
    ensure_content_addressed_script(dir.path(), PATH_GUARD_KIND, PATH_GUARD_SCRIPT_NAME, bytes_b).unwrap();
    assert!(!path_a.exists(), "unreferenced A past grace must be pruned");
    assert!(path_b.exists(), "the current build's file must survive prune");
}

#[test]
fn settings_json_reference_protects_an_old_guard_dir_from_prune() {
    let dir = TempDir::new().unwrap();
    let bytes_a = b"guard-bytes-a\n";
    let bytes_b = b"guard-bytes-b\n";
    let path_a = ensure_content_addressed_script(dir.path(), PATH_GUARD_KIND, PATH_GUARD_SCRIPT_NAME, bytes_a).unwrap();
    let hash_dir = path_a
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    std::fs::write(
        dir.path().join("ws.json"),
        format!(r#"{{"hooks":"python3 '{}'"}}"#, path_a.display()),
    )
    .unwrap();
    let old = SystemTime::now() - GUARD_SCRIPT_PRUNE_GRACE - Duration::from_secs(60);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path_a)
        .unwrap()
        .set_modified(old)
        .unwrap();

    ensure_content_addressed_script(dir.path(), PATH_GUARD_KIND, PATH_GUARD_SCRIPT_NAME, bytes_b).unwrap();
    assert!(
        path_a.exists(),
        "a settings file still pointing at {hash_dir} must keep that directory"
    );
}
