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

/// A publish command whose output contains the origin PR
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

/// The `run_done` path rejects a staged URL whose head
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

/// When no output URL validates, the PR is resolved by
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

/// Stop path: A follow-up citing a merged origin PR ends
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

/// `run_done` path: The same follow-up, declaring
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
/// reused without another head-ref fetch on the `run_done` path; lifecycle
/// state is checked separately.
#[tokio::test]
async fn run_done_delivered_reuses_cached_branch_verification() {
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

/// A PR already bound to a different work item is refused,
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
            .any(|item| item.kind == crate::pr_binding_guard::FOREIGN_PR_REFUSED_ATTENTION_KIND),
        "the refusal is surfaced as an attention item",
    );
}

/// A `delivered` declaration parked by a GitHub outage must not stay parked
/// quietly once GitHub answers "no PR on the branch": the recheck re-enters
/// the declaration path and the run fails visibly.
#[tokio::test]
async fn transient_run_done_that_later_resolves_to_no_pr_fails_visibly_on_recheck() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::err("gh: connect: network is unreachable");
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), detector.clone());
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET run_done_outcome = 'delivered', run_done_declared_at = '2026-10-13T00:00:00Z' \
             WHERE id = ?1",
            rusqlite::params![execution_id],
        )
        .unwrap();

    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert_eq!(outcome, StopOutcome::DetectorFailed);
    assert!(db.get_execution(&execution_id).unwrap().status.is_live());

    detector.set_result(PrStatus::None).await;
    let outcome = handler.recheck_for_pr(&execution_id).await;
    let StopOutcome::RunDoneDeclaredWithoutDelivery { detail } = outcome else {
        panic!("a definitive no-PR answer must fail visibly, got {outcome:?}");
    };
    assert!(
        detail.contains(&own_branch(&execution_id)),
        "the branch looked for is named: {detail}"
    );
    let task = task_row(&db, &chore_id);
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(task.blocked_reason.as_deref(), Some("worker_failed"));
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
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
        use boss_observation_contracts::OpenPrMergeability;

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
        let (_dir, db, _product_id, _revision_id, execution_id) =
            revision_fixture(workspace.path(), pr, "unchanged-head");
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

#[tokio::test]
async fn staged_own_pr_lifecycle_controls_declared_delivery() {
    for state in [
        crate::work::PrOpenState::Merged,
        crate::work::PrOpenState::ClosedUnmerged,
    ] {
        let workspace = tempdir().unwrap();
        let (_dir, db, _, task_id, execution_id) = fixture(workspace.path());
        let staged = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
        staged.record_command_observation(&execution_id, OWN_PR, true);
        // Capture-time verification does not freeze the lifecycle state.
        staged.mark_branch_verified(&execution_id, OWN_PR);
        let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
        let handler = handler
            .with_staged_pr_urls(staged)
            .with_pr_state_checker(Arc::new(crate::work::FakePrStateChecker::always(state.clone())));
        let outcome = handler
            .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
            .await;
        if state == crate::work::PrOpenState::Merged {
            assert!(matches!(outcome, StopOutcome::PrMerged { .. }), "{outcome:?}");
            assert_eq!(task_row(&db, &task_id).status, TaskStatus::Done);
        } else {
            assert!(
                matches!(outcome, StopOutcome::RunDoneDeclaredWithoutDelivery { ref detail } if detail.contains("closed without merging")),
                "{outcome:?}"
            );
        }
    }
}

#[tokio::test]
async fn wrong_stored_pr_falls_back_to_own_branch_on_declared_delivery() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, task_id, execution_id) = fixture(workspace.path());
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?2 WHERE id = ?1",
            rusqlite::params![task_id, ORIGIN_PR],
        )
        .unwrap();
    let detector = StubPrDetector::ok(Some(OWN_PR));
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler.with_branch_verifier(two_pr_verifier(&execution_id));
    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == OWN_PR),
        "{outcome:?}"
    );
    assert_eq!(task_row(&db, &task_id).pr_url.as_deref(), Some(OWN_PR));
    assert_eq!(detector.call_count(), 1);
}

#[tokio::test]
async fn stored_pr_with_equivalent_branch_is_kept_without_detection() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, task_id, execution_id) = fixture(workspace.path());
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?2 WHERE id = ?1",
            rusqlite::params![task_id, OWN_PR],
        )
        .unwrap();
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler.with_branch_verifier(StubBranchVerifier::ok(&format!("custom/{execution_id}")));
    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == OWN_PR),
        "{outcome:?}"
    );
    assert_eq!(task_row(&db, &task_id).pr_url.as_deref(), Some(OWN_PR));
    assert_eq!(detector.call_count(), 0);
}

/// A predecessor execution on the same work item that already opened the PR
/// (the resume / bounce-back shape: the new execution is told to
/// `cube pr update` the predecessor's branch).
fn add_predecessor_execution(db: &WorkDb, task_id: &str) -> String {
    let predecessor = db
        .create_execution(
            boss_protocol::CreateExecutionInput::builder()
                .work_item_id(task_id)
                .kind(boss_protocol::ExecutionKind::ChoreImplementation)
                .status(boss_protocol::ExecutionStatus::Completed)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();
    predecessor.id
}

#[tokio::test]
async fn resumed_execution_delivering_the_predecessors_stored_pr_binds_it() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, task_id, execution_id) = fixture(workspace.path());
    let predecessor = add_predecessor_execution(&db, &task_id);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?2 WHERE id = ?1",
            rusqlite::params![task_id, OWN_PR],
        )
        .unwrap();
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler.with_branch_verifier(StubBranchVerifier::ok(&own_branch(&predecessor)));
    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == OWN_PR),
        "{outcome:?}"
    );
    assert_eq!(task_row(&db, &task_id).pr_url.as_deref(), Some(OWN_PR));
    assert_eq!(detector.call_count(), 0);
}

#[tokio::test]
async fn resumed_execution_delivering_a_staged_cube_pr_update_url_binds_it() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, task_id, execution_id) = fixture(workspace.path());
    let predecessor = add_predecessor_execution(&db, &task_id);
    let staged = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged.record_command_observation(&execution_id, OWN_PR, true);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let handler = handler
        .with_staged_pr_urls(staged)
        .with_branch_verifier(StubBranchVerifier::ok(&own_branch(&predecessor)));
    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == OWN_PR),
        "{outcome:?}"
    );
    assert_eq!(task_row(&db, &task_id).pr_url.as_deref(), Some(OWN_PR));
}

