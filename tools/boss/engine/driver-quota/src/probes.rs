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

    impl ClaudeQuotaProbe {
        /// Override the executable — tests point this at a stub script.
        pub fn with_program(program: impl Into<PathBuf>) -> Self {
            Self {
                program: program.into(),
                ..Self::default()
            }
        }
    }

    #[async_trait]
    impl DriverQuotaProbe for ClaudeQuotaProbe {
        fn driver(&self) -> &'static str {
            DRIVER_SLUG_CLAUDE
        }

        async fn probe(&self) -> DriverQuotaOutcome {
            // Environment is inherited deliberately: the CLI must resolve the
            // maintainer's own credentials from wherever it normally does
            // (macOS keychain, or `~/.claude`). Boss neither reads nor relocates
            // them.
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

            let output = match command.output().await {
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
    }
}

/// Codex quota probe: two JSON-RPC lines to `codex app-server`.
///
/// Codex has no `usage` or `status` subcommand, and its `/status` slash
/// command is TUI-only. Its app-server protocol, however, exposes
/// `account/rateLimits/read` — a first-class, machine-readable method that
/// returns exactly the plan-level figure `/status` renders. The probe speaks
/// it directly: `initialize`, then the read, then exit. No thread is started,
/// no turn is taken, and no credential passes through Boss — the child
/// resolves `$CODEX_HOME/auth.json` itself, from the engine's inherited
/// environment, which is the same location
/// `boss_codex_auth::resolve_operator_auth_path` names.
///
/// Note on the protocol: the app-server answers `initialize` synchronously
/// but resolves `account/rateLimits/read` from the network, so stdin must
/// stay open until that reply arrives. Closing stdin after writing both
/// lines makes the child exit *before* answering — this was observed, and is
/// why the probe drives the exchange itself rather than piping both lines in
/// with a plain "write everything, then read everything" runner.
pub mod codex {
    use std::path::PathBuf;
    use std::process::Stdio;

    use async_trait::async_trait;
    use boss_protocol::{DRIVER_SLUG_CODEX, DriverQuotaFailureKind, DriverQuotaOutcome};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use crate::DriverQuotaProbe;
    use crate::parse::{parse_codex_rate_limits, unavailable};

    /// JSON-RPC ids used by the exchange. Only two requests are ever sent.
    const ID_INITIALIZE: i64 = 1;
    const ID_RATE_LIMITS: i64 = 2;

    /// The app-server method carrying the plan-level rate limit.
    const METHOD_RATE_LIMITS: &str = "account/rateLimits/read";

    /// Probe for the `codex` driver.
    pub struct CodexQuotaProbe {
        program: PathBuf,
    }

    impl Default for CodexQuotaProbe {
        fn default() -> Self {
            Self {
                program: PathBuf::from("codex"),
            }
        }
    }

    impl CodexQuotaProbe {
        /// Override the executable — tests point this at a stub.
        pub fn with_program(program: impl Into<PathBuf>) -> Self {
            Self {
                program: program.into(),
            }
        }
    }

    /// One line of the exchange, classified. Kept separate from the IO so the
    /// line-scanning rules are unit-testable without a child process.
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum ServerLine {
        /// The reply we are waiting for, carrying its `result` payload.
        Result(String),
        /// The reply we are waiting for, carrying a JSON-RPC error message.
        Error(String),
        /// Anything else: the `initialize` reply, an unsolicited notification.
        Other,
    }

