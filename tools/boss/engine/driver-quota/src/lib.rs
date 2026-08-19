//! Per-driver provider quota probes and their cache.
//!
//! Boss drives three coding-agent CLIs, and each one's provider meters the
//! maintainer's subscription independently. Getting all three figures used to
//! mean starting each driver interactively and typing its slash command. This
//! crate obtains the same figures out of band and caches them, so the engine
//! can serve one comparable snapshot over RPC.
//!
//! # Mechanism per driver — three CLIs, three different answers
//!
//! | driver | mechanism | why |
//! |--------|-----------|-----|
//! | `claude` | `claude -p "/usage" --output-format json` | `/usage` is a **local** slash command: in print mode Claude Code executes it without a model turn (`num_turns: 0`, `total_cost_usd: 0`) and prints the provider's report in the JSON envelope's `result`. No credential ever passes through Boss — the CLI reads its own keychain entry. |
//! | `codex` | `codex app-server` stdio JSON-RPC → `account/rateLimits/read` (via the shared client in `boss-engine-codex-hook-trust`) | Codex exposes no `usage`/`status` subcommand, but its app-server protocol has a first-class, machine-readable method for exactly this. One handshake, no thread started, no model turn. Again no credential passes through Boss: the child reads `$CODEX_HOME/auth.json` itself. |
//! | `grok` | HTTPS GET of the CLI's own billing/credits endpoint | Grok's `/usage` is an in-TUI extension with no CLI equivalent, and headless `grok -p "/usage"` treats the text as a *prompt to the model* — it would burn tokens and answer with prose. The endpoint the extension itself calls is the only non-interactive route, so this is the one driver where Boss handles a bearer token. It is read from the same `auth.json` the driver reads, held in memory for one request, and never logged, echoed, or persisted. |
//!
//! # What this must never become
//!
//! Nothing here estimates. If a probe fails, the driver's entry is a typed
//! [`boss_protocol::DriverQuotaOutcome::Unavailable`] — never a blank, never a
//! zero, never last week's figure passed off as current. And no figure here
//! is ever derived from Boss's own token accounting: that measures a
//! different thing (only work Boss dispatched) and presenting it as the
//! provider's view would mislead precisely when headroom matters.
//!
//! # Out-of-band by construction
//!
//! A probe is a short-lived child process (or one HTTPS request) owned by
//! this crate. It never allocates a worker slot, never creates an execution
//! row, never touches the pane/tmux layer, and never asks the dispatcher for
//! anything — see [`cache::QuotaCache`] and the crate's tests. The cache is
//! lazy: no probe runs until something asks for a snapshot, so engine startup
//! is untouched.

pub mod cache;
pub mod parse {
    //! Pure parsers turning each driver's raw probe output into a
    //! [`DriverQuotaOutcome`]. No IO here — this is the half that a change in a
    //! provider's output format breaks, so it is the half that is unit-tested
    //! against captured real output.
    //!
    //! Every parser fails loudly. There is no "assume zero", no "take the first
    //! number you see", and no partial reading: output that does not match what
    //! the parser was written against comes back as
    //! [`DriverQuotaFailureKind::Unparseable`], which the UI renders as an
    //! explicit failure for that driver.

    use boss_protocol::{DriverQuotaFailureKind, DriverQuotaOutcome, DriverQuotaReading, DriverQuotaWindow};

    /// Provenance strings shown in the UI next to each figure.
    pub const SOURCE_CLAUDE: &str = "claude -p /usage";
    pub const SOURCE_CODEX: &str = "codex app-server account/rateLimits/read";
    pub const SOURCE_GROK: &str = "grok billing endpoint";

    pub(crate) fn unavailable(kind: DriverQuotaFailureKind, reason: impl Into<String>) -> DriverQuotaOutcome {
        DriverQuotaOutcome::Unavailable {
            kind,
            reason: reason.into(),
        }
    }

    // ---------------------------------------------------------------------------
    // Claude
    // ---------------------------------------------------------------------------