#[tokio::test]
async fn staged_current_execution_pr_wins_over_an_earlier_predecessor_pr() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, task_id, execution_id) = fixture(workspace.path());
    let predecessor = add_predecessor_execution(&db, &task_id);
    let staged = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged.record_command_observation(&execution_id, ORIGIN_PR, true);
    staged.record_command_observation(&execution_id, OWN_PR, true);
    let verifier = StubBranchVerifier::ok(&own_branch(&predecessor));
    verifier.set_head_ref_for(1764, &own_branch(&execution_id));
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let handler = handler.with_staged_pr_urls(staged).with_branch_verifier(verifier);
    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == OWN_PR),
        "{outcome:?}"
    );
    assert_eq!(task_row(&db, &task_id).pr_url.as_deref(), Some(OWN_PR));
}

#[tokio::test]
async fn resumed_execution_still_rejects_a_foreign_head() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, task_id, execution_id) = fixture(workspace.path());
    add_predecessor_execution(&db, &task_id);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?2 WHERE id = ?1",
            rusqlite::params![task_id, ORIGIN_PR],
        )
        .unwrap();
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let handler = handler.with_branch_verifier(StubBranchVerifier::ok(ORIGIN_BRANCH));
    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(
        matches!(outcome, StopOutcome::RunDoneDeclaredWithoutDelivery { .. }),
        "{outcome:?}"
    );
}

/// Capture-time verification stamps the verified URL onto the live slot so
/// the Agents pane header can show the PR while the worker is still running.
#[tokio::test]
async fn verify_staged_pr_url_after_capture_stamps_verified_url_onto_live_slot() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_command_observation(&execution_id, OWN_PR, true);
    let verifier = StubBranchVerifier::ok(&own_branch(&execution_id));

    let live_states = Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
    live_states.register_spawn(1, &execution_id, "claude-opus-5", std::process::id() as i32, None);

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls)
        .with_live_worker_states(live_states.clone())
        .with_branch_verifier(verifier);

    handler.verify_staged_pr_url_after_capture(&execution_id, false).await;
    assert_eq!(live_states.get(1).unwrap().pr_url.as_deref(), Some(OWN_PR));
}

/// A staged URL whose PR head is some other branch is not the worker's PR;
/// the live slot stays unset so the header does not flash a foreign link.
#[tokio::test]
async fn verify_staged_pr_url_after_capture_leaves_live_slot_unset_on_branch_mismatch() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_command_observation(&execution_id, ORIGIN_PR, true);
    let verifier = StubBranchVerifier::ok(ORIGIN_BRANCH);

    let live_states = Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
    live_states.register_spawn(1, &execution_id, "claude-opus-5", std::process::id() as i32, None);

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls)
        .with_live_worker_states(live_states.clone())
        .with_branch_verifier(verifier);

    handler.verify_staged_pr_url_after_capture(&execution_id, false).await;
    assert_eq!(live_states.get(1).unwrap().pr_url, None);
}

/// An unverified candidate (GitHub unreachable) must not stamp the live
/// slot; the header waits for a later successful verification.
#[tokio::test]
async fn verify_staged_pr_url_after_capture_leaves_live_slot_unset_when_unverified() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_command_observation(&execution_id, OWN_PR, true);

    let live_states = Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
    live_states.register_spawn(1, &execution_id, "claude-opus-5", std::process::id() as i32, None);

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls)
        .with_live_worker_states(live_states.clone())
        .with_branch_verifier(StubBranchVerifier::err("gh: connect: network is unreachable"));

    handler.verify_staged_pr_url_after_capture(&execution_id, false).await;
    assert_eq!(live_states.get(1).unwrap().pr_url, None);
}

