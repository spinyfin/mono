use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use boss_engine::protocol::{
    FrontendEvent, FrontendEventEnvelope, FrontendRequest, FrontendRequestEnvelope,
};
use boss_engine::work::{
    CreateChoreInput, CreateProductInput, CreateProjectInput, CreateTaskInput, Product, Project,
    Task, WorkItem, WorkItemPatch,
};
use clap::{Args, Parser, Subcommand, ValueEnum};
use comfy_table::{ContentArrangement, Table};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::sleep;

const DEFAULT_SOCKET_PATH: &str = "/tmp/boss-engine.sock";
const DEFAULT_PID_PATH: &str = "/tmp/boss-engine.pid";
const ENGINE_START_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Parser)]
#[command(name = "boss", about = "Boss work CLI")]
struct Cli {
    #[command(flatten)]
    global: GlobalFlags,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Clone, Args)]
struct GlobalFlags {
    #[arg(long, global = true)]
    json: bool,

    #[arg(long, global = true)]
    quiet: bool,

    #[arg(long, global = true)]
    no_input: bool,

    #[arg(long, global = true)]
    no_autostart: bool,

    #[arg(long, global = true)]
    socket_path: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Product {
        #[command(subcommand)]
        command: ProductCommand,
    },
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    Chore {
        #[command(subcommand)]
        command: ChoreCommand,
    },
    Engine {
        #[command(subcommand)]
        command: EngineCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ProductCommand {
    Create(ProductCreateArgs),
    List,
    Show(ProductSelectorArg),
    Update(ProductUpdateArgs),
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    Create(ProjectCreateArgs),
    List(ProjectListArgs),
    Show(ProjectShowArgs),
    Update(ProjectUpdateArgs),
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    Create(TaskCreateArgs),
    List(TaskListArgs),
    Show(TaskIdArg),
    Update(TaskUpdateArgs),
    Move(TaskMoveArgs),
    Delete(TaskDeleteArgs),
    Reorder(TaskReorderArgs),
}

#[derive(Debug, Subcommand)]
enum ChoreCommand {
    Create(ChoreCreateArgs),
    List(ChoreListArgs),
    Show(TaskIdArg),
    Update(TaskUpdateArgs),
    Move(TaskMoveArgs),
    Delete(TaskDeleteArgs),
}

#[derive(Debug, Subcommand)]
enum EngineCommand {
    Status,
    Start,
    Stop,
}

#[derive(Debug, Clone, Args)]
struct ProductSelectorArg {
    selector: String,
}

#[derive(Debug, Clone, Args)]
struct ProjectSelectorArgs {
    #[arg(long)]
    product: Option<String>,

    selector: String,
}

#[derive(Debug, Clone, Args)]
struct ProductScopedArgs {
    #[arg(long)]
    product: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct ProductCreateArgs {
    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long = "repo")]
    #[arg(alias = "repo-remote-url")]
    repo_remote_url: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct ProductUpdateArgs {
    selector: String,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long = "repo")]
    #[arg(alias = "repo-remote-url")]
    repo_remote_url: Option<String>,

    #[arg(long)]
    status: Option<ProductStatus>,
}

#[derive(Debug, Clone, Args)]
struct ProjectCreateArgs {
    #[arg(long)]
    product: Option<String>,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long)]
    goal: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct ProjectListArgs {
    #[arg(long)]
    product: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct ProjectShowArgs {
    #[arg(long)]
    product: Option<String>,

    selector: String,
}

#[derive(Debug, Clone, Args)]
struct ProjectUpdateArgs {
    #[arg(long)]
    product: Option<String>,

    selector: String,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long)]
    goal: Option<String>,

    #[arg(long)]
    status: Option<ProjectStatus>,

    #[arg(long)]
    priority: Option<ProjectPriority>,
}

#[derive(Debug, Clone, Args)]
struct TaskCreateArgs {
    #[arg(long)]
    product: Option<String>,

    #[arg(long)]
    project: Option<String>,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct TaskListArgs {
    #[arg(long)]
    product: Option<String>,

    #[arg(long)]
    project: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct ChoreCreateArgs {
    #[arg(long)]
    product: Option<String>,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct ChoreListArgs {
    #[arg(long)]
    product: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct TaskIdArg {
    id: String,
}

#[derive(Debug, Clone, Args)]
struct TaskUpdateArgs {
    id: String,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long)]
    status: Option<TaskStatus>,

    #[arg(long)]
    ordinal: Option<i64>,

    #[arg(long = "pr-url")]
    pr_url: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct TaskMoveArgs {
    id: String,

    #[arg(long = "to")]
    target: MoveTarget,
}

#[derive(Debug, Clone, Args)]
struct TaskDeleteArgs {
    id: String,
}

#[derive(Debug, Clone, Args)]
struct TaskReorderArgs {
    #[arg(long)]
    product: Option<String>,

    #[arg(long)]
    project: Option<String>,

    #[arg(long, value_delimiter = ',')]
    ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProductStatus {
    Active,
    Paused,
    Archived,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProjectStatus {
    Planned,
    Active,
    Blocked,
    Done,
    Archived,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProjectPriority {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TaskStatus {
    Todo,
    Active,
    Blocked,
    InReview,
    Done,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum MoveTarget {
    Backlog,
    Doing,
    Review,
    Done,
    Todo,
    Active,
    Blocked,
    InReview,
}

impl ProductStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Archived => "archived",
        }
    }
}

impl ProjectStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Archived => "archived",
        }
    }
}

impl ProjectPriority {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl TaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::InReview => "in_review",
            Self::Done => "done",
        }
    }
}

