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

            parse_claude_usage_json(&String::from_utf8_lossy(&output.stdout), crate::now_epoch_s())
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
///   for that reason: no provider body and no header value is echoed;
/// - Boss never *writes* the credential. When the stored access token has
///   expired, the probe asks the Grok CLI to refresh it through the CLI's own
///   non-interactive path (a [`GrokCredentialRefresher`]), then re-reads the
///   file the CLI rewrote.
///
/// # Why a refresh step at all
///
/// The CLI stores a short-lived access token (observed: six hours) beside a
/// refresh token, and renews it lazily — on its next API call, never on a
/// timer. Judged by `expires_at` alone, the stored token therefore reads as
/// expired for most of any day on which `grok` has not been run.
/// The refresh step lets the CLI renew that token before the panel reports
/// sign-in state. "Not signed in" means no account, no refresh token, or a
/// refresh the issuer rejected.
pub mod grok {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

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

    /// The Grok CLI's own credential-refresh path, as the probe sees it.
    ///
    /// The production implementation runs `grok models` against the shared
    /// credential (it is the CLI subcommand that exercises — and, when the
    /// token has expired, refreshes — the credential without an inference
    /// call, and the same probe the worker preflight relies on). The contract
    /// an implementation must keep:
    ///
    /// - it never opens a browser, starts a device-code flow, or prompts;
    /// - it never reads, logs, or writes the credential file itself — the
    ///   CLI does any rewriting;
    /// - the `detail` strings it returns are CLI banner text, never file
    ///   content, because they reach the panel.
    #[async_trait]
    pub trait GrokCredentialRefresher: Send + Sync {
        /// Run the CLI's credential pass to completion and report what it said.
        async fn refresh(&self) -> GrokRefreshOutcome;
    }

