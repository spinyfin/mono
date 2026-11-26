use super::*;
use crate::merge_when_ready::{DirectMergeExecutor, DirectMergeSuccess};
use boss_engine_merge_poller::tests::StubProbe;
use boss_engine_merge_poller::{OpenPrStatus, PrLifecycleState, run_one_pass};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct RecordingMerge {
    calls: AtomicUsize,
    disarms: AtomicUsize,
}

struct MovingMerge<'a> {
    db: &'a WorkDb,
    id: &'a str,
}

#[async_trait::async_trait]
impl DirectMergeExecutor for MovingMerge<'_> {
    async fn execute(&self, _: &str) -> Result<DirectMergeSuccess> {
        self.db
            .stop_merge_conflict_resolution(self.id, "Stopped during merge")?;
        Ok(DirectMergeSuccess {
            head_sha: "resolved".into(),
        })
    }

    async fn disarm(&self, _: &str) -> Result<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl DirectMergeExecutor for RecordingMerge {
    async fn execute(&self, _: &str) -> Result<DirectMergeSuccess> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(DirectMergeSuccess {
            head_sha: "resolved".into(),
        })
    }

    async fn disarm(&self, _: &str) -> Result<()> {
        self.disarms.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn clean_sweeps_wait_for_tripwire_and_only_resume_approved_resolution_once() {
    for (rejected, moved) in [(false, false), (true, false), (true, true)] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let (candidate, attempt, id) = blocked_with_attempt(&db);
        db.record_github_merge_intent(
            crate::work::GithubMergeIntentInsertInput::builder()
                .work_item_id(&id)
                .pr_url(&candidate.pr_url)
                .head_sha("before-rebase")
                .build(),
            "{}",
        )
        .unwrap();
        db.start_merge_conflict_resolution(&id, &candidate.pr_url, "main")
            .unwrap();
        let gate = Arc::new(tokio::sync::Notify::new());
        let cube = ScriptCube {
            tripwire_gate: Some(gate.clone()),
            ..ScriptCube::with_tripwire_findings(
                Script::CleanPushed,
                if rejected { vec!["important.rs".into()] } else { vec![] },
            )
        };
        let publisher = RecordingPublisher::default();
        let executor = Arc::new(RecordingMerge::default());
        let host =
            crate::merge_poller_adapters::CoreSweepHost::with_remediation(None).with_merge_executor(executor.clone());
        let probe = StubProbe::new();
        probe.set(&candidate.pr_url, PrLifecycleState::Open(OpenPrStatus::clean()));
        let ladder = try_mechanical_rungs(&db, &publisher, &cube, &candidate, &attempt);
        tokio::pin!(ladder);
        // Poll the real ladder until its tripwire future is pending.
        tokio::select! {
            outcome = &mut ladder => panic!("tripwire must still be pending: {outcome:?}"),
            _ = tokio::task::yield_now() => {}
        }
        assert_eq!(
            db.get_conflict_resolution(&attempt.id)
                .unwrap()
                .unwrap()
                .mechanical_rung_in_flight,
            Some(1)
        );
        run_one_pass(&db, probe.as_ref(), &publisher, None, None, Some(&host)).await;
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        assert!(db.merge_conflict_resolution_requested(&id, &candidate.pr_url).unwrap());
        assert_eq!(db.task_blocked_reason(&id).unwrap().as_deref(), Some("merge_conflict"));
        if moved {
            db.update_task(
                &id,
                boss_protocol::WorkItemPatch {
                    status: Some("in_review".into()),
                    ..Default::default()
                },
                "human",
            )
            .unwrap();
        }
        gate.notify_one();
        let outcome = ladder.await;
        assert_eq!(
            outcome,
            if rejected {
                LadderOutcome::HaltedForSignoff { had_conflicts: false }
            } else {
                LadderOutcome::Retired
            }
        );
        for _ in 0..3 {
            run_one_pass(&db, probe.as_ref(), &publisher, None, None, Some(&host)).await;
        }
        assert_eq!(executor.calls.load(Ordering::SeqCst), usize::from(!rejected));
        let WorkItem::Chore(card) = db.get_work_item(&id).unwrap() else {
            panic!("expected chore")
        };
        if rejected {
            assert_eq!(
                card.blocked_reason.as_deref(),
                if moved { None } else { Some("deletion_signoff") }
            );
            assert_eq!(card.merge_queue_state, None);
            assert!(card.merge_queue_detail.unwrap().contains("approval for deleted files"));
            assert!(
                db.get_active_github_merge_intent(&id, &candidate.pr_url)
                    .unwrap()
                    .is_none()
            );
        } else {
            assert_eq!(card.merge_queue_state.as_deref(), Some("queued"));
            assert_eq!(
                db.get_active_github_merge_intent(&id, &candidate.pr_url)
                    .unwrap()
                    .unwrap()
                    .head_sha,
                "resolved"
            );
        }
    }
}