impl MoveTarget {
    fn as_status(self) -> &'static str {
        match self {
            Self::Backlog | Self::Todo => "todo",
            Self::Doing | Self::Active => "active",
            Self::Review | Self::InReview => "in_review",
            Self::Done => "done",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputMode {
    Human,
    Json,
}

#[derive(Debug)]
enum CliError {
    Usage(String),
    NotFound(String),
    Conflict(String),
    EngineUnavailable(String),
    Application(String),
    Internal(anyhow::Error),
}

impl CliError {
    fn internal(err: impl Into<anyhow::Error>) -> Self {
        Self::Internal(err.into())
    }

    fn usage(message: impl Into<String>) -> Self {
        Self::Usage(message.into())
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }

    fn engine_unavailable(message: impl Into<String>) -> Self {
        Self::EngineUnavailable(message.into())
    }

    fn application(message: impl Into<String>) -> Self {
        Self::Application(message.into())
    }

    fn exit_code(&self) -> ExitCode {
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

struct RunContext {
    output_mode: OutputMode,
    quiet: bool,
    allow_input: bool,
    autostart: bool,
    socket_path: String,
    pid_file_path: String,
    engine_program: String,
    engine_args: Vec<String>,
    launch_directory: PathBuf,
}

#[derive(Debug, Clone)]
struct EngineCommandSpec {
    program: String,
    args: Vec<String>,
}

struct BossClient {
    reader: Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    next_request_id: AtomicU64,
}

impl BossClient {
    async fn connect(socket_path: &str) -> Result<Self> {
        let stream = UnixStream::connect(socket_path)
            .await
            .with_context(|| format!("failed to connect to engine socket {socket_path}"))?;
        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(read_half).lines(),
            writer: write_half,
            next_request_id: AtomicU64::new(1),
        })
    }

    async fn send_request(&mut self, request: &FrontendRequest) -> Result<FrontendEvent> {
        let request_id = format!(
            "cli-{}",
            self.next_request_id.fetch_add(1, Ordering::Relaxed)
        );
        let payload = serde_json::to_string(&FrontendRequestEnvelope {
            request_id: request_id.clone(),
            payload: request.clone(),
        })?;
        self.writer.write_all(payload.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;

        while let Some(line) = self.reader.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let envelope: FrontendEventEnvelope = serde_json::from_str(&line)
                .with_context(|| format!("failed to decode engine event: {line}"))?;
            if envelope.request_id.as_deref() == Some(request_id.as_str()) {
                return Ok(envelope.payload);
            }
        }

        bail!("engine closed the socket before returning a response")
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run_cli(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            err.exit_code()
        }
    }
}

async fn run_cli(cli: Cli) -> Result<(), CliError> {
    let ctx = RunContext::from_flags(&cli.global)?;
    match cli.command {
        Commands::Product { command } => run_product_command(command, &ctx).await,
        Commands::Project { command } => run_project_command(command, &ctx).await,
        Commands::Task { command } => run_task_command(command, &ctx).await,
        Commands::Chore { command } => run_chore_command(command, &ctx).await,
        Commands::Engine { command } => run_engine_command(command, &ctx).await,
    }
}

impl RunContext {
    fn from_flags(flags: &GlobalFlags) -> Result<Self, CliError> {
        let allow_input =
            !flags.no_input && io::stdin().is_terminal() && io::stdout().is_terminal();
        let socket_path = flags
            .socket_path
            .clone()
            .or_else(|| std::env::var("BOSS_SOCKET_PATH").ok())
            .unwrap_or_else(|| DEFAULT_SOCKET_PATH.to_owned());
        let pid_file_path =
            std::env::var("BOSS_ENGINE_PID_PATH").unwrap_or_else(|_| DEFAULT_PID_PATH.to_owned());
        let launch_directory = resolve_launch_directory().map_err(CliError::internal)?;
        let engine =
            resolve_engine_command(&launch_directory, &socket_path).map_err(CliError::internal)?;

        Ok(Self {
            output_mode: if flags.json {
                OutputMode::Json
            } else {
                OutputMode::Human
            },
            quiet: flags.quiet,
            allow_input,
            autostart: !flags.no_autostart,
            socket_path: socket_path.clone(),
            pid_file_path,
            engine_program: engine.program,
            engine_args: engine.args,
            launch_directory,
        })
    }
}

async fn run_product_command(command: ProductCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        ProductCommand::Create(args) => {
            let name = required_text(args.name, "Product name", ctx)?;
            let description = optional_text(args.description, "Description", ctx)?;
            let repo_remote_url = optional_text(args.repo_remote_url, "Repo remote URL", ctx)?;

            let product = create_product(
                &mut client,
                CreateProductInput {
                    name,
                    description,
                    repo_remote_url,
                },
            )
            .await?;

            print_entity(ctx, &serde_json::json!({ "product": product }), || {
                print_product_details("Created product", &product);
            })
        }
        ProductCommand::List => {
            let products = list_products(&mut client).await?;
            print_entity(ctx, &serde_json::json!({ "products": products }), || {
                print_products_table(&products);
            })
        }
        ProductCommand::Show(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            print_entity(ctx, &serde_json::json!({ "product": product }), || {
                print_product_details("Product", &product);
            })
        }
        ProductCommand::Update(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                name: args.name,
                description: args.description,
                status: args.status.map(|status| status.as_str().to_owned()),
                repo_remote_url: args.repo_remote_url,
                ..WorkItemPatch::default()
            };
            ensure_patch_present(
                &patch,
                "provide at least one field to update, such as --name or --status",
            )?;
            let item = update_work_item(&mut client, &product.id, patch).await?;
            let product = expect_product(item)?;
            print_entity(ctx, &serde_json::json!({ "product": product }), || {
                print_product_details("Updated product", &product);
            })
        }
    }
}

