//! Per-call telemetry for every GitHub API call Boss makes.
//!
//! # What this exists to answer
//!
//! "The GraphQL budget hit 0/5000 — who spent it?" Before this crate
//! there was no counter, no structured log line, and no metric
//! recording a `gh` invocation or an API call with enough detail to
//! attribute the drain. The only signal was a threshold-crossing log
//! line that fires when the remaining budget crosses 1500 in either
//! direction, which means a rate could only be *inferred* from the time
//! between two crossings — never attributed to a caller.
//!
//! # The three things a sample must carry
//!
//! 1. **Which subsystem** made the call ([`caller`]). A single global
//!    count reproduces the problem it was meant to solve.
//! 2. **Which bucket** it charged ([`sample::GhApi`]). GraphQL and REST
//!    are metered separately; the exhausted one was GraphQL.
//! 3. **What it cost** ([`sample::RateLimit`]). Every GraphQL response
//!    can carry a `rateLimit` block for free, and Boss was discarding
//!    it. Capturing it turns "we ran out" into a time series at zero
//!    additional API cost.
//!
//! # Shape
//!
//! ```ignore
//! let timer = GhCallTimer::start_args(&args);   // classify + stamp
//! let result = spawn_gh(&args).await;           // the actual call
//! timer.finish_process(&result);                // classify outcome, emit
//! ```
//!
//! [`sink::record`] is a no-op until the engine installs a
//! [`sink::GhCallSink`], so `boss_github`'s other consumers (the `boss`
//! CLI, `bossctl`, tests) pay nothing but the classification.

pub mod caller;
pub mod classify;
pub mod sample;
pub mod sink;

pub use caller::{
    UNATTRIBUTED, callers, current as current_caller, scope, scope_blocking, scope_blocking_if_unattributed,
};
pub use classify::{
    GhCallShape, RATE_LIMIT_SELECTION, classify_args, is_rate_limited, normalize_endpoint, parse_rate_limit,
    parse_rate_limit_headers,
};
pub use sample::{GhApi, GhCallSample, GhOutcome, RateLimit};
pub use sink::{GhCallSink, GhCallTimer, clear_sink, install_sink, now_ms, record};
