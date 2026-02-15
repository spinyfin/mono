use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use tokio::task::JoinSet;

use crate::check::CheckRegistry;
use crate::config::ConfigResolver;
use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree};
use crate::output::{CheckResult, Finding, Severity};

#[derive(Debug, Clone)]
struct ScheduledCheckRun {
    configured_check_id: String,
    implementation_check_id: String,
    config: toml::Value,
    changeset: ChangeSet,
}

pub struct Runner {
    registry: Arc<CheckRegistry>,
    resolver: Arc<ConfigResolver>,
    source_tree: Arc<dyn SourceTree>,
}

impl Runner {
    pub fn new(
        registry: Arc<CheckRegistry>,
        resolver: Arc<ConfigResolver>,
        source_tree: Arc<dyn SourceTree>,
    ) -> Self {
        Self {
            registry,
            resolver,
            source_tree,
        }
    }

    pub async fn run_changeset(&self, changeset: &ChangeSet) -> Result<Vec<CheckResult>> {
        let scheduled = self.schedule_runs(changeset)?;

        let mut results = Vec::new();
        let mut join_set = JoinSet::new();
        for run in scheduled {
            let Some(check) = self.registry.get(&run.implementation_check_id) else {
                results.push(CheckResult {
                    check_id: run.configured_check_id,
                    findings: vec![Finding {
                        severity: Severity::Error,
                        message: format!(
                            "configured check references unknown implementation `{}`",
                            run.implementation_check_id
                        ),
                        location: None,
                        remediation: Some(
                            "Register this check implementation in the binary or fix `check = ...` in CHECKS.toml."
                                .to_owned(),
                        ),
                        suggested_fix: None,
                    }],
                });
                continue;
            };
            let source_tree = Arc::clone(&self.source_tree);
            let configured_check_id = run.configured_check_id.clone();
            let run_changeset = run.changeset;
            let run_config = run.config;

            join_set.spawn(async move {
                check
                    .run(&run_changeset, source_tree.as_ref(), &run_config)
                    .await
                    .map(|mut result| {
                        // Report findings under the configured instance id.
                        result.check_id = configured_check_id.clone();
                        result
                    })
                    .map_err(|err| (configured_check_id, err))
            });
        }

        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok(Ok(result)) => results.push(result),
                Ok(Err((check_id, err))) => {
                    results.push(CheckResult {
                        check_id,
                        findings: vec![Finding {
                            severity: Severity::Error,
                            message: format!("check execution failed: {err}"),
                            location: None,
                            remediation: None,
                            suggested_fix: None,
                        }],
                    });
                }
                Err(join_err) => {
                    return Err(anyhow!("runner task failed to execute: {join_err}"));
                }
            }
        }

        results.sort_by(|left, right| left.check_id.cmp(&right.check_id));
        Ok(results)
    }

    pub fn list_configured_checks(&self, changeset: &ChangeSet) -> Result<Vec<String>> {
        let mut checks = BTreeSet::new();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }

            let resolved = self.resolver.resolve_for_file(&changed_file.path)?;
            if should_skip_file(changed_file, &resolved) {
                continue;
            }
            checks.extend(resolved.enabled().map(|check| check.id.clone()));
        }

        Ok(checks.into_iter().collect())
    }

    fn schedule_runs(&self, changeset: &ChangeSet) -> Result<Vec<ScheduledCheckRun>> {
        let mut grouped_runs: BTreeMap<(String, String), ScheduledCheckRun> = BTreeMap::new();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }

            let resolved = self.resolver.resolve_for_file(&changed_file.path)?;
            if should_skip_file(changed_file, &resolved) {
                continue;
            }
            for check in resolved.enabled() {
                let config_fingerprint = toml::to_string(&check.config).unwrap_or_default();
                let key = (check.id.clone(), config_fingerprint);

                let entry = grouped_runs
                    .entry(key)
                    .or_insert_with(|| ScheduledCheckRun {
                        configured_check_id: check.id.clone(),
                        implementation_check_id: check.check.clone(),
                        config: check.config.clone(),
                        changeset: ChangeSet {
                            changed_files: Vec::new(),
                            commit_description: changeset.commit_description.clone(),
                            pr_description: changeset.pr_description.clone(),
                            change_id: changeset.change_id.clone(),
                            repository: changeset.repository.clone(),
                        },
                    });

                let already_present = entry
                    .changeset
                    .changed_files
                    .iter()
                    .any(|scheduled_file| scheduled_file.path == changed_file.path);
                if !already_present {
                    entry.changeset.changed_files.push(ChangedFile {
                        path: changed_file.path.clone(),
                        kind: changed_file.kind,
                        old_path: changed_file.old_path.clone(),
                    });
                }
            }
        }

        Ok(grouped_runs.into_values().collect())
    }
}

