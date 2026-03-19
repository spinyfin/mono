use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use tokio::task::JoinSet;

use crate::bypass::{bypass_applied_finding, bypass_failure_guidance, bypass_name_for_check_id};
use crate::check::CheckRegistry;
use crate::config::{CheckConfig, ConfigResolver};
use crate::external::{
    ExternalCheckExecutor, ExternalCheckPackage, ExternalCheckPackageProvider,
    NoopExternalCheckExecutor, NoopExternalCheckPackageProvider,
};
use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree};
use crate::output::{CheckResult, Finding, Severity};

#[derive(Debug, Clone)]
struct ScheduledCheckRun {
    configured_check_id: String,
    execution: ScheduledExecution,
    policy: EffectiveCheckPolicy,
    config: toml::Value,
    changeset: ChangeSet,
}

#[derive(Debug, Clone)]
enum ScheduledExecution {
    BuiltIn {
        implementation_check_id: String,
    },
    ExternalResolved {
        package: ExternalCheckPackage,
    },
    Invalid {
        message: String,
        remediation: Option<String>,
    },
}

#[derive(Debug, Clone)]
struct EffectiveCheckPolicy {
    severity_override: Option<Severity>,
    allow_bypass: bool,
    bypass_name: String,
}

impl EffectiveCheckPolicy {
    fn fingerprint(&self) -> String {
        format!(
            "severity={:?};allow_bypass={};bypass_name={}",
            self.severity_override, self.allow_bypass, self.bypass_name
        )
    }
}

pub struct Runner {
    registry: Arc<CheckRegistry>,
    resolver: Arc<ConfigResolver>,
    source_tree: Arc<dyn SourceTree>,
    external_package_provider: Arc<dyn ExternalCheckPackageProvider>,
    external_executor: Arc<dyn ExternalCheckExecutor>,
}

impl Runner {
    pub fn new(
        registry: Arc<CheckRegistry>,
        resolver: Arc<ConfigResolver>,
        source_tree: Arc<dyn SourceTree>,
    ) -> Self {
        Self::with_external(
            registry,
            resolver,
            source_tree,
            Arc::new(NoopExternalCheckPackageProvider),
            Arc::new(NoopExternalCheckExecutor),
        )
    }

    pub fn with_external_package_provider(
        registry: Arc<CheckRegistry>,
        resolver: Arc<ConfigResolver>,
        source_tree: Arc<dyn SourceTree>,
        external_package_provider: Arc<dyn ExternalCheckPackageProvider>,
    ) -> Self {
        Self::with_external(
            registry,
            resolver,
            source_tree,
            external_package_provider,
            Arc::new(NoopExternalCheckExecutor),
        )
    }

    pub fn with_external(
        registry: Arc<CheckRegistry>,
        resolver: Arc<ConfigResolver>,
        source_tree: Arc<dyn SourceTree>,
        external_package_provider: Arc<dyn ExternalCheckPackageProvider>,
        external_executor: Arc<dyn ExternalCheckExecutor>,
    ) -> Self {
        Self {
            registry,
            resolver,
            source_tree,
            external_package_provider,
            external_executor,
        }
    }

