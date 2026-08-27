//! Tests for the followup task kind: creation via block_pending_revisions_on_parent_close,
//! provenance fields (origin_task_short_id / origin_pr_number), body-text rewrite,
//! and visibility to the merge-poller candidate lists.

use super::*;

const FOLLOWUP_PR_URL: &str = "https://github.com/spinyfin/mono/pull/1537";

/// A revision whose `created_via` starts with `"pr_review:"` must be
/// converted to a `followup` (not a plain `chore`) when the parent PR merges.
/// The followup must carry origin provenance and the rewritten description.
#[test]
fn pr_review_revision_creates_followup_with_correct_kind_and_provenance() {
    let db = WorkDb::open(temp_db_path("followup-creation")).unwrap();
    let product_id = make_revision_product(&db, "fu-create");
    let pr_url = FOLLOWUP_PR_URL;
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    // Verify the parent has a short_id so provenance can be recorded.
    let parent_task = db.get_work_item(&parent_id).unwrap();
    let parent_short_id = match &parent_task {
        WorkItem::Chore(t) | WorkItem::Task(t) => t.short_id,
        other => panic!("unexpected variant: {other:?}"),
    };
    assert!(parent_short_id.is_some(), "parent must have a short_id for provenance");

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent_id.clone())
                .description("Address ALL findings before finalising this revision.")
                .created_via(format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_test_123"))
                .build(),
            &checker,
        )
        .unwrap();

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    // The revision is the one materialisation owned by this review
    // execution. Parent-close conversion changes that row in place rather
    // than archiving it and minting a second work item.
    let conn = db.connect().unwrap();
    let rev_after = query_task(&conn, &revision.id)
        .unwrap()
        .expect("revision row must still exist");
    drop(conn);
    assert_eq!(rev_after.kind, TaskKind::Followup);
    assert_eq!(rev_after.status, TaskStatus::Todo);
    assert!(rev_after.deleted_at.is_none());

    // A followup must be created (not a plain chore).
    let chores = db.list_chores(&product_id, None, false).unwrap();
    let followup = chores
        .iter()
        .find(|c| c.id == revision.id && c.kind == TaskKind::Followup);
    assert!(
        followup.is_some(),
        "a followup task must be created for a pr_review revision; chores: {chores:?}",
    );
    let followup = followup.unwrap();
    assert_eq!(
        followup.created_via,
        format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_test_123")
    );

    let conn = db.connect().unwrap();
    let materialisation_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE created_via = ?1",
            params![followup.created_via],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        materialisation_count, 1,
        "revision creation and parent-close conversion must share one execution-keyed row"
    );

    // No plain chore must be created (only the followup).
    let plain_chore = chores.iter().find(|c| c.id != parent_id && c.kind == TaskKind::Chore);
    assert!(
        plain_chore.is_none(),
        "no plain chore should exist alongside the followup; chores: {chores:?}",
    );

    // Provenance: origin_task_short_id must match the chain-root (parent chore).
    assert_eq!(
        followup.origin_task_short_id, parent_short_id,
        "followup must carry the parent's short_id as origin_task_short_id",
    );

    // Provenance: origin_pr_number must be extracted from the parent pr_url (1537).
    assert_eq!(
        followup.origin_pr_number,
        Some(1537),
        "followup must carry the PR number from the parent's pr_url",
    );

    // Description: old wording replaced.
    assert!(
        !followup.description.contains("finalising this revision"),
        "followup description must not contain 'finalising this revision'",
    );
    assert!(
        followup.description.contains("closing this follow-up"),
        "followup description must contain 'closing this follow-up'",
    );

    // Derivation must survive conversion: the revision's `pr_review:` prefix
    // is stamped onto the followup (not overwritten with `engine_auto`), so
    // dispatch emits the human "review findings" kind label.
    assert!(
        followup.created_via.starts_with(CREATED_VIA_PR_REVIEW_PREFIX),
        "followup must carry the revision's pr_review: created_via; got {:?}",
        followup.created_via,
    );
    let prefix = crate::runner::work_item::followup_pr_body_prefix(
        &followup.kind,
        &followup.created_via,
        followup.origin_pr_number,
        "git@github.com:spinyfin/mono.git",
    )
    .expect("followup with origin PR and github remote must yield a body prefix")
    .expect("followup must have a provenance body prefix");
    assert_eq!(
        prefix,
        "## Boss follow-up\n\nThis `review findings` follow-up derives from [the origin PR](https://github.com/spinyfin/mono/pull/1537).",
        "chain-helpers conversion must yield the complete review-findings provenance prefix"
    );
}

