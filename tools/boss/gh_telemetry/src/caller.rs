//! Ambient attribution: which subsystem is making the GitHub call.
//!
//! # Why a task-local and not a parameter
//!
//! Threading a `caller: &str` through every `gh` helper would look
//! tidier, and for the top-level call sites it would work. It fails on
//! the calls that matter most.
//!
//! The dominant GraphQL consumer is the un-batched adaptive per-PR path
//! (`reconcile_one` → `probe_batch(&[one])`), and its spend does not
//! happen at one call site: `CommandMergeProbe::probe` fans out into
//! `pr_merge_queue_entry` and
//! `fetch_commit_combined_state_for_empty_rollup`, each of which spawns
//! its own `gh`. Those helpers are shared with the batched full sweep.
//! A per-call-site label would either attribute both paths to the same
//! bucket — reproducing exactly the ambiguity this instrumentation
//! exists to remove — or require plumbing a caller parameter through
//! every intermediate signature.
//!
//! A task-local scope set once at the subsystem's entry point covers
//! the whole call tree beneath it, including helpers nobody remembered
//! to label.
//!
//! # The propagation caveat
//!
//! `tokio::task_local!` values do **not** cross a `tokio::spawn`
//! boundary. A subsystem that spawns sub-tasks must set the scope
//! inside each spawned task, or its calls fall back to
//! [`UNATTRIBUTED`]. That fallback is deliberately visible: the
//! `github_api.unattributed.calls` counter is the self-check that says
//! the attribution has a hole in it, so a miss shows up as a number
//! rather than as silently mis-attributed spend.

use std::future::Future;

/// The label used when no [`scope`] is active. Deliberately a real,
/// countable bucket rather than "unknown" — see the module note.
pub const UNATTRIBUTED: &str = "unattributed";

tokio::task_local! {
    static GH_CALLER: &'static str;
}

/// Run `fut` with `caller` as the ambient attribution for every GitHub
/// call it makes, including calls made by helpers it delegates to.
///
/// Scopes nest: an inner scope wins for the duration of its future, and
/// the outer label is restored afterwards. That is what lets a broad
/// per-loop scope coexist with a narrow one around the specific path
/// being measured.
pub async fn scope<F: Future>(caller: &'static str, fut: F) -> F::Output {
    GH_CALLER.scope(caller, fut).await
}

/// Synchronous counterpart to [`scope`], for the call sites that spawn
/// `gh` from a blocking trait method rather than a future.
///
/// Those sites otherwise inherit whatever scope the surrounding task
/// happens to be in — which for a synchronous checker reached from a
/// dozen different request handlers is usually none at all. Naming
/// themselves keeps them out of the `unattributed` bucket.
pub fn scope_blocking<R>(caller: &'static str, f: impl FnOnce() -> R) -> R {
    GH_CALLER.sync_scope(caller, f)
}

/// The subsystem currently attributed, or [`UNATTRIBUTED`] outside any
/// scope.
pub fn current() -> &'static str {
    GH_CALLER.try_with(|c| *c).unwrap_or(UNATTRIBUTED)
}

