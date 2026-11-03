use super::*;
use boss_engine_test_git::jj::JjRepo;

fn fixture(root: &Path) -> JjRepo {
    let repo = JjRepo::new(root);
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    JjRepo::run(&repo.repo, &["git", "export"]);
    JjRepo::run(
        &repo.repo,
        &[
            "git",
            "remote",
            "add",
            "origin",
            repo.repo.join(".jj/repo/store/git").to_str().unwrap(),
        ],
    );
    JjRepo::run(&repo.repo, &["git", "fetch"]);
    repo
}

fn names(repo: &JjRepo) -> String {
    JjRepo::run(&repo.repo, &["bookmark", "list", "-T", "name ++ \"\\n\""])
}

async fn preserve_tip(jj: &LocalJj, record: &ExecutionBookmark, workspace: &Path) -> Result<Option<PreservedTip>> {
    preserve(jj, record, workspace, None, None).await
}

#[tokio::test]
async fn conflicted_variants_prefer_pr_ancestry_then_jj_evolution() {
    for on_pr in [0, 1, 2] {
        let dir = tempfile::tempdir().unwrap();
        let repo = fixture(dir.path());
        let record = create(&LocalJj, &repo.worker, "exec_divergent", "local").await.unwrap();
        std::fs::write(repo.worker.join("common"), "common unpublished parent").unwrap();
        JjRepo::run(&repo.worker, &["new", "-m", "Diverging child"]);
        std::fs::write(repo.worker.join("work"), "preserved work").unwrap();
        let saved = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
        let op = JjRepo::run(&repo.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
        for title in ["Earlier variant", "Middle variant", "Later variant"] {
            JjRepo::run(&repo.repo, &["--at-op", &op, "describe", &saved.commit_id, "-m", title]);
        }
        let variants = JjRepo::run(
            &repo.repo,
            &[
                "log",
                "--no-graph",
                "-r",
                &revision(&preserved_name(&record.execution_id)),
                "-T",
                "commit_id ++ \"\\n\"",
            ],
        );
        assert_eq!(variants.lines().count(), 3);
        let mut positioning = vec!["new", "-m", "Current PR head"];
        if on_pr == 0 {
            positioning.push("main");
        } else {
            positioning.push("description(substring:\"Earlier variant\")");
            if on_pr == 2 {
                positioning.push("description(substring:\"Middle variant\")");
            }
        }
        JjRepo::run(&repo.replacement, &positioning);
        let report = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
            .await
            .unwrap()
            .unwrap();
        if on_pr > 0 {
            assert!(
                report.commits.is_empty(),
                "PR history already contains the selected variant"
            );
        } else {
            assert!(report.commits.contains("Later variant"), "{}", report.commits);
        }
        assert_eq!(
            std::fs::read_to_string(repo.replacement.join("work")).unwrap(),
            "preserved work"
        );
    }
}

#[tokio::test]
async fn unpushed_tip_creates_exactly_one_bookmark_and_release_retry_reuses_it() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_preserve", "local").await.unwrap();
    let before = names(&repo);
    std::fs::write(repo.worker.join("work"), "uncommitted edits").unwrap();
    let tip = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    assert_eq!(tip.count, 1);
    assert_eq!(names(&repo).lines().count(), before.lines().count() + 1);
    assert!(names(&repo).contains("preserved/exec_preserve"));
    assert_eq!(
        JjRepo::run(&repo.worker, &["log", "-r", "@", "--no-graph", "-T", "change_id"]),
        tip.change_id
    );
    assert_eq!(preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap(), Some(tip));
    assert_eq!(names(&repo).lines().count(), before.lines().count() + 1);
}

#[tokio::test]
async fn later_unpublished_work_moves_the_preserved_bookmark() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_move", "local").await.unwrap();
    std::fs::write(repo.worker.join("first"), "first unpublished").unwrap();
    let first = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    JjRepo::run(&repo.worker, &["new", "-m", "Later unpublished work"]);
    std::fs::write(repo.worker.join("second"), "second unpublished").unwrap();
    let second = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    assert_ne!(second.commit_id, first.commit_id);
    assert_eq!(
        JjRepo::run(
            &repo.repo,
            &[
                "log",
                "--no-graph",
                "-r",
                &revision(&preserved_name(&record.execution_id)),
                "-T",
                "commit_id",
            ],
        ),
        second.commit_id
    );
    assert!(
        std::fs::read_to_string(repo.worker.join("second"))
            .unwrap()
            .contains("second")
    );
}