#[tokio::test(start_paused = true)]
async fn base_refusal_blocks_delivery_and_only_explicit_refusal_files_attention() {
    use crate::work::{FakePrStateChecker, PrBaseVerdict, PrOpenState};
    for declared in [false, true] {
        let workspace = tempdir().unwrap();
        let (_dir, db, _, chore_id, execution_id) = followup_fixture(workspace.path());
        let handler = TestHarness::new(db.clone(), StubPrDetector::ok(Some(OWN_PR)))
            .handler
            .with_branch_verifier(two_pr_verifier(&execution_id))
            .with_pr_state_checker(Arc::new(
                FakePrStateChecker::always(PrOpenState::Open).with_base(PrBaseVerdict::Refused("feature base".into())),
            ));
        let outcome = if declared {
            handler
                .finalize_declared_delivery(&db.get_execution(&execution_id).unwrap())
                .await
        } else {
            handler
                .finalize_pr_transition(&execution_id, OWN_PR.into(), WorkerPrCompletionTarget::InReview, "test")
                .await
        };
        assert!(matches!(outcome, StopOutcome::PrBaseRefused { .. }), "{outcome:?}");
        assert!(task_row(&db, &chore_id).pr_url.is_none());
        assert_eq!(
            db.list_attention_items(&execution_id)
                .unwrap()
                .iter()
                .filter(|item| item.kind == boss_engine_storage_values::attention_kinds::PR_BASE_REFUSED)
                .count(),
            1
        );
        let open_refusals = |db: &WorkDb| {
            db.list_attention_items(&execution_id)
                .unwrap()
                .iter()
                .filter(|item| {
                    item.kind == boss_engine_storage_values::attention_kinds::PR_BASE_REFUSED && item.status == "open"
                })
                .count()
        };
        let refusal_id = db
            .list_attention_items(&execution_id)
            .unwrap()
            .into_iter()
            .find(|item| item.kind == boss_engine_storage_values::attention_kinds::PR_BASE_REFUSED)
            .unwrap()
            .id;
        let execution = db.get_execution(&execution_id).unwrap();
        // Concurrent retries must retain one open row.
        let (first, second) = tokio::join!(
            handler.guard_pr_base(&execution, OWN_PR),
            handler.guard_pr_base(&execution, OWN_PR),
        );
        assert!(matches!(first, Some(StopOutcome::PrBaseRefused { .. })));
        assert!(matches!(second, Some(StopOutcome::PrBaseRefused { .. })));
        assert_eq!(open_refusals(&db), 1);
        let assert_original_open = || {
            let items = db.list_attention_items(&execution_id).unwrap();
            let item = items.iter().find(|item| item.id == refusal_id).unwrap();
            assert_eq!(item.status, "open");
            assert_eq!(open_refusals(&db), 1);
        };
        // NotChecked does not resolve it.
        let handler = handler.with_pr_state_checker(Arc::new(
            FakePrStateChecker::always(PrOpenState::Open).with_base(PrBaseVerdict::NotChecked),
        ));
        assert!(
            handler
                .guard_pr_base(&db.get_execution(&execution_id).unwrap(), OWN_PR)
                .await
                .is_none()
        );
        assert_eq!(open_refusals(&db), 1);
        assert_original_open();
        let handler = handler.with_pr_state_checker(Arc::new(UnavailableBaseChecker));
        assert!(matches!(
            handler.guard_pr_base(&execution, OWN_PR).await,
            Some(StopOutcome::DetectorFailed)
        ));
        assert_original_open();
        let (release, wait) = std::sync::mpsc::channel();
        let handler = handler.with_pr_state_checker(Arc::new(WaitingBaseChecker(std::sync::Mutex::new(wait))));
        let (outcome, ()) = tokio::join!(handler.guard_pr_base(&execution, OWN_PR), async {
            tokio::task::yield_now().await;
            tokio::time::advance(
                crate::run_done_declaration::DECLARED_DELIVERY_NETWORK_BUDGET + std::time::Duration::from_secs(1),
            )
            .await;
        });
        drop(release);
        assert!(matches!(outcome, Some(StopOutcome::DetectorFailed)));
        assert_original_open();
        let handler = handler.with_pr_state_checker(Arc::new(
            FakePrStateChecker::always(PrOpenState::Open).with_base(PrBaseVerdict::DefaultBranch),
        ));
        assert!(
            handler
                .guard_pr_base(&db.get_execution(&execution_id).unwrap(), OWN_PR)
                .await
                .is_none()
        );
        assert_eq!(open_refusals(&db), 0);
        let items = db.list_attention_items(&execution_id).unwrap();
        assert_ne!(items.iter().find(|item| item.id == refusal_id).unwrap().status, "open");
    }
}

struct UnavailableBaseChecker;
impl crate::work::PrStateChecker for UnavailableBaseChecker {
    fn check(&self, _: &str) -> anyhow::Result<crate::work::PrOpenState> {
        Ok(crate::work::PrOpenState::Open)
    }
    fn base_verdict(&self, _: &str) -> anyhow::Result<crate::work::PrBaseVerdict> {
        anyhow::bail!("GitHub unavailable")
    }
}

#[tokio::test]
async fn base_lookup_error_leaves_delivery_retryable_without_attention() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, execution_id) = followup_fixture(workspace.path());
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(Some(OWN_PR)))
        .handler
        .with_pr_state_checker(Arc::new(UnavailableBaseChecker));
    let execution = db.get_execution(&execution_id).unwrap();
    assert!(matches!(
        handler.guard_pr_base(&execution, OWN_PR).await,
        Some(StopOutcome::DetectorFailed)
    ));
    assert_eq!(db.get_execution(&execution_id).unwrap().status, execution.status);
    assert!(db.list_attention_items(&execution_id).unwrap().is_empty());
    assert!(task_row(&db, &chore_id).pr_url.is_none());
}

struct WaitingBaseChecker(std::sync::Mutex<std::sync::mpsc::Receiver<()>>);
impl crate::work::PrStateChecker for WaitingBaseChecker {
    fn check(&self, _: &str) -> anyhow::Result<crate::work::PrOpenState> {
        Ok(crate::work::PrOpenState::Open)
    }
    fn base_verdict(&self, _: &str) -> anyhow::Result<crate::work::PrBaseVerdict> {
        let _ = self.0.lock().unwrap().recv();
        Ok(crate::work::PrBaseVerdict::DefaultBranch)
    }
}

#[tokio::test(start_paused = true)]
async fn base_lookup_timeout_leaves_delivery_retryable_without_attention() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, execution_id) = followup_fixture(workspace.path());
    let (release, wait) = std::sync::mpsc::channel();
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(Some(OWN_PR)))
        .handler
        .with_pr_state_checker(Arc::new(WaitingBaseChecker(std::sync::Mutex::new(wait))));
    let execution = db.get_execution(&execution_id).unwrap();
    let (outcome, ()) = tokio::join!(handler.guard_pr_base(&execution, OWN_PR), async {
        tokio::task::yield_now().await;
        tokio::time::advance(
            crate::run_done_declaration::DECLARED_DELIVERY_NETWORK_BUDGET + std::time::Duration::from_secs(1),
        )
        .await;
    });
    drop(release);
    assert!(matches!(outcome, Some(StopOutcome::DetectorFailed)));
    assert_eq!(db.get_execution(&execution_id).unwrap().status, execution.status);
    assert!(db.list_attention_items(&execution_id).unwrap().is_empty());
}

#[tokio::test]
async fn declared_state_resolution_distinguishes_base_refusal_from_network_failure() {
    use crate::run_done_declaration::DeclaredPrResolution;
    use crate::work::{FakePrStateChecker, PrBaseVerdict, PrOpenState};
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, execution_id) = followup_fixture(workspace.path());
    let execution = db.get_execution(&execution_id).unwrap();
    let handler = TestHarness::new(db, StubPrDetector::ok(Some(OWN_PR)))
        .handler
        .with_pr_state_checker(Arc::new(
            FakePrStateChecker::always(PrOpenState::Open).with_base(PrBaseVerdict::Refused("feature/parent".into())),
        ));
    assert!(matches!(
        handler.resolve_verified_pr_state(&execution, OWN_PR.into()).await,
        DeclaredPrResolution::BaseRefused { pr_url, reason }
            if pr_url == OWN_PR && reason == "feature/parent"
    ));
    let handler = handler.with_pr_state_checker(Arc::new(
        FakePrStateChecker::always(PrOpenState::Merged).with_base(PrBaseVerdict::Refused("feature/parent".into())),
    ));
    assert!(matches!(
        handler.resolve_verified_pr_state(&execution, OWN_PR.into()).await,
        DeclaredPrResolution::Bound { merged: true, .. }
    ));
    let handler = handler.with_pr_state_checker(Arc::new(UnavailableBaseChecker));
    assert!(matches!(
        handler.resolve_verified_pr_state(&execution, OWN_PR.into()).await,
        DeclaredPrResolution::Transient { detail } if detail.contains("GitHub unavailable")
    ));
}

