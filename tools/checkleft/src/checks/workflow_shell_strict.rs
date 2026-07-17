use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_yaml::Value;

use crate::check::{Check, ConfiguredCheck, count_applicable, run_per_text_file};
use crate::checks::workflow_yaml::{
    is_github_workflow_file, mapping_get, parse_workflow, workflow_steps, yaml_parse_finding,
};
use crate::input::{ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

const STRICT_MODE_PREFIX: &str = "set -euo pipefail";

#[derive(Debug, Default)]
pub struct WorkflowShellStrictCheck;

#[async_trait]
impl Check for WorkflowShellStrictCheck {
    fn id(&self) -> &str {
        "workflow-shell-strict"
    }

    fn description(&self) -> &str {
        "requires GitHub Actions run scripts to start with strict shell mode"
    }

    fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(Self))
    }
}

#[async_trait]
impl ConfiguredCheck for WorkflowShellStrictCheck {
    fn applicable_file_count(&self, changeset: &ChangeSet) -> usize {
        count_applicable(changeset, is_github_workflow_file)
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
            is_github_workflow_file,
            &*on_file_processed,
            |changed_file, contents, findings| {
                let workflow = match parse_workflow(contents) {
                    Ok(workflow) => workflow,
                    Err(error) => {
                        findings.push(yaml_parse_finding(
                            changed_file,
                            format!("failed to parse workflow YAML while enforcing strict shell mode: {error}"),
                            "Fix YAML syntax so checks can validate `run:` script blocks.",
                        ));
                        return;
                    }
                };

                for violation in find_non_strict_run_scripts(&workflow) {
                    findings.push(Finding {
                        fixable: false,
                        severity: Severity::Error,
                        message: format!(
                            "GitHub Actions run script in job `{}` step {} must start with `set -euo pipefail`.",
                            violation.job_name, violation.step_index
                        ),
                        location: Some(Location {
                            path: changed_file.path.clone(),
                            line: None,
                            column: None,
                        }),
                        remediations: vec![
                            "Add `set -euo pipefail` as the first non-comment line in each `run:` script block."
                                .to_owned(),
                        ],
                        suggested_fix: None,
                    });
                }
            },
        );

        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings,
        })
    }
}

#[derive(Debug)]
struct RunScriptViolation {
    job_name: String,
    step_index: usize,
}

fn find_non_strict_run_scripts(workflow: &Value) -> Vec<RunScriptViolation> {
    let mut violations = Vec::new();

    for step in workflow_steps(workflow) {
        let Some(run_script) = mapping_get(step.step, "run").and_then(Value::as_str) else {
            continue;
        };
        if !is_multiline_script(run_script) {
            continue;
        }
        if !starts_with_strict_mode(run_script) {
            violations.push(RunScriptViolation {
                job_name: step.job_name,
                step_index: step.step_index,
            });
        }
    }

    violations
}

fn is_multiline_script(script: &str) -> bool {
    script.contains('\n')
}

fn starts_with_strict_mode(script: &str) -> bool {
    let first_command = script
        .lines()
        .map(str::trim_start)
        .find(|line| !line.is_empty() && !line.starts_with('#'));

    first_command
        .map(|line| line.starts_with(STRICT_MODE_PREFIX))
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::source_tree::LocalSourceTree;

    use super::WorkflowShellStrictCheck;

    #[tokio::test]
    async fn flags_missing_strict_mode_in_workflow_run_block() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join(".github/workflows")).expect("create workflows dir");
        fs::write(
            temp.path().join(".github/workflows/ci.yaml"),
            r#"jobs:
  test:
    steps:
      - run: |
          echo "hello"
"#,
        )
        .expect("write workflow");

        let check = WorkflowShellStrictCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(".github/workflows/ci.yaml").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert!(
            result.findings[0]
                .message
                .contains("job `test` step 1 must start with `set -euo pipefail`")
        );
    }

    #[tokio::test]
    async fn accepts_strict_mode_after_comments_and_blank_lines() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join(".github/workflows")).expect("create workflows dir");
        fs::write(
            temp.path().join(".github/workflows/ci.yml"),
            r#"jobs:
  test:
    steps:
      - run: |

          # strict shell mode
          set -euo pipefail
          echo "hello"
"#,
        )
        .expect("write workflow");

        let check = WorkflowShellStrictCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(".github/workflows/ci.yml").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn ignores_non_workflow_files() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("docs")).expect("create docs dir");
        fs::write(
            temp.path().join("docs/example.yaml"),
            r#"run: |
  echo "hello"
"#,
        )
        .expect("write yaml");

        let check = WorkflowShellStrictCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("docs/example.yaml").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn ignores_single_line_run_entries() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join(".github/workflows")).expect("create workflows dir");
        fs::write(
            temp.path().join(".github/workflows/ci.yaml"),
            r#"jobs:
  test:
    steps:
      - run: echo "hello"
"#,
        )
        .expect("write workflow");

        let check = WorkflowShellStrictCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(".github/workflows/ci.yaml").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn reports_yaml_parse_failures() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join(".github/workflows")).expect("create workflows dir");
        fs::write(
            temp.path().join(".github/workflows/ci.yaml"),
            r#"jobs:
  test:
    steps:
      - run: |
          echo "hello"
      - bad: [unclosed
"#,
        )
        .expect("write workflow");

        let check = WorkflowShellStrictCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(".github/workflows/ci.yaml").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert!(result.findings[0].message.contains("failed to parse workflow YAML"));
    }
}
