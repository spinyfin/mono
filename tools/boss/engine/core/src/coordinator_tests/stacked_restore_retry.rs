use super::blocked_workspace::chain_harness;
use super::helpers::*;
use crate::coordinator::{CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus};
use crate::host_adapter::HostAdapter;
use boss_engine_recovery::execution_bookmark::{ExecutionBookmark, RestoreReport};

/// Delegates to a real adapter, but fails the first
/// `restore_preserved_work` with an ordinary (transient) error and
/// records every base it is asked to restage onto.
struct FlakyRestoreAdapter {
    inner: Arc<dyn HostAdapter>,
    bases: std::sync::Mutex<Vec<String>>,
    integrity: Option<boss_engine_recovery::execution_bookmark::PointerIntegrityKind>,
}

#[async_trait]
impl HostAdapter for FlakyRestoreAdapter {
    async fn restore_preserved_work(
        &self,
        record: &ExecutionBookmark,
        workspace: &Path,
        recorded: Option<&boss_engine_recovery::execution_bookmark::PreservedTip>,
    ) -> Result<Option<RestoreReport>> {
        let base = self.inner.positioned_parent(workspace).await?;
        let first = {
            let mut bases = self.bases.lock().unwrap();
            bases.push(base);
            bases.len() == 1
        };
        if first {
            if let Some(kind) = self.integrity {
                return Err(boss_engine_recovery::execution_bookmark::pointer_integrity_error_of(
                    kind,
                    "injected integrity verdict",
                ));
            }
            return Err(anyhow!("injected transient fetch failure"));
        }
        self.inner.restore_preserved_work(record, workspace, recorded).await
    }

    async fn recovery_pr_base(&self, origin: &str, pr: u64) -> Result<String> {
        self.inner.recovery_pr_base(origin, pr).await
    }

    async fn release_execution_bookmark_heads(
        &self,
        record: &ExecutionBookmark,
        release_names: bool,
    ) -> Result<Vec<(String, String)>> {
        self.inner.release_execution_bookmark_heads(record, release_names).await
    }

    async fn execution_bookmark_diff(&self, record: &ExecutionBookmark) -> Result<String> {
        self.inner.execution_bookmark_diff(record).await
    }

    fn host_id(&self) -> &str {
        self.inner.host_id()
    }

    async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle> {
        self.inner.ensure_repo(origin).await
    }

    async fn lease_workspace(
        &self,
        repo_id: &str,
        task: &str,
        prefer_workspace_id: Option<&str>,
        allow_dirty: bool,
        exclude_workspace_ids: &[&str],
    ) -> Result<CubeWorkspaceLease> {
        self.inner
            .lease_workspace(repo_id, task, prefer_workspace_id, allow_dirty, exclude_workspace_ids)
            .await
    }

    async fn release_workspace(&self, lease_id: &str) -> Result<()> {
        self.inner.release_workspace(lease_id).await
    }

    async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()> {
        self.inner.heartbeat_lease(lease_id, ttl_seconds).await
    }

    async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()> {
        self.inner.force_release_lease(lease_id, reason).await
    }

    async fn create_change(&self, workspace_path: &Path, title: &str) -> Result<CubeChangeHandle> {
        self.inner.create_change(workspace_path, title).await
    }

    async fn positioned_parent(&self, workspace: &Path) -> Result<String> {
        self.inner.positioned_parent(workspace).await
    }

    async fn goto_workspace(&self, workspace_path: &Path, pr: u64) -> Result<()> {
        self.inner.goto_workspace(workspace_path, pr).await
    }

    async fn goto_workspace_revision(&self, workspace_path: &Path, sha: &str) -> Result<()> {
        self.inner.goto_workspace_revision(workspace_path, sha).await
    }

    async fn start_fresh_workspace(&self, workspace: &Path) -> Result<String> {
        self.inner.start_fresh_workspace(workspace).await
    }

    async fn create_execution_bookmark(
        &self,
        workspace: &Path,
        execution_id: &str,
        predecessor: Option<&ExecutionBookmark>,
        inherited_base: Option<&str>,
    ) -> Result<ExecutionBookmark> {
        self.inner
            .create_execution_bookmark(workspace, execution_id, predecessor, inherited_base)
            .await
    }

    async fn discard_orphaned_execution_baseline(&self, record: &ExecutionBookmark) -> Result<()> {
        self.inner.discard_orphaned_execution_baseline(record).await
    }

    async fn workspace_status(&self, workspace_path: &Path) -> Result<CubeWorkspaceStatus> {
        self.inner.workspace_status(workspace_path).await
    }

    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        self.inner.list_workspaces().await
    }

    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        self.inner.list_repos().await
    }

    fn command_repr(&self, args: &[&str]) -> Option<(String, String)> {
        self.inner.command_repr(args)
    }

    async fn spawn_worker(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &Path,
        cube_change_id: Option<&str>,
    ) -> Result<RunOutcome> {
        self.inner
            .spawn_worker(worker_id, execution, work_item, workspace_path, cube_change_id)
            .await
    }
}

