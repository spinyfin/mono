use super::*;
use crate::conflict_inventory::*;

fn fixture(db: &WorkDb, product: &str, number: i64) -> ConflictResolutionInsertInput {
    let url = format!("https://github.com/spinyfin/mono/pull/{number}");
    let chore = create_test_chore_manual(db, product, format!("Conflicting PR #{number}"));
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            params![chore.id, url],
        )
        .unwrap();
    ConflictResolutionInsertInput {
        product_id: product.into(),
        work_item_id: chore.id,
        pr_url: url,
        pr_number: number,
        head_branch: "feature".into(),
        base_branch: "main".into(),
        base_sha_at_trigger: Some("base".into()),
        head_sha_before: Some("head".into()),
    }
}

fn resolved(db: &WorkDb, input: &ConflictResolutionInsertInput) -> ConflictResolution {
    let attempt = db.insert_conflict_resolution(input.clone()).unwrap().unwrap();
    db.mark_conflict_resolution_succeeded(&attempt.id, Some("resolved"))
        .unwrap()
        .unwrap()
}

fn stop(db: &WorkDb, input: &ConflictResolutionInsertInput) -> (Attention, AttentionGroup) {
    let (question, group, _) = db
        .stop_long_conflicting_pr(
            input,
            ConflictInventoryLimits {
                resolutions: 1,
                days: 7,
            },
        )
        .unwrap()
        .unwrap();
    (question, group)
}

#[test]
fn resolution_limit_stops_and_persists_one_question_across_head_changes_and_retry() {
    let db = WorkDb::open(temp_db_path("inventory-count")).unwrap();
    let product = make_revision_product(&db, "inventory");
    let mut input = fixture(&db, &product, 1);
    assert!(
        db.stop_long_conflicting_pr(&input, ConflictInventoryLimits::default())
            .unwrap()
            .is_none()
    );
    let succeeded = resolved(&db, &input);
    let (question, group) = stop(&db, &input);
    input.head_sha_before = Some("new-head".into());
    assert_eq!(stop(&db, &input).0.id, question.id);
    assert_eq!(db.list_attentions_for_group(&group.id).unwrap().len(), 1);
    let stopped = db
        .get_conflict_resolution(question.source_anchor.as_deref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(stopped.status, "abandoned");
    assert_eq!(stopped.failure_reason.as_deref(), Some(CONFLICT_INVENTORY_STOP_REASON));
    assert!(stopped.revision_task_id.is_none());
    assert_eq!(
        db.get_conflict_resolution(&succeeded.id).unwrap().unwrap().status,
        "succeeded"
    );
    assert!(
        db.retry_conflict_resolution(&stopped.id)
            .unwrap_err()
            .to_string()
            .contains("answer")
    );
    assert_eq!(task_status(&db, &input.work_item_id), "blocked");
}

#[test]
fn age_limit_has_an_exact_boundary_and_ignores_telemetry() {
    let db = WorkDb::open(temp_db_path("inventory-age")).unwrap();
    let product = make_revision_product(&db, "inventory");
    let input = fixture(&db, &product, 2);
    let attempt = resolved(&db, &input);
    let now: i64 = now_string().parse().unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE conflict_resolutions SET created_at = ?2 WHERE id = ?1",
            params![attempt.id, (now - 7 * 86400).to_string()],
        )
        .unwrap();
    for source in ["producer", "speculative_predicted"] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE conflict_resolutions SET event_source = ?2 WHERE id = ?1",
                params![attempt.id, source],
            )
            .unwrap();
        assert!(
            db.stop_long_conflicting_pr(&input, ConflictInventoryLimits::default())
                .unwrap()
                .is_none()
        );
    }
    db.connect()
        .unwrap()
        .execute(
            "UPDATE conflict_resolutions SET event_source = 'review_watch' WHERE id = ?1",
            [&attempt.id],
        )
        .unwrap();
    assert!(
        db.stop_long_conflicting_pr(
            &input,
            ConflictInventoryLimits {
                resolutions: 100,
                days: 8
            }
        )
        .unwrap()
        .is_none()
    );
    assert!(
        db.stop_long_conflicting_pr(&input, ConflictInventoryLimits::default())
            .unwrap()
            .is_some()
    );
}

