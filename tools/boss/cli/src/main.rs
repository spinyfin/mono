pub(crate) use std::collections::HashMap;
pub(crate) use std::io::{self, IsTerminal, Read, Write};
pub(crate) use std::path::PathBuf;
pub(crate) use std::process::{Command, ExitCode};

pub(crate) use anyhow::Result;
pub(crate) use boss_client::{
    BossClient, Discovery, engine_socket_reachable, ensure_engine_running, running_engine_pid, stop_engine,
};
pub(crate) use boss_protocol::{
    AddDependencyInput, Attention, AttentionGroup, Automation, AutomationDedupSuppression, AutomationPatch,
    AutomationRun, AutomationTrigger, CREATED_VIA_CLI, CiBudgetSnapshot, CiRemediation, ConflictHotspotReport,
    ConflictResolution, CreateAttentionInput, CreateAutomationInput, CreateChoreInput, CreateInvestigationInput,
    CreateManyChoresInput, CreateManyTasksInput, CreateProductInput, CreateProjectInput, CreateRevisionInput,
    CreateTaskInput, DependencyDirection, DependencyEdge, DependencyFilter, EditorialAction, EditorialRules,
    EffortAuditReport, EffortLevel, EngineAttemptListEntry, ExecutionKind, FollowupMemberOverride, FrontendEvent,
    FrontendRequest, GitHubAuthStateDto, LinkExternalRefInput, ListDependenciesInput, OrgAuthState, PlannerOutput,
    PlannerRun, PrWorkItemMatch, Product, Project, ProjectDesignDocState, RemoveDependencyInput,
    ResolveProjectDesignDocOutput, ResolvedDesignDocKind, SetProductEditorialRulesInput,
    SetProductExternalTrackerInput, SetProjectDesignDocInput, Task, TaskRuntime, UnpopulatePreservedTask,
    WorkAttentionItem, WorkExecution, WorkItem, WorkItemDependency, WorkItemDependencyDetail, WorkItemDependencyView,
    WorkItemPatch,
};
pub(crate) use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
pub(crate) use comfy_table::{Cell, ContentArrangement, Table};
pub(crate) use serde::Serialize;

mod buildkite_release;
mod context;
mod propose;
mod repo_resolution;
pub(crate) use boss_github as github_app;
pub(crate) use git_utils::repo_slug::short_name_for;

/// Send an RPC request and map the engine's reply to a `Result`.
///
/// Encapsulates the boilerplate shared by every dedicated RPC wrapper:
/// it awaits `send_request`, maps transport failures through
/// [`CliError::internal`], yields the happy-path value on the expected
/// event variant, maps `WorkError`/`Error` events to
/// [`CliError::application`], and any other event to [`unexpected_event`]
/// with the supplied context `$label`.
///
/// The common form wraps the happy-path value in `Ok`:
/// ```ignore
/// rpc_call!(
///     client,
///     FrontendRequest::GetAutomation { id: id.to_owned() },
///     "automation show",
///     FrontendEvent::AutomationResult { automation } => automation,
/// )
/// ```
///
/// When the happy arm already yields a `Result` (e.g. it runs
/// `expect_product(item)`), prefix the invocation with `try` so the
/// value is returned as-is instead of being wrapped in `Ok`:
/// ```ignore
/// rpc_call!(
///     try client,
///     FrontendRequest::CreateProduct { input },
///     "product create",
///     FrontendEvent::WorkItemCreated { item } => expect_product(item),
/// )
/// ```
macro_rules! rpc_call {
    // Internal: run the request and dispatch on the reply. `$result` must
    // evaluate to the wrapper's `Result` return type.
    (@run $client:expr, $req:expr, $label:expr, $happy:pat => $result:expr) => {
        match $client.send_request(&$req).await.map_err(CliError::internal)? {
            $happy => $result,
            FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                Err(CliError::application(message))
            }
            // Worker-tier refusal. Handled in the shared macro rather than
            // per verb because any verb can draw one: the engine's gate runs
            // before dispatch and applies to the whole surface. The denial's
            // own message already names the refused verb and the `boss
            // propose …` to use instead, so it renders as-is.
            FrontendEvent::WorkerTierDenied { denial } => Err(CliError::application(denial.message)),
            other => Err(unexpected_event($label, &other)),
        }
    };
    // Happy arm already yields a `Result`; return it unchanged.
    (try $client:expr, $req:expr, $label:expr, $happy:pat => $value:expr $(,)?) => {
        rpc_call!(@run $client, $req, $label, $happy => $value)
    };
    // Happy arm yields a plain value; wrap it in `Ok`.
    ($client:expr, $req:expr, $label:expr, $happy:pat => $value:expr $(,)?) => {
        rpc_call!(@run $client, $req, $label, $happy => Ok($value))
    };
}