#[tokio::test]
async fn clean_sweep_recovers_persisted_rejection_before_resuming() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = WorkDb::open(path.clone()).unwrap();
    let (candidate, attempt, id) = blocked_with_attempt(&db);
    let attempt = supersede_with_decoy(&db, &candidate, &attempt);
    db.start_merge_conflict_resolution(&id, &candidate.pr_url, "main")
        .unwrap();
    record_target_halt(&db, &attempt);
    db.mark_conflict_resolution_succeeded_at_rung(&attempt.id, None, 1)
        .unwrap();
    drop(db);
    let db = WorkDb::open(path).unwrap();
    let executor = Arc::new(RecordingMerge::default());
    let host =
        crate::merge_poller_adapters::CoreSweepHost::with_remediation(None).with_merge_executor(executor.clone());
    let probe = StubProbe::new();
    probe.set(&candidate.pr_url, PrLifecycleState::Open(OpenPrStatus::clean()));
    for _ in 0..3 {
        run_one_pass(
            &db,
            probe.as_ref(),
            &RecordingPublisher::default(),
            None,
            None,
            Some(&host),
        )
        .await;
    }
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        db.task_blocked_reason(&id).unwrap().as_deref(),
        Some("deletion_signoff")
    );
    assert!(!db.merge_conflict_resolution_requested(&id, &candidate.pr_url).unwrap());
    assert_signoff_attention(&db, &id);
    assert_target_deletions_listed(&db, &id, &candidate.pr_url);
}

const TARGET_DELETIONS: [&str; 2] = ["a.rs", "b.rs"];

/// Record a superseded earlier attempt that halted on different evidence, then
/// start the attempt the card is really blocked on. Recovery must list the
/// deletions of the attempt that halted this card, not the earlier one's.
fn supersede_with_decoy(
    db: &WorkDb,
    candidate: &PendingMergeCheck,
    attempt: &ConflictResolution,
) -> ConflictResolution {
    db.record_conflict_ladder_outcome_with_deletions(
        &attempt.id,
        "engine_rebase",
        "halted_for_signoff",
        "stale evidence",
        &["stale.rs".to_owned()],
    )
    .unwrap();
    db.mark_conflict_resolution_abandoned(&attempt.id, "superseded")
        .unwrap();
    db.insert_conflict_resolution(ConflictResolutionInsertInput {
        product_id: attempt.product_id.clone(),
        work_item_id: attempt.work_item_id.clone(),
        pr_url: candidate.pr_url.clone(),
        pr_number: 42,
        head_branch: "feature".to_owned(),
        base_branch: "main".to_owned(),
        base_sha_at_trigger: Some("base222".to_owned()),
        head_sha_before: Some("head333".to_owned()),
    })
    .unwrap()
    .expect("replacement attempt row")
}

fn record_target_halt(db: &WorkDb, attempt: &ConflictResolution) {
    let deletions: Vec<String> = TARGET_DELETIONS.iter().map(|s| (*s).to_owned()).collect();
    db.record_conflict_ladder_outcome_with_deletions(
        &attempt.id,
        "engine_rebase",
        "halted_for_signoff",
        "deletion tripwire",
        &deletions,
    )
    .unwrap();
}

