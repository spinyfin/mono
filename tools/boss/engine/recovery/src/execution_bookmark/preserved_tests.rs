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

#[tokio::test]
async fn lease_boundary_reads_never_resurrect_hidden_work() {
    for (action, ownership) in ["commit", "squash", "abandon"]
        .into_iter()
        .flat_map(|action| [BoundaryOwnership::FreeUnproven, BoundaryOwnership::Reassigned].map(|o| (action, o)))
    {
        {
            let dir = tempfile::tempdir().unwrap();
            let repo = fixture(dir.path());
            let record = create(&LocalJj, &repo.worker, "exec_hidden_history", "local")
                .await
                .unwrap();
            if action == "squash" {
                std::fs::write(repo.worker.join("parent"), "parent work").unwrap();
                JjRepo::run(&repo.worker, &["new", "-m", "Child work"]);
            }
            std::fs::write(repo.worker.join("work"), "work to rewrite or abandon").unwrap();
            let hidden = JjRepo::run(&repo.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
            let boundary = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            match action {
                "commit" => {
                    JjRepo::run(&repo.worker, &["commit", "-m", "Published work"]);
                }
                "squash" => {
                    JjRepo::run(&repo.worker, &["squash", "-m", "Published combined work"]);
                }
                _ => {
                    JjRepo::run(&repo.worker, &["abandon", "@"]);
                }
            }
            if action != "abandon" {
                JjRepo::run(&repo.worker, &["bookmark", "set", "published", "-r", "@-"]);
                JjRepo::run(&repo.worker, &["git", "export"]);
                JjRepo::run(&repo.repo, &["git", "fetch"]);
            }
            let source = WorkspaceTipSource::LeaseBoundary {
                unix_millis: boundary,
                ownership,
            };
            assert!(
                !super::preserved::commit_is_visible(&LocalJj, &repo.repo, hidden.trim())
                    .await
                    .unwrap()
            );
            let outcome = preserve_from(&LocalJj, &record, &repo.worker, None, None, source)
                .await
                .unwrap();
            assert!(outcome.tip.is_none(), "{action}: {outcome:?}");
            assert!(outcome.inspect.is_none(), "{action}: {outcome:?}");
            assert!(!names(&repo).contains("preserved/exec_hidden_history"));
            assert!(
                !super::preserved::commit_is_visible(&LocalJj, &repo.repo, hidden.trim())
                    .await
                    .unwrap()
            );
        }
    }
}

async fn preserve_tip(jj: &LocalJj, record: &ExecutionBookmark, workspace: &Path) -> Result<Option<PreservedTip>> {
    Ok(preserve(jj, record, workspace, None, None).await?.tip)
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
    let outcome = preserve(&LocalJj, &record, &repo.replacement, None, None)
        .await
        .unwrap();
    assert!(outcome.inspect.is_some(), "teardown must inspect, not refuse");
    assert_eq!(
        outcome.tip.as_ref().map(|tip| tip.commit_id.as_str()),
        Some(saved.commit_id.as_str())
    );
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
        pr_head_before: None,
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
        // Not jj's root change id (32 `z`s), which `present()` would resolve.
        change_id: "kqkqkqkqkqkqkqkqkqkqkqkqkqkqkqkq".into(),
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
    let outcome = preserve(&LocalJj, &record, &repo.replacement, None, None)
        .await
        .unwrap();
    assert!(outcome.inspect.is_some(), "teardown must inspect, not refuse");
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
    assert!(
        !report.unrestaged_unpublished.trim().is_empty(),
        "unselected unpublished variants must keep a named archive: {}",
        report.unrestaged_unpublished
    );
    assert!(
        report.unrestaged_unpublished.contains("preserved-archive/"),
        "{}",
        report.unrestaged_unpublished
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
        .tip
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
        pr_head_before: None,
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

#[tokio::test]
async fn self_retry_amend_and_push_clears_preservation_without_resurrecting_hidden_commit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_retry", "local").await.unwrap();
    std::fs::write(repo.worker.join("work"), "unpublished").unwrap();
    let saved = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    JjRepo::run(&repo.replacement, &["new", "main", "-m", "Current PR head"]);
    restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap()
        .unwrap();
    std::fs::write(repo.replacement.join("work"), "amended and published").unwrap();
    JjRepo::run(&repo.replacement, &["status"]);
    let change = JjRepo::run(&repo.replacement, &["log", "-r", "@", "--no-graph", "-T", "change_id"]);
    assert_eq!(change, saved.change_id);
    let rewritten = JjRepo::run(&repo.replacement, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    assert_ne!(rewritten, saved.commit_id);
    JjRepo::run(&repo.replacement, &["bookmark", "set", "published", "-r", "@"]);
    JjRepo::run(&repo.replacement, &["git", "export"]);
    JjRepo::run(&repo.repo, &["git", "fetch"]);
    JjRepo::run(&repo.replacement, &["new", "-m", "Empty trailing commit"]);
    let outcome = preserve(&LocalJj, &record, &repo.replacement, Some(&saved), None)
        .await
        .unwrap();
    assert!(outcome.tip.is_none(), "published rewritten work must clear the row");
    assert!(outcome.inspect.is_none());
    let listed = names(&repo);
    assert!(
        !listed.lines().any(|name| name == "preserved/exec_retry"),
        "local preserved bookmark must be deleted: {listed}"
    );
    let visible = JjRepo::run(
        &repo.repo,
        &[
            "log",
            "--no-graph",
            "-r",
            &format!("present({})", saved.change_id),
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    );
    assert_eq!(
        visible.lines().count(),
        1,
        "the change must not be divergent: {visible}"
    );
    assert!(!visible.lines().any(|id| id == saved.commit_id));
}

#[tokio::test]
async fn missing_bookmark_and_absent_recorded_preserves_fresh_unpublished_tip() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_absent", "local").await.unwrap();
    let recorded = PreservedTip {
        change_id: "kqkqkqkqkqkqkqkqkqkqkqkqkqkqkqkq".into(),
        commit_id: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".into(),
        count: 1,
    };
    std::fs::write(repo.worker.join("fresh"), "new unpublished").unwrap();
    let outcome = preserve(&LocalJj, &record, &repo.worker, Some(&recorded), None)
        .await
        .unwrap();
    let tip = outcome.tip.expect("fresh unpublished work must be recorded");
    assert_eq!(tip.count, 1);
    assert!(names(&repo).contains("preserved/exec_absent"));
    let inspect = outcome.inspect.expect("absent recorded tip must raise inspect");
    assert_eq!(inspect.existing_commit_id.as_deref(), Some(recorded.commit_id.as_str()));
    assert_eq!(inspect.workspace_commit_id.as_deref(), Some(tip.commit_id.as_str()));
    assert_eq!(inspect.workspace_count, Some(1));
}

#[tokio::test]
async fn abandoned_restaged_tip_is_consumed_and_not_restaged() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_abandon", "local").await.unwrap();
    std::fs::write(repo.worker.join("work"), "unpublished").unwrap();
    let saved = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    JjRepo::run(&repo.replacement, &["new", "main", "-m", "Current PR head"]);
    restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap()
        .unwrap();
    JjRepo::run(&repo.replacement, &["abandon", "@"]);
    let outcome = preserve(&LocalJj, &record, &repo.replacement, Some(&saved), None)
        .await
        .unwrap();
    assert!(outcome.tip.is_none(), "abandoned work must clear the recorded tip");
    assert!(outcome.inspect.is_none());
    assert!(
        restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
            .await
            .unwrap()
            .is_none()
    );
    assert!(!repo.replacement.join("work").exists());
}

#[tokio::test]
async fn hidden_recorded_tip_does_not_replace_fresh_unpublished_work() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_hidden_fresh", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("work"), "unpublished").unwrap();
    let saved = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    JjRepo::run(&repo.replacement, &["new", "main", "-m", "Current PR head"]);
    restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap()
        .unwrap();
    JjRepo::run(&repo.replacement, &["abandon", "@"]);
    JjRepo::run(&repo.replacement, &["new", "-m", "Fresh unpublished tip"]);
    std::fs::write(repo.replacement.join("fresh"), "new unpublished").unwrap();
    let outcome = preserve(&LocalJj, &record, &repo.replacement, Some(&saved), None)
        .await
        .unwrap();
    let tip = outcome.tip.expect("fresh unpublished work must be recorded");
    assert_ne!(tip.commit_id, saved.commit_id);
    assert!(names(&repo).contains("preserved/exec_hidden_fresh"));
    assert!(
        outcome.inspect.is_none(),
        "abandoning restaged work and starting a new unpublished tip is routine; no inspect verdict"
    );
    assert!(
        restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
            .await
            .unwrap()
            .is_some()
    );
    assert!(!repo.replacement.join("work").exists());
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("fresh")).unwrap(),
        "new unpublished"
    );
}