#[tokio::test]
async fn late_recovery_checks_base_for_fresh_and_merged_prs() {
    use crate::work::{FakePrStateChecker, PrBaseVerdict, PrOpenState};
    for merged in [false, true] {
        for transient in [false, true] {
            let (_dir, db, _, work_item_id, execution_id) = abandoned_execution_fixture();
            let detected = if merged {
                PrStatus::Merged { url: OWN_PR.into() }
            } else {
                PrStatus::Fresh { url: OWN_PR.into() }
            };
            let checker: Arc<dyn crate::work::PrStateChecker> = if transient {
                Arc::new(UnavailableBaseChecker)
            } else {
                Arc::new(
                    FakePrStateChecker::always(PrOpenState::Open)
                        .with_base(PrBaseVerdict::Refused("feature/parent".into())),
                )
            };
            let handler = TestHarness::new(db.clone(), StubPrDetector::ok_status(detected))
                .handler
                .with_pr_state_checker(checker);
            let candidate = crate::work::LatePrCandidate {
                execution_id: execution_id.clone(),
                work_item_id: work_item_id.clone(),
                repo_remote_url: "git@github.com:spinyfin/mono.git".into(),
                branch_naming: BranchNaming::BossExecPrefix,
                worker_branch_prefix: None,
            };
            let outcome = handler.recheck_for_pr_late(&candidate).await;
            let WorkItem::Chore(task) = db.get_work_item(&work_item_id).unwrap() else {
                panic!("expected chore")
            };
            let open = db
                .list_attention_items_for_work_item(&work_item_id)
                .unwrap()
                .into_iter()
                .filter(|item| {
                    item.kind == boss_engine_storage_values::attention_kinds::PR_BASE_REFUSED && item.status == "open"
                })
                .count();
            if merged {
                // Merged PRs cannot be retargeted, so late recovery binds them
                // without consulting the base policy.
                assert!(matches!(outcome, StopOutcome::PrDetected { .. }), "{outcome:?}");
                assert_eq!(task.pr_url.as_deref(), Some(OWN_PR));
                assert_eq!(open, 0);
            } else if transient {
                assert_eq!(task.status, TaskStatus::Active);
                assert!(matches!(outcome, StopOutcome::DetectorFailed), "{outcome:?}");
                assert!(task.pr_url.is_none());
                assert_eq!(open, 0);
            } else {
                assert_eq!(task.status, TaskStatus::Active);
                assert!(matches!(outcome, StopOutcome::PrBaseRefused { .. }), "{outcome:?}");
                assert!(task.pr_url.is_none());
                assert_eq!(open, 1);
                let handler = handler.with_pr_state_checker(Arc::new(
                    FakePrStateChecker::always(PrOpenState::Open).with_base(PrBaseVerdict::DefaultBranch),
                ));
                assert!(matches!(
                    handler.recheck_for_pr_late(&candidate).await,
                    StopOutcome::PrDetected { .. }
                ));
                assert!(
                    db.list_attention_items_for_work_item(&work_item_id)
                        .unwrap()
                        .iter()
                        .all(|item| item.status != "open")
                );
            }
        }
    }
}

#[tokio::test]
async fn late_recovery_dedupes_an_identical_open_base_refusal() {
    use crate::work::{FakePrStateChecker, PrBaseVerdict, PrOpenState};
    let (_dir, db, _, work_item_id, execution_id) = abandoned_execution_fixture();
    let TestHarness { handler, publisher, .. } = TestHarness::new(
        db.clone(),
        StubPrDetector::ok_status(PrStatus::Fresh { url: OWN_PR.into() }),
    );
    let handler = handler.with_pr_state_checker(Arc::new(
        FakePrStateChecker::always(PrOpenState::Open).with_base(PrBaseVerdict::Refused("feature/parent".into())),
    ));
    let candidate = crate::work::LatePrCandidate {
        execution_id: execution_id.clone(),
        work_item_id: work_item_id.clone(),
        repo_remote_url: "git@github.com:spinyfin/mono.git".into(),
        branch_naming: BranchNaming::BossExecPrefix,
        worker_branch_prefix: None,
    };
    assert!(matches!(
        handler.recheck_for_pr_late(&candidate).await,
        StopOutcome::PrBaseRefused { .. }
    ));
    // `upsert_work_item_attention` reuses the open row and stamps
    // `last_raised_at` without inserting. A skip that still re-raises
    // would keep the row count at one; a frozen sentinel is what
    // proves the second pass did not write.
    const LAST_RAISED_SENTINEL: &str = "1";
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_attention_items SET last_raised_at = ?1
             WHERE work_item_id = ?2 AND kind = ?3 AND status = 'open'",
            rusqlite::params![
                LAST_RAISED_SENTINEL,
                work_item_id,
                boss_engine_storage_values::attention_kinds::PR_BASE_REFUSED
            ],
        )
        .unwrap();
    assert!(matches!(
        handler.recheck_for_pr_late(&candidate).await,
        StopOutcome::PrBaseRefused { .. }
    ));
    let last_raised_at: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT last_raised_at FROM work_attention_items
             WHERE work_item_id = ?1 AND kind = ?2 AND status = 'open'",
            rusqlite::params![
                work_item_id,
                boss_engine_storage_values::attention_kinds::PR_BASE_REFUSED
            ],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        last_raised_at, LAST_RAISED_SENTINEL,
        "an identical open refusal must not re-stamp last_raised_at"
    );
    let open: Vec<_> = db
        .list_attention_items_for_work_item(&work_item_id)
        .unwrap()
        .into_iter()
        .filter(|item| {
            item.kind == boss_engine_storage_values::attention_kinds::PR_BASE_REFUSED && item.status == "open"
        })
        .collect();
    assert_eq!(open.len(), 1);
    let raised: i64 = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM work_attention_items WHERE work_item_id = ?1 AND kind = ?2",
            rusqlite::params![
                work_item_id,
                boss_engine_storage_values::attention_kinds::PR_BASE_REFUSED
            ],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(raised, 1, "an identical open refusal must not insert a second row");
    let refusals = publisher
        .events
        .lock()
        .await
        .iter()
        .filter(|(_, id, reason)| {
            id == &work_item_id && reason == boss_engine_storage_values::attention_kinds::PR_BASE_REFUSED
        })
        .count();
    assert_eq!(refusals, 1, "an identical open refusal must publish once");
}

