//! The Grok CLI's own non-interactive credential pass, run against the shared
//! OAuth credential so an expired-but-refreshable stored token is rewritten by
//! the CLI itself.
//!
//! # Why this exists
//!
//! The Grok CLI stores a short-lived access token (observed: six hours) next
//! to a refresh token in `auth.json`, and refreshes the access token lazily —
//! on its next API call, not on a timer. So the *file* says "expired" for most
//! of the day while the CLI, asked, would happily say it is logged in. Any
//! Boss surface that judges the stored token by its `expires_at` alone (the
//! driver-quota panel) therefore has to ask the CLI to perform that refresh
//! before it can report "not signed in" truthfully.
//!
//! # Why `grok models`
//!
//! It is the one CLI subcommand that exercises the credential without an
//! inference call, and the same probe the worker preflight already relies on
//! (see [`super::preflight`]). Observed on `grok 1.0.70`:
//!
//! - a usable credential prints [`GROK_LOGIN_AFFIRMATION`]; an expired but
//!   refreshable token is rewritten in `auth.json`, possibly with a stale
//!   unauthenticated banner until the next invocation;
//! - no credential, or a refresh token the issuer rejects, prints
//!   "You are not authenticated." and exits 0 — it never opens a browser or
//!   starts a device-code flow, so it is safe to run from a headless engine;
//! - an inherited `XAI_API_KEY` makes it print "You are using XAI_API_KEY."
//!   and skip the OAuth credential entirely, which is why the OAuth-only
//!   environment belt from [`super::environment`] is mandatory here.
//!
//! # What this never does
//!
//! It reads only credential metadata, never credential bytes, and never writes
//! the file. It waits for the CLI's refresh lock and re-probes once if an
//! unaffirmed pass changes that metadata, within the same time budget.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, bail};

use super::environment::{oauth_only_directives, shared_credential_directive, wait_for_grok_auth_refresh_with_timeout};
use super::preflight::run_bounded_as;
use crate::EnvDirective;

/// The line `grok models` prints when the shared credential is usable (after
/// refreshing it if it had to). Shared with the worker preflight's assertion.
pub const GROK_LOGIN_AFFIRMATION: &str = "You are logged in with grok.com.";

/// Prefix on this module's errors, so a failure reads as what it is.
const FAILURE_PREFIX: &str = "Grok credential refresh failed";

/// Longest banner line carried into a report. Banners are one short sentence;
/// anything longer is not a banner and is cut rather than echoed.
const DETAIL_MAX_CHARS: usize = 160;

/// What the CLI said about the shared credential after its pass. Carries no
/// credential material: `detail` is the CLI's first banner line, never file
/// content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrokOauthLiveness {
    /// The CLI affirmed a grok.com login. If the stored token had expired,
    /// `auth.json` now holds the refreshed one.
    LoggedIn,
    /// The CLI ran to completion but did not affirm a login — no credential,
    /// or a refresh token the issuer no longer accepts.
    NotLoggedIn { detail: String },
}

/// Cheap, content-blind snapshot of the shared OAuth credential's on-disk
/// state, used to detect whether a refresh rewrote the file during the
/// probe. Never reads the credential bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AuthFingerprint {
    modified: SystemTime,
    len: u64,
}

fn fingerprint_auth_credential(auth_path: &Path) -> Option<AuthFingerprint> {
    let metadata = std::fs::metadata(auth_path).ok()?;
    let modified = metadata.modified().ok()?;
    Some(AuthFingerprint {
        modified,
        len: metadata.len(),
    })
}

/// Re-probe once when an unaffirmed pass changes the credential metadata.
/// The CLI can render its banner before refreshing the credential.
pub(super) fn with_credential_reprobe<T>(
    auth_path: &Path,
    mut run: impl FnMut() -> anyhow::Result<T>,
    affirmed: impl Fn(&T) -> bool,
) -> anyhow::Result<T> {
    let before = fingerprint_auth_credential(auth_path);
    let output = run()?;
    if !affirmed(&output) && before != fingerprint_auth_credential(auth_path) {
        tracing::info!(
            auth_path = %auth_path.display(),
            "Grok OAuth credential was rewritten during an unaffirmed `grok models` pass; re-probing once"
        );
        return run();
    }
    Ok(output)
}

