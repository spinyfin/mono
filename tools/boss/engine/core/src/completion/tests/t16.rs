//! PR binding is decided by the execution's own branch — never by which
//! URL a publish command printed first, never by an unverified staged URL
//! on the `run_done` path, and never for a PR another work item owns.
//!
//! Regression suite for the 2026-10 post-merge follow-up mis-binds: a
//! `cube pr create` whose output echoed the PR body printed the origin PR's
//! URL before the new PR's, the first URL was staged, and the `delivered`
//! declaration bound it without the branch check.

use super::*;

const ORIGIN_PR: &str = "https://github.com/spinyfin/mono/pull/1757";
const OWN_PR: &str = "https://github.com/spinyfin/mono/pull/1764";
const ORIGIN_BRANCH: &str = "boss/exec_18dad2723a80ed10_23";

fn own_branch(execution_id: &str) -> String {
    expected_branch_name(execution_id, &BranchNaming::BossExecPrefix, None)
}

/// Verifier that answers the origin PR's real head for #1757 and this
/// execution's branch for #1764 — the two PRs a follow-up's publish output
/// names.
fn two_pr_verifier(execution_id: &str) -> Arc<StubBranchVerifier> {
    let verifier = StubBranchVerifier::ok(ORIGIN_BRANCH);
    verifier.set_head_ref_for(1764, &own_branch(execution_id));
    verifier
}

fn task_row(db: &WorkDb, id: &str) -> boss_protocol::Task {
    match db.get_work_item(id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        other => panic!("expected a task row, got {other:?}"),
    }
}

/// Like [`fixture`], but the work item is a post-merge review follow-up
/// whose brief cites the merged origin PR — the row shape from the
/// incident.
fn followup_fixture(workspace_path: &Path) -> (TempDir, Arc<WorkDb>, String, String, String) {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let followup = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Address post-merge review findings")
                .maybe_description(Some(format!(
                    "Found in post-merge review of {ORIGIN_PR}. Address ALL findings before closing this follow-up."
                )))
                .maybe_kind_override(Some(TaskKind::Followup))
                .maybe_origin_pr_number(Some(1757))
                .build(),
        )
        .unwrap();
    assert_eq!(followup.kind, TaskKind::Followup);
    let execution = create_ready_chore_execution(&db, followup.id.clone());
    let (execution, run) = db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            workspace_path.to_str().unwrap(),
        )
        .unwrap();
    finish_run_worker_pane_alive(&db, &execution.id, &run.id, Some("spawned worker pane"));
    (dir, db, product.id, followup.id, execution.id)
}

/// Deliverable 5a: a publish command whose output contains the origin PR
/// URL before the new PR URL binds the new PR. Both are staged; the branch
/// check — not text position — selects.
#[tokio::test]
async fn stop_binds_the_pr_on_the_execution_branch_when_publish_output_named_the_origin_pr_first() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    // A wrong detector URL makes any fall-through to the cold path visible.
    let detector = StubPrDetector::ok(Some("https://github.com/should/not/pull/999"));
    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    assert_eq!(
        staged_pr_urls.record_command_observations(&execution_id, &[ORIGIN_PR.to_owned(), OWN_PR.to_owned()], true,),
        crate::pr_url_capture::RecordCommandObservationOutcome::Bound,
    );
    let verifier = two_pr_verifier(&execution_id);

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(verifier.clone());

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { ref pr_url } if pr_url == OWN_PR),
        "the PR on this execution's branch must bind and enter review, got {outcome:?}",
    );
    assert_eq!(
        verifier.head_ref_calls_snapshot(),
        vec![1757, 1764],
        "both candidates are checked by head branch; the origin PR is rejected, the new PR accepted",
    );
    assert_eq!(
        detector.call_count(),
        0,
        "a verified staged candidate needs no cold-path detection"
    );
    let task = task_row(&db, &chore_id);
    assert_eq!(
        task.pr_url.as_deref(),
        Some(OWN_PR),
        "never the origin PR the output printed first"
    );
    assert_eq!(db.get_execution(&execution_id).unwrap().pr_url.as_deref(), Some(OWN_PR));
    assert!(
        staged_pr_urls.get_entry(&execution_id).is_none(),
        "cache cleared after binding"
    );
}

