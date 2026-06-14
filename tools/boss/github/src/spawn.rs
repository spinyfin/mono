//! Shared `gh` subprocess spawn primitives.
//!
//! Every engine surface that shells out to `gh` uses the identical stdio
//! envelope: stdin null, stdout+stderr piped, `kill_on_drop(true)`. Keeping
//! that setup in one place (here, in the shared `boss-github` crate) lets
//! both the engine and the Contents helper reuse it without duplication.

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
pub async fn gh_output(args: &[&str]) -> std::io::Result<Output> {
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
pub async fn run_gh(args: &[&str], display: &str) -> Result<String> {
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
