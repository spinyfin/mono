//! Post-hoc audit of a queue-side CI-fix `delivered` or `blocked` declaration. The
//! declaration gate is purely local (it runs on the termination path), so the
//! facts it cannot check without GitHub are checked here, off that path:
//! the verified head is the PR's head and contains the stated `main_sha`, and
//! an unchanged-head requeue receipt names a live queue entry at that head.
//! A contradiction files the flagged attention; it never reverses completion.

use boss_github::gh_runner::PrMergeQueueSnapshot;

use super::*;
use crate::work::queue_ci_evidence::QueueCiAuditInput;

fn rfc3339_epoch(t: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(t).ok().map(|t| t.timestamp())
}

/// The first contradiction between the evidence and GitHub, if any.
/// Lookups that fail are skipped: an audit that cannot ask proves nothing.
/// A `blocked` declaration is only checked for main-rebased ancestry: its
/// tree may legitimately be unpushed, and it makes no requeue claim.
async fn find_contradiction(
    branch_verifier: &dyn BranchVerifier,
    repo_slug: &str,
    pr_number: u64,
    input: &QueueCiAuditInput,
) -> Option<Contradiction> {
    let QueueCiAuditInput {
        attempt,
        evidence,
        delivered,
    } = input;
    if *delivered
        && let Ok(head) = branch_verifier.fetch_pr_head_oid_fresh(repo_slug, pr_number).await
        && head != evidence.verified_head_sha
    {
        return Some(Contradiction::plain(format!(
            "`verified_head_sha` is `{}` but the PR head is `{head}`: the verified tree is not the branch that was pushed.",
            evidence.verified_head_sha
        )));
    }
    if let Ok(false) = branch_verifier
        .fetch_is_ancestor(repo_slug, &evidence.main_sha, &evidence.verified_head_sha)
        .await
    {
        return Some(Contradiction::plain(format!(
            "`main_sha` `{}` is not an ancestor of `verified_head_sha` `{}`: the run was not on a main-rebased tree.",
            evidence.main_sha, evidence.verified_head_sha
        )));
    }
    let receipt = evidence.queue_entry_id.as_deref()?;
    if !*delivered || attempt.failure_kind.as_deref() != Some("merge_queue_rebounce") {
        return None;
    }
    let snapshot = branch_verifier
        .fetch_pr_merge_queue_snapshot(repo_slug, pr_number)
        .await
        .ok()?;
    if snapshot.pr_state == "OPEN" && snapshot.entry_id.is_none() {
        let enqueued = evidence.queue_entry_enqueued_at.as_deref().and_then(rfc3339_epoch)?;
        let events = branch_verifier.fetch_queue_dequeues(repo_slug, pr_number).await.ok()?;
        if events.iter().any(|event| {
            event.reason.eq_ignore_ascii_case("failed_checks")
                && event.pr_head_oid.as_deref() == Some(&evidence.verified_head_sha)
                && event
                    .created_at
                    .as_deref()
                    .and_then(rfc3339_epoch)
                    .is_some_and(|t| t > enqueued)
        }) {
            return None;
        }
    }
    receipt_contradiction(
        &snapshot,
        receipt,
        evidence.queue_entry_enqueued_at.as_deref(),
        &evidence.verified_head_sha,
    )
}