#[tokio::test]
async fn restore_transport_failure_retries_on_the_same_stacked_base() {
    use boss_engine_test_git::jj::JjRepo;
    let h = chain_harness(true, true, false).await;
    JjRepo::run(&h.repo.repo, &["new", "main", "-m", "Stacked parent"]);
    std::fs::write(h.repo.repo.join("parent.txt"), "parent").unwrap();
    JjRepo::run(&h.repo.repo, &["bookmark", "set", "stack-parent", "-r", "@"]);
    // The PR head descends from its stack parent, as a real stacked PR does.
    JjRepo::run(&h.repo.repo, &["new", "stack-parent", "-m", "Stacked PR head"]);
    std::fs::write(h.repo.repo.join("pr-head.txt"), "pr head").unwrap();
    JjRepo::run(
        &h.repo.repo,
        &["bookmark", "set", "pr/99", "-r", "@", "--allow-backwards"],
    );
    JjRepo::run(&h.repo.repo, &["git", "export"]);
    *h.cube.pr_base.lock().await = Some("stack-parent".into());
    let lease = CubeWorkspaceLease {
        lease_id: "lease-transient".into(),
        workspace_id: "replacement".into(),
        workspace_path: h.repo.replacement.clone(),
        dirty_verified: Some(true),
    };
    let flaky = Arc::new(FlakyRestoreAdapter {
        inner: h.coordinator.host_adapter().clone(),
        bases: std::sync::Mutex::new(Vec::new()),
        integrity: None,
    });
    let adapter: Arc<dyn HostAdapter> = flaky.clone();
    let source = h.coordinator.work_db().recovery_predecessor(&h.next).unwrap().unwrap();
    let record = h.coordinator.work_db().execution_bookmark(&source.id).unwrap();
    let preserved = JjRepo::run(
        &h.repo.repo,
        &["log", "-r", &record.head(), "--no-graph", "-T", "commit_id"],
    );

    let error = h
        .coordinator
        .recover_execution_bookmark_for_test(&h.next, &lease, &adapter, "mono", Some(99), Some(99))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("injected transient fetch failure"));
    assert!(
        h.coordinator
            .work_db()
            .execution_bookmark_optional(&source.id)
            .unwrap()
            .is_some()
    );
    assert!(
        h.coordinator
            .work_db()
            .execution_bookmark_retired_reason(&source.id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        JjRepo::run(
            &h.repo.repo,
            &["log", "-r", &record.head(), "--no-graph", "-T", "commit_id"]
        ),
        preserved
    );
    assert!(
        JjRepo::run(
            &h.repo.repo,
            &["bookmark", "list", &format!("glob:preserved-archive/{}/*", source.id)]
        )
        .is_empty()
    );
    let recovered = h
        .coordinator
        .recover_execution_bookmark_for_test(&h.next, &lease, &adapter, "mono", Some(99), Some(99))
        .await
        .unwrap()
        .unwrap();
    assert!(recovered.has_work);
    let bases = flaky.bases.lock().unwrap();
    assert_eq!(bases.len(), 2);
    assert_eq!(bases[0], bases[1]);
    drop(bases);
    assert_eq!(
        std::fs::read_to_string(h.repo.replacement.join("revision.txt")).unwrap(),
        "unpushed revision"
    );
    let parent_commits = JjRepo::run(
        &h.repo.replacement,
        &[
            "log",
            "-r",
            "description(substring:\"Stacked parent\") & ::@",
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    );
    assert_eq!(parent_commits.lines().count(), 1, "{parent_commits}");
}

#[tokio::test]
async fn every_integrity_verdict_retains_saved_work_and_allows_fresh_dispatch() {
    use boss_engine_recovery::execution_bookmark::PointerIntegrityKind;
    use boss_engine_test_git::jj::JjRepo;
    for kind in [PointerIntegrityKind::Other, PointerIntegrityKind::Quarantined] {
        let h = chain_harness(true, true, false).await;
        let adapter: Arc<dyn HostAdapter> = Arc::new(FlakyRestoreAdapter {
            inner: h.coordinator.host_adapter().clone(),
            bases: std::sync::Mutex::new(Vec::new()),
            integrity: Some(kind),
        });
        let lease = CubeWorkspaceLease {
            lease_id: "lease-integrity".into(),
            workspace_id: "replacement".into(),
            workspace_path: h.repo.replacement.clone(),
            dirty_verified: Some(true),
        };
        let db = &h.coordinator.work_db();
        let prior = db.recovery_predecessor(&h.next).unwrap().unwrap();
        assert!(
            h.coordinator
                .recover_execution_bookmark_for_test(&h.next, &lease, &adapter, "mono", Some(99), Some(99),)
                .await
                .unwrap()
                .is_none(),
            "{kind:?}"
        );
        assert!(db.preserved_tip(&prior.id).unwrap().is_some(), "{kind:?}");
        assert!(
            !JjRepo::run(&h.repo.repo, &["bookmark", "list", &format!("preserved/{}", prior.id)]).is_empty(),
            "{kind:?}"
        );
        let notes = db.list_attention_items(&prior.id).unwrap();
        assert!(
            notes
                .iter()
                .any(|note| note.body_markdown.contains("injected integrity verdict")),
            "{kind:?}"
        );
        h.dispatch().await.unwrap();
        let after = db.get_execution(&h.next.id).unwrap();
        assert_eq!(after.status, ExecutionStatus::Running, "{kind:?}");
        assert_eq!(after.pre_start_failure_count, 0, "{kind:?}");
    }
}

#[tokio::test]
async fn legacy_conflicts_remain_untouched_and_never_prevent_start() {
    use boss_engine_recovery::execution_bookmark::{LocalJj, PointerIntegrityKind, diff, pointer_integrity_kind};
    use boss_engine_test_git::jj::JjRepo;
    for kind in [
        ExecutionKind::AnswerAgent,
        ExecutionKind::AutomationTriage,
        ExecutionKind::ChoreImplementation,
        ExecutionKind::CiRemediation,
        ExecutionKind::ConflictResolution,
        ExecutionKind::InvestigationImplementation,
        ExecutionKind::PrReview,
        ExecutionKind::PrReviewGuide,
        ExecutionKind::ProductDesign,
        ExecutionKind::ProjectDesign,
        ExecutionKind::RevisionImplementation,
        ExecutionKind::TaskImplementation,
    ] {
        let mut h = chain_harness(true, true, false).await;
        let db = &h.coordinator.work_db();
        // Resolve the fixture predecessor before changing its execution kind.
        let prior = db.recovery_predecessor(&h.next).unwrap().unwrap();
        db.clear_preserved_tip(&prior.id).unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET kind = ?1 WHERE id IN (?2, ?3)",
                rusqlite::params![kind.as_str(), prior.id, h.next.id],
            )
            .unwrap();
        h.next = db.get_execution(&h.next.id).unwrap();
        let record = db.execution_bookmark(&prior.id).unwrap();
        let head = JjRepo::run(
            &h.repo.repo,
            &["log", "-r", &record.head(), "--no-graph", "-T", "commit_id"],
        );
        let op = JjRepo::run(&h.repo.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
        for description in ["left recovery", "right recovery"] {
            JjRepo::run(
                &h.repo.repo,
                &["--at-op", op.trim(), "describe", &head, "-m", description],
            );
        }
        let twins = JjRepo::run(
            &h.repo.repo,
            &[
                "log",
                "-r",
                &format!("bookmarks(exact:{})", record.head()),
                "--no-graph",
                "-T",
                "commit_id ++ \"\\n\"",
            ],
        );
        assert_eq!(twins.lines().count(), 2);
        let error = diff(&LocalJj, &record).await.unwrap_err();
        assert_eq!(pointer_integrity_kind(&error), Some(PointerIntegrityKind::Other));
        if kind == ExecutionKind::RevisionImplementation {
            h.dispatch().await.unwrap();
            assert_eq!(db.get_execution(&h.next.id).unwrap().status, ExecutionStatus::Running);
        } else {
            let lease = CubeWorkspaceLease {
                lease_id: "lease-conflicted".into(),
                workspace_id: "replacement".into(),
                workspace_path: h.repo.replacement.clone(),
                dirty_verified: Some(true),
            };
            assert!(
                h.coordinator
                    .recover_execution_bookmark_for_test(
                        &h.next,
                        &lease,
                        &h.coordinator.host_adapter(),
                        "mono",
                        Some(99),
                        Some(99),
                    )
                    .await
                    .unwrap()
                    .is_none(),
                "{kind:?}"
            );
        }
        assert_eq!(
            db.get_execution(&h.next.id).unwrap().pre_start_failure_count,
            0,
            "{kind:?}"
        );
        if kind.never_writes_pr_branch() {
            assert!(db.execution_bookmark_optional(&prior.id).unwrap().is_some());
            assert_eq!(
                pointer_integrity_kind(&diff(&LocalJj, &record).await.unwrap_err()),
                Some(PointerIntegrityKind::Other)
            );
            continue;
        }
        assert!(db.execution_bookmark_optional(&prior.id).unwrap().is_some());
        assert!(
            db.list_attention_items_for_work_item(&prior.work_item_id)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            JjRepo::run(
                &h.repo.repo,
                &[
                    "log",
                    "-r",
                    &format!("bookmarks(exact:{})", record.head()),
                    "--no-graph",
                    "-T",
                    "commit_id ++ \"\\n\""
                ]
            ),
            twins
        );
    }
}
