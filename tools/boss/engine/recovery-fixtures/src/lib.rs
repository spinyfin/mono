//! Test fixtures that record an execution the way dispatch does.
//!
//! Dispatch records only a baseline commit on the execution row and creates no
//! bookmark. These helpers pick the baseline from where a test positioned its
//! workspace, which production resolves from the PR head or trunk.

use std::path::Path;

use anyhow::{Result, ensure};
use boss_engine_recovery::execution_bookmark::{ExecutionBookmark, Jj};

/// Commit directly below `@` that the workspace was positioned on.
async fn positioned_base(jj: &dyn Jj, workspace: &Path) -> Result<String> {
    let output = jj
        .run(
            workspace,
            &[
                "log",
                "--no-graph",
                "-r",
                r#"heads(heads(::trunk() & ::@-) | ((trunk()..@-) ~ (empty() & description(exact:""))))"#,
                "-T",
                "commit_id ++ \"\\n\"",
            ],
        )
        .await?;
    let ids: Vec<_> = output.lines().filter(|s| !s.is_empty()).collect();
    ensure!(
        ids.len() == 1,
        "expected exactly one positioned baseline commit below @; found {}",
        ids.len()
    );
    Ok(ids[0].to_owned())
}

/// Record an execution whose baseline is the commit below `@`.
pub async fn create(jj: &dyn Jj, workspace: &Path, execution_id: &str, host_id: &str) -> Result<ExecutionBookmark> {
    let baseline = positioned_base(jj, workspace).await?;
    create_from(jj, workspace, execution_id, host_id, None, Some(&baseline)).await
}

/// Same as [`create`], with a predecessor or an explicit recorded floor for
/// tests that reconstruct a bound-PR dispatch.
pub async fn create_from(
    jj: &dyn Jj,
    workspace: &Path,
    execution_id: &str,
    host_id: &str,
    predecessor: Option<&ExecutionBookmark>,
    inherited_base: Option<&str>,
) -> Result<ExecutionBookmark> {
    boss_engine_recovery::execution_bookmark::create_from(
        jj,
        workspace,
        execution_id,
        host_id,
        predecessor,
        inherited_base,
    )
    .await
}
