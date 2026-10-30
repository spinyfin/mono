use boss_engine_effort::PoolModelTier;
use boss_protocol::ExecutionKind;

/// Hard cap on the review worker pool. The runtime config can request a
/// smaller pool via `BOSS_REVIEW_POOL_SIZE`, but values above this are
/// clamped. The third pool, modeled on the automation pool, that runs the
/// always-Opus `pr_review` reviewer agents. See design:
/// automated-reviewer-pass-on-every-agent-authored-pr.md
///
/// Raised from 8 to 16 (design: multi-agent-code-review.md, "Expand the
/// static review pool to 16 slots") so a four-unit pre-merge batch
/// reservation (`crate::work::can_admit_review_batch_in_tx`) can admit up
/// to four concurrent pre-merge PRs without leaf reviewers starving their
/// own supervisors. Global review-pool slot ids now span 25–40 (was
/// 25–32) — see `slot_id_from_worker_id` and `worker_id_for_slot`.
pub const MAX_REVIEW_POOL_SIZE: usize = 16;

/// Default review-pool slot count when `BOSS_REVIEW_POOL_SIZE` is unset.
/// Raised to 16 to match [`MAX_REVIEW_POOL_SIZE`] so the pool's weighted
/// admission accounting (four units per pre-merge batch, one per post-merge
/// batch) has its full static capacity available by default rather than
/// requiring an operator override.
pub const DEFAULT_REVIEW_POOL_SIZE: usize = 16;

/// Worker ID prefix for automation-pool slots. Distinct from the main-pool
/// `"worker-"` prefix so `pool_for_worker_id` can route releases to the
/// correct pool without an extra DB round-trip.
pub const AUTOMATION_WORKER_ID_PREFIX: &str = "auto-worker-";

/// Worker ID prefix for review-pool slots. Distinct from both the main-pool
/// `"worker-"` and automation-pool `"auto-worker-"` prefixes so
/// `pool_for_worker_id` can route releases to the review pool.
pub const REVIEW_WORKER_ID_PREFIX: &str = "review-";

/// Driver slug the review/automation pools dispatch on, independent of
/// whatever `tasks.driver` the reviewed/automated row itself carries. Named
/// so the reason is legible at every call site: reviews stay on Opus, and
/// Opus is a Claude model, so the reviewer driver is Claude — never inherited
/// from (and never varying with) the row under review.
pub const REVIEWER_POOL_DRIVER: &str = "claude";

/// Driver + model-tier dispatch policy for a pool-level worker
/// (review or automation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolDispatchPolicy {
    /// Driver slug the pool worker dispatches on. Overrides whatever
    /// `tasks.driver` the reviewed/automated row itself carries — see
    /// [`pool_dispatch_policy_for_worker_id`]'s doc comment for why.
    pub driver: &'static str,
    /// Model-tier override within that driver's menu
    /// (`resolve_spawn_config` precedence step 2).
    pub model_tier: PoolModelTier,
}