fn assert_target_deletions_listed(db: &WorkDb, id: &str, pr_url: &str) {
    let items = db.list_attention_items_for_work_item(id).unwrap();
    let body = &items
        .iter()
        .find(|item| item.kind == crate::merge_parent_deletion::SIGNOFF_ATTENTION_KIND)
        .expect("sign-off attention item")
        .body_markdown;
    let deletions: Vec<String> = TARGET_DELETIONS.iter().map(|s| (*s).to_owned()).collect();
    assert_eq!(
        body,
        &crate::merge_parent_deletion::render_signoff_attention_body(&deletions, pr_url)
    );
    assert!(body.contains("2 surface(s)"), "{body}");
    assert!(body.contains("- a.rs") && body.contains("- b.rs"), "{body}");
    assert!(!body.contains("stale.rs"), "{body}");
}

fn assert_signoff_attention(db: &WorkDb, id: &str) {
    let items = db
        .list_open_attention_items_of_kind(crate::merge_parent_deletion::SIGNOFF_ATTENTION_KIND)
        .unwrap();
    let items: Vec<_> = items
        .iter()
        .filter(|item| item.work_item_id.as_deref() == Some(id))
        .collect();
    assert_eq!(items.len(), 1);
    assert!(
        items[0].body_markdown.contains("evidence unavailable") || items[0].body_markdown.contains("surface(s)"),
        "{}",
        items[0].body_markdown
    );
}

#[tokio::test]
async fn startup_recovery_stops_unverified_merge_requests_before_clean_sweeps() {
    for rejected in [false, true] {
        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        let db = WorkDb::open(path.clone()).unwrap();
        let (candidate, attempt, id) = blocked_with_attempt(&db);
        let attempt = if rejected {
            supersede_with_decoy(&db, &candidate, &attempt)
        } else {
            attempt
        };
        db.start_merge_conflict_resolution(&id, &candidate.pr_url, "main")
            .unwrap();
        db.stamp_conflict_resolution_mechanical_rung(&attempt.id, 1, "lease", "workspace")
            .unwrap();
        if rejected {
            record_target_halt(&db, &attempt);
        }
        drop(db);
        let db = WorkDb::open(path).unwrap();
        assert_eq!(db.reconcile_orphaned_conflict_ladder_attempts().unwrap().len(), 1);
        let executor = Arc::new(RecordingMerge::default());
        let host =
            crate::merge_poller_adapters::CoreSweepHost::with_remediation(None).with_merge_executor(executor.clone());
        let probe = StubProbe::new();
        probe.set(&candidate.pr_url, PrLifecycleState::Open(OpenPrStatus::clean()));
        for _ in 0..3 {
            run_one_pass(
                &db,
                probe.as_ref(),
                &RecordingPublisher::default(),
                None,
                None,
                Some(&host),
            )
            .await;
        }
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        assert!(!db.merge_conflict_resolution_requested(&id, &candidate.pr_url).unwrap());
        if rejected {
            assert_eq!(
                db.task_blocked_reason(&id).unwrap().as_deref(),
                Some("deletion_signoff")
            );
            assert_signoff_attention(&db, &id);
            assert_target_deletions_listed(&db, &id, &candidate.pr_url);
        }
    }
}

