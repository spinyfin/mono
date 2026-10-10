//! Retry policy for reaching an engine that is briefly unavailable.
//!
//! The engine is restarted routinely (an update, a crash-and-relaunch), so a
//! call can land while it is briefly unavailable.
//! [`BossClient`](crate::BossClient) rides that window out: connect-phase
//! failures are retried with exponential backoff and jitter until
//! [`RetryPolicy::max_wait`] is spent, then the call fails loudly with
//! [`EngineUnreachable`].
//!
//! This module owns the *schedule* and the *errors*; whether a request that
//! was already sent may be sent again lives in [`crate::replay`].
//!
//! ## Why 10 minutes
//!
//! A normal restart (stop, relaunch, migrations, socket bind) is a few
//! seconds to a minute. The default budget is deliberately generous so a
//! slow restart — a cold Bazel-built engine, a long migration — still lands
//! inside it; a wedged engine is not made worse by waiting, because the
//! caller was going to fail anyway and now fails with a message that says
//! how long it waited. Callers that need fast failure opt out with
//! `--no-retry` / `--engine-max-wait` / `BOSS_ENGINE_MAX_WAIT_SECS`.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::time::sleep;

/// Default total time a call waits for an unreachable engine.
pub const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(10 * 60);
/// First backoff sleep, before jitter.
pub const DEFAULT_INITIAL_DELAY: Duration = Duration::from_millis(250);
/// Upper bound on a single backoff sleep, before jitter.
pub const DEFAULT_MAX_DELAY: Duration = Duration::from_secs(5);
/// How often the "still waiting" notice repeats after the first one.
pub const DEFAULT_NOTICE_INTERVAL: Duration = Duration::from_secs(15);
/// Environment override for [`RetryPolicy::max_wait`], in whole seconds.
/// `0` disables retry entirely (fail on the first unreachable attempt).
pub const MAX_WAIT_ENV: &str = "BOSS_ENGINE_MAX_WAIT_SECS";

/// Where progress notices go. Always a side channel — never stdout — so
/// `--json` output stays machine-readable.
#[derive(Clone)]
pub struct NoticeSink(Arc<dyn Fn(&str) + Send + Sync>);

impl NoticeSink {
    pub fn new(sink: impl Fn(&str) + Send + Sync + 'static) -> Self {
        Self(Arc::new(sink))
    }

    /// One line per notice on stderr.
    pub fn stderr() -> Self {
        Self::new(|line| eprintln!("{line}"))
    }

    pub fn silent() -> Self {
        Self::new(|_| {})
    }

    pub(crate) fn emit(&self, line: &str) {
        (self.0)(line);
    }
}

impl fmt::Debug for NoticeSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NoticeSink(..)")
    }
}

/// How long and how fast to keep trying. Durations are plain fields so tests
/// inject a millisecond-scale budget instead of waiting on real time.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total budget across all attempts of one call. [`Duration::ZERO`]
    /// disables retry.
    pub max_wait: Duration,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub notice_interval: Duration,
    pub notices: NoticeSink,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_wait: DEFAULT_MAX_WAIT,
            initial_delay: DEFAULT_INITIAL_DELAY,
            max_delay: DEFAULT_MAX_DELAY,
            notice_interval: DEFAULT_NOTICE_INTERVAL,
            notices: NoticeSink::stderr(),
        }
    }
}

impl RetryPolicy {
    /// The default policy, with `BOSS_ENGINE_MAX_WAIT_SECS` applied if set.
    /// An unparseable value is ignored with a warning on stderr rather
    /// than silently disabling retry.
    pub fn from_env() -> Self {
        let mut policy = Self::default();
        match std::env::var(MAX_WAIT_ENV) {
            Ok(raw) if !raw.trim().is_empty() => match raw.trim().parse::<u64>() {
                Ok(secs) => policy.max_wait = Duration::from_secs(secs),
                Err(_) => eprintln!(
                    "boss: warning: {MAX_WAIT_ENV}={raw:?} is not a whole number of seconds; using the default of {}s",
                    DEFAULT_MAX_WAIT.as_secs()
                ),
            },
            _ => {}
        }
        policy
    }

    /// Fail on the first unreachable attempt.
    pub fn disabled() -> Self {
        Self::default().with_max_wait(Duration::ZERO)
    }

    pub fn with_max_wait(mut self, max_wait: Duration) -> Self {
        self.max_wait = max_wait;
        self
    }

