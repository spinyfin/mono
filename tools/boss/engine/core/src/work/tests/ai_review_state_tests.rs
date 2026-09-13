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
        // Real code (`pr_flow.rs`'s `record_worker_pr_completion`) always
        // finalizes the producing execution to `completed` in the same
        // transaction as the verdict it records — never leaves it `ready`.
        // Match that here so the row isn't mistaken for a still-queued pass.
        conn.execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            rusqlite::params![execution.id],
        )
        .unwrap();
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

/// The rollup to the last completed revision's verdict is a preference, not
/// a hard redirect: when that revision has no informative verdict of its
/// own, the root must fall back to its OWN verdict rather than rendering no
/// badge. This is the exact defect that left Done chain roots with a
/// perfectly good verdict on record blanking out once their terminal
/// revision (itself unreviewed) became the rollup target.
#[test]
fn ai_review_state_falls_back_to_root_verdict_when_terminal_revision_has_none() {
    let db = WorkDb::open(temp_db_path("ai-review-state-fallback")).unwrap();
    let product_id = make_revision_product(&db, "fallback");
    let pr_url = "https://github.com/spinyfin/mono/pull/5101";
    let root_id = make_in_review_chore(&db, &product_id, pr_url);

    // The root's own push was reviewed and got a verdict recorded against
    // the root's own id.
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let followup_id = "task_followup_fallback_test";
    {
        let conn = db.connect().unwrap();
        WorkDb::insert_review_verdict_in_tx(
            &conn,
            &execution.id,
            &root_id,
            &crate::work::ReviewVerdictInput {
                head_sha: Some("sha-root-findings".to_owned()),
                findings_count: 1,
                revision_warranted: true,
                gate_outcome: crate::work::REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS,
            },
        )
        .unwrap();
    }
    db.set_review_verdict_revision_task_id(&execution.id, followup_id)
        .unwrap();

    // A later revision completed but never itself got an informative
    // verdict — it becomes the rollup target with nothing to report, so
    // the root must fall back to the verdict it already has. Both rows
    // are driven to `done` (rather than `in_review`) because the reported
    // defect is specifically Done-biased: it surfaces once a card reaches
    // Done and its terminal revision flips to `done` alongside it.
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&root_id), &checker).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'done' WHERE id IN (?1, ?2)",
            rusqlite::params![root_id, revision.id],
        )
        .unwrap();
    }

    let tree = db.get_work_tree(&product_id).unwrap();
    let root_card = tree
        .chores
        .iter()
        .find(|c| c.id == root_id)
        .expect("root chore present");
    assert_eq!(
        root_card.ai_review_state.as_deref(),
        Some("reviewed_with_findings"),
        "the root must fall back to its own verdict when the terminal revision has none"
    );
    assert_eq!(
        root_card.ai_review_findings_revision_id.as_deref(),
        Some(followup_id),
        "the reveal target must be the root's own verdict's revision_task_id"
    );
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
        // See the matching comment in
        // `ai_review_state_rolls_up_from_last_completed_revision_on_chain_root`:
        // real code always finalizes the producing execution to `completed`
        // in the same transaction as the verdict.
        conn.execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            rusqlite::params![execution.id],
        )
        .unwrap();
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

