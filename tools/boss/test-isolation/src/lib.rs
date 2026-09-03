//! Process-startup guard that keeps a `rust_test` binary off Boss's
//! production state paths.
//!
//! Linking this crate — which every `rust_test` target under `tools/boss/**`
//! does via the `boss_rust_test` Bazel macro (`boss_rust_test.bzl`) — installs
//! an isolated, private state root into `boss_log_files` before `main` runs,
//! via a `ctor`-attributed constructor. Every `default_*_path` function in
//! `boss_log_files::paths` derives from that root once installed, so a test
//! binary can never resolve production's `~/Library/Application Support/Boss`
//! — whether it is invoked through `bazel test` (which additionally redirects
//! `$HOME` and sandboxes writes) or run directly as a `bazel-bin/...` binary
//! with the caller's real environment, bypassing both of those. See
//! `boss_log_files::paths` for the resolver side of this guard, and its doc
//! comments for the 2026-09-03 incident that motivated it.
//!
//! This crate must never be a dependency of production code (the `engine`
//! binary, `bossctl`, the `boss` CLI) — only of `rust_test` targets. Nothing
//! in Rust itself enforces that; `boss_rust_test` and the
//! `boss/raw-rust-test-forbidden` checkleft check are what keep it that way.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Runs before any other code in a binary that links this crate — including
/// `main`, and including every `#[test]` function, since libtest's harness
/// itself only starts running after the process's constructors have. Aborts
/// rather than falling back: a test process that cannot obtain an isolated
/// root must not silently keep running against whatever `default_state_root`
/// would otherwise resolve.
#[ctor::ctor]
fn install_isolated_state_root() {
    match create_isolated_root() {
        Ok(root) => boss_log_files::install_test_state_root(root),
        Err(err) => {
            eprintln!(
                "boss-test-isolation: refusing to start — could not create a private isolated state root \
                 ({err}). This binary must run with a writable system temp directory; under `bazel test` \
                 that is provided by the sandbox, and a `bazel-bin/...` binary invoked directly needs one \
                 too for this guard to do its job."
            );
            std::process::abort();
        }
    }
}

/// A private-to-this-process directory under the system temp dir, named with
/// both the pid and a nanosecond timestamp so two processes launched in the
/// same tick — parallel `bazel test` shards, or a fast pid-reuse race — never
/// collide on the same directory.
fn create_isolated_root() -> std::io::Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut dir = std::env::temp_dir();
    dir.push(format!("boss-test-isolation-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    /// End-to-end smoke test: by the time this test body runs, the ctor
    /// above has already fired (libtest only starts after process
    /// constructors have), so the isolation it installs must already be
    /// live and must never coincide with the real production state root.
    #[test]
    fn ctor_installs_a_real_isolated_root_before_this_test_runs() {
        assert!(
            boss_log_files::is_test_process(),
            "the ctor must have installed a root before this test body ran"
        );
        let root = boss_log_files::default_state_root().expect("an installed root always resolves to Some");
        assert!(
            root.exists(),
            "the installed root must actually exist on disk: {root:?}"
        );

        let production_under_real_home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .map(|home| home.join(boss_log_files::STATE_ROOT_SUFFIX));
        assert_ne!(
            Some(root),
            production_under_real_home,
            "the installed root must never equal production's, even if $HOME happens to be set here"
        );
    }
}
