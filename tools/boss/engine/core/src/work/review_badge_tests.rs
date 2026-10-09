use super::*;

#[test]
fn review_badge_labels_and_icons_cover_every_state() {
    let history = ReviewBadgeHistory {
        verdict_payload: None,
        revision_description: None,
        is_findings_revision: false,
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
        let badge = badge_presentation(state, Some(&history));
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
        is_findings_revision: false,
        head: Some("newhead123".into()),
        reviewed_sha: Some("oldhead456".into()),
        verdict: Some(("completed_with_findings".into(), "0".into(), 2)),
        revision: Some(("Fix review findings".into(), "active".into(), false)),
    };
    let badge = badge_presentation("not_reviewed", Some(&history));
    assert_eq!(badge.label, "Not reviewed: latest commit");
    assert!(badge.tooltip.contains("Last reviewed oldhead"));
    assert!(badge.tooltip.contains("Current head newhead differs"));
    assert!(badge.tooltip.contains("Fix review findings (Doing)"));
    assert!(!badge.tooltip.contains("Findings addressed"));
    history.revision = Some(("Fix review findings".into(), "in_review".into(), true));
    let badge = badge_presentation("not_reviewed", Some(&history));
    assert!(
        badge
            .tooltip
            .contains("Findings addressed; the fix has not been AI-reviewed.")
    );
    assert_eq!(badge.label, "Not reviewed: latest commit");
    history.head = history.reviewed_sha.clone();
    let badge = badge_presentation("not_reviewed", Some(&history));
    assert_eq!(badge.label, "Findings addressed");
    assert_eq!(badge.system_image, "checkmark.circle");
}

#[test]
fn review_badge_missing_history_is_explicit() {
    let history = ReviewBadgeHistory {
        verdict_payload: None,
        revision_description: None,
        is_findings_revision: false,
        head: None,
        reviewed_sha: None,
        verdict: None,
        revision: None,
    };
    assert!(
        badge_presentation("not_reviewed", Some(&history))
            .tooltip
            .contains("No reviewed commit")
    );
    let legacy = ReviewBadgeHistory {
        reviewed_sha: Some("legacy1".into()),
        ..history
    };
    let badge = badge_presentation("not_reviewed", Some(&legacy));
    assert!(badge.tooltip.contains("review time and verdict unavailable"));
    assert!(badge.tooltip.contains("Current PR head is not yet known"));
}

#[test]
fn review_badge_unknown_head_and_clean_pending_have_distinct_explanations() {
    assert!(
        badge_presentation("not_reviewed", None)
            .tooltip
            .contains("no completed AI review")
    );
    let pending = badge_presentation("reviewed_clean_pending", None);
    assert!(pending.tooltip.contains("AI review passed"));
    assert!(pending.tooltip.contains("prevent readiness"));
}