#[tokio::test]
async fn historical_tip_does_not_snapshot_a_reassigned_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_reassigned", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("ours.txt"), "reaped worker edits").unwrap();
    let ours = JjRepo::run(&repo.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    let boundary = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    JjRepo::run(&repo.worker, &["new", "main", "-m", "Unrelated holder stack"]);
    std::fs::write(repo.worker.join("theirs.txt"), "other holder's unpublished").unwrap();
    let theirs = JjRepo::run(&repo.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    assert_ne!(ours.trim(), theirs.trim());
    let outcome = preserve_from(
        &LocalJj,
        &record,
        &repo.worker,
        None,
        None,
        WorkspaceTipSource::LeaseBoundary {
            unix_millis: boundary,
            ownership: BoundaryOwnership::Reassigned,
        },
    )
    .await
    .unwrap();
    assert!(
        outcome.inspect.is_none(),
        "a reassigned workspace's own stack is not reported against this execution: {outcome:?}"
    );
    let tip = outcome
        .tip
        .expect("historical lookup must recover the reaped worker tip");
    assert_eq!(tip.commit_id, ours.trim());
    assert_ne!(tip.commit_id, theirs.trim());
    assert_eq!(
        JjRepo::run(&repo.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]).trim(),
        theirs.trim(),
        "the other holder's stack must not be rewritten"
    );
    assert_eq!(
        std::fs::read_to_string(repo.worker.join("theirs.txt")).unwrap(),
        "other holder's unpublished"
    );
}

