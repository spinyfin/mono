//! Legacy three-bookmark layout for recovery compatibility fixtures.
//!
//! Production dispatch uses [`super::create_from`], which records the starting
//! commit without creating any bookmarks.

use super::*;

/// Rebuild the pre-migration baseline, recovery, and publication bookmarks.
/// Callers are tests that still exercise restore, archive, and teardown
/// against that layout.
pub async fn create_legacy_three_bookmark_fixture(
    jj: &dyn Jj,
    workspace: &Path,
    execution_id: &str,
    host_id: &str,
) -> Result<ExecutionBookmark> {
    create_legacy_three_bookmark_fixture_from(jj, workspace, execution_id, host_id, None, None).await
}

/// Same layout as [`create_legacy_three_bookmark_fixture`], with a predecessor
/// or recorded floor for tests that reconstruct a bound-PR dispatch.
pub async fn create_legacy_three_bookmark_fixture_from(
    jj: &dyn Jj,
    workspace: &Path,
    execution_id: &str,
    host_id: &str,
    predecessor: Option<&ExecutionBookmark>,
    inherited_base: Option<&str>,
) -> Result<ExecutionBookmark> {
    let mut record = create_from(jj, workspace, execution_id, host_id, predecessor, inherited_base).await?;
    let baseline = record.base_commit.as_deref().unwrap();
    jj.run(workspace, &["bookmark", "set", &record.base(), "-r", baseline])
        .await?;
    jj.run(
        workspace,
        &["bookmark", "set", &record.head(), &record.publication(), "-r", "@"],
    )
    .await?;
    record.has_recovery_head = Some(true);
    Ok(record)
}
