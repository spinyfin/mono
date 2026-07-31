//! Per-operation wall-clock budgets for a single `cube` invocation.
//!
//! Most cube subcommands are cheap reads that should fail fast. Two are
//! not: `cube workspace lease` and `cube repo ensure` do real provisioning
//! work (health scan, `jj workspace add`, setup steps, and for `ensure` a
//! first-time `jj git clone`), and their cost is dominated by what happens
//! on the machine cube runs on, not by the call itself.
//!
//! Those two therefore get named, deliberately-sized budgets, and this
//! module is the one place they are written down. Before it, the numbers
//! lived only as private constants in `coordinator.rs` and were applied
//! *only* as an outer `tokio::time::timeout` around the host adapter. That
//! worked for the local adapter, which imposes no bound of its own — but
//! the SSH adapter runs every cube call through
//! [`boss_engine_ssh_transport::SSH_COMMAND_TIMEOUT`] (30s), so on a remote
//! host the inner generic bound always fired first and the outer budget was
//! unreachable. That is the anaplian first-lease incident: the engine's own
//! policy said a lease may take 90s, and the transport gave it 30.
//!
//! The fix is not a bigger number. [`LEASE`] and [`REPO_ENSURE`] are
//! unchanged from the values `coordinator.rs` already carried; what changed
//! is that the remote transport now honours them instead of quietly
//! substituting its own. A lease on a remote host is still bounded, and
//! still fails loudly when it blows the bound.

use std::time::Duration;

/// Upper bound on a single `cube workspace lease` invocation.
///
/// The motivating incident (`exec_18aec07893bd2e30_29`, 2026-05-12) sat in
/// `worker_claimed/ok` for ~46 seconds with no event because the cube
/// subprocess never returned and the engine was awaiting it unboundedly.
/// With this bound the engine surfaces a `cube_workspace_lease_failed`
/// event and either falls back or fails cleanly.
///
/// Raised 30s → 90s as an explicit stopgap, NOT as a fix. The real bound
/// belongs to cube and now lives there: `cube workspace lease` caps its
/// pre-claim health scan by probe count and by wall clock, so lease latency
/// no longer grows with the size of the free pool. This engine-side number
/// is only the outer backstop for the remaining tail — the `jj workspace
/// add` + setup-step provisioning path, ~6s nominal but network- and
/// host-load-dependent. A lease that takes longer than 90s is a cube bug to
/// be fixed in cube; do not keep raising this number in its place.
pub const LEASE: Duration = Duration::from_secs(90);

/// Upper bound on a single `cube repo ensure` invocation. Normally fast
/// (an idempotent record lookup), but the first ensure for a repo on a
/// given machine performs the initial `jj git clone`, and the same
/// wedged-subprocess hang class as [`LEASE`] applies, so it is bounded too.
pub const REPO_ENSURE: Duration = Duration::from_secs(60);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh_transport::SSH_COMMAND_TIMEOUT;

    /// The whole point of the module: the transport bound must sit strictly
    /// inside the outer bound, so the transport's better-attributed timeout
    /// error is the one the operator sees.
    #[test]
    fn transport_budgets_are_strictly_inside_their_outer_bounds() {
        assert!(lease_transport_budget() < LEASE);
        assert!(repo_ensure_transport_budget() < REPO_ENSURE);
    }

    /// The regression this module exists to prevent. If a provisioning-class
    /// budget ever collapses to (or below) the generic fast-command bound,
    /// the remote path is back to the anaplian behaviour: the operation is
    /// killed at 30s no matter what the dispatcher budgeted for it.
    #[test]
    fn provisioning_budgets_exceed_the_generic_fast_command_bound() {
        assert!(
            lease_transport_budget() > SSH_COMMAND_TIMEOUT,
            "a remote lease must get more than the generic {}s ssh bound",
            SSH_COMMAND_TIMEOUT.as_secs()
        );
        assert!(
            repo_ensure_transport_budget() > SSH_COMMAND_TIMEOUT,
            "a remote repo ensure must get more than the generic {}s ssh bound",
            SSH_COMMAND_TIMEOUT.as_secs()
        );
    }

    /// `saturating_sub` must not silently produce a zero budget if someone
    /// later sets an outer bound below the grace margin — a zero budget
    /// would time out every call instantly.
    #[test]
    fn transport_budget_never_reaches_zero_for_the_defined_operations() {
        assert!(!lease_transport_budget().is_zero());
        assert!(!repo_ensure_transport_budget().is_zero());
    }
}