#[tokio::test]
async fn recycled_workspace_cannot_replace_unadopted_preserved_work() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_recycled", "local").await.unwrap();
    std::fs::write(repo.worker.join("first"), "original unpublished").unwrap();
    let saved = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    JjRepo::run(&repo.replacement, &["new", "main"]);
    std::fs::write(repo.replacement.join("other"), "another occupant").unwrap();
    let err = preserve_tip(&LocalJj, &record, &repo.replacement).await.unwrap_err();
    assert!(is_pointer_integrity_error(&err));
    assert_eq!(
        JjRepo::run(
            &repo.repo,
            &[
                "log",
                "--no-graph",
                "-r",
                &preserved_name(&record.execution_id),
                "-T",
                "commit_id"
            ]
        ),
        saved.commit_id
    );
}

#[tokio::test]
async fn published_work_and_empty_trailing_commit_create_no_bookmark() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_published", "local").await.unwrap();
    std::fs::write(repo.worker.join("work"), "published edits").unwrap();
    JjRepo::run(&repo.worker, &["bookmark", "set", "published", "-r", "@"]);
    JjRepo::run(&repo.worker, &["git", "export"]);
    JjRepo::run(&repo.repo, &["git", "fetch"]);
    JjRepo::run(&repo.worker, &["new", "-m", "Empty trailing commit"]);
    let before = names(&repo);
    assert_eq!(preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap(), None);
    assert_eq!(names(&repo), before);
}

#[tokio::test]
async fn missing_and_forgotten_workspace_uses_last_journaled_working_copy() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_lost", "local").await.unwrap();
    std::fs::write(repo.worker.join("work"), "journaled edits").unwrap();
    let tip = JjRepo::run(&repo.worker, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    std::fs::remove_dir_all(&repo.worker).unwrap();
    JjRepo::run(&repo.repo, &["workspace", "forget", "worker"]);
    for i in 0..270 {
        JjRepo::run(&repo.repo, &["describe", "-m", &format!("Unrelated operation {i}")]);
    }
    let saved = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    assert_eq!(saved.commit_id, tip);
    assert_eq!(saved.count, 1);
}

#[tokio::test]
async fn production_positioning_commits_are_not_restage_ancestors() {
    for description in [None, Some("Fresh execution")] {
        let dir = tempfile::tempdir().unwrap();
        let repo = fixture(dir.path());
        let record = create(&LocalJj, &repo.worker, "exec_position", "local").await.unwrap();
        std::fs::write(repo.worker.join("work"), "unpublished").unwrap();
        let saved = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
        let mut args = vec!["new", "main"];
        if let Some(description) = description {
            args.extend(["-m", description]);
        }
        JjRepo::run(&repo.replacement, &args);
        let report = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            report.base_sha,
            JjRepo::run(&repo.repo, &["log", "--no-graph", "-r", "main", "-T", "commit_id"])
        );
        assert!(
            JjRepo::run(
                &repo.replacement,
                &["log", "--no-graph", "-r", "(::@ ~ ::main) & empty()", "-T", "commit_id"]
            )
            .is_empty()
        );
    }
}

struct FailedRecordedLookup;

#[async_trait]
impl Jj for FailedRecordedLookup {
    async fn shared_repo(&self, _: &Path) -> Result<PathBuf> {
        Ok(PathBuf::from("store"))
    }
    async fn run(&self, _: &Path, args: &[&str]) -> Result<String> {
        if args.iter().any(|arg| arg.starts_with("present(")) {
            anyhow::bail!("SSH transport unavailable");
        }
        Ok(String::new())
    }
}

#[tokio::test]
async fn recorded_commit_transport_failure_is_not_an_integrity_verdict() {
    let record = ExecutionBookmark {
        execution_id: "exec_transport".into(),
        repo_path: "store".into(),
        host_id: "remote".into(),
        base_commit: None,
    };
    let saved = PreservedTip {
        change_id: "change".into(),
        commit_id: "commit".into(),
        count: 1,
    };
    let err = restore_preserved(&FailedRecordedLookup, &record, Path::new("workspace"), Some(&saved))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("SSH transport unavailable"));
    assert!(!is_pointer_integrity_error(&err));
}