struct CountingBaseChecker(std::sync::atomic::AtomicUsize);
impl crate::work::PrStateChecker for CountingBaseChecker {
    fn check(&self, _: &str) -> anyhow::Result<crate::work::PrOpenState> {
        Ok(crate::work::PrOpenState::Open)
    }
    fn base_verdict(&self, _: &str) -> anyhow::Result<crate::work::PrBaseVerdict> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(crate::work::PrBaseVerdict::DefaultBranch)
    }
}

#[tokio::test]
async fn declared_delivery_checks_base_once_for_detected_and_stored_urls() {
    for stored in [false, true] {
        let workspace = tempdir().unwrap();
        let (_dir, db, _, chore_id, execution_id) = followup_fixture(workspace.path());
        if stored {
            db.update_work_item(
                &chore_id,
                crate::work::WorkItemPatch {
                    pr_url: Some(OWN_PR.into()),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let checker = Arc::new(CountingBaseChecker(std::sync::atomic::AtomicUsize::new(0)));
        let handler = TestHarness::new(db.clone(), StubPrDetector::ok(Some(OWN_PR)))
            .handler
            .with_branch_verifier(two_pr_verifier(&execution_id))
            .with_pr_state_checker(checker.clone());
        handler
            .finalize_declared_delivery(&db.get_execution(&execution_id).unwrap())
            .await;
        // Already-bound PRs skip the default-branch policy; a new bind still
        // checks exactly once.
        assert_eq!(
            checker.0.load(std::sync::atomic::Ordering::SeqCst),
            usize::from(!stored)
        );
    }
}

/// A confirmed refusal is a parked outcome, not a retryable lookup failure:
/// `finalize_resolved` stops the hot retry loop and no stuck-finalize
/// attention is raised for it.
#[test]
fn base_refusal_is_not_retried_hot() {
    let refused = StopOutcome::PrBaseRefused {
        pr_url: OWN_PR.into(),
        detail: "feature base".into(),
    };
    assert!(crate::pending_finalize::finalize_resolved(&refused));
    assert!(!crate::pending_finalize::finalize_resolved(
        &StopOutcome::DetectorFailed
    ));
}

/// A merged PR can no longer be retargeted, so a `Done` completion on a
/// non-default base is not refused.
#[tokio::test]
async fn merged_pr_on_non_default_base_is_not_refused() {
    use crate::work::{FakePrStateChecker, PrBaseVerdict, PrOpenState};
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, execution_id) = followup_fixture(workspace.path());
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(Some(OWN_PR)))
        .handler
        .with_branch_verifier(two_pr_verifier(&execution_id))
        .with_pr_state_checker(Arc::new(
            FakePrStateChecker::always(PrOpenState::Merged).with_base(PrBaseVerdict::Refused("feature base".into())),
        ));
    let outcome = handler
        .finalize_pr_transition(&execution_id, OWN_PR.into(), WorkerPrCompletionTarget::Done, "test")
        .await;
    assert!(!matches!(outcome, StopOutcome::PrBaseRefused { .. }), "{outcome:?}");
    assert!(
        db.list_attention_items(&execution_id)
            .unwrap()
            .iter()
            .all(|item| item.kind != boss_engine_storage_values::attention_kinds::PR_BASE_REFUSED)
    );
}

/// The base policy applies to a PR the execution is about to bind; a PR the
/// work item already holds (a CI-fix or conflict-fix of a stacked PR) is not
/// re-checked.
#[tokio::test]
async fn already_bound_pr_skips_the_base_guard() {
    use crate::work::{FakePrStateChecker, PrBaseVerdict, PrOpenState};
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, execution_id) = followup_fixture(workspace.path());
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?2 WHERE id = ?1",
            rusqlite::params![chore_id, OWN_PR],
        )
        .unwrap();
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(Some(OWN_PR)))
        .handler
        .with_pr_state_checker(Arc::new(
            FakePrStateChecker::always(PrOpenState::Open).with_base(PrBaseVerdict::Refused("feature base".into())),
        ));
    let execution = db.get_execution(&execution_id).unwrap();
    assert!(handler.guard_pr_base(&execution, OWN_PR).await.is_none());
    assert!(db.list_attention_items(&execution_id).unwrap().is_empty());
}

/// A revision's deliverable is the chain root's PR; the default-branch
/// policy does not apply even when GitHub reports a non-default base.
#[tokio::test]
async fn revision_implementation_skips_the_base_guard() {
    use crate::work::{FakePrStateChecker, PrBaseVerdict, PrOpenState};
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _revision_id, execution_id) =
        revision_fixture(workspace.path(), OWN_PR, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(Some(OWN_PR)))
        .handler
        .with_pr_state_checker(Arc::new(
            FakePrStateChecker::always(PrOpenState::Open).with_base(PrBaseVerdict::Refused("feature base".into())),
        ));
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.kind, ExecutionKind::RevisionImplementation);
    assert!(handler.guard_pr_base(&execution, OWN_PR).await.is_none());
    assert!(db.list_attention_items(&execution_id).unwrap().is_empty());
}

