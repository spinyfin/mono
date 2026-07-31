//! Metadata-KV persistence round-trip tests: live-status disabled slots,
//! dispatch/automation pause state (including the operator-reason
//! restore rules), and the dispatch concurrency limit. Split out of
//! `t02.rs` to keep that file under the repo's file-size cap — these
//! tests share nothing with the rest of `t02.rs` beyond the
//! `open_temp_work_db` fixture below.

use super::*;

use crate::coordinator::DispatchPauseOrigin;

// ── live-status disabled-slot persistence ────────────────────────────────

/// Open a bare `WorkDb` on a throwaway temp path — enough for the pure
/// metadata-KV helpers below, which need no product/chore fixtures. The
/// returned `TempDir` must be kept alive by the caller for the DB's
/// lifetime; dropping it deletes the backing file.
fn open_temp_work_db() -> (tempfile::TempDir, WorkDb) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = WorkDb::open(path).unwrap();
    (dir, db)
}

#[test]
fn disabled_slots_roundtrip_preserves_set() {
    let (_dir, db) = open_temp_work_db();
    persist_live_status_disabled_slots(&db, &[3, 1, 7]).unwrap();
    let mut loaded = load_live_status_disabled_slots(&db);
    loaded.sort();
    assert_eq!(
        loaded,
        vec![1, 3, 7],
        "persisting a slice of slot ids must load back as the same set"
    );
}

#[test]
fn disabled_slots_empty_roundtrips_to_empty() {
    let (_dir, db) = open_temp_work_db();
    persist_live_status_disabled_slots(&db, &[]).unwrap();
    assert!(
        load_live_status_disabled_slots(&db).is_empty(),
        "persisting an empty slice must load back as an empty Vec"
    );
}

#[test]
fn disabled_slots_absent_key_loads_empty() {
    let (_dir, db) = open_temp_work_db();
    // Never persisted — the metadata row is absent. Must degrade to an
    // empty Vec rather than erroring (first-boot behaviour).
    assert!(
        load_live_status_disabled_slots(&db).is_empty(),
        "an unset disabled-slot key must load as an empty Vec"
    );
}

#[test]
fn disabled_slots_malformed_entries_are_filtered() {
    let (_dir, db) = open_temp_work_db();
    // Write raw malformed metadata directly (bypassing the persist
    // helper, which only ever emits clean output): whitespace-padded,
    // non-numeric, empty, and out-of-u8-range entries must all be
    // dropped, leaving only the valid u8 slots.
    db.set_metadata("live_status_disabled_slots", " 3 ,x,5,,999").unwrap();
    let mut loaded = load_live_status_disabled_slots(&db);
    loaded.sort();
    assert_eq!(
        loaded,
        vec![3, 5],
        "only well-formed u8 slot ids survive: whitespace is trimmed, junk/empty/overflow dropped"
    );
}

// ── dispatch-pause state persistence ─────────────────────────────────────

#[test]
fn dispatch_paused_state_defaults_when_absent() {
    let (_dir, db) = open_temp_work_db();
    assert_eq!(
        load_dispatch_paused_state(&db),
        (false, 0, DispatchPauseOrigin::Breaker, None),
        "with no metadata keys set, the state defaults to (not-paused, since 0, breaker origin, no reason)"
    );
}

#[test]
fn dispatch_paused_state_reads_paused_since_and_reason() {
    let (_dir, db) = open_temp_work_db();
    db.set_metadata(METADATA_KEY_DISPATCH_PAUSED, "1").unwrap();
    db.set_metadata(METADATA_KEY_DISPATCH_PAUSED_SINCE, "1700000000")
        .unwrap();
    db.set_metadata(METADATA_KEY_DISPATCH_PAUSE_ORIGIN, "operator").unwrap();
    db.set_metadata(
        METADATA_KEY_DISPATCH_PAUSE_REASON,
        "investigating a spike in failed dispatch attempts",
    )
    .unwrap();
    assert_eq!(
        load_dispatch_paused_state(&db),
        (
            true,
            1_700_000_000,
            DispatchPauseOrigin::Operator,
            Some("investigating a spike in failed dispatch attempts".to_string())
        ),
        "paused flag '1' with a numeric since, a persisted origin, and a persisted reason must parse all four components"
    );
}

#[test]
fn dispatch_paused_state_since_defaults_to_zero_when_missing_or_garbage() {
    let (_dir, db) = open_temp_work_db();
    // Paused, but the since key is absent → since defaults to 0.
    db.set_metadata(METADATA_KEY_DISPATCH_PAUSED, "1").unwrap();
    let (paused, since, origin, _reason) = load_dispatch_paused_state(&db);
    assert!(paused, "since key absent must still preserve paused=true");
    assert_eq!(since, 0, "missing since key must default the timestamp to 0");
    assert_eq!(origin, DispatchPauseOrigin::Breaker);
    // Non-numeric since value is also treated as 0.
    db.set_metadata(METADATA_KEY_DISPATCH_PAUSED_SINCE, "not-a-number")
        .unwrap();
    let (_, since, _, _) = load_dispatch_paused_state(&db);
    assert_eq!(since, 0, "non-numeric since value must fall back to 0");
}

