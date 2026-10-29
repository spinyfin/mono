//! Workspace preparation for an interactive terminal, independent of worker dispatch.
use super::handler_helpers::{get_pr_head_branch, run_jj};
use super::*;

pub(super) async fn open_review_terminal_async(
    cube_client: &Arc<dyn CubeClient>,
    repo_remote_url: &str,
    pr_url: &str,
    work_item_id: &str,
    known_head: Option<&str>,
    progress: impl Fn(&str),
) -> Result<(String, String)> {
    open_with(
        cube_client,
        repo_remote_url,
        pr_url,
        work_item_id,
        known_head,
        progress,
        &LocalCheckout,
    )
    .await
}

#[async_trait::async_trait]
trait CheckoutCommands: Send + Sync {
    async fn run(&self, workspace: &std::path::Path, args: &[&str], display: &str) -> Result<()>;
    async fn head_branch(&self, pr_url: &str) -> Result<String>;
}

struct LocalCheckout;

#[async_trait::async_trait]
impl CheckoutCommands for LocalCheckout {
    async fn run(&self, workspace: &std::path::Path, args: &[&str], display: &str) -> Result<()> {
        run_jj(workspace, args, display).await
    }
    async fn head_branch(&self, pr_url: &str) -> Result<String> {
        get_pr_head_branch(pr_url).await
    }
}

async fn open_with(
    cube_client: &Arc<dyn CubeClient>,
    repo_remote_url: &str,
    pr_url: &str,
    work_item_id: &str,
    known_head: Option<&str>,
    progress: impl Fn(&str),
    commands: &dyn CheckoutCommands,
) -> Result<(String, String)> {
    progress("Registering repository");
    let repo = cube_client.ensure_repo(repo_remote_url).await?;
    progress("Leasing workspace");
    let lease = cube_client
        .lease_terminal_workspace(&repo.repo_id, &format!("review terminal for {work_item_id}"))
        .await?;
    let result = async {
        progress("Checking local PR revision");
        let known_head = known_head.filter(|sha| valid_head(sha));
        // The poller's SHA pins the last observed PR head, even if its branch
        // was deleted after merge. Never interpret arbitrary text as a revset.
        if let Some(sha) = known_head
            && commands
                .run(
                    &lease.workspace_path,
                    &["log", "-r", sha, "--no-graph"],
                    "resolve local PR head",
                )
                .await
                .is_ok()
        {
            progress("Positioning workspace");
            commands
                .run(&lease.workspace_path, &["new", "-r", sha], "position local PR head")
                .await?;
            return Ok(());
        }
        progress("Resolving PR branch");
        let branch = commands.head_branch(pr_url).await?;
        // Quote the bookmark as a revset string, including names containing
        // punctuation. Fetch only this branch when it is absent locally.
        let rev = match known_head {
            Some(sha) => sha.to_owned(),
            None => format!("{}@origin", serde_json::to_string(&branch)?),
        };
        if commands
            .run(
                &lease.workspace_path,
                &["log", "-r", &rev, "--no-graph"],
                "resolve local PR branch",
            )
            .await
            .is_err()
        {
            progress("Fetching PR branch");
            commands
                .run(
                    &lease.workspace_path,
                    &["git", "fetch", "--branch", &branch],
                    "fetch PR branch",
                )
                .await?;
        }
        progress("Positioning workspace");
        commands
            .run(&lease.workspace_path, &["new", "-r", &rev], "position PR branch")
            .await
    }
    .await;
    if let Err(error) = result {
        progress(&format!("Workspace preparation failed: {error:#}; releasing lease"));
        if let Err(release_error) = cube_client.release_workspace(&lease.lease_id).await {
            tracing::error!(%work_item_id, %release_error, lease_id = %lease.lease_id, "terminal lease cleanup failed");
        }
        return Err(error);
    }
    progress("Workspace ready");
    Ok((lease.workspace_path.display().to_string(), lease.lease_id))
}

fn valid_head(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests;
