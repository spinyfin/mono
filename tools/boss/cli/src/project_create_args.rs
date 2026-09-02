//! `boss project create` arguments, kept outside `commands.rs` so that
//! command definitions remain below the repository file-size limit.

use crate::*;

#[derive(Debug, Clone, Args)]
pub(crate) struct ProjectCreateArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    #[arg(long)]
    pub(crate) name: Option<String>,

    #[arg(long)]
    pub(crate) description: Option<String>,

    #[arg(long)]
    pub(crate) goal: Option<String>,

    /// Skip the auto-generated `kind=design` seed task. Pass this for
    /// non-design-shaped projects (postmortems, checklists, milestone
    /// aggregators) where the seed task would be dead weight.
    /// Defaults to false (preserves existing behaviour).
    #[arg(long = "no-design-task", default_value_t = false)]
    pub(crate) no_design_task: bool,

    /// Set the auto-created design task's driver reasoning effort to `xhigh`.
    /// This is a coordinator judgement for a complex design, not the work-size
    /// `--effort` estimate, and defaults to off.
    #[arg(long = "design-reasoning-effort-xhigh", default_value_t = false)]
    pub(crate) design_reasoning_effort_xhigh: bool,
}