/// A merged PR can no longer be retargeted, so a declared delivery skips the
/// base policy rather than parking the run.
#[tokio::test]
async fn finalize_declared_delivery_skips_base_guard_for_merged_pr() {
    use crate::work::{FakePrStateChecker, PrBaseVerdict, PrOpenState};
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, execution_id) = followup_fixture(workspace.path());
    let handler = TestHarness::new(
        db.clone(),
        StubPrDetector::ok_status(PrStatus::Merged { url: OWN_PR.into() }),
    )
    .handler
    .with_pr_state_checker(Arc::new(
        FakePrStateChecker::always(PrOpenState::Merged).with_base(PrBaseVerdict::Refused("feature base".into())),
    ));
    let outcome = handler
        .finalize_declared_delivery(&db.get_execution(&execution_id).unwrap())
        .await;
    assert!(!matches!(outcome, StopOutcome::PrBaseRefused { .. }), "{outcome:?}");
    assert!(
        db.list_attention_items(&execution_id)
            .unwrap()
            .iter()
            .all(|item| item.kind != boss_engine_storage_values::attention_kinds::PR_BASE_REFUSED)
    );
}

struct RecordingBaseChecker {
    inner: crate::work::FakePrStateChecker,
    calls: std::sync::atomic::AtomicUsize,
    merged: std::sync::atomic::AtomicBool,
}

impl crate::work::PrStateChecker for RecordingBaseChecker {
    fn check(&self, pr_url: &str) -> anyhow::Result<crate::work::PrOpenState> {
        if self.merged.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(crate::work::PrOpenState::Merged);
        }
        self.inner.check(pr_url)
    }
    fn base_verdict(&self, pr_url: &str) -> anyhow::Result<crate::work::PrBaseVerdict> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.base_verdict(pr_url)
    }
}

fn flags_on_run_done(workspace: &Path) -> Arc<crate::feature_flags::FeatureFlagsStore> {
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.join("feature-flags.toml"),
    ));
    flags.set("worker_proposals", true).unwrap();
    flags.set("run_done_proposals_seam", true).unwrap();
    flags
}

fn stamp_delivered(db: &WorkDb, execution_id: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET run_done_outcome = 'delivered', run_done_declared_at = '1' WHERE id = ?1",
            rusqlite::params![execution_id],
        )
        .unwrap();
}

const SUPERSEDE_CLOSED_PR: &str = "https://github.com/spinyfin/mono/pull/2927";
const SUPERSEDE_SUPERSEDING_PR: &str = "https://github.com/spinyfin/mono/pull/2977";

async fn ask_close_as_superseded(db: &Arc<WorkDb>, workspace: &Path, chore_id: &str, execution_id: &str, answer: bool) {
    use crate::work::SubmitWorkerProposalInput;
    let q_payload = serde_json::json!({
        "outcome": "blocked",
        "summary": "PR #2927 is superseded by #2977; asking to close.",
        "question": {
            "text": "Close PR #2927 as superseded by #2977?",
            "answer_type": { "kind": "yes_no" },
            "explanation": "Context:\nPR #2977 supersedes #2927.\nYes:\nClose #2927.\nNo:\nKeep #2927.\nRecommendation:\nYes."
        }
    })
    .to_string();
    let _ = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id,
            work_item_id: chore_id,
            kind: boss_protocol::ProposalKind::RunDone,
            payload_json: &q_payload,
            idempotency_key: "ask-supersede",
        })
        .unwrap();
    let ask_handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_feature_flags(flags_on_run_done(workspace));
    let _ = ask_handler
        .finalize_declared_run_done(execution_id, boss_protocol::RunDoneOutcome::Blocked)
        .await;
    db.answer_operator_question(chore_id, boss_protocol::OperatorAnswer::YesNo { value: answer })
        .unwrap();
}

async fn finalize_closed_superseded_delivery(
    db: Arc<WorkDb>,
    workspace: &Path,
    chore_id: &str,
    superseded_by: &str,
    detector: Arc<StubPrDetector>,
) -> (String, StopOutcome) {
    use crate::work::{FakePrStateChecker, PrOpenState, SubmitWorkerProposalInput};
    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.to_owned())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .repo_remote_url("https://github.com/spinyfin/mono.git")
                .pr_url(SUPERSEDE_CLOSED_PR)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET pr_head_before = 'deadbeef' WHERE id = ?1",
            [&exec.id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?2 WHERE id = ?1",
            [chore_id, SUPERSEDE_CLOSED_PR],
        )
        .unwrap();
    let done_payload = serde_json::json!({
        "outcome": "delivered",
        "summary": "Closed PR #2927 as superseded by #2977 per owner approval.",
        "superseded_by": superseded_by,
    })
    .to_string();
    let _ = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &exec.id,
            work_item_id: chore_id,
            kind: boss_protocol::ProposalKind::RunDone,
            payload_json: &done_payload,
            idempotency_key: "deliver-superseded",
        })
        .unwrap();
    let handler = TestHarness::new(db.clone(), detector)
        .handler
        .with_feature_flags(flags_on_run_done(workspace))
        .with_branch_verifier(StubBranchVerifier::ok(&own_branch(&exec.id)))
        .with_pr_state_checker(Arc::new(FakePrStateChecker::always(PrOpenState::ClosedUnmerged)));
    let outcome = handler
        .finalize_declared_run_done(&exec.id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    (exec.id, outcome)
}

