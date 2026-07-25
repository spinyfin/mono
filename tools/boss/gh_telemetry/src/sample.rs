//! The record one GitHub API call produces.

use serde::{Deserialize, Serialize};

/// Which GitHub API a call hit. The distinction is load-bearing, not
/// cosmetic: GraphQL and REST are metered against **separate** hourly
/// buckets, so a single blended "GitHub calls" number cannot tell you
/// which budget drained. The exhausted bucket in the incident this
/// instrumentation was built for was GraphQL specifically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GhApi {
    /// `gh api graphql …`, or an HTTP POST to `/graphql`. Metered in
    /// *points* (~node count / 100), 5000 points/hour.
    Graphql,
    /// `gh api <path>`, or an HTTP request to a REST endpoint. Metered
    /// in *requests*, 5000 requests/hour on a separate bucket.
    Rest,
    /// A `gh` porcelain subcommand (`gh pr view`, `gh pr merge`, …).
    /// The CLI decides internally whether to serve it over GraphQL or
    /// REST, so the bucket it charges is not knowable from the argv —
    /// but a `rateLimit` block in the response resolves it after the
    /// fact (see [`GhCallSample::api`]).
    Cli,
}

impl GhApi {
    /// Lowercase wire/database representation.
    pub fn as_str(self) -> &'static str {
        match self {
            GhApi::Graphql => "graphql",
            GhApi::Rest => "rest",
            GhApi::Cli => "cli",
        }
    }
}

/// How a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GhOutcome {
    /// The call completed and GitHub answered successfully.
    Ok,
    /// GitHub rejected the call for exceeding the rate limit. Split out
    /// from [`GhOutcome::Error`] because a rate-limited call is evidence
    /// *about the budget*, whereas a transport failure is noise against
    /// it — conflating them is what made the original exhaustion look
    /// like a network problem in the trace.
    RateLimited,
    /// Anything else: a spawn failure, a non-zero exit, a transport
    /// error, an unparseable response.
    Error,
}

impl GhOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            GhOutcome::Ok => "ok",
            GhOutcome::RateLimited => "rate_limited",
            GhOutcome::Error => "error",
        }
    }
}

/// The `rateLimit` block GitHub folds into a GraphQL response when the
/// query asks for it.
///
/// Querying `rateLimit` is itself free — GitHub charges it 0 points and
/// does not count it as a call — so capturing this on every call costs
/// nothing. It is the difference between "the budget ran out and we
/// don't know why" and an attributable time series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimit {
    /// Points this specific query cost. The per-call attribution number.
    pub cost: i64,
    /// Points left in the current hourly window after this call.
    pub remaining: i64,
    /// Total points in the window (5000 for a personal token).
    pub limit: i64,
    /// Epoch **milliseconds** at which the window resets. `None` when
    /// GitHub sent an unparseable or absent `resetAt`.
    pub reset_at_ms: Option<i64>,
}

/// One GitHub API call, as observed at the point of egress.
///
/// Builder-equipped per the repo convention for structs with more than
/// five fields, so adding a future dimension (the token identity, say)
/// doesn't have to touch every construction site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct GhCallSample {
    /// The subsystem that made the call — the whole point of this
    /// record. See [`crate::caller`]; `unattributed` when no scope was
    /// active.
    pub caller: String,
    /// Which API bucket this charged. For a [`GhApi::Cli`] call that
    /// came back carrying a `rateLimit` block, this is upgraded to
    /// [`GhApi::Graphql`] — the response proves which bucket paid.
    pub api: GhApi,
    /// The operation: a `gh` subcommand pair (`pr view`, `api graphql`)
    /// or an HTTP method (`GET`, `POST`).
    pub verb: String,
    /// The endpoint: a REST path, `graphql`, or the porcelain's target.
    /// Normalised to keep cardinality bounded — see
    /// [`crate::classify::normalize_endpoint`].
    pub endpoint: String,
    /// Epoch **milliseconds** when the call started.
    pub started_at_ms: i64,
    /// Wall-clock duration of the call.
    pub duration_ms: i64,
    pub outcome: GhOutcome,
    /// The free quota reading, when the response carried one.
    pub rate_limit: Option<RateLimit>,
}

impl GhCallSample {
    /// Points this call cost, or 0 when no `rateLimit` block came back.
    ///
    /// A GraphQL call with no reading is genuinely unknown, not free —
    /// it is counted as a call but contributes nothing to the points
    /// total. Widening every engine GraphQL query to request
    /// [`crate::classify::RATE_LIMIT_SELECTION`] is what keeps that
    /// undercount near zero.
    pub fn points(&self) -> u64 {
        self.rate_limit.map(|rl| rl.cost.max(0) as u64).unwrap_or(0)
    }
}