/// A replayed `create_revision` for the same review execution, arriving
/// after the parent-close conversion already turned the revision into a
/// followup, must resolve to that followup rather than mint a second row.
/// This is the branch of the dedup query most likely to regress if the
/// dedup check is ever moved after parent resolution: by the time the
/// replay lands, `kind` is no longer `revision` and `parent_task_id` is
/// `NULL`.
#[test]
fn replayed_create_revision_resolves_to_already_converted_followup() {
    let db = WorkDb::open(temp_db_path("followup-replay-after-conversion")).unwrap();
    let product_id = make_revision_product(&db, "fu-replay");
    let pr_url = FOLLOWUP_PR_URL;
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);
    let created_via = format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_replay_after_conversion");

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent_id.clone())
                .description("Address ALL findings before finalising this revision.")
                .created_via(created_via.clone())
                .build(),
            &checker,
        )
        .unwrap();

    // Parent PR merges: converts the revision in place to a followup.
    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    let conn = db.connect().unwrap();
    let converted = query_task(&conn, &revision.id)
        .unwrap()
        .expect("converted row must still exist");
    drop(conn);
    assert_eq!(converted.kind, TaskKind::Followup);
    assert!(
        converted.parent_task_id.is_none(),
        "converted followup must not retain parent_task_id"
    );

    // A replayed mint for the same review execution arrives after the
    // conversion. It must resolve to the followup, not insert a second row.
    let replay = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent_id.clone())
                .description("Differently rendered text from the same review.")
                .created_via(created_via.clone())
                .build(),
            &checker,
        )
        .unwrap();

    assert_eq!(
        replay.id, revision.id,
        "replayed mint must resolve to the already-converted followup"
    );

    let conn = db.connect().unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE created_via = ?1",
            params![created_via],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "one review execution must materialise at most one work item, even after conversion"
    );
}

/// When the chain root's PR URL cannot yield an origin PR number, a
/// pr_review revision must fall back to a plain `chore` rather than mint
/// an un-spawnable `Followup` with `origin_pr_number = None`.
#[test]
fn pr_review_revision_without_parseable_origin_pr_falls_back_to_chore() {
    let db = WorkDb::open(temp_db_path("followup-no-origin")).unwrap();
    let product_id = make_revision_product(&db, "fu-no-origin");
    // Issues URL is accepted as a bound URL for gate/check purposes but
    // extract_pr_number_from_url rejects it (no `/pull/<n>` segment).
    let pr_url = "https://github.com/spinyfin/mono/issues/1537";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent_id.clone())
                .description("Address ALL findings before finalising this revision.")
                .created_via(format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_no_origin"))
                .build(),
            &checker,
        )
        .unwrap();

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    let chores = db.list_chores(&product_id, None, false).unwrap();
    let followups: Vec<_> = chores
        .iter()
        .filter(|c| c.id != parent_id && c.kind == TaskKind::Followup)
        .collect();
    assert!(
        followups.is_empty(),
        "must not mint Followup without a parseable origin PR; got {followups:?}"
    );
    let chore = chores
        .iter()
        .find(|c| c.id == revision.id && c.kind == TaskKind::Chore)
        .expect("pr_review revision without origin PR must fall back to a plain chore");
    assert!(
        chore.origin_pr_number.is_none(),
        "fallback chore must not invent an origin_pr_number; got {:?}",
        chore.origin_pr_number
    );
    assert_eq!(
        chore.created_via,
        format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_no_origin"),
        "fallback chore must retain the review execution idempotency key"
    );
}