/// A confirmed refusal stays pending on the durable sweep, but later ticks
/// within the park window do not re-query GitHub or re-raise the attention.
#[tokio::test]
async fn sweep_pending_finalizes_parks_a_confirmed_refusal_without_reprobing() {
    use crate::work::{FakePrStateChecker, PrBaseVerdict, PrOpenState};
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, execution_id) = followup_fixture(workspace.path());
    stamp_delivered(&db, &execution_id);
    let checker = Arc::new(RecordingBaseChecker {
        inner: FakePrStateChecker::always(PrOpenState::Open).with_base(PrBaseVerdict::Refused("feature base".into())),
        calls: std::sync::atomic::AtomicUsize::new(0),
        merged: std::sync::atomic::AtomicBool::new(false),
    });
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(Some(OWN_PR)))
        .handler
        .with_branch_verifier(two_pr_verifier(&execution_id))
        .with_pr_state_checker(checker.clone())
        .with_feature_flags(flags_on_run_done(workspace.path()));
    handler
        .staged_pr_urls
        .record_command_observation(&execution_id, OWN_PR, true);
    let first = handler.sweep_pending_finalizes(i64::MAX).await;
    assert_eq!(first.still_pending, 1, "{first:?}");
    assert_eq!(first.finalized, 0);
    assert_eq!(checker.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let raised_at: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT last_raised_at FROM work_attention_items
             WHERE execution_id = ?1 AND kind = ?2 AND status = 'open'",
            rusqlite::params![
                execution_id,
                boss_engine_storage_values::attention_kinds::PR_BASE_REFUSED
            ],
            |row| row.get(0),
        )
        .unwrap();
    for _ in 0..3 {
        assert_eq!(handler.recheck_for_pr(&execution_id).await, StopOutcome::AwaitingInput);
        let pass = handler.sweep_pending_finalizes(i64::MAX).await;
        assert_eq!(pass.still_pending, 1, "{pass:?}");
        assert_eq!(pass.finalized, 0);
    }
    assert_eq!(checker.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let raised_again: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT last_raised_at FROM work_attention_items
             WHERE execution_id = ?1 AND kind = ?2 AND status = 'open'",
            rusqlite::params![
                execution_id,
                boss_engine_storage_values::attention_kinds::PR_BASE_REFUSED
            ],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(raised_at, raised_again);
    handler
        .pr_base_refusal_next_check
        .lock()
        .unwrap()
        .insert(execution_id.clone(), std::time::Instant::now());
    let deadline = handler.pr_base_refusal_next_check.lock().unwrap()[&execution_id];
    assert_eq!(handler.recheck_for_pr(&execution_id).await, StopOutcome::AwaitingInput);
    assert_eq!(
        handler.pr_base_refusal_next_check.lock().unwrap()[&execution_id],
        deadline
    );
    checker.merged.store(true, std::sync::atomic::Ordering::SeqCst);
    let recovered = handler.sweep_pending_finalizes(i64::MAX).await;
    assert_eq!(recovered.finalized, 1, "{recovered:?}");
    assert_eq!(checker.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
    handler.sweep_pending_finalizes(i64::MAX).await;
    assert!(handler.pr_base_refusal_next_check.lock().unwrap().is_empty());
}

#[tokio::test]
async fn closed_conflict_delivery_clears_existing_base_refusal() {
    use crate::work::{FakePrStateChecker, PrBaseVerdict, PrOpenState};
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, _, execution_id, _) = conflict_revision_fixture(workspace.path(), OWN_PR, "head");
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_pr_state_checker(Arc::new(
            FakePrStateChecker::always(PrOpenState::ClosedUnmerged)
                .with_base(PrBaseVerdict::Refused("feature base".into())),
        ));
    let execution = db.get_execution(&execution_id).unwrap();
    let kind = boss_engine_storage_values::attention_kinds::PR_BASE_REFUSED;
    handler
        .upsert_execution_attention(&execution, kind, "Wrong base", "Retarget")
        .await
        .unwrap();
    let outcome = handler.finalize_declared_delivery(&execution).await;
    assert!(!matches!(outcome, StopOutcome::PrBaseRefused { .. }), "{outcome:?}");
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
    assert!(
        db.list_attention_items(&execution_id)
            .unwrap()
            .iter()
            .any(|item| item.kind == kind && item.status == "resolved")
    );
}

#[tokio::test]
async fn pending_finalize_prunes_cancelled_refusal() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, execution_id) = followup_fixture(workspace.path());
    stamp_delivered(&db, &execution_id);
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_feature_flags(flags_on_run_done(workspace.path()));
    handler.park_pr_base_refusal(&execution_id);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'cancelled' WHERE id = ?1",
            [&execution_id],
        )
        .unwrap();
    handler.sweep_pending_finalizes(i64::MAX).await;
    assert!(handler.pr_base_refusal_next_check.lock().unwrap().is_empty());
}

#[tokio::test]
async fn approved_superseded_closed_pr_records_succeeded_with_superseding_pr_and_no_failure_metrics() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, first_exec_id) = followup_fixture(workspace.path());
    ask_close_as_superseded(&db, workspace.path(), &chore_id, &first_exec_id, true).await;
    let answered = db.list_operator_questions(&chore_id).unwrap();
    assert!(
        answered
            .iter()
            .any(|q| matches!(q.answer, Some(boss_protocol::OperatorAnswer::YesNo { value: true }))),
        "approved path must record a Yes answer the engine can read, got {answered:?}"
    );

    let (exec_id, outcome) = finalize_closed_superseded_delivery(
        db.clone(),
        workspace.path(),
        &chore_id,
        "#2977",
        StubPrDetector::ok(None),
    )
    .await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == SUPERSEDE_CLOSED_PR),
        "expected PrDetected for closed superseded PR, got {outcome:?}"
    );

    let finished = db.get_execution(&exec_id).unwrap();
    assert_eq!(finished.status, ExecutionStatus::Completed);
    assert_eq!(
        finished.superseded_by_pr.as_deref(),
        Some(SUPERSEDE_SUPERSEDING_PR),
        "short superseded_by form must be stored as a full PR URL in this repo"
    );

    let crate::work::WorkItem::Chore(task) = db.get_work_item(&chore_id).unwrap() else {
        panic!("expected chore work item");
    };
    assert_eq!(task.status, TaskStatus::Done);
    assert_eq!(task.blocked_reason, None);
    assert!(db.get_merge_evidence(&chore_id).unwrap().is_none());
    assert!(
        db.list_attention_items(&exec_id)
            .unwrap()
            .iter()
            .all(|item| item.kind != crate::RUN_DONE_AUDIT_FLAGGED_ATTENTION_KIND),
        "superseded completion must not file a delivery audit attention item"
    );

    let report = db.stuck_daily_snapshot(1_700_000_000, &|_| false).unwrap();
    assert!(
        !report
            .rows
            .iter()
            .any(|e| e.work_item_id == chore_id && e.classification.contains("worker_failed")),
        "approved supersede-and-close must not appear in worker_failed report: {:?}",
        report.rows
    );
}

