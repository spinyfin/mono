//! `boss-event` — a thin stdin-to-Unix-socket shim invoked by claude
//! hooks running inside a Boss-managed worker.
//!
//! Each claude hook is configured (via the engine's per-lease
//! `settings.json` template) to spawn this binary, with the hook
//! payload arriving on stdin. The shim reads stdin to EOF, opens the
//! engine's events socket at `$BOSS_EVENTS_SOCKET`, writes the
//! payload, and exits.
//!
//! The shim is intentionally minimal: no parsing, no retries, no
//! framing. The engine derives the worker's lease via `LOCAL_PEERPID`
//! on its side, so the shim doesn't need to embed the lease id, only
//! the raw hook JSON.
//!
//! Hooks fire on the worker's hot path; staying small and synchronous
//! keeps the per-hook overhead trivial. If the socket isn't reachable
//! (engine restarting, upgrading) we fail loudly with a non-zero exit
//! code so claude logs the hook failure rather than silently dropping
//! events.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow};

const SOCKET_ENV: &str = "BOSS_EVENTS_SOCKET";
const RUN_ID_ENV: &str = "BOSS_RUN_ID";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("boss-event: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<()> {
    let socket_path = std::env::var(SOCKET_ENV)
        .map_err(|_| anyhow!("{SOCKET_ENV} not set; refusing to deliver hook event"))?;

    let mut payload = Vec::new();
    io::stdin()
        .read_to_end(&mut payload)
        .context("reading hook payload from stdin")?;

    if payload.is_empty() {
        return Err(anyhow!("hook payload on stdin was empty"));
    }

    // If `BOSS_RUN_ID` is set in the worker's env, splice it into the
    // hook JSON object so the engine can correlate this event to the
    // run without needing a working shell-pid lookup. We only rewrite
    // when both inputs are valid; on any failure (env not set, payload
    // not a JSON object) we forward the original bytes unchanged so
    // the shim stays best-effort and never blocks the worker.
    let payload = match maybe_splice_run_id(&payload) {
        Ok(bytes) => bytes,
        Err(_) => payload,
    };

    let mut stream = UnixStream::connect(&socket_path)
        .with_context(|| format!("connecting to events socket at {socket_path}"))?;

    stream
        .write_all(&payload)
        .context("writing hook payload to events socket")?;

    // Half-close on our end signals end-of-message to the engine.
    stream
        .shutdown(std::net::Shutdown::Write)
        .context("shutting down write half of events socket")?;

    Ok(())
}

/// Inject `_boss_run_id` into a hook JSON object payload when the env
/// is present and the payload parses as a JSON object. Returns the
/// rewritten bytes, or `Err` so the caller can fall back to forwarding
/// the original payload untouched (the shim must never silently drop
/// a hook event just because the env-id splicing failed).
fn maybe_splice_run_id(payload: &[u8]) -> Result<Vec<u8>> {
    let run_id = std::env::var(RUN_ID_ENV).context("BOSS_RUN_ID not set")?;
    if run_id.is_empty() {
        return Err(anyhow!("BOSS_RUN_ID is empty"));
    }
    let mut value: serde_json::Value =
        serde_json::from_slice(payload).context("hook payload was not JSON")?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("hook payload was not a JSON object"))?;
    object.insert(
        "_boss_run_id".to_owned(),
        serde_json::Value::String(run_id),
    );
    Ok(serde_json::to_vec(&value)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_env<R>(key: &str, value: Option<&str>, f: impl FnOnce() -> R) -> R {
        let prior = std::env::var(key).ok();
        // SAFETY: tests serialize on the env (see `set_var` warning).
        // Each test uses the env synchronously inside `f`.
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        let out = f();
        unsafe {
            match prior {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        out
    }

    #[test]
    fn splice_inserts_run_id_into_object_payload() {
        with_env(RUN_ID_ENV, Some("run-xyz"), || {
            let payload = br#"{"hook_event_name":"PreToolUse","tool_name":"Bash"}"#;
            let result = maybe_splice_run_id(payload).unwrap();
            let parsed: serde_json::Value = serde_json::from_slice(&result).unwrap();
            assert_eq!(parsed["_boss_run_id"], "run-xyz");
            // Original fields must survive.
            assert_eq!(parsed["hook_event_name"], "PreToolUse");
            assert_eq!(parsed["tool_name"], "Bash");
        });
    }

    #[test]
    fn splice_errors_when_env_missing_so_caller_falls_back() {
        with_env(RUN_ID_ENV, None, || {
            let payload = br#"{"hook_event_name":"PreToolUse"}"#;
            assert!(maybe_splice_run_id(payload).is_err());
        });
    }

    #[test]
    fn splice_errors_when_env_empty() {
        with_env(RUN_ID_ENV, Some(""), || {
            let payload = br#"{"hook_event_name":"PreToolUse"}"#;
            assert!(maybe_splice_run_id(payload).is_err());
        });
    }

    #[test]
    fn splice_errors_when_payload_not_a_json_object() {
        with_env(RUN_ID_ENV, Some("run-xyz"), || {
            assert!(maybe_splice_run_id(b"not json at all").is_err());
            assert!(maybe_splice_run_id(b"\"a string\"").is_err());
            assert!(maybe_splice_run_id(b"[1,2,3]").is_err());
        });
    }
}