async fn run_project_command(command: ProjectCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        ProjectCommand::Create(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let name = required_text(args.name, "Project name", ctx)?;
            let description = optional_text(args.description, "Description", ctx)?;
            let goal = optional_text(args.goal, "Goal", ctx)?;

            let project = create_project(
                &mut client,
                CreateProjectInput {
                    product_id: product.id,
                    name,
                    description,
                    goal,
                },
            )
            .await?;

            print_entity(ctx, &serde_json::json!({ "project": project }), || {
                print_project_details("Created project", &project);
            })
        }
        ProjectCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let projects = list_projects(&mut client, &product.id).await?;
            print_entity(
                ctx,
                &serde_json::json!({ "product": product, "projects": projects }),
                || print_projects_table(&projects),
            )
        }
        ProjectCommand::Show(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let project =
                resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            print_entity(ctx, &serde_json::json!({ "project": project }), || {
                print_project_details("Project", &project);
            })
        }
        ProjectCommand::Update(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let project =
                resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                name: args.name,
                description: args.description,
                goal: args.goal,
                status: args.status.map(|status| status.as_str().to_owned()),
                priority: args.priority.map(|priority| priority.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            ensure_patch_present(
                &patch,
                "provide at least one field to update, such as --goal or --priority",
            )?;
            let item = update_work_item(&mut client, &project.id, patch).await?;
            let project = expect_project(item)?;
            print_entity(ctx, &serde_json::json!({ "project": project }), || {
                print_project_details("Updated project", &project);
            })
        }
    }
}

async fn run_task_command(command: TaskCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        TaskCommand::Create(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let project = resolve_project(&mut client, &product.id, args.project, ctx).await?;
            let name = required_text(args.name, "Task name", ctx)?;
            let description = optional_text(args.description, "Description", ctx)?;
            let task = create_task(
                &mut client,
                CreateTaskInput {
                    product_id: product.id,
                    project_id: project.id,
                    name,
                    description,
                },
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "task": task }), || {
                print_task_details("Created task", &task);
            })
        }
        TaskCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let project = match args.project {
                Some(selector) => {
                    Some(resolve_project(&mut client, &product.id, Some(selector), ctx).await?)
                }
                None => None,
            };
            let tasks = list_tasks(
                &mut client,
                &product.id,
                project.as_ref().map(|project| project.id.as_str()),
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "tasks": tasks }), || {
                print_tasks_table(&tasks)
            })
        }
        TaskCommand::Show(args) => {
            let task = expect_task(get_work_item(&mut client, &args.id).await?)?;
            print_entity(ctx, &serde_json::json!({ "task": task }), || {
                print_task_details("Task", &task);
            })
        }
        TaskCommand::Update(args) => {
            let patch = WorkItemPatch {
                name: args.name,
                description: args.description,
                status: args.status.map(|status| status.as_str().to_owned()),
                ordinal: args.ordinal,
                pr_url: args.pr_url,
                ..WorkItemPatch::default()
            };
            ensure_patch_present(
                &patch,
                "provide at least one field to update, such as --status or --pr-url",
            )?;
            let task = expect_task(update_work_item(&mut client, &args.id, patch).await?)?;
            print_entity(ctx, &serde_json::json!({ "task": task }), || {
                print_task_details("Updated task", &task);
            })
        }
        TaskCommand::Move(args) => {
            let patch = WorkItemPatch {
                status: Some(args.target.as_status().to_owned()),
                ..WorkItemPatch::default()
            };
            let task = expect_task(update_work_item(&mut client, &args.id, patch).await?)?;
            print_entity(ctx, &serde_json::json!({ "task": task }), || {
                print_task_details("Moved task", &task);
            })
        }
        TaskCommand::Delete(args) => {
            delete_work_item(&mut client, &args.id).await?;
            print_entity(
                ctx,
                &serde_json::json!({ "id": args.id, "deleted": true }),
                || {
                    if !ctx.quiet {
                        println!("Deleted task {}", args.id);
                    }
                },
            )
        }
        TaskCommand::Reorder(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let project = resolve_project(&mut client, &product.id, args.project, ctx).await?;
            if args.ids.is_empty() {
                return Err(CliError::usage("provide at least one task id via --ids"));
            }
            reorder_project_tasks(&mut client, &project.id, &args.ids).await?;
            print_entity(
                ctx,
                &serde_json::json!({ "project_id": project.id, "task_ids": args.ids }),
                || {
                    if !ctx.quiet {
                        println!(
                            "Reordered {} tasks for project {}",
                            args.ids.len(),
                            project.name
                        );
                    }
                },
            )
        }
    }
}

