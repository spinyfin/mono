use super::*;

#[test]
fn review_family_uses_parent_index_and_preserves_cycle_semantics() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE tasks(id TEXT PRIMARY KEY, kind TEXT, parent_task_id TEXT);
        CREATE INDEX idx_tasks_parent_task_id ON tasks(parent_task_id);
        INSERT INTO tasks VALUES ('root','project_task',NULL), ('child','revision','root'),
        ('grandchild','revision','child'), ('cycle-a','revision','cycle-b'),
        ('cycle-b','revision','cycle-a'), ('unrelated','project_task',NULL);",
    )
    .unwrap();
    let sql = format!(
        "{} SELECT root, id, depth FROM family ORDER BY root,id,depth",
        review_family_cte("?1,?2", ReviewKey::Card)
    );
    let rows = |query: &str| {
        conn.prepare(query)
            .unwrap()
            .query_map(["grandchild", "cycle-a"], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(
        rows(&sql),
        rows(&sql.replace(" INDEXED BY idx_tasks_parent_task_id", ""))
    );
    assert_eq!(rows(&sql).len(), 3);
    let plan = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(["grandchild", "cycle-a"], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    assert!(
        plan.contains("USING INDEX idx_tasks_parent_task_id (parent_task_id=?)"),
        "{plan}"
    );
}

#[test]
fn reviewing_badge_reads_differently_for_the_consolidation_stage() {
    let reviewers = badge_presentation("reviewing", Some("reviewers"), None, "card");
    let consolidation = badge_presentation("reviewing", Some("consolidation"), None, "card");
    assert_eq!(reviewers.label, "AI reviewing…");
    assert_eq!(consolidation.label, "Consolidating review findings");
    assert_ne!(reviewers.system_image, consolidation.system_image);
    assert_ne!(reviewers.tooltip, consolidation.tooltip);
    // No stage (a legacy reviewer with no batch member) is the plain review.
    assert_eq!(badge_presentation("reviewing", None, None, "card"), reviewers);
}

#[test]
fn review_badge_labels_and_icons_cover_every_state() {
    let history = ReviewBadgeHistory {
        verdict_payload: None,
        revision_description: None,
        findings_revision_id: None,
        head: Some("abcdef12345".into()),
        reviewed_sha: Some("abcdef12345".into()),
        verdict: Some(("completed_with_findings".into(), "0".into(), 2)),
        revision: None,
    };
    for (state, label, icon) in [
        ("not_reviewed", "Not reviewed: latest commit", "questionmark.circle"),
        ("reviewing", "AI reviewing…", "brain"),
        ("review_queued", "AI review queued", "clock"),
        (
            "reviewed_with_findings",
            "AI review: 2 findings",
            "exclamationmark.circle.fill",
        ),
        ("reviewed_all_clear", "AI review: clean", "checkmark.seal.fill"),
        ("reviewed_clean_pending", "AI review: clean", "checkmark.seal.fill"),
        ("review_not_required", "AI review not required", "minus.circle"),
    ] {
        let badge = badge_presentation(state, None, Some(&history), "card");
        assert_eq!(badge.label, label, "{state}");
        assert_eq!(badge.system_image, icon, "{state}");
        assert!(
            badge
                .tooltip
                .contains("Last reviewed abcdef1 on 1970-01-01 00:00 UTC: 2 findings.")
        );
    }
}

#[test]
fn review_badge_head_moved_preserves_findings_and_revision_history() {
    let mut history = ReviewBadgeHistory {
        verdict_payload: None,
        revision_description: None,
        findings_revision_id: None,
        head: Some("newhead123".into()),
        reviewed_sha: Some("oldhead456".into()),
        verdict: Some(("completed_with_findings".into(), "0".into(), 2)),
        revision: Some(("Fix review findings".into(), "active".into(), false)),
    };
    let badge = badge_presentation("not_reviewed", None, Some(&history), "card");
    assert_eq!(badge.label, "Not reviewed: latest commit");
    assert!(badge.tooltip.contains("Last reviewed oldhead"));
    assert!(badge.tooltip.contains("Current head newhead differs"));
    assert!(badge.tooltip.contains("Fix review findings (Doing)"));
    assert!(!badge.tooltip.contains("Findings addressed"));
    history.revision = Some(("Fix review findings".into(), "in_review".into(), true));
    let badge = badge_presentation("not_reviewed", None, Some(&history), "card");
    assert!(
        badge
            .tooltip
            .contains("Findings addressed; the fix has not been AI-reviewed.")
    );
    assert_eq!(badge.label, "Not reviewed: latest commit");
    history.head = history.reviewed_sha.clone();
    let badge = badge_presentation("not_reviewed", None, Some(&history), "card");
    assert_eq!(badge.label, "Findings addressed");
    assert_eq!(badge.system_image, "checkmark.circle");
}

#[test]
fn review_badge_missing_history_is_explicit() {
    let history = ReviewBadgeHistory {
        verdict_payload: None,
        revision_description: None,
        findings_revision_id: None,
        head: None,
        reviewed_sha: None,
        verdict: None,
        revision: None,
    };
    assert!(
        badge_presentation("not_reviewed", None, Some(&history), "card")
            .tooltip
            .contains("No reviewed commit")
    );
    let legacy = ReviewBadgeHistory {
        reviewed_sha: Some("legacy1".into()),
        ..history
    };
    let badge = badge_presentation("not_reviewed", None, Some(&legacy), "card");
    assert!(badge.tooltip.contains("review time and verdict unavailable"));
    assert!(badge.tooltip.contains("Current PR head is not yet known"));
}

#[test]
fn review_badge_unknown_head_and_clean_pending_have_distinct_explanations() {
    assert!(
        badge_presentation("not_reviewed", None, None, "card")
            .tooltip
            .contains("no completed AI review")
    );
    let pending = badge_presentation("reviewed_clean_pending", None, None, "card");
    assert!(pending.tooltip.contains("AI review passed"));
    assert!(pending.tooltip.contains("prevent readiness"));
}

#[test]
fn owner_keyed_family_matches_the_card_walk_for_resolved_owners() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE tasks(id TEXT PRIMARY KEY, kind TEXT, parent_task_id TEXT);
        CREATE INDEX idx_tasks_parent_task_id ON tasks(parent_task_id);
        INSERT INTO tasks VALUES ('root','project_task',NULL), ('child','revision','root'),
        ('grandchild','revision','child'), ('other','chore',NULL), ('other-rev','revision','other');",
    )
    .unwrap();
    let rows = |key: ReviewKey, ids: [&str; 2]| {
        let sql = format!(
            "{} SELECT o.card, f.root, f.id FROM owners o JOIN family f ON f.root = o.id ORDER BY 1, 2, 3",
            review_family_cte("?1,?2", key)
        );
        conn.prepare(&sql)
            .unwrap()
            .query_map(ids, |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    // Cards resolve to their owner; the owner walk takes the roots as given.
    let by_card = rows(ReviewKey::Card, ["grandchild", "other-rev"]);
    let by_owner = rows(ReviewKey::Owner, ["root", "other"]);
    let family = |rows: &[(String, String, String)]| {
        let mut family: Vec<_> = rows.iter().map(|(_, root, id)| (root.clone(), id.clone())).collect();
        family.sort();
        family
    };
    assert_eq!(family(&by_card), family(&by_owner));
    assert_eq!(by_card.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), ["grandchild"; 3].iter().chain(["other-rev"; 2].iter()).copied().collect::<Vec<_>>());
    assert_eq!(by_owner.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), ["other", "other", "root", "root", "root"]);
    // A revision handed to the owner walk is not an owner and yields nothing.
    assert!(rows(ReviewKey::Owner, ["grandchild", "other-rev"]).is_empty());
}