#[test]
fn dispatch_paused_state_non_one_flag_is_not_paused() {
    let (_dir, db) = open_temp_work_db();
    // Any flag value other than exactly "1" reads as not paused, but the
    // since component is parsed independently.
    db.set_metadata(METADATA_KEY_DISPATCH_PAUSED, "0").unwrap();
    db.set_metadata(METADATA_KEY_DISPATCH_PAUSED_SINCE, "1700000000")
        .unwrap();
    let (paused, since, _origin, reason) = load_dispatch_paused_state(&db);
    assert!(!paused, "flag '0' must read as not paused");
    assert_eq!(
        since, 1_700_000_000,
        "the since component still parses independently of the flag"
    );
    assert_eq!(reason, None, "reason must be None whenever not paused");
}

#[test]
fn dispatch_paused_state_missing_origin_defaults_to_breaker() {
    let (_dir, db) = open_temp_work_db();
    // A pause persisted before the origin key existed (or corrupted data)
    // must NOT be restored as review-exempt — default conservatively to the
    // non-exempt breaker origin.
    db.set_metadata(METADATA_KEY_DISPATCH_PAUSED, "1").unwrap();
    let (_, _, origin, _reason) = load_dispatch_paused_state(&db);
    assert_eq!(
        origin,
        DispatchPauseOrigin::Breaker,
        "a pause with no persisted origin must default to the non-exempt breaker origin"
    );
}

#[test]
fn dispatch_paused_state_missing_reason_falls_back_to_legacy_constant_while_paused() {
    let (_dir, db) = open_temp_work_db();
    // A pause persisted before the reason key existed must still surface a
    // non-empty reason on restore, rather than an anonymous pause — but this
    // fallback is reachable only via restored legacy state, never a live
    // pause call (both `pause_dispatch` and `pause_automation` require a
    // real `PauseReason`).
    db.set_metadata(METADATA_KEY_DISPATCH_PAUSED, "1").unwrap();
    let (_, _, _, reason) = load_dispatch_paused_state(&db);
    assert_eq!(
        reason,
        Some(crate::app::handler_helpers::LEGACY_PAUSE_REASON_FALLBACK.to_string())
    );
}

#[test]
fn dispatch_paused_state_treats_retired_fabricated_reason_as_not_recorded() {
    let (_dir, db) = open_temp_work_db();
    // `bossctl dispatch pause` used to fabricate a reason client-side when
    // the operator omitted `--reason`; that default has been removed, but a
    // pause persisted under it before the fix must not be echoed back on
    // restore as though an operator had actually authored it. Assembled
    // from word fragments (rather than one literal) so the retired phrase
    // itself does not reappear in the source tree.
    let retired_fabricated_reason = ["the", "operator", "asked", "me", "to"].join(" ");
    db.set_metadata(METADATA_KEY_DISPATCH_PAUSED, "1").unwrap();
    db.set_metadata(METADATA_KEY_DISPATCH_PAUSE_REASON, &retired_fabricated_reason)
        .unwrap();
    let (_, _, _, reason) = load_dispatch_paused_state(&db);
    assert_eq!(
        reason,
        Some(crate::app::handler_helpers::FABRICATED_PAUSE_REASON_FALLBACK.to_string()),
        "a persisted reason matching the retired client-side default must render as not-recorded, \
         never be echoed back verbatim as though an operator authored it"
    );
}

// ── automation-pause state persistence ───────────────────────────────────

#[test]
fn automation_paused_state_defaults_when_absent() {
    let (_dir, db) = open_temp_work_db();
    assert_eq!(
        load_automation_paused_state(&db),
        (false, 0, None),
        "with no metadata keys set, the state defaults to (not-paused, since 0, no reason)"
    );
}

#[test]
fn automation_paused_state_reads_paused_since_and_reason() {
    let (_dir, db) = open_temp_work_db();
    db.set_metadata(METADATA_KEY_AUTOMATION_PAUSED, "1").unwrap();
    db.set_metadata(METADATA_KEY_AUTOMATION_PAUSED_SINCE, "1700000000")
        .unwrap();
    db.set_metadata(
        METADATA_KEY_AUTOMATION_PAUSE_REASON,
        "investigating a spike in failed dispatch attempts",
    )
    .unwrap();
    assert_eq!(
        load_automation_paused_state(&db),
        (
            true,
            1_700_000_000,
            Some("investigating a spike in failed dispatch attempts".to_string())
        ),
        "paused flag '1' with a numeric since and a persisted reason must parse all three components"
    );
}