#[tokio::test]
async fn clean_sweep_stops_persisted_rejection_after_human_move_to_review() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, id) = blocked_with_attempt(&db);
    db.start_merge_conflict_resolution(&id, &candidate.pr_url, "main")
        .unwrap();
    db.record_conflict_ladder_outcome(&attempt.id, "engine_rebase", "halted_for_signoff", "deletion tripwire")
        .unwrap();
    db.update_task(
        &id,
        boss_protocol::WorkItemPatch {
            status: Some("in_review".into()),
            ..Default::default()
        },
        "human",
    )
    .unwrap();
    let executor = Arc::new(RecordingMerge::default());
    let host =
        crate::merge_poller_adapters::CoreSweepHost::with_remediation(None).with_merge_executor(executor.clone());
    let probe = StubProbe::new();
    probe.set(&candidate.pr_url, PrLifecycleState::Open(OpenPrStatus::clean()));
    for _ in 0..3 {
        run_one_pass(
            &db,
            probe.as_ref(),
            &RecordingPublisher::default(),
            None,
            None,
            Some(&host),
        )
        .await;
    }
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(!db.merge_conflict_resolution_requested(&id, &candidate.pr_url).unwrap());
}

#[tokio::test]
async fn a_card_moved_during_merge_does_not_publish_continuation() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, id) = blocked_with_attempt(&db);
    db.start_merge_conflict_resolution(&id, &candidate.pr_url, "main")
        .unwrap();
    db.mark_conflict_resolution_succeeded_at_rung(&attempt.id, None, 1)
        .unwrap();
    db.clear_resolved_pr_signal(&id, &candidate.pr_url, false).unwrap();
    let publisher = RecordingPublisher::default();
    crate::merge_when_ready::resume_after_conflict(&db, &publisher, &candidate, &MovingMerge { db: &db, id: &id })
        .await;
    assert!(
        !publisher
            .lifecycle_reasons()
            .await
            .contains(&"merge_conflict_continuation".into())
    );
    assert!(!db.merge_conflict_resolution_requested(&id, &candidate.pr_url).unwrap());
}

#[test]
fn failed_signoff_write_rolls_back_attempt_retirement() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, id) = blocked_with_attempt(&db);
    db.start_merge_conflict_resolution(&id, &candidate.pr_url, "main")
        .unwrap();
    db.stamp_conflict_resolution_mechanical_rung(&attempt.id, 1, "lease", "workspace")
        .unwrap();
    db.connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_signoff BEFORE UPDATE ON tasks
         WHEN NEW.blocked_reason = 'deletion_signoff'
         BEGIN SELECT RAISE(ABORT, 'signoff write failed'); END;",
        )
        .unwrap();
    assert!(
        db.halt_conflict_resolution_for_deletion_signoff(&attempt.id, 1, &[])
            .is_err()
    );
    let live = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(live.status, "pending");
    assert_eq!(live.mechanical_rung_in_flight, Some(1));
    db.clear_resolved_pr_signal(&id, &candidate.pr_url, false).unwrap();
    assert_eq!(db.task_blocked_reason(&id).unwrap().as_deref(), Some("merge_conflict"));
    assert!(db.merge_conflict_resolution_requested(&id, &candidate.pr_url).unwrap());
}

#[test]
fn signoff_attention_lists_every_deleted_surface_on_halt_and_recovery() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, id) = blocked_with_attempt(&db);
    db.start_merge_conflict_resolution(&id, &candidate.pr_url, "main")
        .unwrap();
    let deletions = vec!["a.rs".to_owned(), "b.rs".to_owned()];
    db.halt_conflict_resolution_for_deletion_signoff(&attempt.id, 1, &deletions)
        .unwrap();
    let expected = crate::merge_parent_deletion::render_signoff_attention_body(&deletions, &candidate.pr_url);
    let items = db.list_attention_items_for_work_item(&id).unwrap();
    let body = &items
        .iter()
        .find(|item| item.kind == crate::merge_parent_deletion::SIGNOFF_ATTENTION_KIND)
        .unwrap()
        .body_markdown;
    assert_eq!(body, &expected);
    assert!(body.contains("2 surface(s)"));
    assert!(body.contains("- a.rs"));
    assert!(body.contains("- b.rs"));

    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, id) = blocked_with_attempt(&db);
    db.record_conflict_ladder_outcome_with_deletions(
        &attempt.id,
        "engine_rebase",
        "halted_for_signoff",
        "deletion tripwire",
        &deletions,
    )
    .unwrap();
    db.mark_chore_blocked_deletion_signoff(&id, &candidate.pr_url).unwrap();
    let recovered = db
        .list_attention_items_for_work_item(&id)
        .unwrap()
        .into_iter()
        .find(|item| item.kind == crate::merge_parent_deletion::SIGNOFF_ATTENTION_KIND)
        .unwrap()
        .body_markdown;
    assert_eq!(
        recovered,
        crate::merge_parent_deletion::render_signoff_attention_body(&deletions, &candidate.pr_url)
    );
}

