use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::CheckResult;

mod config;
mod java;

use config::{PatternLanguage, parse_config};
use java::analyze_java_file;

#[derive(Debug, Default)]
pub struct CodePatternsCheck;

#[async_trait]
impl Check for CodePatternsCheck {
    fn id(&self) -> &str {
        "code-patterns"
    }

    fn description(&self) -> &str {
        "flags configured language-aware code patterns in changed files"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config)?))
    }
}

#[async_trait]
impl ConfiguredCheck for config::CompiledCodePatternsConfig {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        let mut findings = Vec::new();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }
            if !matches_language_path(&changed_file.path, self.language) {
                continue;
            }

            let Ok(contents) = tree.read_file(&changed_file.path) else {
                continue;
            };
            let Ok(contents) = std::str::from_utf8(&contents) else {
                continue;
            };

            findings.extend(analyze_java_file(&changed_file.path, contents, &self.rules));
        }

        Ok(CheckResult {
            check_id: "code-patterns".to_owned(),
            findings,
        })
    }
}

fn matches_language_path(path: &Path, language: PatternLanguage) -> bool {
    match language {
        PatternLanguage::Java => {
            matches!(path.extension().and_then(|ext| ext.to_str()), Some("java"))
        }
    }
}

#[cfg(test)]
mod tests;
