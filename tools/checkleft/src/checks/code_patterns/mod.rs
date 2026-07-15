use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::check::{Check, ConfiguredCheck, count_applicable, run_per_text_file};
use crate::input::{ChangeSet, SourceTree};
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
    fn applicable_file_count(&self, changeset: &ChangeSet) -> usize {
        count_applicable(changeset, |path| matches_language_path(path, self.language))
    }

    async fn run_with_progress(
        &self,
        changeset: &ChangeSet,
        tree: &dyn SourceTree,
        on_file_processed: Arc<dyn Fn(usize) + Send + Sync>,
    ) -> Result<CheckResult> {
        let findings = run_per_text_file(
            changeset,
            tree,
            |path| matches_language_path(path, self.language),
            &*on_file_processed,
            |changed_file, contents, findings| {
                findings.extend(analyze_java_file(&changed_file.path, contents, &self.rules));
            },
        );

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