/// Compare a requeue receipt with the PR's live queue state. A PR that merged
/// or closed has legitimately left the queue; only an open PR with no entry is
/// stranded.
fn receipt_contradiction(
    snapshot: &PrMergeQueueSnapshot,
    receipt: &str,
    receipt_enqueued_at: Option<&str>,
    verified_head: &str,
) -> Option<Contradiction> {
    if snapshot.pr_state != "OPEN" {
        return None;
    }
    let Some(live_id) = snapshot.entry_id.as_deref() else {
        return Some(Contradiction {
            detail: format!(
                "the requeue receipt `{receipt}` does not correspond to a live queue entry; the PR is open and not in the queue."
            ),
            rearm: true,
            corrected_boundary: None,
        });
    };
    let live_enqueued_at = snapshot.entry.as_ref().and_then(|e| e.enqueued_at.as_deref());
    let (receipt_t, live_t) = (
        receipt_enqueued_at.and_then(rfc3339_epoch),
        live_enqueued_at.and_then(rfc3339_epoch),
    );
    if live_id == receipt {
        if snapshot.head_oid != verified_head {
            return Some(Contradiction::plain(format!(
                "the PR is queued under entry `{live_id}` at `{}`, not at the verified head `{verified_head}`.",
                snapshot.head_oid
            )));
        }
        return match (receipt_t, live_t) {
            (Some(r), Some(l)) if r != l => Some(Contradiction {
                corrected_boundary: Some(l),
                rearm: false,
                detail: format!(
                    "the receipt claims entry `{receipt}` was enqueued at `{}` but GitHub says `{}`.",
                    receipt_enqueued_at.unwrap_or_default(),
                    live_enqueued_at.unwrap_or_default()
                ),
            }),
            _ => None,
        };
    }
    // A strictly newer live entry means the receipt's entry was consumed and
    // the PR was queued again since: not a contradiction.
    if let (Some(r), Some(l)) = (receipt_t, live_t)
        && l > r
    {
        return None;
    }
    Some(Contradiction::plain(format!(
        "the PR is queued under entry `{live_id}` at `{}`, not the receipt `{receipt}` at `{verified_head}`.",
        snapshot.head_oid
    )))
}

struct Contradiction {
    detail: String,
    /// The retired attempt's failure signal should resurface: the PR was left
    /// out of the queue with no active attempt.
    rearm: bool,
    corrected_boundary: Option<i64>,
}

impl Contradiction {
    fn plain(detail: String) -> Self {
        Self {
            detail,
            rearm: false,
            corrected_boundary: None,
        }
    }
}

/// Resurface the failure signal that accepting the delivery cleared. The
/// signal belongs to the PR-owning parent (`attempt.work_item_id`), not to the
/// CI-fix revision that made the declaration, and a newer attempt means the
/// failure was already picked up again.
fn rearm_parent_failure_signal(work_db: &crate::work::WorkDb, execution_id: &str, input: &QueueCiAuditInput) -> bool {
    let attempt = &input.attempt;
    match work_db.has_newer_ci_remediation(&attempt.work_item_id, &attempt.id, &attempt.created_at) {
        Ok(false) => {}
        Ok(true) => return false,
        Err(err) => {
            tracing::warn!(
                execution_id,
                ?err,
                "queue CI evidence audit: failed to look for a newer attempt"
            );
            return false;
        }
    }
    match work_db.rearm_audited_queue_failure(attempt) {
        Ok(rearmed) => {
            if !rearmed {
                tracing::warn!(
                    execution_id,
                    parent_work_item_id = %attempt.work_item_id,
                    "queue CI evidence audit: episode or parent no longer eligible for re-arm"
                );
            }
            rearmed
        }
        Err(err) => {
            tracing::warn!(
                execution_id,
                ?err,
                "queue CI evidence audit: failed to re-arm the failure signal"
            );
            false
        }
    }
}

async fn rearm_and_notify(
    work_db: &crate::work::WorkDb,
    publisher: &dyn ExecutionPublisher,
    execution_id: &str,
    input: &QueueCiAuditInput,
) -> bool {
    if !rearm_parent_failure_signal(work_db, execution_id, input) {
        return false;
    }
    publisher
        .publish_work_item_changed(
            &input.attempt.product_id,
            &input.attempt.work_item_id,
            "queue_ci_audit_rearmed",
        )
        .await;
    true
}

