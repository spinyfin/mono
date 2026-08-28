//! End-to-end regression test for the actual process exit code of `checkleft
//! run` when the changeset cannot be determined.
//!
//! Every other assertion about this behavior (`src/tests.rs`'s
//! `changeset_undetermined` module, `tests/change_detection_e2e.rs`) goes
//! through `exit_code_for_error`, a pure helper — none of them spawn the real
//! binary and check `std::process::ExitStatus::code()`. This test closes that
//! gap: it drives the built `checkleft` binary (staged via Bazel `data` +
//! `CHECKLEFT_E2E_BIN`, resolved from runfiles) against a real repo with no
//! merge base and asserts the process actually exits `3`, with stderr naming
//! the reason — the one thing that matters to a caller and that no other test
//! in the suite observes.

use std::path::PathBuf;
use std::process::Command;

use tempfile::tempdir;

/// Resolve the real `checkleft` binary from the test's runfiles, or `None`
/// when it was not staged (e.g. running under plain `cargo test`, where there
/// are no runfiles). Mirrors the pattern used for the staged `buildifier` /
/// `rustfmt` binaries in the lib-test shards: under `bazel test`,
/// `CHECKLEFT_E2E_BIN` is always set, so this test always runs for real in
/// CI; outside Bazel we assert we are genuinely not under `bazel test` rather
/// than silently skipping.
fn checkleft_bin_from_runfiles() -> Option<PathBuf> {
    match std::env::var("CHECKLEFT_E2E_BIN") {
        Ok(rlocationpath) => {
            let runfiles = runfiles::Runfiles::create().expect("runfiles must initialize under `bazel test`");
            let path = runfiles
                .rlocation(&rlocationpath)
                .unwrap_or_else(|| panic!("checkleft binary rlocation must resolve"));
            assert!(
                path.exists(),
                "staged checkleft binary must exist at {}",
                path.display()
            );
            Some(path)
        }
        Err(_) => {
            assert!(
                std::env::var_os("TEST_SRCDIR").is_none(),
                "running under `bazel test` but CHECKLEFT_E2E_BIN is unset — the checkleft binary \
                 `data`/`env` wiring on changeset_undetermined_exit_code_e2e_test is broken; refusing \
                 to silently skip the test it backs"
            );
            None
        }
    }
}

fn git(root: &std::path::Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(root).output().expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn commit(root: &std::path::Path, name: &str, content: &str, msg: &str) {
    std::fs::write(root.join(name), content).expect("write file");
    git(root, &["add", name]);
    git(root, &["commit", "-m", msg]);
}

/// The real regression this closes: an orphan root commit on the default
/// branch (first push to a new repo) has no merge base at all, which must
/// make the *process* exit `3` (`ChangesetUndetermined`) with a message
/// naming the reason — not exit `1` (indistinguishable from "checks found
/// problems") and not exit `0` (a silent, wrong pass).
#[test]
fn no_merge_base_exits_with_dedicated_code() {
    let Some(bin) = checkleft_bin_from_runfiles() else {
        return;
    };

    let dir = tempdir().expect("tempdir");
    let root = dir.path();
    checkleft::test_git::init_repo_with_branch(root, "main");
    git(root, &["config", "user.email", "test@checkleft.example"]);
    git(root, &["config", "user.name", "Checkleft Test"]);
    commit(root, "base.txt", "base\n", "A: base on main");
    // An orphan branch with its own root commit shares no ancestry with main.
    git(root, &["checkout", "--orphan", "feature"]);
    git(root, &["rm", "-rf", "--cached", "."]);
    std::fs::remove_file(root.join("base.txt")).ok();
    commit(root, "feature.rs", "fn feature() {}\n", "X: orphan root");

    let output = Command::new(&bin)
        .arg("run")
        .current_dir(root)
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_EVENT_NAME", "pull_request")
        .env("GITHUB_BASE_REF", "main")
        .env("CI", "true")
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", bin.display()));

    assert_eq!(
        output.status.code(),
        Some(3),
        "expected exit code 3 (ChangesetUndetermined), got {:?}; stdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no common ancestor"),
        "stderr must name the specific reason: {stderr}"
    );
}
