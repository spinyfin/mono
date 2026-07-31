//! `attach_ai_review_state` — the resolver behind `Task.ai_review_state` /
//! `ai_review_findings_revision_id`. Covers the traps called out in the
//! design: the chain-root rollup to the last completed revision's OWN
//! verdict, kind exclusion, ignoring stale attentions, and "no informative
//! verdict" rendering as no badge rather than being inferred as clean.

use super::*;

/// A chain root's card must reflect the last completed (`in_review`/`done`)
/// revision's own verdict — never the root's own (nonexistent) row.
/// `pr_review_verdicts.work_item_id` is recorded against whichever row
/// actually produced the reviewed push (`finalize_pr_review_pass`'s
/// `producing_task_id`), which is the revision's own id for a
/// revision-triggered review, so a resolver that only ever looked at the
/// root's id would find nothing and wrongly blank the badge.
#[test]
fn ai_review_state_rolls_up_from_last_completed_revision_on_chain_root() {
    let db = WorkDb::open(temp_db_path("ai-review-state-rollup")).unwrap();
    let product_id = make_revision_product(&db, "rollup");
    let pr_url = "https://github.com/spinyfin/mono/pull/5001";
    let root_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&root_id), &checker).unwrap();

    // The revision itself finished its own push and was reviewed — mark it
    // `in_review` (a "completed" revision for rollup purposes; `done`
    // would also qualify).
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
            rusqlite::params![revision.id],
        )
        .unwrap();
    }

    // The revision's OWN pr_review pass recorded a verdict against its OWN
    // id, exactly as `finalize_pr_review_pass` does for a revision-
    // triggered review.
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let followup_id = "task_followup_findings_test";
    {
        let conn = db.connect().unwrap();
        WorkDb::insert_review_verdict_in_tx(
            &conn,
            &execution.id,
            &revision.id,
            &crate::work::ReviewVerdictInput {
                head_sha: Some("sha-findings".to_owned()),
                findings_count: 2,
                revision_warranted: true,
                gate_outcome: crate::work::REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS,
            },
        )
        .unwrap();
    }
    db.set_review_verdict_revision_task_id(&execution.id, followup_id)
        .unwrap();

    let tree = db.get_work_tree(&product_id).unwrap();
    let root_card = tree
        .chores
        .iter()
        .find(|c| c.id == root_id)
        .expect("root chore present");
    assert_eq!(
        root_card.ai_review_state.as_deref(),
        Some("reviewed_with_findings"),
        "the chain root's card must reflect the last completed revision's own verdict"
    );
    assert_eq!(
        root_card.ai_review_findings_revision_id.as_deref(),
        Some(followup_id),
        "the reveal target must be the verdict's own revision_task_id"
    );

    // The revision's own row (were it ever rendered standalone) resolves
    // the same state directly from its own id — no rollup needed there.
    let revision_card = tree
        .tasks
        .iter()
        .find(|t| t.id == revision.id)
        .expect("revision present");
    assert_eq!(revision_card.ai_review_state.as_deref(), Some("reviewed_with_findings"));
}

/// `design`/`design_postmortem`/`investigation` kinds never get an initial
/// AI review (`should_enqueue_reviewer_for_primary` excludes them) — the
/// badge must read `review_not_required` regardless of status or any PR
/// state, re-derived from `tasks.kind` via the shared predicate rather than
/// a duplicated kind list.
#[test]
fn ai_review_state_is_review_not_required_for_kind_excluded_investigation() {
    let db = WorkDb::open(temp_db_path("ai-review-state-kind-excluded")).unwrap();
    let product = create_test_product(&db);
    let investigation = db
        .create_investigation(
            boss_protocol::CreateInvestigationInput::builder()
                .product_id(product.id.clone())
                .name("Root-cause investigation")
                .build(),
        )
        .unwrap();

    // Even sitting in Review with an open PR, an investigation is never
    // reviewable — the badge must say so, not read blank as if a review
    // simply hasn't run yet.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![investigation.id, "https://github.com/spinyfin/mono/pull/6001"],
        )
        .unwrap();
    }

    let tree = db.get_work_tree(&product.id).unwrap();
    let card = tree
        .tasks
        .iter()
        .find(|t| t.id == investigation.id)
        .expect("investigation present");
    assert_eq!(card.ai_review_state.as_deref(), Some("review_not_required"));
    assert!(card.ai_review_findings_revision_id.is_none());
}

