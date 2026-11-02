use super::*;

#[test]
fn every_revision_rebases_before_edits_and_preserves_the_existing_gate() {
    for bazel in [true, false] {
        for recovered in [true, false] {
            let prompt = compose_execution_prompt(
                ExecutionPromptParams::builder()
                    .review_signal_checks(|_| &[])
                    .execution(&revision_execution("https://github.com/org/repo/pull/77"))
                    .work_item(&revision_task_with_created_via(None, "operator"))
                    .workspace_path(Path::new("/tmp/workspace"))
                    .bookmark_recovery(&("recovery".into(), recovered))
                    .bazel_workspace(bazel)
                    .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                    .build(),
            );
            let rebase = prompt.find("workspace local-rebase --pr").unwrap();
            assert!(
                rebase
                    < prompt
                        .find("1. After the mandatory rebase, make the requested change")
                        .unwrap()
            );
            assert!(prompt.contains("must retain every commit and its description"));
            assert!(prompt.contains("integrating the PR's intent and the base branch's changes"));
            assert!(prompt.contains("Only `MERGEABLE` permits delivery"));
            assert!(prompt.contains("at most three reads"));
            assert!(prompt.contains("valid to have NO new commit"));
            assert!(!prompt.contains("Start making your changes directly"));
            assert!(!prompt.contains("continue making changes directly"));
            assert!(prompt.contains("including a parent PR branch for stacked PRs"));
            assert!(prompt.contains("If it changed the head, validate"));
            assert!(prompt.contains("no-op declaration is rejected unless GitHub reports"));
            if bazel {
                assert!(prompt.contains("full `bazel build` + `bazel test` gate before pushing"));
            } else {
                assert!(prompt.contains("non-Bazel repository"));
            }
        }
    }
}

#[test]
fn dispatch_notice_reads_the_revision_chain_roots_conflict_snapshot() {
    use crate::test_support::{create_test_chore_manual, create_test_product_named};
    use crate::work::{FakePrStateChecker, PrOpenState, WorkDb};
    let dir = tempfile::tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("test.db")).unwrap();
    let product = create_test_product_named(&db, "revision prompt");
    let parent = create_test_chore_manual(&db, product.id.clone(), "Parent");
    let pr = "https://github.com/org/repo/pull/77";
    db.update_work_item(
        &parent.id,
        crate::work::WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let revision = db
        .create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(&parent.id)
                .description("Address review")
                .build(),
            &FakePrStateChecker::always(PrOpenState::Open),
        )
        .unwrap();
    let mut execution = revision_execution(pr);
    execution.work_item_id = revision.id;
    for (mergeable, expected) in [("CONFLICTING", true), ("MERGEABLE", false), ("UNKNOWN", false)] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET pr_mergeable_state = ?2 WHERE id = ?1",
                rusqlite::params![parent.id, mergeable],
            )
            .unwrap();
        let notice = super::super::revision_rebase::revision_conflict_notice(&db, &execution).unwrap();
        assert_eq!(notice.contains("Conflict known at dispatch"), expected);
        if expected {
            assert!(notice.contains("Rebase first, preserve both sides"));
            assert!(notice.contains("no conflicting-file list"));
        }
    }
    let attempt = db
        .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
            product_id: product.id,
            work_item_id: parent.id.clone(),
            pr_url: pr.into(),
            pr_number: 77,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: None,
            head_sha_before: None,
        })
        .unwrap()
        .unwrap();
    db.set_conflict_resolution_diagnosis(&attempt.id,
        r#"{"schema_version":1,"base_sha":"base","head_sha":"head","files":[{"path":"src/review.rs","shape":"content"}],"error":null}"#).unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_mergeable_state = 'CONFLICTING' WHERE id = ?1",
            [&parent.id],
        )
        .unwrap();
    let notice = super::super::revision_rebase::revision_conflict_notice(&db, &execution).unwrap();
    assert!(notice.contains("src/review.rs"), "{notice}");
}