#[tokio::test]
async fn restore_reports_only_archives_it_created() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_legacy_archive", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("work"), "preserved work").unwrap();
    let saved = preserve_tip(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    JjRepo::run(&repo.worker, &["new", "main", "-m", "Legacy integrity archive"]);
    std::fs::write(repo.worker.join("legacy.txt"), "pointer-integrity archive").unwrap();
    let legacy = JjRepo::run(&repo.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    let archive = preserved_archive_name(&record.execution_id, legacy.trim());
    JjRepo::run(
        &repo.repo,
        &["bookmark", "set", &archive, "-r", legacy.trim(), "--allow-backwards"],
    );
    JjRepo::run(&repo.replacement, &["new", "main", "-m", "Current PR head"]);
    let report = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap()
        .unwrap();
    assert!(
        !report.unrestaged_unpublished.contains(&archive),
        "legacy integrity archives must not be reported as unrestaged variants: {}",
        report.unrestaged_unpublished
    );
}

async fn now_millis() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    now
}

fn tip_of_workspace(repo: &JjRepo) -> String {
    JjRepo::run(&repo.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"])
        .trim()
        .to_owned()
}

/// A store with many operations in which the workspace never resolves: the
/// journal is listed in full, but the walk may load only a bounded number of
/// operation views.
struct CountingViews {
    loads: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl Jj for CountingViews {
    async fn run(&self, repo: &Path, args: &[&str]) -> Result<String> {
        if args.contains(&"--at-op") {
            self.loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(String::new());
        }
        if args.iter().any(|arg| arg.starts_with("time.end().format(\"%s.%f\")")) {
            return Ok((0..2_000)
                .map(|n| format!("{}.000000000 op{n}\n", 1_700_000_000 + n))
                .collect());
        }
        LocalJj.run(repo, args).await
    }

    async fn shared_repo(&self, workspace: &Path) -> Result<PathBuf> {
        LocalJj.shared_repo(workspace).await
    }
}

#[tokio::test]
async fn journal_walks_load_a_bounded_number_of_operation_views() {
    for source in [
        WorkspaceTipSource::Live,
        WorkspaceTipSource::LeaseBoundary {
            unix_millis: now_millis().await + 60_000,
            ownership: BoundaryOwnership::Reassigned,
        },
    ] {
        let dir = tempfile::tempdir().unwrap();
        let repo = fixture(dir.path());
        let record = create(&LocalJj, &repo.worker, "exec_view_bound", "local")
            .await
            .unwrap();
        let jj = CountingViews {
            loads: Default::default(),
        };
        let outcome = preserve_from(&jj, &record, &repo.worker.join("gone"), None, None, source)
            .await
            .unwrap();
        assert!(outcome.tip.is_none(), "{source:?}: {outcome:?}");
        let loads = jj.loads.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            loads <= preserved::OP_VIEW_LOAD_LIMIT,
            "{source:?}: {loads} operation views were loaded"
        );
    }
}

#[tokio::test]
async fn unknown_boundary_resolves_nothing_and_raises_inspection() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_unknown_boundary", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("work"), "work").unwrap();
    let outcome = preserve_from(
        &LocalJj,
        &record,
        &repo.worker,
        None,
        None,
        WorkspaceTipSource::UnknownBoundary,
    )
    .await
    .unwrap();
    assert!(outcome.tip.is_none());
    assert!(outcome.inspect.is_some());
}