#[test]
fn automation_paused_state_since_defaults_to_zero_when_missing_or_garbage() {
    let (_dir, db) = open_temp_work_db();
    // Paused, but the since key is absent → since defaults to 0.
    db.set_metadata(METADATA_KEY_AUTOMATION_PAUSED, "1").unwrap();
    let (paused, since, _reason) = load_automation_paused_state(&db);
    assert!(paused, "since key absent must still preserve paused=true");
    assert_eq!(since, 0, "missing since key must default the timestamp to 0");
    // Non-numeric since value is also treated as 0.
    db.set_metadata(METADATA_KEY_AUTOMATION_PAUSED_SINCE, "not-a-number")
        .unwrap();
    let (_, since, _) = load_automation_paused_state(&db);
    assert_eq!(since, 0, "non-numeric since value must fall back to 0");
}

#[test]
fn automation_paused_state_non_one_flag_is_not_paused() {
    let (_dir, db) = open_temp_work_db();
    // Any flag value other than exactly "1" reads as not paused, but the
    // since component is parsed independently.
    db.set_metadata(METADATA_KEY_AUTOMATION_PAUSED, "0").unwrap();
    db.set_metadata(METADATA_KEY_AUTOMATION_PAUSED_SINCE, "1700000000")
        .unwrap();
    let (paused, since, reason) = load_automation_paused_state(&db);
    assert!(!paused, "flag '0' must read as not paused");
    assert_eq!(
        since, 1_700_000_000,
        "the since component still parses independently of the flag"
    );
    assert_eq!(reason, None, "reason must be None whenever not paused");
}

#[test]
fn automation_paused_state_treats_retired_fabricated_reason_as_not_recorded() {
    let (_dir, db) = open_temp_work_db();
    // Same retired `bossctl automation pause` client-side default as
    // `dispatch_paused_state_treats_retired_fabricated_reason_as_not_recorded`
    // — see that test's comment for why the phrase is word-assembled
    // rather than a literal.
    let retired_fabricated_reason = ["the", "operator", "asked", "me", "to"].join(" ");
    db.set_metadata(METADATA_KEY_AUTOMATION_PAUSED, "1").unwrap();
    db.set_metadata(METADATA_KEY_AUTOMATION_PAUSE_REASON, &retired_fabricated_reason)
        .unwrap();
    let (_, _, reason) = load_automation_paused_state(&db);
    assert_eq!(
        reason,
        Some(crate::app::handler_helpers::FABRICATED_PAUSE_REASON_FALLBACK.to_string())
    );
}

// ── dispatch concurrency limit persistence ───────────────────────────────

#[test]
fn dispatch_concurrency_limit_defaults_when_absent() {
    let (_dir, db) = open_temp_work_db();
    assert_eq!(
        load_dispatch_concurrency_limit(&db),
        crate::coordinator::MAX_CONCURRENT_INTERACTIVE_WORKERS,
        "with no metadata key set, the limit defaults to MAX_CONCURRENT_INTERACTIVE_WORKERS"
    );
}

#[test]
fn dispatch_concurrency_limit_reads_persisted_value() {
    let (_dir, db) = open_temp_work_db();
    db.set_metadata(METADATA_KEY_DISPATCH_CONCURRENCY_LIMIT, "12").unwrap();
    assert_eq!(
        load_dispatch_concurrency_limit(&db),
        12,
        "a well-formed persisted value must parse verbatim"
    );
}

#[test]
fn dispatch_concurrency_limit_zero_falls_back_to_default() {
    let (_dir, db) = open_temp_work_db();
    // `0` would wedge all mainline dispatch, so it is filtered out just
    // like an absent key rather than being honored verbatim.
    db.set_metadata(METADATA_KEY_DISPATCH_CONCURRENCY_LIMIT, "0").unwrap();
    assert_eq!(
        load_dispatch_concurrency_limit(&db),
        crate::coordinator::MAX_CONCURRENT_INTERACTIVE_WORKERS,
        "a persisted 0 must fall back to the default, not wedge dispatch at 0"
    );
}

#[test]
fn dispatch_concurrency_limit_garbage_falls_back_to_default() {
    let (_dir, db) = open_temp_work_db();
    db.set_metadata(METADATA_KEY_DISPATCH_CONCURRENCY_LIMIT, "not-a-number")
        .unwrap();
    assert_eq!(
        load_dispatch_concurrency_limit(&db),
        crate::coordinator::MAX_CONCURRENT_INTERACTIVE_WORKERS,
        "an unparseable persisted value must fall back to the default"
    );
}