mod automation_cmds;
mod commands;
mod data;
mod engine_cmds;
mod output;
mod work_cmds;

pub(crate) use automation_cmds::*;
pub(crate) use commands::*;
pub(crate) use data::*;
pub(crate) use engine_cmds::*;
pub(crate) use output::*;
pub(crate) use work_cmds::*;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputMode {
    Human,
    Json,
}

#[derive(bon::Builder, Debug, Serialize)]
#[builder(on(String, into))]
pub(crate) struct CliReferenceDocument {
    pub(crate) cli: &'static str,
    pub(crate) usage_rules: Vec<&'static str>,
    pub(crate) selector_semantics: Vec<&'static str>,
    pub(crate) status_semantics: Vec<&'static str>,
    pub(crate) workflow_guidance: Vec<&'static str>,
    pub(crate) commands: Vec<CliReferenceSection>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CliReferenceSection {
    pub(crate) path: String,
    pub(crate) help: String,
}

#[derive(Debug)]
pub(crate) enum CliError {
    Usage(String),
    NotFound(String),
    Conflict(String),
    EngineUnavailable(String),
    Application(String),
    Internal(anyhow::Error),
}

impl CliError {
    pub(crate) fn internal(err: impl Into<anyhow::Error>) -> Self {
        Self::Internal(err.into())
    }

    pub(crate) fn usage(message: impl Into<String>) -> Self {
        Self::Usage(message.into())
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }

    pub(crate) fn engine_unavailable(message: impl Into<String>) -> Self {
        Self::EngineUnavailable(message.into())
    }

    pub(crate) fn application(message: impl Into<String>) -> Self {
        Self::Application(message.into())
    }

    pub(crate) fn exit_code(&self) -> ExitCode {
        match self {
            Self::Usage(_) => ExitCode::from(2),
            Self::NotFound(_) => ExitCode::from(3),
            Self::Conflict(_) => ExitCode::from(4),
            Self::EngineUnavailable(_) => ExitCode::from(5),
            Self::Application(_) => ExitCode::from(6),
            Self::Internal(_) => ExitCode::from(7),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(message)
            | Self::NotFound(message)
            | Self::Conflict(message)
            | Self::EngineUnavailable(message)
            | Self::Application(message) => f.write_str(message),
            Self::Internal(err) => write!(f, "{err:#}"),
        }
    }
}

pub(crate) struct RunContext {
    pub(crate) output_mode: OutputMode,
    pub(crate) quiet: bool,
    pub(crate) allow_input: bool,
    pub(crate) discovery: Discovery,
    /// Mirror of the global `--no-autostart` flag. Gates per-work-item
    /// auto-dispatch (`boss chore create --no-autostart` → engine
    /// creates the chore in `todo` but does not spin up a worker for
    /// it). It does NOT affect transparent engine startup — that is
    /// governed by `--no-engine-autostart` via `discovery.autostart`.
    pub(crate) no_autostart: bool,
}

// Per-binary build-info stamp + `version_string` accessor. The
// include!(env!("BOSS_BUILD_INFO_RS")) must be evaluated in this crate
// (this rust_binary sets its own rustc_env), so the shared logic is a
// macro rather than a plain function. See the boss_build_info crate.
boss_build_info::stamp!();

#[tokio::main]
pub(crate) async fn main() -> ExitCode {
    // Intercept --version/-V before Cli::parse() so we print the
    // canonical version string.
    if boss_build_info::print_version_if_requested(&build_info::version_string("boss")) {
        return ExitCode::SUCCESS;
    }

    let cli = Cli::parse();
    match run_cli(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            err.exit_code()
        }
    }
}

pub(crate) async fn run_cli(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        Commands::Reference => run_reference_command(&cli.global),
        Commands::Product { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_product_command(command, &ctx).await
        }
        Commands::Project { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_project_command(command, &ctx).await
        }
        Commands::Task { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_task_command(command, &ctx).await
        }
        Commands::Chore { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_chore_command(command, &ctx).await
        }
        Commands::Comment { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_comment_command(command, &ctx).await
        }
        Commands::Automation { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_automation_command(command, &ctx).await
        }
        Commands::Attention { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_attention_command(command, &ctx).await
        }
        Commands::Propose(args) => {
            let ctx = RunContext::from_flags(&cli.global)?;
            propose::run_propose_command(args, &ctx).await
        }
        Commands::Context => {
            let ctx = RunContext::from_flags(&cli.global)?;
            context::run_context_command(&ctx).await
        }
        Commands::Engine { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_engine_command(command, &ctx).await
        }
        Commands::Uninstall(args) => run_uninstall_command(args, &cli.global).await,
        Commands::Shake(args) => run_shake_command(args, &cli.global).await,
        Commands::Release => run_release_command(&cli.global).await,
        Commands::Github { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_github_command(command, &ctx).await
        }
        Commands::Editorial { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_editorial_command(command, &ctx).await
        }
    }
}

