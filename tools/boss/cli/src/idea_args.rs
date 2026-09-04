//! `boss idea …` clap command / argument / value-enum definitions.
//!
//! Split out of `commands.rs` to keep that file under the repo's
//! file-size limit (mirrors `status_args.rs`).

use crate::*;

/// Subcommands under `boss idea …`.
///
/// Markdown drafts, authored over time and later graduated into a chore or
/// project. Selectors accept `I<n>` (requires `--product`) or the canonical
/// `idea_…` id.
#[derive(Debug, Subcommand)]
pub(crate) enum IdeaCommand {
    /// Create a new idea (markdown draft) for a product.
    ///
    /// `--body` / `--body-file` are both optional — an idea can be created
    /// with just a name and authored incrementally afterward via `update`.
    Create(IdeaCreateArgs),
    /// List ideas for a product, newest first.
    ///
    /// `--status` filters to one lifecycle state; omitted, every idea
    /// regardless of status is returned.
    List(IdeaListArgs),
    /// Show one idea by `I<n>` or `idea_…` id.
    Show(IdeaSelectorArgs),
    /// Update an idea's name and/or body. Only supplied flags are changed.
    Update(IdeaUpdateArgs),
    /// Permanently delete an idea.
    Delete(IdeaSelectorArgs),
    /// Graduate a `draft` idea into a chore or project.
    ///
    /// A thin, deterministic wrapper — not a general promote/convert
    /// mechanism. The idea is kept (never deleted) and flipped to
    /// `graduated` with `graduated_to_id` pointing at what it became.
    /// Graduating to a project puts the idea's markdown into the
    /// auto-minted design seed task's description, born with autostart
    /// disabled so the gesture never silently dispatches a design worker.
    /// `--effort` / `--reasoning` apply only to `--as chore`. Only a
    /// `draft` idea can be graduated.
    Graduate(IdeaGraduateArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum IdeaStatusArg {
    Draft,
    Graduated,
    Archived,
}

impl IdeaStatusArg {
    pub(crate) fn as_protocol(self) -> boss_protocol::IdeaStatus {
        match self {
            Self::Draft => boss_protocol::IdeaStatus::Draft,
            Self::Graduated => boss_protocol::IdeaStatus::Graduated,
            Self::Archived => boss_protocol::IdeaStatus::Archived,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum IdeaGraduateAsArg {
    Chore,
    Project,
}

impl IdeaGraduateAsArg {
    pub(crate) fn as_protocol(self) -> boss_protocol::IdeaGraduationKind {
        match self {
            Self::Chore => boss_protocol::IdeaGraduationKind::Chore,
            Self::Project => boss_protocol::IdeaGraduationKind::Project,
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct IdeaCreateArgs {
    /// Product to create the idea in (id or slug).
    #[arg(long)]
    pub(crate) product: Option<String>,
    /// Idea name/title.
    #[arg(long)]
    pub(crate) name: Option<String>,
    /// Markdown draft body, given inline.
    #[arg(long, conflicts_with = "body_file")]
    pub(crate) body: Option<String>,
    /// Markdown draft body, read from a file. Prefer this over `--body` for
    /// anything long — ideas are markdown drafts, and shell-quoting
    /// multi-line markdown is painful.
    #[arg(long = "body-file", value_name = "PATH", conflicts_with = "body")]
    pub(crate) body_file: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct IdeaListArgs {
    /// Product whose ideas to list.
    #[arg(long)]
    pub(crate) product: Option<String>,
    /// Filter to one lifecycle state. Omitted: every idea regardless of status.
    #[arg(long)]
    pub(crate) status: Option<IdeaStatusArg>,
}

/// Shared selector args for show / delete.
#[derive(Debug, Args)]
pub(crate) struct IdeaSelectorArgs {
    /// Idea selector: `I<n>` (e.g. `I1`) or canonical `idea_…` id.
    pub(crate) selector: String,
    /// Product context for `I<n>` selectors. Not needed for `idea_…` ids.
    #[arg(long)]
    pub(crate) product: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct IdeaUpdateArgs {
    /// Idea selector: `I<n>` or canonical `idea_…` id.
    pub(crate) selector: String,
    /// Product context for `I<n>` selectors.
    #[arg(long)]
    pub(crate) product: Option<String>,
    /// New name/title.
    #[arg(long)]
    pub(crate) name: Option<String>,
    /// New markdown body, given inline.
    #[arg(long, conflicts_with = "body_file")]
    pub(crate) body: Option<String>,
    /// New markdown body, read from a file.
    #[arg(long = "body-file", value_name = "PATH", conflicts_with = "body")]
    pub(crate) body_file: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct IdeaGraduateArgs {
    /// Idea selector: `I<n>` or canonical `idea_…` id.
    pub(crate) selector: String,
    /// Product context for `I<n>` selectors.
    #[arg(long)]
    pub(crate) product: Option<String>,
    /// Graduation target: `chore` or `project`.
    #[arg(long = "as")]
    pub(crate) target: IdeaGraduateAsArg,
    /// Override the produced row's name/title. Defaults to the idea's own name.
    #[arg(long)]
    pub(crate) name: Option<String>,
    /// Effort level for the produced chore. Only valid with `--as chore`.
    #[arg(long)]
    pub(crate) effort: Option<EffortLevelArg>,
    /// Reasoning mode for the produced chore. Only valid with `--as chore`.
    #[arg(long)]
    pub(crate) reasoning: Option<ReasoningArg>,
}
