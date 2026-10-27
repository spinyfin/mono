//! Transient-failure classification for a failed CI job, and the per-head
//! retry budget the engine's auto-retry sweep enforces.
//!
//! The engine retries a failed required check only on *positive* evidence
//! that the failure came from infrastructure rather than from the change.
//! Evidence is consulted in order of how structured it is:
//!
//! 1. Buildkite's own job record (`signal_reason`, `exit_status`, `state`):
//!    an agent that stopped or refused the job, or a job that never produced
//!    an exit status, is infrastructure by definition.
//! 2. A tool's distinct machine-readable outcome: checkleft exits `3` when
//!    it could not determine what changed or its GitHub call timed out, as
//!    opposed to `1` for a real finding (`tools/checkleft/README.md`, "Exit
//!    codes"). A failed PR-description lookup that answers HTTP 5xx is **not**
//!    yet a distinct outcome: checkleft reports it as an error finding with
//!    exit `1`, so that case (the original incident) is classified by step 3
//!    from the log text, not by exit status.
//! 3. A conservative allowlist of upstream-unavailable log signatures (HTTP
//!    5xx from an API, gateway timeouts, connection resets, DNS failures),
//!    applied only when the same log carries no deterministic marker (a
//!    failing test target, a compiler error, a lint finding).
//!
//! Anything else is `Indeterminate` and is never retried: a deterministic
//! failure keeps flowing to the existing CI-fix revision path.
//!
//! The constants below are derived from measured mono pipeline history
//! (2026-10-06 → 2026-10-21, 2,323 builds): one positively transient PR
//! failure (an HTTP 504 resolving the PR description), two checkleft
//! input-unavailable exits on `main` builds, and no failure that needed
//! more than one retry to clear. The full table — including retry
//! clear times per job — is in `tools/boss/docs/blocked-pr-sweep.md`
//! ("Measurements behind the limits").

/// Maximum number of job retries the engine will request for one PR head
/// (`(work_item, head_sha)`), across all of its failing checks. Every
/// observed transient failure cleared on its first retry, and at most two
/// jobs on one build were ever retried together, so two is one full retry
/// per failing check with a 2x margin over the observed need. A head that
/// keeps failing transiently past this falls through to the existing
/// CI-fix revision path, where the failing check and reason are surfaced.
pub const TRANSIENT_RETRY_BUDGET_PER_HEAD: u32 = 2;

/// How many trailing log lines the engine reads for classification. A
/// failing `bazel-build-test` job uploads every test log as an artifact
/// after the failure, so its decisive `FAIL: //…` line sits 700–1,100 lines
/// before the end of the log in every sampled failure (five jobs, logs of
/// 1,159–1,683 lines); a window that misses it would call a real test
/// failure "indeterminate" instead of "deterministic". 5,000 lines covers
/// the longest sampled log three times over while still bounding the read.
pub const LOG_TAIL_LINES: usize = 5_000;

/// What the engine observed about one failed Buildkite job.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobEvidence {
    /// Buildkite job `state` (`failed`, `timed_out`, `canceled`, …).
    pub state: String,
    /// Buildkite job `exit_status`; `None` when the job never reported one.
    pub exit_status: Option<i64>,
    /// Buildkite job `signal_reason` (`agent_stop`, `agent_refused`,
    /// `process_run_error`, `cancel`, …); `None` when the job exited on its own.
    pub signal_reason: Option<String>,
    /// Cleaned (no ANSI / Buildkite timestamp escapes) tail of the job log.
    pub log_tail: Option<String>,
}

/// The infrastructure class a transient verdict names. Stored verbatim in
/// the `ci_job_retries` ledger and rendered on the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransientSignal {
    /// The Buildkite agent stopped, refused, or could not start the job.
    AgentLost,
    /// The check tool reported it could not gather its inputs (checkleft
    /// exit 3 with its PR-description-unavailable status line).
    CheckInputUnavailable,
    /// An upstream dependency (an API, a package mirror, DNS) was
    /// unreachable or answered 5xx.
    UpstreamUnavailable,
}

impl TransientSignal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AgentLost => "agent_lost",
            Self::CheckInputUnavailable => "check_input_unavailable",
            Self::UpstreamUnavailable => "upstream_unavailable",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "agent_lost" => Some(Self::AgentLost),
            "check_input_unavailable" => Some(Self::CheckInputUnavailable),
            "upstream_unavailable" => Some(Self::UpstreamUnavailable),
            _ => None,
        }
    }
}

