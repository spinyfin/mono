use super::*;

#[test]
fn review_churn_gets_a_fresh_grace_when_ci_recovers() {
    let f = fixture();
    let id = f.chore("Churn after CI recovery");
    f.stick(&id, "status = 'in_review'", 8);
    f.db.update_task_pr_poll_state(
        &id,
        PrPollStateInput {
            ci_required_state: "fail",
            pr_mergeable_state: "mergeable",
            ..Default::default()
        },
    )
    .unwrap();
    f.age(&id, 4 * HOUR);
    let attention =
        f.db.upsert_work_item_attention(&id, "churn_guard_parked", "title", "body")
            .unwrap();
    f.sql(&format!(
        "UPDATE work_attention_items SET created_at = {} WHERE id = '{attention}'",
        now() - 4 * HOUR
    ));
    f.assert_candidate(&id, true);
    f.db.update_task_pr_poll_state(
        &id,
        PrPollStateInput {
            ci_required_state: "success",
            pr_mergeable_state: "mergeable",
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(f.clauses(&id).stuck_kind.as_deref(), Some("S16_review_churn"));
    assert!((f.clauses(&id).stuck_since - now()).abs() < 5);
    f.assert_candidate(&id, false);
}

#[test]
fn resolving_an_overriding_attention_starts_the_new_kinds_grace() {
    let f = fixture();
    let id = f.chore("Recovery after abandoned branch");
    f.stick(&id, "status = 'active', autostart = 1", 8);
    for kind in ["worker_recovery_exhausted", "abandoned_branch_no_pr"] {
        let attention = f.db.upsert_work_item_attention(&id, kind, "title", "body").unwrap();
        f.sql(&format!(
            "UPDATE work_attention_items SET created_at = {} WHERE id = '{attention}'",
            now() - 4 * HOUR
        ));
    }
    f.assert_candidate(&id, true);
    crate::dispatch_helpers::resolve_attention_kind_in_tx(&f.db.connect().unwrap(), &id, "abandoned_branch_no_pr")
        .unwrap();
    assert_eq!(f.clauses(&id).stuck_kind.as_deref(), Some("S14_worker_recovery"));
    assert!((f.clauses(&id).stuck_since - now()).abs() < 5);
    f.assert_candidate(&id, false);
}

#[test]
fn attention_entry_and_reopening_never_inherit_old_task_history() {
    for (kind, status) in [
        ("worker_recovery_exhausted", "active"),
        ("abandoned_branch_no_pr", "active"),
        ("churn_guard_parked", "in_review"),
    ] {
        let f = fixture();
        let id = f.chore(kind);
        f.age(&id, 8 * HOUR);
        transition_at(&f, &id, &format!("status = '{status}', autostart = 1"), now() - HOUR);
        let attention = f.db.upsert_work_item_attention(&id, kind, "title", "body").unwrap();
        assert!((f.clauses(&id).stuck_since - now()).abs() < 5);
        f.assert_candidate(&id, false);
        f.sql(&format!(
            "UPDATE work_attention_items SET created_at = {}, last_raised_at = {} WHERE id = '{attention}'",
            now() - 3 * HOUR,
            now() - 3 * HOUR
        ));
        // The task entered the eligible status after the attention was raised.
        assert!((f.clauses(&id).stuck_since - (now() - HOUR)).abs() < 5);
        f.sql(&format!(
            "UPDATE task_status_audit SET changed_at = CAST(changed_at AS INTEGER) - {} WHERE work_item_id = '{id}'",
            3 * HOUR
        ));
        f.assert_candidate(&id, true);
        let began = f.clauses(&id).stuck_since;
        f.db.upsert_work_item_attention(&id, kind, "still open", "body")
            .unwrap();
        assert_eq!(f.clauses(&id).stuck_since, began);
        crate::dispatch_helpers::resolve_attention_kind_in_tx(&f.db.connect().unwrap(), &id, kind).unwrap();
        assert!(f.clauses(&id).stuck_kind.is_none());
        f.sql(&format!(
            "UPDATE work_attention_items SET resolved_at = {} WHERE id = '{attention}'",
            now() - HOUR
        ));
        f.db.upsert_work_item_attention(&id, kind, "reopened", "body").unwrap();
        assert!((f.clauses(&id).stuck_since - now()).abs() < 5);
        f.assert_candidate(&id, false);
    }
}

#[test]
fn observation_upgrade_skips_terminal_deleted_and_unobserved_tasks() {
    let f = fixture();
    let mut ids = Vec::new();
    for state in ["done", "archived", "deleted", "unobserved"] {
        let id = f.chore(state);
        if state != "unobserved" {
            transition_at(&f, &id, "pr_mergeable_state = 'conflicting'", now());
        }
        match state {
            "deleted" => f.sql(&format!("UPDATE tasks SET deleted_at = '{}' WHERE id = '{id}'", now())),
            "unobserved" => {}
            _ => f.sql(&format!("UPDATE tasks SET status = '{state}' WHERE id = '{id}'")),
        }
        f.sql(&format!("DELETE FROM task_status_audit WHERE work_item_id = '{id}' AND property IN ('pr_mergeable_state','ci_required_state')"));
        ids.push(id);
    }
    let conn = f.db.connect().unwrap();
    crate::task_status_audit::migrate_pr_observations(&conn).unwrap();
    for id in ids {
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM task_status_audit WHERE work_item_id = ?1 AND property IN ('pr_mergeable_state','ci_required_state')", [&id], |r| r.get(0)).unwrap();
        assert_eq!(count, 0);
    }
}

#[test]
fn legacy_conflict_baseline_is_conservative_and_idempotent() {
    let f = fixture();
    let id = f.chore("Conflict at upgrade");
    f.stick(&id, "status = 'in_review'", 8);
    transition_at(&f, &id, "pr_mergeable_state = 'conflicting'", now() - 8 * HOUR);
    let conn = f.db.connect().unwrap();
    conn.execute("DELETE FROM task_status_audit WHERE work_item_id = ?1 AND property IN ('pr_mergeable_state','ci_required_state')", [&id]).unwrap();
    crate::task_status_audit::migrate_pr_observations(&conn).unwrap();
    crate::task_status_audit::migrate_pr_observations(&conn).unwrap();
    let (count, lower): (i64, bool) = conn.query_row("SELECT COUNT(*), MIN(age_is_lower_bound) FROM task_status_audit WHERE work_item_id = ?1 AND property = 'pr_mergeable_state'", [&id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(count, 1);
    assert!(lower);
    drop(conn);
    assert!((f.clauses(&id).stuck_since - now()).abs() < 5);
    f.assert_candidate(&id, false);
}

#[test]
fn poll_changes_preserve_report_age_and_classify_conflicts_consistently() {
    let f = fixture();
    let id = f.chore("Review poll history");
    f.stick(&id, "status = 'in_review', autostart = 1", 4);
    let report = || {
        f.db.stuck_report(0, now(), &observed_live_pid)
            .unwrap()
            .rows
            .into_iter()
            .find(|r| r.work_item_id == id)
            .unwrap()
    };
    let before = report();
    transition_at(&f, &id, "pr_mergeable_state = 'conflicting'", now());
    let after = report();
    assert_eq!(after.classification, "S5_merge_conflict");
    assert_eq!(f.clauses(&id).stuck_kind.as_deref(), Some("S5_merge_conflict"));
    assert!((after.age_in_state_secs - before.age_in_state_secs).abs() < 5);
    assert_eq!(after.last_ledger_basis, before.last_ledger_basis);
    f.db.update_task_pr_poll_state(
        &id,
        PrPollStateInput {
            ci_required_state: "fail",
            pr_mergeable_state: "conflicting",
            ..Default::default()
        },
    )
    .unwrap();
    assert!((report().age_in_state_secs - before.age_in_state_secs).abs() < 5);
}

#[test]
fn thousands_of_observations_keep_the_original_conflict_episode() {
    let f = fixture();
    let id = f.chore("Long poll history");
    f.stick(&id, "status = 'in_review'", 8);
    transition_at(&f, &id, "pr_mergeable_state = 'conflicting'", now() - 4 * HOUR);
    let began = f.clauses(&id).stuck_since;
    let mut conn = f.db.connect().unwrap();
    eprintln!("creating 4000 PR observation transitions");
    let tx = conn.transaction().unwrap();
    {
        let mut update = tx
            .prepare("UPDATE tasks SET ci_required_state = ?2 WHERE id = ?1")
            .unwrap();
        for i in 0..4000 {
            update
                .execute(params![id, if i % 2 == 0 { "fail" } else { "success" }])
                .unwrap();
        }
    }
    tx.commit().unwrap();
    let observations: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_status_audit WHERE work_item_id = ?1 AND property = 'ci_required_state'",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(observations >= 4000);
    let instructions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = instructions.clone();
    conn.progress_handler(
        1000,
        Some(move || counted.fetch_add(1000, std::sync::atomic::Ordering::Relaxed) >= 10_000_000),
    );
    let started = std::time::Instant::now();
    let result = conn.query_row(
        "SELECT stuck_since FROM progression_candidate_clauses WHERE work_item_id = ?1",
        [&id],
        |r| r.get::<_, i64>(0),
    );
    eprintln!(
        "episode lookup: {result:?}, {} instructions",
        instructions.load(std::sync::atomic::Ordering::Relaxed)
    );
    assert_eq!(result.unwrap(), began);
    let candidate: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM progression_candidates WHERE work_item_id = ?1)",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(candidate);
    eprintln!(
        "4000-observation episode and candidate queries: {:?}, {} instructions",
        started.elapsed(),
        instructions.load(std::sync::atomic::Ordering::Relaxed)
    );
}

fn open_attention_at(f: &Fixture, id: &str, kind: &str, opened: i64) -> String {
    let attention = f.db.upsert_work_item_attention(id, kind, "title", "body").unwrap();
    f.sql(&format!(
        "UPDATE work_attention_items SET created_at = {opened}, last_raised_at = {opened} WHERE id = '{attention}'"
    ));
    attention
}

fn resolve_attention_at(f: &Fixture, id: &str, kind: &str, attention: &str, closed: i64) {
    crate::dispatch_helpers::resolve_attention_kind_in_tx(&f.db.connect().unwrap(), id, kind).unwrap();
    f.sql(&format!(
        "UPDATE work_attention_items SET resolved_at = {closed} WHERE id = '{attention}'"
    ));
}

fn review_conflict_since(f: &Fixture, name: &str, hours: i64) -> String {
    let id = f.chore(name);
    f.stick(&id, "status = 'in_review', autostart = 1", hours);
    transition_at(f, &id, "pr_mergeable_state = 'conflicting'", now() - hours * HOUR);
    id
}

#[test]
fn resolving_a_long_recovery_hold_starts_a_fresh_conflict_episode() {
    let debounce = boss_engine_storage_values::progression::STUCK_EPISODE_RECOVERY_SECS;
    let f = fixture();
    let id = review_conflict_since(&f, "Recovery then conflict", 4);
    let opened = now() - 3 * HOUR;
    let closed = now() - 60;
    let attention = open_attention_at(&f, &id, "worker_recovery_exhausted", opened);
    resolve_attention_at(&f, &id, "worker_recovery_exhausted", &attention, closed);
    assert_eq!(f.clauses(&id).stuck_kind.as_deref(), Some("S5_merge_conflict"));
    assert!((f.clauses(&id).stuck_since - closed).abs() < 5);
    f.assert_candidate(&id, false);
    assert!(closed - opened > debounce);
}

#[test]
fn a_short_recovery_hold_preserves_the_conflict_episode() {
    let debounce = boss_engine_storage_values::progression::STUCK_EPISODE_RECOVERY_SECS;
    let f = fixture();
    let id = review_conflict_since(&f, "Brief recovery blip", 4);
    let began = f.clauses(&id).stuck_since;
    let opened = now() - 30;
    let closed = opened + 10;
    let attention = open_attention_at(&f, &id, "worker_recovery_exhausted", opened);
    resolve_attention_at(&f, &id, "worker_recovery_exhausted", &attention, closed);
    assert_eq!(f.clauses(&id).stuck_kind.as_deref(), Some("S5_merge_conflict"));
    assert_eq!(f.clauses(&id).stuck_since, began);
    f.assert_candidate(&id, true);
    assert!(closed - opened < debounce);
}

#[test]
fn releasing_a_long_deferral_starts_a_fresh_conflict_episode() {
    let f = fixture();
    let id = review_conflict_since(&f, "Deferred then released", 4);
    transition_at(&f, &id, "deferred = 1", now() - 3 * HOUR);
    transition_at(&f, &id, "deferred = 0", now() - 60);
    assert_eq!(f.clauses(&id).stuck_kind.as_deref(), Some("S5_merge_conflict"));
    assert!((f.clauses(&id).stuck_since - (now() - 60)).abs() < 5);
    f.assert_candidate(&id, false);
}

#[test]
fn releasing_a_long_manual_start_hold_starts_a_fresh_conflict_episode() {
    let f = fixture();
    let id = review_conflict_since(&f, "Manual start then released", 4);
    transition_at(&f, &id, "status = 'todo', autostart = 0", now() - 3 * HOUR);
    transition_at(&f, &id, "status = 'in_review', autostart = 1", now() - 60);
    assert_eq!(f.clauses(&id).stuck_kind.as_deref(), Some("S5_merge_conflict"));
    assert!((f.clauses(&id).stuck_since - (now() - 60)).abs() < 5);
    f.assert_candidate(&id, false);
}

#[test]
fn resolving_a_long_trunk_queue_hold_starts_a_fresh_conflict_episode() {
    let f = fixture();
    let id = review_conflict_since(&f, "Trunk queue then conflict", 4);
    let opened = now() - 3 * HOUR;
    let closed = now() - 60;
    let attention = open_attention_at(&f, &id, "trunk_queue_waiting", opened);
    resolve_attention_at(&f, &id, "trunk_queue_waiting", &attention, closed);
    assert_eq!(f.clauses(&id).stuck_kind.as_deref(), Some("S5_merge_conflict"));
    assert!((f.clauses(&id).stuck_since - closed).abs() < 5);
    f.assert_candidate(&id, false);
}

#[test]
fn a_new_dependency_cycle_ages_from_the_joining_edge() {
    let f = fixture();
    let first = f.chore("Cycle A");
    let second = f.chore("Cycle B");
    f.stick(&first, "status = 'blocked', blocked_reason = 'dependency'", 8);
    f.stick(&second, "status = 'blocked', blocked_reason = 'dependency'", 8);
    f.sql(&format!(
        "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at)
         VALUES ('{first}', '{second}', 'blocks', '{}'),
                ('{second}', '{first}', 'blocks', '{}')",
        now() - 8 * HOUR,
        now() - HOUR
    ));
    assert_eq!(f.clauses(&first).stuck_kind.as_deref(), Some("S4_dependency_cycle"));
    assert!((f.clauses(&first).stuck_since - (now() - HOUR)).abs() < 5);
    f.assert_candidate(&first, false);
}

#[test]
fn review_churn_keeps_its_age_across_pending_to_success_ci() {
    let f = fixture();
    let id = f.chore("Churn CI poll");
    f.stick(&id, "status = 'in_review'", 8);
    open_attention_at(&f, &id, "churn_guard_parked", now() - 4 * HOUR);
    assert_eq!(f.clauses(&id).stuck_kind.as_deref(), Some("S16_review_churn"));
    let began = f.clauses(&id).stuck_since;
    assert!((now() - began - 4 * HOUR).abs() < 5);
    f.assert_candidate(&id, true);
    f.db.update_task_pr_poll_state(
        &id,
        PrPollStateInput {
            ci_required_state: "pending",
            pr_mergeable_state: "mergeable",
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(f.clauses(&id).stuck_since, began);
    f.assert_candidate(&id, true);
    f.db.update_task_pr_poll_state(
        &id,
        PrPollStateInput {
            ci_required_state: "success",
            pr_mergeable_state: "mergeable",
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(f.clauses(&id).stuck_kind.as_deref(), Some("S16_review_churn"));
    assert_eq!(f.clauses(&id).stuck_since, began);
    f.assert_candidate(&id, true);
}

#[test]
fn an_open_conflict_deferral_is_not_the_merge_conflict_kind() {
    let f = fixture();
    let id = f.chore("Deferred review conflict");
    f.sql(&format!(
        "UPDATE tasks SET pr_url = 'https://github.com/o/r/pull/7' WHERE id = '{id}'"
    ));
    f.stick(&id, "status = 'in_review', autostart = 1", 5);
    f.db.update_task_pr_poll_state(
        &id,
        PrPollStateInput {
            ci_required_state: "success",
            review_required_state: "approved",
            pr_mergeable_state: "conflicting",
            ..Default::default()
        },
    )
    .unwrap();
    f.age(&id, 3 * HOUR);
    assert_eq!(f.clauses(&id).stuck_kind.as_deref(), Some("S5_merge_conflict"));
    f.assert_candidate(&id, true);
    f.db.record_conflict_deferral(
        ConflictResolutionInsertInput::builder()
            .product_id(f.product_id(&id))
            .work_item_id(&id)
            .pr_url("https://github.com/o/r/pull/7")
            .pr_number(7)
            .head_branch("feature")
            .base_branch("main")
            .base_sha_at_trigger("base-1")
            .head_sha_before("head-1")
            .build(),
    )
    .unwrap();
    assert!(f.clauses(&id).stuck_kind.is_none());
    f.assert_candidate(&id, false);
    let conn = f.db.connect().unwrap();
    let task = query_task(&conn, &id).unwrap().unwrap();
    assert_eq!(
        crate::stuck_report::classification(&conn, &task, None).unwrap(),
        "awaiting_merge:conflict_deferred"
    );
}