#[tokio::test]
async fn sweep_disarms_a_deletion_signoff_card() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, id) = blocked_with_attempt(&db);
    db.record_github_merge_intent(
        crate::work::GithubMergeIntentInsertInput::builder()
            .work_item_id(&id)
            .pr_url(&candidate.pr_url)
            .head_sha("before-rebase")
            .build(),
        "{}",
    )
    .unwrap();
    db.start_merge_conflict_resolution(&id, &candidate.pr_url, "main")
        .unwrap();
    db.halt_conflict_resolution_for_deletion_signoff(&attempt.id, 1, &["gone.rs".into()])
        .unwrap();
    assert_eq!(
        db.task_blocked_reason(&id).unwrap().as_deref(),
        Some("deletion_signoff")
    );
    assert!(db.merge_disarm_pending(&id, &candidate.pr_url).unwrap());
    let executor = Arc::new(RecordingMerge::default());
    let host =
        crate::merge_poller_adapters::CoreSweepHost::with_remediation(None).with_merge_executor(executor.clone());
    let probe = StubProbe::new();
    probe.set(&candidate.pr_url, PrLifecycleState::Open(OpenPrStatus::clean()));
    run_one_pass(
        &db,
        probe.as_ref(),
        &RecordingPublisher::default(),
        None,
        None,
        Some(&host),
    )
    .await;
    assert_eq!(executor.disarms.load(Ordering::SeqCst), 1);
    assert!(!db.merge_disarm_pending(&id, &candidate.pr_url).unwrap());
}

#[tokio::test]
async fn sweep_retires_a_pending_disarm_for_a_merged_pr_without_calling_github() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, id) = blocked_with_attempt(&db);
    db.record_github_merge_intent(
        crate::work::GithubMergeIntentInsertInput::builder()
            .work_item_id(&id)
            .pr_url(&candidate.pr_url)
            .head_sha("before-rebase")
            .build(),
        "{}",
    )
    .unwrap();
    db.start_merge_conflict_resolution(&id, &candidate.pr_url, "main")
        .unwrap();
    db.halt_conflict_resolution_for_deletion_signoff(&attempt.id, 1, &["gone.rs".into()])
        .unwrap();
    assert!(db.merge_disarm_pending(&id, &candidate.pr_url).unwrap());
    let executor = Arc::new(RecordingMerge::default());
    let host =
        crate::merge_poller_adapters::CoreSweepHost::with_remediation(None).with_merge_executor(executor.clone());
    let probe = StubProbe::new();
    probe.set(&candidate.pr_url, PrLifecycleState::Merged);
    for _ in 0..3 {
        run_one_pass(
            &db,
            probe.as_ref(),
            &RecordingPublisher::default(),
            None,
            None,
            Some(&host),
        )
        .await;
    }
    assert_eq!(executor.disarms.load(Ordering::SeqCst), 0);
    assert!(!db.merge_disarm_pending(&id, &candidate.pr_url).unwrap());
    assert!(db.list_pending_merge_disarms().unwrap().is_empty());
    // GitHub merged it first: the card must not keep saying the merge is
    // paused for sign-off; it is handed to the terminal-merge path.
    assert_eq!(db.task_blocked_reason(&id).unwrap(), None);
    let merged_card = db
        .get_work_tree(&candidate.product_id)
        .unwrap()
        .chores
        .into_iter()
        .find(|task| task.id == id)
        .unwrap();
    assert_eq!(merged_card.status.to_string(), "done");
    assert_eq!(merged_card.merge_queue_state, None);
}