/// `pr_url` may be empty (a `blocked` declaration resolves no PR); the
/// attempt's own PR is used then.
pub(super) async fn audit_queue_ci_delivery(
    branch_verifier: &dyn BranchVerifier,
    work_db: &crate::work::WorkDb,
    publisher: &dyn ExecutionPublisher,
    execution_id: &str,
    work_item_id: &str,
    repo_remote_url: &str,
    pr_url: &str,
) {
    let Ok(Some(input)) = work_db.queue_ci_delivery_for_audit(execution_id) else {
        return;
    };
    let pr_url = if pr_url.is_empty() {
        input.attempt.pr_url.as_str()
    } else {
        pr_url
    };
    let (Ok(repo_slug), Some(pr_number)) = (parse_repo_slug(repo_remote_url), pr_number_from_url(pr_url)) else {
        return;
    };
    let Some(found) = find_contradiction(branch_verifier, &repo_slug, pr_number, &input).await else {
        return;
    };
    tracing::warn!(execution_id, work_item_id, pr_url, detail = %found.detail, "queue CI evidence audit: declaration contradicted");
    if let Some(boundary) = found.corrected_boundary
        && let Err(err) = work_db.correct_requeue_boundary(&input.attempt.id, boundary)
    {
        tracing::warn!(?err, "failed to correct authoritative requeue boundary");
    }
    if found.rearm {
        rearm_and_notify(work_db, publisher, execution_id, &input).await;
    }
    let body = if input.delivered {
        format!(
            "This execution declared a verified queue-CI delivery for {pr_url}, and the declaration was accepted on local checks alone. A post-hoc audit against GitHub found: {}\n\n\
         The completion stands, but the queue-failure evidence is contradicted. Check that the PR is in the merge queue at the tested head and decide whether to re-run the verification.",
            found.detail
        )
    } else {
        format!(
            "The blocked declaration for {pr_url} supplied verification evidence whose main_sha ancestry was contradicted: {}. Nothing was delivered. Check the rebased tree and repeat verification.",
            found.detail
        )
    };
    match work_db.create_attention_item(CreateAttentionItemInput {
        execution_id: Some(execution_id.to_owned()),
        work_item_id: None,
        kind: RUN_DONE_AUDIT_FLAGGED_ATTENTION_KIND.to_owned(),
        status: None,
        title: if input.delivered {
            "Queue CI delivery evidence contradicted by GitHub"
        } else {
            "Blocked declaration verification evidence contradicted by GitHub"
        }
        .to_owned(),
        body_markdown: body,
        resolved_at: None,
    }) {
        Ok(item) => {
            if let Ok(work_item) = work_db.get_work_item(work_item_id) {
                let product_id = work_item.product_id().to_string();
                publisher
                    .publish_frontend_event_on_product(&product_id, FrontendEvent::AttentionItemCreated { item })
                    .await;
            }
        }
        Err(err) => tracing::warn!(
            execution_id,
            ?err,
            "queue CI evidence audit: failed to file attention item"
        ),
    }
}

#[cfg(test)]
mod tests {
    use boss_github::gh_runner::MergeQueueEntry;
    use boss_protocol::{QueueCiEvidence, QueueCiRun};

    use super::*;

    const HEAD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const MAIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ENQUEUED: &str = "2026-07-27T23:10:00Z";

    /// Answers the audit's three GitHub lookups from fixed results.
    struct Stub {
        head: Result<String, String>,
        ancestor: Result<bool, String>,
        snapshot: Result<PrMergeQueueSnapshot, String>,
        dequeues: Vec<crate::merge_poller::MergeQueueDequeueEvent>,
    }

    #[async_trait]
    impl BranchVerifier for Stub {
        async fn fetch_queue_dequeues(
            &self,
            _: &str,
            _: u64,
        ) -> Result<Vec<crate::merge_poller::MergeQueueDequeueEvent>> {
            Ok(self.dequeues.clone())
        }

        async fn fetch_pr_head_ref(&self, _: &str, _: u64) -> Result<String> {
            unimplemented!()
        }
        async fn fetch_pr_head_oid(&self, _: &str, _: u64) -> Result<String> {
            unimplemented!()
        }
        async fn fetch_pr_head_oid_fresh(&self, _: &str, _: u64) -> Result<String> {
            self.head.clone().map_err(|e| anyhow!(e))
        }
        async fn fetch_diff_line_count(&self, _: &str, _: &str, _: &str) -> Result<u64> {
            unimplemented!()
        }
        async fn fetch_pr_base_ref(&self, _: &str, _: u64) -> Result<String> {
            unimplemented!()
        }
        async fn fetch_diff_signature(&self, _: &str, _: &str, _: &str) -> Result<String> {
            unimplemented!()
        }
        async fn fetch_pr_title_and_body(&self, _: &str, _: u64) -> Result<(String, String)> {
            unimplemented!()
        }
        async fn fetch_is_ancestor(&self, _: &str, _: &str, _: &str) -> Result<bool> {
            self.ancestor.clone().map_err(|e| anyhow!(e))
        }
        async fn fetch_pr_merge_queue_snapshot(&self, _: &str, _: u64) -> Result<PrMergeQueueSnapshot> {
            self.snapshot.clone().map_err(|e| anyhow!(e))
        }
    }