    /// Line prefix Claude Code's `/usage` prints for the all-models weekly
    /// window. The session ("Current session:") and per-model ("Current week
    /// (Fable)") lines are deliberately ignored: the pane compares the weekly
    /// window across drivers, and silently substituting a different window would
    /// be exactly the misleading-number failure this parser exists to prevent.
    const CLAUDE_WEEK_PREFIX: &str = "Current week (all models):";

    /// Parse the JSON envelope `claude -p "/usage" --output-format json` emits.
    ///
    /// `/usage` is handled locally by the CLI: the envelope comes back with
    /// `num_turns: 0` and `total_cost_usd: 0`, and the human-readable report
    /// lands in `result`. The figure inside it is fetched by Claude Code from
    /// the provider, not computed by Boss.
    pub fn parse_claude_usage_json(stdout: &str) -> DriverQuotaOutcome {
        let envelope: serde_json::Value = match serde_json::from_str(stdout) {
            Ok(v) => v,
            Err(err) => {
                return unavailable(
                    DriverQuotaFailureKind::Unparseable,
                    format!("`/usage` output was not JSON: {err}"),
                );
            }
        };
        if envelope.get("is_error").and_then(serde_json::Value::as_bool) == Some(true) {
            return unavailable(
                DriverQuotaFailureKind::ProbeFailed,
                "Claude Code reported an error running /usage".to_owned(),
            );
        }
        let Some(report) = envelope.get("result").and_then(serde_json::Value::as_str) else {
            return unavailable(
                DriverQuotaFailureKind::Unparseable,
                "`/usage` JSON had no `result` text".to_owned(),
            );
        };
        parse_claude_usage_report(report)
    }

    /// Parse the human-readable `/usage` report body.
    ///
    /// The line looks like:
    /// `Current week (all models): 3% used · resets Aug 25 at 7pm (America/Chicago)`
    /// — with the reset clause sometimes absent entirely.
    pub fn parse_claude_usage_report(report: &str) -> DriverQuotaOutcome {
        let Some(line) = report
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with(CLAUDE_WEEK_PREFIX))
        else {
            if report.contains("not logged in") || report.contains("/login") {
                return unavailable(
                    DriverQuotaFailureKind::NotAuthenticated,
                    "Claude Code is not logged in".to_owned(),
                );
            }
            return unavailable(
                DriverQuotaFailureKind::Unparseable,
                format!("`/usage` output had no \"{CLAUDE_WEEK_PREFIX}\" line"),
            );
        };
        let rest = line[CLAUDE_WEEK_PREFIX.len()..].trim();
        // "3% used · resets Aug 25 at 7pm (America/Chicago)" — split the
        // percentage off the front, keep the reset clause verbatim.
        let Some((percent_text, tail)) = rest.split_once('%') else {
            return unavailable(
                DriverQuotaFailureKind::Unparseable,
                format!("no percentage in `/usage` weekly line: {rest:?}"),
            );
        };
        let Ok(used_percent) = percent_text.trim().parse::<f64>() else {
            return unavailable(
                DriverQuotaFailureKind::Unparseable,
                format!("weekly percentage was not a number: {percent_text:?}"),
            );
        };
        let resets_at_text = tail
            .split_once("resets")
            .map(|(_, after)| after.trim().to_owned())
            .filter(|s| !s.is_empty());