#[tokio::test]
async fn restoration_follows_rewrites_and_keeps_change_ids_on_new_base() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_restore", "local").await.unwrap();
    std::fs::write(repo.worker.join("work"), "preserved edits").unwrap();
    let saved = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Rewritten description"]);
    JjRepo::run(&repo.replacement, &["new", "main", "-m", "New upstream"]);
    std::fs::write(repo.replacement.join("upstream"), "upstream edits").unwrap();
    let base = JjRepo::run(&repo.replacement, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    let report = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.base_sha, base);
    assert!(report.conflicts.is_empty());
    let empty = JjRepo::run(
        &repo.replacement,
        &[
            "log",
            "--no-graph",
            "-r",
            &format!("({base}..@) & empty()"),
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    );
    assert!(
        empty.trim().is_empty(),
        "restaging must not insert empty commits: {empty}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("work")).unwrap(),
        "preserved edits"
    );
    assert!(repo.replacement.join("upstream").exists());
    assert_eq!(
        JjRepo::run(&repo.replacement, &["log", "-r", "@", "--no-graph", "-T", "change_id"]),
        saved.change_id
    );
    create_from(
        &LocalJj,
        &repo.replacement,
        "exec_next",
        "local",
        None,
        report.inherited_base.as_deref(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn restoration_keeps_content_conflicts_for_worker_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_conflicts", "local").await.unwrap();
    std::fs::write(repo.worker.join("base.txt"), "worker edits\n").unwrap();
    let saved = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    JjRepo::run(&repo.replacement, &["new", "main", "-m", "Upstream edits"]);
    std::fs::write(repo.replacement.join("base.txt"), "upstream edits\n").unwrap();
    let report = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap()
        .unwrap();
    assert!(!report.conflicts.is_empty());
    assert!(report.instructions().contains("FIRST TASK"));
    let file = std::fs::read_to_string(repo.replacement.join("base.txt")).unwrap();
    assert!(file.contains("worker edits") && file.contains("upstream edits"));
}

#[tokio::test]
async fn missing_bookmark_falls_back_to_the_recorded_commit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_hidden", "local").await.unwrap();
    std::fs::write(repo.worker.join("hidden"), "recorded unpublished").unwrap();
    let saved = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    JjRepo::run(
        &repo.repo,
        &["bookmark", "delete", &preserved_name(&record.execution_id)],
    );
    JjRepo::run(&repo.replacement, &["new", "main", "-m", "Current PR head"]);
    let report = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap()
        .unwrap();
    assert!(report.commits.contains("recorded unpublished") || !report.commits.is_empty());
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("hidden")).unwrap(),
        "recorded unpublished"
    );
}

#[tokio::test]
async fn missing_bookmark_and_missing_commit_are_an_integrity_failure() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_gone", "local").await.unwrap();
    std::fs::write(repo.worker.join("gone"), "unpublished").unwrap();
    preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap();
    JjRepo::run(
        &repo.repo,
        &["bookmark", "delete", &preserved_name(&record.execution_id)],
    );
    let recorded = PreservedTip {
        change_id: "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".into(),
        // A well-formed id that is not the null revision and is not in this store.
        commit_id: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".into(),
        count: 1,
    };
    let err = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&recorded))
        .await
        .unwrap_err();
    assert!(is_pointer_integrity_error(&err), "{err:#}");
}

#[tokio::test]
async fn conflicted_preservation_requires_current_work_to_be_retained() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_variant", "local").await.unwrap();
    std::fs::write(repo.worker.join("work"), "saved work").unwrap();
    let saved = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    let op = JjRepo::run(&repo.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
    for title in ["First variant", "Second variant"] {
        JjRepo::run(&repo.repo, &["--at-op", &op, "describe", &saved.commit_id, "-m", title]);
    }
    let name = preserved_name(&record.execution_id);
    let variants = JjRepo::run(
        &repo.repo,
        &["log", "--no-graph", "-r", &name, "-T", "commit_id ++ \"\\n\""],
    );
    assert_eq!(variants.lines().count(), 2);
    JjRepo::run(&repo.replacement, &["edit", variants.lines().next().unwrap()]);
    assert!(preserve_tip(&LocalJj, &record, &repo.replacement).await.is_ok());
    JjRepo::run(&repo.replacement, &["new", "-m", "Additional work"]);
    std::fs::write(repo.replacement.join("extra"), "not yet retained").unwrap();
    let error = preserve_tip(&LocalJj, &record, &repo.replacement).await.unwrap_err();
    assert!(is_pointer_integrity_error(&error));
    assert_eq!(
        JjRepo::run(
            &repo.repo,
            &["log", "--no-graph", "-r", &name, "-T", "commit_id ++ \"\\n\""]
        ),
        variants
    );
}

