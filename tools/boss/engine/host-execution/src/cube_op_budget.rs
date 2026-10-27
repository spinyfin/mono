//! Shared wall-clock budgets for cube provisioning operations.
//! Transports honor these budgets so network and setup work can exceed the
//! generic read timeout while remaining bounded.

use std::time::Duration;

/// Backstop for workspace creation and setup after cube's bounded health scan.
pub const LEASE: Duration = Duration::from_secs(90);

/// Upper bound on a single `cube repo ensure` invocation. Normally fast
/// (an idempotent record lookup), but the first ensure for a repo on a
/// given machine performs the initial `jj git clone`, and the same
/// wedged-subprocess hang class as [`LEASE`] applies, so it is bounded too.
pub const REPO_ENSURE: Duration = Duration::from_secs(60);

/// Positioning may fetch a large repository; stay within the executor's 300s bound.
pub const GOTO: Duration = Duration::from_secs(290);

/// Margin between a caller's outer budget and the budget handed to the
/// transport underneath it.
///
/// Both bounds are real and both must stay: the outer one
/// (`tokio::time::timeout` around the adapter call) is the backstop that
/// fires even if a transport forgets to bound itself, and the inner one is
/// the transport's own. Handing the transport the *same* number would make
/// which of the two fires a coin flip, and they do not produce equally
/// useful errors — the transport's names the host and the exact argv, the
/// backstop's does not. Giving the transport slightly less guarantees the
/// better-attributed error wins, without changing the end-to-end bound the
/// caller advertises.
pub const TRANSPORT_GRACE: Duration = Duration::from_secs(10);

/// The budget to hand a transport for an operation whose outer bound is
/// `outer`. Always strictly less than `outer` (see [`TRANSPORT_GRACE`]).
pub const fn transport_budget(outer: Duration) -> Duration {
    outer.saturating_sub(TRANSPORT_GRACE)
}

/// Transport-level budget for `cube workspace lease`.
pub const fn lease_transport_budget() -> Duration {
    transport_budget(LEASE)
}

/// Transport-level budget for `cube repo ensure`.
pub const fn repo_ensure_transport_budget() -> Duration {
    transport_budget(REPO_ENSURE)
}