/// A `pr_review_died_without_findings` attention left `open` (e.g. the
/// auto-resolve on the next completed pass hasn't run for whatever reason)
/// must never suppress or otherwise influence the badge. The resolver reads
/// `pr_review_verdicts` only — it never queries `work_attention_items` — so
/// a later genuinely completed pass's verdict wins regardless of what the
/// stale attention says.
#[test]
fn ai_review_state_ignores_a_stale_open_pr_review_died_attention() {
    let db = WorkDb::open(temp_db_path("ai-review-state-stale-attention")).unwrap();
    let product_id = make_revision_product(&db, "stale-attention");
    let pr_url = "https://github.com/spinyfin/mono/pull/7001";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    db.create_attention_item(CreateAttentionItemInput {
        execution_id: None,
        work_item_id: Some(chore_id.clone()),
        kind: crate::pr_review_recovery::PR_REVIEW_DIED_ATTENTION_KIND.to_owned(),
        status: None,
        title: "Automated review died without findings — auto-refired".to_owned(),
        body_markdown: "test".to_owned(),
        resolved_at: None,
    })
    .unwrap();

    // A later pass actually completed clean — the attention above is
    // deliberately left `open` (not resolved) to prove it plays no role.
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    {
        let conn = db.connect().unwrap();
        WorkDb::insert_review_verdict_in_tx(
            &conn,
            &execution.id,
            &chore_id,
            &crate::work::ReviewVerdictInput {
                head_sha: Some("sha-clean".to_owned()),
                findings_count: 0,
                revision_warranted: false,
                gate_outcome: crate::work::REVIEW_GATE_OUTCOME_COMPLETED_CLEAN,
            },
        )
        .unwrap();
    }

    let tree = db.get_work_tree(&product_id).unwrap();
    let card = tree.chores.iter().find(|c| c.id == chore_id).expect("chore present");
    assert_eq!(
        card.ai_review_state.as_deref(),
        Some("reviewed_all_clear"),
        "a stale open pr_review_died_without_findings attention must never override the badge"
    );
}

/// A card with no `pr_review_verdicts` row at all for its id — the pass
/// simply has not completed yet — must render no badge. Absence of evidence
/// must never be promoted to "reviewed all clear."
#[test]
fn ai_review_state_is_none_when_no_verdict_exists_for_the_current_head() {
    let db = WorkDb::open(temp_db_path("ai-review-state-no-verdict")).unwrap();
    let product_id = make_revision_product(&db, "no-verdict");
    let pr_url = "https://github.com/spinyfin/mono/pull/8001";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let tree = db.get_work_tree(&product_id).unwrap();
    let card = tree.chores.iter().find(|c| c.id == chore_id).expect("chore present");
    assert!(
        card.ai_review_state.is_none(),
        "no informative verdict must render as no badge, never inferred as clean or reviewing"
    );
}

/// The "reviewing" state must track the existing `ai_reviewing` flag exactly
/// — this is a thin remap, not a second independent computation.
#[test]
fn ai_review_state_reviewing_matches_ai_reviewing_flag() {
    let db = WorkDb::open(temp_db_path("ai-review-state-reviewing")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Has PR under review");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'active', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![chore.id, "https://github.com/spinyfin/mono/pull/9101"],
        )
        .unwrap();
    }
    let review = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run(
        &review.id,
        "worker-rev",
        "mono",
        "lease-rev",
        "mono-agent-001",
        "/tmp/mono-agent-001",
    )
    .unwrap();

    let tree = db.get_work_tree(&product.id).unwrap();
    let card = tree.chores.iter().find(|c| c.id == chore.id).expect("chore present");
    assert!(
        card.ai_reviewing,
        "precondition: ai_reviewing must be set by the running pr_review"
    );
    assert_eq!(card.ai_review_state.as_deref(), Some("reviewing"));
}

/// There is deliberately no "review failed" badge state (design decision:
/// absence is the signal). A `gave_up` verdict — the reviewer never
/// produced a result even after re-prompting — must render exactly like no
/// verdict at all, not a distinguishable failure indicator.
#[test]
fn ai_review_state_treats_gave_up_verdict_as_no_badge() {
    let db = WorkDb::open(temp_db_path("ai-review-state-gave-up")).unwrap();
    let product_id = make_revision_product(&db, "gave-up");
    let pr_url = "https://github.com/spinyfin/mono/pull/9201";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    {
        let conn = db.connect().unwrap();
        WorkDb::insert_review_verdict_in_tx(
            &conn,
            &execution.id,
            &chore_id,
            &crate::work::ReviewVerdictInput {
                head_sha: None,
                findings_count: 0,
                revision_warranted: false,
                gate_outcome: crate::work::REVIEW_GATE_OUTCOME_GAVE_UP,
            },
        )
        .unwrap();
    }

    let tree = db.get_work_tree(&product_id).unwrap();
    let card = tree.chores.iter().find(|c| c.id == chore_id).expect("chore present");
    assert!(
        card.ai_review_state.is_none(),
        "gave_up must render exactly like no verdict at all — never a failure badge"
    );
}