#[tokio::test]
async fn lease_boundary_never_snapshots_a_successors_edits() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_successor_edits", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("ours.txt"), "first holder").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let ours = tip_of_workspace(&repo);
    let boundary = now_millis().await;
    // A successor leases the workspace dirty, writes without running jj, and
    // releases: the registered tip is unchanged.
    std::fs::write(repo.worker.join("theirs.txt"), "successor edits").unwrap();
    let outcome = preserve_from(
        &LocalJj,
        &record,
        &repo.worker,
        None,
        None,
        WorkspaceTipSource::LeaseBoundary {
            unix_millis: boundary,
            ownership: BoundaryOwnership::Reassigned,
        },
    )
    .await
    .unwrap();
    assert!(outcome.inspect.is_none(), "{outcome:?}");
    assert_eq!(outcome.tip.unwrap().commit_id, ours);
    let preserved = JjRepo::run(
        &repo.repo,
        &[
            "log",
            "--no-graph",
            "-r",
            &revision(&preserved_name("exec_successor_edits")),
            "-T",
            "commit_id",
        ],
    );
    assert_eq!(preserved.trim(), ours);
    let files = JjRepo::run(
        &repo.repo,
        &["file", "list", "-r", &revision(&preserved_name("exec_successor_edits"))],
    );
    assert!(files.contains("ours.txt") && !files.contains("theirs.txt"), "{files}");
}

fn free_unproven(boundary: i64) -> WorkspaceTipSource {
    WorkspaceTipSource::LeaseBoundary {
        unix_millis: boundary,
        ownership: BoundaryOwnership::FreeUnproven,
    }
}