async fn run_chore_command(command: ChoreCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        ChoreCommand::Create(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let name = required_text(args.name, "Chore name", ctx)?;
            let description = optional_text(args.description, "Description", ctx)?;
            let chore = create_chore(
                &mut client,
                CreateChoreInput {
                    product_id: product.id,
                    name,
                    description,
                },
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "chore": chore }), || {
                print_task_details("Created chore", &chore);
            })
        }
        ChoreCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let chores = list_chores(&mut client, &product.id).await?;
            print_entity(ctx, &serde_json::json!({ "chores": chores }), || {
                print_tasks_table(&chores)
            })
        }
        ChoreCommand::Show(args) => {
            let chore = expect_chore(get_work_item(&mut client, &args.id).await?)?;
            print_entity(ctx, &serde_json::json!({ "chore": chore }), || {
                print_task_details("Chore", &chore);
            })
        }
        ChoreCommand::Update(args) => {
            let patch = WorkItemPatch {
                name: args.name,
                description: args.description,
                status: args.status.map(|status| status.as_str().to_owned()),
                ordinal: args.ordinal,
                pr_url: args.pr_url,
                ..WorkItemPatch::default()
            };
            ensure_patch_present(
                &patch,
                "provide at least one field to update, such as --status or --pr-url",
            )?;
            let chore = expect_chore(update_work_item(&mut client, &args.id, patch).await?)?;
            print_entity(ctx, &serde_json::json!({ "chore": chore }), || {
                print_task_details("Updated chore", &chore);
            })
        }
        ChoreCommand::Move(args) => {
            let patch = WorkItemPatch {
                status: Some(args.target.as_status().to_owned()),
                ..WorkItemPatch::default()
            };
            let chore = expect_chore(update_work_item(&mut client, &args.id, patch).await?)?;
            print_entity(ctx, &serde_json::json!({ "chore": chore }), || {
                print_task_details("Moved chore", &chore);
            })
        }
        ChoreCommand::Delete(args) => {
            delete_work_item(&mut client, &args.id).await?;
            print_entity(
                ctx,
                &serde_json::json!({ "id": args.id, "deleted": true }),
                || {
                    if !ctx.quiet {
                        println!("Deleted chore {}", args.id);
                    }
                },
            )
        }
    }
}

async fn run_engine_command(command: EngineCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        EngineCommand::Status => {
            let running = can_connect_socket(&ctx.socket_path).await;
            let pid = running_engine_pid(&ctx.pid_file_path);
            print_entity(
                ctx,
                &serde_json::json!({
                    "running": running,
                    "pid": pid,
                    "socket_path": ctx.socket_path,
                    "pid_file_path": ctx.pid_file_path,
                }),
                || {
                    if running {
                        println!("Boss engine is running.");
                    } else {
                        println!("Boss engine is stopped.");
                    }
                    println!("Socket: {}", ctx.socket_path);
                    println!("PID file: {}", ctx.pid_file_path);
                    if let Some(pid) = pid {
                        println!("PID: {pid}");
                    }
                },
            )
        }
        EngineCommand::Start => {
            ensure_engine_running(ctx).await?;
            print_entity(
                ctx,
                &serde_json::json!({ "running": true, "socket_path": ctx.socket_path }),
                || {
                    if !ctx.quiet {
                        println!("Boss engine is running.");
                    }
                },
            )
        }
        EngineCommand::Stop => {
            stop_engine(&ctx.pid_file_path)?;
            print_entity(
                ctx,
                &serde_json::json!({ "running": false, "socket_path": ctx.socket_path }),
                || {
                    if !ctx.quiet {
                        println!("Stopped Boss engine.");
                    }
                },
            )
        }
    }
}

async fn connect_for_work(ctx: &RunContext) -> Result<BossClient, CliError> {
    if let Ok(client) = BossClient::connect(&ctx.socket_path).await {
        return Ok(client);
    }

    if !ctx.autostart {
        return Err(CliError::engine_unavailable(format!(
            "boss engine is not reachable at {}",
            ctx.socket_path
        )));
    }

    ensure_engine_running(ctx).await?;
    BossClient::connect(&ctx.socket_path)
        .await
        .map_err(|err| CliError::engine_unavailable(err.to_string()))
}