fn should_skip_file(changed_file: &ChangedFile, resolved: &crate::config::ResolvedChecks) -> bool {
    is_checks_config_file(&changed_file.path) && !resolved.include_config_files()
}

fn is_checks_config_file(path: &std::path::Path) -> bool {
    path.file_name() == Some(OsStr::new("CHECKS.toml"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use async_trait::async_trait;
    use tempfile::tempdir;

    use crate::check::{Check, CheckRegistry};
    use crate::config::ConfigResolver;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree};
    use crate::output::{CheckResult, Severity};
    use crate::source_tree::LocalSourceTree;

    use super::Runner;

    struct CapturingCheck {
        id: String,
        seen_files: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Check for CapturingCheck {
        fn id(&self) -> &str {
            &self.id
        }

        fn description(&self) -> &str {
            "captures the input files"
        }

        async fn run(
            &self,
            changeset: &ChangeSet,
            _tree: &dyn SourceTree,
            _config: &toml::Value,
        ) -> Result<CheckResult> {
            let files: Vec<_> = changeset
                .changed_files
                .iter()
                .map(|changed| changed.path.display().to_string())
                .collect();
            self.seen_files.lock().expect("lock files").extend(files);

            Ok(CheckResult {
                check_id: self.id().to_owned(),
                findings: Vec::new(),
            })
        }
    }

    struct MetadataCapturingCheck {
        id: String,
        directive_name: String,
        seen_bypass_reason: Arc<Mutex<Option<String>>>,
        seen_change_id: Arc<Mutex<Option<String>>>,
        seen_repository: Arc<Mutex<Option<String>>>,
    }

    #[async_trait]
    impl Check for MetadataCapturingCheck {
        fn id(&self) -> &str {
            &self.id
        }

        fn description(&self) -> &str {
            "captures description and change metadata"
        }

        async fn run(
            &self,
            changeset: &ChangeSet,
            _tree: &dyn SourceTree,
            _config: &toml::Value,
        ) -> Result<CheckResult> {
            *self.seen_bypass_reason.lock().expect("lock bypass reason") =
                changeset.bypass_reason(&self.directive_name);
            *self.seen_change_id.lock().expect("lock change id") = changeset.change_id.clone();
            *self.seen_repository.lock().expect("lock repository") = changeset.repository.clone();

            Ok(CheckResult {
                check_id: self.id().to_owned(),
                findings: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn runner_groups_files_by_check() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("backend/src")).expect("create dirs");
        fs::write(
            temp.path().join("CHECKS.toml"),
            r#"
[[checks]]
id = "capture"
"#,
        )
        .expect("write config");

        let seen_files = Arc::new(Mutex::new(Vec::new()));
        let mut registry = CheckRegistry::new();
        registry
            .register(CapturingCheck {
                id: "capture".to_owned(),
                seen_files: Arc::clone(&seen_files),
            })
            .expect("register check");

        let runner = Runner::new(
            Arc::new(registry),
            Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
            Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        );

        let results = runner
            .run_changeset(&ChangeSet::new(vec![
                ChangedFile {
                    path: Path::new("backend/src/a.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                },
                ChangedFile {
                    path: Path::new("backend/src/b.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                },
            ]))
            .await
            .expect("run checks");

        assert_eq!(results.len(), 1);
        let files = seen_files.lock().expect("lock files").clone();
        assert_eq!(
            files,
            vec!["backend/src/a.rs".to_owned(), "backend/src/b.rs".to_owned()]
        );
    }

    #[tokio::test]
    async fn runner_propagates_description_and_change_metadata_to_checks() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("backend/src")).expect("create dirs");
        fs::write(
            temp.path().join("CHECKS.toml"),
            r#"
[[checks]]
id = "capture-descriptions"
"#,
        )
        .expect("write config");

        let directive_name = "BYPASS_CAPTURE_DESCRIPTIONS".to_owned();
        let seen_bypass_reason = Arc::new(Mutex::new(None));
        let seen_change_id = Arc::new(Mutex::new(None));
        let seen_repository = Arc::new(Mutex::new(None));
        let mut registry = CheckRegistry::new();
        registry
            .register(MetadataCapturingCheck {
                id: "capture-descriptions".to_owned(),
                directive_name: directive_name.clone(),
                seen_bypass_reason: Arc::clone(&seen_bypass_reason),
                seen_change_id: Arc::clone(&seen_change_id),
                seen_repository: Arc::clone(&seen_repository),
            })
            .expect("register check");

        let runner = Runner::new(
            Arc::new(registry),
            Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
            Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        );

        let results = runner
            .run_changeset(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("backend/src/a.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }])
                .with_commit_description(Some(
                    "BYPASS_CAPTURE_DESCRIPTIONS=Legitimate exception for validation.".to_owned(),
                ))
                .with_change_id(Some("235".to_owned()))
                .with_repository(Some("brianduff/flunge".to_owned())),
            )
            .await
            .expect("run checks");

        assert_eq!(results.len(), 1);
        assert_eq!(
            *seen_bypass_reason.lock().expect("lock bypass reason"),
            Some("Legitimate exception for validation.".to_owned())
        );
        assert_eq!(
            *seen_change_id.lock().expect("lock change id"),
            Some("235".to_owned())
        );
        assert_eq!(
            *seen_repository.lock().expect("lock repository"),
            Some("brianduff/flunge".to_owned())
        );
    }

    #[tokio::test]
    async fn runner_ignores_checks_toml_by_default() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("CHECKS.toml"),
            r#"
[[checks]]
id = "capture"
"#,
        )
        .expect("write config");

        let seen_files = Arc::new(Mutex::new(Vec::new()));
        let mut registry = CheckRegistry::new();
        registry
            .register(CapturingCheck {
                id: "capture".to_owned(),
                seen_files: Arc::clone(&seen_files),
            })
            .expect("register check");

        let runner = Runner::new(
            Arc::new(registry),
            Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
            Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        );

        let results = runner
            .run_changeset(&ChangeSet::new(vec![ChangedFile {
                path: Path::new("CHECKS.toml").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }]))
            .await
            .expect("run checks");

        assert!(results.is_empty());
        let files = seen_files.lock().expect("lock files").clone();
        assert!(files.is_empty());

        let configured = runner
            .list_configured_checks(&ChangeSet::new(vec![ChangedFile {
                path: Path::new("CHECKS.toml").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }]))
            .expect("list checks");
        assert!(configured.is_empty());
    }

    #[tokio::test]
    async fn runner_can_opt_in_to_check_checks_toml() {
        let temp = tempdir().expect("create temp dir");
        fs::write(
            temp.path().join("CHECKS.toml"),
            r#"
[settings]
include_config_files = true

[[checks]]
id = "capture"
"#,
        )
        .expect("write config");

        let seen_files = Arc::new(Mutex::new(Vec::new()));
        let mut registry = CheckRegistry::new();
        registry
            .register(CapturingCheck {
                id: "capture".to_owned(),
                seen_files: Arc::clone(&seen_files),
            })
            .expect("register check");

        let runner = Runner::new(
            Arc::new(registry),
            Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
            Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        );

        let results = runner
            .run_changeset(&ChangeSet::new(vec![ChangedFile {
                path: Path::new("CHECKS.toml").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }]))
            .await
            .expect("run checks");

        assert_eq!(results.len(), 1);
        let files = seen_files.lock().expect("lock files").clone();
        assert_eq!(files, vec!["CHECKS.toml".to_owned()]);

        let configured = runner
            .list_configured_checks(&ChangeSet::new(vec![ChangedFile {
                path: Path::new("CHECKS.toml").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }]))
            .expect("list checks");
        assert_eq!(configured, vec!["capture".to_owned()]);
    }

    #[tokio::test]
    async fn runner_reports_check_errors_in_output() {
        struct FailingCheck;

        #[async_trait]
        impl Check for FailingCheck {
            fn id(&self) -> &str {
                "fails"
            }

            fn description(&self) -> &str {
                "fails intentionally"
            }

            async fn run(
                &self,
                _changeset: &ChangeSet,
                _tree: &dyn SourceTree,
                _config: &toml::Value,
            ) -> Result<CheckResult> {
                anyhow::bail!("boom");
            }
        }

        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("backend/src")).expect("create dirs");
        fs::write(
            temp.path().join("CHECKS.toml"),
            r#"
[[checks]]
id = "fails"
"#,
        )
        .expect("write config");

        let mut registry = CheckRegistry::new();
        registry.register(FailingCheck).expect("register check");

        let runner = Runner::new(
            Arc::new(registry),
            Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
            Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        );

        let results = runner
            .run_changeset(&ChangeSet::new(vec![ChangedFile {
                path: Path::new("backend/src/a.rs").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }]))
            .await
            .expect("run checks");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].check_id, "fails");
        assert_eq!(results[0].findings[0].severity, Severity::Error);
        assert!(results[0].findings[0].message.contains("boom"));
    }

    #[tokio::test]
    async fn runner_reports_unknown_configured_checks() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("backend/src")).expect("create dirs");
        fs::write(
            temp.path().join("CHECKS.toml"),
            r#"
[[checks]]
id = "spelling-typos"
check = "not-registered"
"#,
        )
        .expect("write config");

        let runner = Runner::new(
            Arc::new(CheckRegistry::new()),
            Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
            Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        );

        let results = runner
            .run_changeset(&ChangeSet::new(vec![ChangedFile {
                path: Path::new("backend/src/a.rs").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }]))
            .await
            .expect("run checks");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].check_id, "spelling-typos");
        assert_eq!(results[0].findings[0].severity, Severity::Error);
        assert!(
            results[0].findings[0]
                .message
                .contains("unknown implementation")
        );
    }

    #[tokio::test]
    async fn runner_reports_instance_id_not_implementation_id() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("docs")).expect("create dirs");
        fs::write(temp.path().join("docs/file.md"), "teh value\n").expect("write file");
        fs::write(
            temp.path().join("CHECKS.toml"),
            r#"
[[checks]]
id = "spelling"
check = "capture"
"#,
        )
        .expect("write config");

        let seen_files = Arc::new(Mutex::new(Vec::new()));
        let mut registry = CheckRegistry::new();
        registry
            .register(CapturingCheck {
                id: "capture".to_owned(),
                seen_files,
            })
            .expect("register check");

        let runner = Runner::new(
            Arc::new(registry),
            Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
            Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        );

        let results = runner
            .run_changeset(&ChangeSet::new(vec![ChangedFile {
                path: Path::new("docs/file.md").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }]))
            .await
            .expect("run checks");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].check_id, "spelling");
    }

    #[test]
    fn list_configured_checks_deduplicates() {
        let temp = tempdir().expect("create temp dir");
        fs::create_dir_all(temp.path().join("backend/src")).expect("create dirs");
        fs::write(
            temp.path().join("CHECKS.toml"),
            r#"
[[checks]]
id = "file-size"

[[checks]]
id = "spelling-typos"
"#,
        )
        .expect("write config");

        let runner = Runner::new(
            Arc::new(CheckRegistry::new()),
            Arc::new(ConfigResolver::new(temp.path()).expect("resolver")),
            Arc::new(LocalSourceTree::new(temp.path()).expect("tree")),
        );

        let checks = runner
            .list_configured_checks(&ChangeSet::new(vec![
                ChangedFile {
                    path: Path::new("backend/src/a.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                },
                ChangedFile {
                    path: Path::new("backend/src/b.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                },
            ]))
            .expect("list checks");

        let check_map: BTreeMap<_, _> = checks
            .iter()
            .enumerate()
            .map(|(index, id)| (id.clone(), index))
            .collect();
        assert_eq!(check_map.len(), 2);
        assert!(check_map.contains_key("file-size"));
        assert!(check_map.contains_key("spelling-typos"));
    }
}
