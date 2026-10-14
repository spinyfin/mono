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
