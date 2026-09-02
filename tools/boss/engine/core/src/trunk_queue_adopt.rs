//! Adoption of Trunk merge-queue episodes Boss did not initiate.
//!
//! # The hole this closes
//!
//! Trunk merge-queue coverage used to be split across two lanes that only
//! together cover the signal, and the split had a gap:
//!
//! 1. **The PR-head rollup** ([`crate::merge_poller::classify_ci`])
//!    deliberately drops Trunk's own `"Trunk Merge Queue (<branch>)"` check
//!    from the required-failure set. That exclusion is correct — the check
//!    is not a CI run, and letting it read as a failing required check
//!    spawns a duplicate, misleading `pr_branch_ci` remediation — and it
//!    stays exactly as it is. It states that the Trunk lane owns this
//!    signal.
//!
//! 2. **The Trunk queue poller** ([`crate::trunk_queue_poller`]) enumerates
//!    exactly one thing: `active` rows of `trunk_merge_intents`, whose only
//!    production writer is the Boss merge verb
//!    (`app::review::handle_trunk_queue_merge`). A queue with no active
//!    intents is idled off entirely.
//!
//! So the union of the two lanes is complete only if **every** Trunk queue
//! episode is Boss-initiated — and nothing enforces that. Trunk advertises
//! the opposite affordance on every PR it touches ("To merge this pull
//! request, check the box to the left or comment `/trunk merge` below"), and
//! an operator using it falls straight through: lane 1 discards the eviction
//! by design, lane 2 never enumerates the episode, and no `ci-fix` revision
//! is ever minted. Observed on `brianduff/flunge` PR #1156, where an
//! eviction went undetected for the 85 minutes until a human found it by
//! hand; the poller was meanwhile recording `ci_required_state: "success"`,
//! *correctly*, because the failure was never on the PR head at all — it was
//! Buildkite build 2837 on the ephemeral `trunk-merge/pr-1156/…`
//! construction branch.
//!
//! # What this module does instead
//!
//! It makes the invariant true rather than depending on it. Lane 1 stops
//! discarding the Trunk check ([`crate::merge_poller::trunk_queue_check_failure`])
//! and uses it to *adopt* the episode: one `trunk_merge_intents` row, so the
//! queue poller's candidate set becomes "every episode Boss has observed"
//! instead of "every episode Boss started". Every step downstream then runs
//! unchanged — `getSubmittedPullRequest` resolves the terminal state,
//! `handle_trunk_queue_eviction` classifies it against the Buildkite
//! construction build and the `trunk-io[bot]` comment, and a `TestFailure`
//! reaches `ci_watch::on_trunk_queue_eviction_detected` and mints the
//! revision. Nothing about the remediation path is duplicated or forked
//! here: adoption is the only new step, and it exists purely to make the
//! episode enumerable.
//!
//! Keeping detection in one lane is also what keeps remediation single.
//! Minting stays exclusively with the queue poller, under its one
//! `trunk:<entry-id>@<stateChangedAt>` discriminator — this module never
//! writes a `ci_remediations` row, so there is no second discriminator that
//! could double-remediate the same eviction.

use std::time::Duration;

use crate::merge_mechanism::MergeMechanism;
use crate::merge_poller::PrLifecycleProbe;
use crate::work::{PendingMergeCheck, TrunkMergeIntentInsertInput, WorkDb};

/// How long after Trunk concluded its check an eviction may still be
/// adopted.
///
/// Adoption is deliberately not gated on Boss having started the episode, so
/// without a bound it is also not gated on the episode being *recent*: a
/// failed Trunk check sits on a PR head until that head moves, so the first
/// sweep after this ships would otherwise adopt every long-abandoned queue
/// attempt in the repo at once — each minting a `ci-fix` revision, and each
/// carrying the same authority as a merge-verb intent, whose tail is
/// `ci_watch::on_ci_resolved` -> `mark_trunk_intent_awaiting_resubmit` ->
/// `resubmit_intent`. That is Boss re-enqueueing and merging a PR whose
/// queue attempt a human deliberately walked away from days ago, which no
/// part of this design intends.
///
/// Six hours is two orders of magnitude above the merge poller's coldest
/// per-PR cadence (`PollTier::Cold`, 180 s), so no episode that is still
/// plausibly live is missed by it, while an episode abandoned overnight is
/// out of reach. Declining is loud (`warn!`, naming the age) rather than
/// silent — a suppressed eviction must always be visible.
const MAX_ADOPTABLE_EPISODE_AGE: Duration = Duration::from_secs(6 * 60 * 60);

/// Age in seconds of a Trunk check run that concluded at `completed_at`
/// (verbatim RFC 3339, as GitHub reports it) as of `now_epoch_secs`.
///
/// `None` when the leaf carried no `completedAt` or GitHub reported one this
/// can't parse — in both cases there is nothing to bound the episode
/// against, and the caller adopts rather than declining on a missing datum.
/// A negative age (Trunk's clock marginally ahead of ours) is a fresh
/// episode, not a stale one, and is returned as-is so the caller's
/// `> MAX_ADOPTABLE_EPISODE_AGE` comparison admits it.
fn episode_age_secs(completed_at: Option<&str>, now_epoch_secs: i64) -> Option<i64> {
    let completed = chrono::DateTime::parse_from_rfc3339(completed_at?).ok()?;
    Some(now_epoch_secs - completed.timestamp())
}