/// A worker dies after its last confirmed lease boundary. Whatever it did
/// next, with no jj command between its last write and teardown, must surface
/// as an inspection item naming the live registered tip.
#[tokio::test]
async fn work_after_the_boundary_is_reported_not_dropped() {
    for action in ["commit", "abandon", "new", "edit"] {
        let dir = tempfile::tempdir().unwrap();
        let repo = fixture(dir.path());
        let record = create(&LocalJj, &repo.worker, "exec_post_boundary", "local")
            .await
            .unwrap();
        std::fs::write(repo.worker.join("first.txt"), "first").unwrap();
        JjRepo::run(&repo.worker, &["status"]);
        let boundary = now_millis().await;
        match action {
            "commit" => {
                JjRepo::run(&repo.worker, &["commit", "-m", "Later work"]);
                std::fs::write(repo.worker.join("later.txt"), "later").unwrap();
            }
            "abandon" => {
                JjRepo::run(&repo.worker, &["abandon", "@"]);
                std::fs::write(repo.worker.join("redo.txt"), "redo").unwrap();
            }
            "new" => {
                JjRepo::run(&repo.worker, &["new", "-m", "Newer change"]);
                std::fs::write(repo.worker.join("newer.txt"), "newer").unwrap();
            }
            _ => std::fs::write(repo.worker.join("first.txt"), "edited without any jj command").unwrap(),
        }
        let outcome = preserve_from(&LocalJj, &record, &repo.worker, None, None, free_unproven(boundary))
            .await
            .unwrap();
        // Inspecting the live tip afterwards is safe: preservation has already
        // snapshotted the free workspace.
        let live = tip_of_workspace(&repo);
        let inspect = outcome
            .inspect
            .as_ref()
            .unwrap_or_else(|| panic!("{action}: post-boundary work was dropped silently: {outcome:?}"));
        assert_eq!(inspect.workspace_commit_id.as_deref(), Some(live.as_str()), "{action}");
        assert!(inspect.workspace_count >= Some(1), "{action}: {inspect:?}");
        assert!(inspect.reason.contains("lease boundary"), "{action}: {inspect:?}");
        // Reported work is never bookmarked as this execution's.
        let preserved = JjRepo::run(
            &repo.repo,
            &[
                "log",
                "--no-graph",
                "-r",
                &format!("present({})", revision(&preserved_name("exec_post_boundary"))),
                "-T",
                "commit_id",
            ],
        );
        assert_ne!(preserved.trim(), live, "{action}: live tip must not be bookmarked");
    }
}

#[tokio::test]
async fn a_reassigned_workspace_is_never_snapshotted_or_reported() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_reassigned_quiet", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("ours.txt"), "first holder").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let boundary = now_millis().await;
    JjRepo::run(&repo.worker, &["new", "main", "-m", "Other holder stack"]);
    std::fs::write(repo.worker.join("theirs.txt"), "other holder, unsnapshotted").unwrap();
    let before = JjRepo::run(
        &repo.repo,
        &[
            "--ignore-working-copy",
            "op",
            "log",
            "--no-graph",
            "-n",
            "1",
            "-T",
            "id",
        ],
    );
    let outcome = preserve_from(
        &LocalJj,
        &record,
        &repo.worker,
        None,
        None,
        WorkspaceTipSource::LeaseBoundary {
            unix_millis: boundary,
            ownership: BoundaryOwnership::Reassigned,
        },
    )
    .await
    .unwrap();
    assert!(outcome.inspect.is_none(), "{outcome:?}");
    assert_eq!(
        JjRepo::run(
            &repo.repo,
            &[
                "--ignore-working-copy",
                "op",
                "log",
                "--no-graph",
                "-n",
                "2",
                "-T",
                "id ++ \"\\n\""
            ]
        )
        .lines()
        .nth(1),
        Some(before.trim()),
        "teardown of a reassigned workspace may bookmark but must not snapshot its working copy"
    );
}

