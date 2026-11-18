use super::*;
use boss_engine_utility_model::UtilityModelError;
use boss_protocol::{CreateChoreInput, CreateProductInput};
use boss_work_db::{PrSourceCapturePersistOutcome, PrSourceCaptureTrigger};

const STUCK_ITEM: &str = "Cards show 'Queued — reason unknown' for rows the engine has nothing queued for";
const CACHED: &str = "fixing queued-reason labels on cards";
const CASES: &[(ExecutionKind, Option<ReviewStage>, &str, &str)] = &[
    (ExecutionKind::Progression, None, "unsticking", "unsticking Cards show…"),
    (
        ExecutionKind::PrReview,
        None,
        "reviewing",
        "reviewing the PR for Cards show…",
    ),
    (
        ExecutionKind::PrReview,
        Some(ReviewStage::Consolidation),
        "consolidating review",
        "consolidating review findings for Cards show…",
    ),
    (
        ExecutionKind::CiRemediation,
        None,
        "fixing CI",
        "fixing CI for Cards show…",
    ),
    (
        ExecutionKind::ConflictResolution,
        None,
        "resolving conflicts",
        "resolving merge conflicts for Cards show…",
    ),
    (
        ExecutionKind::PrReviewGuide,
        None,
        "review guide",
        "generating a review guide for Cards show…",
    ),
];

#[derive(Debug)]
struct NoModel;

impl UtilityModel for NoModel {
    fn provider_id(&self) -> &str {
        "forbidden"
    }
    fn resolve(&self, _: UtilityTask) -> Result<UtilityCall, UtilityModelError> {
        panic!("derived titles must never resolve a utility model")
    }
}

// Persist a real comparison so this exercises the same owner lookup as spawn.
fn subject(db: &WorkDb, kind: &ExecutionKind) -> (WorkItem, String, String) {
    let product = db
        .create_product(
            CreateProductInput::builder()
                .name("Pane titles")
                .repo_remote_url("https://github.com/acme/widget")
                .build(),
        )
        .unwrap();
    let task = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id)
                .name(STUCK_ITEM)
                .autostart(false)
                .build(),
        )
        .unwrap();
    let owner = task.id.clone();
    if kind != &ExecutionKind::PrReviewGuide {
        return (WorkItem::Chore(task), owner.clone(), owner);
    }
    let packet = serde_json::from_value(serde_json::json!({
        "schema_version": 3,
        "canonical_pr_url": "https://github.com/acme/widget/pull/11",
        "pr_number": 11, "title": "A different PR title", "body": null,
        "base_repository": "acme/widget", "head_repository": "acme/widget",
        "observed_base_sha": "base", "probe_base_sha": null,
        "merge_base_sha": "base", "head_sha": "head", "files": [], "omissions": []
    }))
    .unwrap();
    let PrSourceCapturePersistOutcome::Stored(capture) = db
        .persist_pr_review_guide_source_capture(&owner, 1, PrSourceCaptureTrigger::Creation, &packet)
        .unwrap()
    else {
        panic!("first capture must persist")
    };
    assert!(capture.comparison_id.starts_with("prgc_"));
    let synthetic = super::tests::sample_task(&capture.comparison_id, "Review guide: A different PR title", "");
    (synthetic, owner, capture.comparison_id)
}

#[tokio::test]
async fn derived_spawn_titles_read_the_owner_cache_without_writes_or_model_calls() {
    for (kind, stage, verb, _) in CASES {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let (item, owner, execution_item) = subject(&db, kind);
        // Deliberately stale basis: accidentally taking get_or_generate must
        // reach NoModel and fail, rather than passing on a normal cache hit.
        db.set_pane_summary(&owner, CACHED, "old basis").unwrap();
        db.set_pane_summary("unrelated", "untouched", "other basis").unwrap();
        if execution_item != owner {
            db.set_pane_summary(&execution_item, "wrong synthetic summary", "synthetic basis")
                .unwrap();
        }
        let before = db.get_pane_summary(&owner).unwrap();
        let synthetic_before = db.get_pane_summary(&execution_item).unwrap();
        let title = title_for_spawn(&db, &NoModel, kind, *stage, &execution_item, &item)
            .await
            .unwrap();
        assert_eq!(title, format!("{verb}: {CACHED}"), "{kind:?} {stage:?}");
        assert!(title.split_whitespace().count() <= DERIVED_TITLE_WORD_BUDGET, "{title}");
        assert_eq!(db.get_pane_summary(&owner).unwrap(), before);
        assert_eq!(db.get_pane_summary(&execution_item).unwrap(), synthetic_before);
        assert_eq!(
            db.get_pane_summary("unrelated").unwrap(),
            Some(("untouched".into(), "other basis".into()))
        );
    }
}

#[tokio::test]
async fn derived_spawn_titles_without_cache_keep_delimiters_closed_and_cache_empty() {
    for (kind, stage, _, expected) in CASES {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let (item, owner, execution_item) = subject(&db, kind);
        let title = title_for_spawn(&db, &NoModel, kind, *stage, &execution_item, &item)
            .await
            .unwrap();
        assert_eq!(&title, expected, "{kind:?} {stage:?}");
        assert!(!has_open_delimiter(&title), "{title}");
        assert_eq!(db.get_pane_summary(&owner).unwrap(), None);
        assert_eq!(db.get_pane_summary(&execution_item).unwrap(), None);
    }
}

#[test]
fn quoted_closers_and_unmatched_closers_cannot_cancel_open_brackets() {
    assert_eq!(
        progression_summary("Handle \")\" (closing delimiters in labels)", None).as_deref(),
        Some("unsticking Handle \")\"…")
    );
    assert_eq!(
        progression_summary("", Some("Handle \")\" (closing delimiters in labels and more words)")).as_deref(),
        Some("unsticking: Handle \")\"…")
    );
    assert!(has_open_delimiter("Handle ) (closing"));
    assert!(!has_open_delimiter("Handle \"(\" labels"));
    assert!(!has_open_delimiter("Handle '( ' labels"));
}

#[test]
fn possessives_and_sentence_periods_do_not_force_an_earlier_cut() {
    assert_eq!(
        name_fragment("Handle users' toggles carefully today", 3).as_deref(),
        Some("Handle users' toggles…")
    );
    assert_eq!(
        name_fragment("Handle users’ toggles carefully today", 3).as_deref(),
        Some("Handle users’ toggles…")
    );
    assert!(!has_open_delimiter("users'"));
    assert!(has_open_delimiter("'users' ("));
    assert_eq!(
        name_fragment("Fix labels. Then verify output", 2).as_deref(),
        Some("Fix labels.…")
    );
    assert_eq!(
        name_fragment("Handle e.g. labels carefully", 2).as_deref(),
        Some("Handle e.g.…")
    );
}

#[test]
fn cached_summary_respects_the_word_budget() {
    assert_eq!(
        progression_summary("x", Some("persisting slot ids on the run record today")).as_deref(),
        Some("unsticking: persisting slot ids on the run…")
    );
}

#[test]
fn fallback_does_not_end_mid_identifier_or_on_filler() {
    assert_eq!(
        progression_summary("Fix the engine:: resolver breakage today", None).as_deref(),
        Some("unsticking Fix…")
    );
    assert_eq!(
        progression_summary("Rows that are never queued anywhere", None).as_deref(),
        Some("unsticking Rows…")
    );
    assert!(!has_open_delimiter("don't stop"));
    assert_eq!(
        pr_review_summary("Handle (parent and child tasks in the tree", None).as_deref(),
        Some("reviewing the PR for Handle…")
    );
}
