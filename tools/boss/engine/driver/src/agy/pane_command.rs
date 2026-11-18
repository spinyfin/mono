//! The interactive pane command for an `agy` worker.
//!
//! The command is the shared initial-brief launcher running the Boss-owned
//! `agy` executable, never `agy -i`: `-i` puts the brief in argv and runs the
//! first turn behind the workspace-trust dialog, while the launcher holds the
//! brief until the composer is ready and the CLI's own startup log proves the
//! guards loaded (pane viability study, Q1; permission study, section 8).
//!
//! `--settings <file>` is not composed here. The permission renderer returns
//! it in `PermissionArtifacts::extra_args` and the spawn flow merges it into
//! this command, so the posture the CLI starts with is the one that was
//! rendered and attested.

use std::path::Path;

use boss_ssh_transport::shell_quote;

use super::environment::AgyProcessEnvironment;

/// Effort rungs `agy` accepts as `--effort`, which are also the model-id suffixes.
const MODEL_ID_RUNGS: [&str; 4] = ["low", "medium", "high", "xhigh"];

/// Values `agy` accepts for `--effort` (`max` is a flag value, not a model id suffix).
const EFFORT_VALUES: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The rung a model id encodes in its suffix (`gemini-4-hydrogen-xhigh`).
fn model_id_rung(model: &str) -> Option<&str> {
    let (_, suffix) = model.rsplit_once('-')?;
    MODEL_ID_RUNGS.contains(&suffix).then_some(suffix)
}

/// The `--effort` value to pass, or `None` to pass none.
///
/// The model id carries the rung, so a flag that disagrees with it would run
/// the worker at an effort nobody chose. The model id wins: a disagreeing or
/// unrecognised value is dropped, never rewritten.
fn effort_flag<'a>(model: &str, effort: Option<&'a str>) -> Option<&'a str> {
    let effort = effort.filter(|value| EFFORT_VALUES.contains(value))?;
    match model_id_rung(model) {
        Some(rung) if rung != effort => {
            tracing::warn!(
                model,
                effort,
                "dropping agy --effort that contradicts the model id rung"
            );
            None
        }
        _ => Some(effort),
    }
}

/// Compose the launcher invocation for one run.
///
/// `control_dir` holds the launcher, its configuration and the private brief
/// (see `initial_prompt::provision`), outside every checkout.
pub(super) fn build_agy_pane_command(
    environment: &AgyProcessEnvironment,
    control_dir: &Path,
    model: &str,
    effort: Option<&str>,
) -> String {
    let mut command = shell_quote(&environment.agy_binary().display().to_string());
    for arg in environment.launch_args() {
        command.push(' ');
        command.push_str(&shell_quote(&arg.to_string_lossy()));
    }
    command.push_str(" --model ");
    command.push_str(&shell_quote(model));
    if let Some(effort) = effort_flag(model, effort) {
        command.push_str(" --effort ");
        command.push_str(&shell_quote(effort));
    }
    command.push('\n');
    crate::initial_prompt::launcher_command(&shell_quote(&control_dir.display().to_string()), &command)
}

#[cfg(test)]
#[path = "pane_command_tests.rs"]
mod tests;