async fn ensure_engine_running(ctx: &RunContext) -> Result<(), CliError> {
    if can_connect_socket(&ctx.socket_path).await {
        return Ok(());
    }

    if let Some(pid) = running_engine_pid(&ctx.pid_file_path) {
        if wait_for_socket(&ctx.socket_path, ENGINE_START_TIMEOUT).await {
            return Ok(());
        }
        return Err(CliError::engine_unavailable(format!(
            "boss engine pid file points to pid {pid}, but socket {} never became ready",
            ctx.socket_path
        )));
    }

    start_engine_process(ctx)?;
    if wait_for_socket(&ctx.socket_path, ENGINE_START_TIMEOUT).await {
        return Ok(());
    }

    Err(CliError::engine_unavailable(format!(
        "boss engine did not become ready at {} within {} seconds",
        ctx.socket_path,
        ENGINE_START_TIMEOUT.as_secs()
    )))
}

fn start_engine_process(ctx: &RunContext) -> Result<(), CliError> {
    Command::new(&ctx.engine_program)
        .args(&ctx.engine_args)
        .current_dir(&ctx.launch_directory)
        .env("BOSS_ENGINE_PID_PATH", &ctx.pid_file_path)
        .env("BOSS_SOCKET_PATH", &ctx.socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "failed to start engine using `{}`",
                format_engine_command(&ctx.engine_program, &ctx.engine_args)
            )
        })
        .map(|_| ())
        .map_err(CliError::internal)
}

fn stop_engine(pid_file_path: &str) -> Result<(), CliError> {
    let Some(pid) = running_engine_pid(pid_file_path) else {
        return Ok(());
    };

    let status = Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(CliError::internal)?;
    if !status.success() {
        return Err(CliError::engine_unavailable(format!(
            "failed to stop boss engine pid {pid}"
        )));
    }

    if let Some(owner) = read_pid_file(pid_file_path) {
        if owner == pid {
            let _ = std::fs::remove_file(pid_file_path);
        }
    }

    Ok(())
}

async fn can_connect_socket(socket_path: &str) -> bool {
    UnixStream::connect(socket_path).await.is_ok()
}

async fn wait_for_socket(socket_path: &str, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if can_connect_socket(socket_path).await {
            return true;
        }
        sleep(Duration::from_millis(100)).await;
    }
    false
}

fn running_engine_pid(pid_file_path: &str) -> Option<u32> {
    let pid = read_pid_file(pid_file_path)?;
    let status = Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if status.success() {
        Some(pid)
    } else {
        let _ = std::fs::remove_file(pid_file_path);
        None
    }
}

fn read_pid_file(pid_file_path: &str) -> Option<u32> {
    let content = std::fs::read_to_string(pid_file_path).ok()?;
    content.trim().parse().ok()
}

fn resolve_launch_directory() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("BUILD_WORKSPACE_DIRECTORY") {
        let candidate = PathBuf::from(path);
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }
    std::env::current_dir().context("failed to resolve current directory")
}

fn resolve_engine_command(
    _launch_directory: &Path,
    socket_path: &str,
) -> Result<EngineCommandSpec> {
    if let Ok(value) = std::env::var("BOSS_ENGINE_CMD") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            let parts = shlex::split(trimmed)
                .with_context(|| format!("failed to parse BOSS_ENGINE_CMD: {trimmed}"))?;
            let Some((program, args)) = parts.split_first() else {
                bail!("BOSS_ENGINE_CMD resolved to an empty command");
            };
            return Ok(EngineCommandSpec {
                program: program.clone(),
                args: args.to_vec(),
            });
        }
    }

    if let Some(program) = resolve_sibling_engine_binary() {
        return Ok(EngineCommandSpec {
            program,
            args: vec![
                "--mode=server".to_owned(),
                "--socket-path".to_owned(),
                socket_path.to_owned(),
            ],
        });
    }

    Ok(EngineCommandSpec {
        program: "boss-engine".to_owned(),
        args: vec![
            "--mode=server".to_owned(),
            "--socket-path".to_owned(),
            socket_path.to_owned(),
        ],
    })
}

fn resolve_sibling_engine_binary() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let mut candidates = Vec::new();
    if let Some(dir) = exe.parent() {
        candidates.push(dir.join("boss-engine"));
        if let Some(boss_dir) = dir.parent() {
            candidates.push(boss_dir.join("engine").join("engine"));
        }
    }

    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .map(|candidate| candidate.to_string_lossy().into_owned())
}

fn format_engine_command(program: &str, args: &[String]) -> String {
    std::iter::once(program.to_owned())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

async fn list_products(client: &mut BossClient) -> Result<Vec<Product>, CliError> {
    match client
        .send_request(&FrontendRequest::ListProducts)
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::ProductsList { products } => Ok(products),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("products list", &other)),
    }
}

async fn list_projects(
    client: &mut BossClient,
    product_id: &str,
) -> Result<Vec<Project>, CliError> {
    match client
        .send_request(&FrontendRequest::ListProjects {
            product_id: product_id.to_owned(),
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::ProjectsList { projects, .. } => Ok(projects),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("projects list", &other)),
    }
}

