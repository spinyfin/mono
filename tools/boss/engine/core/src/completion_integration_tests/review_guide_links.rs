#[test]
fn updated_guide_links_validate_at_the_new_head_and_reject_stale_head_links() {
    use crate::review_guide_workspace::{validate, verify_previous_head};
    use boss_engine_test_git::jj::JjRepo;
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let old = JjRepo::run(&repo.repo, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    JjRepo::run(&repo.repo, &["new", &old]);
    std::fs::write(repo.repo.join("new.rs"), "fn fixed() {}\n").unwrap();
    JjRepo::run(&repo.repo, &["describe", "-m", "Fix caller"]);
    let head = JjRepo::run(&repo.repo, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    let mut packet = crate::test_support::review_guide_source_packet(&old, &head);
    // The previous head is an inspection endpoint, not a valid current-guide link.
    packet.merge_base_sha = "a".repeat(40);
    let markdown = format!(
        "# Guide\n## Orientation\nA change.\n## Changed since the previous version\nThe caller now uses the fix.\n## Problem\n## Fix\n## Walkthrough\n[code](https://github.com/{}/blob/{head}/new.rs#L1)\n## Tests\n## Evidence\nNo validation results reported; no screenshots registered.",
        packet.head_repository
    );
    assert!(boss_review_guide::validate_update_section(&markdown).is_ok());
    assert!(validate(&repo.worker, &packet, &markdown).is_ok());
    assert!(validate(&repo.worker, &packet, &markdown.replace(&head, &old)).is_err());
    assert!(verify_previous_head(&repo.worker, &old).is_ok());
    assert!(verify_previous_head(&repo.worker, &"f".repeat(40)).is_err());
}