pub(crate) fn run_reference_command(flags: &GlobalFlags) -> Result<(), CliError> {
    let output_mode = if flags.json {
        OutputMode::Json
    } else {
        OutputMode::Human
    };
    let reference = build_cli_reference()?;

    match output_mode {
        OutputMode::Human => print_cli_reference_human(&reference).map_err(CliError::internal)?,
        OutputMode::Json => {
            serde_json::to_writer_pretty(io::stdout().lock(), &reference).map_err(CliError::internal)?;
            println!();
        }
    }

    Ok(())
}

pub(crate) fn build_cli_reference() -> Result<CliReferenceDocument, CliError> {
    let command = Cli::command().color(clap::ColorChoice::Never);
    let mut commands = Vec::new();
    collect_cli_reference_sections(command, Vec::new(), &mut commands)?;

    Ok(CliReferenceDocument {
        cli: "boss",
        usage_rules: vec![
            "For agent use, prefer non-interactive commands with --json --no-input.",
            "Treat this reference output as the authoritative current CLI surface for this build.",
            "Do not use boss ... --help for syntax discovery when this reference is available.",
            "Omit --socket-path unless you explicitly need a non-default socket.",
            "Omit --no-autostart unless you explicitly need to suppress worker auto-dispatch on `task create` / `chore create` (also gates the auto-spawned `kind=design` seed task on `project create`). --no-autostart does NOT prevent the CLI from transparently starting the engine — the engine is always needed to track work. To forbid transparent engine startup, use --no-engine-autostart (independent of --no-autostart).",
            "Kind-agnostic verbs (show, update, move, delete, restore, depend, bind-pr, link-external, unlink-external) accept any leaf work item id under either `boss task` or `boss chore` — a chore is a kind of task. Use whichever noun reads more naturally for the call site; the engine resolves the kind from the id.",
            "Kind-specific verbs (create, create-many, list, reorder) stay split by kind because their inputs and filters genuinely differ (e.g. tasks have a project, chores don't; reorder is project-task-only).",
        ],
        selector_semantics: vec![
            "Product selectors accept a product id, slug, or 1-based interactive index. For agent use, prefer slug or id, not numeric indexes.",
            "Project selectors accept a project id, slug, short id (#42 or 42), or 1-based interactive index within the selected product. For agent use, prefer slug, short id, or primary id; avoid numeric indexes.",
            "Task and chore selectors accept: (1) primary id (task_…); (2) friendly short id — `T441` / `t441` / `42` / `#42` within the context product, or `boss/42` / `boss/#42` for a specific product. Projects accept `P7` / `p7` in the same position. For agent use, prefer the short id form (T-prefix or #42) when talking to a human, and the primary id when calling other engine RPCs.",
            "Kind-agnostic verbs (show, update, move, delete, restore, depend, bind-pr, link-external, unlink-external) accept any leaf work item id under either `boss task` or `boss chore` — a chore is a kind of task. Use whichever noun reads more naturally for the call site; the engine resolves the kind from the id.",
            "Kind-specific verbs (create, create-many, list, reorder) stay split by kind because their inputs and filters genuinely differ (e.g. tasks have a project, chores don't; reorder is project-task-only).",
        ],
        status_semantics: vec![
            "Task and chore status uses the board (kanban) names: backlog, doing, review, done, blocked, archived. These are the canonical values shown in --status help and emitted in --json.",
            "The legacy stored names are accepted as aliases on input: todo->backlog, active->doing, in_review (or in-review)->review. They remain how rows are stored, so --json/human output always shows the board name regardless of how a row was set.",
            "boss task|chore update --status and --status list filters accept either vocabulary; boss task|chore move --to backlog|doing|review|done|blocked|archived (legacy names also accepted).",
            "archived is a terminal status for leaf work items (tasks/chores), distinct from delete: `boss task|chore update --status archived` (or `move --to archived`) marks a row as no longer relevant while keeping it queryable — it is NOT soft-deleted (deleted_at stays NULL) and leaves the kanban board the same way an archived project does. It can be reached from any non-terminal status, and moving it back to backlog (`--status backlog` / `--to backlog`) un-archives it. Archived rows are hidden from `boss task|chore list` by default; pass `--include-archived` or filter `--status archived` explicitly to see them.",
            "Product move/delete: --to active|paused|archived. delete is a soft archive (sets status=archived).",
            "Project move/delete: --to planned|active|blocked|done|archived. delete is a soft archive (sets status=archived).",
            "Task/chore delete is a soft delete (sets deleted_at). Recover an accidentally deleted leaf work item with `boss task restore <id>` (alias `undelete`); it clears deleted_at and is idempotent. Find tombstoned rows to restore with `boss task list --deleted` / `boss chore list --deleted`.",
        ],
        workflow_guidance: vec![
            "Use the current UI or conversational context first when deciding where new work belongs.",
            "If you need to compare against existing projects in a product, use boss project list --product <product-selector> --json --no-input.",
            "If the work fits an existing project, create a task in that project.",
            "If it does not fit an existing project and is small and self-contained, create a chore.",
            "If it does not fit an existing project and is broad, ambiguous, investigative, or multi-stage, create a project.",
            "`boss project create` auto-spawns a `kind=design` seed task under the new project (surfaced as `design_task` in the --json response). Do NOT follow up by filing a parallel \"Design\" task; populate the brief by running `boss task update <design_task.id> --description ...` on the seed task. Use `--no-autostart` on `project create` if you want to author the brief before the engine dispatches a worker against the seed task. Use `--no-design-task` for non-design-shaped projects (postmortems, checklists, milestone aggregators) where no seed task is needed; the project is filed with zero child tasks.",
            "Revision tasks (`boss task create-revision`): the engine auto-sequences revisions on the same parent PR — filing a new revision while a prior one is still in flight is SAFE; they run in order with no workspace clobbering. File revisions normally and let them autostart. Do NOT defensively pass `--no-autostart` on `create-revision`, and do NOT wait for the prior revision to land before filing the next, UNLESS the user explicitly asked to queue without dispatching. The pre-filing check (parent PR still open and unmerged) remains required as always.",
        ],
        commands,
    })
}

