use std::fs;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::app::CubeError;
use crate::metadata::{RepoRecord, WorkspaceRecord, WorkspaceState};
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
                |row| {
                    Ok(RepoRecord {
                        repo: row.get(0)?,
                        origin: row.get(1)?,
                        main_branch: row.get(2)?,
                        workspace_root: row.get::<_, String>(3)?.into(),
                        workspace_prefix: row.get(4)?,
                        source: row.get::<_, Option<String>>(5)?.map(Into::into),
                    })
                },
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
            .query_map([], |row| {
                Ok(RepoRecord {
                    repo: row.get(0)?,
                    origin: row.get(1)?,
                    main_branch: row.get(2)?,
                    workspace_root: row.get::<_, String>(3)?.into(),
                    workspace_prefix: row.get(4)?,
                    source: row.get::<_, Option<String>>(5)?.map(Into::into),
                })
            })
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