/// A pr_review revision in the `active` (WIP) state must produce an autostart
/// followup so the work is immediately redispatched on a fresh PR.
#[test]
fn pr_review_active_revision_creates_autostart_followup() {
    let db = WorkDb::open(temp_db_path("followup-active")).unwrap();
    let product_id = make_revision_product(&db, "fu-active");
    let pr_url = FOLLOWUP_PR_URL;
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent_id.clone())
                .description("Address ALL findings before finalising this revision.")
                .created_via(format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_test_456"))
                .build(),
            &checker,
        )
        .unwrap();

    // Simulate the revision being dispatched with a live lease. Conversion
    // changes the task kind in place, but the merge-poller's stop step must
    // still be able to find this revision_implementation execution.
    let implementation = db
        .list_executions(Some(&revision.id))
        .unwrap()
        .into_iter()
        .find(|execution| execution.kind == ExecutionKind::RevisionImplementation)
        .expect("revision creation must enqueue its implementation");
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'active' WHERE id = ?1",
        rusqlite::params![revision.id],
    )
    .unwrap();
    conn.execute(
        "UPDATE work_executions
         SET status = 'running', cube_lease_id = 'lease-review-revision',
             cube_workspace_id = 'mono-agent-review-revision', workspace_path = '/tmp/review-revision'
         WHERE id = ?1",
        params![implementation.id],
    )
    .unwrap();
    drop(conn);

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    let chores = db.list_chores(&product_id, None, false).unwrap();
    let followup = chores
        .iter()
        .find(|c| c.id == revision.id && c.kind == TaskKind::Followup);
    assert!(
        followup.is_some(),
        "a followup must be created for a WIP pr_review revision"
    );
    assert!(
        followup.unwrap().autostart,
        "WIP pr_review revision must produce autostart followup"
    );
    let active = db.list_active_revision_executions_for_chain(&parent_id).unwrap();
    assert_eq!(
        active.len(),
        1,
        "converted row's live revision execution must remain discoverable"
    );
    assert_eq!(active[0].id, implementation.id);
}

/// A completed implementation is already the delivered fix even though the
/// revision card stays active while its re-review runs. If the parent PR
/// merges during that window, the parent-close path must settle the existing
/// row instead of materialising the original review findings again.
#[test]
fn pr_review_completed_implementation_is_not_rematerialised_during_re_review() {
    let db = WorkDb::open(temp_db_path("followup-completed-revision")).unwrap();
    let product_id = make_revision_product(&db, "fu-completed");
    let pr_url = FOLLOWUP_PR_URL;
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);
    let created_via = format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_completed_findings");

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent_id.clone())
                .description("Address ALL findings before finalising this revision.")
                .created_via(created_via.clone())
                .build(),
            &checker,
        )
        .unwrap();
    let implementation = db
        .list_executions(Some(&revision.id))
        .unwrap()
        .into_iter()
        .find(|execution| execution.kind == ExecutionKind::RevisionImplementation)
        .expect("revision creation must enqueue its implementation");
    let conn = db.connect().unwrap();
    conn.execute("UPDATE tasks SET status = 'active' WHERE id = ?1", params![revision.id])
        .unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'completed', finished_at = ?2 WHERE id = ?1",
        params![implementation.id, now_string()],
    )
    .unwrap();
    drop(conn);

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    let conn = db.connect().unwrap();
    let (count, kind, status): (i64, String, String) = conn
        .query_row(
            "SELECT COUNT(*), MIN(kind), MIN(status) FROM tasks WHERE created_via = ?1",
            params![created_via],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(count, 1, "the re-review race must not mint a second findings row");
    assert_eq!(kind, "revision", "the delivered revision keeps its identity");
    assert_eq!(status, "done", "the merged implementation must settle as delivered");
}

/// If the revision was redispatched after its first implementation
/// completed (retry, recheck, stale-worker redispatch) and a SECOND
/// implementation was created after it, the completed-implementation shortcut
/// must NOT fire — even when the parent-close sweep abandons that later row.
/// The findings must still be carried forward as a followup rather than
/// silently marked done.
#[test]
fn pr_review_redispatched_revision_with_pending_second_implementation_still_converts() {
    for second_status in ["running", "queued"] {
        let db = WorkDb::open(temp_db_path(&format!("followup-redispatched-{second_status}"))).unwrap();
        let product_id = make_revision_product(&db, &format!("fu-redispatched-{second_status}"));
        let pr_url = FOLLOWUP_PR_URL;
        let parent_id = make_in_review_chore(&db, &product_id, pr_url);
        let created_via = format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_redispatched_{second_status}");

        let checker = FakePrStateChecker::always(PrOpenState::Open);
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(parent_id.clone())
                    .description("Address ALL findings before finalising this revision.")
                    .created_via(created_via.clone())
                    .build(),
                &checker,
            )
            .unwrap();
        let first_impl = db
            .list_executions(Some(&revision.id))
            .unwrap()
            .into_iter()
            .find(|execution| execution.kind == ExecutionKind::RevisionImplementation)
            .expect("revision creation must enqueue its implementation");

        let conn = db.connect().unwrap();
        conn.execute("UPDATE tasks SET status = 'active' WHERE id = ?1", params![revision.id])
            .unwrap();
        conn.execute(
            "UPDATE work_executions
             SET status = 'completed', finished_at = ?2, created_at = '2026-01-01T00:00:00Z'
             WHERE id = ?1",
            params![first_impl.id, now_string()],
        )
        .unwrap();
        // A second implementation was redispatched when the parent PR merges.
        let second_impl_id = next_id("exec");
        conn.execute(
            "INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at)
         VALUES (?1, ?2, 'revision_implementation', ?3, 'git@github.com:spinyfin/mono.git', ?4)",
            params![second_impl_id, revision.id, second_status, "2026-01-01T00:00:01Z"],
        )
        .unwrap();
        drop(conn);

        db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

        let conn = db.connect().unwrap();
        let (count, kind, status): (i64, String, String) = conn
            .query_row(
                "SELECT COUNT(*), MIN(kind), MIN(status) FROM tasks WHERE created_via = ?1",
                params![created_via],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(count, 1, "redispatch must not mint a second findings row");
        assert_eq!(
            kind, "followup",
            "a revision with a still-pending second implementation must be converted to a followup, \
         not silently marked done"
        );
        assert_eq!(
            status, "todo",
            "the followup must be dispatchable, not settled as done while findings are unresolved"
        );
    }
}

