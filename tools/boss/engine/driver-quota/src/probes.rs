//! The three driver probes. One module each, because the three
//! mechanisms have nothing in common beyond the trait they implement —
//! a local slash command, a JSON-RPC handshake, and an HTTPS read.
//! See the crate doc for the table of what each one does and why.

/// Claude quota probe: `claude -p "/usage" --output-format json`.
///
/// `/usage` is one of Claude Code's *local* slash commands. Running it in
/// print mode makes the CLI execute the command and exit — the JSON envelope
/// comes back with `num_turns: 0` and `total_cost_usd: 0`, so the probe costs
/// no model tokens and cannot be confused with dispatched work. The figure in
/// the report is the provider's, fetched by Claude Code from its own usage
/// endpoint; Boss neither computes it nor sees the credential behind it.
pub mod claude {
    use std::path::PathBuf;
    use std::process::Stdio;

    use async_trait::async_trait;
    use boss_protocol::{DRIVER_SLUG_CLAUDE, DriverQuotaFailureKind, DriverQuotaOutcome};

    use crate::DriverQuotaProbe;
    use crate::parse::{parse_claude_usage_json, unavailable};

    /// Probe for the `claude` driver.
    pub struct ClaudeQuotaProbe {
        /// Executable to run. Defaults to `claude` resolved off `PATH`, which is
        /// how every other Boss call site invokes it.
        program: PathBuf,
        /// Working directory for the child. A directory with no project config
        /// keeps the probe from picking up a repo's settings; `/` is always
        /// present and never a Boss workspace.
        cwd: PathBuf,
    }

    impl Default for ClaudeQuotaProbe {
        fn default() -> Self {
            Self {
                program: PathBuf::from("claude"),
                cwd: PathBuf::from("/"),
            }
        }
    }

    /// Environment variables every `claude` child Boss starts must not
    /// inherit. Kept in lockstep with `ClaudeDriver::spawn_invocation`
    /// (`EnvDirective::Unset`): the engine process holds `ANTHROPIC_API_KEY`
    /// for pane-summary LLM calls, but a stray key makes Claude Code meter
    /// API usage instead of the subscription `/usage` reports.
    pub const CLAUDE_UNSET_ENV_VARS: &[&str] = &["ANTHROPIC_API_KEY"];

    impl ClaudeQuotaProbe {
        /// Override the executable — tests point this at a stub script.
        #[cfg(test)]
        pub fn with_program(program: impl Into<PathBuf>) -> Self {
            Self {
                program: program.into(),
                ..Self::default()
            }
        }

        fn usage_command(&self) -> tokio::process::Command {
            let mut command = tokio::process::Command::new(&self.program);
            command
                .arg("-p")
                .arg("/usage")
                .arg("--output-format")
                .arg("json")
                .current_dir(&self.cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            for key in CLAUDE_UNSET_ENV_VARS {
                command.env_remove(*key);
            }
            command
        }
    }

    #[async_trait]
    impl DriverQuotaProbe for ClaudeQuotaProbe {
        fn driver(&self) -> &'static str {
            DRIVER_SLUG_CLAUDE
        }

        async fn probe(&self) -> DriverQuotaOutcome {
            // Remaining environment is inherited so the CLI can resolve
            // OAuth credentials from the macOS keychain / `~/.claude`.
            // `ANTHROPIC_API_KEY` is the exception — see
            // [`CLAUDE_UNSET_ENV_VARS`].
            let output = match self.usage_command().output().await {
                Ok(output) => output,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    return unavailable(
                        DriverQuotaFailureKind::NotInstalled,
                        format!("`{}` is not installed or not on PATH", self.program.display()),
                    );
                }
                Err(err) => {
                    return unavailable(
                        DriverQuotaFailureKind::ProbeFailed,
                        format!("could not run `claude -p /usage`: {err}"),
                    );
                }
            };

            if !output.status.success() {
                // stderr can carry an auth prompt but never a token; still, only
                // the first line is surfaced so a verbose failure cannot spill
                // an unexpected payload into the UI.
                let detail = String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .next()
                    .unwrap_or("no stderr")
                    .trim()
                    .to_owned();
                let kind = if detail.contains("login") || detail.contains("authenticat") {
                    DriverQuotaFailureKind::NotAuthenticated
                } else {
                    DriverQuotaFailureKind::ProbeFailed
                };
                return unavailable(kind, format!("`claude -p /usage` exited non-zero: {detail}"));
            }

            parse_claude_usage_json(&String::from_utf8_lossy(&output.stdout))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use boss_protocol::DriverQuotaOutcome;