#[test]
fn review_badge_self_link_is_decided_per_card() {
    let history = ReviewBadgeHistory {
        verdict_payload: None,
        revision_description: None,
        findings_revision_id: Some("rev".into()),
        head: Some("head".into()),
        reviewed_sha: Some("head".into()),
        verdict: Some(("completed_with_findings".into(), "0".into(), 1)),
        revision: Some(("Fix findings".into(), "active".into(), false)),
    };
    let on_root = badge_presentation("reviewed_with_findings", None, Some(&history), "root");
    assert!(on_root.tooltip.contains("Findings revision: Fix findings (Doing)."));
    let on_revision = badge_presentation("reviewed_with_findings", None, Some(&history), "rev");
    assert!(on_revision.tooltip.contains("This revision addresses the findings (Doing)."));
}

#[test]
fn trimming_keeps_findings_text_only_for_the_findings_state() {
    let history = || {
        ReviewBadgeHistory::builder()
            .verdict_payload("{}")
            .revision_description("brief")
            .reviewed_sha("abc")
            .build()
    };
    let kept = history().trimmed_for(Some(AI_REVIEW_STATE_REVIEWED_WITH_FINDINGS));
    assert_eq!(kept.verdict_payload.as_deref(), Some("{}"));
    assert_eq!(kept.revision_description.as_deref(), Some("brief"));
    for state in [Some(AI_REVIEW_STATE_NOT_REVIEWED), Some(AI_REVIEW_STATE_REVIEWED_ALL_CLEAR), None] {
        let trimmed = history().trimmed_for(state);
        assert!(trimmed.verdict_payload.is_none() && trimmed.revision_description.is_none());
        assert_eq!(trimmed.reviewed_sha.as_deref(), Some("abc"));
    }
}