#[test]
fn grouped_choices_record_close_and_create_one_merge_or_port_revision() {
    let db = WorkDb::open(temp_db_path("inventory-batch")).unwrap();
    let product = make_revision_product(&db, "inventory");
    let choices = [
        CONFLICT_INVENTORY_MERGE,
        CONFLICT_INVENTORY_CLOSE,
        boss_protocol::CONFLICT_MISMATCH_CHOICE_REWRITE,
    ];
    let mut stops = Vec::new();
    for (i, choice) in choices.iter().enumerate() {
        let input = fixture(&db, &product, i as i64 + 10);
        resolved(&db, &input);
        let (question, group) = stop(&db, &input);
        db.answer_attention(&question.id, Some((*choice).into()), false, false)
            .unwrap();
        stops.push((input, question, group));
    }
    assert!(stops.iter().all(|(_, _, group)| group.id == stops[0].2.id));
    assert!(
        db.pending_conflict_inventory_actions().unwrap().is_empty(),
        "answers alone do not apply a batch"
    );
    let result = db
        .action_attention_group_as_actor(
            &stops[0].2.id,
            false,
            &Default::default(),
            &FakePrStateChecker::always(PrOpenState::Open),
            "human",
        )
        .unwrap();
    assert_eq!(result.group.state, "actioned");
    for ((input, question, _), choice) in stops.iter().zip(choices) {
        let (answer, approved): (String, Option<String>) = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT answer, action_attempt_id FROM conflict_inventory_decisions WHERE attention_id = ?1",
                [&question.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(answer, choice);
        let stopped = db
            .get_conflict_resolution(question.source_anchor.as_deref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            stopped.status, "abandoned",
            "a decision never marks the stopped row clean"
        );
        if choice == CONFLICT_INVENTORY_CLOSE {
            assert!(approved.is_none());
            assert_eq!(stop(&db, input).0.id, question.id);
        } else {
            let approved = db.get_conflict_resolution(&approved.unwrap()).unwrap().unwrap();
            let revision = query_task(&db.connect().unwrap(), approved.revision_task_id.as_deref().unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(revision.parent_task_id.as_deref(), Some(input.work_item_id.as_str()));
            assert!(approved.mismatch_exit_barred);
            if choice == CONFLICT_INVENTORY_MERGE {
                assert!(revision.description.contains("normal checked merge"));
                db.mark_conflict_resolution_succeeded(&approved.id, Some("fixed"))
                    .unwrap();
            } else {
                assert_eq!(revision.effort_level, Some(EffortLevel::Large));
                assert!(
                    revision
                        .description
                        .contains("Automatic conflict resolution reached its lifetime limit")
                );
                assert!(!revision.description.contains("How should PR"));
                assert!(revision.description.contains(" UTC)"));
                assert!(
                    revision
                        .description
                        .contains("Preserve the PR's behaviour and every earlier review fix")
                );
            }
        }
    }
    let pending = db.pending_conflict_inventory_actions().unwrap();
    assert_eq!(pending.len(), 2);
    assert!(pending.iter().any(|(_, choice, _)| choice == CONFLICT_INVENTORY_CLOSE));
    assert!(pending.iter().any(|(_, choice, _)| choice == CONFLICT_INVENTORY_MERGE));
}

#[test]
fn skipped_decisions_stay_stopped_and_invalid_choices_roll_back_the_batch() {
    let db = WorkDb::open(temp_db_path("inventory-skip")).unwrap();
    let product = make_revision_product(&db, "inventory");
    let input = fixture(&db, &product, 20);
    resolved(&db, &input);
    let (question, group) = stop(&db, &input);
    db.answer_attention(&question.id, Some("invented".into()), false, false)
        .unwrap();
    assert!(
        db.action_attention_group_as_actor(
            &group.id,
            false,
            &Default::default(),
            &FakePrStateChecker::always(PrOpenState::Open),
            "human"
        )
        .is_err()
    );
    assert!(db.pending_conflict_inventory_actions().unwrap().is_empty());
    db.answer_attention(&question.id, None, true, false).unwrap();
    db.action_attention_group_as_actor(
        &group.id,
        false,
        &Default::default(),
        &FakePrStateChecker::always(PrOpenState::Open),
        "human",
    )
    .unwrap();
    let (next, next_group) = stop(&db, &input);
    assert_ne!(next.id, question.id);
    db.answer_attention(&next.id, Some(CONFLICT_INVENTORY_CLOSE.into()), false, false)
        .unwrap();
    db.action_attention_group_as_actor(
        &next_group.id,
        false,
        &Default::default(),
        &FakePrStateChecker::always(PrOpenState::Open),
        "human",
    )
    .unwrap();
    assert_eq!(db.pending_conflict_inventory_actions().unwrap().len(), 1);
}

#[test]
fn restart_preserves_the_stop_and_the_pending_close() {
    let (_dir, path) = disk_db_path("inventory-restart");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = make_revision_product(&db, "inventory");
    let input = fixture(&db, &product, 21);
    resolved(&db, &input);
    let (question, group) = stop(&db, &input);
    drop(db);
    let db = WorkDb::open(path.clone()).unwrap();
    assert_eq!(stop(&db, &input).0.id, question.id);
    db.answer_attention(&question.id, Some(CONFLICT_INVENTORY_CLOSE.into()), false, false)
        .unwrap();
    db.action_attention_group_as_actor(
        &group.id,
        false,
        &Default::default(),
        &FakePrStateChecker::always(PrOpenState::Open),
        "human",
    )
    .unwrap();
    drop(db);
    let db = WorkDb::open(path).unwrap();
    assert_eq!(db.pending_conflict_inventory_actions().unwrap().len(), 1);
    assert_eq!(stop(&db, &input).0.id, question.id);
}

#[test]
fn stale_binding_or_nonhuman_action_cannot_authorize_closing_a_pr() {
    let db = WorkDb::open(temp_db_path("inventory-stale")).unwrap();
    let product = make_revision_product(&db, "inventory");
    let input = fixture(&db, &product, 22);
    resolved(&db, &input);
    let (question, group) = stop(&db, &input);
    db.answer_attention(&question.id, Some(CONFLICT_INVENTORY_CLOSE.into()), false, false)
        .unwrap();
    assert!(
        db.action_attention_group_as_actor(
            &group.id,
            false,
            &Default::default(),
            &FakePrStateChecker::always(PrOpenState::Open),
            "engine"
        )
        .is_err()
    );
    db.update_work_item(
        &input.work_item_id,
        WorkItemPatch {
            pr_url: Some("https://github.com/spinyfin/mono/pull/23".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        db.action_attention_group_as_actor(
            &group.id,
            false,
            &Default::default(),
            &FakePrStateChecker::always(PrOpenState::Open),
            "human"
        )
        .is_err()
    );
    assert!(db.pending_conflict_inventory_actions().unwrap().is_empty());
}

#[test]
fn a_new_stop_holds_merge_authorization_until_a_replacement_answer_is_applied() {
    let db = WorkDb::open(temp_db_path("inventory-recurrence")).unwrap();
    let product = make_revision_product(&db, "inventory");
    let input = fixture(&db, &product, 30);
    resolved(&db, &input);
    let (question, group) = stop(&db, &input);
    db.answer_attention(&question.id, Some(CONFLICT_INVENTORY_MERGE.into()), false, false)
        .unwrap();
    db.action_attention_group_as_actor(
        &group.id,
        false,
        &Default::default(),
        &FakePrStateChecker::always(PrOpenState::Open),
        "human",
    )
    .unwrap();
    let approved = db
        .active_conflict_resolution_for_work_item(&input.work_item_id)
        .unwrap()
        .unwrap();
    assert!(
        db.stop_long_conflicting_pr(
            &input,
            ConflictInventoryLimits {
                resolutions: 1,
                days: 7
            }
        )
        .unwrap()
        .is_none(),
        "the authorized revision must be allowed to finish"
    );
    db.mark_conflict_resolution_succeeded(&approved.id, Some("fixed"))
        .unwrap();
    assert_eq!(db.pending_conflict_inventory_actions().unwrap().len(), 1);
    let (next, group) = stop(&db, &input);
    assert_ne!(next.id, question.id);
    assert!(
        db.pending_conflict_inventory_actions().unwrap().is_empty(),
        "an unresolved decision holds the earlier merge request"
    );
    let prior: Option<String> = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT action_completed_at FROM conflict_inventory_decisions WHERE attention_id = ?1",
            [&question.id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(prior.is_none(), "raising a question preserves explicit authorization");
    db.dismiss_attention_as_actor(&group.id, None, "human").unwrap();
    let (fresh, fresh_group) = stop(&db, &input);
    assert_ne!(fresh.id, next.id);
    db.answer_attention(
        &fresh.id,
        Some(boss_protocol::CONFLICT_MISMATCH_CHOICE_REWRITE.into()),
        false,
        false,
    )
    .unwrap();
    db.action_attention_group_as_actor(
        &fresh_group.id,
        false,
        &Default::default(),
        &FakePrStateChecker::always(PrOpenState::Open),
        "human",
    )
    .unwrap();
    assert!(db.pending_conflict_inventory_actions().unwrap().is_empty());
    let prior: Option<String> = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT action_completed_at FROM conflict_inventory_decisions WHERE attention_id = ?1",
            [&question.id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        prior.is_some(),
        "applying a replacement answer retires the earlier merge request"
    );
}

#[test]
fn a_skipped_member_of_an_applied_batch_can_be_answered_in_a_successor_batch() {
    let db = WorkDb::open(temp_db_path("inventory-mixed-batch")).unwrap();
    let product = make_revision_product(&db, "inventory");
    let first = fixture(&db, &product, 40);
    let second = fixture(&db, &product, 41);
    resolved(&db, &first);
    resolved(&db, &second);
    let (answered, group) = stop(&db, &first);
    let (skipped, _) = stop(&db, &second);
    db.answer_attention(&answered.id, Some(CONFLICT_INVENTORY_CLOSE.into()), false, false)
        .unwrap();
    db.answer_attention(&skipped.id, None, true, false).unwrap();
    db.action_attention_group_as_actor(
        &group.id,
        false,
        &Default::default(),
        &FakePrStateChecker::always(PrOpenState::Open),
        "human",
    )
    .unwrap();
    let (renewed, successor) = stop(&db, &second);
    assert_ne!(renewed.id, skipped.id);
    assert_ne!(successor.id, group.id);
    db.answer_attention(&renewed.id, Some(CONFLICT_INVENTORY_CLOSE.into()), false, false)
        .unwrap();
    db.action_attention_group_as_actor(
        &successor.id,
        false,
        &Default::default(),
        &FakePrStateChecker::always(PrOpenState::Open),
        "human",
    )
    .unwrap();
    assert_eq!(db.pending_conflict_inventory_actions().unwrap().len(), 2);
}
