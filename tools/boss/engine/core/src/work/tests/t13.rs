use super::*;

// ── CI in-flight observation tracking (Phase 12 #39) ───────────────────────
//
// Behavioural coverage for the `ci_inflight_observations` cluster in
// `work/blocking.rs`: `observe_ci_in_flight`, `mark_ci_inflight_alert_level`,
// and `clear_ci_inflight_observations`. These pin the observable invariants
// (returned `CiInFlightObservation` fields and re-read state) rather than any
// SQL detail.
//
// `now_string()` has whole-second resolution, so two observations in the same
// test tick would carry identical timestamps regardless of whether the row is
// re-stamped. To prove first-observation-*wins* (the row is genuinely left
// alone, not merely coincidentally equal) we backdate the stored
// `first_observed_at` to a sentinel via raw SQL and assert the sentinel
// survives the second observation.

/// Backdate the persisted `first_observed_at` for a `(work_item_id, head_sha)`
/// row to `sentinel` so a later `observe_ci_in_flight` can prove it read the
/// *first* observation rather than re-stamping `now`.
fn backdate_first_observed_at(db: &WorkDb, work_item_id: &str, head_sha: &str, sentinel: &str) {
    let conn = db.connect().unwrap();
    let n = conn
        .execute(
            "UPDATE ci_inflight_observations
                SET first_observed_at = ?3
              WHERE work_item_id = ?1 AND head_sha = ?2",
            rusqlite::params![work_item_id, head_sha, sentinel],
        )
        .unwrap();
    assert_eq!(n, 1, "backdate must touch exactly the one seeded row");
}

/// `observe_ci_in_flight` is first-observation-wins: the first call stamps
/// `first_observed_at` and starts `alert_level_emitted` at `'none'`; repeated
/// calls for the same pair return the *same* `first_observed_at`, and distinct
/// head shas track independently.
#[test]
fn observe_ci_in_flight_first_observation_wins() {
    let db = WorkDb::open(temp_db_path("observe-first-wins")).unwrap();

    // First observation stamps the row and starts at 'none'.
    let first = db.observe_ci_in_flight("task_a", "sha_1").unwrap();
    assert_eq!(first.work_item_id, "task_a");
    assert_eq!(first.head_sha, "sha_1");
    assert_eq!(
        first.alert_level_emitted, "none",
        "a fresh observation must start at the 'none' alert level"
    );
    assert!(
        !first.first_observed_at.is_empty(),
        "first_observed_at must be stamped on first observation"
    );

    // Backdate to a sentinel the wall clock can never produce, then re-observe.
    // The returned timestamp must still be the sentinel — elapsed-time math
    // reads from the first observation, never the latest.
    backdate_first_observed_at(&db, "task_a", "sha_1", "1000");
    let repeat = db.observe_ci_in_flight("task_a", "sha_1").unwrap();
    assert_eq!(
        repeat.first_observed_at, "1000",
        "a repeated observation must return the first-observed timestamp, not re-stamp now"
    );
    assert_eq!(
        repeat.alert_level_emitted, "none",
        "re-observing must not disturb the persisted alert level"
    );

    // A distinct head sha for the same work item tracks independently: it gets
    // its own fresh stamp, not the backdated sentinel of the sibling row.
    let other_sha = db.observe_ci_in_flight("task_a", "sha_2").unwrap();
    assert_ne!(
        other_sha.first_observed_at, "1000",
        "a distinct head sha must record its own first_observed_at"
    );
    assert_eq!(other_sha.alert_level_emitted, "none");

    // ...and the original pair is still untouched by the sibling observation.
    let sha1_again = db.observe_ci_in_flight("task_a", "sha_1").unwrap();
    assert_eq!(
        sha1_again.first_observed_at, "1000",
        "observing a sibling head sha must not perturb the original row"
    );
}