    fn snapshot(state: &str, head: &str, entry: Option<(&str, &str)>) -> PrMergeQueueSnapshot {
        PrMergeQueueSnapshot {
            pr_state: state.into(),
            head_oid: head.into(),
            entry_id: entry.map(|(id, _)| id.into()),
            entry: entry.map(|(_, at)| MergeQueueEntry {
                state: "QUEUED".into(),
                position: Some(1),
                enqueued_at: Some(at.into()),
            }),
        }
    }

    fn stub(snapshot: PrMergeQueueSnapshot) -> Stub {
        Stub {
            head: Ok(HEAD.into()),
            ancestor: Ok(true),
            snapshot: Ok(snapshot),
            dequeues: vec![],
        }
    }

    fn input(work_item_id: &str, delivered: bool) -> QueueCiAuditInput {
        let mut evidence = QueueCiEvidence::builder()
            .main_sha(MAIN)
            .verified_head_sha(HEAD)
            .queue_sha("c".repeat(40))
            .queue_base_sha("d".repeat(40))
            .runs(vec![QueueCiRun {
                check_name: "bazel-build-test".into(),
                source_url: "https://buildkite.com/x/y/builds/1".into(),
                command: "bazel test //...".into(),
                exit_code: 0,
                output: "ok".into(),
                coverage: "all".into(),
            }])
            .build();
        evidence.queue_entry_id = Some("MQE_receipt".into());
        evidence.queue_entry_enqueued_at = Some(ENQUEUED.into());
        let attempt = crate::work::CiRemediation::builder()
            .id("att_1")
            .product_id("prod")
            .work_item_id(work_item_id)
            .attempt_kind("fix")
            .consumes_budget(1)
            .created_at("100")
            .failed_checks("[]")
            .head_branch("feature")
            .head_sha_at_trigger(format!("mq:{HEAD}"))
            .pr_number(7)
            .pr_url("https://github.com/spinyfin/mono/pull/7")
            .status("running")
            .failure_kind("merge_queue_rebounce")
            .build();
        QueueCiAuditInput {
            attempt,
            evidence,
            delivered,
        }
    }

    async fn contradiction(verifier: &Stub, input: &QueueCiAuditInput) -> Option<Contradiction> {
        find_contradiction(verifier, "spinyfin/mono", 7, input).await
    }

    #[tokio::test]
    async fn matching_head_receipt_and_queue_entry_is_clean() {
        let verifier = stub(snapshot("OPEN", HEAD, Some(("MQE_receipt", ENQUEUED))));
        assert!(contradiction(&verifier, &input("p", true)).await.is_none());
    }

    #[tokio::test]
    async fn head_mismatch_and_non_ancestor_main_are_contradictions() {
        let mut verifier = stub(snapshot("OPEN", HEAD, Some(("MQE_receipt", ENQUEUED))));
        verifier.head = Ok("f".repeat(40));
        let found = contradiction(&verifier, &input("p", true)).await.unwrap();
        assert!(found.detail.contains("PR head") && !found.rearm);
        // A blocked declaration may be unpushed, so the head is not compared.
        assert!(contradiction(&verifier, &input("p", false)).await.is_none());
        verifier.head = Ok(HEAD.into());
        verifier.ancestor = Ok(false);
        for delivered in [true, false] {
            let found = contradiction(&verifier, &input("p", delivered)).await.unwrap();
            assert!(found.detail.contains("not an ancestor"), "delivered={delivered}");
        }
    }

    #[tokio::test]
    async fn failed_lookups_are_skipped() {
        let verifier = Stub {
            head: Err("boom".into()),
            ancestor: Err("boom".into()),
            snapshot: Err("boom".into()),
            dequeues: vec![],
        };
        assert!(contradiction(&verifier, &input("p", true)).await.is_none());
    }

    #[tokio::test]
    async fn blocked_declarations_make_no_requeue_claim() {
        let verifier = stub(snapshot("OPEN", HEAD, None));
        assert!(contradiction(&verifier, &input("p", false)).await.is_none());
    }

