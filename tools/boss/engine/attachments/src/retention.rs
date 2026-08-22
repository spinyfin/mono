//! Retention for stored evidence.
//!
//! Same shape as the Codex/Grok home sweeps
//! (`tools/boss/engine/grok-home-retention/src/lib.rs`) so an operator has one
//! retention model to learn, not three: age-based with a total-bytes backstop,
//! candidates supplied by the caller from recorded rows rather than discovered
//! by scanning a directory, and liveness taken as a fact about the owning
//! execution rather than inferred from file mtime.
//!
//! ## Policy
//!
//! An attachment is eligible once it is older than
//! [`AttachmentRetentionPolicy::max_age`] (default 90 days). Inside that
//! window, [`AttachmentRetentionPolicy::max_total_bytes`] (default 1 GiB)
//! reclaims oldest-first until the store fits. The window is generous on
//! purpose — screenshots are small and a local gallery link that dies in a
//! fortnight is barely better than no link — but it is bounded and enforced, not a
//! promise to add GC later.
//!
//! ## Two rules the home sweeps do not need
//!
//! 1. **Blobs are shared.** Content addressing means several rows can point at
//!    one file, so a blob is deleted only when *every* row referencing that
//!    digest is being reclaimed. Reclaiming one row of a deduplicated pair
//!    must not blank the other.
//! 2. **Rows outlive blobs.** Reclaiming deletes bytes and stamps
//!    `reclaimed_at`; it does not delete the row. A stale local gallery link
//!    then answers "reclaimed on <date>" instead of a bare 404 that reads like
//!    a bug. A tombstone is not removed when its execution is pruned: the
//!    row's provenance is denormalized, so it outlives the execution and only
//!    attachment retention takes its bytes. Tombstone rows are kept
//!    indefinitely by design. They are a few hundred bytes of metadata, they
//!    are what makes a stale gallery link answer instead of 404, and there is
//!    no later reconstruction of that provenance once the row is gone. Live
//!    attachment caps count only `reclaimed_at IS NULL` rows, so a tombstone
//!    does not consume ingest budget.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use boss_protocol::AttachmentMediaType;

/// Default retention window. Longer than the 14-day home-retention window
/// because the artefacts are ~1000x smaller and local inspection may happen
/// well after a run completes.
pub const DEFAULT_MAX_AGE_DAYS: u64 = 90;

/// Default total-bytes backstop across retained blobs.
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;

/// Env var overriding [`DEFAULT_MAX_AGE_DAYS`].
pub const MAX_AGE_DAYS_ENV: &str = "BOSS_ATTACHMENT_RETENTION_DAYS";
/// Env var overriding [`DEFAULT_MAX_TOTAL_BYTES`].
pub const MAX_TOTAL_BYTES_ENV: &str = "BOSS_ATTACHMENT_MAX_TOTAL_BYTES";

/// Operator-tunable retention bound. See the module doc for the rule.
#[derive(Debug, Clone, Copy)]
pub struct AttachmentRetentionPolicy {
    pub max_age: Duration,
    pub max_total_bytes: u64,
}

impl Default for AttachmentRetentionPolicy {
    fn default() -> Self {
        let max_age_days = std::env::var(MAX_AGE_DAYS_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_MAX_AGE_DAYS);
        let max_total_bytes = std::env::var(MAX_TOTAL_BYTES_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_MAX_TOTAL_BYTES);
        Self {
            max_age: Duration::from_secs(max_age_days.saturating_mul(24 * 60 * 60)),
            max_total_bytes,
        }
    }
}

impl AttachmentRetentionPolicy {
    /// Construct with explicit knobs (tests / `bossctl` flags).
    pub fn new(max_age: Duration, max_total_bytes: u64) -> Self {
        Self {
            max_age,
            max_total_bytes,
        }
    }
}

/// One retained attachment, as the sweep sees it. Built by the engine from
/// `work_attachments` joined against execution status — never by scanning the
/// store directory, so a blob the database does not know about is never
/// deleted by this path.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct RetainedAttachment {
    pub id: String,
    pub content_digest: String,
    pub media_type: AttachmentMediaType,
    /// `true` when the owning execution is non-terminal. Live runs' evidence
    /// is never reclaimed, regardless of age or the byte cap — a run still
    /// going is still going to want to link it.
    pub is_live: bool,
    /// Epoch seconds the age is measured from: the attachment's `created_at`.
    pub age_anchor_epoch: i64,
    pub size_bytes: u64,
}