        #[tokio::test]
        async fn missing_executable_reports_not_installed_rather_than_a_blank() {
            let probe = ClaudeQuotaProbe::with_program("/nonexistent/definitely-not-claude");
            match probe.probe().await {
                DriverQuotaOutcome::Unavailable { kind, .. } => {
                    assert_eq!(kind, DriverQuotaFailureKind::NotInstalled);
                }
                other => panic!("expected NotInstalled, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn driver_slug_is_claude() {
            assert_eq!(ClaudeQuotaProbe::default().driver(), "claude");
        }

        #[tokio::test]
        async fn probe_unsets_anthropic_api_key_on_the_child() {
            let dir = std::env::temp_dir().join(format!("boss-driver-quota-claude-env-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            let script = dir.join("claude-stub");
            std::fs::write(
                &script,
                "#!/bin/sh\n\
                 if [ -n \"${ANTHROPIC_API_KEY+x}\" ]; then\n\
                   echo 'ANTHROPIC_API_KEY was inherited' >&2\n\
                   exit 1\n\
                 fi\n\
                 printf '%s\\n' '{\"is_error\":false,\"result\":\"Current week (all models): 0% used\"}'\n",
            )
            .expect("write stub");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&script).expect("stat").permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&script, perms).expect("chmod");
            }

            // SAFETY: process-global, but this test is the only one that
            // writes the var, and the assertion is on the child, not on
            // later tests reading it back.
            unsafe {
                std::env::set_var("ANTHROPIC_API_KEY", "probe-must-not-see-this");
            }

            let probe = ClaudeQuotaProbe::with_program(&script);
            match probe.probe().await {
                DriverQuotaOutcome::Reading(reading) => {
                    assert_eq!(reading.used_percent, 0.0);
                }
                other => panic!("child inherited ANTHROPIC_API_KEY (or stub failed): {other:?}"),
            }
            let _ = std::fs::remove_file(&script);
        }

        #[test]
        fn unset_list_matches_the_driver_contract() {
            assert_eq!(CLAUDE_UNSET_ENV_VARS, &["ANTHROPIC_API_KEY"]);
        }
    }
}

/// Codex quota probe: `codex app-server` JSON-RPC `account/rateLimits/read`.
///
/// Codex has no `usage` or `status` subcommand, and its `/status` slash
/// command is TUI-only. Its app-server protocol, however, exposes
/// `account/rateLimits/read` — a first-class, machine-readable method that
/// returns exactly the plan-level figure `/status` renders. The handshake
/// (spawn with `--listen stdio://`, `initialize`, `notifications/initialized`,
/// then the method) lives in `boss_engine_codex_hook_trust::app_server`,
/// the same client the hook-trust gate uses, so the two cannot drift. No thread is started,
/// no turn is taken, and no credential passes through Boss — the child
/// resolves `$CODEX_HOME/auth.json` itself.
pub mod codex {
    use std::path::PathBuf;
    use std::time::Duration;

    use async_trait::async_trait;
    use boss_engine_codex_hook_trust::{AppServer, AppServerError, ClientInfo};
    use boss_protocol::{DRIVER_SLUG_CODEX, DriverQuotaFailureKind, DriverQuotaOutcome};

    use crate::DEFAULT_PROBE_TIMEOUT;
    use crate::DriverQuotaProbe;
    use crate::parse::{parse_codex_rate_limits, unavailable};

    /// The app-server method carrying the plan-level rate limit.
    const METHOD_RATE_LIMITS: &str = "account/rateLimits/read";

    /// Probe for the `codex` driver.
    pub struct CodexQuotaProbe {
        program: PathBuf,
        timeout: Duration,
    }

    impl Default for CodexQuotaProbe {
        fn default() -> Self {
            Self {
                program: PathBuf::from("codex"),
                timeout: DEFAULT_PROBE_TIMEOUT,
            }
        }
    }

    impl CodexQuotaProbe {
        /// Override the executable — tests point this at a stub.
        #[cfg(test)]
        pub fn with_program(program: impl Into<PathBuf>) -> Self {
            Self {
                program: program.into(),
                timeout: DEFAULT_PROBE_TIMEOUT,
            }
        }
    }

    #[async_trait]
    impl DriverQuotaProbe for CodexQuotaProbe {
        fn driver(&self) -> &'static str {
            DRIVER_SLUG_CODEX
        }