    #[tokio::test]
    async fn open_pr_with_no_entry_is_stranded_but_merged_or_closed_is_not() {
        let stranded = contradiction(&stub(snapshot("OPEN", HEAD, None)), &input("p", true))
            .await
            .unwrap();
        assert!(stranded.rearm);
        for state in ["MERGED", "CLOSED"] {
            let verifier = stub(snapshot(state, HEAD, None));
            assert!(contradiction(&verifier, &input("p", true)).await.is_none(), "{state}");
        }
    }

    #[tokio::test]
    async fn newer_same_head_dequeue_consumes_the_receipt_before_next_attempt() {
        let mut verifier = stub(snapshot("OPEN", HEAD, None));
        verifier.dequeues.push(crate::merge_poller::MergeQueueDequeueEvent {
            reason: "failed_checks".into(),
            created_at: Some("2026-07-27T23:11:00Z".into()),
            pr_head_oid: Some(HEAD.into()),
            before_commit_oid: None,
            queue_base_oid: None,
        });
        assert!(contradiction(&verifier, &input("p", true)).await.is_none());
        verifier.dequeues[0].pr_head_oid = Some("other-head".into());
        assert!(contradiction(&verifier, &input("p", true)).await.unwrap().rearm);
        verifier.dequeues[0].pr_head_oid = Some(HEAD.into());
        verifier.dequeues[0].created_at = Some("2026-07-27T23:09:00Z".into());
        assert!(contradiction(&verifier, &input("p", true)).await.unwrap().rearm);
    }

    #[tokio::test]
    async fn mismatched_entries_are_contradictions_unless_strictly_newer() {
        let other = |at| stub(snapshot("OPEN", HEAD, Some(("MQE_other", at))));
        let found = contradiction(&other(ENQUEUED), &input("p", true)).await.unwrap();
        assert!(!found.rearm && found.detail.contains("MQE_other"));
        assert!(
            contradiction(&other("2026-07-27T23:20:00Z"), &input("p", true))
                .await
                .is_none()
        );
        let wrong_head = stub(snapshot("OPEN", &"e".repeat(40), Some(("MQE_receipt", ENQUEUED))));
        assert!(contradiction(&wrong_head, &input("p", true)).await.is_some());
        let wrong_time = stub(snapshot("OPEN", HEAD, Some(("MQE_receipt", "2026-07-27T22:00:00Z"))));
        let found = contradiction(&wrong_time, &input("p", true)).await.unwrap();
        assert!(found.detail.contains("enqueued at"));
        assert_eq!(found.corrected_boundary, rfc3339_epoch("2026-07-27T22:00:00Z"));
    }

