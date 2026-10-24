use super::reconcile_tests::{bookmark, one, revision_on_pr_head};
use super::*;
use boss_engine_test_git::jj::JjRepo;

#[test]
fn only_legacy_pr_anchor_failures_are_rearmed() {
    for suffix in [
        "does not exist in the shared store; the bound-PR checkout must write it before reconciliation",
        "must resolve to exactly one change; it is conflicted",
    ] {
        assert!(is_legacy_pr_anchor_blocker(&format!(
            "PR head bookmark pr/139 {suffix}"
        )));
        assert!(!is_legacy_pr_anchor_blocker(&format!(
            "PR head bookmark pr/not-a-number {suffix}"
        )));
    }
    assert!(!is_legacy_pr_anchor_blocker(
        "cannot repair pr/139 from GitHub: unavailable"
    ));
    assert!(!is_legacy_pr_anchor_blocker(
        "recovery bookmark boss-recovery/exec_test must resolve to exactly one change"
    ));
}

async fn repair_and_reconcile(conflicted: bool) {
    let s = revision_on_pr_head("exec_repair_anchor").await;
    let repo = &s.repo.repo;
    let head = one(repo, &revision(&s.record.head()));
    bookmark(repo, "published", &head);
    JjRepo::run(repo, &["git", "export"]);
    JjRepo::run(
        repo,
        &[
            "git",
            "remote",
            "add",
            "origin",
            repo.join(".jj/repo/store/git").to_str().unwrap(),
        ],
    );
    // Force reconciliation without damaging the preserved work itself.
    bookmark(repo, &s.record.base(), &one(repo, "main"));
    if conflicted {
        let op = JjRepo::run(repo, &["op", "log", "-n", "1", "--no-graph", "-T", "id"]);
        for target in [head.as_str(), "main"] {
            JjRepo::run(
                repo,
                &[
                    "--at-op",
                    op.trim(),
                    "bookmark",
                    "set",
                    "pr/7",
                    "-r",
                    target,
                    "--allow-backwards",
                ],
            );
        }
    } else {
        JjRepo::run(repo, &["bookmark", "delete", "pr/7"]);
    }
    let error = reconcile(&LocalJj, &s.record, Some("pr/7"), "main").await.unwrap_err();
    assert_eq!(
        pointer_integrity_kind(&error),
        Some(PointerIntegrityKind::PrHeadUnavailable)
    );
    let saved = snapshot_pointers(&LocalJj, &s.record).await.unwrap();
    repair_pr_anchor(&LocalJj, &s.record, 7, "origin", "published", &head)
        .await
        .unwrap();
    assert_eq!(snapshot_pointers(&LocalJj, &s.record).await.unwrap(), saved);
    assert_eq!(one(repo, "pr/7"), head);
    let repaired = reconcile(&LocalJj, &s.record, Some("pr/7"), "main")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repaired.anchor.commit(), head);
    assert!(diff(&LocalJj, &s.record).await.unwrap().is_empty());
}

#[tokio::test]
async fn missing_pr_bookmark_is_recreated_before_reconciliation() {
    repair_and_reconcile(false).await;
}

#[tokio::test]
async fn conflicted_pr_bookmark_is_repaired_before_reconciliation() {
    repair_and_reconcile(true).await;
}