/// Ask the Grok CLI to exercise (and, if needed, refresh) the shared OAuth
/// credential at `auth_path`, within `budget` of wall-clock time.
///
/// Blocking: waits for the CLI's own refresh lock, then runs `grok models`
/// under the OAuth-only environment belt and a process-group-killing timeout.
/// Callers on an async runtime must use `spawn_blocking`. `Err` means the CLI
/// could not be run to completion (not installed, non-zero exit, timed out,
/// budget spent waiting on a live refresh lock) — it says nothing about
/// whether a login is available.
pub fn refresh_shared_oauth_credential_blocking(
    auth_path: &Path,
    budget: Duration,
) -> anyhow::Result<GrokOauthLiveness> {
    refresh_with_program(Path::new("grok"), auth_path, budget)
}

fn refresh_with_program(program: &Path, auth_path: &Path, budget: Duration) -> anyhow::Result<GrokOauthLiveness> {
    let started = Instant::now();
    wait_for_grok_auth_refresh_with_timeout(auth_path, budget)
        .with_context(|| format!("{FAILURE_PREFIX}: waiting for a live Grok refresh to release its lock"))?;
    let remaining = budget.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        bail!(
            "{FAILURE_PREFIX}: spent the whole {}s budget waiting for a live Grok refresh lock at {}",
            budget.as_secs(),
            auth_path.display()
        );
    }

    with_credential_reprobe(
        auth_path,
        || {
            let remaining = budget.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                bail!("{FAILURE_PREFIX}: credential pass exhausted its time budget");
            }
            let mut command = credential_pass_command(program, auth_path);
            let output = run_bounded_as(&mut command, remaining, FAILURE_PREFIX)?;
            classify(output.status.success(), &output.stdout, &output.stderr)
        },
        |outcome| matches!(outcome, GrokOauthLiveness::LoggedIn),
    )
}

/// `grok models` against the shared credential, OAuth only, no terminal.
///
/// The working directory is `/`: it is never a Boss workspace, so the CLI
/// picks up no project configuration, and it always exists. `HOME` and
/// `GROK_HOME` are inherited so the pass refreshes the same credential as
/// running `grok models` by hand.
fn credential_pass_command(program: &Path, auth_path: &Path) -> Command {
    let mut command = Command::new(program);
    command.arg("models").current_dir("/").stdin(Stdio::null());
    for directive in std::iter::once(shared_credential_directive(auth_path)).chain(oauth_only_directives()) {
        match directive {
            EnvDirective::Set(key, value) => {
                command.env(key, value);
            }
            EnvDirective::Unset(key) => {
                command.env_remove(key);
            }
        }
    }
    command
}

/// Turn the CLI's exit and banner into a report. Exit zero plus the
/// affirmation is a login; exit zero without it is "not logged in" carrying
/// the banner; non-zero exit is a failure to run the pass at all.
fn classify(success: bool, stdout: &[u8], stderr: &[u8]) -> anyhow::Result<GrokOauthLiveness> {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    if !success {
        bail!(
            "{FAILURE_PREFIX}: `grok models` exited non-zero: {}",
            first_line(&stderr)
                .or_else(|| first_line(&stdout))
                .unwrap_or_else(|| "no output".to_owned())
        );
    }
    if stdout.contains(GROK_LOGIN_AFFIRMATION) {
        return Ok(GrokOauthLiveness::LoggedIn);
    }
    Ok(GrokOauthLiveness::NotLoggedIn {
        detail: first_line(&stdout)
            .or_else(|| first_line(&stderr))
            .unwrap_or_else(|| "`grok models` printed no authentication banner".to_owned()),
    })
}