/// Whether a Trunk check run's `completedAt` is older than
/// [`MAX_ADOPTABLE_EPISODE_AGE`]. Shared with the merge-poller mint path
/// so a Direct-mechanism product (which never adopts) still declines to
/// resurrect an abandoned queue attempt. Missing/`unparseable` timestamps
/// are treated as fresh — same as adoption.
pub(crate) fn trunk_check_episode_is_stale(completed_at: Option<&str>) -> bool {
    episode_age_secs(completed_at, boss_engine_utils::epoch_time::now_epoch_secs())
        .is_some_and(|age| age > MAX_ADOPTABLE_EPISODE_AGE.as_secs() as i64)
}

/// Adopt the Trunk merge-queue episode `probe` just reported an eviction
/// for, if there is one and no lane already owns it. Returns `true` when a
/// `trunk_merge_intents` row was actually inserted.
///
/// Called from the merge poller's per-candidate sweep for every open PR, so
/// the common case — any PR on any product without a failed Trunk check — is
/// one `Option` test and no DB traffic at all.
///
/// Ordered gates, each a genuine no-op rather than a swallowed failure:
///
///   1. **No failed Trunk check on the head** → nothing happened. Note that
///      "failed" here is [`crate::merge_poller::is_trunk_eviction_conclusion`],
///      not the broad CI failure set: a `CANCELLED` check is a human
///      decision, and adopting on it would file a
///      "removed from the Trunk merge queue" attention item for a card that
///      was never in the Merging lane.
///   2. **Not a `trunk_queue` product** → this repo has no Trunk queue; a
///      leaf with that name could not be about one.
///   3. **The episode is older than [`MAX_ADOPTABLE_EPISODE_AGE`]** → a
///      human walked away from it; resurrecting it would eventually
///      re-enqueue and merge a PR they abandoned. Declined at `warn!` with
///      the age, never silently.
///   4. **An active intent already exists** → the queue poller is already
///      tracking this work item and will resolve the eviction itself. This
///      is the no-duplicate-remediation gate: adopting here would be a
///      second row for the same episode. It also covers the whole
///      remediation window, because an evicted intent is deliberately left
///      `active` until its fix lands.
///   5. **No head sha** → there is no episode key to be idempotent on, and
///      adopting without one would re-adopt every sweep. Logged loudly; in
///      practice GitHub always reports one.
///   6. **This exact episode was already adopted** → exactly-once. The key
///      is `(work_item, head sha, check `completedAt`)`, not
///      `(work_item, head sha)`: a commit is not an episode. A human can
///      cancel and re-check Trunk's box, or requeue after the base-mismatch
///      retire whose own attention item tells them to, all without moving
///      the head — and the second eviction must be adopted, not declined.
///
/// Note what is *not* a gate: whether Boss started the episode, who the
/// operator is, or which product it belongs to. The failure mode being fixed
/// is precisely an episode Boss did not initiate, so "Boss did not initiate
/// it" cannot be a reason to decline.
/// Synchronous on purpose: adoption is local-DB only. It issues no Trunk
/// call and no GitHub call — the observation it acts on was already paid for
/// by the merge poller's batched probe — so it adds nothing to either budget
/// and cannot stall the sweep it runs inside.
pub fn adopt_unattributed_trunk_queue_episode(
    work_db: &WorkDb,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
) -> bool {
    let Some(check) = probe.trunk_queue_check_failure.as_ref() else {
        return false;
    };
    let Some(default_target_branch) = trunk_queue_target_branch(work_db, &candidate.product_id) else {
        return false;
    };

    let age_secs = episode_age_secs(
        check.completed_at.as_deref(),
        boss_engine_utils::epoch_time::now_epoch_secs(),
    );
    if age_secs.is_some_and(|age| age > MAX_ADOPTABLE_EPISODE_AGE.as_secs() as i64) {
        // Repeats every sweep for as long as the abandoned PR stays open on
        // this head, and that is the intended trade: a Trunk eviction Boss
        // is choosing not to act on is exactly the thing that must not be
        // invisible, and the condition is operator-actionable (close the PR,
        // push a fix, or requeue it).
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            check = %check.name,
            completed_at = check.completed_at.as_deref().unwrap_or(""),
            age_secs = age_secs.unwrap_or_default(),
            max_age_secs = MAX_ADOPTABLE_EPISODE_AGE.as_secs(),
            "trunk queue adopt: declining to adopt a Trunk merge-queue eviction older than the \
             adoption window; a stale episode would be remediated and eventually auto-resubmitted, \
             re-enqueueing a PR whose queue attempt was abandoned",
        );
        return false;
    }

    match work_db.get_active_trunk_merge_intent(&candidate.work_item_id) {
        Ok(Some(intent)) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                intent_id = %intent.id,
                "trunk queue adopt: an active merge intent already tracks this work item; \
                 the queue poller owns this eviction",
            );
            return false;
        }
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "trunk queue adopt: failed to read the active trunk merge intent; \
                 leaving this eviction unadopted this pass",
            );
            return false;
        }
    }

    let Some(head_sha) = probe.head_ref_oid.as_deref() else {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            check = %check.name,
            "trunk queue adopt: Trunk reports an evicted queue episode but GitHub reported no head sha; \
             cannot adopt it idempotently, so this eviction stays unremediated",
        );
        return false;
    };
    match work_db.trunk_merge_intent_adopted_at_episode(
        &candidate.work_item_id,
        head_sha,
        check.completed_at.as_deref(),
    ) {
        Ok(true) => {
            // `debug!` rather than `warn!`, and this is the one decline for
            // which that is right: the key is now per-episode, so reaching
            // here means Boss has already acted on *this* eviction and is
            // merely seeing the same still-failed leaf again — which it will
            // on every sweep until the head moves. Nothing is suppressed.
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                head_sha,
                completed_at = check.completed_at.as_deref().unwrap_or(""),
                "trunk queue adopt: this episode has already been adopted",
            );
            return false;
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "trunk queue adopt: failed to check prior adoptions; \
                 leaving this eviction unadopted this pass",
            );
            return false;
        }
    }

    let coords = match crate::trunk_merge::parse_trunk_pr_coordinates(&candidate.pr_url) {
        Ok(coords) => coords,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                %err,
                "trunk queue adopt: cannot derive Trunk coordinates from the PR URL; \
                 this eviction stays unremediated",
            );
            return false;
        }
    };
    // The PR's own base is the queue this episode belongs to — a PR can only
    // be queued into the branch it targets. The product-level default is the
    // fallback for the rare probe that reports no `baseRefName`.
    let target_branch = probe
        .base_ref_name
        .clone()
        .unwrap_or_else(|| default_target_branch.clone());

    let input = TrunkMergeIntentInsertInput::builder()
        .work_item_id(candidate.work_item_id.clone())
        .pr_url(candidate.pr_url.clone())
        .pr_number(coords.number as i64)
        .repo(format!("{}/{}", coords.owner, coords.repo))
        .target_branch(target_branch.clone())
        .adopted_at_head_sha(head_sha.to_owned())
        .maybe_adopted_at_check_completed_at(check.completed_at.clone())
        .build();
    match work_db.insert_trunk_merge_intent(input) {
        Ok(Some(intent)) => {
            // `warn!`, not `info!`: an eviction Boss cannot attribute to
            // anything it did is exactly the condition that went unnoticed
            // for 85 minutes, and the durable `adopted_at_head_sha` marker
            // this writes is the other half of making it visible.
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                intent_id = %intent.id,
                head_sha,
                target_branch = %target_branch,
                check = %check.name,
                conclusion = %check.conclusion,
                completed_at = check.completed_at.as_deref().unwrap_or(""),
                details_url = %check.details_url,
                "trunk queue adopt: Trunk evicted a merge-queue episode Boss did not initiate; \
                 adopting it so the queue poller can resolve and remediate the eviction",
            );
            true
        }
        // Lost a race with a merge click between the gate above and here.
        // The merge verb's intent is the better record of the same episode
        // (it carries a real `submit_count`), so leave it alone.
        Ok(None) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "trunk queue adopt: an active intent appeared while adopting; leaving it to the merge verb",
            );
            false
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "trunk queue adopt: failed to insert the adopted merge intent; \
                 this eviction stays unremediated until the next pass",
            );
            false
        }
    }
}

/// The Trunk queue target branch configured for `product_id`, or `None` when
/// the product does not merge through a Trunk queue.
///
/// A corrupt `merge_mechanism` value is treated as "not a Trunk product"
/// here rather than being surfaced: `app::review::handle_merge_when_ready`
/// already fails loudly on it at merge time, which is where an operator can
/// act on it, and a poller pass is not a place to raise it a second time per
/// sweep.
fn trunk_queue_target_branch(work_db: &WorkDb, product_id: &str) -> Option<String> {
    let product = match work_db.get_product(product_id) {
        Ok(Some(product)) => product,
        Ok(None) => return None,
        Err(err) => {
            tracing::warn!(
                product_id,
                ?err,
                "trunk queue adopt: failed to load product to check merge mechanism",
            );
            return None;
        }
    };
    match MergeMechanism::parse(product.merge_mechanism.as_deref()) {
        Ok(MergeMechanism::TrunkQueue { target_branch }) => Some(target_branch),
        Ok(MergeMechanism::Direct) => None,
        Err(err) => {
            tracing::debug!(
                product_id,
                %err,
                "trunk queue adopt: unparseable merge_mechanism; not treating as a trunk_queue product",
            );
            None
        }
    }
}

#[cfg(test)]
mod tests;
