//! The card's "ci failing" chip must follow the PR's current head.
//!
//! The app derives the chip from the newest attempt row it holds for the PR.
//! When a push replaces a red head, the engine abandons the attempt that was
//! fixing the old head; unless that terminal row reaches the app, it keeps a
//! `pending`/`running` copy and re-raises the chip on every board refresh,
//! while the engine itself records required CI as passing.

use super::helpers::*;
use boss_protocol::EngineAttemptListEntry;

/// Detect a CI failure at `head` and return the attempt it opened.
async fn fail_at(
    db: &WorkDb,
    pub_: &RecordingPublisher,
    product: &str,
    chore: &str,
    pr: &str,
    head: &str,
) -> CiRemediation {
    assert!(
        on_ci_failure_detected(
            db,
            pub_,
            &fix_checker(),
            &candidate(product, chore, pr),
            &probe(pr, head),
            &one_failure(),
        )
        .await
    );
    db.active_ci_remediation_for_work_item(chore).unwrap().unwrap()
}

/// What a freshly connected app would be told about this PR.
fn projected(db: &WorkDb, pr: &str, kind: &str) -> EngineAttemptListEntry {
    db.open_remediation_state()
        .unwrap()
        .into_iter()
        .find(|row| row.kind == kind && row.pr_url == pr)
        .unwrap_or_else(|| panic!("no {kind} row projected for {pr}"))
}

/// `(status, card state)` of an attempt row carried on a push.
fn row_state(row: &EngineAttemptListEntry) -> (&str, &str) {
    (
        row.status.as_str(),
        row.extra.get("card_state").map(String::as_str).unwrap_or(""),
    )
}

#[tokio::test]
async fn force_push_from_red_head_to_green_head_clears_the_card() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3852";
    let (product, chore) = make_in_review(&db, "C-force-push-green", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cand = candidate(&product, &chore, pr);

    let attempt = fail_at(&db, pub_.as_ref(), &product, &chore, pr, "head-red").await;
    assert_eq!(db.ci_card_state(&chore).unwrap(), "in_flight");
    pub_.typed_events.lock().await.clear();

    // The head is replaced and CI restarts on it.
    assert!(on_ci_in_flight_supersedes_failure(&db, pub_.as_ref(), &cand, &[], Some("head-green")).await);

    // The clear carries the retired attempt, so the app's copy stops reading
    // as "still fixing" and a board refresh cannot re-raise the chip.
    let typed = pub_.typed_events.lock().await.clone();
    let carried = typed
        .iter()
        .find_map(|(_, event)| match event {
            FrontendEvent::CiFailureCleared { pr_url, attempt, .. } if pr_url == pr => Some(attempt.clone()),
            _ => None,
        })
        .expect("CiFailureCleared must be published")
        .expect("the clear must carry the PR's newest attempt row");
    assert_eq!(carried.id, attempt.id);
    assert_eq!(row_state(&carried), ("abandoned", "clear"));

    // CI then passes on the new head: nothing is left to retire, and the
    // projection agrees with required CI passing.
    pub_.typed_events.lock().await.clear();
    assert!(!on_ci_resolved(&db, pub_.as_ref(), &cand, &[]).await);
    assert_eq!(db.ci_card_state(&chore).unwrap(), "clear");
    assert_eq!(projected(&db, pr, "ci_state").status, "clear");
    let latest = projected(&db, pr, "ci");
    assert_eq!(latest.id, attempt.id);
    assert_eq!(latest.status, "abandoned");
    assert_eq!(latest.extra.get("card_state").map(String::as_str), Some("clear"));
    assert!(
        db.active_ci_remediation_for_work_item(&chore).unwrap().is_none(),
        "no attempt may still be counted as fixing the replaced head",
    );
}

#[tokio::test]
async fn failing_current_head_still_shows_failing_after_a_force_push() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3853";
    let (product, chore) = make_in_review(&db, "C-force-push-red", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cand = candidate(&product, &chore, pr);

    let first = fail_at(&db, pub_.as_ref(), &product, &chore, pr, "head-red").await;
    assert!(on_ci_in_flight_supersedes_failure(&db, pub_.as_ref(), &cand, &[], Some("head-red-2")).await);
    assert_eq!(db.ci_card_state(&chore).unwrap(), "clear");
    pub_.typed_events.lock().await.clear();

    // The new head fails too.
    let second = fail_at(&db, pub_.as_ref(), &product, &chore, pr, "head-red-2").await;
    assert_ne!(second.id, first.id);
    assert_eq!(second.head_sha_at_trigger, "head-red-2");

    assert_eq!(db.ci_card_state(&chore).unwrap(), "in_flight");
    assert_eq!(projected(&db, pr, "ci_state").status, "in_flight");
    let latest = projected(&db, pr, "ci");
    assert_eq!(latest.id, second.id);
    assert!(matches!(latest.status.as_str(), "pending" | "running"));
    let typed = pub_.typed_events.lock().await.clone();
    assert!(
        typed.iter().any(|(_, event)| matches!(
            event,
            FrontendEvent::CiRemediationStarted { attempt, .. } if attempt.id == second.id
        )),
        "the attempt for the failing current head must be published as started",
    );
}

/// The failure signal can already be gone when the head moves (the parent
/// was never blocked). The supersede then reports nothing to clear, but the
/// abandoned attempt still has to be published.
#[tokio::test]
async fn stale_attempt_without_a_blocking_signal_is_still_reported() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3854";
    let (product, chore) = make_in_review(&db, "C-force-push-no-signal", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cand = candidate(&product, &chore, pr);

    let attempt = fail_at(&db, pub_.as_ref(), &product, &chore, pr, "head-red").await;
    db.clear_chore_blocked_ci_failure(&chore, pr).unwrap();
    db.clear_ci_failure_signal_only(&chore).unwrap();
    pub_.typed_events.lock().await.clear();

    assert!(!on_ci_in_flight_supersedes_failure(&db, pub_.as_ref(), &cand, &[], Some("head-green")).await);

    let typed = pub_.typed_events.lock().await.clone();
    let pushed = typed
        .iter()
        .find_map(|(_, event)| match event {
            FrontendEvent::CiRemediationAbandoned { attempt: row, .. } if row.id == attempt.id => Some(row.clone()),
            _ => None,
        })
        .expect("the abandoned attempt row must be published");
    assert_eq!(row_state(&pushed), ("abandoned", "clear"));
}