        DriverQuotaOutcome::Reading(DriverQuotaReading {
            used_percent,
            window: DriverQuotaWindow::Weekly,
            resets_at_epoch_s: None,
            resets_at_text,
            source: SOURCE_CLAUDE.to_owned(),
        })
    }

    // ---------------------------------------------------------------------------
    // Codex
    // ---------------------------------------------------------------------------

    /// Parse the `result` payload of Codex's `account/rateLimits/read`.
    ///
    /// Shape (fields Boss reads only):
    /// ```json
    /// { "rateLimits": { "primary": { "usedPercent": 5,
    ///                                "windowDurationMins": 10080,
    ///                                "resetsAt": 1787346271 } } }
    /// ```
    /// `primary` is the plan-level limit — the one `/status` shows as the weekly
    /// limit. Per-model sub-limits under `rateLimitsByLimitId` are not merged in:
    /// they measure different windows and averaging them would invent a figure
    /// no provider reported.
    pub fn parse_codex_rate_limits(result: &serde_json::Value) -> DriverQuotaOutcome {
        let Some(primary) = result.pointer("/rateLimits/primary") else {
            // A logged-out app-server answers with a null/absent primary rather
            // than an error, so this is the shape "not authenticated" arrives in.
            if result.pointer("/rateLimits").is_some() {
                return unavailable(
                    DriverQuotaFailureKind::NotAuthenticated,
                    "Codex reported no plan rate limit — sign in with `codex login`".to_owned(),
                );
            }
            return unavailable(
                DriverQuotaFailureKind::Unparseable,
                "app-server reply had no `rateLimits` object".to_owned(),
            );
        };
        if primary.is_null() {
            return unavailable(
                DriverQuotaFailureKind::NotAuthenticated,
                "Codex reported no plan rate limit — sign in with `codex login`".to_owned(),
            );
        }
        let Some(used_percent) = primary.get("usedPercent").and_then(serde_json::Value::as_f64) else {
            return unavailable(
                DriverQuotaFailureKind::Unparseable,
                "`rateLimits.primary` had no numeric `usedPercent`".to_owned(),
            );
        };
        let window = primary
            .get("windowDurationMins")
            .and_then(serde_json::Value::as_u64)
            .map(|m| DriverQuotaWindow::from_minutes(m.min(u64::from(u32::MAX)) as u32))
            // Absent window: report the length we did not get rather than
            // asserting a week Codex never claimed.
            .unwrap_or(DriverQuotaWindow::Other { minutes: 0 });
        let resets_at_epoch_s = primary.get("resetsAt").and_then(serde_json::Value::as_i64);

        DriverQuotaOutcome::Reading(DriverQuotaReading {
            used_percent,
            window,
            resets_at_epoch_s,
            resets_at_text: None,
            source: SOURCE_CODEX.to_owned(),
        })
    }

    // ---------------------------------------------------------------------------
    // Grok
    // ---------------------------------------------------------------------------

    /// Parse the body of Grok's credits/billing endpoint — the same call the
    /// CLI's own `/usage` makes.
    ///
    /// Shape (fields Boss reads only):
    /// ```json
    /// { "config": { "creditUsagePercent": 3.0,
    ///               "currentPeriod": { "type": "USAGE_PERIOD_TYPE_WEEKLY",
    ///                                  "end": "2026-08-21T22:22:42.845319+00:00" } } }
    /// ```
    pub fn parse_grok_billing(body: &str) -> DriverQuotaOutcome {
        let payload: serde_json::Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(err) => {
                return unavailable(
                    DriverQuotaFailureKind::Unparseable,
                    format!("billing response was not JSON: {err}"),
                );
            }
        };
        // The endpoint has answered both bare and wrapped in `config`; accept
        // either rather than breaking on the wrapper.
        let config = payload.get("config").unwrap_or(&payload);
        let Some(used_percent) = config.get("creditUsagePercent").and_then(serde_json::Value::as_f64) else {
            return unavailable(
                DriverQuotaFailureKind::Unparseable,
                "billing response had no numeric `creditUsagePercent`".to_owned(),
            );
        };
        let window = match config
            .pointer("/currentPeriod/type")
            .and_then(serde_json::Value::as_str)
        {
            Some("USAGE_PERIOD_TYPE_WEEKLY") => DriverQuotaWindow::Weekly,
            // Any other period is reported as-is in the label rather than being
            // silently presented as the weekly figure the pane is comparing.
            _ => DriverQuotaWindow::Other {
                minutes: grok_period_minutes(config).unwrap_or(0),
            },
        };
        let resets_at_text = config
            .pointer("/currentPeriod/end")
            .and_then(serde_json::Value::as_str)
            .or_else(|| config.get("billingPeriodEnd").and_then(serde_json::Value::as_str))
            .map(str::to_owned);

        DriverQuotaOutcome::Reading(DriverQuotaReading {
            used_percent,
            window,
            resets_at_epoch_s: None,
            resets_at_text,
            source: SOURCE_GROK.to_owned(),
        })
    }

    /// Period length for a non-weekly Grok billing period. Returns `None` when
    /// either RFC-3339 bound is absent or unparseable — the caller then reports
    /// `minutes: 0`, i.e. "a period Boss could not size", never a fabricated
    /// one. Uses the real calendar duration so a February month is 28 days, not
    /// 31, and a period that crosses a year boundary is not a negative.
    fn grok_period_minutes(config: &serde_json::Value) -> Option<u32> {
        let start = chrono::DateTime::parse_from_rfc3339(config.pointer("/currentPeriod/start")?.as_str()?).ok()?;
        let end = chrono::DateTime::parse_from_rfc3339(config.pointer("/currentPeriod/end")?.as_str()?).ok()?;
        let minutes = end.signed_duration_since(start).num_minutes();
        u32::try_from(minutes).ok()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn reading(outcome: &DriverQuotaOutcome) -> &DriverQuotaReading {
            match outcome {
                DriverQuotaOutcome::Reading(r) => r,
                DriverQuotaOutcome::Unavailable { kind, reason } => {
                    panic!("expected a reading, got {kind:?}: {reason}")
                }
            }
        }

        fn failure_kind(outcome: &DriverQuotaOutcome) -> DriverQuotaFailureKind {
            match outcome {
                DriverQuotaOutcome::Unavailable { kind, .. } => *kind,
                DriverQuotaOutcome::Reading(r) => panic!("expected a failure, got {r:?}"),
            }
        }

        // --- Claude ----------------------------------------------------------

        /// Captured verbatim from `claude -p "/usage" --output-format json`.
        const CLAUDE_REPORT: &str = "You are currently using your subscription to power your Claude Code usage\n\
             \n\
             Current session: 4% used · resets Aug 19 at 3:40am (America/Chicago)\n\
             Current week (all models): 3% used · resets Aug 25 at 7pm (America/Chicago)\n\
             Current week (Fable): 0% used\n";

        #[test]
        fn claude_weekly_line_parsed_from_real_report() {
            let outcome = parse_claude_usage_report(CLAUDE_REPORT);
            let r = reading(&outcome);
            assert_eq!(r.used_percent, 3.0);
            assert_eq!(r.window, DriverQuotaWindow::Weekly);
            assert_eq!(r.resets_at_text.as_deref(), Some("Aug 25 at 7pm (America/Chicago)"));
            assert_eq!(r.source, SOURCE_CLAUDE);
        }

        #[test]
        fn claude_session_line_is_never_mistaken_for_the_weekly_one() {
            // Session line present, weekly line absent: must fail, not fall back
            // to the 4% session figure.
            let report = "Current session: 4% used · resets Aug 19 at 3:40am (America/Chicago)\n";
            assert_eq!(
                failure_kind(&parse_claude_usage_report(report)),
                DriverQuotaFailureKind::Unparseable
            );
        }

        #[test]
        fn claude_weekly_line_without_a_reset_clause_still_reads() {
            let outcome = parse_claude_usage_report("Current week (all models): 12% used\n");
            let r = reading(&outcome);
            assert_eq!(r.used_percent, 12.0);
            assert_eq!(r.resets_at_text, None);
        }

        #[test]
        fn claude_zero_percent_is_a_reading_not_a_failure() {
            let outcome = parse_claude_usage_report("Current week (all models): 0% used\n");
            assert_eq!(reading(&outcome).used_percent, 0.0);
        }

        #[test]
        fn claude_json_envelope_unwrapped() {
            let envelope = serde_json::json!({
                "is_error": false,
                "num_turns": 0,
                "total_cost_usd": 0,
                "result": CLAUDE_REPORT,
            });
            let outcome = parse_claude_usage_json(&envelope.to_string());
            assert_eq!(reading(&outcome).used_percent, 3.0);
        }

        #[test]
        fn claude_error_envelope_is_a_probe_failure() {
            let envelope = serde_json::json!({ "is_error": true, "result": "boom" });
            assert_eq!(
                failure_kind(&parse_claude_usage_json(&envelope.to_string())),
                DriverQuotaFailureKind::ProbeFailed
            );
        }

        #[test]
        fn claude_non_json_output_is_unparseable() {
            assert_eq!(
                failure_kind(&parse_claude_usage_json("not json at all")),
                DriverQuotaFailureKind::Unparseable
            );
        }

        #[test]
        fn claude_logged_out_report_is_an_auth_failure() {
            let outcome = parse_claude_usage_report("You are not logged in. Run /login to continue.");
            assert_eq!(failure_kind(&outcome), DriverQuotaFailureKind::NotAuthenticated);
        }

        // --- Codex -----------------------------------------------------------

        #[test]
        fn codex_primary_limit_parsed_from_real_reply() {
            let result = serde_json::json!({
                "rateLimits": {
                    "limitId": "codex",
                    "primary": { "usedPercent": 5, "windowDurationMins": 10080, "resetsAt": 1_787_346_271_i64 },
                    "secondary": serde_json::Value::Null,
                    "planType": "pro"
                }
            });
            let outcome = parse_codex_rate_limits(&result);
            let r = reading(&outcome);
            assert_eq!(r.used_percent, 5.0);
            assert_eq!(r.window, DriverQuotaWindow::Weekly);
            assert_eq!(r.resets_at_epoch_s, Some(1_787_346_271));
            assert_eq!(r.source, SOURCE_CODEX);
        }

        #[test]
        fn codex_non_weekly_window_is_not_relabelled_weekly() {
            let result = serde_json::json!({
                "rateLimits": { "primary": { "usedPercent": 40, "windowDurationMins": 300 } }
            });
            assert_eq!(
                reading(&parse_codex_rate_limits(&result)).window,
                DriverQuotaWindow::Other { minutes: 300 }
            );
        }

        #[test]
        fn codex_null_primary_reads_as_not_authenticated() {
            let result = serde_json::json!({ "rateLimits": { "primary": serde_json::Value::Null } });
            assert_eq!(
                failure_kind(&parse_codex_rate_limits(&result)),
                DriverQuotaFailureKind::NotAuthenticated
            );
        }

        #[test]
        fn codex_missing_rate_limits_object_is_unparseable() {
            assert_eq!(
                failure_kind(&parse_codex_rate_limits(&serde_json::json!({}))),
                DriverQuotaFailureKind::Unparseable
            );
        }

        #[test]
        fn codex_zero_percent_is_a_reading_not_a_failure() {
            let result = serde_json::json!({
                "rateLimits": { "primary": { "usedPercent": 0, "windowDurationMins": 10080 } }
            });
            assert_eq!(reading(&parse_codex_rate_limits(&result)).used_percent, 0.0);
        }

        // --- Grok ------------------------------------------------------------

        /// Captured verbatim from the endpoint the CLI's `/usage` calls.
        const GROK_BODY: &str = r#"{"config":{"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","start":"2026-08-14T22:22:42.845319+00:00","end":"2026-08-21T22:22:42.845319+00:00"},"creditUsagePercent":3.0,"onDemandCap":{"val":0},"isUnifiedBillingUser":true,"billingPeriodEnd":"2026-08-21T22:22:42.845319+00:00"}}"#;

        #[test]
        fn grok_weekly_credit_usage_parsed_from_real_body() {
            let outcome = parse_grok_billing(GROK_BODY);
            let r = reading(&outcome);
            assert_eq!(r.used_percent, 3.0);
            assert_eq!(r.window, DriverQuotaWindow::Weekly);
            assert_eq!(r.resets_at_text.as_deref(), Some("2026-08-21T22:22:42.845319+00:00"));
            assert_eq!(r.source, SOURCE_GROK);
        }

        #[test]
        fn grok_unwrapped_body_also_parses() {
            let body = r#"{"creditUsagePercent":11.5,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY"}}"#;
            assert_eq!(reading(&parse_grok_billing(body)).used_percent, 11.5);
        }

        #[test]
        fn grok_monthly_period_is_not_presented_as_weekly() {
            let body = r#"{"creditUsagePercent":50.0,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_MONTHLY","start":"2026-08-01T00:00:00+00:00","end":"2026-08-31T00:00:00+00:00"}}"#;
            let window = reading(&parse_grok_billing(body)).window;
            assert!(!matches!(window, DriverQuotaWindow::Weekly), "got {window:?}");
        }

        #[test]
        fn grok_february_monthly_period_is_28_days_not_31() {
            let body = r#"{"creditUsagePercent":50.0,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_MONTHLY","start":"2026-02-01T00:00:00+00:00","end":"2026-03-01T00:00:00+00:00"}}"#;
            assert_eq!(
                reading(&parse_grok_billing(body)).window,
                DriverQuotaWindow::Other { minutes: 28 * 24 * 60 }
            );
        }

        #[test]
        fn grok_period_crossing_a_year_boundary_is_not_zero() {
            let body = r#"{"creditUsagePercent":1.0,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_MONTHLY","start":"2025-12-15T00:00:00+00:00","end":"2026-01-15T00:00:00+00:00"}}"#;
            assert_eq!(
                reading(&parse_grok_billing(body)).window,
                DriverQuotaWindow::Other { minutes: 31 * 24 * 60 }
            );
        }

        #[test]
        fn grok_body_without_a_percentage_is_unparseable() {
            assert_eq!(
                failure_kind(&parse_grok_billing(r#"{"config":{}}"#)),
                DriverQuotaFailureKind::Unparseable
            );
        }

        #[test]
        fn grok_zero_percent_is_a_reading_not_a_failure() {
            let body = r#"{"creditUsagePercent":0.0,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY"}}"#;
            assert_eq!(reading(&parse_grok_billing(body)).used_percent, 0.0);
        }
    }
}
pub mod probes;

pub use cache::{DEFAULT_MIN_REFRESH_INTERVAL, DEFAULT_TTL, QuotaCache, QuotaLookup, QuotaProbeSet};

// ---------------------------------------------------------------------------
// The probe seam
// ---------------------------------------------------------------------------

use std::time::Duration;

use async_trait::async_trait;
use boss_protocol::DriverQuotaOutcome;

/// Per-driver deadline. Generous enough for a cold CLI start on a loaded
/// laptop, short enough that three probes in parallel cannot make the
/// Preferences pane feel hung. A probe that exceeds it is reported as
/// [`boss_protocol::DriverQuotaFailureKind::Timeout`], never left pending.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(25);

/// Reads one driver's provider-reported quota.
///
/// Implementations must return a [`DriverQuotaOutcome`] for every input,
/// including failure — a probe never returns an error type, because "we
/// could not tell" is itself a value the UI has to render.
#[async_trait]
pub trait DriverQuotaProbe: Send + Sync {
    /// Driver slug this probe answers for.
    fn driver(&self) -> &'static str;

    /// Run the probe. Must not log, return, or otherwise expose any
    /// credential, token, API key, or session identifier it encounters.
    async fn probe(&self) -> DriverQuotaOutcome;
}

/// Current wall-clock time in epoch seconds. Saturates rather than panics on
/// a clock set before 1970.
pub(crate) fn now_epoch_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Build the production probe set: one probe per implemented driver.
///
/// `grok_auth_path` must come from the driver's own resolution
/// (`boss_engine_driver::grok::resolve_grok_auth_source`) so the probe reads
/// the identical `auth.json` the Grok driver reads — this crate deliberately
/// does not re-derive that path, because two independent derivations of a
/// credential location is exactly how they drift apart.
pub fn default_probes(grok_auth_path: std::path::PathBuf) -> QuotaProbeSet {
    vec![
        std::sync::Arc::new(probes::claude::ClaudeQuotaProbe::default()),
        std::sync::Arc::new(probes::codex::CodexQuotaProbe::default()),
        std::sync::Arc::new(probes::grok::GrokQuotaProbe::new(grok_auth_path)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::DRIVER_QUOTA_ORDER;

    #[test]
    fn production_probe_set_covers_every_driver_the_snapshot_promises() {
        let probes = default_probes(std::path::PathBuf::from("/tmp/auth.json"));
        let slugs: Vec<&str> = probes.iter().map(|p| p.driver()).collect();
        assert_eq!(slugs, DRIVER_QUOTA_ORDER.to_vec());
    }
}