/// `mark_ci_inflight_alert_level` escalates monotonically
/// (`none → warn → alert`): it may upgrade, but must never downgrade, and a
/// repeated emit of the same level is a no-op. Every assertion re-reads the
/// persisted level through `observe_ci_in_flight`.
#[test]
fn mark_ci_inflight_alert_level_is_monotonic() {
    let db = WorkDb::open(temp_db_path("alert-monotonic")).unwrap();

    let level = |wid: &str, sha: &str| db.observe_ci_in_flight(wid, sha).unwrap().alert_level_emitted;

    // none → warn → alert climbs each step.
    db.observe_ci_in_flight("climb", "sha").unwrap();
    assert_eq!(level("climb", "sha"), "none");
    db.mark_ci_inflight_alert_level("climb", "sha", "warn").unwrap();
    assert_eq!(level("climb", "sha"), "warn", "none must upgrade to warn");
    db.mark_ci_inflight_alert_level("climb", "sha", "alert").unwrap();
    assert_eq!(level("climb", "sha"), "alert", "warn must upgrade to alert");

    // alert must never downgrade to warn.
    db.mark_ci_inflight_alert_level("climb", "sha", "warn").unwrap();
    assert_eq!(level("climb", "sha"), "alert", "alert must never downgrade to warn");
    // ...nor an idempotent re-emit of alert change anything.
    db.mark_ci_inflight_alert_level("climb", "sha", "alert").unwrap();
    assert_eq!(level("climb", "sha"), "alert", "re-emitting alert is a no-op");

    // warn must never downgrade to none.
    db.observe_ci_in_flight("nodrop", "sha").unwrap();
    db.mark_ci_inflight_alert_level("nodrop", "sha", "warn").unwrap();
    db.mark_ci_inflight_alert_level("nodrop", "sha", "none").unwrap();
    assert_eq!(level("nodrop", "sha"), "warn", "warn must never downgrade to none");
    // Re-emitting warn is a no-op.
    db.mark_ci_inflight_alert_level("nodrop", "sha", "warn").unwrap();
    assert_eq!(level("nodrop", "sha"), "warn", "re-emitting warn is a no-op");

    // none → alert may skip warn.
    db.observe_ci_in_flight("skip", "sha").unwrap();
    db.mark_ci_inflight_alert_level("skip", "sha", "alert").unwrap();
    assert_eq!(
        level("skip", "sha"),
        "alert",
        "none must be allowed to jump straight to alert"
    );
}

/// `clear_ci_inflight_observations` removes every row for the target work item
/// (and only that work item), so a later `observe` starts fresh with a new
/// `first_observed_at`; a sibling work item's rows are untouched.
#[test]
fn clear_ci_inflight_observations_scoped_to_work_item() {
    let db = WorkDb::open(temp_db_path("clear-scoped")).unwrap();

    // Seed two shas under the target work item, plus one under a sibling.
    db.observe_ci_in_flight("target", "sha_1").unwrap();
    db.observe_ci_in_flight("target", "sha_2").unwrap();
    db.observe_ci_in_flight("bystander", "sha_1").unwrap();

    // Backdate to sentinels so a fresh stamp is distinguishable.
    backdate_first_observed_at(&db, "target", "sha_1", "1000");
    backdate_first_observed_at(&db, "target", "sha_2", "1001");
    backdate_first_observed_at(&db, "bystander", "sha_1", "2000");

    // Also escalate the bystander so we can confirm its full row survives.
    db.mark_ci_inflight_alert_level("bystander", "sha_1", "warn").unwrap();

    db.clear_ci_inflight_observations("target").unwrap();

    // The target's rows are gone: re-observing starts fresh (no sentinel, level
    // back to 'none').
    let reobserved_1 = db.observe_ci_in_flight("target", "sha_1").unwrap();
    assert_ne!(
        reobserved_1.first_observed_at, "1000",
        "clearing must drop the row so a later observe re-stamps first_observed_at"
    );
    assert_eq!(reobserved_1.alert_level_emitted, "none");
    let reobserved_2 = db.observe_ci_in_flight("target", "sha_2").unwrap();
    assert_ne!(
        reobserved_2.first_observed_at, "1001",
        "clear must remove every head sha for the work item, not just one"
    );

    // The bystander work item is entirely untouched — same timestamp and level.
    let bystander = db.observe_ci_in_flight("bystander", "sha_1").unwrap();
    assert_eq!(
        bystander.first_observed_at, "2000",
        "clearing one work item must not touch another's observations"
    );
    assert_eq!(
        bystander.alert_level_emitted, "warn",
        "the sibling's escalated alert level must survive an unrelated clear"
    );
}