    pub fn with_delays(mut self, initial: Duration, max: Duration) -> Self {
        self.initial_delay = initial;
        self.max_delay = max;
        self
    }

    pub fn with_notices(mut self, notices: NoticeSink) -> Self {
        self.notices = notices;
        self
    }

    pub fn is_enabled(&self) -> bool {
        !self.max_wait.is_zero()
    }

    /// Sleep before attempt number `attempt` (0-based): `initial * 2^attempt`
    /// capped at `max_delay`, then "equal jitter" — a point in
    /// `[base/2, base]` chosen by `jitter_unit` (`0.0..=1.0`) — so a herd of
    /// workers released by the same restart does not reconnect in lockstep.
    pub fn delay_for_attempt(&self, attempt: u32, jitter_unit: f64) -> Duration {
        boss_backoff::equal_jitter_at(self.base_delay(attempt), jitter_unit)
    }

    fn base_delay(&self, attempt: u32) -> Duration {
        boss_backoff::exponential_delay(self.initial_delay, self.max_delay, attempt)
    }
}

/// The engine could not be reached within the retry budget (or retry was
/// disabled and the first attempt failed). Nothing was sent.
#[derive(Debug, Clone)]
pub struct EngineUnreachable {
    pub socket_path: String,
    pub waited: Duration,
    pub attempts: u32,
    pub retry_enabled: bool,
    pub last_error: String,
}

impl fmt::Display for EngineUnreachable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.retry_enabled {
            write!(
                f,
                "boss engine is not reachable at {}: gave up after waiting {:.1}s ({} attempt{}); last error: {}. \
                 Start the engine, or raise the wait with --engine-max-wait / {MAX_WAIT_ENV}.",
                self.socket_path,
                self.waited.as_secs_f64(),
                self.attempts,
                if self.attempts == 1 { "" } else { "s" },
                self.last_error,
            )
        } else {
            write!(
                f,
                "boss engine is not reachable at {} (retry disabled): {}",
                self.socket_path, self.last_error,
            )
        }
    }
}

impl std::error::Error for EngineUnreachable {}

/// The connection dropped after a request was sent, and the request is not
/// safe to send again, so whether it took effect is unknown. The caller must
/// check state rather than blindly re-run.
#[derive(Debug, Clone)]
pub struct OutcomeUnknown {
    pub request: String,
    pub socket_path: String,
    pub detail: String,
}

impl fmt::Display for OutcomeUnknown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "outcome unknown: the connection to the engine at {} dropped after the `{}` request was sent ({}). \
             The engine may or may not have applied it, and it is not safe to resend automatically \
             (it could be applied twice). Check the current state before retrying.",
            self.socket_path, self.request, self.detail,
        )
    }
}

impl std::error::Error for OutcomeUnknown {}

/// Whether a connect-phase I/O error means "the engine is not (yet) there"
/// and is worth waiting out, as opposed to a configuration problem
/// (permission denied, path too long) that waiting cannot fix.
pub(crate) fn is_unreachable_kind(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::TimedOut
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::UnexpectedEof
    )
}

/// One call's retry bookkeeping: when it started, how many attempts it has
/// made, and when it last told the user it was waiting.
pub(crate) struct RetryState {
    policy: RetryPolicy,
    started: Instant,
    attempts: u32,
    last_notice: Option<Instant>,
}

impl RetryState {
    pub(crate) fn new(policy: &RetryPolicy) -> Self {
        Self {
            policy: policy.clone(),
            started: Instant::now(),
            attempts: 0,
            last_notice: None,
        }
    }