/// Returns the pool-level dispatch policy for the given `worker_id`, or
/// `None` for the main pool (which has no override: it dispatches on the
/// row's own `driver` / effort-driven model, falling through to the
/// product/engine default exactly as before).
///
/// **Single resolution point** for review/automation dispatch policy — no
/// other call site should special-case a driver for pool dispatch. Today it
/// returns a constant: both the automation pool (`auto-worker-N`) and the
/// review pool (`review-N`) always dispatch on [`REVIEWER_POOL_DRIVER`] at
/// [`PoolModelTier::Strong`] (Opus), per the automated-reviewer design §5.
///
/// This function takes only `worker_id` — deliberately, not the reviewed
/// row's `driver` column — so the reviewer's dispatch policy cannot vary
/// with who authored the work under review. Who authored a change must not
/// determine who reviews it: a codex-authored chore gets a Claude reviewer
/// on Opus exactly like a Claude-authored one, never a codex reviewer that
/// inherited the row's own driver (the original bug: a hard-coded `"opus"`
/// model string reaching whatever driver the reviewed row happened to carry,
/// which 400s instantly against a non-Claude CLI).
///
/// The model half is a driver-relative [`PoolModelTier`] rather than a
/// literal model slug, resolved against [`REVIEWER_POOL_DRIVER`]'s menu by
/// [`boss_engine_effort::resolve_spawn_config`] — never applied as a raw string
/// before a driver is known.
///
/// Follow-on work (configurable reviewer model, load balancing across
/// reviewer models, two-party review with two distinct reviewers on one PR)
/// only needs to change what this function returns.
pub fn pool_dispatch_policy_for_worker_id(worker_id: &str) -> Option<PoolDispatchPolicy> {
    if worker_id.starts_with(REVIEW_WORKER_ID_PREFIX) || worker_id.starts_with(AUTOMATION_WORKER_ID_PREFIX) {
        Some(PoolDispatchPolicy {
            driver: REVIEWER_POOL_DRIVER,
            model_tier: PoolModelTier::Strong,
        })
    } else {
        None
    }
}

/// `true` when EVERY execution of `kind` runs on a pool whose driver is
/// pinned by [`pool_dispatch_policy_for_worker_id`], rather than on the
/// driver its work item resolves to.
///
/// The companion of that function for callers that have a kind but no worker
/// id yet — chiefly `crate::work::driver_allocation`, which decides a
/// row's driver when its execution row is created, long before a pool slot
/// is claimed. Traffic allocation records the pool's fixed driver for these
/// executions (reason `pool`) rather than allocating them: their driver is
/// already decided elsewhere, so an allocation would be a decision record
/// that does not describe where the worker actually ran.
///
/// `pr_review` (review pool) and `automation_triage` (automation pool) are
/// the two kinds that are *always* pool-bound by kind alone. Ordinary work
/// originating from an automation prefers the automation pool, but can spill
/// to an ordinary `worker-N` slot, so provenance alone is not a pool-driver
/// guarantee and is intentionally not included here.
///
pub fn kind_always_dispatches_on_pool_driver(kind: &ExecutionKind) -> bool {
    match kind {
        ExecutionKind::PrReview | ExecutionKind::AutomationTriage => true,
        // Pool-bound (review pool) but NOT on `REVIEWER_POOL_DRIVER`
        // ("claude") — this kind pins its own fixed `codex`/`gpt-6-astra`
        // profile in `compose_worker_spawn`, independent of this generic
        // pool-driver mechanism. See `execution_targets_review_pool`.
        ExecutionKind::PrReviewGuide => false,
        ExecutionKind::AnswerAgent
        | ExecutionKind::ChoreImplementation
        | ExecutionKind::CiRemediation
        | ExecutionKind::ConflictResolution
        | ExecutionKind::InvestigationImplementation
        | ExecutionKind::ProductDesign
        | ExecutionKind::ProjectDesign
        | ExecutionKind::RevisionImplementation
        | ExecutionKind::TaskImplementation => false,
    }
}

/// Driver slug every execution of a pool-bound [`ExecutionKind`] actually
/// runs on — the companion of [`kind_always_dispatches_on_pool_driver`] for
/// callers that have a kind but no worker id (notably
/// `crate::work::driver_lookup`'s events-socket resolver).
///
/// Returns [`REVIEWER_POOL_DRIVER`] for `pr_review` / `automation_triage`
/// and `None` for main-pool kinds. Matches what
/// [`pool_dispatch_policy_for_worker_id`] returns for a `review-*` /
/// `auto-worker-*` slot today, so a reviewed row's live `tasks.driver` pin
/// cannot select the wrong normaliser for the reviewer worker's hook stream.
pub fn pool_driver_slug_for_execution_kind(kind: &ExecutionKind) -> Option<&'static str> {
    kind_always_dispatches_on_pool_driver(kind).then_some(REVIEWER_POOL_DRIVER)
}