/// Deliverable 5b: the `run_done` path rejects a staged URL whose head
/// branch is not the execution's, and — with no PR on the branch — fails
/// loudly with the branch and the rejected URL on the row.
#[tokio::test]
async fn run_done_delivered_rejects_a_staged_url_whose_head_is_not_the_execution_branch() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);
    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_command_observation(&execution_id, ORIGIN_PR, true);

    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(StubBranchVerifier::ok(ORIGIN_BRANCH));

    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    let StopOutcome::RunDoneDeclaredWithoutDelivery { detail } = outcome else {
        panic!("an unverifiable delivery must fail visibly, got {outcome:?}");
    };
    assert!(detail.contains(ORIGIN_PR), "the rejected URL is named: {detail}");
    assert!(
        detail.contains(&own_branch(&execution_id)),
        "the branch looked for is named: {detail}"
    );
    assert_eq!(
        detector.call_count(),
        1,
        "the execution's own branch is consulted before failing"
    );

    let task = task_row(&db, &chore_id);
    assert_eq!(task.pr_url, None, "the origin PR must never be bound");
    assert_eq!(task.status, TaskStatus::Blocked, "not parked silently in the backlog");
    assert_eq!(task.blocked_reason.as_deref(), Some("worker_failed"));
    assert!(
        task.blocked_detail.as_deref().is_some_and(|d| d.contains(ORIGIN_PR)),
        "the row says why: {:?}",
        task.blocked_detail
    );
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
}

/// Deliverable 1: when no output URL validates, the PR is resolved by
/// branch instead — and that is what binds.
#[tokio::test]
async fn run_done_delivered_resolves_by_branch_after_rejecting_the_staged_url() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(Some(OWN_PR));
    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_command_observation(&execution_id, ORIGIN_PR, true);

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(StubBranchVerifier::ok(ORIGIN_BRANCH));

    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == OWN_PR),
        "got {outcome:?}"
    );
    let calls = detector.calls_snapshot();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].expected_branch, own_branch(&execution_id));
    assert_eq!(task_row(&db, &chore_id).pr_url.as_deref(), Some(OWN_PR));
    assert_eq!(db.get_execution(&execution_id).unwrap().pr_url.as_deref(), Some(OWN_PR));
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
}

/// Deliverable 5c (Stop path): a follow-up citing a merged origin PR ends
/// up bound to its own PR and enters review.
#[tokio::test]
async fn followup_citing_a_merged_origin_pr_binds_its_own_pr_and_enters_review_on_stop() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, followup_id, execution_id) = followup_fixture(workspace.path());
    let detector = StubPrDetector::ok(Some("https://github.com/should/not/pull/999"));
    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    // The incident shape: the publish command's output carried the body's
    // origin citation before the new PR's URL.
    staged_pr_urls.record_command_observations(&execution_id, &[ORIGIN_PR.to_owned(), OWN_PR.to_owned()], true);

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(two_pr_verifier(&execution_id));

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { ref pr_url } if pr_url == OWN_PR),
        "the follow-up must enter review on ITS PR, got {outcome:?}",
    );
    let task = task_row(&db, &followup_id);
    assert_eq!(task.kind, TaskKind::Followup);
    assert_eq!(task.origin_pr_number, Some(1757), "provenance is untouched");
    assert_eq!(
        task.pr_url.as_deref(),
        Some(OWN_PR),
        "bound to the PR it opened, not its origin"
    );
    assert_eq!(db.get_execution(&execution_id).unwrap().pr_url.as_deref(), Some(OWN_PR));
}