pub(crate) fn collect_cli_reference_sections(
    command: clap::Command,
    path: Vec<String>,
    sections: &mut Vec<CliReferenceSection>,
) -> Result<(), CliError> {
    let mut current_path = path;
    current_path.push(command.get_name().to_owned());

    sections.push(CliReferenceSection {
        path: current_path.join(" "),
        help: render_command_help(command.clone())?,
    });

    for subcommand in command.get_subcommands() {
        collect_cli_reference_sections(subcommand.clone(), current_path.clone(), sections)?;
    }

    Ok(())
}

pub(crate) fn render_command_help(mut command: clap::Command) -> Result<String, CliError> {
    command = command.color(clap::ColorChoice::Never);
    let mut buffer = Vec::new();
    command.write_long_help(&mut buffer).map_err(CliError::internal)?;
    let help = String::from_utf8(buffer).map_err(CliError::internal)?;
    Ok(help.trim().to_owned())
}

pub(crate) fn print_cli_reference_human(reference: &CliReferenceDocument) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "Boss CLI reference:")?;
    writeln!(stdout)?;
    print_reference_list(&mut stdout, "General rules", &reference.usage_rules)?;
    print_reference_list(&mut stdout, "Selector semantics", &reference.selector_semantics)?;
    print_reference_list(&mut stdout, "Status semantics", &reference.status_semantics)?;
    print_reference_list(&mut stdout, "Workflow guidance", &reference.workflow_guidance)?;
    writeln!(stdout, "Command help:")?;
    for section in &reference.commands {
        writeln!(stdout, "[{}]", section.path)?;
        writeln!(stdout, "{}", section.help)?;
        writeln!(stdout)?;
    }
    Ok(())
}

pub(crate) fn print_reference_list(writer: &mut impl Write, title: &str, items: &[&str]) -> io::Result<()> {
    writeln!(writer, "{title}:")?;
    for item in items {
        writeln!(writer, "- {item}")?;
    }
    writeln!(writer)?;
    Ok(())
}

impl RunContext {
    pub(crate) fn from_flags(flags: &GlobalFlags) -> Result<Self, CliError> {
        let allow_input = !flags.no_input && io::stdin().is_terminal() && io::stdout().is_terminal();
        let discovery = Discovery::from_env(flags.socket_path.as_deref())
            .map_err(CliError::internal)?
            .with_autostart(!flags.no_engine_autostart);

        Ok(Self {
            output_mode: if flags.json {
                OutputMode::Json
            } else {
                OutputMode::Human
            },
            quiet: flags.quiet,
            allow_input,
            discovery,
            no_autostart: flags.no_autostart,
        })
    }
}
