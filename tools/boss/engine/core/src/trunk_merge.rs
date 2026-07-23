//! Engine-side "submit to Trunk's merge queue" verb — the `trunk_queue`
//! sibling of [`crate::merge_when_ready::gh_merge_when_ready`]. Called by
//! `app::review::handle_merge_when_ready` once the task's product resolves
//! to [`crate::merge_mechanism::MergeMechanism::TrunkQueue`].
//!
//! Unlike the `Direct` path, this module owns no retry/HTTP logic itself —
//! that lives in `boss_trunk_client::TrunkClient` — it only derives the
//! `(owner, repo, number)` Trunk needs from the task's PR URL.

use anyhow::{Result, anyhow};

use crate::work::WorkDb;

/// The `host` every `TrunkRepoRef` Boss builds carries. Boss only ever
/// tracks GitHub-hosted PRs (`parse_trunk_pr_coordinates` rejects anything
/// else outright), so this is a constant rather than a product setting.
pub const TRUNK_REPO_HOST: &str = "github.com";

// ── Boss-synthesized `trunk_merge_intents.last_trunk_state` sentinels ──────
//
// Trunk's own PR states (`not_ready`/`pending`/…/`failed`/`cancelled`) are
// the only values the `getQueue`/`getSubmittedPullRequest` transport ever
// writes into this column. The two constants below are never sent by
// Trunk — they are Boss's own bookkeeping for the auto-resubmit /
// conflict-during-queue coordination (design
// `trunk-merge-queue-integration-queue-backed-merges-merging-ui.md`
// §"Eviction: a first-class failure signal" / §"Coordination with
// conflict_watch / ci_watch"), namespaced with a `boss:` prefix so they can
// never collide with a real (or future/unknown) Trunk state string.
//
// State flow for an intent that needs Boss-driven remediation:
//
//   `failed` / `pending_failure`  (eviction, ci_watch owns the fix)  ─┐
//   `TRUNK_INTENT_SUPERSEDED_BY_CONFLICT` (conflict mid-queue,       ─┼─▶ TRUNK_INTENT_AWAITING_RESUBMIT ─▶ submitPullRequest ─▶ (cleared; live tracking resumes)
//    conflict_watch owns the fix, poller cancels the entry)          ─┘
pub const TRUNK_INTENT_AWAITING_RESUBMIT: &str = "boss:awaiting_resubmit";
pub const TRUNK_INTENT_SUPERSEDED_BY_CONFLICT: &str = "boss:superseded_by_conflict";

/// Whether an active intent's `last_trunk_state` marks it as needing a
/// Boss-driven fix before it can be resubmitted: evicted (an active
/// `ci_watch::on_trunk_queue_eviction_detected` episode owns it) or
/// superseded by a mid-queue conflict (`conflict_watch` owns it).
fn needs_remediation(last_trunk_state: Option<&str>) -> bool {
    matches!(
        last_trunk_state,
        Some("failed") | Some("pending_failure") | Some(TRUNK_INTENT_SUPERSEDED_BY_CONFLICT)
    )
}

/// Called once the fix for an evicted or conflict-superseded Trunk intent
/// has genuinely landed (see call sites: `ci_watch::on_ci_resolved` gates
/// on the spawned revision reaching `done`; `conflict_watch::on_resolved`
/// gates on GitHub reporting the PR mergeable again). Flips the intent's
/// `last_trunk_state` sentinel to [`TRUNK_INTENT_AWAITING_RESUBMIT`] so the
/// next `TrunkQueueProbe` pass calls `submitPullRequest` again.
///
/// A no-op — not an error — when the work item has no active Trunk merge
/// intent (not a `trunk_queue` product, or the intent already retired) or
/// the intent isn't in a [`needs_remediation`] sub-state (e.g. it's still
/// live in the queue, or a resubmit is already in flight). Best-effort:
/// failures are logged, not propagated, mirroring every other side-table
/// write in the `ci_watch`/`conflict_watch` retire paths.
pub fn mark_trunk_intent_awaiting_resubmit(work_db: &WorkDb, work_item_id: &str) {
    let intent = match work_db.get_active_trunk_merge_intent(work_item_id) {
        Ok(Some(intent)) => intent,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(
                work_item_id,
                ?err,
                "trunk_merge: failed to look up active trunk merge intent",
            );
            return;
        }
    };
    if !needs_remediation(intent.last_trunk_state.as_deref()) {
        return;
    }
    if let Err(err) = work_db.record_trunk_merge_intent_state(&intent.id, TRUNK_INTENT_AWAITING_RESUBMIT) {
        tracing::warn!(
            intent_id = %intent.id,
            work_item_id,
            ?err,
            "trunk_merge: failed to mark intent awaiting_resubmit",
        );
    }
}

