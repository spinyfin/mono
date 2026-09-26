use super::*;
use boss_engine_test_git::jj::JjRepo;

#[test]
fn checkout_and_links_use_comparison_commits_despite_branch_and_working_file_changes() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let base = JjRepo::run(&repo.repo, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    JjRepo::run(&repo.repo, &["new", &base]);
    std::fs::write(repo.repo.join("caller.rs"), "fn caller() {\n    fixed();\n}\n").unwrap();
    JjRepo::run(&repo.repo, &["describe", "-m", "Add caller"]);
    let head = JjRepo::run(&repo.repo, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    let mut packet = crate::test_support::review_guide_source_packet(&base, &head);
    packet.merge_base_sha = base.clone();
    packet.files.clear();
    JjRepo::run(&repo.worker, &["new", &head]);
    assert!(verify(&repo.worker, &packet).is_ok());
    assert_eq!(
        git(&repo.worker, &["cat-file", "blob", &format!("{base}:base.txt")]).unwrap(),
        b"baseline\n"
    );
    JjRepo::run(&repo.repo, &["new", &head]);
    std::fs::write(repo.repo.join("caller.rs"), "unrelated new main\n").unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    assert!(
        verify(&repo.worker, &packet).is_ok(),
        "moving main cannot change the guide checkout"
    );
    let guide = |sha: &str, path: &str, range: &str| {
        format!(
            "# Guide\n## Problem\n## Fix\n## Walkthrough\n[code](https://github.com/{}/blob/{sha}/{path}#{range})\n## Tests",
            packet.head_repository
        )
    };
    assert!(validate(&repo.worker, &packet, &guide(&head, "caller.rs", "L1-L3")).is_ok());
    assert!(validate(&repo.worker, &packet, &guide(&base, "base.txt", "L1")).is_ok());
    for (sha, path, lines) in [
        (&head, "caller.rs", "L4"),
        (&base, "caller.rs", "L1"),
        (&head, "missing.rs", "L1"),
        (&head, "caller.rs", "L0"),
        (&head, "caller.rs", "L3-L1"),
    ] {
        assert!(validate(&repo.worker, &packet, &guide(sha, path, lines)).is_err());
    }
    for link in [
        format!(
            "<https://github.com/{}/blob/{head}/missing.rs#L1>",
            packet.head_repository
        ),
        format!(
            "[ref]: https://github.com/{}/blob/{head}/missing.rs#L1",
            packet.head_repository
        ),
    ] {
        assert!(
            validate(
                &repo.worker,
                &packet,
                &format!("{}\n{link}", guide(&head, "caller.rs", "L1"))
            )
            .is_err()
        );
    }
    std::fs::write(repo.worker.join("caller.rs"), "tampered\n").unwrap();
    assert!(
        verify(&repo.worker, &packet).is_err(),
        "dirty checkout must fail pin verification"
    );
    assert!(
        validate(&repo.worker, &packet, &guide(&head, "caller.rs", "L3")).is_ok(),
        "link validation reads git objects, not working files"
    );
    packet.head_sha = base;
    assert!(verify(&repo.worker, &packet).is_err(), "mismatched SHA must fail");
}
