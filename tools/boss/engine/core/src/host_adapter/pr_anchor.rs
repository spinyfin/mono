//! A PR alias is disposable Cube state, not a durable recovery pointer.
use super::*;

// Recovery needs branch and SHA from one fresh response; BranchVerifier's
// separate ref/OID methods can observe different heads during a force push.
#[async_trait]
trait PrHeadLookup: Send + Sync {
    async fn fetch(&self, slug: &str, pr: u64) -> Result<(String, String)>;
}

/// A successful GitHub response that cannot supply a usable PR head.
#[derive(Debug, thiserror::Error)]
#[error("GitHub returned no usable PR head")]
struct PrHeadUnavailable;

/// Parse the `.head | .ref, .sha` jq output into branch and SHA.
fn parse_pr_head(stdout: &str) -> Result<(String, String)> {
    let mut lines = stdout.lines();
    let head_ref = lines.next().unwrap_or_default().trim();
    let sha = lines.next().unwrap_or_default().trim();
    if head_ref.is_empty() || head_ref == "null" || sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(PrHeadUnavailable.into());
    }
    Ok((head_ref.to_owned(), sha.to_owned()))
}

/// Production lookup: an uncached REST read through the shared instrumented
/// `gh` spawn, which records one telemetry sample from the real arguments.
struct GhApiHead;