/// Classification of one failed job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Positive infrastructure evidence; safe to retry. `summary` is the
    /// short card label ("HTTP 504", "agent stopped"); `evidence` is the
    /// structured field or log line that proved it.
    Transient {
        signal: TransientSignal,
        summary: String,
        evidence: String,
    },
    /// The log carries a marker that the change itself is at fault.
    Deterministic { summary: String, evidence: String },
    /// No positive evidence either way. Never retried.
    Indeterminate { reason: String },
}

impl Verdict {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Transient { .. } => "transient",
            Self::Deterministic { .. } => "deterministic",
            Self::Indeterminate { .. } => "indeterminate",
        }
    }

    /// Short human label for the card, whatever the verdict.
    pub fn summary(&self) -> &str {
        match self {
            Self::Transient { summary, .. } | Self::Deterministic { summary, .. } => summary,
            Self::Indeterminate { reason } => reason,
        }
    }

    /// The proving field / line, or the reason nothing could be proved.
    pub fn evidence(&self) -> &str {
        match self {
            Self::Transient { evidence, .. } | Self::Deterministic { evidence, .. } => evidence,
            Self::Indeterminate { reason } => reason,
        }
    }
}

/// Buildkite `signal_reason` values that mean the agent, not the command,
/// ended the job. `cancel` is deliberately absent: a cancelled job is a
/// human or queue decision, not a transient fault.
const AGENT_SIGNAL_REASONS: &[(&str, &str)] = &[
    ("agent_stop", "agent stopped"),
    ("agent_refused", "agent refused the job"),
    ("process_run_error", "agent could not start the job"),
];

/// Exit code checkleft uses for "could not gather inputs" (its
/// `EXIT_CHANGESET_UNDETERMINED`).
const CHECKLEFT_INPUT_UNAVAILABLE_EXIT: i64 = 3;

/// Lines checkleft prints when its PR-description input could not be
/// resolved. Any one of them, together with exit status 3, is the tool's
/// own statement that it did not evaluate the change.
const CHECKLEFT_INPUT_UNAVAILABLE_LINES: &[&str] = &[
    "PR description surface: UNAVAILABLE",
    "GitHub API call timed out",
    "GitHub API PR description lookup",
];

/// Log markers that prove the change (not infrastructure) failed. Any one of
/// these vetoes a transient verdict for the whole job.
const DETERMINISTIC_MARKERS: &[(&str, &str)] = &[
    ("FAIL: //", "test target failed"),
    ("TIMEOUT: //", "test target timed out"),
    ("FAILED: Build did NOT complete", "build failed"),
    ("error[E", "compiler error"),
    ("** TEST FAILED **", "test failed"),
    ("** BUILD FAILED **", "build failed"),
];

/// Upstream-unavailable log signatures: `(needle, summary)`. Matched
/// case-sensitively against each log line; the summary is the card label.
const UPSTREAM_SIGNATURES: &[(&str, &str)] = &[
    ("HTTP 502", "HTTP 502"),
    ("HTTP 503", "HTTP 503"),
    ("HTTP 504", "HTTP 504"),
    ("502 Bad Gateway", "HTTP 502"),
    ("503 Service Unavailable", "HTTP 503"),
    ("504 Gateway Timeout", "HTTP 504"),
    ("Gateway Timeout", "gateway timeout"),
    ("GitHub API call timed out", "GitHub API timeout"),
    ("lookup transport error", "GitHub API unreachable"),
    ("connection reset by peer", "connection reset"),
    ("Connection reset by peer", "connection reset"),
    ("Could not resolve host", "DNS failure"),
    ("Temporary failure in name resolution", "DNS failure"),
    ("TLS handshake timeout", "TLS handshake timeout"),
];

/// Bazel prints `Error downloading [<url>] to <path>: <cause>` for every
/// failed fetch, permanent or not. It is evidence only together with a
/// retryable cause on the same line.
const DOWNLOAD_FAILURE_NEEDLE: &str = "Error downloading [";

