use super::*;

fn ev_from_json(s: &str) -> DispatchEvent {
    serde_json::from_str(s).expect("fixture DispatchEvent")
}

fn scope(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn sig1_benign_chain_hold_does_not_flag_after_progress() {
    let events = vec![
        ev_from_json(
            r#"{"ts_epoch_ms":1784923279414,"stage":"worker_claimed","outcome":"skipped","execution_id":"exec_18c5513362ac2ed8_148","work_item_id":"task_18c5513362a6cbf0_147","details":{"reason":"chain_serialized_review_held"}}"#,
        ),
        ev_from_json(
            r#"{"ts_epoch_ms":1784923334223,"stage":"stage_stalled","outcome":"ok","execution_id":"exec_18c5513362ac2ed8_148","details":{"stalled_stage":"worker_claimed","stalled_at_ts_epoch_ms":1784923279414,"elapsed_in_stage_ms":42874}}"#,
        ),
        ev_from_json(
            r#"{"ts_epoch_ms":1784923379833,"stage":"request_recorded","outcome":"ok","execution_id":"exec_18c5513362ac2ed8_148","details":{}}"#,
        ),
    ];
    // now far in the future — last real is request_recorded, not worker_claimed.
    let findings = match_dispatch_signatures(
        &events,
        1784923279414 + 400_000,
        None,
        None,
        &scope(&["exec_18c5513362ac2ed8_148"]),
    );
    assert!(
        findings.iter().all(|f| f.sig_id != "SIG-1"),
        "progress cleared stall: {findings:?}"
    );
}

#[test]
fn sig1_durable_stall_recomputes_elapsed_not_frozen_field() {
    let events = vec![
        ev_from_json(
            r#"{"ts_epoch_ms":1784923279414,"stage":"worker_claimed","outcome":"skipped","execution_id":"exec_sig1_durable","work_item_id":"task_sig1_durable","details":{"reason":"chain_serialized"}}"#,
        ),
        ev_from_json(
            r#"{"ts_epoch_ms":1784923322288,"stage":"stage_stalled","outcome":"ok","execution_id":"exec_sig1_durable","details":{"stalled_stage":"worker_claimed","stalled_at_ts_epoch_ms":1784923279414,"elapsed_in_stage_ms":42874}}"#,
        ),
    ];
    let now = 1784923279414 + 400_000;
    let findings = match_dispatch_signatures(&events, now, None, None, &scope(&["exec_sig1_durable"]));
    let sig1: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-1").collect();
    assert_eq!(sig1.len(), 1, "{findings:?}");
    assert_eq!(sig1[0].severity, Severity::P0);
    assert_eq!(sig1[0].details["elapsed_ms"], 400_000);
    // Frozen field must not be the gate — elapsed is recomputed.
    assert_ne!(sig1[0].details["elapsed_ms"], 42874);
}

#[test]
fn sig4_matches_shell_pid_zero_on_ack_timeout_only() {
    let hit = ev_from_json(
        r#"{"ts_epoch_ms":1784895019484,"stage":"spawn_ack_timeout","outcome":"ok","execution_id":"exec_18c5387f08addde0_211","details":{"slot_id":25,"shell_pid":0,"threshold_secs":60}}"#,
    );
    // Bare provisional pid 0 on successful spawn must NOT match.
    let benign = ev_from_json(
        r#"{"ts_epoch_ms":1784895019000,"stage":"pane_spawned","outcome":"ok","execution_id":"exec_18c5387f08addde0_211","details":{"shell_pid":0}}"#,
    );
    let findings = match_dispatch_signatures(&[hit, benign], 0, None, None, &scope(&["exec_18c5387f08addde0_211"]));
    let sig4: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-4").collect();
    assert_eq!(sig4.len(), 1);
}

#[test]
fn sig_i_reaches_match_dispatch_signatures() {
    let hit = ev_from_json(
        r#"{"ts_epoch_ms":1784895019484,"stage":"spawn_failed","outcome":"error","execution_id":"exec_sig_i_wiring","error_message":"spawning worker pane for run exec_sig_i_wiring: preparing progress ingress: /p/sessions is not a real directory","details":{"spawn_failure":{"class":"progress_ingress","cause":"/p/sessions is not a real directory"}}}"#,
    );
    let findings = match_dispatch_signatures(&[hit], 0, None, None, &scope(&["exec_sig_i_wiring"]));
    let sig_i: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-I").collect();
    assert_eq!(sig_i.len(), 1, "{findings:?}");
    assert_eq!(sig_i[0].severity, Severity::P0);
}

#[test]
fn sig6_requires_reason_timeout() {
    let timeout = ev_from_json(
        r#"{"ts_epoch_ms":1784879999312,"stage":"cube_workspace_lease_failed","outcome":"error","execution_id":"exec_lease_to","error_message":"cube workspace lease timed out after 30s","details":{"attempt":1,"reason":"timeout","fallback_policy":"any_free"}}"#,
    );
    let cube_err = ev_from_json(
        r#"{"ts_epoch_ms":1784879999313,"stage":"cube_workspace_lease_failed","outcome":"error","execution_id":"exec_lease_err","error_message":"jj git fetch failed","details":{"reason":"cube_error"}}"#,
    );
    let findings = match_dispatch_signatures(
        &[timeout, cube_err],
        0,
        None,
        None,
        &scope(&["exec_lease_to", "exec_lease_err"]),
    );
    let sig6: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-6").collect();
    assert_eq!(sig6.len(), 1);
    assert_eq!(sig6[0].execution_id.as_deref(), Some("exec_lease_to"));
    assert_eq!(sig6[0].severity, Severity::P1, "single timeout is P1, not fleet");
}

#[test]
fn sig6_fleet_escalate_requires_windowed_distinct_execs() {
    let base = 1_000_000u128;
    // Three distinct execs within 10m → P0 for each in-scope timeout.
    let fleet: Vec<DispatchEvent> = (0..3)
        .map(|i| {
            ev_from_json(&format!(
                r#"{{"ts_epoch_ms":{},"stage":"cube_workspace_lease_failed","outcome":"error","execution_id":"exec_to_{}","details":{{"attempt":1,"reason":"timeout","fallback_policy":"any_free"}}}}"#,
                base + i * 60_000,
                i,
            ))
        })
        .collect();
    let findings = match_dispatch_signatures(
        &fleet,
        0,
        Some(&fleet),
        None,
        &scope(&["exec_to_0", "exec_to_1", "exec_to_2"]),
    );
    let sig6: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-6").collect();
    assert_eq!(sig6.len(), 3, "{findings:?}");
    assert!(sig6.iter().all(|f| f.severity == Severity::P0), "{sig6:?}");

    // Same three execs spread over >10m → no fleet escalate (P1).
    let spread: Vec<DispatchEvent> = (0..3)
        .map(|i| {
            ev_from_json(&format!(
                r#"{{"ts_epoch_ms":{},"stage":"cube_workspace_lease_failed","outcome":"error","execution_id":"exec_spread_{}","details":{{"reason":"timeout"}}}}"#,
                base + i * (SIG6_FLEET_WINDOW_MS + 1),
                i,
            ))
        })
        .collect();
    let findings_spread = match_dispatch_signatures(
        &spread,
        0,
        Some(&spread),
        None,
        &scope(&["exec_spread_0", "exec_spread_1", "exec_spread_2"]),
    );
    let sig6_spread: Vec<_> = findings_spread.iter().filter(|f| f.sig_id == "SIG-6").collect();
    assert_eq!(sig6_spread.len(), 3);
    assert!(
        sig6_spread.iter().all(|f| f.severity == Severity::P1),
        "unwindowed distinct count must not escalate: {sig6_spread:?}"
    );
}

#[test]
fn sig_a_aggregates_untracked_heartbeats() {
    let a = ev_from_json(
        r#"{"ts_epoch_ms":1781515001787,"stage":"cube_lease_heartbeat","outcome":"error","execution_id":"exec_18b9288b7fd1c568_3","cube_lease_id":"a2a7ae36-0e16-4858-8e50-a8e54055e71a","error_message":"Cube command failed: {\n  \"error\": \"lease `a2a7ae36-0e16-4858-8e50-a8e54055e71a` is not tracked\"\n}","details":{"ttl_secs":1800}}"#,
    );
    let mut b = a.clone();
    b.ts_epoch_ms = 1781515301787;
    // Storm of 12 without SIG-2 stays P1 (count alone is not P0).
    let mut storm = Vec::new();
    for i in 0..12u128 {
        let mut e = a.clone();
        e.ts_epoch_ms = a.ts_epoch_ms + i * 300_000;
        storm.push(e);
    }
    let findings = match_dispatch_signatures(&storm, 0, None, None, &scope(&["exec_18b9288b7fd1c568_3"]));
    let siga: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-A").collect();
    assert_eq!(siga.len(), 1);
    assert_eq!(siga[0].count, 12);
    assert_eq!(siga[0].severity, Severity::P1, "no SIG-2 co-occurrence → P1: {siga:?}");
    // Two-row aggregate still works.
    let findings2 = match_dispatch_signatures(&[a, b], 0, None, None, &scope(&["exec_18b9288b7fd1c568_3"]));
    let siga2: Vec<_> = findings2.iter().filter(|f| f.sig_id == "SIG-A").collect();
    assert_eq!(siga2.len(), 1);
    assert_eq!(siga2[0].count, 2);
}

#[test]
fn sig_a_p0_only_with_sig2_cooccurrence() {
    let live = "exec_zombie_with_heartbeats";
    let base = 1_000_000u128;
    let mut fleet = Vec::new();
    for i in 0..12u128 {
        fleet.push(ev_from_json(&format!(
            r#"{{"ts_epoch_ms":{},"stage":"host_selected","outcome":"error","execution_id":"exec_blocked_{}","details":{{"reason":"redundant_spawn","live_execution_id":"{live}"}}}}"#,
            base + i * 400_000,
            i,
        )));
    }
    // Heartbeats on the zombie target (SIG-A).
    for i in 0..3u128 {
        fleet.push(ev_from_json(&format!(
            r#"{{"ts_epoch_ms":{},"stage":"cube_lease_heartbeat","outcome":"error","execution_id":"{live}","cube_lease_id":"lease-z","error_message":"lease `lease-z` is not tracked","details":{{"ttl_secs":1800}}}}"#,
            base + i * 300_000,
        )));
    }
    let findings = match_dispatch_signatures(&fleet, base + 5_000_000, Some(&fleet), None, &scope(&[live]));
    let siga: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-A").collect();
    assert_eq!(siga.len(), 1, "{findings:?}");
    assert_eq!(siga[0].severity, Severity::P0, "SIG-2 zombie + SIG-A → P0: {siga:?}");
    assert_eq!(siga[0].details["co_occurring_sig2"], true);
}

#[test]
fn sig2_zombie_recurrence_heuristic() {
    let live = "exec_zombie_target";
    let mut fleet = Vec::new();
    let base = 1_000_000u128;
    for i in 0..12u128 {
        fleet.push(ev_from_json(&format!(
            r#"{{"ts_epoch_ms":{},"stage":"host_selected","outcome":"error","execution_id":"exec_blocked_{}","details":{{"reason":"redundant_spawn","live_execution_id":"{live}"}}}}"#,
            base + i * 400_000, // ~1.2h span across 12 hits
            i,
        )));
    }
    let findings = match_dispatch_signatures(&fleet, base + 5_000_000, Some(&fleet), None, &scope(&[live]));
    let sig2: Vec<_> = findings
        .iter()
        .filter(|f| f.sig_id == "SIG-2" && f.details.get("kind").and_then(|v| v.as_str()) == Some("zombie"))
        .collect();
    assert_eq!(sig2.len(), 1, "{findings:?}");
    assert_eq!(sig2[0].severity, Severity::P0);
    assert!(sig2[0].count >= 10);
}

#[test]
fn sig_b_slot_busy() {
    let event = ev_from_json(
        r#"{"ts_epoch_ms":1784927838618,"stage":"pane_spawned","outcome":"error","execution_id":"exec_slot","error_message":"SlotBusy","details":{"slot_busy":{"slot_id":26,"occupying_run_id":"exec_other"}}}"#,
    );
    let findings = match_dispatch_signatures(&[event], 0, None, None, &scope(&["exec_slot"]));
    assert!(findings.iter().any(|f| f.sig_id == "SIG-B"));
}

/// The wedge as it actually presented: the operator diagnoses the
/// redispatch that keeps failing `SlotBusy`, while the breaker events
/// that explain it are attributed to entirely different (held-back)
/// execution ids. Matching only in scope is what produced "signatures:
/// no matches" for an hour. Held-back slots here are confirmed terminal
/// in state.db, so they are exactly the case where naming a retire-pane
/// command is safe.
#[test]
fn sig_h_husk_breaker_wedge_is_found_from_the_blocked_redispatch() {
    let breaker_ts = 1784927840000u128;
    let now = breaker_ts + 60_000; // 1 minute later — well inside the window
    let blocked = ev_from_json(
        r#"{"ts_epoch_ms":1784927838618,"stage":"pane_spawned","outcome":"error","execution_id":"exec_resume","error_message":"SlotBusy","details":{"slot_busy":{"slot_id":7,"occupying_run_id":"exec_husk_7"}}}"#,
    );
    let mut fleet = vec![blocked.clone()];
    for slot in 4..9u64 {
        fleet.push(ev_from_json(&format!(
            r#"{{"ts_epoch_ms":{breaker_ts},"stage":"husk_pane_reconcile","outcome":"skipped","execution_id":"exec_husk_{slot}","details":{{"slot_id":{slot},"skipped_reason":"mass_retirement_circuit_breaker","max_per_pass":3,"escalated":true}}}}"#,
        )));
    }
    let mut db_facts = ExecDbFacts::default();
    for slot in 4..9u64 {
        db_facts.terminal_by_exec.insert(format!("exec_husk_{slot}"), true);
    }

    let findings = match_dispatch_signatures(&[blocked], now, Some(&fleet), Some(&db_facts), &scope(&["exec_resume"]));
    let sig_h: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-H").collect();
    assert_eq!(sig_h.len(), 1, "{findings:?}");
    assert_eq!(
        sig_h[0].severity,
        Severity::P0,
        "the diagnosed execution is itself being rejected SlotBusy",
    );
    assert_eq!(sig_h[0].count, 5);
    assert_eq!(
        sig_h[0].details["slots"],
        serde_json::json!([4, 5, 6, 7, 8]),
        "every held-back slot must be named",
    );
    assert_eq!(
        sig_h[0].details["slots_confirmed_dead"],
        serde_json::json!([4, 5, 6, 7, 8])
    );
    assert!(sig_h[0].details["slots_confirmed_live"].as_array().unwrap().is_empty());
    assert!(
        sig_h[0].details["slots_unknown_liveness"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(sig_h[0].recovery.contains("bossctl agents retire-pane 7"));
}

/// A trip that could not file its attention item is worse, not better,
/// and the diagnosis must say so rather than implying someone was told.
#[test]
fn sig_h_reports_when_the_breaker_could_not_escalate() {
    let breaker_ts = 1784927840000u128;
    let event = ev_from_json(&format!(
        r#"{{"ts_epoch_ms":{breaker_ts},"stage":"husk_pane_reconcile","outcome":"skipped","execution_id":"exec_husk_1","details":{{"slot_id":1,"skipped_reason":"mass_retirement_circuit_breaker","max_per_pass":3,"escalated":false}}}}"#,
    ));
    let fleet = std::slice::from_ref(&event);
    let findings = match_dispatch_signatures(fleet, breaker_ts + 60_000, Some(fleet), None, &scope(&["exec_husk_1"]));
    let sig_h: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-H").collect();
    assert_eq!(sig_h.len(), 1);
    assert_eq!(sig_h[0].severity, Severity::P1, "no SlotBusy in scope yet");
    assert_eq!(sig_h[0].details["escalated"], false);
    assert!(sig_h[0].recovery.contains("NO attention item was filed"));
}

/// A husk sweep that retires normally must not look like a wedge.
#[test]
fn sig_h_ignores_ordinary_husk_retirements() {
    let event = ev_from_json(
        r#"{"ts_epoch_ms":1784927840000,"stage":"husk_pane_reconcile","outcome":"ok","execution_id":"exec_husk_2","details":{"slot_id":2,"death_evidence":"db_confirmed_terminal"}}"#,
    );
    let fleet = std::slice::from_ref(&event);
    let findings = match_dispatch_signatures(fleet, 1784927840000, Some(fleet), None, &scope(&["exec_husk_2"]));
    assert!(!findings.iter().any(|f| f.sig_id == "SIG-H"), "{findings:?}");
}

/// A circuit-breaker trip from days ago that has not recurred since must
/// not be reported as a live wedge — this is the exact defect that
/// misdirected an incident: `dispatch diagnose` reported SIG-H as a live
/// P1 on the basis of evidence nine days stale, on every invocation.
#[test]
fn sig_h_does_not_fire_on_evidence_outside_the_recency_window() {
    let breaker_ts = 1784927840000u128;
    let now = breaker_ts + SIG_H_RECENCY_WINDOW_MS + 1;
    let event = ev_from_json(&format!(
        r#"{{"ts_epoch_ms":{breaker_ts},"stage":"husk_pane_reconcile","outcome":"skipped","execution_id":"exec_husk_1","details":{{"slot_id":1,"skipped_reason":"mass_retirement_circuit_breaker","max_per_pass":3,"escalated":true}}}}"#,
    ));
    let fleet = std::slice::from_ref(&event);
    let findings = match_dispatch_signatures(fleet, now, Some(fleet), None, &scope(&["exec_husk_1"]));
    assert!(
        !findings.iter().any(|f| f.sig_id == "SIG-H"),
        "stale evidence outside the recency window must not fire: {findings:?}"
    );
}

/// Companion to the above: the identical event just inside the window
/// still fires, and mixed old/recent evidence reports only the recent
/// count — never the lifetime tally.
#[test]
fn sig_h_fires_on_evidence_inside_the_recency_window_and_reports_recent_count_only() {
    let stale_ts = 1784000000000u128; // days before `recent_ts`, outside the window
    let recent_ts = 1784927840000u128;
    let now = recent_ts + SIG_H_RECENCY_WINDOW_MS - 1;
    let stale = ev_from_json(&format!(
        r#"{{"ts_epoch_ms":{stale_ts},"stage":"husk_pane_reconcile","outcome":"skipped","execution_id":"exec_husk_9","details":{{"slot_id":9,"skipped_reason":"mass_retirement_circuit_breaker","max_per_pass":3,"escalated":true}}}}"#,
    ));
    let recent = ev_from_json(&format!(
        r#"{{"ts_epoch_ms":{recent_ts},"stage":"husk_pane_reconcile","outcome":"skipped","execution_id":"exec_husk_1","details":{{"slot_id":1,"skipped_reason":"mass_retirement_circuit_breaker","max_per_pass":3,"escalated":true}}}}"#,
    ));
    let fleet = vec![stale, recent];
    let findings = match_dispatch_signatures(&fleet, now, Some(&fleet), None, &scope(&["exec_husk_1"]));
    let sig_h: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-H").collect();
    assert_eq!(sig_h.len(), 1, "{findings:?}");
    assert_eq!(
        sig_h[0].count, 1,
        "only the in-window event counts, not the lifetime tally"
    );
    assert_eq!(
        sig_h[0].details["slots"],
        serde_json::json!([1]),
        "the stale slot must not be named"
    );
    assert_eq!(sig_h[0].details["held_back_events_lifetime"], 2);
}

/// The recovery text is the exact thing an operator can copy-paste into
/// a shell. A slot whose occupying execution is still LIVE in state.db
/// must never appear in a `retire-pane` command — following stale advice
/// here means killing real work in flight.
#[test]
fn sig_h_never_names_retire_pane_for_a_slot_whose_execution_is_still_live() {
    let breaker_ts = 1784927840000u128;
    let now = breaker_ts + 60_000;
    let event = ev_from_json(&format!(
        r#"{{"ts_epoch_ms":{breaker_ts},"stage":"husk_pane_reconcile","outcome":"skipped","execution_id":"exec_husk_live","details":{{"slot_id":26,"skipped_reason":"mass_retirement_circuit_breaker","max_per_pass":3,"escalated":true}}}}"#,
    ));
    let fleet = std::slice::from_ref(&event);
    let mut db_facts = ExecDbFacts::default();
    db_facts.terminal_by_exec.insert("exec_husk_live".into(), false);

    let findings = match_dispatch_signatures(fleet, now, Some(fleet), Some(&db_facts), &scope(&["exec_husk_live"]));
    let sig_h: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-H").collect();
    assert_eq!(sig_h.len(), 1);
    assert!(
        !sig_h[0].recovery.contains("retire-pane 26"),
        "must never print a retire command for a live slot: {}",
        sig_h[0].recovery
    );
    assert_eq!(sig_h[0].details["slots_confirmed_live"], serde_json::json!([26]));
    assert!(sig_h[0].details["slots_confirmed_dead"].as_array().unwrap().is_empty());
    assert!(
        sig_h[0]
            .recovery
            .contains("Slot(s) [26] have a state.db row that says the run is LIVE")
    );
}

/// With no state.db available at all, liveness of every held-back slot
/// is unknown — the safe default is to withhold every retire command,
/// not assume dead-until-proven-otherwise.
#[test]
fn sig_h_withholds_retire_pane_when_state_db_is_unavailable() {
    let breaker_ts = 1784927840000u128;
    let now = breaker_ts + 60_000;
    let event = ev_from_json(&format!(
        r#"{{"ts_epoch_ms":{breaker_ts},"stage":"husk_pane_reconcile","outcome":"skipped","execution_id":"exec_husk_unknown","details":{{"slot_id":3,"skipped_reason":"mass_retirement_circuit_breaker","max_per_pass":3,"escalated":true}}}}"#,
    ));
    let fleet = std::slice::from_ref(&event);
    let findings = match_dispatch_signatures(fleet, now, Some(fleet), None, &scope(&["exec_husk_unknown"]));
    let sig_h: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-H").collect();
    assert_eq!(sig_h.len(), 1);
    assert!(!sig_h[0].recovery.contains("retire-pane"), "{}", sig_h[0].recovery);
    assert_eq!(sig_h[0].details["slots_unknown_liveness"], serde_json::json!([3]));
    assert!(sig_h[0].recovery.contains("state.db was unavailable to this scan"));
}

#[test]
fn sig_d_wire_reasons_only() {
    let indet = ev_from_json(
        r#"{"ts_epoch_ms":1784900000000,"stage":"transient_recovery_exhausted","outcome":"error","execution_id":"exec_sigd_indet","details":{"reason":"retries_exhausted","class":"indeterminate","prior_attempts":3,"max_attempts":3}}"#,
    );
    let bad = ev_from_json(
        r#"{"ts_epoch_ms":1784900000500,"stage":"transient_recovery_exhausted","outcome":"error","execution_id":"exec_sigd_bad","details":{"reason":"unrecognized_error","class":"indeterminate"}}"#,
    );
    let findings = match_dispatch_signatures(
        &[indet, bad],
        0,
        None,
        None,
        &scope(&["exec_sigd_indet", "exec_sigd_bad"]),
    );
    let sigd: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-D").collect();
    assert_eq!(sigd.len(), 1);
    assert_eq!(sigd[0].execution_id.as_deref(), Some("exec_sigd_indet"));
}

#[test]
fn sig_e_and_f() {
    let nack = ev_from_json(
        r#"{"ts_epoch_ms":1784890000000,"stage":"spawn_nack","outcome":"ok","execution_id":"exec_sig_e","details":{"reason":"libghostty surface creation failed"}}"#,
    );
    // Same failure class, different stage: the app-reported pane death
    // that lost no shell (it never had one) is reaped as
    // `pane_death_before_start` and must diagnose as SIG-E too.
    let before_start = ev_from_json(
        r#"{"ts_epoch_ms":1784890001000,"stage":"pane_death_before_start","outcome":"ok","execution_id":"exec_sig_e_pane","details":{"reason":"pane-death-before-start: the app reported that the pane's child process exited"}}"#,
    );
    let parse = ev_from_json(
        r#"{"ts_epoch_ms":1783000000000,"stage":"pane_spawned","outcome":"error","execution_id":"exec_sig_f","worker_id":"auto-worker-2","error_message":"PaneSpawnRunner received worker_id \"auto-worker-2\" that does not parse as worker-{N}","details":{"slot_id":2}}"#,
    );
    let findings = match_dispatch_signatures(
        &[nack, before_start, parse],
        0,
        None,
        None,
        &scope(&["exec_sig_e", "exec_sig_e_pane", "exec_sig_f"]),
    );
    let sig_e: Vec<_> = findings
        .iter()
        .filter(|f| f.sig_id == "SIG-E")
        .filter_map(|f| f.execution_id.as_deref())
        .collect();
    assert_eq!(sig_e, vec!["exec_sig_e", "exec_sig_e_pane"], "{findings:?}");
    assert!(findings.iter().any(|f| f.sig_id == "SIG-F"));
}

#[test]
fn sig5_and_5c_from_trace_json() {
    let rebounce: Value = serde_json::from_str(
        r#"{"timestamp":"2026-07-24T21:45:44.118022Z","level":"INFO","fields":{"message":"ci_watch: queue-side failure detected; parent flipped to blocked: ci_failure","work_item_id":"task_18c5549a33ae7248_29c","pr_url":"https://github.com/spinyfin/mono/pull/2298","discriminator":"8d075ec06ac101f204a04da9f3f86b65a88d705f","head_sha_at_trigger":"8d075ec06ac101f204a04da9f3f86b65a88d705f","failure_kind":"merge_queue_rebounce"},"target":"boss_engine::ci_watch"}"#,
    )
    .unwrap();
    let trunk: Value = serde_json::from_str(
        r#"{"timestamp":"2026-07-26T00:00:00.000000Z","level":"INFO","fields":{"message":"ci_watch: queue-side failure detected; parent flipped to blocked: ci_failure","work_item_id":"task_trunk_example","pr_url":"https://github.com/spinyfin/mono/pull/9999","discriminator":"trunk:entry-abc@2026-07-26T00:00:00Z","head_sha_at_trigger":"trunk:entry-abc@2026-07-26T00:00:00Z","failure_kind":"trunk_queue_eviction"},"target":"boss_engine::ci_watch"}"#,
    )
    .unwrap();
    // before_commit_sha in an embedded worker payload must not match.
    let noise: Value = serde_json::from_str(
        r#"{"timestamp":"2026-07-24T00:00:00Z","level":"INFO","fields":{"message":"PostToolUse payload mentioning before_commit_sha"},"target":"boss_engine::hooks"}"#,
    )
    .unwrap();
    let items = scope(&["task_18c5549a33ae7248_29c", "task_trunk_example"]);
    let findings = match_trace_signatures(&[rebounce, trunk, noise], &BTreeSet::new(), &items);
    assert!(findings.iter().any(|f| f.sig_id == "SIG-5"));
    assert!(findings.iter().any(|f| f.sig_id == "SIG-5c"));
    assert!(!findings.iter().any(|f| f.title.contains("before_commit")));
}

#[test]
fn sig4b_and_sig_g_from_trace() {
    let hook: Value = serde_json::from_str(
        r#"{"timestamp":"2026-07-24T12:00:00.000000Z","level":"WARN","fields":{"message":"[engine-reconcile] live hook event arrived for a TERMINAL execution — the engine believes this run is dead but its worker is still emitting hooks.","run_id":"exec_18c5387f08addde0_211","kind":"session_end","status":"orphaned","work_item_id":"task_example"},"target":"boss_engine::app::worker_events"}"#,
    )
    .unwrap();
    let wake: Value = serde_json::from_str(
        r#"{"timestamp":"2026-07-24T18:00:00.000000Z","level":"WARN","fields":{"message":"scheduler heartbeat: ready execution(s) older than the heartbeat interval found — kick/drain handoff may have dropped a wakeup; re-kicking now","count":2,"oldest_age_ms":45000,"execution_ids":["exec_stranded_1","exec_stranded_2"]},"target":"boss_engine::coordinator::scheduler"}"#,
    )
    .unwrap();
    let execs = scope(&["exec_18c5387f08addde0_211", "exec_stranded_1"]);
    let findings = match_trace_signatures(&[hook, wake], &execs, &BTreeSet::new());
    assert!(findings.iter().any(|f| f.sig_id == "SIG-4b"));
    assert!(findings.iter().any(|f| f.sig_id == "SIG-G"));
}

#[test]
fn sig_c_historical_only() {
    let event = ev_from_json(
        r#"{"ts_epoch_ms":1784000000000,"stage":"status_transitioned","outcome":"error","execution_id":"exec_hist_autobind","work_item_id":"task_deleted_example","error_message":"cannot complete a deleted task: task_deleted_example","details":{"source":"auto_bind_poller"}}"#,
    );
    let findings = match_dispatch_signatures(&[event], 0, None, None, &scope(&["exec_hist_autobind"]));
    let sigc: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-C").collect();
    assert_eq!(sigc.len(), 1);
    assert_eq!(sigc[0].severity, Severity::Info);
}

#[test]
fn load_scope_events_reconstructs_partial_missing_mirrors() {
    use std::io::Write;
    let root = std::env::temp_dir().join(format!("bossctl-doctor-partial-mirrors-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let de = root.join("dispatch-events");
    std::fs::create_dir_all(&de).unwrap();
    // Mirrors live at `<root>/executions/<id>/dispatch.jsonl`, NOT under
    // `dispatch-events/` — see `dispatch_reader::execution_path`. This
    // fixture used to write them under `dispatch-events/executions/`, where
    // the reader never looks, so BOTH executions were reconstructed from the
    // flat stream and the "partial" case the test names was never exercised.
    let execs = root.join("executions");
    std::fs::create_dir_all(&execs).unwrap();

    let present = "exec_present";
    let missing = "exec_missing_mirror";
    // Mirror only for present.
    let mirror_dir = execs.join(present);
    std::fs::create_dir_all(&mirror_dir).unwrap();
    let line_present = format!(
        r#"{{"ts_epoch_ms":100,"stage":"request_recorded","outcome":"ok","execution_id":"{present}","details":{{}}}}"#
    );
    let line_missing = format!(
        r#"{{"ts_epoch_ms":200,"stage":"request_recorded","outcome":"ok","execution_id":"{missing}","details":{{}}}}"#
    );
    std::fs::write(mirror_dir.join("dispatch.jsonl"), format!("{line_present}\n")).unwrap();
    // current.jsonl has both.
    let mut current = std::fs::File::create(de.join("current.jsonl")).unwrap();
    writeln!(current, "{line_present}").unwrap();
    writeln!(current, "{line_missing}").unwrap();

    let scope = DiagnoseScope {
        input_id: "task_partial".into(),
        resolved_as: ResolvedAs::WorkItem,
        execution_ids: vec![present.into(), missing.into()],
        work_item_id: Some("task_partial".into()),
    };
    let loaded = load_scope_events(&root, &scope).unwrap();
    let _ = std::fs::remove_dir_all(&root);
    let ids: BTreeSet<_> = loaded.events.iter().map(|e| e.execution_id.as_str()).collect();
    assert!(ids.contains(present), "{ids:?}");
    assert!(
        ids.contains(missing),
        "partial missing mirror must be reconstructed from current.jsonl: {ids:?}"
    );
    assert_eq!(loaded.events.len(), 2);
    assert!(
        loaded.reconstructed_from_flat.contains(missing),
        "the reconstructed execution must be recorded, so flat-stream damage is \
         attributed to its timeline"
    );
    assert!(
        !loaded.reconstructed_from_flat.contains(present),
        "an execution with its own mirror must not inherit fleet-stream damage"
    );
}

#[test]
fn diagnose_json_single_exec_keeps_v1_compatibility_fields() {
    // build_timeline_json is the events payload; run_diagnose wiring is
    // covered structurally here via the same shape it emits.
    let events = vec![ev_from_json(
        r#"{"ts_epoch_ms":1,"stage":"request_recorded","outcome":"ok","execution_id":"exec_compat","details":{}}"#,
    )];
    let durations = vec![0u128];
    let timeline = build_timeline_json("exec_compat", &events, &durations);
    let events_value = timeline.get("events").cloned().unwrap_or(Value::Array(Vec::new()));
    let payload = serde_json::json!({
        "schema_version": DIAGNOSE_JSON_SCHEMA_VERSION,
        "id": "exec_compat",
        "resolved_as": ResolvedAs::Execution,
        "work_item_id": null,
        "execution_ids": ["exec_compat"],
        "findings": [],
        "timelines": { "exec_compat": timeline },
        // v1 compatibility fields restored on single-exec path
        "execution_id": "exec_compat",
        "events": events_value,
    });
    assert_eq!(payload["schema_version"], DIAGNOSE_JSON_SCHEMA_VERSION);
    assert_eq!(payload["execution_id"], "exec_compat");
    assert_eq!(payload["events"].as_array().unwrap().len(), 1);
    assert!(payload["findings"].as_array().unwrap().is_empty());
    assert!(payload.get("timelines").is_some());
}

// ── Stream-damage surfacing ────────────────────────────────────────────

/// Fixture root with one execution mirror whose lines reproduce the two
/// corruption shapes: a concatenated pair (the writer's interleave, fully
/// recoverable) and a truncated record (genuinely lost). Returns the root;
/// the caller removes it.
fn damaged_mirror_root(name: &str, exec_id: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("bossctl-doctor-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mirror_dir = root.join("executions").join(exec_id);
    std::fs::create_dir_all(&mirror_dir).unwrap();
    let record = |ts: u128, stage: &str| {
        format!(
            r#"{{"ts_epoch_ms":{ts},"stage":"{stage}","outcome":"ok","execution_id":"{exec_id}","work_item_id":"task_damaged","details":{{}}}}"#
        )
    };
    let contents = format!(
        "{}\n{}{}\n{{\"ts_epoch_ms\":400,\"stage\":\"cube_workspace_lea\n{}\n",
        record(100, "status_transition"),
        // Two whole records on one line — bodyA+bodyB+\n+\n on disk.
        record(200, "request_recorded"),
        record(300, "host_selected"),
        // A truncated record: no resync can recover this one.
        record(500, "worker_claimed"),
    );
    std::fs::write(mirror_dir.join("dispatch.jsonl"), contents).unwrap();
    root
}

fn exec_scope(exec_id: &str) -> DiagnoseScope {
    DiagnoseScope {
        input_id: exec_id.to_owned(),
        resolved_as: ResolvedAs::Execution,
        execution_ids: vec![exec_id.to_owned()],
        work_item_id: Some("task_damaged".to_owned()),
    }
}

/// End to end over a stream containing BOTH corruption shapes: the
/// concatenated records must be recovered into the timeline (not silently
/// dropped as they were before), and the truncated one must be reported as
/// a loss rather than vanishing.
#[test]
fn a_damaged_mirror_yields_recovered_events_and_reported_losses() {
    let exec_id = "exec_damaged_mirror";
    let root = damaged_mirror_root("damage-e2e", exec_id);
    let loaded = load_scope_events(&root, &exec_scope(exec_id)).unwrap();
    let _ = std::fs::remove_dir_all(&root);

    let stages: Vec<&str> = loaded.events.iter().map(|e| e.stage.as_str()).collect();
    assert_eq!(
        stages,
        vec![
            "status_transition",
            "request_recorded",
            "host_selected",
            "worker_claimed"
        ],
        "both concatenated records must land in the timeline, in order"
    );

    let damage = loaded.damage_for(exec_id);
    assert_eq!(damage.len(), 2, "both damaged lines must be reported: {damage:?}");
    assert_eq!(damage[0].shape, boss_engine::dispatch_reader::DamageShape::Concatenated);
    assert_eq!(damage[0].recovered, 2);
    assert_eq!(
        damage[1].shape,
        boss_engine::dispatch_reader::DamageShape::Unrecoverable
    );
}

/// The timeline itself must carry the marker — the whole point is that a
/// reader of the timeline cannot see it as complete. Asserted against the
/// rendered text, at the right position, not merely against a count.
#[test]
fn the_rendered_timeline_carries_a_marker_where_records_were_unreadable() {
    let exec_id = "exec_marked_timeline";
    let root = damaged_mirror_root("damage-render", exec_id);
    let loaded = load_scope_events(&root, &exec_scope(exec_id)).unwrap();
    let _ = std::fs::remove_dir_all(&root);

    let rendered = render_timeline(&loaded.events, &loaded.damage_for(exec_id), 10_000);
    let lines: Vec<&str> = rendered.lines().collect();
    let marker_positions: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains("UNREADABLE"))
        .map(|(idx, _)| idx)
        .collect();
    assert_eq!(marker_positions.len(), 2, "rendered timeline:\n{rendered}");
    assert!(
        rendered.contains("unrecoverable"),
        "the lost record must be named as lost, not merely flagged:\n{rendered}"
    );

    // The concatenated line sat between the ts=100 and ts=200 records, so
    // its marker must land after the first stage line and before the last.
    let first_stage = lines
        .iter()
        .position(|line| line.contains("status_transition"))
        .expect("timeline renders the first stage");
    let last_stage = lines
        .iter()
        .position(|line| line.contains("worker_claimed"))
        .expect("timeline renders the last stage");
    assert!(
        marker_positions[0] > first_stage && marker_positions[0] < last_stage,
        "the marker must sit inside the timeline at the damaged position, not be appended:\n{rendered}"
    );
}

/// Load the damaged-mirror fixture for `exec_id`, returning both the
/// per-execution attribution (`ScopeEvents`) and the pooled report built from
/// it. Absence qualification needs both: the pooled view for the fleet-level
/// fallback, the attribution for everything else.
fn damaged_scope(exec_id: &str, name: &str) -> (ScopeEvents, crate::stream_integrity::IntegrityReport) {
    let root = damaged_mirror_root(name, exec_id);
    let loaded = load_scope_events(&root, &exec_scope(exec_id)).unwrap();
    let _ = std::fs::remove_dir_all(&root);
    let integrity = crate::stream_integrity::IntegrityReport::new(loaded.all_damage());
    (loaded, integrity)
}

fn damaged_integrity(exec_id: &str, name: &str) -> crate::stream_integrity::IntegrityReport {
    damaged_scope(exec_id, name).1
}

/// One unrecoverable line, bracketed wide enough to overlap any test window.
fn lost_line(path: &str, line_number: u64) -> DamagedLine {
    DamagedLine::builder()
        .path(std::path::PathBuf::from(path))
        .line_number(line_number)
        .byte_len(400)
        .recovered(0)
        .lost_bytes(400)
        .lost_excerpt("{\"ts_epoch_ms\":1,\"stage\":\"worker_cla")
        .shape(dispatch_reader::DamageShape::Unrecoverable)
        .build()
}

/// A clean timeline with no findings prints "no matches"; a damaged one must
/// not let that read as a clean bill of health.
#[test]
fn no_matches_is_qualified_when_the_stream_was_damaged() {
    let intact = crate::stream_integrity::IntegrityReport::default();
    let clean = render_findings(&[], &intact);
    assert!(clean.contains("no matches"));
    assert!(
        !clean.contains("does NOT mean"),
        "an intact stream must not be qualified: {clean}"
    );

    let damaged = damaged_integrity("exec_no_matches", "damage-nomatch");
    let rendered = render_findings(&[], &damaged);
    assert!(rendered.contains("no matches"));
    assert!(
        rendered.contains(crate::stream_integrity::NO_MATCHES_CAVEAT),
        "\"no matches\" over a damaged stream must be qualified: {rendered}"
    );
}

/// SIG-1's conclusion is that nothing followed `worker_claimed`. With
/// unreadable records in the window, that must be marked unreliable rather
/// than asserted — and must NOT be marked when the stream is intact.
#[test]
fn an_absence_based_finding_is_qualified_only_when_damage_overlaps_it() {
    let exec_id = "exec_qualified";
    let events = vec![ev_from_json(
        r#"{"ts_epoch_ms":100,"stage":"worker_claimed","outcome":"ok","execution_id":"exec_qualified","work_item_id":"task_damaged","details":{}}"#,
    )];
    let now = 100 + SIG1_CRITICAL_MS;
    let mut findings = match_dispatch_signatures(&events, now, None, None, &scope(&[exec_id]));
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].sig_id, "SIG-1");
    assert!(findings[0].absence_based, "SIG-1 must declare itself absence-based");

    let intact = crate::stream_integrity::IntegrityReport::default();
    assert_eq!(
        qualify_absence_findings(&mut findings, &events, &ScopeEvents::default(), &intact, now),
        0,
        "an intact stream must leave the finding stated as fact"
    );
    assert!(!findings[0].evidence.iter().any(|e| e.contains("UNRELIABLE")));

    // Damage in THIS execution's own mirror.
    let (loaded, damaged) = damaged_scope(exec_id, "damage-qualify");
    assert_eq!(
        qualify_absence_findings(&mut findings, &events, &loaded, &damaged, now),
        1,
        "damage overlapping the window must qualify the conclusion"
    );
    assert!(
        findings[0]
            .evidence
            .iter()
            .any(|e| e == crate::stream_integrity::ABSENCE_CAVEAT),
        "{:?}",
        findings[0].evidence
    );
    assert_eq!(findings[0].details["absence_unreliable"], Value::Bool(true));
}

/// A SIG-1 fixture plus the `[exec, now]` window it is about.
fn sig1_finding(exec_id: &str) -> (Vec<Finding>, Vec<DispatchEvent>, u128) {
    let events = vec![ev_from_json(&format!(
        r#"{{"ts_epoch_ms":100,"stage":"worker_claimed","outcome":"ok","execution_id":"{exec_id}","work_item_id":"task_damaged","details":{{}}}}"#
    ))];
    let now = 100 + SIG1_CRITICAL_MS;
    let findings = match_dispatch_signatures(&events, now, None, None, &scope(&[exec_id]));
    assert_eq!(findings.len(), 1);
    (findings, events, now)
}

/// Attribution, not pooling. An unrecoverable line in execution B's mirror
/// says nothing about execution A's timeline — the mirrors are separate files
/// and the sink writes A's events to A's mirror. Qualifying A's finding on B's
/// damage would stamp `UNRELIABLE:` on a timeline the same command reports
/// `complete: true`, which is the over-qualification that teaches readers to
/// skip the caveat.
#[test]
fn another_executions_mirror_damage_does_not_qualify_this_finding() {
    let exec_id = "exec_attributed";
    let (mut findings, events, now) = sig1_finding(exec_id);

    let loaded = ScopeEvents {
        covered: scope(&[exec_id, "exec_other"]),
        mirror_damage: BTreeMap::from([(
            "exec_other".to_owned(),
            vec![lost_line("/state/executions/exec_other/dispatch.jsonl", 7)],
        )]),
        ..ScopeEvents::default()
    };
    let pooled = crate::stream_integrity::IntegrityReport::new(loaded.all_damage());
    assert!(pooled.lost_lines().next().is_some(), "the pooled view IS damaged");

    assert_eq!(
        qualify_absence_findings(&mut findings, &events, &loaded, &pooled, now),
        0,
        "damage in a sibling execution's mirror must not qualify this execution's finding"
    );
    assert!(!findings[0].evidence.iter().any(|e| e.contains("UNRELIABLE")));
}

/// Flat-stream damage belongs to an execution's timeline only when that
/// timeline was reconstructed from the flat stream. Otherwise the mirror is
/// the source and holds every one of that execution's events.
#[test]
fn flat_stream_damage_qualifies_only_a_reconstructed_timeline() {
    let exec_id = "exec_flat";
    let flat = vec![lost_line("/state/dispatch-events/current.jsonl", 266_198)];

    let from_mirror = ScopeEvents {
        covered: scope(&[exec_id]),
        flat_damage: flat.clone(),
        ..ScopeEvents::default()
    };
    let (mut findings, events, now) = sig1_finding(exec_id);
    let pooled = crate::stream_integrity::IntegrityReport::new(from_mirror.all_damage());
    assert_eq!(
        qualify_absence_findings(&mut findings, &events, &from_mirror, &pooled, now),
        0,
        "the mirror was intact, so a torn fleet-stream line hides nothing from this timeline"
    );

    let reconstructed = ScopeEvents {
        covered: scope(&[exec_id]),
        flat_damage: flat,
        reconstructed_from_flat: scope(&[exec_id]),
        ..ScopeEvents::default()
    };
    let (mut findings, events, now) = sig1_finding(exec_id);
    let pooled = crate::stream_integrity::IntegrityReport::new(reconstructed.all_damage());
    assert_eq!(
        qualify_absence_findings(&mut findings, &events, &reconstructed, &pooled, now),
        1,
        "when the flat stream IS the only record of this timeline, its damage is this timeline's"
    );
}

/// A finding about an execution this load never read has no per-execution
/// attribution available, so it falls back to the pooled view rather than
/// silently reading as intact.
#[test]
fn an_uncovered_execution_falls_back_to_the_pooled_view() {
    let exec_id = "exec_uncovered";
    let (mut findings, events, now) = sig1_finding(exec_id);
    let loaded = ScopeEvents {
        covered: scope(&["exec_someone_else"]),
        mirror_damage: BTreeMap::from([(
            "exec_someone_else".to_owned(),
            vec![lost_line("/state/executions/exec_someone_else/dispatch.jsonl", 3)],
        )]),
        ..ScopeEvents::default()
    };
    let pooled = crate::stream_integrity::IntegrityReport::new(loaded.all_damage());
    assert_eq!(
        qualify_absence_findings(&mut findings, &events, &loaded, &pooled, now),
        1,
        "no narrower window is available, so the conservative reading applies"
    );
}

/// A finding that rests on events that ARE present must never be qualified,
/// however damaged the stream is: over-qualifying everything would make the
/// caveat noise and teach operators to ignore it.
#[test]
fn a_presence_based_finding_is_never_qualified_by_stream_damage() {
    let events = vec![ev_from_json(
        r#"{"ts_epoch_ms":100,"stage":"spawn_ack_timeout","outcome":"ok","execution_id":"exec_presence","details":{"slot_id":3,"shell_pid":0,"threshold_secs":60}}"#,
    )];
    let mut findings = match_dispatch_signatures(&events, 1_000, None, None, &scope(&["exec_presence"]));
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].sig_id, "SIG-4");
    assert!(!findings[0].absence_based);

    let (loaded, damaged) = damaged_scope("exec_presence", "damage-presence");
    assert_eq!(
        qualify_absence_findings(&mut findings, &events, &loaded, &damaged, 1_000),
        0
    );
    assert!(!findings[0].evidence.iter().any(|e| e.contains("UNRELIABLE")));
}

/// Write an engine-trace fixture into a scratch state root and return it.
fn trace_root(name: &str, contents: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("bossctl-trace-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(boss_log_files::ENGINE_TRACE_FILENAME), contents).unwrap();
    root
}

fn trace_line(ts: &str, message: &str, exec_id: &str) -> String {
    format!(
        r#"{{"timestamp":"{ts}","level":"INFO","target":"boss_engine::coordinator","fields":{{"message":"{message}","execution_id":"{exec_id}"}}}}"#
    )
}

/// The trace scan keeps the records that touch the scope, skips the rest,
/// and tolerates an unparseable line rather than failing the whole scan.
#[test]
fn the_trace_scan_keeps_in_scope_records_and_skips_unreadable_lines() {
    let contents = format!(
        "{}\n{}\n{{\"timestamp\":\"2026-07-26T00:00:02Z\",\"level\":\"IN\n",
        trace_line("2026-07-26T00:00:00Z", "mine", "exec_trace"),
        trace_line("2026-07-26T00:00:01Z", "someone else's", "exec_other"),
    );
    let root = trace_root("scope", &contents);
    let records = load_scope_trace_records(&root, &scope(&["exec_trace"]), &BTreeSet::new()).unwrap();
    let _ = std::fs::remove_dir_all(&root);

    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["fields"]["execution_id"], Value::String("exec_trace".into()));
}