/// Called from `conflict_watch::on_conflict_detected` when a PR with a live
/// Trunk merge intent goes `CONFLICTING` while still enqueued. Marks the
/// intent [`TRUNK_INTENT_SUPERSEDED_BY_CONFLICT`] so the next
/// `TrunkQueueProbe` pass calls `cancelPullRequest` — the conflict resolver
/// owns the slot, per the design's "conflict pre-empts CI" precedence; no
/// eviction remediation is spawned for this exit.
///
/// A no-op when there is no active intent, or the intent is already in a
/// [`needs_remediation`] or [`TRUNK_INTENT_AWAITING_RESUBMIT`] sub-state —
/// an eviction or an already-superseded/awaiting-resubmit episode must not
/// be clobbered by a second conflict detection racing the same sweep.
pub fn mark_trunk_intent_superseded_by_conflict(work_db: &WorkDb, work_item_id: &str) {
    let intent = match work_db.get_active_trunk_merge_intent(work_item_id) {
        Ok(Some(intent)) => intent,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(
                work_item_id,
                ?err,
                "trunk_merge: failed to look up active trunk merge intent",
            );
            return;
        }
    };
    let live = !needs_remediation(intent.last_trunk_state.as_deref())
        && intent.last_trunk_state.as_deref() != Some(TRUNK_INTENT_AWAITING_RESUBMIT);
    if !live {
        return;
    }
    if let Err(err) = work_db.record_trunk_merge_intent_state(&intent.id, TRUNK_INTENT_SUPERSEDED_BY_CONFLICT) {
        tracing::warn!(
            intent_id = %intent.id,
            work_item_id,
            ?err,
            "trunk_merge: failed to mark intent superseded_by_conflict",
        );
    }
}

/// Repo/PR coordinates Trunk's queue API addresses, parsed from a task's
/// canonical GitHub PR URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrunkPrCoordinates {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

/// Parse `pr_url` (`https://github.com/<owner>/<repo>/pull/<N>`) into the
/// coordinates a `submitPullRequest` call needs. Errs loudly — no silent
/// fallback — when the URL isn't a canonical GitHub PR URL, since a
/// `trunk_queue` product's merge click has nothing else to fall back to.
pub fn parse_trunk_pr_coordinates(pr_url: &str) -> Result<TrunkPrCoordinates> {
    let (owner, repo, number) = boss_github::pr_url::parse_pr_url_parts(pr_url)
        .ok_or_else(|| anyhow!("not a canonical GitHub PR URL: {pr_url}"))?;
    Ok(TrunkPrCoordinates {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        number,
    })
}