/// Causes that make a failed download change-caused or permanent: a wrong
/// URL, missing credentials, or a pinned checksum that does not match.
const PERMANENT_DOWNLOAD_CAUSES: &[&str] = &[
    "returned 404",
    "404 Not Found",
    "Not Found",
    "returned 401",
    "returned 403",
    "Unauthorized",
    "Forbidden",
    "hecksum",
    "SHA-256",
    "sha256",
];

/// Causes that name an unreachable or overloaded server, with the card label.
const RETRYABLE_DOWNLOAD_CAUSES: &[(&str, &str)] = &[
    ("timed out", "dependency download timed out"),
    ("Timeout", "dependency download timed out"),
    ("onnection reset", "dependency download connection reset"),
    ("returned 500", "dependency download HTTP 500"),
    ("returned 502", "dependency download HTTP 502"),
    ("returned 503", "dependency download HTTP 503"),
    ("returned 504", "dependency download HTTP 504"),
    ("500 Internal Server Error", "dependency download HTTP 500"),
    ("Service Unavailable", "dependency download HTTP 503"),
    ("Bad Gateway", "dependency download HTTP 502"),
];

/// Classify one failed job from its structured record and cleaned log tail.
pub fn classify_job(evidence: &JobEvidence) -> Verdict {
    // 1. Structured Buildkite signals.
    if let Some(reason) = evidence.signal_reason.as_deref()
        && let Some((_, summary)) = AGENT_SIGNAL_REASONS.iter().find(|(r, _)| *r == reason)
    {
        return Verdict::Transient {
            signal: TransientSignal::AgentLost,
            summary: (*summary).to_owned(),
            evidence: format!("buildkite job signal_reason={reason}"),
        };
    }
    if evidence.state == "failed" && evidence.exit_status == Some(-1) {
        return Verdict::Transient {
            signal: TransientSignal::AgentLost,
            summary: "agent lost".to_owned(),
            evidence: "buildkite job failed with exit_status=-1 (no exit status: agent lost)".to_owned(),
        };
    }

    let Some(log) = evidence.log_tail.as_deref() else {
        return Verdict::Indeterminate {
            reason: format!(
                "no log available (state={}, exit_status={})",
                evidence.state,
                exit_label(evidence.exit_status)
            ),
        };
    };

    // 2. Deterministic markers veto everything below.
    if let Some((line, summary)) = first_deterministic_marker(log) {
        return Verdict::Deterministic {
            summary: summary.to_owned(),
            evidence: line,
        };
    }

    // 3. The tool's own machine-readable "input unavailable" outcome.
    if evidence.exit_status == Some(CHECKLEFT_INPUT_UNAVAILABLE_EXIT)
        && let Some(line) = first_line_containing(log, CHECKLEFT_INPUT_UNAVAILABLE_LINES)
    {
        return Verdict::Transient {
            signal: TransientSignal::CheckInputUnavailable,
            summary: upstream_summary(&line).unwrap_or("check input unavailable").to_owned(),
            evidence: format!("exit status 3 (input undetermined): {line}"),
        };
    }

    // 4. Upstream-unavailable signatures.
    for line in log.lines() {
        if let Some(summary) = upstream_summary(line) {
            return Verdict::Transient {
                signal: TransientSignal::UpstreamUnavailable,
                summary: summary.to_owned(),
                evidence: truncate(line.trim(), 300),
            };
        }
    }

    Verdict::Indeterminate {
        reason: format!(
            "no transient evidence (state={}, exit_status={})",
            evidence.state,
            exit_label(evidence.exit_status)
        ),
    }
}

fn exit_label(exit: Option<i64>) -> String {
    exit.map(|e| e.to_string()).unwrap_or_else(|| "none".to_owned())
}

fn upstream_summary(line: &str) -> Option<&'static str> {
    UPSTREAM_SIGNATURES
        .iter()
        .find(|(needle, _)| line.contains(needle))
        .map(|(_, summary)| *summary)
        .or_else(|| download_failure_summary(line))
}

/// A failed Bazel download is transient only when the same line names a
/// retryable cause and no permanent one.
fn download_failure_summary(line: &str) -> Option<&'static str> {
    if !line.contains(DOWNLOAD_FAILURE_NEEDLE) {
        return None;
    }
    let cause = download_cause(line);
    if PERMANENT_DOWNLOAD_CAUSES.iter().any(|p| cause.contains(p)) {
        return None;
    }
    RETRYABLE_DOWNLOAD_CAUSES
        .iter()
        .find(|(needle, _)| cause.contains(needle))
        .map(|(_, summary)| *summary)
}