/// A failed reviewer attempt is operational history, not a verdict. Once a
/// later pass completes, its durable verdict must still drive the card badge.
/// This specifically protects retry/recovery after reviewer-pool contention:
/// the failed execution remains queryable for diagnosis without hiding the
/// completed review from the board.
#[test]
fn ai_review_state_shows_completed_review_after_an_earlier_failed_attempt() {
    let db = WorkDb::open(temp_db_path("ai-review-state-failed-then-completed")).unwrap();
    let product_id = make_revision_product(&db, "failed-then-completed");
    let chore_id = make_in_review_chore(&db, &product_id, "https://github.com/spinyfin/mono/pull/8101");

    let failed = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let completed = db
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
        conn.execute(
            "UPDATE work_executions SET status = 'failed' WHERE id = ?1",
            rusqlite::params![failed.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            rusqlite::params![completed.id],
        )
        .unwrap();
        WorkDb::insert_review_verdict_in_tx(
            &conn,
            &completed.id,
            &chore_id,
            &crate::work::ReviewVerdictInput {
                head_sha: Some("sha-completed-after-retry".to_owned()),
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
        "a completed review must show even when an earlier pr_review execution failed"
    );
}

/// A terminal failure does not itself establish that a review occurred. The
/// card remains unbadged until a reviewer produces an informative verdict.
#[test]
fn ai_review_state_hides_badge_when_every_review_attempt_failed() {
    let db = WorkDb::open(temp_db_path("ai-review-state-only-failed")).unwrap();
    let product_id = make_revision_product(&db, "only-failed");
    let chore_id = make_in_review_chore(&db, &product_id, "https://github.com/spinyfin/mono/pull/8102");

    let failed = db
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
        conn.execute(
            "UPDATE work_executions SET status = 'failed' WHERE id = ?1",
            rusqlite::params![failed.id],
        )
        .unwrap();
    }

    let tree = db.get_work_tree(&product_id).unwrap();
    let card = tree.chores.iter().find(|c| c.id == chore_id).expect("chore present");
    assert!(
        card.ai_review_state.is_none(),
        "failed pr_review executions must not be inferred as completed reviews"
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

#[test]
fn ai_review_state_marks_a_queued_reviewer_distinctly() {
    let db = WorkDb::open(temp_db_path("ai-review-state-queued")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Queued AI review");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            pr_url: Some("https://github.com/spinyfin/mono/pull/9102".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::PrReview)
            .status(ExecutionStatus::Ready)
            .build(),
    )
    .unwrap();

    let tree = db.get_work_tree(&product.id).unwrap();
    let card = tree.chores.iter().find(|c| c.id == chore.id).expect("chore present");
    assert!(!card.ai_reviewing, "a queued reviewer has not started yet");
    assert_eq!(card.ai_review_state.as_deref(), Some("review_queued"));
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
        // See the matching comment in
        // `ai_review_state_rolls_up_from_last_completed_revision_on_chain_root`:
        // real code always finalizes the producing execution to `completed`
        // in the same transaction as the verdict.
        conn.execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            rusqlite::params![execution.id],
        )
        .unwrap();
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

/// A revision held `active` pending its cycle root's review pass has NO
/// `pr_review` execution of its own — batch leaves are created against the
/// cycle root — so it must attribute the root's running pass to itself:
/// `ai_reviewing = true` / `ai_review_state = "reviewing"`. Without this a
/// held revision's Doing card shows no reviewing indicator at all while
/// reviewer workers run against its parent (observed live: a held revision
/// returned `ai_reviewing: null, ai_review_state: null` while three
/// `pr_review` workers ran against its parent).
#[test]
fn ai_reviewing_attributes_cycle_root_running_review_to_held_revision() {
    let db = WorkDb::open(temp_db_path("ai-reviewing-held-revision")).unwrap();
    let product_id = make_revision_product(&db, "held-revision");
    let pr_url = "https://github.com/spinyfin/mono/pull/6001";
    let root_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&root_id), &checker).unwrap();
    // Simulate the revision's worker completing and the engine holding it
    // `active` pending review: `pr_flow.rs`'s `PendingReview` completion
    // target stamps the cycle root's `pr_url` onto the revision too.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'active', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![revision.id, pr_url],
        )
        .unwrap();

    // The cycle root's own `pr_review` execution is running — there is NO
    // execution against the revision itself.
    let review = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run(
        &review.id,
        "review-worker",
        "review-repo",
        "review-lease",
        "review-workspace",
        "/tmp/review-workspace",
    )
    .unwrap();

    let tree = db.get_work_tree(&product_id).unwrap();
    let revision_card = tree
        .tasks
        .iter()
        .find(|t| t.id == revision.id)
        .expect("revision present");
    assert!(
        revision_card.ai_reviewing,
        "a held revision must attribute its cycle root's running review to itself"
    );
    assert_eq!(revision_card.ai_review_state.as_deref(), Some("reviewing"));
}

/// A held revision can also own a legacy reviewer. Its own running execution
/// must light the same badge even when the cycle root has no reviewer.
#[test]
fn ai_reviewing_attributes_own_running_review_to_held_revision() {
    let db = WorkDb::open(temp_db_path("ai-reviewing-own-held-revision")).unwrap();
    let product_id = make_revision_product(&db, "own-held-revision");
    let pr_url = "https://github.com/spinyfin/mono/pull/6003";
    let root_id = make_in_review_chore(&db, &product_id, pr_url);
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&root_id), &checker).unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'active', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![revision.id, pr_url],
        )
        .unwrap();
    let review = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run(
        &review.id,
        "review-worker",
        "review-repo",
        "review-lease",
        "review-workspace",
        "/tmp/review-workspace",
    )
    .unwrap();

    let tree = db.get_work_tree(&product_id).unwrap();
    let card = tree.tasks.iter().find(|task| task.id == revision.id).unwrap();
    assert!(card.ai_reviewing);
    assert_eq!(card.ai_review_state.as_deref(), Some("reviewing"));
}