/// Build the `{host, owner, name}` repo reference Trunk's API expects from
/// a `trunk_merge_intents.repo` value (`"<owner>/<name>"`).
///
/// Returns `None` for anything that isn't exactly one `owner/name` pair.
/// The queue poller treats that as "this intent's coordinates are
/// unusable" and parks the queue rather than issuing a request Trunk would
/// reject anyway — the column is written by
/// `app::review::handle_trunk_queue_merge` from already-parsed
/// [`TrunkPrCoordinates`], so a malformed value means data corruption, not
/// a user typo.
pub fn trunk_repo_ref(repo: &str) -> Option<boss_trunk_client::TrunkRepoRef> {
    let (owner, name) = repo.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(boss_trunk_client::TrunkRepoRef::new(TRUNK_REPO_HOST, owner, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_repo_ref_from_an_owner_name_slug() {
        let repo_ref = trunk_repo_ref("brianduff/flunge").unwrap();
        assert_eq!(repo_ref.host, TRUNK_REPO_HOST);
        assert_eq!(repo_ref.owner, "brianduff");
        assert_eq!(repo_ref.name, "flunge");
    }

    #[test]
    fn rejects_repo_slugs_that_are_not_exactly_owner_slash_name() {
        for bad in ["flunge", "", "/flunge", "brianduff/", "a/b/c"] {
            assert!(trunk_repo_ref(bad).is_none(), "expected {bad:?} to be rejected");
        }
    }

    #[test]
    fn parses_a_canonical_pr_url() {
        let coords = parse_trunk_pr_coordinates("https://github.com/brianduff/flunge/pull/978").unwrap();
        assert_eq!(
            coords,
            TrunkPrCoordinates {
                owner: "brianduff".to_owned(),
                repo: "flunge".to_owned(),
                number: 978,
            }
        );
    }

    #[test]
    fn rejects_a_non_github_url() {
        let err = parse_trunk_pr_coordinates("https://gitlab.com/o/r/-/merge_requests/1").unwrap_err();
        assert!(err.to_string().contains("not a canonical GitHub PR URL"), "{err}");
    }

    #[test]
    fn rejects_a_malformed_url() {
        assert!(parse_trunk_pr_coordinates("not a url").is_err());
    }

    // ── awaiting_resubmit / superseded_by_conflict sentinel transitions ────

    fn test_db() -> WorkDb {
        WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap()
    }

    fn seed_active_intent(db: &WorkDb, name: &str) -> String {
        let product = crate::test_support::create_test_product_named(db, name);
        let task = crate::test_support::create_test_chore_manual(db, product.id.clone(), name);
        db.insert_trunk_merge_intent(
            crate::work::TrunkMergeIntentInsertInput::builder()
                .work_item_id(task.id.clone())
                .pr_url("https://github.com/brianduff/flunge/pull/1")
                .pr_number(1)
                .repo("brianduff/flunge")
                .target_branch("main")
                .build(),
        )
        .unwrap()
        .unwrap();
        task.id
    }

    fn last_trunk_state(db: &WorkDb, work_item_id: &str) -> Option<String> {
        db.get_active_trunk_merge_intent(work_item_id)
            .unwrap()
            .unwrap()
            .last_trunk_state
    }

    #[test]
    fn awaiting_resubmit_flips_an_evicted_intent() {
        let db = test_db();
        let work_item_id = seed_active_intent(&db, "evicted");
        let intent = db.get_active_trunk_merge_intent(&work_item_id).unwrap().unwrap();
        db.record_trunk_merge_intent_state(&intent.id, "failed").unwrap();

        mark_trunk_intent_awaiting_resubmit(&db, &work_item_id);

        assert_eq!(
            last_trunk_state(&db, &work_item_id).as_deref(),
            Some(TRUNK_INTENT_AWAITING_RESUBMIT)
        );
    }

    #[test]
    fn awaiting_resubmit_flips_a_conflict_superseded_intent() {
        let db = test_db();
        let work_item_id = seed_active_intent(&db, "conflicted");
        let intent = db.get_active_trunk_merge_intent(&work_item_id).unwrap().unwrap();
        db.record_trunk_merge_intent_state(&intent.id, TRUNK_INTENT_SUPERSEDED_BY_CONFLICT)
            .unwrap();

        mark_trunk_intent_awaiting_resubmit(&db, &work_item_id);

        assert_eq!(
            last_trunk_state(&db, &work_item_id).as_deref(),
            Some(TRUNK_INTENT_AWAITING_RESUBMIT)
        );
    }

    #[test]
    fn awaiting_resubmit_is_a_no_op_for_a_live_or_missing_intent() {
        let db = test_db();
        let work_item_id = seed_active_intent(&db, "live");
        let intent = db.get_active_trunk_merge_intent(&work_item_id).unwrap().unwrap();
        db.record_trunk_merge_intent_state(&intent.id, "testing").unwrap();

        mark_trunk_intent_awaiting_resubmit(&db, &work_item_id);
        assert_eq!(last_trunk_state(&db, &work_item_id).as_deref(), Some("testing"));

        // No active intent at all — must not panic or error.
        mark_trunk_intent_awaiting_resubmit(&db, "no_such_work_item");
    }

    #[test]
    fn superseded_by_conflict_flips_a_live_intent() {
        let db = test_db();
        let work_item_id = seed_active_intent(&db, "queued");

        mark_trunk_intent_superseded_by_conflict(&db, &work_item_id);

        assert_eq!(
            last_trunk_state(&db, &work_item_id).as_deref(),
            Some(TRUNK_INTENT_SUPERSEDED_BY_CONFLICT)
        );
    }

    #[test]
    fn superseded_by_conflict_does_not_clobber_an_eviction_or_a_pending_resubmit() {
        let db = test_db();
        for state in ["failed", "pending_failure", TRUNK_INTENT_AWAITING_RESUBMIT] {
            let work_item_id = seed_active_intent(&db, &format!("guarded-{state}"));
            let intent = db.get_active_trunk_merge_intent(&work_item_id).unwrap().unwrap();
            db.record_trunk_merge_intent_state(&intent.id, state).unwrap();

            mark_trunk_intent_superseded_by_conflict(&db, &work_item_id);

            assert_eq!(
                last_trunk_state(&db, &work_item_id).as_deref(),
                Some(state),
                "state {state:?} must not be overwritten by a conflict detection",
            );
        }
    }
}