    #[tokio::test]
    async fn rearm_restores_the_parent_signal_not_the_revisions() {
        let (_dir, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let parent = crate::test_support::create_test_chore(&db, product.id.clone(), "Parent");
        let revision = crate::test_support::create_test_chore(&db, product.id.clone(), "CI fix");
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'blocked', blocked_reason = 'ci_failure' WHERE id = ?1",
                [&parent.id],
            )
            .unwrap();
        }
        let mut audit = input(&parent.id, true);
        audit.attempt = db
            .insert_ci_remediation(crate::work::CiRemediationInsertInput {
                product_id: product.id.clone(),
                work_item_id: parent.id.clone(),
                pr_url: "https://github.com/foo/bar/pull/1".into(),
                pr_number: 1,
                head_branch: "feature".into(),
                head_sha_at_trigger: format!("mq:{HEAD}@queue"),
                attempt_kind: "fix".into(),
                consumes_budget: 1,
                failed_checks: "[]".into(),
                failure_kind: "merge_queue_rebounce".into(),
                before_commit_sha: Some("queue".into()),
            })
            .unwrap()
            .unwrap();
        db.connect().unwrap().execute("UPDATE ci_remediations SET status = 'abandoned', failure_reason = 'verified_requeue', requeue_enqueued_at = 1 WHERE id = ?1", [&audit.attempt.id]).unwrap();
        // The revision itself is not blocked: re-arming it would be a no-op.
        assert!(!db.rearm_blocked_ci_failure_signal(&revision.id).unwrap());
        let publisher = crate::test_support::RecordingPublisher::default();
        assert!(rearm_and_notify(&db, &publisher, "exec", &audit).await);
        assert_eq!(
            *publisher.events.lock().await,
            vec![(product.id.clone(), parent.id.clone(), "queue_ci_audit_rearmed".into())]
        );
        assert!(!rearm_and_notify(&db, &publisher, "exec", &audit).await);
        assert_eq!(publisher.events.lock().await.len(), 1);
        let reasons: Vec<_> = db
            .active_blocked_signals(&parent.id)
            .unwrap()
            .into_iter()
            .map(|s| s.reason)
            .collect();
        assert_eq!(reasons, vec!["ci_failure"]);
        assert!(db.active_blocked_signals(&revision.id).unwrap().is_empty());
        let restored = db.get_ci_remediation(&audit.attempt.id).unwrap().unwrap();
        assert_eq!(restored.status, "pending");
        assert_eq!(restored.head_sha_at_trigger, format!("mq:{HEAD}"));
        let boundary: Option<i64> = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT requeue_enqueued_at FROM ci_remediations WHERE id = ?1",
                [&restored.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(boundary.is_none());
        assert!(restored.failure_reason.is_none());
        let candidate = crate::work::PendingMergeCheck {
            work_item_id: parent.id.clone(),
            product_id: product.id,
            pr_url: restored.pr_url,
        };
        let probe = crate::merge_poller::PrLifecycleProbe::builder()
            .labels(vec![])
            .review(crate::merge_poller::PrReviewState::Unknown)
            .url(&candidate.pr_url)
            .state(crate::merge_poller::PrLifecycleState::Open(
                crate::merge_poller::OpenPrStatus {
                    mergeability: crate::merge_poller::OpenPrMergeability::Clean,
                    ci: crate::merge_poller::OpenPrCiStatus::Clean,
                },
            ))
            .build();
        let mut outcome = crate::merge_poller::SweepOutcome::default();
        crate::merge_poller::maybe_clear_blocked(
            &db,
            &boss_execution_contracts::NoopExecutionPublisher,
            (None, None),
            &candidate,
            &probe,
            (&crate::merge_poller::OpenPrCiStatus::Clean, true),
            &mut outcome,
        )
        .await;
        assert_eq!(
            db.task_blocked_reason(&parent.id).unwrap().as_deref(),
            Some("ci_failure")
        );
        // A sweep may have already cleared the block before the audit ran.
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review', blocked_reason = NULL WHERE id = ?1",
            [&parent.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE ci_remediations SET status = 'abandoned', failure_reason = 'verified_requeue' WHERE id = ?1",
            [&audit.attempt.id],
        )
        .unwrap();
        drop(conn);
        assert!(rearm_parent_failure_signal(&db, "exec", &audit));
        assert_eq!(
            db.task_blocked_reason(&parent.id).unwrap().as_deref(),
            Some("ci_failure")
        );
        assert!(!rearm_parent_failure_signal(&db, "exec", &audit), "rearm is idempotent");
        // An explicit human suppression must still win over the audit.
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review', blocked_reason = NULL WHERE id = ?1",
            [&parent.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE ci_remediations SET status = 'abandoned', failure_reason = 'verified_requeue' WHERE id = ?1",
            [&audit.attempt.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ci_failure_suppressions (work_item_id, head_sha, created_at) VALUES (?1, ?2, '0')",
            rusqlite::params![parent.id, HEAD],
        )
        .unwrap();
        drop(conn);
        assert!(!rearm_parent_failure_signal(&db, "exec", &audit));
        assert!(db.task_blocked_reason(&parent.id).unwrap().is_none());
    }

    #[test]
    fn rearm_is_skipped_when_a_newer_attempt_exists() {
        let (_dir, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let parent = crate::test_support::create_test_chore(&db, product.id.clone(), "Parent");
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'blocked', blocked_reason = 'ci_failure' WHERE id = ?1",
                [&parent.id],
            )
            .unwrap();
        }
        db.insert_ci_remediation(crate::work::CiRemediationInsertInput {
            product_id: product.id,
            work_item_id: parent.id.clone(),
            pr_url: "https://github.com/spinyfin/mono/pull/7".into(),
            pr_number: 7,
            head_branch: "feature".into(),
            head_sha_at_trigger: "mq:newer".into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: "merge_queue_rebounce".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .unwrap();
        assert!(!rearm_parent_failure_signal(&db, "exec", &input(&parent.id, true)));
        assert!(db.active_blocked_signals(&parent.id).unwrap().is_empty());
    }
}