/// Deliverable 5c (`run_done` path): the same follow-up, declaring
/// `delivered` with both URLs staged, binds its own PR — the staged entry
/// is branch-checked, not taken as-is.
#[tokio::test]
async fn followup_citing_a_merged_origin_pr_binds_its_own_pr_on_run_done() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, followup_id, execution_id) = followup_fixture(workspace.path());
    let detector = StubPrDetector::ok(Some("https://github.com/should/not/pull/999"));
    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_command_observations(&execution_id, &[ORIGIN_PR.to_owned(), OWN_PR.to_owned()], true);
    let verifier = two_pr_verifier(&execution_id);

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(verifier.clone());

    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == OWN_PR),
        "got {outcome:?}"
    );
    assert_eq!(verifier.head_ref_calls_snapshot(), vec![1757, 1764]);
    assert_eq!(detector.call_count(), 0);
    assert_eq!(task_row(&db, &followup_id).pr_url.as_deref(), Some(OWN_PR));
    assert_eq!(db.get_execution(&execution_id).unwrap().pr_url.as_deref(), Some(OWN_PR));
}

/// A staged entry the capture-time verification already confirmed is
/// trusted without another GitHub call on the `run_done` path.
#[tokio::test]
async fn run_done_delivered_trusts_a_branch_verified_staged_entry_without_a_network_call() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);
    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_command_observations(&execution_id, &[ORIGIN_PR.to_owned(), OWN_PR.to_owned()], true);
    let verifier = two_pr_verifier(&execution_id);

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(verifier.clone());

    // What the hook dispatcher spawns right after staging.
    handler.verify_staged_pr_url_after_capture(&execution_id, true).await;
    let entry = staged_pr_urls.get_entry(&execution_id).expect("entry retained");
    assert!(entry.branch_verified);
    assert_eq!(entry.pr_url, OWN_PR);
    assert_eq!(verifier.head_ref_calls_snapshot(), vec![1757, 1764]);

    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == OWN_PR),
        "got {outcome:?}"
    );
    assert_eq!(
        verifier.head_ref_calls_snapshot(),
        vec![1757, 1764],
        "no further head-ref fetch at declaration"
    );
    assert_eq!(detector.call_count(), 0);
    assert_eq!(task_row(&db, &chore_id).pr_url.as_deref(), Some(OWN_PR));
}

/// A GitHub outage during the declaration binds nothing and leaves the run
/// live for the Stop / recheck paths — never an unverified binding to make
/// the deadline, never a silent park.
#[tokio::test]
async fn run_done_delivered_defers_when_github_is_unreachable_instead_of_binding_unverified() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::err("gh: connect: network is unreachable");
    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_command_observation(&execution_id, ORIGIN_PR, true);

    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(StubBranchVerifier::err("gh: connect: network is unreachable"));

    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert_eq!(outcome, StopOutcome::DetectorFailed);
    assert!(
        db.get_execution(&execution_id).unwrap().status.is_live(),
        "left for the recheck paths"
    );
    assert_eq!(task_row(&db, &chore_id).pr_url, None);
    assert!(
        staged_pr_urls.get_entry(&execution_id).is_some(),
        "candidate kept for the retry"
    );
    assert!(cube.release_calls.lock().await.is_empty());
}

/// Deliverable 3: a PR already bound to a different work item is refused,
/// even when it is the PR the branch-keyed detector returns.
#[tokio::test]
async fn stop_refuses_to_bind_a_pr_that_belongs_to_another_work_item() {
    let workspace = tempdir().unwrap();
    let (_dir, db, product_id, chore_id, execution_id) = fixture(workspace.path());
    let other = create_test_chore(&db, product_id, "The real owner");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?2, status = 'done' WHERE id = ?1",
            rusqlite::params![other.id, OWN_PR],
        )
        .unwrap();
    let detector = StubPrDetector::ok(Some(OWN_PR));

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let outcome = handler.on_stop(&execution_id).await;
    let StopOutcome::ForeignPrRefused { pr_url, detail } = outcome else {
        panic!("expected ForeignPrRefused, got {outcome:?}");
    };
    assert_eq!(pr_url, OWN_PR);
    assert!(detail.contains(&other.id), "the conflicting owner is named: {detail}");
    let task = task_row(&db, &chore_id);
    assert_eq!(task.pr_url, None, "another work item's PR is never bound");
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
    assert!(
        db.list_attention_items(&execution_id)
            .unwrap()
            .iter()
            .any(|item| item.kind == crate::completion::pr_binding_guard::FOREIGN_PR_REFUSED_ATTENTION_KIND),
        "the refusal is surfaced as an attention item",
    );
}

