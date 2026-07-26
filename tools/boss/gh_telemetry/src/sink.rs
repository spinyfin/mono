//! The egress seam: where a completed GitHub call is handed off.
//!
//! This crate deliberately knows nothing about counters or databases.
//! It defines the sample and the seam; the engine installs a
//! [`GhCallSink`] that fans a sample out to the metrics registry and to
//! `state.db`. That keeps the dependency edge one-directional —
//! `boss_engine` → `boss_gh_telemetry`, and `boss_github` →
//! `boss_gh_telemetry`, never the reverse — so instrumenting the
//! low-level `gh` runner does not drag the engine into its dependency
//! graph.
//!
//! With no sink installed (every non-engine consumer of `boss_github`:
//! the `boss` CLI, `bossctl`, tests) [`record`] is a cheap no-op.

use std::process::Output;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::caller;
use crate::classify::{self, GhCallShape};
use crate::sample::{GhApi, GhCallSample, GhOutcome};

/// Receives every observed GitHub API call.
///
/// Implementations must be cheap and must not block: [`record`] is
/// called inline on the egress path, once per API call. The engine's
/// implementation bumps in-memory atomics and pushes onto an unbounded
/// channel; the sqlite write happens on a separate task.
pub trait GhCallSink: Send + Sync {
    fn record(&self, sample: GhCallSample);
}

static SINK: RwLock<Option<Arc<dyn GhCallSink>>> = RwLock::new(None);

/// Install the process-wide sink. The engine calls this once at
/// startup, before any subsystem loop is spawned.
///
/// Replacing an existing sink is allowed (tests rely on it) rather than
/// panicking — a double-install in production would be a wiring bug,
/// but refusing to start the engine over telemetry plumbing is the
/// wrong trade.
pub fn install_sink(sink: Arc<dyn GhCallSink>) {
    *SINK.write().expect("gh telemetry sink lock poisoned") = Some(sink);
}

/// Remove the installed sink. Exists for tests that must not leak a
/// capture into the next test in the same process.
pub fn clear_sink() {
    *SINK.write().expect("gh telemetry sink lock poisoned") = None;
}

/// Hand one observed call to the installed sink. A no-op when none is
/// installed.
pub fn record(sample: GhCallSample) {
    let sink = SINK.read().expect("gh telemetry sink lock poisoned").clone();
    if let Some(sink) = sink {
        sink.record(sample);
    }
}

/// Epoch milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// An in-flight GitHub call: classified at start, recorded at finish.
///
/// Constructing one is a few string allocations and a clock read — the
/// cost is irrelevant next to the subprocess spawn or HTTP round trip
/// it wraps, which is what makes it affordable to put on *every* egress
/// path rather than only the ones someone remembered to instrument.
pub struct GhCallTimer {
    caller: &'static str,
    shape: GhCallShape,
    started_at_ms: i64,
    started: Instant,
}

impl GhCallTimer {
    /// Start timing a `gh` invocation, classifying `args` for the
    /// verb / endpoint / API bucket.
    pub fn start_args(args: &[&str]) -> Self {
        Self::start(classify::classify_args(args))
    }

    /// Start timing a call whose shape the caller already knows (the
    /// direct-HTTP paths, which have a method and URL rather than an
    /// argv).
    pub fn start(shape: GhCallShape) -> Self {
        Self {
            caller: caller::current(),
            shape,
            started_at_ms: now_ms(),
            started: Instant::now(),
        }
    }