#[tokio::test]
async fn reused_workspace_name_preserves_the_current_incarnation() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_reuse", "local").await.unwrap();
    std::fs::write(repo.worker.join("stale"), "previous occupant").unwrap();
    let stale = JjRepo::run(&repo.worker, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    std::fs::remove_dir_all(&repo.worker).unwrap();
    JjRepo::run(&repo.repo, &["workspace", "forget", "worker"]);
    JjRepo::run(
        &repo.repo,
        &[
            "workspace",
            "add",
            repo.worker.to_str().unwrap(),
            "--name",
            "worker",
            "-r",
            "main",
        ],
    );
    std::fs::write(repo.worker.join("fresh"), "new incarnation").unwrap();
    let fresh = JjRepo::run(&repo.worker, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    assert_ne!(fresh, stale);
    std::fs::remove_dir_all(&repo.worker).unwrap();
    let saved = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    assert_eq!(saved.commit_id, fresh);
}

#[tokio::test]
async fn forget_lookup_matches_the_workspace_name_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let extra = dir.path().join("worker-extra");
    JjRepo::run(
        &repo.repo,
        &[
            "workspace",
            "add",
            extra.to_str().unwrap(),
            "--name",
            "worker-extra",
            "-r",
            "main",
        ],
    );
    std::fs::write(extra.join("stale"), "other workspace").unwrap();
    let stale = JjRepo::run(&extra, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    std::fs::remove_dir_all(&extra).unwrap();
    JjRepo::run(&repo.repo, &["workspace", "forget", "worker-extra"]);
    let record = create(&LocalJj, &repo.worker, "exec_exact", "local").await.unwrap();
    std::fs::write(repo.worker.join("current"), "this workspace").unwrap();
    let current = JjRepo::run(&repo.worker, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    assert_ne!(current, stale);
    std::fs::remove_dir_all(&repo.worker).unwrap();
    JjRepo::run(&repo.repo, &["workspace", "forget", "worker"]);
    let saved = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    assert_eq!(saved.commit_id, current);
}

#[tokio::test]
async fn restoring_a_conflicted_bookmark_resolves_it_so_later_work_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_resolve", "local").await.unwrap();
    std::fs::write(repo.worker.join("work"), "saved work").unwrap();
    let saved = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    let op = JjRepo::run(&repo.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
    for title in ["First variant", "Second variant"] {
        JjRepo::run(&repo.repo, &["--at-op", &op, "describe", &saved.commit_id, "-m", title]);
    }
    let name = preserved_name(&record.execution_id);
    assert_eq!(
        JjRepo::run(
            &repo.repo,
            &["log", "--no-graph", "-r", &name, "-T", "commit_id ++ \"\\n\""]
        )
        .lines()
        .count(),
        2
    );
    JjRepo::run(&repo.replacement, &["new", "main"]);
    let report = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap()
        .unwrap();
    assert!(
        !report.discarded_tips.trim().is_empty(),
        "unselected variants must be named: {}",
        report.discarded_tips
    );
    assert_eq!(
        JjRepo::run(
            &repo.repo,
            &["log", "--no-graph", "-r", &name, "-T", "commit_id ++ \"\\n\""]
        )
        .lines()
        .count(),
        1
    );
    JjRepo::run(&repo.replacement, &["new", "-m", "Additional work"]);
    std::fs::write(repo.replacement.join("extra"), "later unpublished").unwrap();
    let updated = preserve_tip(&LocalJj, &record, &repo.replacement)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        JjRepo::run(&repo.repo, &["log", "--no-graph", "-r", &name, "-T", "commit_id"]),
        updated.commit_id
    );
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("extra")).unwrap(),
        "later unpublished"
    );
}

#[tokio::test]
async fn missing_bookmark_keeps_recorded_unpublished_work() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_recorded", "local").await.unwrap();
    std::fs::write(repo.worker.join("kept"), "recorded unpublished").unwrap();
    let saved = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    JjRepo::run(
        &repo.repo,
        &["bookmark", "delete", &preserved_name(&record.execution_id)],
    );
    JjRepo::run(&repo.replacement, &["new", "main"]);
    let kept = preserve(&LocalJj, &record, &repo.replacement, Some(&saved), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.commit_id, saved.commit_id);
    assert_eq!(
        JjRepo::run(
            &repo.repo,
            &[
                "log",
                "--no-graph",
                "-r",
                &preserved_name(&record.execution_id),
                "-T",
                "commit_id"
            ]
        ),
        saved.commit_id
    );
}

#[tokio::test]
async fn missing_bookmark_recorded_inspect_failure_is_not_empty_work() {
    let record = ExecutionBookmark {
        execution_id: "exec_inspect".into(),
        repo_path: "store".into(),
        host_id: "remote".into(),
        base_commit: None,
    };
    let saved = PreservedTip {
        change_id: "change".into(),
        commit_id: "commit".into(),
        count: 1,
    };
    let err = preserve(
        &FailedRecordedLookup,
        &record,
        Path::new("workspace"),
        Some(&saved),
        None,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("SSH transport unavailable"));
    assert!(!is_pointer_integrity_error(&err));
}