/// Canonical subsystem labels.
///
/// Kept as one closed list so the attribution axis is a fixed
/// vocabulary rather than whatever string each call site invented, and
/// so a reader can see every consumer of the shared token in one place.
/// Labels use `a-z0-9._` only — they are embedded verbatim in metric
/// names, which the registry validates against that charset.
pub mod callers {
    /// The 60s batched full sweep — one aliased GraphQL query covering
    /// every tracked PR.
    pub const MERGE_POLLER_SWEEP: &str = "merge_poller.sweep";
    /// The adaptive per-PR timer: `reconcile_one` on a single due PR,
    /// which probes that PR on its own (2 points per PR per reconcile).
    /// Split from the sweep on purpose — the two paths have very
    /// different per-point efficiency and blaming the wrong one would
    /// misdirect the remediation.
    pub const MERGE_POLLER_ADAPTIVE: &str = "merge_poller.adaptive";
    /// A keyed `PrReconcileRequested` event reconciling one named PR.
    pub const MERGE_POLLER_REQUESTED: &str = "merge_poller.requested";
    /// The free pre-sweep `rateLimit` read.
    pub const MERGE_POLLER_BUDGET_REFRESH: &str = "merge_poller.budget_refresh";
    /// Merge-queue dequeue-event polling.
    pub const MERGE_POLLER_MERGE_QUEUE: &str = "merge_poller.merge_queue";
    /// The Trunk merge-queue observer riding the poller's loop.
    pub const TRUNK_QUEUE_POLLER: &str = "trunk_queue_poller";
    /// CI status/check polling.
    pub const CI_WATCH: &str = "ci_watch";
    /// Merge-conflict detection.
    pub const CONFLICT_WATCH: &str = "conflict_watch";
    /// The on-Stop worker-completion path (PR detection/binding).
    pub const COMPLETION: &str = "completion";
    /// `gh pr merge --auto` and its state confirmation.
    pub const MERGE_WHEN_READY: &str = "merge_when_ready";
    /// Post-merge parent-branch deletion checks.
    pub const MERGE_PARENT_DELETION: &str = "merge_parent_deletion";
    /// Worker spawn-time PR diff fetches.
    pub const WORKER_SPAWN: &str = "worker_spawn";
    /// Design-doc detection against a PR.
    pub const DESIGN_DETECTOR: &str = "design_detector";
    /// Speculative conflict prediction.
    pub const SPECULATIVE_CONFLICT: &str = "speculative_conflict";
    /// Stacked-PR auto-structuring.
    pub const STACKED_PR_STRUCTURING: &str = "stacked_pr_structuring";
    /// Abandoned-branch / stale-PR sweep.
    pub const ABANDONED_BRANCH_SWEEP: &str = "abandoned_branch_sweep";
    /// External issue-tracker reconciliation (GitHub Issues/Projects).
    pub const EXTERNAL_TRACKER: &str = "external_tracker";
    /// Front-end request handlers that reach GitHub inline.
    pub const REQUEST_HANDLER: &str = "request_handler";
    /// Host-capability probing (`gh auth status`).
    pub const HOST_REGISTRY: &str = "host_registry";
    /// PR open/merged state checks issued outside the poller.
    pub const PR_STATE_CHECK: &str = "pr_state_check";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn outside_a_scope_calls_are_unattributed() {
        assert_eq!(current(), UNATTRIBUTED);
    }

    #[tokio::test]
    async fn scope_applies_to_the_whole_call_tree() {
        async fn nested_helper() -> &'static str {
            current()
        }
        let observed = scope("merge_poller.sweep", async { nested_helper().await }).await;
        assert_eq!(observed, "merge_poller.sweep");
    }

    #[tokio::test]
    async fn scopes_nest_and_restore() {
        let (inner, after) = scope("outer", async {
            let inner = scope("inner", async { current() }).await;
            (inner, current())
        })
        .await;
        assert_eq!(inner, "inner");
        assert_eq!(after, "outer", "the outer label must be restored");
    }

    #[tokio::test]
    async fn scope_does_not_leak_past_the_future() {
        scope("merge_poller.sweep", async {}).await;
        assert_eq!(current(), UNATTRIBUTED);
    }

    #[test]
    fn blocking_scope_attributes_synchronous_call_sites() {
        // Deliberately not a `#[tokio::test]`: the synchronous checkers
        // this exists for can run off any thread, with or without a
        // runtime.
        assert_eq!(scope_blocking("pr_state_check", current), "pr_state_check");
        assert_eq!(current(), UNATTRIBUTED, "must not leak past the closure");
    }

    #[tokio::test]
    async fn spawned_tasks_fall_back_to_unattributed() {
        // Documents the propagation caveat rather than pretending it
        // away: a subsystem that spawns must re-enter the scope inside
        // the spawned task, and the `unattributed` counter is what
        // surfaces the miss.
        let observed = scope("merge_poller.sweep", async {
            tokio::spawn(async { current() }).await.unwrap()
        })
        .await;
        assert_eq!(observed, UNATTRIBUTED);
    }
}