    pub(crate) fn policy(&self) -> &RetryPolicy {
        &self.policy
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Time left in the budget (the whole per-attempt bound when retry is
    /// disabled, so the single attempt still gets a real connect timeout).
    pub(crate) fn remaining(&self) -> Duration {
        self.policy.max_wait.saturating_sub(self.started.elapsed())
    }

    /// Absolute deadline for one connect attempt: the remaining budget
    /// (floored so a nearly spent budget still gets a real attempt), or
    /// `single_attempt` when retry is disabled and there is no budget.
    pub(crate) fn connect_deadline(&self, single_attempt: Duration) -> Instant {
        let span = if self.policy.is_enabled() {
            self.remaining().max(Duration::from_millis(50))
        } else {
            single_attempt
        };
        Instant::now() + span
    }

    /// Never wait past `total` (measured from this call's start). Used to
    /// keep a guarded create's reconnect inside the duplicate-guard window.
    pub(crate) fn limit_total(&mut self, total: Duration) {
        if self.policy.max_wait > total {
            self.policy.max_wait = total;
        }
    }

    /// Record a failed attempt. Returns `true` after sleeping if another
    /// attempt is allowed, `false` when the budget is spent (or retry is
    /// disabled) and the caller must give up.
    pub(crate) async fn backoff(&mut self, socket_path: &str, what: &str, last_error: &str) -> bool {
        self.attempts += 1;
        let elapsed = self.started.elapsed();
        if !self.policy.is_enabled() || elapsed >= self.policy.max_wait {
            return false;
        }
        let remaining = self.policy.max_wait - elapsed;
        let delay = boss_backoff::equal_jitter(self.policy.base_delay(self.attempts - 1)).min(remaining);

        let due = match self.last_notice {
            None => true,
            Some(at) => at.elapsed() >= self.policy.notice_interval,
        };
        if due {
            self.last_notice = Some(Instant::now());
            self.policy.notices.emit(&format!(
                "boss: engine not reachable at {socket_path} ({what}: {last_error}); \
                 retrying for up to {}s more",
                remaining.as_secs().max(1),
            ));
        }
        sleep(delay).await;
        true
    }

    pub(crate) fn unreachable(&self, socket_path: &str, last_error: &str) -> EngineUnreachable {
        EngineUnreachable {
            socket_path: socket_path.to_owned(),
            waited: self.started.elapsed(),
            attempts: self.attempts,
            retry_enabled: self.policy.is_enabled(),
            last_error: last_error.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RetryPolicy {
        RetryPolicy::default()
    }

    #[test]
    fn delay_doubles_then_caps() {
        let p = policy();
        // jitter_unit = 1.0 → the full base delay.
        assert_eq!(p.delay_for_attempt(0, 1.0), Duration::from_millis(250));
        assert_eq!(p.delay_for_attempt(1, 1.0), Duration::from_millis(500));
        assert_eq!(p.delay_for_attempt(2, 1.0), Duration::from_secs(1));
        assert_eq!(p.delay_for_attempt(4, 1.0), Duration::from_secs(4));
        assert_eq!(p.delay_for_attempt(5, 1.0), Duration::from_secs(5));
        assert_eq!(
            p.delay_for_attempt(60, 1.0),
            Duration::from_secs(5),
            "no overflow far out"
        );
    }

    #[test]
    fn jitter_stays_within_half_to_full_base() {
        let p = policy();
        assert_eq!(p.delay_for_attempt(1, 0.0), Duration::from_millis(250));
        assert_eq!(p.delay_for_attempt(1, 1.0), Duration::from_millis(500));
        let mid = p.delay_for_attempt(1, 0.5);
        assert!(mid > Duration::from_millis(250) && mid < Duration::from_millis(500));
        // Out-of-range jitter is clamped, not extrapolated.
        assert_eq!(p.delay_for_attempt(1, 7.0), Duration::from_millis(500));
        assert_eq!(p.delay_for_attempt(1, -3.0), Duration::from_millis(250));
    }

    #[test]
    fn default_budget_is_ten_minutes_and_zero_disables() {
        assert_eq!(RetryPolicy::default().max_wait, Duration::from_secs(600));
        assert!(RetryPolicy::default().is_enabled());
        assert!(!RetryPolicy::disabled().is_enabled());
    }

    #[test]
    fn only_not_there_yet_errors_are_retryable() {
        assert!(is_unreachable_kind(io::ErrorKind::NotFound));
        assert!(is_unreachable_kind(io::ErrorKind::ConnectionRefused));
        assert!(is_unreachable_kind(io::ErrorKind::TimedOut));
        assert!(!is_unreachable_kind(io::ErrorKind::PermissionDenied));
        assert!(!is_unreachable_kind(io::ErrorKind::InvalidInput));
    }

    #[test]
    fn unreachable_message_names_socket_and_time_waited() {
        let msg = EngineUnreachable {
            socket_path: "/tmp/x.sock".into(),
            waited: Duration::from_millis(12_300),
            attempts: 9,
            retry_enabled: true,
            last_error: "No such file or directory".into(),
        }
        .to_string();
        assert!(msg.contains("/tmp/x.sock"), "{msg}");
        assert!(msg.contains("12.3s"), "{msg}");
        assert!(msg.contains("9 attempts"), "{msg}");
    }
}
