use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredRun {
    pub kind: Kind,
    pub tool: Tool,
    pub command: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Lint,
    Build,
    Test,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    Bazel,
    Project,
}

pub fn validate_contract(runs: &[RequiredRun]) -> Result<()> {
    anyhow::ensure!(!runs.is_empty(), "empty verification contract");
    for run in runs {
        anyhow::ensure!(!run.command.trim().is_empty(), "empty verification command");
        if matches!(run.tool, Tool::Bazel) {
            let words: Vec<_> = run.command.split_whitespace().collect();
            let verb = match run.kind {
                Kind::Build => "build",
                Kind::Test => "test",
                Kind::Lint => "run",
            };
            anyhow::ensure!(
                words.first() == Some(&"bazel") && words.get(1) == Some(&verb),
                "Bazel verification kind disagrees with command"
            );
        }
    }
    Ok(())
}

/// `true` for a `failure_kind` whose failing evidence lives on a
/// synthetic/ephemeral commit distinct from the PR's own head — the PR's
/// own head-branch CI reads green for both, so a clean head-branch probe
/// must never be trusted to clear (or validate a rebase claim against) the
/// attempt. `'merge_queue_rebounce'` (GitHub's native merge queue) and
/// `'trunk_queue_eviction'` (Trunk's merge queue) share this property —
/// see `on_trunk_queue_eviction_detected`'s doc comment for the trunk case.
/// `None` (pre-`failure_kind` rows) and `'pr_branch_ci'` are ordinary.
pub fn is_queue_side_failure_kind(kind: Option<&str>) -> bool {
    matches!(kind, Some("merge_queue_rebounce") | Some("trunk_queue_eviction"))
}