async fn list_tasks(
    client: &mut BossClient,
    product_id: &str,
    project_id: Option<&str>,
) -> Result<Vec<Task>, CliError> {
    match client
        .send_request(&FrontendRequest::ListTasks {
            product_id: product_id.to_owned(),
            project_id: project_id.map(str::to_owned),
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::TasksList { tasks, .. } => Ok(tasks),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("tasks list", &other)),
    }
}

async fn list_chores(client: &mut BossClient, product_id: &str) -> Result<Vec<Task>, CliError> {
    match client
        .send_request(&FrontendRequest::ListChores {
            product_id: product_id.to_owned(),
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::ChoresList { chores, .. } => Ok(chores),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("chores list", &other)),
    }
}

async fn create_product(
    client: &mut BossClient,
    input: CreateProductInput,
) -> Result<Product, CliError> {
    match client
        .send_request(&FrontendRequest::CreateProduct { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemCreated { item } => expect_product(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("product create", &other)),
    }
}

async fn create_project(
    client: &mut BossClient,
    input: CreateProjectInput,
) -> Result<Project, CliError> {
    match client
        .send_request(&FrontendRequest::CreateProject { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemCreated { item } => expect_project(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("project create", &other)),
    }
}

async fn create_task(client: &mut BossClient, input: CreateTaskInput) -> Result<Task, CliError> {
    match client
        .send_request(&FrontendRequest::CreateTask { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemCreated { item } => expect_task(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("task create", &other)),
    }
}

async fn create_chore(client: &mut BossClient, input: CreateChoreInput) -> Result<Task, CliError> {
    match client
        .send_request(&FrontendRequest::CreateChore { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemCreated { item } => expect_chore(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("chore create", &other)),
    }
}

async fn get_work_item(client: &mut BossClient, id: &str) -> Result<WorkItem, CliError> {
    match client
        .send_request(&FrontendRequest::GetWorkItem { id: id.to_owned() })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemResult { item } => Ok(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("work item fetch", &other)),
    }
}

async fn update_work_item(
    client: &mut BossClient,
    id: &str,
    patch: WorkItemPatch,
) -> Result<WorkItem, CliError> {
    match client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: id.to_owned(),
            patch,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemUpdated { item } => Ok(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("work item update", &other)),
    }
}

async fn delete_work_item(client: &mut BossClient, id: &str) -> Result<(), CliError> {
    match client
        .send_request(&FrontendRequest::DeleteWorkItem { id: id.to_owned() })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemDeleted { .. } => Ok(()),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("work item delete", &other)),
    }
}

async fn reorder_project_tasks(
    client: &mut BossClient,
    project_id: &str,
    task_ids: &[String],
) -> Result<(), CliError> {
    match client
        .send_request(&FrontendRequest::ReorderProjectTasks {
            project_id: project_id.to_owned(),
            task_ids: task_ids.to_vec(),
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::ProjectTasksReordered { .. } => Ok(()),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("task reorder", &other)),
    }
}

async fn resolve_product(
    client: &mut BossClient,
    selector: Option<String>,
    ctx: &RunContext,
) -> Result<Product, CliError> {
    let products = list_products(client).await?;
    if products.is_empty() {
        return Err(CliError::not_found("no products exist"));
    }

    let selector = match selector {
        Some(selector) => selector,
        None if products.len() == 1 => return Ok(products[0].clone()),
        None if ctx.allow_input => choose_product(&products)?,
        None => {
            return Err(CliError::usage(
                "product is required; pass --product or run interactively",
            ));
        }
    };

    match_products(&products, &selector)
}

async fn resolve_project(
    client: &mut BossClient,
    product_id: &str,
    selector: Option<String>,
    ctx: &RunContext,
) -> Result<Project, CliError> {
    let projects = list_projects(client, product_id).await?;
    if projects.is_empty() {
        return Err(CliError::not_found(
            "no projects exist for the selected product",
        ));
    }

    let selector = match selector {
        Some(selector) => selector,
        None if projects.len() == 1 => return Ok(projects[0].clone()),
        None if ctx.allow_input => choose_project(&projects)?,
        None => {
            return Err(CliError::usage(
                "project is required; pass --project or run interactively",
            ));
        }
    };

    match_projects(&projects, &selector)
}

fn match_products(products: &[Product], selector: &str) -> Result<Product, CliError> {
    if let Some(product) = pick_by_index(products, selector)? {
        return Ok(product);
    }

    let matches = products
        .iter()
        .filter(|product| product.id == selector || product.slug == selector)
        .cloned()
        .collect::<Vec<_>>();
    resolve_single_match(matches, format!("unknown product: {selector}"))
}

fn match_projects(projects: &[Project], selector: &str) -> Result<Project, CliError> {
    if let Some(project) = pick_by_index(projects, selector)? {
        return Ok(project);
    }

    let matches = projects
        .iter()
        .filter(|project| project.id == selector || project.slug == selector)
        .cloned()
        .collect::<Vec<_>>();
    resolve_single_match(matches, format!("unknown project: {selector}"))
}

