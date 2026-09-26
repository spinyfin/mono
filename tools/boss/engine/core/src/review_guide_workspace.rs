//! Pinned cube workspace verification and engine-owned git-object link validation.

use anyhow::{Context, Result, ensure};
use boss_pr_review_sources::SourcePacket;

#[cfg(test)]
#[path = "review_guide_workspace_tests.rs"]
mod tests;
use std::path::{Path, PathBuf};
use std::process::Command;

fn require_sha(sha: &str) -> Result<()> {
    ensure!(
        sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
        "review comparison must contain full immutable SHAs"
    );
    Ok(())
}

fn git(workspace: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .env("GIT_DIR", crate::conflict_diagnosis::resolve_git_dir(workspace)?)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .context("reading pinned git objects")?;
    ensure!(
        output.status.success(),
        "pinned git read failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

/// Cube represents a detached checkout as a fresh empty jj change on the
/// requested commit. Verify that exact parent and empty tree, not a moving ref.
pub(crate) fn verify(workspace: &Path, packet: &SourcePacket) -> Result<PathBuf> {
    require_sha(&packet.head_sha)?;
    require_sha(&packet.merge_base_sha)?;
    let output = Command::new(std::env::var_os("BOSS_JJ_BIN").unwrap_or_else(|| "jj".into()))
        .args([
            "--repository",
            &workspace.display().to_string(),
            "log",
            "--no-graph",
            "-r",
            "@",
            "-T",
            "if(empty, parents.map(|p| p.commit_id()).join(\",\"), \"dirty\")",
        ])
        .output()
        .context("verifying review-guide workspace SHA")?;
    ensure!(
        output.status.success(),
        "could not verify review-guide checkout: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        String::from_utf8_lossy(&output.stdout).trim() == packet.head_sha,
        "review-guide checkout does not match pinned head {} (got {})",
        packet.head_sha,
        String::from_utf8_lossy(&output.stdout).trim()
    );
    for sha in [&packet.merge_base_sha, &packet.head_sha] {
        ensure!(
            git(workspace, &["cat-file", "-t", sha])? == b"commit\n",
            "comparison revision is not a local commit"
        );
    }
    Ok(crate::conflict_diagnosis::resolve_git_dir(workspace)?)
}

/// Read the object, not the working file. cat-file does not invoke diff drivers,
/// filters, hooks, a pager, credentials, or network fetching.
fn range_exists(workspace: &Path, sha: &str, path: &str, start: u32, end: u32) -> Result<()> {
    require_sha(sha)?;
    let whole_file = start == 0 && end == 0;
    ensure!(whole_file || (start > 0 && end >= start), "invalid line range");
    ensure!(
        !path.is_empty()
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
            && !path.contains('\0'),
        "invalid repository path"
    );
    let object = format!("{sha}:{path}");
    let size = String::from_utf8(git(workspace, &["cat-file", "-s", &object])?)?
        .trim()
        .parse::<u64>()?;
    ensure!(
        size <= 16 * 1024 * 1024,
        "linked source exceeds 16 MiB validation limit"
    );
    let text = String::from_utf8(git(workspace, &["cat-file", "blob", &object])?)?;
    ensure!(
        !text.contains('\0') && (whole_file || end as usize <= text.lines().count()),
        "linked range does not exist in textual source"
    );
    Ok(())
}

pub(crate) fn validate(
    workspace: &Path,
    packet: &SourcePacket,
    raw: &str,
) -> Result<boss_review_guide::ValidatedGuide> {
    let failures = std::cell::RefCell::new(Vec::new());
    boss_review_guide::validate_guide_output_with_resolver(raw, packet, |sha, path, start, end| {
        match range_exists(workspace, sha, path, start, end) {
            Ok(()) => true,
            Err(error) => {
                failures.borrow_mut().push(format!("{sha}:{path}: {error:#}"));
                false
            }
        }
    })
    .map_err(|issues| {
        let mut reasons = issues.iter().map(ToString::to_string).collect::<Vec<_>>();
        reasons.extend(failures.into_inner());
        anyhow::anyhow!(reasons.join("; "))
    })
}
