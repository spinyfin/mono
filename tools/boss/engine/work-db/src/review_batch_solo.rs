//! One-member pre-merge review batch support (`single_reviewer = 1`).
//!
//! Used by repeat pre-merge rounds (e.g. Light-tier delta re-reviews) that run
//! one `ClaudeReviewer` at `ReviewProfile::Light.model_tier()` with no
//! consolidating supervisor, while keeping the batch's whole-PR
//! `ReviewClassification` intact for post-merge eligibility and sharing the
//! full batch verdict-application pipeline (cycle counting, stale-head guard,
//! duplicate-head guard, deletion tripwire, revision-hold settlement, and
//! structured verdict storage).

use anyhow::Result;
use boss_protocol::{
    ReviewBatch, ReviewBatchMember, ReviewBatchMemberRole, ReviewBatchMemberStatus, ReviewBatchPhase, ReviewProfile,
};
use rusqlite::{Connection, Transaction, params};

use super::PendingEvents;
use super::review_batches::{
    POST_MERGE_BATCH_RESERVATION_UNITS, PRE_MERGE_BATCH_RESERVATION_UNITS, ReviewBatchMemberCreateInput,
    ReviewBatchQuorumOutcome, fail_review_batch_with_attention, latest_attempt_for_role, member_attempt_is_settled,
};

/// Reservation weight of one non-terminal single-reviewer pre-merge batch: one
/// reviewer slot, with no second leaf or supervisor.
pub const SOLO_PRE_MERGE_BATCH_RESERVATION_UNITS: i64 = 1;

pub(super) const REVIEW_BATCH_SINGLE_REVIEWER_MIGRATION_ID: &str = "20261127_2200_review_batch_single_reviewer";

/// Add `pr_review_batches.single_reviewer` (default 0 for multi-leaf batches).
pub(super) fn migrate_review_batch_single_reviewer(conn: &Connection) -> Result<()> {
    if !super::schema_init::table_has_column(conn, "pr_review_batches", "single_reviewer")? {
        conn.execute_batch("ALTER TABLE pr_review_batches ADD COLUMN single_reviewer INTEGER NOT NULL DEFAULT 0")?;
    }
    Ok(())
}