    /// Record the outcome of a `gh` subprocess.
    ///
    /// This is the shared finish for every subprocess path, so the
    /// rules for reading an outcome out of `gh` live in one place:
    ///
    /// - A spawn failure is an `Error` with no quota reading.
    /// - A non-zero exit whose stderr mentions a rate limit is
    ///   `RateLimited`, not a generic error — the distinction is the
    ///   whole point of separating budget evidence from transport noise.
    /// - A successful exit still gets its stdout parsed, because
    ///   `gh api graphql` returns HTTP 200 with a top-level `errors`
    ///   array for a `RATE_LIMITED` rejection.
    /// - A response carrying a `rateLimit` block proves the call was
    ///   billed to the GraphQL bucket, so a `Cli` classification is
    ///   upgraded to `Graphql`.
    pub fn finish_process(self, result: &std::io::Result<Output>) {
        match result {
            Err(_) => self.finish(GhOutcome::Error, None),
            Ok(output) if !output.status.success() => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let outcome = if classify::is_rate_limited(&stderr) {
                    GhOutcome::RateLimited
                } else {
                    GhOutcome::Error
                };
                self.finish(outcome, None);
            }
            Ok(output) => {
                let body: Option<serde_json::Value> = serde_json::from_slice(&output.stdout).ok();
                let outcome = match body.as_ref().and_then(|b| b.get("errors")) {
                    Some(errors) if classify::is_rate_limited(&errors.to_string()) => GhOutcome::RateLimited,
                    Some(errors) if errors.as_array().is_some_and(|e| !e.is_empty()) => GhOutcome::Error,
                    _ => GhOutcome::Ok,
                };
                self.finish(outcome, body.as_ref());
            }
        }
    }

    /// Record an outcome directly, optionally with the parsed response
    /// body to mine for a `rateLimit` block.
    pub fn finish(self, outcome: GhOutcome, body: Option<&serde_json::Value>) {
        let rate_limit = body.and_then(classify::parse_rate_limit);
        self.finish_with(outcome, rate_limit);
    }

    /// Record an outcome with a quota reading the caller already
    /// resolved — used by the direct-HTTP paths, which read theirs from
    /// the `x-ratelimit-*` response headers rather than from a body
    /// (see [`classify::parse_rate_limit_headers`]).
    pub fn finish_with(self, outcome: GhOutcome, rate_limit: Option<crate::sample::RateLimit>) {
        // Only a `gh` porcelain call has an unresolved bucket, and the
        // only way it comes back carrying a reading is if the CLI served
        // it over GraphQL — so that settles which budget it billed. The
        // direct-HTTP paths already know their bucket from the URL and
        // are left alone.
        let api = match (self.shape.api, rate_limit) {
            (GhApi::Cli, Some(_)) => GhApi::Graphql,
            (api, _) => api,
        };
        record(GhCallSample {
            caller: self.caller.to_owned(),
            api,
            verb: self.shape.verb,
            endpoint: self.shape.endpoint,
            started_at_ms: self.started_at_ms,
            duration_ms: self.started.elapsed().as_millis() as i64,
            outcome,
            rate_limit,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct CapturingSink {
        samples: Mutex<Vec<GhCallSample>>,
    }

    impl GhCallSink for CapturingSink {
        fn record(&self, sample: GhCallSample) {
            self.samples.lock().unwrap().push(sample);
        }
    }

    /// The installed sink is process-wide while `cargo test` runs cases
    /// in parallel threads, so every test that touches it — including
    /// the one that only clears it — must hold this lock. Without that,
    /// one test's `clear_sink` lands between another's install and its
    /// assertion.
    static SINK_SERIAL: Mutex<()> = Mutex::new(());

    fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
        SINK_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Install a capturing sink, run `body`, return what it recorded.
    fn capture(body: impl FnOnce()) -> Vec<GhCallSample> {
        let _guard = serial_guard();
        let sink = Arc::new(CapturingSink::default());
        install_sink(sink.clone());
        body();
        clear_sink();
        sink.samples.lock().unwrap().clone()
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> std::io::Result<Output> {
        Ok(Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        })
    }

    #[test]
    fn records_a_successful_graphql_call_with_its_cost() {
        let samples = capture(|| {
            let timer = GhCallTimer::start_args(&["api", "graphql", "-f", "query={ viewer { login } }"]);
            timer.finish_process(&output(
                0,
                r#"{"data":{"rateLimit":{"cost":2,"limit":5000,"remaining":4998,"used":2,"resetAt":"2026-07-24T17:00:00Z"}}}"#,
                "",
            ));
        });
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].api, GhApi::Graphql);
        assert_eq!(samples[0].outcome, GhOutcome::Ok);
        assert_eq!(samples[0].points(), 2);
        assert_eq!(samples[0].caller, caller::UNATTRIBUTED);
    }

    #[test]
    fn rate_limited_exit_is_distinguished_from_a_generic_error() {
        let samples = capture(|| {
            GhCallTimer::start_args(&["api", "graphql", "-f", "query={}"]).finish_process(&output(
                1,
                "",
                "gh: API rate limit exceeded",
            ));
            GhCallTimer::start_args(&["api", "graphql", "-f", "query={}"]).finish_process(&output(
                1,
                "",
                "could not connect to host",
            ));
        });
        assert_eq!(samples[0].outcome, GhOutcome::RateLimited);
        assert_eq!(samples[1].outcome, GhOutcome::Error);
    }

    #[test]
    fn graphql_rate_limited_error_array_on_http_200_is_caught() {
        // GitHub answers a RATE_LIMITED GraphQL rejection with HTTP 200
        // and an `errors` array. Reading only the exit status would file
        // this as a success and lose the budget evidence.
        let samples = capture(|| {
            GhCallTimer::start_args(&["api", "graphql", "-f", "query={}"]).finish_process(&output(
                0,
                r#"{"errors":[{"type":"RATE_LIMITED","message":"API rate limit exceeded"}]}"#,
                "",
            ));
        });
        assert_eq!(samples[0].outcome, GhOutcome::RateLimited);
    }

    #[test]
    fn spawn_failure_is_recorded_not_dropped() {
        let samples = capture(|| {
            let err = Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no gh"));
            GhCallTimer::start_args(&["pr", "view", "https://github.com/o/r/pull/1"]).finish_process(&err);
        });
        assert_eq!(samples.len(), 1, "a call that never left the box still counts");
        assert_eq!(samples[0].outcome, GhOutcome::Error);
        assert_eq!(samples[0].rate_limit, None);
    }

    #[test]
    fn porcelain_call_is_upgraded_to_graphql_when_the_response_proves_it() {
        let samples = capture(|| {
            GhCallTimer::start_args(&["pr", "view", "https://github.com/o/r/pull/1", "--json", "state"])
                .finish_process(&output(
                    0,
                    r#"{"data":{"rateLimit":{"cost":1,"limit":5000,"remaining":4999,"used":1}}}"#,
                    "",
                ));
        });
        assert_eq!(
            samples[0].api,
            GhApi::Graphql,
            "a rateLimit block only comes from the GraphQL endpoint",
        );
    }

    #[test]
    fn scoped_calls_carry_their_subsystem() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let samples = capture(|| {
            rt.block_on(caller::scope(caller::callers::MERGE_POLLER_ADAPTIVE, async {
                GhCallTimer::start_args(&["api", "graphql", "-f", "query={}"]).finish_process(&output(0, "{}", ""));
            }));
        });
        assert_eq!(samples[0].caller, "merge_poller.adaptive");
    }

    #[test]
    fn no_sink_installed_is_a_no_op() {
        let _guard = serial_guard();
        clear_sink();
        // Must not panic — every non-engine consumer of `boss_github`
        // runs with no sink installed.
        GhCallTimer::start_args(&["pr", "view", "x"]).finish(GhOutcome::Ok, None);
    }

    #[test]
    fn non_json_stdout_still_records_a_successful_call() {
        // `gh pr view --jq .state` prints a bare string, not JSON. It
        // must still be counted; it just carries no quota reading.
        let samples = capture(|| {
            GhCallTimer::start_args(&["pr", "view", "https://github.com/o/r/pull/1", "--jq", ".state"])
                .finish_process(&output(0, "OPEN", ""));
        });
        assert_eq!(samples[0].outcome, GhOutcome::Ok);
        assert_eq!(samples[0].points(), 0);
    }
}
