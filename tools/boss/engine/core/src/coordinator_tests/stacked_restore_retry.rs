use super::blocked_workspace::chain_harness;
use super::helpers::*;
use crate::coordinator::{CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus};
use crate::host_adapter::HostAdapter;
use boss_engine_recovery::execution_bookmark::{ExecutionBookmark, RestoreReport};

/// Delegates to a real adapter, but fails the first
/// `restore_rebased_execution_bookmark` with an ordinary (transient) error and
/// records every base it is asked to restage onto.
struct FlakyRestoreAdapter {
    inner: Arc<dyn HostAdapter>,
    bases: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl HostAdapter for FlakyRestoreAdapter {
    async fn recovery_pr_base(&self, origin: &str, pr: u64) -> Result<String> {
        self.inner.recovery_pr_base(origin, pr).await
    }

    async fn execution_bookmark_diff(&self, record: &ExecutionBookmark) -> Result<String> {
        self.inner.execution_bookmark_diff(record).await
    }

    async fn restore_rebased_execution_bookmark(
        &self,
        record: &ExecutionBookmark,
        workspace: &Path,
        pr_bookmark: Option<&str>,
        base_branch: &str,
    ) -> Result<RestoreReport> {
        let first = {
            let mut bases = self.bases.lock().unwrap();
            bases.push(base_branch.to_owned());
            bases.len() == 1
        };
        if first {
            return Err(anyhow!("injected transient fetch failure"));
        }
        self.inner
            .restore_rebased_execution_bookmark(record, workspace, pr_bookmark, base_branch)
            .await
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

    async fn goto_workspace(&self, workspace_path: &Path, pr: u64) -> Result<()> {
        self.inner.goto_workspace(workspace_path, pr).await
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
async fn transient_restore_failure_keeps_the_stacked_parent_base_on_retry() {
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
    let parent_sha = JjRepo::run(
        &h.repo.repo,
        &["log", "-r", "stack-parent", "--no-graph", "-T", "commit_id"],
    );
    *h.cube.pr_base.lock().await = Some("stack-parent".into());
    let lease = CubeWorkspaceLease {
        lease_id: "lease-transient".into(),
        workspace_id: "replacement".into(),
        workspace_path: h.repo.replacement.clone(),
        dirty_verified: Some(true),
    };
    let flaky = FlakyRestoreAdapter {
        inner: h.coordinator.host_adapter.clone(),
        bases: std::sync::Mutex::new(Vec::new()),
    };
    let adapter: Arc<dyn HostAdapter> = Arc::new(flaky);

    // The first restore fails with an ordinary error after the base lookup
    // succeeded: it must propagate, with only the parent base attempted and no
    // fallback restage onto main.
    let err = h
        .coordinator
        .recover_execution_bookmark(&h.next, &lease, &adapter, "mono", Some(99))
        .await
        .expect_err("a transient restore failure must propagate, not fall back");
    assert!(
        format!("{err:#}").contains("injected transient fetch failure"),
        "{err:#}"
    );
    assert!(!boss_engine_recovery::execution_bookmark::is_base_unresolvable_error(
        &err
    ));
    assert!(
        h.coordinator
            .work_db
            .execution_restore_report(&h.next.id)
            .unwrap()
            .is_none()
    );

    // The retry restages onto the same parent and keeps the preserved work.
    h.coordinator
        .recover_execution_bookmark(&h.next, &lease, &adapter, "mono", Some(99))
        .await
        .unwrap();
    let report = h
        .coordinator
        .work_db
        .execution_restore_report(&h.next.id)
        .unwrap()
        .expect("report");
    assert_eq!(report.base_fallback, None);
    assert_eq!(report.base_sha, parent_sha.trim());
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
