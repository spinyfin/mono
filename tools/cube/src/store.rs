use std::fs;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::app::CubeError;
use crate::metadata::{RepoRecord, WorkspaceCandidate, WorkspaceRecord, WorkspaceState};
use crate::paths::database_path;

pub struct Store {
    connection: Connection,
}

impl Store {
    pub fn open_default() -> Result<Self, CubeError> {
        let path = database_path()?;
        Self::open_at(path)
    }

    pub fn open_at(path: impl AsRef<Path>) -> Result<Self, CubeError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let connection = Connection::open(path).map_err(CubeError::Storage)?;
        let store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    pub fn upsert_repo(&self, config: &RepoRecord) -> Result<RepoRecord, CubeError> {
        self.connection
            .execute(
                r#"
                INSERT INTO repos (
                    repo,
                    origin,
                    main_branch,
                    workspace_root,
                    workspace_prefix,
                    source_path
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(repo) DO UPDATE SET
                    origin = excluded.origin,
                    main_branch = excluded.main_branch,
                    workspace_root = excluded.workspace_root,
                    workspace_prefix = excluded.workspace_prefix,
                    source_path = excluded.source_path
                "#,
                params![
                    config.repo,
                    config.origin,
                    config.main_branch,
                    config.workspace_root.display().to_string(),
                    config.workspace_prefix,
                    config.source.as_ref().map(|path| path_to_string(path)),
                ],
            )
            .map_err(CubeError::Storage)?;

        self.get_repo(&config.repo)?
            .ok_or_else(|| CubeError::RepoNotFound(config.repo.clone()))
    }

    pub fn get_repo(&self, repo: &str) -> Result<Option<RepoRecord>, CubeError> {
        self.connection
            .query_row(
                r#"
                SELECT repo, origin, main_branch, workspace_root, workspace_prefix, source_path
                FROM repos
                WHERE repo = ?1
                "#,
                params![repo],
                row_to_repo_record,
            )
            .optional()
            .map_err(CubeError::Storage)
    }

    pub fn get_repo_by_origin(&self, origin: &str) -> Result<Option<RepoRecord>, CubeError> {
        self.connection
            .query_row(
                r#"
                SELECT repo, origin, main_branch, workspace_root, workspace_prefix, source_path
                FROM repos
                WHERE origin = ?1
                ORDER BY repo
                LIMIT 1
                "#,
                params![origin],
                row_to_repo_record,
            )
            .optional()
            .map_err(CubeError::Storage)
    }

    pub fn list_repos(&self) -> Result<Vec<RepoRecord>, CubeError> {
        let mut statement = self
            .connection
            .prepare(
                r#"
                SELECT repo, origin, main_branch, workspace_root, workspace_prefix, source_path
                FROM repos
                ORDER BY repo
                "#,
            )
            .map_err(CubeError::Storage)?;
        let rows = statement
            .query_map([], row_to_repo_record)
            .map_err(CubeError::Storage)?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(CubeError::Storage)
    }

    pub fn list_workspaces(&self, repo: &str) -> Result<Vec<WorkspaceRecord>, CubeError> {
        let mut statement = self
            .connection
            .prepare(
                r#"
                SELECT
                    repo,
                    workspace_id,
                    workspace_path,
                    state,
                    lease_id,
                    holder,
                    task,
                    leased_at_epoch_s,
                    head_commit
                FROM workspaces
                WHERE repo = ?1
                ORDER BY workspace_id
                "#,
            )
            .map_err(CubeError::Storage)?;
        let rows = statement
            .query_map(params![repo], |row| {
                let state_raw: String = row.get(3)?;
                Ok(WorkspaceRecord {
                    repo: row.get(0)?,
                    workspace_id: row.get(1)?,
                    workspace_path: row.get::<_, String>(2)?.into(),
                    state: WorkspaceState::from_str(&state_raw).ok_or_else(|| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::<dyn std::error::Error + Send + Sync>::from(format!(
                                "invalid workspace state `{state_raw}`"
                            )),
                        )
                    })?,
                    lease_id: row.get(4)?,
                    holder: row.get(5)?,
                    task: row.get(6)?,
                    leased_at_epoch_s: row.get(7)?,
                    head_commit: row.get(8)?,
                })
            })
            .map_err(CubeError::Storage)?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(CubeError::Storage)
    }

    pub fn sync_workspaces(
        &mut self,
        repo: &str,
        candidates: &[WorkspaceCandidate],
    ) -> Result<(), CubeError> {
        let transaction = self.connection.transaction().map_err(CubeError::Storage)?;
        for candidate in candidates {
            transaction
                .execute(
                    r#"
                    INSERT INTO workspaces (
                        repo,
                        workspace_id,
                        workspace_path,
                        state
                    ) VALUES (?1, ?2, ?3, ?4)
                    ON CONFLICT(repo, workspace_id) DO UPDATE SET
                        workspace_path = excluded.workspace_path
                    "#,
                    params![
                        repo,
                        candidate.workspace_id,
                        candidate.workspace_path.display().to_string(),
                        WorkspaceState::Free.as_str(),
                    ],
                )
                .map_err(CubeError::Storage)?;
        }
        transaction.commit().map_err(CubeError::Storage)
    }

    pub fn claim_workspace(
        &mut self,
        repo: &str,
        holder: &str,
        task: &str,
        lease_id: &str,
        leased_at_epoch_s: i64,
    ) -> Result<Option<WorkspaceRecord>, CubeError> {
        let transaction = self.connection.transaction().map_err(CubeError::Storage)?;
        let candidate = transaction
            .query_row(
                r#"
                SELECT workspace_id, workspace_path
                FROM workspaces
                WHERE repo = ?1 AND state = ?2
                ORDER BY workspace_id
                LIMIT 1
                "#,
                params![repo, WorkspaceState::Free.as_str()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(CubeError::Storage)?;

        let Some((workspace_id, workspace_path)) = candidate else {
            transaction.rollback().map_err(CubeError::Storage)?;
            return Ok(None);
        };

        transaction
            .execute(
                r#"
                UPDATE workspaces
                SET
                    state = ?1,
                    lease_id = ?2,
                    holder = ?3,
                    task = ?4,
                    leased_at_epoch_s = ?5,
                    head_commit = NULL
                WHERE repo = ?6 AND workspace_id = ?7 AND state = ?8
                "#,
                params![
                    WorkspaceState::Leased.as_str(),
                    lease_id,
                    holder,
                    task,
                    leased_at_epoch_s,
                    repo,
                    workspace_id,
                    WorkspaceState::Free.as_str(),
                ],
            )
            .map_err(CubeError::Storage)?;

        let claimed = transaction
            .query_row(
                r#"
                SELECT
                    repo,
                    workspace_id,
                    workspace_path,
                    state,
                    lease_id,
                    holder,
                    task,
                    leased_at_epoch_s,
                    head_commit
                FROM workspaces
                WHERE repo = ?1 AND workspace_id = ?2
                "#,
                params![repo, workspace_id],
                |row| row_to_workspace_record(row),
            )
            .map_err(CubeError::Storage)?;
        transaction.commit().map_err(CubeError::Storage)?;

        debug_assert_eq!(claimed.workspace_path, Path::new(&workspace_path));
        Ok(Some(claimed))
    }

    pub fn update_workspace_head_commit(
        &self,
        lease_id: &str,
        head_commit: Option<&str>,
    ) -> Result<(), CubeError> {
        self.connection
            .execute(
                r#"
                UPDATE workspaces
                SET head_commit = ?2
                WHERE lease_id = ?1
                "#,
                params![lease_id, head_commit],
            )
            .map_err(CubeError::Storage)?;
        Ok(())
    }

    pub fn get_workspace_by_path(
        &self,
        workspace_path: &Path,
    ) -> Result<Option<WorkspaceRecord>, CubeError> {
        self.connection
            .query_row(
                r#"
                SELECT
                    repo,
                    workspace_id,
                    workspace_path,
                    state,
                    lease_id,
                    holder,
                    task,
                    leased_at_epoch_s,
                    head_commit
                FROM workspaces
                WHERE workspace_path = ?1
                "#,
                params![workspace_path.display().to_string()],
                row_to_workspace_record,
            )
            .optional()
            .map_err(CubeError::Storage)
    }

    pub fn get_workspace_by_lease(
        &self,
        lease_id: &str,
    ) -> Result<Option<WorkspaceRecord>, CubeError> {
        self.connection
            .query_row(
                r#"
                SELECT
                    repo,
                    workspace_id,
                    workspace_path,
                    state,
                    lease_id,
                    holder,
                    task,
                    leased_at_epoch_s,
                    head_commit
                FROM workspaces
                WHERE lease_id = ?1
                "#,
                params![lease_id],
                row_to_workspace_record,
            )
            .optional()
            .map_err(CubeError::Storage)
    }

    pub fn release_workspace(&self, lease_id: &str) -> Result<Option<WorkspaceRecord>, CubeError> {
        let before = self.get_workspace_by_lease(lease_id)?;
        let Some(record) = before else {
            return Ok(None);
        };

        self.connection
            .execute(
                r#"
                UPDATE workspaces
                SET
                    state = ?2,
                    lease_id = NULL,
                    holder = NULL,
                    task = NULL,
                    leased_at_epoch_s = NULL,
                    head_commit = NULL
                WHERE lease_id = ?1
                "#,
                params![lease_id, WorkspaceState::Free.as_str()],
            )
            .map_err(CubeError::Storage)?;

        Ok(Some(WorkspaceRecord {
            state: WorkspaceState::Free,
            lease_id: None,
            holder: None,
            task: None,
            leased_at_epoch_s: None,
            head_commit: None,
            ..record
        }))
    }

    fn migrate(&self) -> Result<(), CubeError> {
        self.connection
            .execute_batch(
                r#"
                PRAGMA foreign_keys = ON;

                CREATE TABLE IF NOT EXISTS repos (
                    repo TEXT PRIMARY KEY,
                    origin TEXT NOT NULL,
                    main_branch TEXT NOT NULL,
                    workspace_root TEXT NOT NULL,
                    workspace_prefix TEXT NOT NULL,
                    source_path TEXT
                );

                CREATE INDEX IF NOT EXISTS repos_origin_idx
                    ON repos(origin);

                CREATE TABLE IF NOT EXISTS workspaces (
                    repo TEXT NOT NULL,
                    workspace_id TEXT NOT NULL,
                    workspace_path TEXT NOT NULL UNIQUE,
                    state TEXT NOT NULL,
                    lease_id TEXT,
                    holder TEXT,
                    task TEXT,
                    leased_at_epoch_s INTEGER,
                    head_commit TEXT,
                    PRIMARY KEY(repo, workspace_id),
                    FOREIGN KEY(repo) REFERENCES repos(repo) ON DELETE CASCADE
                );

                CREATE INDEX IF NOT EXISTS workspaces_repo_state_idx
                    ON workspaces(repo, state);
                "#,
            )
            .map_err(CubeError::Storage)
    }
}

fn path_to_string(path: &Path) -> String {
    path.display().to_string()
}

fn row_to_repo_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<RepoRecord> {
    Ok(RepoRecord {
        repo: row.get(0)?,
        origin: row.get(1)?,
        main_branch: row.get(2)?,
        workspace_root: row.get::<_, String>(3)?.into(),
        workspace_prefix: row.get(4)?,
        source: row.get::<_, Option<String>>(5)?.map(Into::into),
    })
}

fn row_to_workspace_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceRecord> {
    let state_raw: String = row.get(3)?;
    Ok(WorkspaceRecord {
        repo: row.get(0)?,
        workspace_id: row.get(1)?,
        workspace_path: row.get::<_, String>(2)?.into(),
        state: WorkspaceState::from_str(&state_raw).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::<dyn std::error::Error + Send + Sync>::from(format!(
                    "invalid workspace state `{state_raw}`"
                )),
            )
        })?,
        lease_id: row.get(4)?,
        holder: row.get(5)?,
        task: row.get(6)?,
        leased_at_epoch_s: row.get(7)?,
        head_commit: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use crate::metadata::RepoRecord;

    use super::Store;

    fn open_store() -> (TempDir, Store) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let store = Store::open_at(tempdir.path().join("state.db")).expect("store");
        (tempdir, store)
    }

    #[test]
    fn list_workspaces_defaults_to_empty() {
        let (_tempdir, store) = open_store();
        let config = RepoRecord {
            repo: "mono".to_string(),
            origin: "git@github.com:spinyfin/mono.git".to_string(),
            main_branch: "main".to_string(),
            workspace_root: "/tmp/workspaces".into(),
            workspace_prefix: "mono-agent-".to_string(),
            source: None,
        };
        store.upsert_repo(&config).expect("repo");

        let workspaces = store.list_workspaces("mono").expect("workspaces");
        assert!(workspaces.is_empty());
    }
}