/// Count non-terminal (reservation-holding) batches of `phase` whose cycle
/// root is still a live work item.
pub(super) fn reserved_batch_count_in_tx(tx: &Transaction<'_>, phase: ReviewBatchPhase) -> Result<i64> {
    let count: i64 = tx.query_row(
        "SELECT COUNT(*)
         FROM pr_review_batches b
         JOIN tasks t ON t.id = b.cycle_root_id
         WHERE b.phase = ?1
           AND b.status NOT IN ('completed', 'failed')
           AND t.deleted_at IS NULL
           AND t.status NOT IN ('done', 'archived')",
        params![phase.as_str()],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// Floors capacity at `PRE_MERGE_BATCH_RESERVATION_UNITS` so a pre-merge batch
/// can always be admitted on an empty pool.
pub(super) fn reservation_capacity(review_pool_size: usize) -> i64 {
    review_pool_size.clamp(
        PRE_MERGE_BATCH_RESERVATION_UNITS as usize,
        boss_engine_storage_values::pool_policy::MAX_REVIEW_POOL_SIZE,
    ) as i64
}

/// Sum reserved review-pool units across live pre-merge batches: 1 unit for a
/// single-reviewer batch and `PRE_MERGE_BATCH_RESERVATION_UNITS` for a
/// multi-leaf batch.
pub(super) fn reserved_pre_merge_units_in_tx(tx: &Transaction<'_>) -> Result<i64> {
    let units: i64 = tx.query_row(
        "SELECT COALESCE(SUM(CASE WHEN b.single_reviewer != 0 THEN ?1 ELSE ?2 END), 0)
         FROM pr_review_batches b
         JOIN tasks t ON t.id = b.cycle_root_id
         WHERE b.phase = 'pre_merge'
           AND b.status NOT IN ('completed', 'failed')
           AND t.deleted_at IS NULL
           AND t.status NOT IN ('done', 'archived')",
        params![
            SOLO_PRE_MERGE_BATCH_RESERVATION_UNITS,
            PRE_MERGE_BATCH_RESERVATION_UNITS
        ],
        |row| row.get(0),
    )?;
    Ok(units)
}

/// Whether one more non-terminal batch of `phase` can be admitted into the
/// review pool's weighted reservation accounting right now.
pub fn can_admit_review_batch_in_tx(
    tx: &Transaction<'_>,
    phase: ReviewBatchPhase,
    reservation_capacity: i64,
) -> Result<bool> {
    let units = match phase {
        ReviewBatchPhase::PreMerge => PRE_MERGE_BATCH_RESERVATION_UNITS,
        ReviewBatchPhase::PostMerge => POST_MERGE_BATCH_RESERVATION_UNITS,
    };
    can_admit_review_batch_for_units_in_tx(tx, phase, units, reservation_capacity)
}

pub(super) fn can_admit_review_batch_for_units_in_tx(
    tx: &Transaction<'_>,
    phase: ReviewBatchPhase,
    units_needed: i64,
    reservation_capacity: i64,
) -> Result<bool> {
    let pre_merge_units = reserved_pre_merge_units_in_tx(tx)?;
    match phase {
        ReviewBatchPhase::PreMerge => Ok(pre_merge_units + units_needed <= reservation_capacity),
        ReviewBatchPhase::PostMerge => {
            let post_merge_units =
                reserved_batch_count_in_tx(tx, ReviewBatchPhase::PostMerge)? * POST_MERGE_BATCH_RESERVATION_UNITS;
            Ok(pre_merge_units + post_merge_units + units_needed <= reservation_capacity)
        }
    }
}

/// Build the sole `ClaudeReviewer` member for a one-member Light-tier
/// pre-merge batch. Deliberately uses `ReviewProfile::Light.model_tier()`
/// regardless of the batch's whole-PR `classification.profile` so the batch
/// row retains the PR's real classification for post-merge eligibility while
/// this repeat reviewer runs at Light tier (`sonnet`).
pub(super) fn solo_pre_merge_member_input(
    registry: &boss_engine_driver::DriverRegistry,
    execution_id: String,
) -> Result<ReviewBatchMemberCreateInput> {
    let driver = "claude";
    let driver_descriptor = registry
        .get(driver)
        .ok_or_else(|| anyhow::anyhow!("review batch driver {driver:?} is not registered"))?
        .descriptor();
    Ok(ReviewBatchMemberCreateInput::builder()
        .attempt(1)
        .provider_effort("medium")
        .requested_driver(driver)
        .resolved_model((driver_descriptor.model_menu.review_model_for_tier)(
            ReviewProfile::Light.model_tier(),
        ))
        .role(ReviewBatchMemberRole::ClaudeReviewer)
        .status(ReviewBatchMemberStatus::Pending)
        .execution_id(execution_id)
        .build())
}

/// Advance a `collecting` single-reviewer pre-merge batch straight to
/// `applying` when its sole `ClaudeReviewer` reports its verdict, or to
/// `failed` when its retry is exhausted. No supervisor is ever dispatched.
pub(super) fn try_advance_solo_pre_merge_quorum_in_tx(
    tx: &Transaction<'_>,
    batch: &ReviewBatch,
    members: &[ReviewBatchMember],
    now: &str,
    pending: &mut PendingEvents,
) -> Result<ReviewBatchQuorumOutcome> {
    match latest_attempt_for_role(members, ReviewBatchMemberRole::ClaudeReviewer) {
        Some(member) if member.status == ReviewBatchMemberStatus::Reported => {
            let proposal_id = member.report_proposal_id.clone().ok_or_else(|| {
                anyhow::anyhow!("reported single-reviewer pre-merge member is missing its report_proposal_id")
            })?;
            tx.execute(
                "UPDATE pr_review_batches
                 SET status = 'applying', updated_at = ?2, final_verdict_proposal_id = ?3
                 WHERE id = ?1",
                params![batch.id, now, proposal_id],
            )?;
            Ok(ReviewBatchQuorumOutcome::Applying)
        }
        Some(member) if member_attempt_is_settled(member) => {
            fail_review_batch_with_attention(
                pending,
                tx,
                batch,
                now,
                "Automated reviewer: repeat review did not produce a verdict",
                format!(
                    "The single-reviewer repeat pass for {} (batch `{}`) exhausted its one retry \
                     without submitting a review verdict.",
                    batch.pr_url, batch.id,
                ),
            )?;
            Ok(ReviewBatchQuorumOutcome::InsufficientQuorum)
        }
        _ => Ok(ReviewBatchQuorumOutcome::NoOp),
    }
}
