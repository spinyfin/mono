use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::bypass::parse_bypass_directives_from_descriptions;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChangeSet {
    pub changed_files: Vec<ChangedFile>,
    #[serde(default)]
    pub file_line_deltas: HashMap<PathBuf, FileLineDelta>,
    #[serde(default)]
    pub commit_description: Option<String>,
    #[serde(default)]
    pub pr_description: Option<String>,
    #[serde(default)]
    pub change_id: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
}

impl ChangeSet {
    pub fn new(changed_files: Vec<ChangedFile>) -> Self {
        Self {
            changed_files,
            file_line_deltas: HashMap::new(),
            commit_description: None,
            pr_description: None,
            change_id: None,
            repository: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.changed_files.is_empty()
    }

    pub fn with_commit_description(mut self, commit_description: Option<String>) -> Self {
        self.commit_description = commit_description;
        self
    }

    pub fn with_pr_description(mut self, pr_description: Option<String>) -> Self {
        self.pr_description = pr_description;
        self
    }

    pub fn with_change_id(mut self, change_id: Option<String>) -> Self {
        self.change_id = change_id;
        self
    }

    pub fn with_repository(mut self, repository: Option<String>) -> Self {
        self.repository = repository;
        self
    }

    pub fn with_file_line_delta(mut self, path: PathBuf, delta: FileLineDelta) -> Self {
        self.file_line_deltas.insert(path, delta);
        self
    }

    pub fn bypass_reason(&self, bypass_name: &str) -> Option<String> {
        parse_bypass_directives_from_descriptions(
            self.commit_description.as_deref(),
            self.pr_description.as_deref(),
        )
        .get(bypass_name)
        .cloned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FileLineDelta {
    pub added_lines: usize,
    pub removed_lines: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: PathBuf,
    pub kind: ChangeKind,
    pub old_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

pub trait SourceTree: Send + Sync {
    fn read_file(&self, path: &Path) -> Result<Vec<u8>>;

    fn exists(&self, path: &Path) -> bool;

    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>>;

    fn glob(&self, pattern: &str) -> Result<Vec<PathBuf>>;
}

#[cfg(test)]
mod tests {
    use super::{ChangeKind, ChangeSet, ChangedFile};

    #[test]
    fn bypass_reason_uses_commit_description() {
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: "backend/blob/src/v3/auth.rs".into(),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some(
            "BYPASS_API_BREAKING_SURFACE=Legitimate exception in commit.".to_owned(),
        ));

        assert_eq!(
            changeset.bypass_reason("BYPASS_API_BREAKING_SURFACE"),
            Some("Legitimate exception in commit.".to_owned())
        );
    }

    #[test]
    fn bypass_reason_prefers_pr_description_over_commit_description() {
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: "backend/blob/src/v3/auth.rs".into(),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some("BYPASS_API_BREAKING_SURFACE=From commit.".to_owned()))
        .with_pr_description(Some("BYPASS_API_BREAKING_SURFACE=From PR.".to_owned()));

        assert_eq!(
            changeset.bypass_reason("BYPASS_API_BREAKING_SURFACE"),
            Some("From PR.".to_owned())
        );
    }

    #[test]
    fn changeset_metadata_fields_round_trip_through_builders() {
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: "backend/blob/src/v3/auth.rs".into(),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_change_id(Some("235".to_owned()))
        .with_repository(Some("brianduff/flunge".to_owned()));

        assert_eq!(changeset.change_id.as_deref(), Some("235"));
        assert_eq!(changeset.repository.as_deref(), Some("brianduff/flunge"));
    }
}