#[async_trait]
impl PrHeadLookup for GhApiHead {
    async fn fetch(&self, slug: &str, pr: u64) -> Result<(String, String)> {
        boss_gh_telemetry::scope(boss_gh_telemetry::callers::WORKER_SPAWN, async {
            let endpoint = format!("repos/{slug}/pulls/{pr}");
            let output = boss_github::gh_runner::gh_output(&[
                "api",
                "-H",
                "Cache-Control: no-cache",
                &endpoint,
                "--jq",
                ".head | .ref, .sha",
            ])
            .await
            .context("failed to spawn `gh api PR head`")?;
            if !output.status.success() {
                anyhow::bail!(
                    "`gh api {endpoint}` failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            parse_pr_head(&String::from_utf8_lossy(&output.stdout))
        })
        .await
    }
}

pub(super) async fn reconcile(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    pr_bookmark: Option<&str>,
    trunk: &str,
) -> Result<Option<execution_bookmark::Reconciliation>> {
    reconcile_with_gh(jj, record, pr_bookmark, trunk, &GhApiHead).await
}

async fn reconcile_with_gh(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    pr_bookmark: Option<&str>,
    trunk: &str,
    gh: &dyn PrHeadLookup,
) -> Result<Option<execution_bookmark::Reconciliation>> {
    use execution_bookmark::{PointerIntegrityKind, pointer_integrity_kind};
    let initial = execution_bookmark::reconcile(jj, record, pr_bookmark, trunk).await;
    if !initial
        .as_ref()
        .is_err_and(|err| pointer_integrity_kind(err) == Some(PointerIntegrityKind::PrHeadUnavailable))
    {
        return initial;
    }
    let bookmark = pr_bookmark.ok_or_else(|| anyhow::anyhow!("missing bound PR"))?;
    let pr: u64 = bookmark
        .strip_prefix("pr/")
        .ok_or_else(|| anyhow::anyhow!("invalid PR anchor {bookmark}"))?
        .parse()?;
    let head = repair_with_gh(jj, record, pr, gh).await?;
    after_repair(
        execution_bookmark::reconcile(jj, record, pr_bookmark, trunk).await,
        pr,
        &head,
    )
}

pub(super) async fn ensure(jj: &dyn Jj, record: &ExecutionBookmark, pr: u64) -> Result<()> {
    ensure_with_gh(jj, record, pr, &GhApiHead).await
}

async fn ensure_with_gh(jj: &dyn Jj, record: &ExecutionBookmark, pr: u64, gh: &dyn PrHeadLookup) -> Result<()> {
    match execution_bookmark::resolve_bookmark_commit(jj, record, &format!("pr/{pr}")).await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => repair_with_gh(jj, record, pr, gh).await.map(|_| ()),
        Err(err) if execution_bookmark::is_pointer_integrity_error(&err) => {
            repair_with_gh(jj, record, pr, gh).await.map(|_| ())
        }
        Err(err) => Err(err),
    }
}

pub(super) async fn restore_rebased(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    pr_bookmark: Option<&str>,
    base_branch: &str,
    upstream: &str,
) -> Result<execution_bookmark::RestoreReport> {
    restore_with_gh(jj, record, workspace, pr_bookmark, base_branch, upstream, &GhApiHead).await
}

async fn restore_with_gh(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    pr_bookmark: Option<&str>,
    base_branch: &str,
    upstream: &str,
    gh: &dyn PrHeadLookup,
) -> Result<execution_bookmark::RestoreReport> {
    let initial = execution_bookmark::restore_rebased(jj, record, workspace, pr_bookmark, base_branch, upstream).await;
    if !initial.as_ref().is_err_and(|err| {
        execution_bookmark::pointer_integrity_kind(err)
            == Some(execution_bookmark::PointerIntegrityKind::PrHeadUnavailable)
    }) {
        return initial;
    }
    let bookmark = pr_bookmark.ok_or_else(|| anyhow::anyhow!("missing bound PR"))?;
    let pr = bookmark
        .strip_prefix("pr/")
        .ok_or_else(|| anyhow::anyhow!("invalid PR anchor {bookmark}"))?
        .parse()?;
    let head = repair_with_gh(jj, record, pr, gh).await?;
    after_repair(
        execution_bookmark::restore_rebased(jj, record, workspace, pr_bookmark, base_branch, upstream).await,
        pr,
        &head,
    )
}

fn after_repair<T>(result: Result<T>, pr: u64, head: &str) -> Result<T> {
    result.map_err(|err| {
        if execution_bookmark::pointer_integrity_kind(&err)
            == Some(execution_bookmark::PointerIntegrityKind::PrHeadUnavailable)
        {
            execution_bookmark::pointer_integrity_error(format!(
                "pr/{pr} was repaired from GitHub head {head} but was removed or conflicted again"
            ))
        } else {
            err
        }
    })
}

async fn repair_with_gh(jj: &dyn Jj, record: &ExecutionBookmark, pr: u64, gh: &dyn PrHeadLookup) -> Result<String> {
    let bookmark = format!("pr/{pr}");
    // One repair, never a retry loop. The host's jj runner imports the head;
    // existing gh authentication supplies the authoritative GitHub identity.
    let metadata = async {
        let remotes = jj.run(&record.repo_path, &["git", "remote", "list"]).await?;
        let (remote, slug) = git_utils::repo_slug::parse_github_remote(&remotes)
            .ok_or_else(|| execution_bookmark::pointer_integrity_error("recovery store has no GitHub remote"))?;
        let (head_ref, head_sha) = gh.fetch(&slug, pr).await.map_err(|err| {
            if err.is::<PrHeadUnavailable>() {
                execution_bookmark::pointer_integrity_error(format!("GitHub head for {bookmark} is unavailable"))
            } else {
                err
            }
        })?;
        if head_ref.is_empty() || head_sha.len() != 40 || !head_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(execution_bookmark::pointer_integrity_error(format!(
                "GitHub head for {bookmark} is invalid or absent"
            )));
        }
        Ok::<_, anyhow::Error>((remote, head_ref, head_sha))
    };
    let (remote, head_ref, head_sha) = tokio::time::timeout(std::time::Duration::from_secs(120), metadata)
        .await
        .map_err(|_| anyhow::anyhow!("repair of {bookmark} from GitHub timed out"))?
        .with_context(|| format!("cannot repair {bookmark} from GitHub"))?;
    // The jj runner bounds each command. Do not cancel the fetch/restore
    // sequence as a whole: even a failed fetch must restore saved pointers.
    execution_bookmark::repair_pr_anchor(jj, record, pr, &remote, &head_ref, &head_sha)
        .await
        .with_context(|| format!("cannot import GitHub head for {bookmark}"))?;
    Ok(head_sha)
}

#[cfg(test)]
#[path = "pr_anchor_tests.rs"]
mod tests;
