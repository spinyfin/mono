//! Shared spawn helpers for the `gh` CLI subprocess invocations scattered
//! across engine core.
//!
//! Roughly a dozen call sites in `completion`, `merge_poller`, `runner`,
//! `merge_when_ready`, and `design_detector` shell out to `gh` with the
//! exact same stdio envelope: stdin closed, stdout and stderr captured,
//! and `kill_on_drop(true)` so a cancelled future does not leak a child
//! process. Only the post-spawn handling varies — some sites parse stdout
//! as JSON, some compare it as a string, and the error handling ranges
//! from `with_context` through `.ok()?` to bool-on-error graceful
//! degradation.
//!
//! [`gh_output`] is the low-level primitive: it spawns `gh` with the
//! standard envelope and returns the raw [`Output`], leaving every site's
//! tailored success / error handling untouched. [`run_gh`] layers the
//! common happy path on top — trim stdout on success, surface stderr in
//! the error — for the sites that want a `Result<String>`.

use std::process::Output;

use anyhow::{Context, Result, anyhow};
use tokio::process::Command;

/// Spawn a `gh` subprocess with the standard stdio / kill-on-drop envelope
/// (stdin null, stdout+stderr piped, `kill_on_drop(true)`) and return its
/// raw [`Output`].
///
/// This is the shared spawn primitive: it deliberately performs no
/// exit-code or stderr handling so each call site keeps its own tailored
/// logic on top (`with_context`, `.ok()?`, bool-on-error, JSON vs string
/// parsing). The returned `io::Result` is the spawn result — callers apply
/// their own context (`.with_context(...)`, `.ok()?`, `.map_err(...)`).
pub(crate) async fn gh_output(args: &[&str]) -> std::io::Result<Output> {
    Command::new("gh")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
}

/// Spawn `gh` via [`gh_output`] and return the trimmed stdout on success.
/// `display` is a human-readable rendering of the command and is reused in
/// both the spawn-failure context and the non-zero-exit error message
/// (which also carries the captured stderr).
///
/// This is the happy-path convenience for sites that want a
/// `Result<String>` with the conventional "spawn failed" / "command
/// failed: <stderr>" error shape. Sites that need different exit-code
/// handling (graceful degradation, JSON parsing) call [`gh_output`]
/// directly.
pub(crate) async fn run_gh(args: &[&str], display: &str) -> Result<String> {
    let output = gh_output(args)
        .await
        .with_context(|| format!("failed to spawn `{display}`"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`{display}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