        async fn probe(&self) -> DriverQuotaOutcome {
            let program = self.program.clone();
            let timeout = self.timeout;
            let result = tokio::task::spawn_blocking(move || {
                AppServer {
                    program,
                    extra_env: Vec::new(),
                    cwd: None,
                    client_info: ClientInfo {
                        name: "boss-quota-probe".into(),
                        title: Some("Boss quota probe".into()),
                        version: "1.0.0".into(),
                    },
                    timeout,
                }
                .request(METHOD_RATE_LIMITS, serde_json::json!({}))
            })
            .await;

            match result {
                Ok(Ok(value)) => parse_codex_rate_limits(&value),
                Ok(Err(AppServerError::NotFound { program })) => unavailable(
                    DriverQuotaFailureKind::NotInstalled,
                    format!("`{}` is not installed or not on PATH", program.display()),
                ),
                Ok(Err(AppServerError::Rpc { message, .. })) => {
                    let kind = if message.contains("auth") || message.contains("login") {
                        DriverQuotaFailureKind::NotAuthenticated
                    } else {
                        DriverQuotaFailureKind::ProbeFailed
                    };
                    unavailable(kind, format!("codex {METHOD_RATE_LIMITS} failed: {message}"))
                }
                Ok(Err(err)) => unavailable(DriverQuotaFailureKind::ProbeFailed, err.to_string()),
                Err(err) => unavailable(
                    DriverQuotaFailureKind::ProbeFailed,
                    format!("codex app-server task failed: {err}"),
                ),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn missing_executable_reports_not_installed_rather_than_a_blank() {
            let probe = CodexQuotaProbe::with_program("/nonexistent/definitely-not-codex");
            match probe.probe().await {
                DriverQuotaOutcome::Unavailable { kind, .. } => {
                    assert_eq!(kind, DriverQuotaFailureKind::NotInstalled);
                }
                other => panic!("expected NotInstalled, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn driver_slug_is_codex() {
            assert_eq!(CodexQuotaProbe::default().driver(), "codex");
        }
    }
}

/// Grok quota probe: an HTTPS read of the endpoint the CLI's own `/usage`
/// calls.
///
/// Grok is the one driver with no non-interactive route through its CLI.
/// `/usage` is an in-TUI extension with no subcommand equivalent, and headless
/// `grok -p "/usage"` does not treat the text as a command at all — it sends
/// it to the model as a prompt, which costs tokens and answers with prose.
/// So the probe calls the endpoint the extension itself calls.
///
/// # Credential handling
///
/// This is the only probe where Boss touches a token. The rules it follows:
///
/// - it reads the *same* `auth.json` the driver reads (path supplied by the
///   caller from `boss_engine_driver::grok::resolve_grok_auth_source`) — the
///   credential is never copied, duplicated, or relocated;
/// - the token is held only for the duration of one request;
/// - it is never logged, never returned, and never placed in a
///   [`DriverQuotaOutcome`] reason. Failure reasons here are written by hand
///   for that reason: no provider body and no header value is echoed.
pub mod grok {
    use std::path::PathBuf;

    use async_trait::async_trait;
    use boss_protocol::{DRIVER_SLUG_GROK, DriverQuotaFailureKind, DriverQuotaOutcome};

    use crate::DriverQuotaProbe;
    use crate::parse::{parse_grok_billing, unavailable};

    /// Default base URL of the CLI chat proxy — the value compiled into the Grok
    /// CLI itself.
    const DEFAULT_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";

    /// Environment override the Grok CLI honours for its proxy base URL. Read
    /// here too so a maintainer pointed at a different proxy gets the quota from
    /// the same place their driver talks to.
    const BASE_URL_ENV: &str = "GROK_CLI_CHAT_PROXY_BASE_URL";

    /// Path and query the CLI's billing extension requests.
    const BILLING_PATH: &str = "/billing?format=credits";

    /// Honest identification. Deliberately not an impersonation of the Grok CLI's
    /// own agent string — the endpoint accepts this, and pretending to be another
    /// client would be the wrong thing for Boss to do.
    const USER_AGENT: &str = "boss-quota-probe/1.0";

    /// Probe for the `grok` driver.
    pub struct GrokQuotaProbe {
        /// Path to the Grok `auth.json` the driver reads, resolved by the
        /// caller through the driver's own resolution so both read the
        /// identical file.
        auth_path: PathBuf,
        base_url: String,
    }

    impl GrokQuotaProbe {
        /// Build a probe reading the given `auth.json`. The base URL comes from
        /// [`BASE_URL_ENV`] when set, matching the CLI's own override.
        pub fn new(auth_path: impl Into<PathBuf>) -> Self {
            let base_url = std::env::var(BASE_URL_ENV)
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
            Self {
                auth_path: auth_path.into(),
                base_url,
            }
        }

        /// Explicit base URL — tests point this at a local server.
        #[cfg(test)]
        pub fn with_base_url(auth_path: impl Into<PathBuf>, base_url: impl Into<String>) -> Self {
            Self {
                auth_path: auth_path.into(),
                base_url: base_url.into(),
            }
        }

        fn billing_url(&self) -> String {
            format!("{}{BILLING_PATH}", self.base_url.trim_end_matches('/'))
        }
    }

    /// Why a bearer token could not be produced. Carries no credential material.
    struct TokenError {
        kind: DriverQuotaFailureKind,
        reason: String,
    }

    /// Pull the bearer token out of a Grok `auth.json` document.
    ///
    /// The file maps an issuer-scoped account key to a record whose `key` field
    /// holds the bearer. More than one account can be present; the entry with the
    /// furthest-future `expires_at` wins, which is the live one after a re-login
    /// under a second issuer. RFC-3339 timestamps compare correctly as strings
    /// when they share an offset, and these are all written by the same CLI.
    ///
    /// Returns the token by value. Callers must not log or store it.
    fn extract_bearer(document: &serde_json::Value) -> Result<String, TokenError> {
        let Some(accounts) = document.as_object() else {
            return Err(TokenError {
                kind: DriverQuotaFailureKind::NotAuthenticated,
                reason: "Grok auth.json was not a JSON object — sign in with `grok login`".to_owned(),
            });
        };
        let mut best: Option<(&str, &str)> = None;
        for record in accounts.values() {
            let Some(key) = record.get("key").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if key.is_empty() {
                continue;
            }
            let expires = record
                .get("expires_at")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            match best {
                Some((best_expires, _)) if best_expires >= expires => {}
                _ => best = Some((expires, key)),
            }
        }
        match best {
            Some((expires, _)) if token_expired(expires) => Err(TokenError {
                kind: DriverQuotaFailureKind::NotAuthenticated,
                reason: "stored Grok token has expired — run any grok command or `grok login` to refresh".to_owned(),
            }),
            Some((_, key)) => Ok(key.to_owned()),
            None => Err(TokenError {
                kind: DriverQuotaFailureKind::NotAuthenticated,
                reason: "Grok auth.json has no signed-in account — sign in with `grok login`".to_owned(),
            }),
        }
    }

    /// `true` when `expires_at` parses as RFC-3339 and is strictly in the
    /// past. Missing or unparseable timestamps are treated as live so a
    /// document without an expiry still produces a request rather than a
    /// false "expired" label.
    fn token_expired(expires_at: &str) -> bool {
        if expires_at.is_empty() {
            return false;
        }
        match chrono::DateTime::parse_from_rfc3339(expires_at) {
            Ok(dt) => dt.timestamp() < crate::now_epoch_s(),
            Err(_) => false,
        }
    }

    /// Read and parse the auth document, mapping IO problems onto the failure
    /// kinds the UI distinguishes.
    fn read_auth_document(path: &std::path::Path) -> Result<serde_json::Value, TokenError> {
        let raw = std::fs::read_to_string(path).map_err(|err| TokenError {
            kind: if err.kind() == std::io::ErrorKind::NotFound {
                DriverQuotaFailureKind::NotAuthenticated
            } else {
                DriverQuotaFailureKind::ProbeFailed
            },
            // The path is Boss-derived, not credential material; the OS error
            // string is not interpolated so a future error variant cannot leak
            // file content.
            reason: format!("could not read Grok credentials at {}", path.display()),
        })?;
        serde_json::from_str(&raw).map_err(|_| TokenError {
            kind: DriverQuotaFailureKind::ProbeFailed,
            // Deliberately no parser detail: serde error messages quote the
            // offending input, which here is a credential file.
            reason: "Grok auth.json could not be parsed".to_owned(),
        })
    }

    #[async_trait]
    impl DriverQuotaProbe for GrokQuotaProbe {
        fn driver(&self) -> &'static str {
            DRIVER_SLUG_GROK
        }

        async fn probe(&self) -> DriverQuotaOutcome {
            let bearer = match read_auth_document(&self.auth_path).and_then(|doc| extract_bearer(&doc)) {
                Ok(bearer) => bearer,
                Err(TokenError { kind, reason }) => return unavailable(kind, reason),
            };

            let response = boss_http_retry::http_client()
                .get(self.billing_url())
                .header("Authorization", format!("Bearer {bearer}"))
                .header("Accept", "application/json")
                .header("User-Agent", USER_AGENT)
                .send()
                .await;

            let response = match response {
                Ok(response) => response,
                // reqwest's Display includes the URL but never a request header,
                // so the bearer cannot reach this string.
                Err(_) => {
                    return unavailable(
                        DriverQuotaFailureKind::ProbeFailed,
                        "could not reach the Grok billing endpoint".to_owned(),
                    );
                }
            };

            let status = response.status();
            if !status.is_success() {
                let kind = if status.as_u16() == 401 || status.as_u16() == 403 {
                    DriverQuotaFailureKind::NotAuthenticated
                } else {
                    DriverQuotaFailureKind::ProbeFailed
                };
                // Status code only — the body is not echoed.
                return unavailable(kind, format!("Grok billing endpoint answered HTTP {}", status.as_u16()));
            }

            match response.text().await {
                Ok(body) => parse_grok_billing(&body),
                Err(_) => unavailable(
                    DriverQuotaFailureKind::ProbeFailed,
                    "Grok billing response could not be read".to_owned(),
                ),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn bearer_taken_from_the_only_account() {
            let doc = serde_json::json!({
                "https://auth.example::acct": { "key": "token-a", "expires_at": "2099-09-01T00:00:00Z" }
            });
            assert_eq!(extract_bearer(&doc).ok(), Some("token-a".to_owned()));
        }

        #[test]
        fn furthest_future_account_wins_when_several_are_present() {
            let doc = serde_json::json!({
                "issuer-a::1": { "key": "old", "expires_at": "2099-01-01T00:00:00Z" },
                "issuer-b::2": { "key": "new", "expires_at": "2099-09-01T00:00:00Z" },
            });
            assert_eq!(extract_bearer(&doc).ok(), Some("new".to_owned()));
        }

        #[test]
        fn empty_document_reads_as_not_authenticated() {
            let err = extract_bearer(&serde_json::json!({})).unwrap_err();
            assert_eq!(err.kind, DriverQuotaFailureKind::NotAuthenticated);
        }

        #[test]
        fn expired_token_is_named_as_expired_not_unsigned() {
            let doc = serde_json::json!({
                "issuer-a::1": { "key": "old", "expires_at": "2020-01-01T00:00:00Z" }
            });
            let err = extract_bearer(&doc).unwrap_err();
            assert_eq!(err.kind, DriverQuotaFailureKind::NotAuthenticated);
            assert!(
                err.reason.contains("expired"),
                "reason must name expiry, got {}",
                err.reason
            );
        }

        #[test]
        fn record_without_a_key_is_skipped_rather_than_treated_as_a_token() {
            let doc = serde_json::json!({ "issuer::1": { "expires_at": "2099-09-01T00:00:00Z" } });
            assert!(extract_bearer(&doc).is_err());
        }

        #[test]
        fn non_object_document_reads_as_not_authenticated() {
            let err = extract_bearer(&serde_json::json!([])).unwrap_err();
            assert_eq!(err.kind, DriverQuotaFailureKind::NotAuthenticated);
        }

        #[test]
        fn missing_auth_file_reads_as_not_authenticated_not_a_blank() {
            let err = read_auth_document(std::path::Path::new("/nonexistent/grok/auth.json")).unwrap_err();
            assert_eq!(err.kind, DriverQuotaFailureKind::NotAuthenticated);
        }

        #[test]
        fn unparseable_auth_file_reason_does_not_quote_its_contents() {
            let dir = std::env::temp_dir().join("boss-driver-quota-grok-test");
            std::fs::create_dir_all(&dir).expect("temp dir");
            let path = dir.join("auth.json");
            std::fs::write(&path, "{ not json: super-secret-token").expect("write");
            let err = read_auth_document(&path).unwrap_err();
            assert!(
                !err.reason.contains("super-secret-token"),
                "credential-bearing file content must never reach a reason string: {}",
                err.reason
            );
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn billing_url_is_the_endpoint_the_cli_calls() {
            let probe = GrokQuotaProbe::with_base_url("/tmp/auth.json", "https://example.test/v1");
            assert_eq!(probe.billing_url(), "https://example.test/v1/billing?format=credits");
        }

        #[test]
        fn trailing_slash_on_the_base_url_does_not_double_up() {
            let probe = GrokQuotaProbe::with_base_url("/tmp/auth.json", "https://example.test/v1/");
            assert_eq!(probe.billing_url(), "https://example.test/v1/billing?format=credits");
        }

        #[test]
        fn driver_slug_is_grok() {
            assert_eq!(GrokQuotaProbe::new("/tmp/auth.json").driver(), "grok");
        }
    }
}