    /// What the CLI's credential pass reported. Carries no credential material.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum GrokRefreshOutcome {
        /// The CLI affirmed a grok.com login; `auth.json` now holds whatever
        /// it wrote (a refreshed token, or the old one if it was still good).
        LoggedIn,
        /// The CLI ran but did not affirm a login — logged out, or a refresh
        /// token the issuer no longer accepts.
        NotLoggedIn { detail: String },
        /// The CLI could not be run to completion (not installed, crashed,
        /// timed out). Says nothing about whether a login is available.
        Failed { detail: String },
    }

    /// Probe for the `grok` driver.
    pub struct GrokQuotaProbe {
        /// Path to the Grok `auth.json` the driver reads, resolved by the
        /// caller through the driver's own resolution so both read the
        /// identical file.
        auth_path: PathBuf,
        base_url: String,
        /// The CLI's refresh path, invoked at most once per probe.
        refresher: Arc<dyn GrokCredentialRefresher>,
    }

    impl GrokQuotaProbe {
        /// Build a probe reading the given `auth.json`. The base URL comes from
        /// [`BASE_URL_ENV`] when set, matching the CLI's own override.
        pub fn new(auth_path: impl Into<PathBuf>, refresher: Arc<dyn GrokCredentialRefresher>) -> Self {
            let base_url = std::env::var(BASE_URL_ENV)
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
            Self {
                auth_path: auth_path.into(),
                base_url,
                refresher,
            }
        }

        /// Explicit base URL — tests point this at a local server.
        #[cfg(test)]
        pub fn with_base_url(
            auth_path: impl Into<PathBuf>,
            base_url: impl Into<String>,
            refresher: Arc<dyn GrokCredentialRefresher>,
        ) -> Self {
            Self {
                auth_path: auth_path.into(),
                base_url: base_url.into(),
                refresher,
            }
        }

        fn billing_url(&self) -> String {
            format!("{}{BILLING_PATH}", self.base_url.trim_end_matches('/'))
        }
    }

    /// Why a bearer token could not be produced. Carries no credential material.
    #[derive(Debug)]
    struct TokenError {
        kind: DriverQuotaFailureKind,
        reason: String,
    }

    /// The signed-in account's credential as stored on disk, judged at one
    /// instant. `refreshable` is whether a refresh token sits beside the access
    /// token — i.e. whether the CLI *could* mint a new one without a login.
    #[derive(Debug, PartialEq, Eq)]
    enum StoredCredential {
        /// `expires_at` is absent, unparseable, or in the future.
        Live { bearer: String, refreshable: bool },
        /// `expires_at` has passed.
        Expired { refreshable: bool },
    }

    /// Read the signed-in account out of a Grok `auth.json` document.
    ///
    /// The file maps an issuer-scoped account key to a record whose `key` field
    /// holds the bearer and whose `refresh_token`, when present, is what the
    /// CLI uses to renew it. More than one account can be present; the entry
    /// with the furthest-future `expires_at` wins, which is the live one after
    /// a re-login under a second issuer. RFC-3339 timestamps compare correctly
    /// as strings when they share an offset, and these are all written by the
    /// same CLI.
    ///
    /// `now_epoch_s` is the instant expiry is judged against; the probe
    /// passes the live clock and tests pass a fixed value so fixtures never
    /// age into "expired".
    ///
    /// Returns the token by value. Callers must not log or store it.
    fn read_stored_credential(document: &serde_json::Value, now_epoch_s: i64) -> Result<StoredCredential, TokenError> {
        let Some(accounts) = document.as_object() else {
            return Err(TokenError {
                kind: DriverQuotaFailureKind::NotAuthenticated,
                reason: "Grok auth.json was not a JSON object — sign in with `grok login`".to_owned(),
            });
        };
        let mut best: Option<(&str, &serde_json::Value, &str)> = None;
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
                Some((best_expires, _, _)) if best_expires >= expires => {}
                _ => best = Some((expires, record, key)),
            }
        }
        let Some((expires, record, key)) = best else {
            return Err(TokenError {
                kind: DriverQuotaFailureKind::NotAuthenticated,
                reason: "Grok auth.json has no signed-in account — sign in with `grok login`".to_owned(),
            });
        };
        let refreshable = record
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|token| !token.is_empty());
        if token_expired(expires, now_epoch_s) {
            Ok(StoredCredential::Expired { refreshable })
        } else {
            Ok(StoredCredential::Live {
                bearer: key.to_owned(),
                refreshable,
            })
        }
    }

    /// `true` when `expires_at` parses as RFC-3339 and is strictly before
    /// `now_epoch_s`. Missing or unparseable timestamps are treated as live
    /// so a document without an expiry still produces a request rather than
    /// a false "expired" label.
    fn token_expired(expires_at: &str, now_epoch_s: i64) -> bool {
        if expires_at.is_empty() {
            return false;
        }
        match chrono::DateTime::parse_from_rfc3339(expires_at) {
            Ok(dt) => dt.timestamp() < now_epoch_s,
            Err(_) => false,
        }
    }

    /// Read and parse the auth document, mapping IO problems onto the failure
    /// kinds the UI distinguishes.
    fn read_auth_document(path: &Path) -> Result<serde_json::Value, TokenError> {
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

    /// Stored token expired and nothing to renew it with: genuinely signed out.
    const EXPIRED_UNREFRESHABLE_REASON: &str =
        "stored Grok token has expired and no refresh token is stored — sign in with `grok login`";

    /// What a probe is holding after the credential has been resolved.
    struct Bearer {
        token: String,
        /// Whether a CLI refresh is still worth trying on a rejected request.
        refreshable: bool,
    }

    impl GrokQuotaProbe {
        /// Current stored credential, judged against the live clock.
        fn stored_credential(&self) -> Result<StoredCredential, TokenError> {
            read_auth_document(&self.auth_path)
                .and_then(|document| read_stored_credential(&document, crate::now_epoch_s()))
        }

        /// Ask the CLI to refresh, then re-read whatever it left on disk.
        /// `why` names the trigger in a failure reason ("has expired" or
        /// "was rejected by the Grok billing endpoint").
        async fn refresh_via_cli(&self, why: &str, rejected_token: Option<&str>) -> Result<Bearer, TokenError> {
            match self.refresher.refresh().await {
                GrokRefreshOutcome::LoggedIn => match self.stored_credential()? {
                    StoredCredential::Live { bearer, refreshable } => Ok(Bearer {
                        token: bearer,
                        refreshable,
                    }),
                    // The CLI affirms a login but there is no usable token,
                    // so this is a check failure rather than a sign-out.
                    StoredCredential::Expired { .. } => Err(TokenError {
                        kind: DriverQuotaFailureKind::ProbeFailed,
                        reason: format!(
                            "stored Grok token {why}; the grok CLI affirmed a login but left the stored token expired (clock skew?)"
                        ),
                    }),
                },
                GrokRefreshOutcome::NotLoggedIn { detail } => {
                    // A banner can describe the state before the CLI refreshed.
                    if let Some(bearer) = self.rewritten_live_credential(rejected_token) {
                        return Ok(bearer);
                    }
                    Err(TokenError {
                        kind: DriverQuotaFailureKind::NotAuthenticated,
                        reason: format!(
                            "stored Grok token {why} and the grok CLI could not refresh it ({detail}) — sign in with `grok login`"
                        ),
                    })
                }
                GrokRefreshOutcome::Failed { detail } => {
                    // The CLI may have rewritten the credential before its
                    // pass ran out of budget.
                    if let Some(bearer) = self.rewritten_live_credential(rejected_token) {
                        return Ok(bearer);
                    }
                    Err(TokenError {
                        kind: DriverQuotaFailureKind::ProbeFailed,
                        reason: format!(
                            "stored Grok token {why} and the grok CLI could not be run to refresh it: {detail}"
                        ),
                    })
                }
            }
        }

        /// The stored credential, if it is live and is not the token the
        /// billing endpoint already rejected.
        fn rewritten_live_credential(&self, rejected_token: Option<&str>) -> Option<Bearer> {
            match self.stored_credential() {
                Ok(StoredCredential::Live { bearer, refreshable }) if rejected_token != Some(bearer.as_str()) => {
                    Some(Bearer {
                        token: bearer,
                        refreshable,
                    })
                }
                _ => None,
            }
        }

        /// Resolve a bearer to send: the stored one if live, otherwise the one
        /// the CLI writes when asked to refresh. `refreshed` records whether
        /// the CLI pass has already been spent on this probe.
        async fn resolve_bearer(&self, refreshed: &mut bool) -> Result<Bearer, TokenError> {
            match self.stored_credential()? {
                StoredCredential::Live { bearer, refreshable } => Ok(Bearer {
                    token: bearer,
                    refreshable,
                }),
                StoredCredential::Expired { refreshable: false } => Err(TokenError {
                    kind: DriverQuotaFailureKind::NotAuthenticated,
                    reason: EXPIRED_UNREFRESHABLE_REASON.to_owned(),
                }),
                StoredCredential::Expired { refreshable: true } => {
                    *refreshed = true;
                    self.refresh_via_cli("has expired", None).await
                }
            }
        }

        /// One GET of the billing endpoint. `Err` is a transport failure (the
        /// error is dropped: reqwest's Display includes the URL but never a
        /// request header, so the bearer could not reach a reason string
        /// anyway, and nothing from it is wanted in one).
        async fn billing_request(&self, bearer: &str) -> Result<BillingResponse, ()> {
            let response = boss_http_retry::http_client()
                .get(self.billing_url())
                .header("Authorization", format!("Bearer {bearer}"))
                .header("Accept", "application/json")
                .header("User-Agent", USER_AGENT)
                .send()
                .await
                .map_err(|_| ())?;
            let status = response.status().as_u16();
            Ok(BillingResponse {
                status,
                body: response.text().await.ok(),
            })
        }
    }

    /// What the billing endpoint answered: the status, and the body when it
    /// could be read. The body is only ever parsed on a 2xx and never echoed.
    struct BillingResponse {
        status: u16,
        body: Option<String>,
    }

    fn rejected(status: u16) -> bool {
        status == 401 || status == 403
    }

    #[async_trait]
    impl DriverQuotaProbe for GrokQuotaProbe {
        fn driver(&self) -> &'static str {
            DRIVER_SLUG_GROK
        }

        async fn probe(&self) -> DriverQuotaOutcome {
            let mut refreshed = false;
            let bearer = match self.resolve_bearer(&mut refreshed).await {
                Ok(bearer) => bearer,
                Err(TokenError { kind, reason }) => return unavailable(kind, reason),
            };

            let mut response = self.billing_request(&bearer.token).await;

            // A token the clock calls live but the endpoint rejects (revoked,
            // or the clock is wrong) gets the same single CLI pass an expired
            // one does — once per probe, and only when there is a refresh
            // token for the CLI to use.
            if let Ok(resp) = &response
                && rejected(resp.status)
                && bearer.refreshable
                && !refreshed
            {
                refreshed = true;
                match self
                    .refresh_via_cli("was rejected by the Grok billing endpoint", Some(&bearer.token))
                    .await
                {
                    Ok(renewed) => response = self.billing_request(&renewed.token).await,
                    Err(TokenError { kind, reason }) => return unavailable(kind, reason),
                }
            }

            let response = match response {
                Ok(response) => response,
                Err(()) => {
                    return unavailable(
                        DriverQuotaFailureKind::ProbeFailed,
                        "could not reach the Grok billing endpoint".to_owned(),
                    );
                }
            };

            let BillingResponse { status, body } = response;
            if !(200..300).contains(&status) {
                let kind = if rejected(status) {
                    DriverQuotaFailureKind::NotAuthenticated
                } else {
                    DriverQuotaFailureKind::ProbeFailed
                };
                let after_refresh = if refreshed {
                    " after a grok CLI credential refresh"
                } else {
                    ""
                };
                // Status code only — the body is not echoed.
                return unavailable(
                    kind,
                    format!("Grok billing endpoint answered HTTP {status}{after_refresh}"),
                );
            }

            match body {
                Some(body) => parse_grok_billing(&body),
                None => unavailable(
                    DriverQuotaFailureKind::ProbeFailed,
                    "Grok billing response could not be read".to_owned(),
                ),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use boss_protocol::DriverQuotaReading;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        /// Fixed "now" every expiry fixture below is judged against:
        /// 2026-06-01T00:00:00Z. Fixtures are written relative to this
        /// instant so the tests never meet the real clock.
        const NOW: i64 = 1_780_272_000;
        const DAY: i64 = 86_400;

        fn rfc3339(epoch_s: i64) -> String {
            chrono::DateTime::from_timestamp(epoch_s, 0)
                .expect("fixture epoch in range")
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        }

        fn live(doc: &serde_json::Value) -> Option<String> {
            match read_stored_credential(doc, NOW) {
                Ok(StoredCredential::Live { bearer, .. }) => Some(bearer),
                _ => None,
            }
        }

        #[test]
        fn bearer_taken_from_the_only_account() {
            let doc = serde_json::json!({
                "https://auth.example::acct": { "key": "token-a", "expires_at": rfc3339(NOW + 30 * DAY) }
            });
            assert_eq!(live(&doc), Some("token-a".to_owned()));
        }

        #[test]
        fn furthest_future_account_wins_when_several_are_present() {
            let doc = serde_json::json!({
                "issuer-a::1": { "key": "old", "expires_at": rfc3339(NOW + 7 * DAY) },
                "issuer-b::2": { "key": "new", "expires_at": rfc3339(NOW + 30 * DAY) },
            });
            assert_eq!(live(&doc), Some("new".to_owned()));
        }

        #[test]
        fn empty_document_reads_as_not_authenticated() {
            let err = read_stored_credential(&serde_json::json!({}), NOW).unwrap_err();
            assert_eq!(err.kind, DriverQuotaFailureKind::NotAuthenticated);
        }

        #[test]
        fn expired_token_with_a_refresh_token_is_expired_but_refreshable() {
            let doc = serde_json::json!({
                "issuer-a::1": { "key": "old", "refresh_token": "r", "expires_at": rfc3339(NOW - DAY) }
            });
            assert_eq!(
                read_stored_credential(&doc, NOW).unwrap(),
                StoredCredential::Expired { refreshable: true }
            );
        }

        #[test]
        fn expired_token_without_a_refresh_token_is_not_refreshable() {
            for record in [
                serde_json::json!({ "key": "old", "expires_at": rfc3339(NOW - DAY) }),
                serde_json::json!({ "key": "old", "refresh_token": "", "expires_at": rfc3339(NOW - DAY) }),
            ] {
                let doc = serde_json::json!({ "issuer-a::1": record });
                assert_eq!(
                    read_stored_credential(&doc, NOW).unwrap(),
                    StoredCredential::Expired { refreshable: false }
                );
            }
        }

        /// The exact shape the CLI writes (`grok 1.0.70`): microsecond
        /// fraction, `Z` suffix. Six hours after creation.
        #[test]
        fn the_clis_own_timestamp_format_is_judged_correctly() {
            let doc = serde_json::json!({
                "https://auth.x.ai::client": {
                    "key": "k", "auth_mode": "oidc", "refresh_token": "r",
                    "create_time": "2026-10-24T21:49:20.645060Z",
                    "expires_at": "2026-10-25T03:49:20.645060Z",
                }
            });
            // 2026-10-24T21:50:00Z — a minute after the refresh.
            let just_after_login = 1_792_878_600;
            assert!(matches!(
                read_stored_credential(&doc, just_after_login).unwrap(),
                StoredCredential::Live { refreshable: true, .. }
            ));
            // 2026-10-25T03:49:21Z — one second past expiry.
            assert_eq!(
                read_stored_credential(&doc, 1_792_900_161).unwrap(),
                StoredCredential::Expired { refreshable: true }
            );
        }

        #[test]
        fn record_without_a_key_is_skipped_rather_than_treated_as_a_token() {
            let doc = serde_json::json!({ "issuer::1": { "expires_at": rfc3339(NOW + 30 * DAY) } });
            assert!(read_stored_credential(&doc, NOW).is_err());
        }

        #[test]
        fn non_object_document_reads_as_not_authenticated() {
            let err = read_stored_credential(&serde_json::json!([]), NOW).unwrap_err();
            assert_eq!(err.kind, DriverQuotaFailureKind::NotAuthenticated);
        }

        #[test]
        fn missing_auth_file_reads_as_not_authenticated_not_a_blank() {
            let err = read_auth_document(Path::new("/nonexistent/grok/auth.json")).unwrap_err();
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
            let probe = GrokQuotaProbe::with_base_url("/tmp/auth.json", "https://example.test/v1", never());
            assert_eq!(probe.billing_url(), "https://example.test/v1/billing?format=credits");
        }

        #[test]
        fn trailing_slash_on_the_base_url_does_not_double_up() {
            let probe = GrokQuotaProbe::with_base_url("/tmp/auth.json", "https://example.test/v1/", never());
            assert_eq!(probe.billing_url(), "https://example.test/v1/billing?format=credits");
        }

        #[test]
        fn driver_slug_is_grok() {
            assert_eq!(GrokQuotaProbe::new("/tmp/auth.json", never()).driver(), "grok");
        }

        // ---- end-to-end: stored credential → CLI refresh → endpoint -------

        /// A scripted CLI: counts calls, optionally rewrites the credential
        /// file the way the real CLI would, then reports the scripted outcome.
        struct ScriptedCli {
            calls: AtomicUsize,
            rewrite: Option<(PathBuf, serde_json::Value)>,
            outcome: GrokRefreshOutcome,
        }

        #[async_trait]
        impl GrokCredentialRefresher for ScriptedCli {
            async fn refresh(&self) -> GrokRefreshOutcome {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if let Some((path, document)) = &self.rewrite {
                    std::fs::write(path, document.to_string()).expect("rewrite auth.json like the CLI");
                }
                self.outcome.clone()
            }
        }

        fn cli(outcome: GrokRefreshOutcome, rewrite: Option<(PathBuf, serde_json::Value)>) -> Arc<ScriptedCli> {
            Arc::new(ScriptedCli {
                calls: AtomicUsize::new(0),
                rewrite,
                outcome,
            })
        }

        /// A refresher that must never be reached.
        fn never() -> Arc<ScriptedCli> {
            cli(
                GrokRefreshOutcome::Failed {
                    detail: "the refresher must not have been called".to_owned(),
                },
                None,
            )
        }

        /// A fresh private directory per test, under the test's own tmpdir.
        fn scratch(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("boss-driver-quota-grok-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            dir
        }

        fn write_auth(dir: &Path, document: &serde_json::Value) -> PathBuf {
            let path = dir.join("auth.json");
            std::fs::write(&path, document.to_string()).expect("write auth.json");
            path
        }

        /// A record whose `expires_at` is well in the future of the real clock
        /// (the probe judges against `now_epoch_s()`, which tests cannot pin).
        fn live_record(key: &str, refreshable: bool) -> serde_json::Value {
            let far_future = crate::now_epoch_s() + 365 * DAY;
            let mut record = serde_json::json!({ "key": key, "expires_at": rfc3339(far_future) });
            if refreshable {
                record["refresh_token"] = serde_json::Value::String("r".to_owned());
            }
            record
        }

        fn expired_record(key: &str, refreshable: bool) -> serde_json::Value {
            let past = crate::now_epoch_s() - DAY;
            let mut record = serde_json::json!({ "key": key, "expires_at": rfc3339(past) });
            if refreshable {
                record["refresh_token"] = serde_json::Value::String("r".to_owned());
            }
            record
        }

        fn doc(record: serde_json::Value) -> serde_json::Value {
            serde_json::json!({ "https://auth.x.ai::client": record })
        }

        const BILLING_OK: &str = r#"{"config":{"creditUsagePercent":12.5,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2026-10-09T22:22:42Z"}}}"#;

        /// One-shot canned HTTP server: answers each connection with the next
        /// scripted (status, body) and records the bearer each request carried.
        struct CannedEndpoint {
            base_url: String,
            bearers: Arc<Mutex<Vec<String>>>,
        }

        async fn endpoint(responses: Vec<(u16, &'static str)>) -> CannedEndpoint {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let base_url = format!("http://{}/v1", listener.local_addr().expect("addr"));
            let bearers: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&bearers);
            tokio::spawn(async move {
                for (status, body) in responses {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let mut buf = vec![0u8; 8192];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let bearer = request
                        .lines()
                        .find_map(|line| line.strip_prefix("authorization: Bearer "))
                        .or_else(|| {
                            request
                                .lines()
                                .find_map(|line| line.strip_prefix("Authorization: Bearer "))
                        })
                        .unwrap_or("<none>")
                        .to_owned();
                    seen.lock().unwrap().push(bearer);
                    let reason = match status {
                        200 => "OK",
                        401 => "Unauthorized",
                        _ => "Error",
                    };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
            });
            CannedEndpoint { base_url, bearers }
        }

        fn reading(outcome: DriverQuotaOutcome) -> DriverQuotaReading {
            match outcome {
                DriverQuotaOutcome::Reading(reading) => reading,
                other => panic!("expected a reading, got {other:?}"),
            }
        }

        fn failure(outcome: DriverQuotaOutcome) -> (DriverQuotaFailureKind, String) {
            match outcome {
                DriverQuotaOutcome::Unavailable { kind, reason } => (kind, reason),
                other => panic!("expected a failure, got {other:?}"),
            }
        }

        /// The stored access token has expired but a refresh token sits beside it.
        /// The CLI refreshes, the probe re-reads, and the panel gets the real figure.
        #[tokio::test]
        async fn expired_but_refreshable_token_is_refreshed_through_the_cli_and_read() {
            let dir = scratch("expired_but_refreshable_token_is_refreshed_through_the_cli_and_read");
            let auth = write_auth(&dir, &doc(expired_record("stale-token", true)));
            let cli = cli(
                GrokRefreshOutcome::LoggedIn,
                Some((auth.clone(), doc(live_record("fresh-token", true)))),
            );
            let server = endpoint(vec![(200, BILLING_OK)]).await;
            let probe = GrokQuotaProbe::with_base_url(&auth, &server.base_url, cli.clone());

            let reading = reading(probe.probe().await);
            assert_eq!(reading.used_percent, 12.5);
            assert_eq!(cli.calls.load(Ordering::SeqCst), 1, "exactly one CLI pass");
            assert_eq!(
                server.bearers.lock().unwrap().as_slice(),
                ["fresh-token"],
                "the request must carry the token the CLI wrote, never the stale one"
            );
        }

        #[tokio::test]
        async fn stale_signed_out_banner_with_live_rewrite_returns_usage() {
            let dir = scratch("stale_signed_out_banner_with_live_rewrite_returns_usage");
            let auth = write_auth(&dir, &doc(expired_record("stale-token", true)));
            let cli = cli(
                GrokRefreshOutcome::NotLoggedIn {
                    detail: "You are not authenticated.".to_owned(),
                },
                Some((auth.clone(), doc(live_record("fresh-token", true)))),
            );
            let server = endpoint(vec![(200, BILLING_OK)]).await;
            let probe = GrokQuotaProbe::with_base_url(&auth, &server.base_url, cli.clone());

            let reading = reading(probe.probe().await);
            assert_eq!(reading.used_percent, 12.5);
            assert_eq!(cli.calls.load(Ordering::SeqCst), 1, "exactly one CLI pass");
            assert_eq!(
                server.bearers.lock().unwrap().as_slice(),
                ["fresh-token"],
                "the request must carry the token the CLI wrote, never the stale one"
            );
        }

        #[tokio::test]
        async fn live_token_is_used_without_consulting_the_cli() {
            let dir = scratch("live_token_is_used_without_consulting_the_cli");
            let auth = write_auth(&dir, &doc(live_record("live-token", true)));
            let cli = never();
            let server = endpoint(vec![(200, BILLING_OK)]).await;
            let probe = GrokQuotaProbe::with_base_url(&auth, &server.base_url, cli.clone());

            reading(probe.probe().await);
            assert_eq!(cli.calls.load(Ordering::SeqCst), 0);
            assert_eq!(server.bearers.lock().unwrap().as_slice(), ["live-token"]);
        }

        #[tokio::test]
        async fn rejected_live_token_the_cli_leaves_unchanged_is_not_signed_in_after_one_request() {
            let dir = scratch("rejected_live_token_the_cli_leaves_unchanged_is_not_signed_in_after_one_request");
            let auth = write_auth(&dir, &doc(live_record("rejected-token", true)));
            let cli = cli(
                GrokRefreshOutcome::NotLoggedIn {
                    detail: "You are not authenticated.".to_owned(),
                },
                None,
            );
            let server = endpoint(vec![(401, "{}")]).await;
            let probe = GrokQuotaProbe::with_base_url(&auth, &server.base_url, cli.clone());

            let (kind, reason) = failure(probe.probe().await);
            assert_eq!(kind, DriverQuotaFailureKind::NotAuthenticated);
            assert!(reason.contains("You are not authenticated."), "{reason}");
            assert!(reason.contains("grok login"), "{reason}");
            assert_eq!(cli.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                server.bearers.lock().unwrap().as_slice(),
                ["rejected-token"],
                "the rejected token must not be sent a second time"
            );
        }

        #[tokio::test]
        async fn expired_token_the_cli_cannot_refresh_is_not_signed_in_naming_the_cli_banner() {
            let dir = scratch("expired_token_the_cli_cannot_refresh_is_not_signed_in_naming_the_cli_banner");
            let auth = write_auth(&dir, &doc(expired_record("stale-token", true)));
            let cli = cli(
                GrokRefreshOutcome::NotLoggedIn {
                    detail: "You are not authenticated.".to_owned(),
                },
                None,
            );
            let probe = GrokQuotaProbe::with_base_url(&auth, "http://127.0.0.1:9/v1", cli.clone());

            let (kind, reason) = failure(probe.probe().await);
            assert_eq!(kind, DriverQuotaFailureKind::NotAuthenticated);
            assert!(reason.contains("has expired"), "{reason}");
            assert!(reason.contains("You are not authenticated."), "{reason}");
            assert!(reason.contains("grok login"), "{reason}");
            assert!(!reason.contains("stale-token"), "{reason}");
            assert_eq!(cli.calls.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn expired_token_with_no_refresh_token_is_not_signed_in_without_running_the_cli() {
            let dir = scratch("expired_token_with_no_refresh_token_is_not_signed_in_without_running_the_cli");
            let auth = write_auth(&dir, &doc(expired_record("stale-token", false)));
            let cli = never();
            let probe = GrokQuotaProbe::with_base_url(&auth, "http://127.0.0.1:9/v1", cli.clone());

            let (kind, reason) = failure(probe.probe().await);
            assert_eq!(kind, DriverQuotaFailureKind::NotAuthenticated);
            assert_eq!(reason, EXPIRED_UNREFRESHABLE_REASON);
            assert_eq!(cli.calls.load(Ordering::SeqCst), 0);
        }

        /// A CLI that cannot run says nothing about sign-in state, so the
        /// row must not claim "not signed in" — nor pretend to know.
        #[tokio::test]
        async fn expired_token_when_the_cli_cannot_run_is_a_check_failure_not_a_sign_in_verdict() {
            let dir = scratch("expired_token_when_the_cli_cannot_run_is_a_check_failure_not_a_sign_in_verdict");
            let auth = write_auth(&dir, &doc(expired_record("stale-token", true)));
            let cli = cli(
                GrokRefreshOutcome::Failed {
                    detail: "`grok` is not installed or not on PATH".to_owned(),
                },
                None,
            );
            let probe = GrokQuotaProbe::with_base_url(&auth, "http://127.0.0.1:9/v1", cli.clone());

            let (kind, reason) = failure(probe.probe().await);
            assert_eq!(kind, DriverQuotaFailureKind::ProbeFailed);
            assert!(reason.contains("not installed"), "{reason}");
        }

        #[tokio::test]
        async fn cli_affirming_a_login_but_leaving_the_token_expired_is_not_reported_as_signed_out() {
            let dir = scratch("cli_affirming_a_login_but_leaving_the_token_expired_is_not_reported_as_signed_out");
            let auth = write_auth(&dir, &doc(expired_record("stale-token", true)));
            let cli = cli(GrokRefreshOutcome::LoggedIn, None);
            let probe = GrokQuotaProbe::with_base_url(&auth, "http://127.0.0.1:9/v1", cli.clone());

            let (kind, reason) = failure(probe.probe().await);
            assert_eq!(kind, DriverQuotaFailureKind::ProbeFailed);
            assert!(reason.contains("affirmed a login"), "{reason}");
        }

        /// A token the clock calls live but the endpoint rejects gets one CLI
        /// pass and one retry with whatever the CLI wrote.
        #[tokio::test]
        async fn rejected_live_token_is_refreshed_once_and_retried() {
            let dir = scratch("rejected_live_token_is_refreshed_once_and_retried");
            let auth = write_auth(&dir, &doc(live_record("revoked-token", true)));
            let cli = cli(
                GrokRefreshOutcome::LoggedIn,
                Some((auth.clone(), doc(live_record("renewed-token", true)))),
            );
            let server = endpoint(vec![(401, "{}"), (200, BILLING_OK)]).await;
            let probe = GrokQuotaProbe::with_base_url(&auth, &server.base_url, cli.clone());

            let reading = reading(probe.probe().await);
            assert_eq!(reading.used_percent, 12.5);
            assert_eq!(cli.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                server.bearers.lock().unwrap().as_slice(),
                ["revoked-token", "renewed-token"]
            );
        }

        #[tokio::test]
        async fn rejected_token_with_no_refresh_token_is_not_signed_in_and_not_retried() {
            let dir = scratch("rejected_token_with_no_refresh_token_is_not_signed_in_and_not_retried");
            let auth = write_auth(&dir, &doc(live_record("revoked-token", false)));
            let cli = never();
            let server = endpoint(vec![(401, "{}")]).await;
            let probe = GrokQuotaProbe::with_base_url(&auth, &server.base_url, cli.clone());

            let (kind, reason) = failure(probe.probe().await);
            assert_eq!(kind, DriverQuotaFailureKind::NotAuthenticated);
            assert_eq!(reason, "Grok billing endpoint answered HTTP 401");
            assert_eq!(cli.calls.load(Ordering::SeqCst), 0);
        }

        /// One CLI pass per probe, however it went: a refresh that produced a
        /// token the endpoint still rejects ends the probe, it does not loop.
        #[tokio::test]
        async fn a_second_rejection_after_the_refresh_ends_the_probe_without_another_cli_pass() {
            let dir = scratch("a_second_rejection_after_the_refresh_ends_the_probe_without_another_cli_pass");
            let auth = write_auth(&dir, &doc(expired_record("stale-token", true)));
            let cli = cli(
                GrokRefreshOutcome::LoggedIn,
                Some((auth.clone(), doc(live_record("still-bad-token", true)))),
            );
            let server = endpoint(vec![(401, "{}")]).await;
            let probe = GrokQuotaProbe::with_base_url(&auth, &server.base_url, cli.clone());

            let (kind, reason) = failure(probe.probe().await);
            assert_eq!(kind, DriverQuotaFailureKind::NotAuthenticated);
            assert_eq!(
                reason,
                "Grok billing endpoint answered HTTP 401 after a grok CLI credential refresh"
            );
            assert_eq!(cli.calls.load(Ordering::SeqCst), 1);
            assert_eq!(server.bearers.lock().unwrap().as_slice(), ["still-bad-token"]);
        }
    }
}
