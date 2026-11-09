//! Record fixtures for this crate's own tests.
//!
//! Production dispatch uses [`super::create_from`], which records the starting
//! commit without creating any bookmarks.

use super::*;

/// Record an execution's starting commit, as dispatch does.
pub async fn create(jj: &dyn Jj, workspace: &Path, execution_id: &str, host_id: &str) -> Result<ExecutionBookmark> {
    create_from(jj, workspace, execution_id, host_id, None, None).await
}