/// A followup in `in_review` with a `pr_url` must appear in
/// `list_chores_pending_merge_check` so the merge poller can flip it to `done`.
#[test]
fn followup_visible_to_merge_check_poller() {
    let db = WorkDb::open(temp_db_path("followup-merge-check")).unwrap();
    let product_id = make_revision_product(&db, "fu-merge");
    let parent_pr = "https://github.com/spinyfin/mono/pull/1000";
    let parent_id = make_in_review_chore(&db, &product_id, parent_pr);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    db.create_revision(
        CreateRevisionInput::builder()
            .parent_task_id(parent_id.clone())
            .description("Address ALL findings before finalising this revision.")
            .created_via(format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_test_789"))
            .build(),
        &checker,
    )
    .unwrap();

    db.mark_chore_pr_merged(&parent_id, parent_pr).unwrap();

    // Find the newly created followup.
    let chores = db.list_chores(&product_id, None, false).unwrap();
    let followup = chores
        .iter()
        .find(|c| c.id != parent_id && c.kind == TaskKind::Followup)
        .expect("a followup must be created");

    // Before the followup has its own PR, it should NOT appear in the merge-check list.
    let before = db.list_chores_pending_merge_check().unwrap();
    assert!(
        !before.iter().any(|p| p.work_item_id == followup.id),
        "followup without pr_url must not appear in merge-check list",
    );

    // Simulate the followup getting its own PR and moving to in_review.
    let followup_pr = "https://github.com/spinyfin/mono/pull/9999";
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
        rusqlite::params![followup.id, followup_pr],
    )
    .unwrap();
    drop(conn);

    // Now it must appear in the merge-check list so the merge poller can close it.
    let after = db.list_chores_pending_merge_check().unwrap();
    let found = after.iter().find(|p| p.work_item_id == followup.id);
    assert!(
        found.is_some(),
        "followup in in_review with pr_url must appear in list_chores_pending_merge_check; \
         found ids: {:?}",
        after.iter().map(|p| &p.work_item_id).collect::<Vec<_>>(),
    );
    assert_eq!(found.unwrap().pr_url, followup_pr);
}

/// list_chores must return followup provenance (origin_task_short_id /
/// origin_pr_number) — not None — so the macOS list path renders the
/// Origin row correctly.
#[test]
fn list_chores_returns_followup_provenance() {
    let db = WorkDb::open(temp_db_path("followup-provenance")).unwrap();
    let product_id = make_revision_product(&db, "fu-prov");
    let pr_url = FOLLOWUP_PR_URL;
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    db.create_revision(
        CreateRevisionInput::builder()
            .parent_task_id(parent_id.clone())
            .description("Address ALL findings before finalising this revision.")
            .created_via(format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_prov_test"))
            .build(),
        &checker,
    )
    .unwrap();

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    let chores = db.list_chores(&product_id, None, false).unwrap();
    let followup = chores
        .iter()
        .find(|c| c.id != parent_id && c.kind == TaskKind::Followup)
        .expect("a followup must be created");

    assert!(
        followup.origin_task_short_id.is_some(),
        "list_chores must populate origin_task_short_id for followups; got None",
    );
    assert!(
        followup.origin_pr_number.is_some(),
        "list_chores must populate origin_pr_number for followups; got None",
    );
    assert_eq!(
        followup.origin_pr_number,
        Some(1537),
        "origin_pr_number must be parsed from parent's pr_url",
    );
}