#[tokio::test]
async fn superseded_closed_pr_without_approval_still_fails() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, _) = followup_fixture(workspace.path());
    let (exec_id, outcome) = finalize_closed_superseded_delivery(
        db.clone(),
        workspace.path(),
        &chore_id,
        SUPERSEDE_SUPERSEDING_PR,
        StubPrDetector::ok(None),
    )
    .await;
    assert!(
        matches!(
            outcome,
            StopOutcome::RunDoneDeclaredWithoutDelivery { ref detail }
                if detail.contains("no recorded owner approval")
        ),
        "superseded_by without approval must fail, got {outcome:?}"
    );
    assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Failed);
    let crate::work::WorkItem::Chore(task) = db.get_work_item(&chore_id).unwrap() else {
        panic!("expected chore work item");
    };
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(task.blocked_reason.as_deref(), Some("worker_failed"));
}

#[tokio::test]
async fn superseded_closed_pr_with_declined_approval_still_fails() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, first_exec_id) = followup_fixture(workspace.path());
    ask_close_as_superseded(&db, workspace.path(), &chore_id, &first_exec_id, false).await;
    let (exec_id, outcome) = finalize_closed_superseded_delivery(
        db.clone(),
        workspace.path(),
        &chore_id,
        SUPERSEDE_SUPERSEDING_PR,
        StubPrDetector::ok(None),
    )
    .await;
    assert!(
        matches!(
            outcome,
            StopOutcome::RunDoneDeclaredWithoutDelivery { ref detail } if detail.contains("declined")
        ),
        "declined close must keep the run failed, got {outcome:?}"
    );
    assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Failed);
}

#[tokio::test]
async fn superseded_by_naming_the_closed_pr_is_rejected() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, first_exec_id) = followup_fixture(workspace.path());
    ask_close_as_superseded(&db, workspace.path(), &chore_id, &first_exec_id, true).await;
    let (exec_id, outcome) = finalize_closed_superseded_delivery(
        db.clone(),
        workspace.path(),
        &chore_id,
        SUPERSEDE_CLOSED_PR,
        StubPrDetector::ok(None),
    )
    .await;
    assert!(
        matches!(
            outcome,
            StopOutcome::RunDoneDeclaredWithoutDelivery { ref detail } if detail.contains("same as the closed PR")
        ),
        "superseded_by naming the closed PR must fail, got {outcome:?}"
    );
    assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Failed);
}

#[tokio::test]
async fn superseded_by_in_another_repo_is_rejected() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, first_exec_id) = followup_fixture(workspace.path());
    ask_close_as_superseded(&db, workspace.path(), &chore_id, &first_exec_id, true).await;
    let (exec_id, outcome) = finalize_closed_superseded_delivery(
        db.clone(),
        workspace.path(),
        &chore_id,
        "https://github.com/other/repo/pull/1",
        StubPrDetector::ok(None),
    )
    .await;
    assert!(
        matches!(
            outcome,
            StopOutcome::RunDoneDeclaredWithoutDelivery { ref detail } if detail.contains("does not match")
        ),
        "foreign-repo superseded_by must fail, got {outcome:?}"
    );
    assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Failed);
}

#[tokio::test]
async fn detector_closed_without_supersede_declaration_still_fails() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _chore_id, execution_id) = followup_fixture(workspace.path());
    stamp_delivered(&db, &execution_id);
    let detector = StubPrDetector::ok_status(PrStatus::Closed {
        url: SUPERSEDE_CLOSED_PR.to_owned(),
    });
    let handler = TestHarness::new(db.clone(), detector)
        .handler
        .with_feature_flags(flags_on_run_done(workspace.path()))
        .with_branch_verifier(StubBranchVerifier::ok(&own_branch(&execution_id)));
    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(
        matches!(
            outcome,
            StopOutcome::RunDoneDeclaredWithoutDelivery { ref detail }
                if detail.contains("closed without merging")
        ),
        "detector Closed without superseded_by must fail, got {outcome:?}"
    );
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
}

#[tokio::test]
async fn unexplained_closed_unmerged_pr_without_supersede_declaration_still_fails() {
    use crate::work::{FakePrStateChecker, PrOpenState};
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, execution_id) = followup_fixture(workspace.path());
    let closed_pr = "https://github.com/spinyfin/mono/pull/2927";
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET pr_url = ?2 WHERE id = ?1", [&chore_id, closed_pr])
        .unwrap();
    stamp_delivered(&db, &execution_id);

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_feature_flags(flags_on_run_done(workspace.path()))
        .with_branch_verifier(StubBranchVerifier::ok(&own_branch(&execution_id)))
        .with_pr_state_checker(Arc::new(FakePrStateChecker::always(PrOpenState::ClosedUnmerged)));

    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(
        matches!(outcome, StopOutcome::RunDoneDeclaredWithoutDelivery { .. }),
        "unexplained closed-unmerged PR must fail, got {outcome:?}"
    );
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
    let crate::work::WorkItem::Chore(task) = db.get_work_item(&chore_id).unwrap() else {
        panic!("expected chore work item");
    };
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(task.blocked_reason.as_deref(), Some("worker_failed"));
}

#[tokio::test]
async fn open_pr_with_superseded_declaration_is_refused() {
    use crate::work::{FakePrStateChecker, PrOpenState, SubmitWorkerProposalInput};
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, execution_id) = followup_fixture(workspace.path());
    let open_pr = "https://github.com/spinyfin/mono/pull/2927";
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET pr_url = ?2 WHERE id = ?1", [&chore_id, open_pr])
        .unwrap();
    let done_payload = serde_json::json!({
        "outcome": "delivered",
        "summary": "Claimed superseded while PR is still open",
        "superseded_by": "https://github.com/spinyfin/mono/pull/2977",
    })
    .to_string();
    let _ = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &execution_id,
            work_item_id: &chore_id,
            kind: boss_protocol::ProposalKind::RunDone,
            payload_json: &done_payload,
            idempotency_key: "deliver-still-open",
        })
        .unwrap();

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_feature_flags(flags_on_run_done(workspace.path()))
        .with_branch_verifier(StubBranchVerifier::ok(&own_branch(&execution_id)))
        .with_pr_state_checker(Arc::new(FakePrStateChecker::always(PrOpenState::Open)));

    let outcome = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(
        matches!(outcome, StopOutcome::RunDoneDeclaredWithoutDelivery { ref detail } if detail.contains("still open")),
        "open PR with superseded_by must fail, got {outcome:?}"
    );
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
}