fn resolve_single_match<T>(matches: Vec<T>, not_found_message: String) -> Result<T, CliError> {
    match matches.len() {
        0 => Err(CliError::not_found(not_found_message)),
        1 => Ok(matches.into_iter().next().expect("len checked")),
        _ => Err(CliError::conflict(
            "selector resolved to multiple work items",
        )),
    }
}

fn pick_by_index<T: Clone>(items: &[T], selector: &str) -> Result<Option<T>, CliError> {
    let Ok(index) = selector.parse::<usize>() else {
        return Ok(None);
    };
    if !(1..=items.len()).contains(&index) {
        return Err(CliError::usage(format!(
            "selection {index} is out of range; choose a value between 1 and {}",
            items.len()
        )));
    }
    Ok(Some(items[index - 1].clone()))
}

fn choose_product(products: &[Product]) -> Result<String, CliError> {
    println!("Select a product:");
    for (index, product) in products.iter().enumerate() {
        println!("  {}. {} ({})", index + 1, product.name, product.slug);
    }
    prompt_index_or_selector("Product", products.len()).map_err(CliError::internal)
}

fn choose_project(projects: &[Project]) -> Result<String, CliError> {
    println!("Select a project:");
    for (index, project) in projects.iter().enumerate() {
        println!("  {}. {} ({})", index + 1, project.name, project.slug);
    }
    prompt_index_or_selector("Project", projects.len()).map_err(CliError::internal)
}

fn required_text(value: Option<String>, label: &str, ctx: &RunContext) -> Result<String, CliError> {
    if let Some(value) = normalize_non_empty(value) {
        return Ok(value);
    }
    if !ctx.allow_input {
        return Err(CliError::usage(format!(
            "{label} is required; pass it explicitly or omit --no-input"
        )));
    }
    loop {
        let input = prompt_text(label, None).map_err(CliError::internal)?;
        if let Some(value) = normalize_non_empty(Some(input)) {
            return Ok(value);
        }
        eprintln!("{label} cannot be empty.");
    }
}

fn optional_text(
    value: Option<String>,
    label: &str,
    ctx: &RunContext,
) -> Result<Option<String>, CliError> {
    if value.is_some() || !ctx.allow_input {
        return Ok(normalize_non_empty(value));
    }
    let input = prompt_text(label, Some("")).map_err(CliError::internal)?;
    Ok(normalize_non_empty(Some(input)))
}

fn prompt_text(label: &str, default: Option<&str>) -> Result<String> {
    let mut stdout = io::stdout();
    match default {
        Some(default) if !default.is_empty() => write!(stdout, "{label} [{default}]: ")?,
        _ => write!(stdout, "{label}: ")?,
    }
    stdout.flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim_end().to_owned();
    if input.is_empty() {
        Ok(default.unwrap_or_default().to_owned())
    } else {
        Ok(input)
    }
}

fn prompt_index_or_selector(label: &str, count: usize) -> Result<String> {
    loop {
        let input = prompt_text(label, None)?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            eprintln!("{label} cannot be empty.");
            continue;
        }
        if let Ok(index) = trimmed.parse::<usize>() {
            if (1..=count).contains(&index) {
                return Ok(index.to_string());
            }
            eprintln!("{label} must be between 1 and {count}.");
            continue;
        }
        return Ok(trimmed.to_owned());
    }
}

fn normalize_non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

fn ensure_patch_present(patch: &WorkItemPatch, message: &str) -> Result<(), CliError> {
    let has_fields = patch.name.is_some()
        || patch.description.is_some()
        || patch.status.is_some()
        || patch.goal.is_some()
        || patch.priority.is_some()
        || patch.repo_remote_url.is_some()
        || patch.pr_url.is_some()
        || patch.ordinal.is_some();

    if has_fields {
        Ok(())
    } else {
        Err(CliError::usage(message))
    }
}

fn expect_product(item: WorkItem) -> Result<Product, CliError> {
    match item {
        WorkItem::Product(product) => Ok(product),
        _ => Err(CliError::conflict("work item is not a product")),
    }
}

fn expect_project(item: WorkItem) -> Result<Project, CliError> {
    match item {
        WorkItem::Project(project) => Ok(project),
        _ => Err(CliError::conflict("work item is not a project")),
    }
}

fn expect_task(item: WorkItem) -> Result<Task, CliError> {
    match item {
        WorkItem::Task(task) => Ok(task),
        WorkItem::Chore(_) => Err(CliError::conflict("work item is a chore, not a task")),
        _ => Err(CliError::conflict("work item is not a task")),
    }
}

fn expect_chore(item: WorkItem) -> Result<Task, CliError> {
    match item {
        WorkItem::Chore(task) => Ok(task),
        WorkItem::Task(_) => Err(CliError::conflict("work item is a task, not a chore")),
        _ => Err(CliError::conflict("work item is not a chore")),
    }
}