    pub async fn run_changeset(&self, changeset: &ChangeSet) -> Result<Vec<CheckResult>> {
        let scheduled = self.schedule_runs(changeset)?;

        let mut results = Vec::new();
        let mut join_set = JoinSet::new();
        for run in scheduled {
            match run.execution {
                ScheduledExecution::BuiltIn {
                    implementation_check_id,
                } => {
                    let Some(check) = self.registry.get(&implementation_check_id) else {
                        results.push(CheckResult {
                            check_id: run.configured_check_id,
                            findings: vec![Finding {
                                severity: Severity::Error,
                                message: format!(
                                    "configured check references unknown implementation `{implementation_check_id}`"
                                ),
                                location: None,
                                remediation: Some(
                                    "Register this check implementation in the binary or fix `check = ...` in CHECKS.yaml."
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
                    let run_policy = run.policy;

                    join_set.spawn(async move {
                        check
                            .run(&run_changeset, source_tree.as_ref(), &run_config)
                            .await
                            .map(|mut result| {
                                // Report findings under the configured instance id.
                                result.check_id = configured_check_id.clone();
                                apply_policy_to_result(result, &run_policy, &run_changeset)
                            })
                            .map_err(|err| (configured_check_id, err))
                    });
                }
                ScheduledExecution::ExternalResolved { package } => {
                    let external_executor = Arc::clone(&self.external_executor);
                    let source_tree = Arc::clone(&self.source_tree);
                    let configured_check_id = run.configured_check_id.clone();
                    let run_changeset = run.changeset;
                    let run_config = run.config;
                    let run_policy = run.policy;

                    join_set.spawn(async move {
                        external_executor
                            .execute(&package, &run_changeset, source_tree.as_ref(), &run_config)
                            .map(|mut result| {
                                result.check_id = configured_check_id.clone();
                                apply_policy_to_result(result, &run_policy, &run_changeset)
                            })
                            .map_err(|err| (configured_check_id, err))
                    });
                }
                ScheduledExecution::Invalid {
                    message,
                    remediation,
                } => {
                    results.push(CheckResult {
                        check_id: run.configured_check_id,
                        findings: vec![Finding {
                            severity: Severity::Error,
                            message,
                            location: None,
                            remediation,
                            suggested_fix: None,
                        }],
                    });
                }
            }
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
        let mut resolution_errors = BTreeMap::new();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }

            let resolved = self.resolver.resolve_for_file(&changed_file.path)?;
            if should_skip_file(changed_file, &resolved) {
                continue;
            }
            for check in resolved.enabled() {
                if let ScheduledExecution::Invalid { message, .. } =
                    self.resolve_scheduled_execution(check)
                {
                    resolution_errors.insert(check.id.clone(), message);
                }
                checks.insert(check.id.clone());
            }
        }

        if !resolution_errors.is_empty() {
            let details = resolution_errors
                .into_iter()
                .map(|(check_id, message)| format!("`{check_id}`: {message}"))
                .collect::<Vec<_>>()
                .join("\n- ");
            bail!("failed to resolve external check packages:\n- {details}");
        }

        Ok(checks.into_iter().collect())
    }

    fn schedule_runs(&self, changeset: &ChangeSet) -> Result<Vec<ScheduledCheckRun>> {
        let mut grouped_runs: BTreeMap<
            (String, String, String, String, String),
            ScheduledCheckRun,
        > = BTreeMap::new();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }

            let resolved = self.resolver.resolve_for_file(&changed_file.path)?;
            if should_skip_file(changed_file, &resolved) {
                continue;
            }
            for check in resolved.enabled() {
                let policy = self.resolve_effective_policy(check);
                let config_fingerprint = toml::to_string(&check.config).unwrap_or_default();
                let implementation_fingerprint = check
                    .implementation
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                let policy_fingerprint = policy.fingerprint();
                let key = (
                    check.id.clone(),
                    check.check.clone(),
                    implementation_fingerprint,
                    config_fingerprint,
                    policy_fingerprint,
                );

                let entry = grouped_runs
                    .entry(key)
                    .or_insert_with(|| ScheduledCheckRun {
                        configured_check_id: check.id.clone(),
                        execution: self.resolve_scheduled_execution(check),
                        policy,
                        config: check.config.clone(),
                        changeset: ChangeSet {
                            changed_files: Vec::new(),
                            file_line_deltas: HashMap::new(),
                            file_diffs: HashMap::new(),
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
                    if let Some(delta) = changeset.file_line_deltas.get(&changed_file.path) {
                        entry
                            .changeset
                            .file_line_deltas
                            .insert(changed_file.path.clone(), *delta);
                    }
                    if let Some(diff) = changeset.file_diffs.get(&changed_file.path) {
                        entry
                            .changeset
                            .file_diffs
                            .insert(changed_file.path.clone(), diff.clone());
                    }
                }
            }
        }

        Ok(grouped_runs.into_values().collect())
    }

    fn resolve_effective_policy(&self, check: &CheckConfig) -> EffectiveCheckPolicy {
        let severity_override = check.policy.severity;
        let allow_bypass = check.policy.allow_bypass.unwrap_or(false);
        let bypass_name = check
            .policy
            .bypass_name
            .clone()
            .unwrap_or_else(|| bypass_name_for_check_id(&check.id));

        EffectiveCheckPolicy {
            severity_override,
            allow_bypass,
            bypass_name,
        }
    }

    fn resolve_scheduled_execution(&self, check: &CheckConfig) -> ScheduledExecution {
        let Some(implementation_ref) = check.implementation.clone() else {
            return ScheduledExecution::BuiltIn {
                implementation_check_id: check.check.clone(),
            };
        };

        let package = match self.external_package_provider.resolve(&implementation_ref) {
            Ok(Some(package)) => package,
            Ok(None) => {
                return ScheduledExecution::Invalid {
                    message: format!(
                        "external implementation `{implementation_ref}` for check `{}` was not found in configured providers",
                        check.id
                    ),
                    remediation: Some(
                        "If this is a file implementation, ensure the manifest path exists. If this is generated, ensure the generated index is configured and includes the ID."
                            .to_owned(),
                    ),
                };
            }
            Err(err) => {
                return ScheduledExecution::Invalid {
                    message: format!(
                        "failed to resolve external implementation `{implementation_ref}` for check `{}`: {err:#}",
                        check.id
                    ),
                    remediation: None,
                };
            }
        };

        if package.id != check.check {
            return ScheduledExecution::Invalid {
                message: format!(
                    "external package id mismatch for check `{}`: expected `{}`, got `{}`",
                    check.id, check.check, package.id
                ),
                remediation: Some(
                    "Set `check = ...` to match the external package `id` or update the package manifest."
                        .to_owned(),
                ),
            };
        }

        ScheduledExecution::ExternalResolved { package }
    }
}

fn should_skip_file(changed_file: &ChangedFile, resolved: &crate::config::ResolvedChecks) -> bool {
    is_checks_config_file(&changed_file.path) && !resolved.include_config_files()
}

fn is_checks_config_file(path: &std::path::Path) -> bool {
    let file_name = path.file_name();
    file_name == Some(OsStr::new("CHECKS.yaml")) || file_name == Some(OsStr::new("CHECKS.toml"))
}

fn apply_policy_to_result(
    mut result: CheckResult,
    policy: &EffectiveCheckPolicy,
    changeset: &ChangeSet,
) -> CheckResult {
    if result.findings.is_empty() {
        return result;
    }

    if policy.allow_bypass {
        if let Some(reason) = changeset.bypass_reason(&policy.bypass_name) {
            let location = result
                .findings
                .iter()
                .find_map(|finding| finding.location.clone());
            result.findings = vec![bypass_applied_finding(
                &policy.bypass_name,
                &reason,
                location,
            )];
            return result;
        }

        let guidance = bypass_failure_guidance(&policy.bypass_name);
        for finding in &mut result.findings {
            finding.remediation = Some(match finding.remediation.take() {
                Some(remediation) => format!("{remediation} {guidance}"),
                None => guidance.clone(),
            });
        }
    }

    if let Some(severity_override) = policy.severity_override {
        for finding in &mut result.findings {
            finding.severity = severity_override;
        }
    }

    result
}

#[cfg(test)]
mod tests;