/// What a sweep decided, before touching anything.
#[derive(Debug, Default, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct ReclaimPlan {
    /// Rows to stamp `reclaimed_at`.
    pub rows_to_reclaim: Vec<String>,
    /// Blobs safe to delete: every row referencing this digest is in
    /// `rows_to_reclaim`.
    pub blobs_to_delete: Vec<(String, AttachmentMediaType)>,
    pub scanned: usize,
    pub skipped_live: usize,
    pub kept_in_policy: usize,
    /// Bytes freed by `blobs_to_delete` — counted once per blob, not once per
    /// row, so a deduplicated pair does not double-count.
    pub reclaimed_bytes: u64,
}

/// Decide what to reclaim. Pure: no filesystem, no clock, no database.
///
/// `candidates` is every non-reclaimed row in the store.
pub fn plan_reclaim(
    candidates: &[RetainedAttachment],
    now_epoch: i64,
    policy: &AttachmentRetentionPolicy,
) -> ReclaimPlan {
    let mut plan = ReclaimPlan {
        scanned: candidates.len(),
        ..ReclaimPlan::default()
    };

    let max_age_secs = policy.max_age.as_secs() as i64;
    let mut eligible: Vec<&RetainedAttachment> = Vec::new();
    let mut retained: Vec<&RetainedAttachment> = Vec::new();
    for candidate in candidates {
        if candidate.is_live {
            plan.skipped_live += 1;
            retained.push(candidate);
            continue;
        }
        if now_epoch.saturating_sub(candidate.age_anchor_epoch) > max_age_secs {
            eligible.push(candidate);
        } else {
            retained.push(candidate);
        }
    }

    // Byte backstop: with the age-eligible set already gone, is what remains
    // still over budget? Reclaim oldest-first until it is not. Size is
    // charged per distinct blob, because that is what deleting actually
    // frees.
    let mut retained_by_age = retained.clone();
    retained_by_age.sort_by_key(|a| a.age_anchor_epoch);
    let mut counted: HashSet<&str> = HashSet::new();
    let mut retained_bytes: u64 = 0;
    for candidate in &retained_by_age {
        if counted.insert(candidate.content_digest.as_str()) {
            retained_bytes = retained_bytes.saturating_add(candidate.size_bytes);
        }
    }
    for candidate in retained_by_age {
        if retained_bytes <= policy.max_total_bytes {
            break;
        }
        // A live run's evidence is never reclaimed, even to satisfy the byte
        // cap: the cap is a growth bound, not a reason to break a running
        // worker's link. If live evidence alone exceeds the cap the sweep
        // simply reports it, which is the loud-failure behaviour we want.
        if candidate.is_live {
            continue;
        }
        retained_bytes = retained_bytes.saturating_sub(candidate.size_bytes);
        eligible.push(candidate);
    }

    let eligible_ids: HashSet<&str> = eligible.iter().map(|a| a.id.as_str()).collect();
    plan.kept_in_policy = candidates.len() - eligible_ids.len() - plan.skipped_live;

    // A blob is only deletable when no surviving row still points at it.
    let mut rows_per_digest: HashMap<&str, usize> = HashMap::new();
    for candidate in candidates {
        *rows_per_digest.entry(candidate.content_digest.as_str()).or_default() += 1;
    }
    let mut reclaimed_per_digest: HashMap<&str, (usize, AttachmentMediaType, u64)> = HashMap::new();
    for candidate in &eligible {
        let entry = reclaimed_per_digest
            .entry(candidate.content_digest.as_str())
            .or_insert((0, candidate.media_type, candidate.size_bytes));
        entry.0 += 1;
    }

    plan.rows_to_reclaim = eligible.iter().map(|a| a.id.clone()).collect();
    plan.rows_to_reclaim.sort();
    plan.rows_to_reclaim.dedup();

    let mut blobs: Vec<(String, AttachmentMediaType, u64)> = reclaimed_per_digest
        .into_iter()
        .filter(|(digest, (count, _, _))| rows_per_digest.get(digest).copied().unwrap_or(0) == *count)
        .map(|(digest, (_, media, size))| (digest.to_owned(), media, size))
        .collect();
    blobs.sort_by(|a, b| a.0.cmp(&b.0));
    plan.reclaimed_bytes = blobs.iter().map(|(_, _, size)| *size).sum();
    plan.blobs_to_delete = blobs.into_iter().map(|(digest, media, _)| (digest, media)).collect();

    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 24 * 60 * 60;

    fn attachment(id: &str, digest: &str, age_days: i64, size: u64, is_live: bool) -> RetainedAttachment {
        RetainedAttachment {
            id: id.to_owned(),
            content_digest: digest.to_owned(),
            media_type: AttachmentMediaType::Png,
            is_live,
            age_anchor_epoch: 1_000_000_000 - age_days * DAY,
            size_bytes: size,
        }
    }

    fn now() -> i64 {
        1_000_000_000
    }

    #[test]
    fn reclaims_only_past_the_age_window() {
        let candidates = vec![
            attachment("atc_old", "aa", 120, 100, false),
            attachment("atc_fresh", "bb", 3, 100, false),
        ];
        let policy = AttachmentRetentionPolicy::new(Duration::from_secs(90 * DAY as u64), u64::MAX);
        let plan = plan_reclaim(&candidates, now(), &policy);

        assert_eq!(plan.rows_to_reclaim, vec!["atc_old".to_owned()]);
        assert_eq!(plan.blobs_to_delete, vec![("aa".to_owned(), AttachmentMediaType::Png)]);
        assert_eq!(plan.kept_in_policy, 1);
        assert_eq!(plan.reclaimed_bytes, 100);
    }

    #[test]
    fn never_reclaims_a_live_execution_however_old() {
        let candidates = vec![attachment("atc_live", "aa", 400, 100, true)];
        let policy = AttachmentRetentionPolicy::new(Duration::from_secs(DAY as u64), 0);
        let plan = plan_reclaim(&candidates, now(), &policy);

        assert!(plan.rows_to_reclaim.is_empty(), "live evidence is never reclaimed");
        assert!(plan.blobs_to_delete.is_empty());
        assert_eq!(plan.skipped_live, 1);
    }

    #[test]
    fn shared_blob_survives_until_its_last_row_is_reclaimed() {
        // Two rows, one blob (a revision chain re-rendered an unchanged
        // view). Only the older row ages out.
        let candidates = vec![
            attachment("atc_old", "shared", 120, 100, false),
            attachment("atc_new", "shared", 3, 100, false),
        ];
        let policy = AttachmentRetentionPolicy::new(Duration::from_secs(90 * DAY as u64), u64::MAX);
        let plan = plan_reclaim(&candidates, now(), &policy);

        assert_eq!(plan.rows_to_reclaim, vec!["atc_old".to_owned()]);
        assert!(
            plan.blobs_to_delete.is_empty(),
            "the surviving row still points at this blob; deleting it would blank a live link"
        );
        assert_eq!(plan.reclaimed_bytes, 0);
    }

    #[test]
    fn shared_blob_is_deleted_once_every_row_ages_out() {
        let candidates = vec![
            attachment("atc_a", "shared", 120, 100, false),
            attachment("atc_b", "shared", 130, 100, false),
        ];
        let policy = AttachmentRetentionPolicy::new(Duration::from_secs(90 * DAY as u64), u64::MAX);
        let plan = plan_reclaim(&candidates, now(), &policy);

        assert_eq!(plan.rows_to_reclaim, vec!["atc_a".to_owned(), "atc_b".to_owned()]);
        assert_eq!(plan.blobs_to_delete.len(), 1);
        assert_eq!(
            plan.reclaimed_bytes, 100,
            "one blob freed 100 bytes, not 200 — dedup must not double-count"
        );
    }

    #[test]
    fn byte_backstop_reclaims_oldest_first_inside_the_age_window() {
        let candidates = vec![
            attachment("atc_1", "d1", 10, 400, false),
            attachment("atc_2", "d2", 5, 400, false),
            attachment("atc_3", "d3", 1, 400, false),
        ];
        // Everything is inside the age window; the cap forces two out.
        let policy = AttachmentRetentionPolicy::new(Duration::from_secs(90 * DAY as u64), 500);
        let plan = plan_reclaim(&candidates, now(), &policy);

        assert_eq!(
            plan.rows_to_reclaim,
            vec!["atc_1".to_owned(), "atc_2".to_owned()],
            "oldest first until the store fits"
        );
        assert_eq!(plan.reclaimed_bytes, 800);
    }

    #[test]
    fn empty_input_is_a_noop() {
        let plan = plan_reclaim(&[], now(), &AttachmentRetentionPolicy::default());
        assert_eq!(plan, ReclaimPlan::default());
    }
}