/// Same attribution as above, but for a residual pre-flatten-migration
/// nested revision (R2 -> R1 -> root) rather than a direct child of the
/// cycle root. `review_execution_target_id` must walk the FULL chain to the
/// true root — attributing R2's badge to its direct parent R1 (which has no
/// `pr_review` execution of its own either) would silently drop the
/// "reviewing" signal even while the root's reviewer is actually running.
#[test]
fn ai_reviewing_attributes_cycle_root_running_review_to_nested_held_revision() {
    let db = WorkDb::open(temp_db_path("ai-reviewing-nested-held-revision")).unwrap();
    let product_id = make_revision_product(&db, "nested-held-revision");
    let pr_url = "https://github.com/spinyfin/mono/pull/6002";
    let root_id = make_in_review_chore(&db, &product_id, pr_url);

    // R1: an ordinary direct child, not itself held for review (just the
    // intermediate hop the residual-nesting case requires).
    let r1_id = insert_revision_row(&db, &product_id, &root_id);
    // R2: nested under R1, not under the root — the pre-flatten-migration
    // shape. Held `active` pending review, exactly as the direct-child case
    // above.
    let r2_id = insert_revision_row(&db, &product_id, &r1_id);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'active', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![r2_id, pr_url],
        )
        .unwrap();

    // The cycle root's own `pr_review` execution is running — there is NO
    // execution against either revision.
    let review = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run(
        &review.id,
        "review-worker",
        "review-repo",
        "review-lease",
        "review-workspace",
        "/tmp/review-workspace",
    )
    .unwrap();

    let tree = db.get_work_tree(&product_id).unwrap();
    let revision_card = tree
        .tasks
        .iter()
        .find(|t| t.id == r2_id)
        .expect("nested revision present");
    assert!(
        revision_card.ai_reviewing,
        "a nested held revision must attribute its review cycle root's running review to itself, \
         not just a direct child of the root"
    );
    assert_eq!(revision_card.ai_review_state.as_deref(), Some("reviewing"));
}

/// A parent already `in_review` with a completed (stale) verdict on record
/// AND a fresh `pr_review` execution running against it must report the
/// live `reviewing` state, not the stale verdict — a live pass in flight is
/// more relevant than history, exactly as already holds for an Active row
/// (see the `Active` arm's "any older verdict is ignored" rule).
#[test]
fn ai_review_state_prefers_live_reviewing_over_stale_verdict_when_in_review() {
    let db = WorkDb::open(temp_db_path("ai-review-state-in-review-live-over-stale")).unwrap();
    let product_id = make_revision_product(&db, "in-review-live-over-stale");
    let pr_url = "https://github.com/spinyfin/mono/pull/6101";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    // An earlier pass already completed clean.
    let old_execution = db
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
        conn.execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            rusqlite::params![old_execution.id],
        )
        .unwrap();
        WorkDb::insert_review_verdict_in_tx(
            &conn,
            &old_execution.id,
            &chore_id,
            &crate::work::ReviewVerdictInput {
                head_sha: Some("sha-old".to_owned()),
                findings_count: 0,
                revision_warranted: false,
                gate_outcome: crate::work::REVIEW_GATE_OUTCOME_COMPLETED_CLEAN,
            },
        )
        .unwrap();
    }

    // A revision pushed a fresh commit; a new review pass is now running.
    let fresh = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run(
        &fresh.id,
        "review-worker",
        "review-repo",
        "review-lease",
        "review-workspace",
        "/tmp/review-workspace",
    )
    .unwrap();

    let tree = db.get_work_tree(&product_id).unwrap();
    let card = tree.chores.iter().find(|c| c.id == chore_id).expect("chore present");
    assert_eq!(
        card.status,
        TaskStatus::InReview,
        "starting the fresh pr_review must not move the row"
    );
    assert_eq!(
        card.ai_review_state.as_deref(),
        Some("reviewing"),
        "a live running pass must win over an older completed verdict"
    );
}
