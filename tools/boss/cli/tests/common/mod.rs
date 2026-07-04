//! Shared test-support helpers for the `boss` CLI integration tests.
//!
//! Each integration test under `tools/boss/cli/tests/*.rs` is its own
//! `rust_test` target, so this file is listed in the `srcs` of every target
//! and pulled in via `mod common;` from each test. It holds the helpers that
//! only drive the compiled `boss` binary as a subprocess: locating it, and
//! running it in JSON / human mode or expecting failure. These need nothing
//! from the engine library, so even the tests that never spawn an engine
//! (`shake`, `uninstall`) can share them without pulling `boss-engine` in.
//!
//! The in-process engine harness (`TestEngine`) lives in the sibling
//! `harness` module, which only the engine-backed targets wire into their
//! `srcs`.

// Each integration test binary compiles this file independently and not every
// binary uses every helper; suppress dead-code noise rather than gate items.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Result, anyhow};
use serde_json::Value;

/// Resolve the `boss` binary path. Cargo defines `CARGO_BIN_EXE_boss`
/// for integration tests automatically. Under Bazel the `rust_test` rule
/// stages the binary as a data dep and we resolve it through
/// `RUNFILES_DIR` (set by `rust_test`'s test runner). Falling back to
/// `$PATH` would silently hit whatever stale binary the user has
/// installed system-wide, so we panic if neither path resolves.
pub fn boss_binary() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_boss") {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }
    if let Ok(runfiles_dir) = std::env::var("RUNFILES_DIR") {
        let p = PathBuf::from(runfiles_dir).join("_main/tools/boss/cli/boss");
        if p.exists() {
            return p;
        }
    }
    panic!("boss binary path not found; ran via cargo or bazel?");
}

/// Run `boss --json …` and return parsed stdout.
pub fn run_boss(socket: &str, args: &[&str]) -> Result<Value> {
    let output = Command::new(boss_binary())
        .args(["--json", "--no-input", "--no-autostart", "--socket-path", socket])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "boss {} failed (status={:?}):\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    let stdout = String::from_utf8(output.stdout)?;
    Ok(serde_json::from_str(&stdout)?)
}

/// Run `boss …` in human (text) mode and return stdout.
pub fn run_boss_human(socket: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(boss_binary())
        .args(["--no-input", "--no-autostart", "--socket-path", socket])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "boss {} failed (status={:?}):\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// Run `boss --json …` expecting failure; return stderr.
pub fn run_boss_expect_failure(socket: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(boss_binary())
        .args(["--json", "--no-input", "--no-autostart", "--socket-path", socket])
        .args(args)
        .output()?;
    if output.status.success() {
        return Err(anyhow!(
            "boss {} unexpectedly succeeded: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
        ));
    }
    Ok(String::from_utf8(output.stderr)?)
}
