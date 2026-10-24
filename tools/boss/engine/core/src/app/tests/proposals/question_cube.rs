//! Cube fixture with real recovery bookmarks for an asking run and its resume.
use crate::coordinator::{CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus};
use boss_engine_recovery::execution_bookmark::{ExecutionBookmark, LocalJj, create_from};
use boss_engine_test_git::jj::JjRepo;
use std::path::Path;

pub(super) struct QuestionCube {
    pub(super) dir: tempfile::TempDir,
    repo: JjRepo,
}

impl QuestionCube {
    pub(super) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = JjRepo::new(dir.path());
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
        Self { dir, repo }
    }
}

crate::stub_cube_client! { QuestionCube {
    async fn ensure_repo(&self, _origin: &str) -> anyhow::Result<CubeRepoHandle> {
        Ok(CubeRepoHandle { repo_id: "question-repo".into() })
    }
    async fn lease_workspace(&self, _repo_id: &str, _task: &str, prefer_workspace_id: Option<&str>, _allow_dirty: bool, _exclude_workspace_ids: &[&str]) -> anyhow::Result<CubeWorkspaceLease> {
        let (id, path) = if prefer_workspace_id.is_some() { ("replacement", &self.repo.replacement) } else { ("worker", &self.repo.worker) };
        Ok(CubeWorkspaceLease { lease_id: format!("lease-{id}"), workspace_id: id.into(), workspace_path: path.clone(), dirty_verified: None })
    }
    async fn create_change(&self, _workspace: &Path, _title: &str) -> anyhow::Result<CubeChangeHandle> {
        Ok(CubeChangeHandle { change_id: "question-change".into() })
    }
    async fn release_workspace(&self, _lease_id: &str) -> anyhow::Result<()> { Ok(()) }
    async fn workspace_status(&self, path: &Path) -> anyhow::Result<CubeWorkspaceStatus> {
        crate::test_support::AlwaysSucceedsCube.workspace_status(path).await
    }
    async fn heartbeat_lease(&self, _lease_id: &str, _ttl_seconds: Option<u64>) -> anyhow::Result<()> { Ok(()) }
    async fn force_release_lease(&self, _lease_id: &str, _reason: Option<&str>) -> anyhow::Result<()> { Ok(()) }
    async fn list_workspaces(&self) -> anyhow::Result<Vec<CubeWorkspaceStatus>> { Ok(vec![]) }
    async fn list_repos(&self) -> anyhow::Result<Vec<CubeRepoSummary>> {
        Ok(vec![CubeRepoSummary::builder().repo_id("question-repo").origin(self.repo.repo.to_string_lossy()).main_branch("main").workspace_root(self.dir.path().to_path_buf()).workspace_prefix("question").build()])
    }
    async fn create_execution_bookmark(&self, workspace: &Path, execution_id: &str, predecessor: Option<&ExecutionBookmark>, inherited_base: Option<&str>) -> anyhow::Result<ExecutionBookmark> {
        create_from(&LocalJj, workspace, execution_id, "local", predecessor, inherited_base).await
    }
} }