    /// Classify one stdout line against the id we are waiting for.
    ///
    /// The app-server interleaves unsolicited notifications (`remoteControl/…`)
    /// with replies, so the probe cannot simply take the next line — it matches
    /// on `id` and ignores everything else.
    pub(crate) fn classify_line(line: &str, awaiting_id: i64) -> ServerLine {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return ServerLine::Other;
        };
        if value.get("id").and_then(serde_json::Value::as_i64) != Some(awaiting_id) {
            return ServerLine::Other;
        }
        if let Some(err) = value.get("error") {
            let message = err
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("app-server returned an error");
            // Truncated: an unknown-method error echoes the entire method list.
            let message: String = message.chars().take(200).collect();
            return ServerLine::Error(message);
        }
        match value.get("result") {
            Some(result) => ServerLine::Result(result.to_string()),
            None => ServerLine::Error("reply carried neither `result` nor `error`".to_owned()),
        }
    }

    fn request_line(id: i64, method: &str, params: serde_json::Value) -> String {
        let line = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        format!("{line}\n")
    }

    /// The `initialize` params the app-server requires before any other method.
    fn initialize_line() -> String {
        request_line(
            ID_INITIALIZE,
            "initialize",
            serde_json::json!({
                "clientInfo": {
                    "name": "boss-quota-probe",
                    "title": "Boss quota probe",
                    "version": "1.0.0",
                }
            }),
        )
    }

    #[async_trait]
    impl DriverQuotaProbe for CodexQuotaProbe {
        fn driver(&self) -> &'static str {
            DRIVER_SLUG_CODEX
        }

        async fn probe(&self) -> DriverQuotaOutcome {
            let mut child = match tokio::process::Command::new(&self.program)
                .arg("app-server")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
            {
                Ok(child) => child,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    return unavailable(
                        DriverQuotaFailureKind::NotInstalled,
                        format!("`{}` is not installed or not on PATH", self.program.display()),
                    );
                }
                Err(err) => {
                    return unavailable(
                        DriverQuotaFailureKind::ProbeFailed,
                        format!("could not start `codex app-server`: {err}"),
                    );
                }
            };

            let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
                let _ = child.start_kill();
                return unavailable(
                    DriverQuotaFailureKind::ProbeFailed,
                    "codex app-server did not expose stdio pipes".to_owned(),
                );
            };

            let outcome = drive_exchange(&mut stdin, stdout).await;
            // Dropping stdin closes it, which is the app-server's shutdown
            // signal; `kill_on_drop` is the backstop for a child that ignores it.
            drop(stdin);
            let _ = child.start_kill();
            outcome
        }
    }

    /// Write the two requests and read until the rate-limit reply lands.
    /// Factored out of [`CodexQuotaProbe::probe`] so the process plumbing above
    /// stays readable; the deadline is applied by the cache around the whole
    /// probe, so this loop simply ends when stdout does.
    async fn drive_exchange(
        stdin: &mut tokio::process::ChildStdin,
        stdout: tokio::process::ChildStdout,
    ) -> DriverQuotaOutcome {
        for line in [
            initialize_line(),
            request_line(ID_RATE_LIMITS, METHOD_RATE_LIMITS, serde_json::json!({})),
        ] {
            if let Err(err) = stdin.write_all(line.as_bytes()).await {
                return unavailable(
                    DriverQuotaFailureKind::ProbeFailed,
                    format!("could not write to codex app-server: {err}"),
                );
            }
        }
        if let Err(err) = stdin.flush().await {
            return unavailable(
                DriverQuotaFailureKind::ProbeFailed,
                format!("could not flush to codex app-server: {err}"),
            );
        }

        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => match classify_line(&line, ID_RATE_LIMITS) {
                    ServerLine::Other => continue,
                    ServerLine::Error(message) => {
                        let kind = if message.contains("auth") || message.contains("login") {
                            DriverQuotaFailureKind::NotAuthenticated
                        } else {
                            DriverQuotaFailureKind::ProbeFailed
                        };
                        return unavailable(kind, format!("codex {METHOD_RATE_LIMITS} failed: {message}"));
                    }
                    ServerLine::Result(result) => {
                        let Ok(value) = serde_json::from_str::<serde_json::Value>(&result) else {
                            return unavailable(
                                DriverQuotaFailureKind::Unparseable,
                                "codex app-server reply was not valid JSON".to_owned(),
                            );
                        };
                        return parse_codex_rate_limits(&value);
                    }
                },
                Ok(None) => {
                    return unavailable(
                        DriverQuotaFailureKind::ProbeFailed,
                        format!("codex app-server closed without answering {METHOD_RATE_LIMITS}"),
                    );
                }
                Err(err) => {
                    return unavailable(
                        DriverQuotaFailureKind::ProbeFailed,
                        format!("could not read from codex app-server: {err}"),
                    );
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn unsolicited_notification_is_ignored_not_mistaken_for_the_reply() {
            let line = r#"{"method":"remoteControl/status/changed","params":{"status":"disabled"}}"#;
            assert_eq!(classify_line(line, ID_RATE_LIMITS), ServerLine::Other);
        }

        #[test]
        fn initialize_reply_is_ignored_while_awaiting_the_rate_limit_reply() {
            let line = r#"{"id":1,"result":{"codexHome":"/home/x/.codex"}}"#;
            assert_eq!(classify_line(line, ID_RATE_LIMITS), ServerLine::Other);
        }

        #[test]
        fn matching_result_is_captured() {
            let line = r#"{"id":2,"result":{"rateLimits":{"primary":{"usedPercent":5}}}}"#;
            match classify_line(line, ID_RATE_LIMITS) {
                ServerLine::Result(result) => {
                    let value: serde_json::Value = serde_json::from_str(&result).expect("json");
                    assert_eq!(
                        value
                            .pointer("/rateLimits/primary/usedPercent")
                            .and_then(|v| v.as_f64()),
                        Some(5.0)
                    );
                }
                other => panic!("expected a result, got {other:?}"),
            }
        }

        #[test]
        fn jsonrpc_error_is_surfaced_and_truncated() {
            let long = "x".repeat(500);
            let line = serde_json::json!({ "id": 2, "error": { "code": -32600, "message": long } }).to_string();
            match classify_line(&line, ID_RATE_LIMITS) {
                ServerLine::Error(message) => assert_eq!(message.chars().count(), 200),
                other => panic!("expected an error, got {other:?}"),
            }
        }

        #[test]
        fn non_json_noise_on_stdout_is_ignored() {
            assert_eq!(classify_line("warning: something", ID_RATE_LIMITS), ServerLine::Other);
        }

        #[test]
        fn initialize_request_is_well_formed_jsonrpc() {
            let line = initialize_line();
            let value: serde_json::Value = serde_json::from_str(line.trim()).expect("json");
            assert_eq!(value["jsonrpc"], "2.0");
            assert_eq!(value["method"], "initialize");
            assert_eq!(value["id"], ID_INITIALIZE);
            assert!(value.pointer("/params/clientInfo/name").is_some());
            assert!(line.ends_with('\n'), "app-server reads line-delimited JSON");
        }

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
        /// Path to the operator's `auth.json`, resolved by the caller through the
        /// driver's own resolution so both read the identical file.
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
            Some((_, key)) => Ok(key.to_owned()),
            None => Err(TokenError {
                kind: DriverQuotaFailureKind::NotAuthenticated,
                reason: "Grok auth.json has no signed-in account — sign in with `grok login`".to_owned(),
            }),
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
                "https://auth.example::acct": { "key": "token-a", "expires_at": "2026-09-01T00:00:00Z" }
            });
            assert_eq!(extract_bearer(&doc).ok(), Some("token-a".to_owned()));
        }

        #[test]
        fn furthest_future_account_wins_when_several_are_present() {
            let doc = serde_json::json!({
                "issuer-a::1": { "key": "old", "expires_at": "2026-01-01T00:00:00Z" },
                "issuer-b::2": { "key": "new", "expires_at": "2026-09-01T00:00:00Z" },
            });
            assert_eq!(extract_bearer(&doc).ok(), Some("new".to_owned()));
        }

        #[test]
        fn empty_document_reads_as_not_authenticated() {
            let err = extract_bearer(&serde_json::json!({})).unwrap_err();
            assert_eq!(err.kind, DriverQuotaFailureKind::NotAuthenticated);
        }

        #[test]
        fn record_without_a_key_is_skipped_rather_than_treated_as_a_token() {
            let doc = serde_json::json!({ "issuer::1": { "expires_at": "2026-09-01T00:00:00Z" } });
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