/// A successor that inherits the boundary change, edits it and runs any jj
/// command rewrites it under the same change id. That rewrite is the
/// successor's work: it is never bookmarked as this execution's.
#[tokio::test]
async fn a_successors_rewrite_of_the_inherited_change_is_not_adopted() {
    for ownership in [BoundaryOwnership::Reassigned, BoundaryOwnership::FreeUnproven] {
        for successor_runs_jj in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let repo = fixture(dir.path());
            let record = create(&LocalJj, &repo.worker, "exec_inherited_rewrite", "local")
                .await
                .unwrap();
            std::fs::write(repo.worker.join("ours.txt"), "first holder").unwrap();
            JjRepo::run(&repo.worker, &["status"]);
            let boundary = now_millis().await;
            std::fs::write(repo.worker.join("theirs.txt"), "successor edits").unwrap();
            if successor_runs_jj {
                JjRepo::run(&repo.worker, &["status"]);
            }
            let outcome = preserve_from(
                &LocalJj,
                &record,
                &repo.worker,
                None,
                None,
                WorkspaceTipSource::LeaseBoundary {
                    unix_millis: boundary,
                    ownership,
                },
            )
            .await
            .unwrap();
            let label = format!("{ownership:?} jj={successor_runs_jj}");
            let preserved = JjRepo::run(
                &repo.repo,
                &[
                    "bookmark",
                    "list",
                    "-T",
                    "name ++ \"\\n\"",
                    "preserved/exec_inherited_rewrite",
                ],
            );
            if successor_runs_jj {
                // The inherited change was rewritten to include the successor's
                // file: nothing may be bookmarked, and inspection is raised.
                assert!(preserved.trim().is_empty(), "{label}: bookmarked {preserved}");
                assert!(outcome.tip.is_none(), "{label}: {outcome:?}");
                let inspect = outcome
                    .inspect
                    .as_ref()
                    .unwrap_or_else(|| panic!("{label}: {outcome:?}"));
                assert!(inspect.workspace_count >= Some(1), "{label}: {inspect:?}");
            } else {
                // The registered boundary commit was untouched when it was
                // resolved, so it is the safe tip. A free workspace is
                // snapshotted only afterwards, so the snapshot cannot hide it,
                // and the snapshot's work is reported rather than adopted.
                assert_eq!(
                    outcome.inspect.is_some(),
                    ownership == BoundaryOwnership::FreeUnproven,
                    "{label}: {outcome:?}"
                );
                let files = JjRepo::run(
                    &repo.repo,
                    &[
                        "file",
                        "list",
                        "-r",
                        &revision(&preserved_name("exec_inherited_rewrite")),
                    ],
                );
                assert!(
                    files.contains("ours.txt") && !files.contains("theirs.txt"),
                    "{label}: {files}"
                );
            }
        }
    }
}

#[tokio::test]
async fn unpublished_amendment_of_a_pushed_change_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_amended_push", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("work"), "pushed").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Pushed work"]);
    JjRepo::run(&repo.worker, &["bookmark", "set", "published", "-r", "@"]);
    JjRepo::run(&repo.worker, &["git", "export"]);
    JjRepo::run(&repo.repo, &["git", "fetch"]);
    std::fs::write(repo.worker.join("work"), "pushed then amended").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let amended = tip_of_workspace(&repo);
    let boundary = now_millis().await;
    JjRepo::run(&repo.worker, &["new", "main"]);
    let outcome = preserve_from(
        &LocalJj,
        &record,
        &repo.worker,
        None,
        None,
        WorkspaceTipSource::LeaseBoundary {
            unix_millis: boundary,
            ownership: BoundaryOwnership::Reassigned,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        outcome.tip.as_ref().map(|tip| tip.commit_id.as_str()),
        Some(amended.as_str())
    );
    let preserved = JjRepo::run(
        &repo.repo,
        &[
            "log",
            "--no-graph",
            "-r",
            &revision(&preserved_name("exec_amended_push")),
            "-T",
            "commit_id",
        ],
    );
    assert_eq!(preserved.trim(), amended);
}
