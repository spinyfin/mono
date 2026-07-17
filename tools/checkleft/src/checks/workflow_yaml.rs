//! Shared plumbing for the GitHub-workflow YAML checks.
//!
//! The `workflow-*` checks all consume the same document shape: a workflow
//! file under `.github/workflows`, parsed into a `jobs` -> `steps` tree. This
//! module owns that traversal so each check keeps only its own predicate and
//! finding logic.

use std::path::Path;

use anyhow::{Context, Result};
use serde_yaml::{Mapping, Value};

use crate::input::ChangedFile;
use crate::output::{Finding, Location, Severity};

pub(super) fn is_github_workflow_file(path: &Path) -> bool {
    if !path.starts_with(Path::new(".github/workflows")) {
        return false;
    }

    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("yml") | Some("yaml")
    )
}

pub(super) fn parse_workflow(contents: &str) -> Result<Value> {
    serde_yaml::from_str(contents).context("invalid YAML document")
}

pub(super) fn mapping_get<'a>(mapping: &'a Mapping, key: &str) -> Option<&'a Value> {
    mapping.get(Value::String(key.to_owned()))
}

/// One step of one job, as located by [`workflow_steps`].
#[derive(Debug)]
pub(super) struct WorkflowStep<'a> {
    pub(super) job_name: String,
    /// 1-based, as reported in user-visible finding messages.
    pub(super) step_index: usize,
    pub(super) step: &'a Mapping,
}

/// Walks `jobs` -> `steps`, yielding every step mapping with its job name and
/// 1-based index. Jobs and steps that are not mappings, and jobs without a
/// `steps` sequence, are skipped.
pub(super) fn workflow_steps(workflow: &Value) -> Vec<WorkflowStep<'_>> {
    let mut steps_out = Vec::new();
    let Some(root) = workflow.as_mapping() else {
        return steps_out;
    };
    let Some(jobs) = mapping_get(root, "jobs").and_then(Value::as_mapping) else {
        return steps_out;
    };

    for (job_key, job_value) in jobs {
        let Some(job) = job_value.as_mapping() else {
            continue;
        };
        let Some(steps) = mapping_get(job, "steps").and_then(Value::as_sequence) else {
            continue;
        };
        let job_name = job_key.as_str().unwrap_or("<unknown-job>").to_owned();

        for (index, step) in steps.iter().enumerate() {
            let Some(step_map) = step.as_mapping() else {
                continue;
            };
            steps_out.push(WorkflowStep {
                job_name: job_name.clone(),
                step_index: index + 1,
                step: step_map,
            });
        }
    }

    steps_out
}

/// The file-level `Finding` every workflow check emits when the YAML itself
/// cannot be parsed. Callers supply their own wording.
pub(super) fn yaml_parse_finding(changed_file: &ChangedFile, message: String, remediation: &str) -> Finding {
    Finding {
        fixable: false,
        severity: Severity::Error,
        message,
        location: Some(Location {
            path: changed_file.path.clone(),
            line: None,
            column: None,
        }),
        remediations: vec![remediation.to_owned()],
        suggested_fix: None,
    }
}
