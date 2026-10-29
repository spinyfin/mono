use super::*;
use crate::coordinator::{CubeRepoHandle, CubeWorkspaceLease};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

struct TerminalCube {
    released: Arc<AtomicBool>,
    path: std::path::PathBuf,
}
crate::stub_cube_client! { TerminalCube {
    async fn ensure_repo(&self, _: &str) -> Result<CubeRepoHandle> {
        Ok(CubeRepoHandle { repo_id: "repo".into() })
    }
    async fn lease_workspace(&self, _: &str, _: &str, _: Option<&str>, _: bool, _: &[&str]) -> Result<CubeWorkspaceLease> {
        Ok(CubeWorkspaceLease { lease_id: "lease".into(), workspace_id: "workspace".into(), workspace_path: self.path.clone(), dirty_verified: None })
    }
    async fn release_workspace(&self, lease: &str) -> Result<()> {
        assert_eq!(lease, "lease");
        self.released.store(true, Ordering::SeqCst);
        Ok(())
    }
} }

struct Commands {
    calls: Mutex<Vec<Vec<String>>>,
    local: bool,
    fail_position: bool,
}
#[async_trait::async_trait]
impl CheckoutCommands for Commands {
    async fn run(&self, _: &std::path::Path, args: &[&str], _: &str) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|s| s.to_string()).collect());
        if (args[0] == "log" && !self.local) || (args[0] == "new" && self.fail_position) {
            anyhow::bail!("fixture checkout error");
        }
        Ok(())
    }
    async fn head_branch(&self, _: &str) -> Result<String> {
        self.calls.lock().unwrap().push(vec!["resolve-pr".into()]);
        Ok("review/topic".into())
    }
}

async fn prepare(commands: &Commands, head: Option<&str>) -> (Result<(String, String)>, bool) {
    let released = Arc::new(AtomicBool::new(false));
    let cube: Arc<dyn CubeClient> = Arc::new(TerminalCube {
        released: released.clone(),
        path: "/tmp/terminal-fixture".into(),
    });
    let result = open_with(&cube, "repo", "pr", "task", head, |_| {}, commands).await;
    (result, released.load(Ordering::SeqCst))
}

#[tokio::test]
async fn known_local_head_never_resolves_pr_or_fetches_and_positions_exact_sha() {
    let commands = Commands {
        calls: Mutex::new(vec![]),
        local: true,
        fail_position: false,
    };
    let head = "1234567890123456789012345678901234567890";
    let (result, released) = prepare(&commands, Some(head)).await;
    assert_eq!(result.unwrap(), ("/tmp/terminal-fixture".into(), "lease".into()));
    assert!(!released);
    assert_eq!(
        *commands.calls.lock().unwrap(),
        vec![vec!["log", "-r", head, "--no-graph"], vec!["new", "-r", head]]
    );
}

#[tokio::test]
async fn missing_local_branch_fetches_only_requested_branch_before_positioning() {
    let commands = Commands {
        calls: Mutex::new(vec![]),
        local: false,
        fail_position: false,
    };
    let (result, released) = prepare(&commands, None).await;
    assert!(result.is_ok());
    assert!(!released);
    let calls = commands.calls.lock().unwrap();
    assert_eq!(calls[2], ["git", "fetch", "--branch", "review/topic"]);
    assert_eq!(calls[3], ["new", "-r", "\"review/topic\"@origin"]);
}

#[tokio::test]
async fn checkout_failure_releases_lease_and_preserves_error() {
    let commands = Commands {
        calls: Mutex::new(vec![]),
        local: true,
        fail_position: true,
    };
    let (result, released) = prepare(&commands, Some("1234567890123456789012345678901234567890")).await;
    assert!(result.unwrap_err().to_string().contains("fixture checkout error"));
    assert!(released);
    assert_eq!(
        commands.calls.lock().unwrap().len(),
        2,
        "a checkout error must not trigger a fetch"
    );
}

#[test]
fn rejects_revset_expressions_as_cached_heads() {
    assert!(!valid_head("all()"));
    assert!(!valid_head("main@origin"));
}

#[tokio::test]
async fn real_local_jj_checkout_has_exact_parent_and_needs_no_remote() {
    use boss_engine_test_git::jj::JjRepo;
    let directory = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(directory.path());
    std::fs::write(repo.worker.join("expected.txt"), "PR content").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "PR head"]);
    let read = |rev: &str| JjRepo::run(&repo.worker, &["log", "-r", rev, "--no-graph", "-T", "commit_id"]);
    let head = read("@");
    let cube: Arc<dyn CubeClient> = Arc::new(TerminalCube {
        released: Arc::new(AtomicBool::new(false)),
        path: repo.worker.clone(),
    });
    let start = std::time::Instant::now();
    open_review_terminal_async(
        &cube,
        "unused",
        "invalid-pr-must-not-be-queried",
        "task",
        Some(&head),
        |_| {},
    )
    .await
    .unwrap();
    eprintln!("local terminal checkout elapsed_ms={}", start.elapsed().as_millis());
    assert_eq!(read("@-"), head);
    assert_eq!(
        std::fs::read_to_string(repo.worker.join("expected.txt")).unwrap(),
        "PR content"
    );
}