/// The text after the URL on a download-failure line. The URL itself may
/// contain "404" or "403"; only the cause is judged.
fn download_cause(line: &str) -> &str {
    line.split_once(']').map_or(line, |(_, rest)| rest)
}

/// Whether `line` is a failed download whose cause is permanent (wrong URL,
/// missing credentials, checksum mismatch).
fn is_permanent_download_failure(line: &str) -> bool {
    if !line.contains(DOWNLOAD_FAILURE_NEEDLE) {
        return false;
    }
    let cause = download_cause(line);
    PERMANENT_DOWNLOAD_CAUSES.iter().any(|p| cause.contains(p))
}

fn first_line_containing(log: &str, needles: &[&str]) -> Option<String> {
    log.lines()
        .find(|line| needles.iter().any(|n| line.contains(n)))
        .map(|line| truncate(line.trim(), 300))
}

/// First log line proving the change failed, with its summary. A checkleft
/// finding line (`error[<check>]: …`) counts, except the input-unavailable
/// refusal, which is the tool declining to report, not a finding about the
/// change.
fn first_deterministic_marker(log: &str) -> Option<(String, &'static str)> {
    for line in log.lines() {
        let trimmed = line.trim();
        if let Some((_, summary)) = DETERMINISTIC_MARKERS.iter().find(|(m, _)| trimmed.contains(m)) {
            return Some((truncate(trimmed, 300), summary));
        }
        if is_check_finding_line(trimmed) {
            return Some((truncate(trimmed, 300), "check finding"));
        }
        // A pin, checksum or auth defect is the change's fault; it vetoes
        // any transient evidence elsewhere in the log or on the same line.
        if is_permanent_download_failure(trimmed) {
            return Some((truncate(trimmed, 300), "dependency download failed permanently"));
        }
    }
    None
}