mod stop_boundary {
    //! Flags-on (`worker_proposals` + `run_done_proposals_seam`) Stop-boundary
    //! behaviour of a no-changes-needed declaration: revisions keep the conflict
    //! refusal and SHA-delta protections, and concurrent Stops are serialized.

    use super::*;

    fn flags_on(workspace: &Path) -> Arc<crate::feature_flags::FeatureFlagsStore> {
        let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.join("feature-flags.toml"),
        ));
        flags.set("worker_proposals", true).unwrap();
        flags.set("run_done_proposals_seam", true).unwrap();
        flags
    }

    /// A merge-conflict revision that declares no-changes-needed while GitHub
    /// still reports CONFLICTING is refused with the conflict probe, not closed.
    #[tokio::test]
    async fn flags_on_conflict_revision_declaration_is_refused_while_github_conflicting() {
        use crate::merge_poller::OpenPrMergeability;

        let workspace = tempdir().unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/2080";
        let head = "dddddddddddddddddddddddddddddddddddddddd";
        let (_dir, db, _product_id, _parent_id, _revision_id, execution_id, attempt_id) =
            conflict_revision_fixture(workspace.path(), parent_pr_url, head);
        declare_no_changes_needed(&db, &execution_id);

        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier.set_head_oid(Ok(head.into())).await;
        let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
        let handler = handler
            .with_branch_verifier(verifier)
            .with_merge_probe(Arc::new(FixedMergeabilityProbe {
                mergeability: OpenPrMergeability::Conflict,
            }))
            .with_feature_flags(flags_on(workspace.path()));

        let outcome = handler.on_stop(&execution_id).await;
        assert_eq!(outcome, StopOutcome::AwaitingInput, "{outcome:?}");
        let queued = probes.snapshot();
        assert_eq!(queued.len(), 1, "{queued:?}");
        assert!(queued[0].1.contains("CONFLICTING"), "{}", queued[0].1);
        assert_ne!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Completed
        );
        assert_eq!(
            db.get_conflict_resolution(&attempt_id).unwrap().unwrap().status,
            "pending"
        );
    }

    /// A revision that pushes in the same turn it declares finalizes as
    /// in_review through the SHA-delta gate, not as a declined no-op.
    #[tokio::test]
    async fn flags_on_revision_that_pushed_and_declared_finalizes_in_review() {
        let workspace = tempdir().unwrap();
        let parent_pr_url = "https://github.com/spinyfin/mono/pull/2081";
        let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(
            workspace.path(),
            parent_pr_url,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        declare_no_changes_needed(&db, &execution_id);

        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier
            .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()))
            .await;
        let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
        let handler = handler
            .with_branch_verifier(verifier)
            .with_feature_flags(flags_on(workspace.path()));

        let outcome = handler.on_stop(&execution_id).await;
        assert!(
            matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
            "{outcome:?}"
        );
        match db.get_work_item(&revision_id).unwrap() {
            WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::InReview),
            other => panic!("expected task, got {other:?}"),
        }
        assert!(
            !db.list_attention_items(&execution_id)
                .unwrap()
                .iter()
                .any(|item| item.kind == REVISION_NO_OP_ATTENTION_KIND)
        );
    }

    /// Verifier whose head lookups park until released, holding one Stop inside
    /// `on_stop_inner`; everything else delegates to the inner stub.
    struct GatedHeadVerifier {
        inner: Arc<StubBranchVerifier>,
        entered: std::sync::atomic::AtomicUsize,
        release: tokio::sync::Notify,
    }

    #[async_trait]
    impl BranchVerifier for GatedHeadVerifier {
        async fn fetch_pr_head_ref(&self, repo_slug: &str, pr_number: u64) -> Result<String> {
            self.inner.fetch_pr_head_ref(repo_slug, pr_number).await
        }

        async fn fetch_pr_head_oid(&self, repo_slug: &str, pr_number: u64) -> Result<String> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.release.notified().await;
            self.inner.fetch_pr_head_oid(repo_slug, pr_number).await
        }

        async fn fetch_pr_head_oid_fresh(&self, repo_slug: &str, pr_number: u64) -> Result<String> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.release.notified().await;
            self.inner.fetch_pr_head_oid_fresh(repo_slug, pr_number).await
        }

        async fn fetch_diff_line_count(&self, repo_slug: &str, base: &str, head: &str) -> Result<u64> {
            self.inner.fetch_diff_line_count(repo_slug, base, head).await
        }

        async fn fetch_pr_base_ref(&self, repo_slug: &str, pr_number: u64) -> Result<String> {
            self.inner.fetch_pr_base_ref(repo_slug, pr_number).await
        }

        async fn fetch_diff_signature(&self, repo_slug: &str, base: &str, head: &str) -> Result<String> {
            self.inner.fetch_diff_signature(repo_slug, base, head).await
        }

        async fn fetch_pr_title_and_body(&self, repo_slug: &str, pr_number: u64) -> Result<(String, String)> {
            self.inner.fetch_pr_title_and_body(repo_slug, pr_number).await
        }
    }

    /// Two overlapping Stops for one execution: the second must wait for the
    /// first, so exactly one decision acts on the declaration, and the lock-map
    /// entry is dropped once both finish.
    #[tokio::test]
    async fn overlapping_stops_for_one_execution_are_serialized() {
        let workspace = tempdir().unwrap();
        let pr = "https://github.com/spinyfin/mono/pull/2082";
        let (_dir, db, _product_id, _revision_id, execution_id) = revision_fixture(workspace.path(), pr, "unchanged-head");
        declare_no_changes_needed(&db, &execution_id);

        let inner = StubBranchVerifier::ok("boss/exec_parent");
        inner
            .set_head_oid(Err("error connecting to api.github.com".into()))
            .await;
        let gated = Arc::new(GatedHeadVerifier {
            inner,
            entered: std::sync::atomic::AtomicUsize::new(0),
            release: tokio::sync::Notify::new(),
        });
        let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
        let handler = Arc::new(
            handler
                .with_branch_verifier(gated.clone())
                .with_feature_flags(flags_on(workspace.path())),
        );

        let first = tokio::spawn({
            let handler = handler.clone();
            let id = execution_id.clone();
            async move { handler.on_stop(&id).await }
        });
        while gated.entered.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        let weak_lock = handler
            .stop_locks
            .lock()
            .unwrap()
            .get(&execution_id)
            .cloned()
            .expect("first Stop registered a lock entry");
        let second = tokio::spawn({
            let handler = handler.clone();
            let id = execution_id.clone();
            async move { handler.on_stop(&id).await }
        });
        // Give the second Stop every chance to run past the lock; it must not
        // reach the verifier while the first holds the boundary.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            gated.entered.load(Ordering::SeqCst),
            1,
            "second Stop ran inside the first"
        );
        assert!(!second.is_finished());

        gated.release.notify_one();
        let first = first.await.unwrap();
        let second = second.await.unwrap();
        let no_ops = [&first, &second]
            .into_iter()
            .filter(|outcome| matches!(outcome, StopOutcome::NoChangesNeeded { .. }))
            .count();
        assert_eq!(no_ops, 1, "{first:?} / {second:?}");
        assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
        assert_eq!(
            db.list_attention_items(&execution_id)
                .unwrap()
                .iter()
                .filter(|item| item.kind == REVISION_NO_OP_ATTENTION_KIND)
                .count(),
            1
        );
        assert!(weak_lock.upgrade().is_none(), "stop lock must be released");
        // The next Stop (any execution) sweeps the expired entry.
        let _ = handler.on_stop("unrelated-execution").await;
        assert!(
            !handler.stop_locks.lock().unwrap().contains_key(&execution_id),
            "expired stop lock entry must be swept by the next Stop"
        );
    }
}