fn first_line(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    Some(line.chars().take(DETAIL_MAX_CHARS).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn affirmation_on_exit_zero_is_logged_in() {
        let outcome = classify(
            true,
            b"You are logged in with grok.com.\n\nDefault model: grok-4.7\n",
            b"",
        )
        .unwrap();
        assert_eq!(outcome, GrokOauthLiveness::LoggedIn);
    }

    #[test]
    fn exit_zero_without_affirmation_is_not_logged_in_and_carries_the_banner() {
        let outcome = classify(true, b"You are not authenticated.\n\nDefault model: grok-4.6\n", b"").unwrap();
        assert_eq!(
            outcome,
            GrokOauthLiveness::NotLoggedIn {
                detail: "You are not authenticated.".to_owned()
            }
        );
    }

    #[test]
    fn api_key_banner_is_reported_not_mistaken_for_a_login() {
        let outcome = classify(true, b"You are using XAI_API_KEY.\n", b"").unwrap();
        assert_eq!(
            outcome,
            GrokOauthLiveness::NotLoggedIn {
                detail: "You are using XAI_API_KEY.".to_owned()
            }
        );
    }

    #[test]
    fn non_zero_exit_is_an_error_naming_the_first_stderr_line() {
        let err = classify(false, b"", b"\nerror: something broke\nmore\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("exited non-zero"), "{err}");
        assert!(err.contains("error: something broke"), "{err}");
        assert!(!err.contains("more"), "{err}");
    }

    #[test]
    fn banner_detail_is_truncated_rather_than_echoed_whole() {
        let long = "x".repeat(1000);
        let GrokOauthLiveness::NotLoggedIn { detail } = classify(true, long.as_bytes(), b"").unwrap() else {
            panic!("expected NotLoggedIn");
        };
        assert_eq!(detail.chars().count(), DETAIL_MAX_CHARS);
    }

    /// The belt that makes the pass meaningful: the child must see the shared
    /// credential path and must not see an API key, or the CLI never touches
    /// the OAuth credential at all.
    #[test]
    fn pass_runs_models_under_the_oauth_only_belt() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        fs::write(&auth, "{}").unwrap();
        let program = stub(
            dir.path(),
            "grok",
            r#"[ "$1" = "models" ] || { echo "unexpected args: $*" >&2; exit 2; }
[ -z "${XAI_API_KEY+x}" ] || { echo "XAI_API_KEY leaked" >&2; exit 3; }
[ -z "${GROK_AUTH+x}" ] || { echo "GROK_AUTH leaked" >&2; exit 4; }
[ "$GROK_DISABLE_API_KEY_AUTH" = "1" ] || { echo "api-key auth not disabled" >&2; exit 5; }
[ "$GROK_AUTH_PATH" = "$EXPECTED_AUTH_PATH" ] || { echo "GROK_AUTH_PATH=$GROK_AUTH_PATH" >&2; exit 6; }
[ "$(pwd)" = "/" ] || { echo "cwd=$(pwd)" >&2; exit 7; }
echo "You are logged in with grok.com."
echo
echo "Default model: grok-4.7""#,
        );
        let _lock = super::super::home::GROK_HOMES_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prior: Vec<(&str, Option<std::ffi::OsString>)> = ["EXPECTED_AUTH_PATH", "XAI_API_KEY", "GROK_AUTH"]
            .into_iter()
            .map(|key| (key, std::env::var_os(key)))
            .collect();
        // SAFETY: serialised by GROK_HOMES_ENV_TEST_LOCK and restored below;
        // the stub reads these back and the engine never sets them.
        unsafe {
            std::env::set_var("EXPECTED_AUTH_PATH", auth.display().to_string());
            std::env::set_var("XAI_API_KEY", "must-not-reach-the-child");
            std::env::set_var("GROK_AUTH", "must-not-reach-the-child");
        }
        let outcome = refresh_with_program(&program, &auth, Duration::from_secs(10));
        // SAFETY: same lock; restores the pre-test values.
        unsafe {
            for (key, value) in prior {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
        assert_eq!(outcome.unwrap(), GrokOauthLiveness::LoggedIn);
    }

    #[test]
    fn repeated_rewrites_do_not_trigger_a_third_pass() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        fs::write(&auth, "{}").unwrap();
        let mut calls = 0;
        let outcome = with_credential_reprobe(
            &auth,
            || {
                calls += 1;
                fs::write(&auth, "x".repeat(calls + 2))?;
                Ok(false)
            },
            |affirmed| *affirmed,
        )
        .unwrap();
        assert!(!outcome);
        assert_eq!(calls, 2);
    }

    #[test]
    fn stale_banner_after_rewrite_is_reprobed_once() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        fs::write(&auth, "{}").unwrap();
        let program = stub(
            dir.path(),
            "grok",
            r#"
echo call >> "$GROK_AUTH_PATH.calls"
if [ "$(cat "$GROK_AUTH_PATH")" = "{}" ]; then
    echo refreshed > "$GROK_AUTH_PATH"
    echo "You are not authenticated."
else
    echo "You are logged in with grok.com."
fi"#,
        );
        assert_eq!(
            refresh_with_program(&program, &auth, Duration::from_secs(10)).unwrap(),
            GrokOauthLiveness::LoggedIn
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("auth.json.calls")).unwrap(),
            "call\ncall\n"
        );
    }

    #[test]
    fn a_not_authenticated_pass_is_a_report_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        fs::write(&auth, "{}").unwrap();
        let program = stub(
            dir.path(),
            "grok",
            r#"echo call >> "$GROK_AUTH_PATH.calls"
echo "You are not authenticated.""#,
        );
        let outcome = refresh_with_program(&program, &auth, Duration::from_secs(10)).unwrap();
        assert_eq!(
            outcome,
            GrokOauthLiveness::NotLoggedIn {
                detail: "You are not authenticated.".to_owned()
            }
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("auth.json.calls")).unwrap(),
            "call\n"
        );
    }

    #[test]
    fn a_missing_cli_is_an_error_not_a_verdict_on_the_login() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        fs::write(&auth, "{}").unwrap();
        let err = refresh_with_program(
            Path::new("/nonexistent/definitely-not-grok"),
            &auth,
            Duration::from_secs(5),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains(FAILURE_PREFIX), "{err}");
    }

    #[test]
    fn a_hung_cli_is_killed_at_the_budget_and_named() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        fs::write(&auth, "{}").unwrap();
        let program = stub(dir.path(), "grok", "sleep 60");
        let started = Instant::now();
        let err = refresh_with_program(&program, &auth, Duration::from_millis(300))
            .unwrap_err()
            .to_string();
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "budget did not bound the call"
        );
        assert!(err.contains(FAILURE_PREFIX), "{err}");
        assert!(err.contains("did not complete within"), "{err}");
        assert!(!err.contains("preflight"), "{err}");
    }

    /// A live refresh lock held by this very process (so the pid is alive and
    /// the stamp fresh) must consume the budget and fail, never run the CLI
    /// against a file another process is rewriting.
    #[test]
    fn a_live_fresh_lock_that_never_clears_spends_the_budget_without_running_the_cli() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        fs::write(&auth, "{}").unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        fs::write(
            dir.path().join("auth.json.lock"),
            format!("{}:{now}", std::process::id()),
        )
        .unwrap();
        let marker = dir.path().join("cli-ran");
        let program = stub(dir.path(), "grok", &format!("touch {}", marker.display()));
        // The lock wait judges "fresh" in whole seconds against the budget, so
        // a sub-second budget would read a stamp written a few milliseconds
        // ago as aged whenever a second boundary falls in between. Two seconds
        // keeps the test deterministic and still quick.
        let started = Instant::now();
        let result = refresh_with_program(&program, &auth, Duration::from_secs(2));
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "budget did not bound the wait"
        );
        assert!(
            !marker.exists(),
            "the CLI must not run while a live refresh holds the lock"
        );
        // The lock wait itself logs and returns Ok at the deadline, so the
        // budget check is what turns this into a failure.
        let err = result.unwrap_err().to_string();
        assert!(err.contains("waiting for a live Grok refresh lock"), "{err}");
    }
}