/// `error[<check-id>]: <message>` from checkleft's human output, excluding
/// the "PR description could not be resolved; refusing to report clean"
/// refusal (which is not a finding about the change).
fn is_check_finding_line(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("error[") else {
        return false;
    };
    let Some((id, message)) = rest.split_once("]: ") else {
        return false;
    };
    // `error[E0308]`-style rustc codes are covered by DETERMINISTIC_MARKERS;
    // a checkleft id always contains a `/` (`boss-ism/pr-text-leakage`).
    id.contains('/') && !message.contains("could not be resolved")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// Strip Buildkite's per-line timestamp escapes (`ESC _bk;t=<ms> BEL`),
/// ANSI SGR colour sequences, and carriage returns from a raw `bk job log`
/// body so the signature matching above sees plain text.
pub fn clean_buildkite_log(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                // OSC/APC-style `ESC _ … BEL` (Buildkite timestamps).
                Some('_') => {
                    for inner in chars.by_ref() {
                        if inner == '\u{07}' {
                            break;
                        }
                    }
                }
                // CSI `ESC [ … <final byte>` (colours, cursor moves).
                Some('[') => {
                    chars.next();
                    for inner in chars.by_ref() {
                        if ('@'..='~').contains(&inner) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

/// Keep the last `n` lines of `log`.
pub fn tail_lines_of(log: &str, n: usize) -> String {
    let lines: Vec<&str> = log.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Whether one more retry fits the per-head budget.
pub fn retry_allowed(retries_used: u32, additional: u32, budget: u32) -> bool {
    retries_used.saturating_add(additional) <= budget
}

/// Card label while a retry is in flight: `CI retrying: HTTP 504 in checks (1/2)`.
pub fn retrying_label(summary: &str, check_name: &str, attempt: u32, budget: u32) -> String {
    format!("CI retrying: {summary} in {check_name} ({attempt}/{budget})")
}

/// Card label once the budget is spent: `CI retry budget exhausted (2/2): HTTP 504 in checks`.
pub fn exhausted_label(summary: &str, check_name: &str, used: u32, budget: u32) -> String {
    format!("CI retry budget exhausted ({used}/{budget}): {summary} in {check_name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(exit: i64, log: &str) -> JobEvidence {
        JobEvidence {
            state: "failed".to_owned(),
            exit_status: Some(exit),
            signal_reason: None,
            log_tail: Some(log.to_owned()),
        }
    }

    /// The incident log from mono build 21619 (2026-10-20), as checkleft
    /// printed it before it had a distinct exit code for this case.
    const PR_DESCRIPTION_504_LOG: &str = "\
--- [checks] running checks
repobin: building //tools/checkleft:checkleft...
checkleft: PR description surface: UNAVAILABLE — GitHub API PR description lookup returned HTTP 504 Gateway Timeout
checks: running checks on 12 modified files
error[boss-ism/pr-text-leakage]: PR description could not be resolved; refusing to report clean. GitHub API PR description lookup returned HTTP 504 Gateway Timeout
  --> <pr description>
   = to resolve: Ensure GitHub authentication is available (CHECKLEFT_GH_TOKEN / CHECKS_GITHUB_TOKEN / GH_TOKEN / GITHUB_TOKEN, or `gh auth login`) and the API is reachable, then re-run. A check that cannot read the PR description must not pass.

warning[boss-ism/file-text-leakage]: something unrelated
summary: 1 error(s), 1 warning(s), 0 info finding(s)
^^^ +++
🚨 Error: The command exited with status 1
";

    fn download_line(cause: &str) -> String {
        format!(
            "ERROR: An error occurred during the fetch of repository 'foo':\n   Traceback\n\tError downloading [https://example.com/v404/foo.tar.gz] to /tmp/out/foo.tar.gz: {cause}"
        )
    }

    #[test]
    fn bazel_download_failures_need_a_retryable_cause() {
        for permanent in [
            "GET returned 404 Not Found",
            "GET returned 403 Forbidden",
            "GET returned 401 Unauthorized",
            "Checksum was abc but wanted def",
            "",
        ] {
            let verdict = classify_job(&failed(1, &download_line(permanent)));
            assert!(
                !matches!(verdict, Verdict::Transient { .. }),
                "{permanent:?} must not be retried: {verdict:?}"
            );
        }
        for retryable in [
            "Read timed out",
            "Connect timed out",
            "GET returned 503 Service Unavailable",
            "GET returned 502 Bad Gateway",
            "Connection reset",
        ] {
            let verdict = classify_job(&failed(1, &download_line(retryable)));
            assert!(
                matches!(
                    verdict,
                    Verdict::Transient {
                        signal: TransientSignal::UpstreamUnavailable,
                        ..
                    }
                ),
                "{retryable:?} should be transient: {verdict:?}"
            );
        }
    }

    #[test]
    fn permanent_download_failure_vetoes_earlier_transient_evidence() {
        for permanent in [
            "Checksum was abc but wanted def",
            "GET returned 404 Not Found",
            "GET returned 401 Unauthorized",
            "GET returned 403 Forbidden",
        ] {
            let log = format!(
                "fetch: connection reset by peer\nGET returned HTTP 504\n{}",
                download_line(permanent)
            );
            let verdict = classify_job(&failed(1, &log));
            assert!(
                matches!(verdict, Verdict::Deterministic { .. }),
                "{permanent:?} after transient noise must be deterministic: {verdict:?}"
            );
        }
    }

    #[test]
    fn single_download_line_with_permanent_and_upstream_cause_is_not_retried() {
        let log = download_line("GET returned 404 Not Found after connection reset by peer");
        let verdict = classify_job(&failed(1, &log));
        assert!(matches!(verdict, Verdict::Deterministic { .. }), "{verdict:?}");
    }

    /// A real `boss-ism/pr-text-leakage` violation (mono build 20351).
    const PR_TEXT_LEAKAGE_LOG: &str = "\
checkleft: PR description surface: scanned (8804 bytes)
checks: running checks on 15 modified files
error[boss-ism/pr-text-leakage]: Do not refer to \"the operator\" in the PR description or commit message; state the reason directly instead of naming who asked for it.
  --> <pr description>
summary: 1 error(s), 0 warning(s), 0 info finding(s)
🚨 Error: The command exited with status 1
";

    /// A real test failure (mono build 20378, engine_lib_test shard 2).
    const BAZEL_TEST_FAILURE_LOG: &str = "\
INFO: From Testing //tools/boss/engine/core:engine_lib_test (shard 2 of 11):
FAIL: //tools/boss/engine/core:engine_lib_test (shard 2 of 11) (Exit 101) (see /mnt/ssd/bazel/testlogs/tools/boss/engine/core/engine_lib_test/shard_2_of_11/test.log)
INFO: Build completed, 1 test FAILED, 2012 total actions
//tools/boss/engine/core:engine_lib_test                                 FAILED in 1 out of 11 in 45.2s
Executed 1 out of 1 test: 1 fails locally.
🚨 Error: The command exited with status 1
";

    /// The main-branch exit-3 case (mono build 21555).
    const CHECKLEFT_TIMEOUT_EXIT3_LOG: &str = "\
--- [checks] running checks
error: GitHub API call timed out after 30 seconds while looking up the open PR for a branch
^^^ +++
🚨 Error: The command exited with status 3
";

    #[test]
    fn pr_description_504_refusal_is_transient_upstream() {
        let v = classify_job(&failed(1, PR_DESCRIPTION_504_LOG));
        match v {
            Verdict::Transient {
                signal,
                summary,
                evidence,
            } => {
                assert_eq!(signal, TransientSignal::UpstreamUnavailable);
                assert_eq!(summary, "HTTP 504");
                assert!(evidence.contains("HTTP 504 Gateway Timeout"), "{evidence}");
            }
            other => panic!("expected transient, got {other:?}"),
        }
    }

    #[test]
    fn pr_description_unavailable_with_exit_3_is_check_input_unavailable() {
        let v = classify_job(&failed(3, PR_DESCRIPTION_504_LOG));
        match v {
            Verdict::Transient { signal, summary, .. } => {
                assert_eq!(signal, TransientSignal::CheckInputUnavailable);
                assert_eq!(summary, "HTTP 504");
            }
            other => panic!("expected transient, got {other:?}"),
        }
    }

    #[test]
    fn github_timeout_exit_3_is_check_input_unavailable() {
        let v = classify_job(&failed(3, CHECKLEFT_TIMEOUT_EXIT3_LOG));
        match v {
            Verdict::Transient { signal, summary, .. } => {
                assert_eq!(signal, TransientSignal::CheckInputUnavailable);
                assert_eq!(summary, "GitHub API timeout");
            }
            other => panic!("expected transient, got {other:?}"),
        }
    }

    #[test]
    fn real_lint_finding_is_deterministic() {
        let v = classify_job(&failed(1, PR_TEXT_LEAKAGE_LOG));
        match v {
            Verdict::Deterministic { summary, evidence } => {
                assert_eq!(summary, "check finding");
                assert!(evidence.starts_with("error[boss-ism/pr-text-leakage]"), "{evidence}");
            }
            other => panic!("expected deterministic, got {other:?}"),
        }
    }

    #[test]
    fn real_test_failure_is_deterministic() {
        let v = classify_job(&failed(1, BAZEL_TEST_FAILURE_LOG));
        match v {
            Verdict::Deterministic { summary, evidence } => {
                assert_eq!(summary, "test target failed");
                assert!(evidence.starts_with("FAIL: //tools/boss/engine/core:engine_lib_test"));
            }
            other => panic!("expected deterministic, got {other:?}"),
        }
    }

    #[test]
    fn upstream_signature_next_to_a_test_failure_is_still_deterministic() {
        // A flaky-network warning mid-log must not launder a real failure.
        let log = format!("WARNING: Remote Cache: HTTP 503 from cache\n{BAZEL_TEST_FAILURE_LOG}");
        assert!(matches!(classify_job(&failed(1, &log)), Verdict::Deterministic { .. }));
    }

    #[test]
    fn exit_3_without_checkleft_corroboration_is_indeterminate() {
        // Exit 3 alone (e.g. a shallow clone with no merge base) is not
        // evidence of a transient fault.
        let v = classify_job(&failed(3, "checkleft: could not determine changes: no merge base\n"));
        assert!(matches!(v, Verdict::Indeterminate { .. }), "{v:?}");
    }

    #[test]
    fn agent_stop_signal_is_transient_regardless_of_log() {
        let evidence = JobEvidence {
            state: "failed".to_owned(),
            exit_status: Some(-1),
            signal_reason: Some("agent_stop".to_owned()),
            log_tail: Some(BAZEL_TEST_FAILURE_LOG.to_owned()),
        };
        match classify_job(&evidence) {
            Verdict::Transient { signal, summary, .. } => {
                assert_eq!(signal, TransientSignal::AgentLost);
                assert_eq!(summary, "agent stopped");
            }
            other => panic!("expected transient, got {other:?}"),
        }
    }

    #[test]
    fn missing_exit_status_is_agent_lost() {
        let evidence = JobEvidence {
            state: "failed".to_owned(),
            exit_status: Some(-1),
            signal_reason: None,
            log_tail: None,
        };
        assert!(matches!(
            classify_job(&evidence),
            Verdict::Transient {
                signal: TransientSignal::AgentLost,
                ..
            }
        ));
    }

    #[test]
    fn cancel_signal_is_not_transient() {
        let evidence = JobEvidence {
            state: "canceled".to_owned(),
            exit_status: None,
            signal_reason: Some("cancel".to_owned()),
            log_tail: None,
        };
        assert!(matches!(classify_job(&evidence), Verdict::Indeterminate { .. }));
    }

    #[test]
    fn plain_exit_1_without_evidence_is_indeterminate() {
        let v = classify_job(&failed(
            1,
            "some unrelated output\n🚨 Error: The command exited with status 1\n",
        ));
        match v {
            Verdict::Indeterminate { reason } => assert!(reason.contains("exit_status=1"), "{reason}"),
            other => panic!("expected indeterminate, got {other:?}"),
        }
    }

    #[test]
    fn verdict_accessors_name_kind_summary_and_evidence() {
        let v = classify_job(&failed(1, PR_DESCRIPTION_504_LOG));
        assert_eq!(v.kind(), "transient");
        assert_eq!(v.summary(), "HTTP 504");
        assert!(v.evidence().contains("HTTP 504"));
        let d = classify_job(&failed(1, PR_TEXT_LEAKAGE_LOG));
        assert_eq!(d.kind(), "deterministic");
        let i = classify_job(&failed(1, "nothing\n"));
        assert_eq!(i.kind(), "indeterminate");
    }

    #[test]
    fn signal_round_trips_through_its_string_form() {
        for signal in [
            TransientSignal::AgentLost,
            TransientSignal::CheckInputUnavailable,
            TransientSignal::UpstreamUnavailable,
        ] {
            assert_eq!(TransientSignal::parse(signal.as_str()), Some(signal));
        }
        assert_eq!(TransientSignal::parse("bogus"), None);
    }

    #[test]
    fn clean_buildkite_log_strips_timestamp_and_colour_escapes() {
        let raw = "\u{1b}_bk;t=1792517979290\u{07}\u{1b}[1;31merror\u{1b}[0m[\u{1b}[38;5;244mboss-ism/x\u{1b}[0m]: msg\r\r\n\u{1b}_bk;t=1\u{07}next\r\n";
        assert_eq!(clean_buildkite_log(raw), "error[boss-ism/x]: msg\nnext\n");
    }

    #[test]
    fn tail_lines_keeps_only_the_last_n() {
        assert_eq!(tail_lines_of("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(tail_lines_of("a\nb", 5), "a\nb");
    }

    #[test]
    fn retry_budget_is_per_head_and_counts_every_requested_retry() {
        let budget = TRANSIENT_RETRY_BUDGET_PER_HEAD;
        assert!(retry_allowed(0, 1, budget));
        assert!(retry_allowed(0, 2, budget));
        assert!(retry_allowed(1, 1, budget));
        assert!(
            !retry_allowed(1, 2, budget),
            "two failing checks after one retry exceed the budget"
        );
        assert!(!retry_allowed(2, 1, budget), "budget exhausted after two retries");
        assert!(!retry_allowed(0, 3, budget));
    }

    #[test]
    fn card_labels_name_signal_check_and_attempt() {
        assert_eq!(
            retrying_label("HTTP 504", "buildkite/mono/checks", 1, 2),
            "CI retrying: HTTP 504 in buildkite/mono/checks (1/2)"
        );
        assert_eq!(
            exhausted_label("HTTP 504", "buildkite/mono/checks", 2, 2),
            "CI retry budget exhausted (2/2): HTTP 504 in buildkite/mono/checks"
        );
    }
}