fn unexpected_event(context: &str, event: &FrontendEvent) -> CliError {
    CliError::internal(anyhow::anyhow!(
        "unexpected engine event for {context}: {}",
        serde_json::to_string(event).unwrap_or_else(|_| "<unserializable>".to_owned())
    ))
}

fn print_entity<T, F>(ctx: &RunContext, json_value: &T, human: F) -> Result<(), CliError>
where
    T: Serialize,
    F: FnOnce(),
{
    match ctx.output_mode {
        OutputMode::Json => {
            let stdout = io::stdout();
            let mut lock = stdout.lock();
            serde_json::to_writer_pretty(&mut lock, json_value).map_err(CliError::internal)?;
            writeln!(lock).map_err(CliError::internal)?;
        }
        OutputMode::Human => human(),
    }
    Ok(())
}

fn print_products_table(products: &[Product]) {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["ID", "SLUG", "NAME", "STATUS", "REPO"]);
    for product in products {
        table.add_row(vec![
            product.id.as_str(),
            product.slug.as_str(),
            product.name.as_str(),
            product.status.as_str(),
            product.repo_remote_url.as_deref().unwrap_or(""),
        ]);
    }
    println!("{table}");
}

fn print_projects_table(projects: &[Project]) {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["ID", "SLUG", "NAME", "STATUS", "PRIORITY", "GOAL"]);
    for project in projects {
        table.add_row(vec![
            project.id.as_str(),
            project.slug.as_str(),
            project.name.as_str(),
            project.status.as_str(),
            project.priority.as_str(),
            project.goal.as_str(),
        ]);
    }
    println!("{table}");
}

fn print_tasks_table(tasks: &[Task]) {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["ID", "NAME", "STATUS", "PROJECT", "ORDINAL", "PR URL"]);
    for task in tasks {
        let ordinal = task
            .ordinal
            .map(|value| value.to_string())
            .unwrap_or_default();
        table.add_row(vec![
            task.id.as_str(),
            task.name.as_str(),
            task.status.as_str(),
            task.project_id.as_deref().unwrap_or(""),
            ordinal.as_str(),
            task.pr_url.as_deref().unwrap_or(""),
        ]);
    }
    println!("{table}");
}

fn print_product_details(title: &str, product: &Product) {
    println!("{title}");
    println!("ID: {}", product.id);
    println!("Name: {}", product.name);
    println!("Slug: {}", product.slug);
    println!("Status: {}", product.status);
    println!("Repo: {}", product.repo_remote_url.as_deref().unwrap_or(""));
    if !product.description.is_empty() {
        println!("Description: {}", product.description);
    }
}

fn print_project_details(title: &str, project: &Project) {
    println!("{title}");
    println!("ID: {}", project.id);
    println!("Product ID: {}", project.product_id);
    println!("Name: {}", project.name);
    println!("Slug: {}", project.slug);
    println!("Status: {}", project.status);
    println!("Priority: {}", project.priority);
    if !project.goal.is_empty() {
        println!("Goal: {}", project.goal);
    }
    if !project.description.is_empty() {
        println!("Description: {}", project.description);
    }
}

fn print_task_details(title: &str, task: &Task) {
    println!("{title}");
    println!("ID: {}", task.id);
    println!("Product ID: {}", task.product_id);
    if let Some(project_id) = &task.project_id {
        println!("Project ID: {}", project_id);
    }
    println!("Name: {}", task.name);
    println!("Kind: {}", task.kind);
    println!("Status: {}", task.status);
    if let Some(ordinal) = task.ordinal {
        println!("Ordinal: {}", ordinal);
    }
    if let Some(pr_url) = &task.pr_url {
        println!("PR URL: {}", pr_url);
    }
    if !task.description.is_empty() {
        println!("Description: {}", task.description);
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Commands, MoveTarget, ProductCommand, TaskCommand, pick_by_index};

    #[test]
    fn move_target_maps_review_to_in_review() {
        assert_eq!(MoveTarget::Review.as_status(), "in_review");
        assert_eq!(MoveTarget::Doing.as_status(), "active");
        assert_eq!(MoveTarget::Blocked.as_status(), "blocked");
    }

    #[test]
    fn parses_product_create_command() {
        let cli = Cli::parse_from(["boss", "product", "create", "--name", "Boss"]);
        match cli.command {
            Commands::Product {
                command: ProductCommand::Create(args),
            } => {
                assert_eq!(args.name.as_deref(), Some("Boss"));
            }
            _ => panic!("expected product create command"),
        }
    }

    #[test]
    fn parses_task_move_command() {
        let cli = Cli::parse_from(["boss", "task", "move", "task_1", "--to", "review"]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::Move(args),
            } => {
                assert_eq!(args.id, "task_1");
                assert!(matches!(args.target, MoveTarget::Review));
            }
            _ => panic!("expected task move command"),
        }
    }

    #[test]
    fn numeric_selection_is_one_based() {
        let values = vec!["alpha".to_owned(), "beta".to_owned()];
        assert_eq!(
            pick_by_index(&values, "2").unwrap(),
            Some("beta".to_owned())
        );
        assert!(pick_by_index(&values, "0").is_err());
    }
}
