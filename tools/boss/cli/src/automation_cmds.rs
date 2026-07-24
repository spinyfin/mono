//! automation, editorial, and attention command handlers
//!
//! Extracted from the former monolithic `main.rs` (mechanical split; behavior unchanged).

use crate::*;

// ---------------------------------------------------------------------------
// Automation short-id / selector support
// ---------------------------------------------------------------------------

/// Parsed form of an automation selector.
#[derive(Debug)]
pub(crate) enum AutomationSelector {
    /// `auto_…` canonical id — used directly without a product lookup.
    PrimaryId(String),
    /// `A<n>` or `a<n>` (or plain integer) — short id within a product.
    ShortId(i64),
}

pub(crate) fn parse_automation_selector(s: &str) -> Result<AutomationSelector, CliError> {
    let s = s.trim();
    if s.starts_with("auto_") {
        return Ok(AutomationSelector::PrimaryId(s.to_owned()));
    }
    // `A<n>` or `a<n>`
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        if (first == b'A' || first == b'a')
            && let Ok(n) = s[1..].parse::<i64>()
            && n > 0
        {
            return Ok(AutomationSelector::ShortId(n));
        }
    }
    // Plain positive integer → short id
    if let Ok(n) = s.parse::<i64>()
        && n > 0
    {
        return Ok(AutomationSelector::ShortId(n));
    }
    Err(CliError::usage(format!(
        "automation selector must be A<n> (e.g. A1) or an auto_… id; got {s:?}"
    )))
}

/// Resolve an automation selector to a full `Automation` row.
///
/// For `auto_…` ids, the product is not needed. For `A<n>` selectors, a
/// `product` must be provided (resolved by the caller beforehand).
pub(crate) async fn resolve_automation(
    client: &mut BossClient,
    selector: &str,
    product: Option<&Product>,
) -> Result<Automation, CliError> {
    match parse_automation_selector(selector)? {
        AutomationSelector::PrimaryId(id) => get_automation(client, &id).await,
        AutomationSelector::ShortId(n) => {
            let product = product.ok_or_else(|| {
                CliError::usage("A<n> selectors require --product to identify the automation namespace")
            })?;
            let automations = list_automations(client, &product.id).await?;
            automations
                .into_iter()
                .find(|a| a.short_id == Some(n))
                .ok_or_else(|| CliError::not_found(format!("no automation A{n} found in product '{}'", product.slug)))
        }
    }
}

// ---------------------------------------------------------------------------
// Preset → cron compilation
// ---------------------------------------------------------------------------

/// Well-known schedule preset keywords.
///
/// Each preset compiles to a standard 5-field cron expression (min hour dom month dow).
/// The timezone is supplied separately via `--timezone`.
pub(crate) const SCHEDULE_PRESETS: &[(&str, &str, &str)] = &[
    ("weekday-2pm", "0 14 * * 1-5", "Every weekday at 2:00 pm"),
    ("nightly", "0 2 * * *", "Every day at 2:00 am"),
    ("weekly-mon-am", "0 9 * * 1", "Every Monday at 9:00 am"),
    ("hourly", "0 * * * *", "Every hour"),
];

/// Compile a `--schedule` value to a cron expression.
///
/// Accepts either a preset keyword (case-insensitive) or a raw 5-field cron
/// string. Raw strings are validated: they must have exactly 5 whitespace-
/// separated fields and each field must contain only cron-legal characters
/// (`0-9`, `*`, `/`, `-`, `,`, alpha for named months/days).
pub(crate) fn compile_schedule(schedule: &str) -> Result<String, CliError> {
    let trimmed = schedule.trim();

    // Check presets first (case-insensitive).
    if let Some((_, cron, _)) = SCHEDULE_PRESETS
        .iter()
        .find(|(k, _, _)| k.eq_ignore_ascii_case(trimmed))
    {
        return Ok((*cron).to_owned());
    }

    // Treat as a raw cron expression and validate.
    validate_cron_expression(trimmed)
}

/// Validate a raw 5-field cron expression.
///
/// Checks that the string has exactly 5 whitespace-separated fields and each
/// field contains only characters valid in cron: digits, `*`, `/`, `-`, `,`,
/// and ASCII alpha (for named days/months like `MON`, `JAN`). Does not check
/// numeric ranges — the engine (once the cron library is wired up in task 5)
/// will reject semantically invalid values.
pub(crate) fn validate_cron_expression(cron: &str) -> Result<String, CliError> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(CliError::usage(format!(
            "cron expression must have exactly 5 fields (got {}); \
             format: \"min hour dom month dow\" (e.g. \"0 14 * * 1-5\")",
            fields.len()
        )));
    }
    for field in &fields {
        if field
            .chars()
            .any(|c| !c.is_ascii_alphanumeric() && !matches!(c, '*' | '/' | '-' | ','))
        {
            return Err(CliError::usage(format!(
                "cron field {:?} contains invalid characters; \
                 allowed: digits, *, /, -, , and alpha (for named months/days)",
                field
            )));
        }
    }
    Ok(cron.to_owned())
}

// ---------------------------------------------------------------------------
// Automation RPC helpers
// ---------------------------------------------------------------------------

pub(crate) async fn create_automation(
    client: &mut BossClient,
    input: CreateAutomationInput,
) -> Result<Automation, CliError> {
    rpc_call!(
        client,
        FrontendRequest::CreateAutomation { input },
        "automation create",
        FrontendEvent::AutomationCreated { automation } => automation,
    )
}

pub(crate) async fn list_automations(client: &mut BossClient, product_id: &str) -> Result<Vec<Automation>, CliError> {
    rpc_call!(
        client,
        FrontendRequest::ListAutomations {
            product_id: product_id.to_owned(),
        },
        "automation list",
        FrontendEvent::AutomationsList { automations, .. } => automations,
    )
}

pub(crate) async fn get_automation(client: &mut BossClient, id: &str) -> Result<Automation, CliError> {
    rpc_call!(
        client,
        FrontendRequest::GetAutomation { id: id.to_owned() },
        "automation show",
        FrontendEvent::AutomationResult { automation } => automation,
    )
}

pub(crate) async fn update_automation(
    client: &mut BossClient,
    id: &str,
    patch: AutomationPatch,
) -> Result<Automation, CliError> {
    rpc_call!(
        client,
        FrontendRequest::UpdateAutomation {
            id: id.to_owned(),
            patch,
        },
        "automation update",
        FrontendEvent::AutomationUpdated { automation } => automation,
    )
}

pub(crate) async fn enable_automation(client: &mut BossClient, id: &str) -> Result<Automation, CliError> {
    rpc_call!(
        client,
        FrontendRequest::EnableAutomation { id: id.to_owned() },
        "automation enable",
        FrontendEvent::AutomationUpdated { automation } => automation,
    )
}

pub(crate) async fn disable_automation(client: &mut BossClient, id: &str) -> Result<Automation, CliError> {
    rpc_call!(
        client,
        FrontendRequest::DisableAutomation { id: id.to_owned() },
        "automation disable",
        FrontendEvent::AutomationUpdated { automation } => automation,
    )
}

pub(crate) async fn delete_automation(client: &mut BossClient, id: &str) -> Result<(), CliError> {
    rpc_call!(
        client,
        FrontendRequest::DeleteAutomation { id: id.to_owned() },
        "automation delete",
        FrontendEvent::AutomationDeleted { .. } => (),
    )
}

pub(crate) async fn list_automation_runs(
    client: &mut BossClient,
    automation_id: &str,
) -> Result<Vec<AutomationRun>, CliError> {
    rpc_call!(
        client,
        FrontendRequest::ListAutomationRuns {
            automation_id: automation_id.to_owned(),
        },
        "automation runs",
        FrontendEvent::AutomationRunsList { runs, .. } => runs,
    )
}

pub(crate) async fn list_automation_dedup_suppressions(
    client: &mut BossClient,
    automation_id: &str,
) -> Result<Vec<AutomationDedupSuppression>, CliError> {
    rpc_call!(
        client,
        FrontendRequest::ListAutomationDedupSuppressions {
            automation_id: automation_id.to_owned(),
        },
        "automation suppressions",
        FrontendEvent::AutomationDedupSuppressionsList { suppressions, .. } => suppressions,
    )
}

pub(crate) async fn list_automation_tasks(client: &mut BossClient, automation_id: &str) -> Result<Vec<Task>, CliError> {
    rpc_call!(
        client,
        FrontendRequest::ListAutomationTasks {
            automation_id: automation_id.to_owned(),
        },
        "automation tasks",
        FrontendEvent::AutomationTasksList { tasks, .. } => tasks,
    )
}

// ---------------------------------------------------------------------------
// Display helpers for automations
// ---------------------------------------------------------------------------

/// Construct a `comfy_table::Table` with the shared dynamic-arrangement
/// setup every list/detail renderer below repeats: dynamic content
/// arrangement plus the given header row. The per-column headers and
/// per-row cells stay at the call site since they differ per table.
pub(crate) fn new_dynamic_table<H: IntoIterator<Item = impl Into<Cell>>>(header: H) -> Table {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(header);
    table
}

/// Print a rendered table to stdout. Pairs with [`new_dynamic_table`]
/// so the identical `println!("{table}")` teardown lives in one place.
pub(crate) fn print_table(table: Table) {
    println!("{table}");
}

/// Builder for the `FIELD`/`VALUE` detail tables used by the various
/// `print_*_detail` / snapshot printers. Starts a two-column table,
/// accumulates rows via [`row`](DetailTable::row) /
/// [`opt_row`](DetailTable::opt_row) (the latter applying the shared
/// `<unset>` fallback for absent optional values), and renders with
/// [`print`](DetailTable::print).
pub(crate) struct DetailTable {
    pub(crate) table: Table,
}

impl DetailTable {
    pub(crate) fn new() -> Self {
        DetailTable {
            table: new_dynamic_table(vec!["FIELD", "VALUE"]),
        }
    }

    /// Add a row with a value that is always present.
    pub(crate) fn row(mut self, field: &str, value: impl AsRef<str>) -> Self {
        self.table.add_row(vec![field, value.as_ref()]);
        self
    }

    /// Add a row for an optional value, rendering `<unset>` when absent.
    pub(crate) fn opt_row(mut self, field: &str, value: Option<String>) -> Self {
        let rendered = value.unwrap_or_else(|| "<unset>".to_owned());
        self.table.add_row(vec![field, rendered.as_str()]);
        self
    }

    /// Add the six-row lifecycle tail shared by attempt detail views:
    /// the cube/worker identifiers followed by the created/started/finished
    /// timestamps.
    pub(crate) fn lifecycle_rows(
        self,
        cube_lease_id: Option<String>,
        cube_workspace_id: Option<String>,
        worker_id: Option<String>,
        created_at: &str,
        started_at: Option<String>,
        finished_at: Option<String>,
    ) -> Self {
        self.opt_row("cube_lease_id", cube_lease_id)
            .opt_row("cube_workspace_id", cube_workspace_id)
            .opt_row("worker_id", worker_id)
            .row("created_at", created_at)
            .opt_row("started_at", started_at)
            .opt_row("finished_at", finished_at)
    }

    /// Render the accumulated table to stdout.
    pub(crate) fn print(self) {
        print_table(self.table);
    }
}

pub(crate) fn print_automations_table(automations: &[Automation]) {
    let mut table = new_dynamic_table(["#", "NAME", "SCHEDULE", "ENABLED", "OPEN", "LAST OUTCOME", "NEXT DUE"]);
    for a in automations {
        let short = a.short_id.map(|n| format!("A{n}")).unwrap_or_default();
        let schedule = match &a.trigger {
            AutomationTrigger::Schedule { cron, timezone } => {
                format!("{cron} ({timezone})")
            }
        };
        let enabled = if a.enabled { "yes" } else { "no" };
        let last_outcome = a.last_outcome.as_deref().unwrap_or("-");
        let next_due = a.next_due_at.as_deref().unwrap_or("-");
        table.add_row([&short, a.name.as_str(), &schedule, enabled, last_outcome, next_due]);
    }
    print_table(table);
}

pub(crate) fn print_automation_details(label: &str, a: &Automation) {
    println!("{label}:");
    let short = a.short_id.map(|n| format!("A{n}")).unwrap_or_default();
    println!("  ID:          {} ({})", a.id, short);
    println!("  Product:     {}", a.product_id);
    println!("  Name:        {}", a.name);
    let (cron, tz) = match &a.trigger {
        AutomationTrigger::Schedule { cron, timezone } => (cron.as_str(), timezone.as_str()),
    };
    println!("  Cron:        {cron}");
    println!("  Timezone:    {tz}");
    println!("  Instruction: {}", a.standing_instruction);
    println!("  Enabled:     {}", if a.enabled { "yes" } else { "no" });
    println!("  Open limit:  {}", a.open_task_limit);
    if let Some(repo) = &a.repo_remote_url {
        println!("  Repo:        {repo}");
    }
    if let Some(last) = &a.last_fired_at {
        println!("  Last fired:  {last}");
    }
    if let Some(outcome) = &a.last_outcome {
        println!("  Last outcome:{outcome}");
    }
    if let Some(next) = &a.next_due_at {
        println!("  Next due:    {next}");
    }
    println!("  Created:     {}", a.created_at);
    println!("  Updated:     {}", a.updated_at);
}

pub(crate) fn print_automation_runs_table(runs: &[AutomationRun]) {
    let mut table = new_dynamic_table(["SCHEDULED FOR", "OUTCOME", "STARTED", "PRODUCED TASK", "DETAIL"]);
    for r in runs {
        let produced = r.produced_task_id.as_deref().unwrap_or("-");
        let detail = r.detail.as_deref().unwrap_or("-");
        // `repeat_count` collapses consecutive same-outcome rows, and each row
        // is one distinct cron occurrence — not one attempt at the same
        // occurrence. Say "occurrences" so the number isn't read as a retry
        // counter.
        let outcome = if r.repeat_count > 1 {
            format!("{} ({} occurrences)", r.outcome, r.repeat_count)
        } else {
            r.outcome.clone()
        };
        table.add_row([
            r.scheduled_for.as_str(),
            outcome.as_str(),
            r.started_at.as_str(),
            produced,
            detail,
        ]);
    }
    print_table(table);
}

pub(crate) fn print_automation_dedup_suppressions_table(suppressions: &[AutomationDedupSuppression]) {
    let mut table = new_dynamic_table(["ATTEMPTED", "MATCHED ON", "MATCH KEY", "SURVIVING TASK", "CREATED"]);
    for s in suppressions {
        table.add_row([
            s.attempted_name.as_str(),
            s.matched_on.as_str(),
            s.match_key.as_str(),
            s.surviving_task_id.as_str(),
            s.created_at.as_str(),
        ]);
    }
    print_table(table);
}

// ---------------------------------------------------------------------------
// Editorial command handler
// ---------------------------------------------------------------------------

pub(crate) async fn run_editorial_command(command: EditorialCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        EditorialCommand::Show(args) => run_editorial_show(args, ctx).await,
        EditorialCommand::Test(args) => run_editorial_test(args, ctx).await,
    }
}

pub(crate) async fn run_editorial_show(args: EditorialShowArgs, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
    let response = client
        .send_request(&FrontendRequest::ListEditorialActions {
            product_id: product.id.clone(),
            limit: args.limit,
        })
        .await
        .map_err(CliError::internal)?;
    match response {
        FrontendEvent::EditorialActionsList { actions, .. } => {
            let filtered: Vec<&EditorialAction> = if let Some(pr_num) = args.pr {
                let suffix = format!("/{pr_num}");
                actions
                    .iter()
                    .filter(|a| a.pr_url.as_deref().map(|u| u.ends_with(&suffix)).unwrap_or(false))
                    .collect()
            } else {
                actions.iter().collect()
            };
            print_entity(
                ctx,
                &serde_json::json!({ "product_id": product.id, "actions": filtered }),
                || {
                    if filtered.is_empty() {
                        if !ctx.quiet {
                            println!("No editorial actions recorded for product {}.", product.slug);
                        }
                    } else {
                        println!("Editorial actions for product {} ({}):", product.name, product.slug);
                        for action in &filtered {
                            let pr = action.pr_url.as_deref().unwrap_or("(no PR)");
                            let first_reason_line = action.reason.lines().next().unwrap_or("");
                            println!("  [{}] {} — {}", action.action, pr, first_reason_line);
                            println!("    at {}", action.created_at);
                        }
                    }
                },
            )
        }
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("editorial show", &other)),
    }
}

pub(crate) async fn run_editorial_test(args: EditorialTestArgs, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
    let body = std::fs::read_to_string(&args.body_file)
        .map_err(|e| CliError::usage(format!("could not read {}: {e}", args.body_file.display())))?;
    let rules = product.editorial_rules.clone().unwrap_or_default();
    let compiled = boss_editorial::CompiledRules::compile(rules)
        .map_err(|e| CliError::application(format!("invalid redaction regex in editorial_rules: {e}")))?;
    let decision = boss_editorial::evaluate(&body, &args.title, &compiled, None);
    let (decision_str, findings): (&str, Vec<String>) = match &decision {
        boss_editorial::EditorialDecision::Allow => ("allow", vec![]),
        boss_editorial::EditorialDecision::Rewrite { findings, .. } => {
            ("rewrite", findings.iter().map(|f| f.description.clone()).collect())
        }
        boss_editorial::EditorialDecision::Block { findings } => {
            ("deny", findings.iter().map(|f| f.description.clone()).collect())
        }
    };
    let rewritten_body: Option<&str> = match &decision {
        boss_editorial::EditorialDecision::Rewrite { body, .. } => Some(body.as_str()),
        _ => None,
    };
    print_entity(
        ctx,
        &serde_json::json!({
            "product_id": product.id,
            "decision": decision_str,
            "findings": findings,
        }),
        || {
            println!("Decision: {decision_str}");
            if findings.is_empty() {
                println!("No findings.");
            } else {
                println!("Findings:");
                for f in &findings {
                    println!("  - {f}");
                }
            }
            if let Some(new_body) = rewritten_body {
                println!("\nRewritten body:");
                println!("{new_body}");
            }
        },
    )
}

// ---------------------------------------------------------------------------
// Automation command handler
// ---------------------------------------------------------------------------

pub(crate) async fn run_automation_command(command: AutomationCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        AutomationCommand::Create(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let name = required_text(args.name, "Automation name", ctx)?;
            let instruction = required_text(args.instruction, "Standing instruction", ctx)?;
            let schedule_raw = required_text(args.schedule, "Schedule", ctx)?;
            let cron = compile_schedule(&schedule_raw)?;
            let trigger = AutomationTrigger::Schedule {
                cron,
                timezone: args.timezone,
            };
            let automation = create_automation(
                &mut client,
                CreateAutomationInput::builder()
                    .product_id(product.id)
                    .name(name)
                    .trigger(trigger)
                    .standing_instruction(instruction)
                    .open_task_limit(args.open_task_limit)
                    .enabled(!args.disabled)
                    .maybe_repo_remote_url(args.repo)
                    .created_via(boss_protocol::CREATED_VIA_CLI)
                    .build(),
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "automation": automation }), || {
                print_automation_details("Created automation", &automation);
            })
        }

        AutomationCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let automations = list_automations(&mut client, &product.id).await?;
            print_entity(ctx, &serde_json::json!({ "automations": automations }), || {
                if automations.is_empty() {
                    println!("No automations for product '{}'.", product.slug);
                } else {
                    print_automations_table(&automations);
                }
            })
        }

        AutomationCommand::Show(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let automation = resolve_automation(&mut client, &args.selector, product.as_ref()).await?;
            print_entity(ctx, &serde_json::json!({ "automation": automation }), || {
                print_automation_details("Automation", &automation);
            })
        }

        AutomationCommand::Update(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let automation = resolve_automation(&mut client, &args.selector, product.as_ref()).await?;

            // Build a trigger patch only when schedule or timezone changed.
            let trigger_patch = match (&args.schedule, &args.timezone) {
                (None, None) => None,
                _ => {
                    // Start from the existing trigger so partial updates work.
                    let AutomationTrigger::Schedule {
                        cron: existing_cron,
                        timezone: existing_tz,
                    } = &automation.trigger;
                    let cron = if let Some(sched) = &args.schedule {
                        compile_schedule(sched)?
                    } else {
                        existing_cron.clone()
                    };
                    let timezone = args.timezone.clone().unwrap_or_else(|| existing_tz.clone());
                    Some(AutomationTrigger::Schedule { cron, timezone })
                }
            };

            let patch = AutomationPatch {
                name: args.name,
                repo_remote_url: args.repo,
                trigger: trigger_patch,
                standing_instruction: args.instruction,
                open_task_limit: args.open_task_limit,
                catch_up_window_secs: None,
                enabled: None,
            };
            let updated = update_automation(&mut client, &automation.id, patch).await?;
            print_entity(ctx, &serde_json::json!({ "automation": updated }), || {
                print_automation_details("Updated automation", &updated);
            })
        }

        AutomationCommand::Enable(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let automation = resolve_automation(&mut client, &args.selector, product.as_ref()).await?;
            let updated = enable_automation(&mut client, &automation.id).await?;
            print_entity(ctx, &serde_json::json!({ "automation": updated }), || {
                if !ctx.quiet {
                    println!("Enabled automation {}", automation.id);
                }
            })
        }

        AutomationCommand::Disable(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let automation = resolve_automation(&mut client, &args.selector, product.as_ref()).await?;
            let updated = disable_automation(&mut client, &automation.id).await?;
            print_entity(ctx, &serde_json::json!({ "automation": updated }), || {
                if !ctx.quiet {
                    println!("Disabled automation {}", automation.id);
                }
            })
        }

        AutomationCommand::Delete(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let automation = resolve_automation(&mut client, &args.selector, product.as_ref()).await?;
            delete_automation(&mut client, &automation.id).await?;
            print_entity(
                ctx,
                &serde_json::json!({ "deleted_automation_id": automation.id }),
                || {
                    if !ctx.quiet {
                        println!("Deleted automation {}", automation.id);
                    }
                },
            )
        }

        AutomationCommand::Run(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let automation = resolve_automation(&mut client, &args.selector, product.as_ref()).await?;
            match client
                .send_request(&FrontendRequest::RunAutomation {
                    automation_id: automation.id.clone(),
                    force: args.force,
                })
                .await
                .map_err(CliError::internal)?
            {
                FrontendEvent::AutomationRunEnqueued { .. } => print_entity(
                    ctx,
                    &serde_json::json!({ "automation_id": automation.id, "enqueued": true }),
                    || {
                        if !ctx.quiet {
                            println!("Triage enqueued for automation {}", automation.id);
                        }
                    },
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("automation run", &other)),
            }
        }

        AutomationCommand::Runs(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let automation = resolve_automation(&mut client, &args.selector, product.as_ref()).await?;
            let runs = list_automation_runs(&mut client, &automation.id).await?;
            print_entity(ctx, &serde_json::json!({ "runs": runs }), || {
                if runs.is_empty() {
                    println!("No runs recorded for automation {}.", automation.id);
                } else {
                    print_automation_runs_table(&runs);
                }
            })
        }

        AutomationCommand::Tasks(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let automation = resolve_automation(&mut client, &args.selector, product.as_ref()).await?;
            let tasks = list_automation_tasks(&mut client, &automation.id).await?;
            let tasks: Vec<Task> = tasks.into_iter().map(with_display_status).collect();
            print_entity(ctx, &serde_json::json!({ "tasks": tasks }), || {
                if tasks.is_empty() {
                    println!("No tasks produced by automation {}.", automation.id);
                } else {
                    print_tasks_table(&tasks, false);
                }
            })
        }

        AutomationCommand::Suppressions(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let automation = resolve_automation(&mut client, &args.selector, product.as_ref()).await?;
            let suppressions = list_automation_dedup_suppressions(&mut client, &automation.id).await?;
            print_entity(ctx, &serde_json::json!({ "suppressions": suppressions }), || {
                if suppressions.is_empty() {
                    println!("No suppressions recorded for automation {}.", automation.id);
                } else {
                    print_automation_dedup_suppressions_table(&suppressions);
                }
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Attention group selector parsing
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum AttentionGroupSelector {
    /// `atg_…` primary id.
    PrimaryId(String),
    /// `A<n>` per-product short id (requires product context at resolution time).
    ShortId(i64),
}

pub(crate) fn parse_attention_group_selector(s: &str) -> Result<AttentionGroupSelector, CliError> {
    let s = s.trim();
    if s.starts_with("atg_") {
        return Ok(AttentionGroupSelector::PrimaryId(s.to_owned()));
    }
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        if (first == b'A' || first == b'a')
            && let Ok(n) = s[1..].parse::<i64>()
            && n > 0
        {
            return Ok(AttentionGroupSelector::ShortId(n));
        }
    }
    if let Ok(n) = s.parse::<i64>()
        && n > 0
    {
        return Ok(AttentionGroupSelector::ShortId(n));
    }
    Err(CliError::usage(format!(
        "attention group selector must be A<n> (e.g. A1) or an atg_… id; got {s:?}"
    )))
}

/// Resolve an attention group selector to a full `AttentionGroup` row.
///
/// For `atg_…` ids the product is not needed. For `A<n>` selectors, a
/// `product` must be provided (resolved by the caller beforehand).
///
/// Note: `A<n>` resolution lists only open/partially-answered groups. Use
/// the `atg_…` primary id to reference actioned or dismissed groups.
pub(crate) async fn resolve_attention_group(
    client: &mut BossClient,
    selector: &str,
    product: Option<&Product>,
) -> Result<AttentionGroup, CliError> {
    match parse_attention_group_selector(selector)? {
        AttentionGroupSelector::PrimaryId(id) => get_attention_group(client, &id).await,
        AttentionGroupSelector::ShortId(n) => {
            let product = product.ok_or_else(|| {
                CliError::usage("A<n> selectors require --product to identify the attention group namespace")
            })?;
            let groups = list_attention_groups(client, &product.id, None, None, None, None).await?;
            groups.into_iter().find(|g| g.short_id == Some(n)).ok_or_else(|| {
                CliError::not_found(format!(
                    "no active attention group A{n} found in product '{}' \
                         (use the atg_… id to reference actioned or dismissed groups)",
                    product.slug
                ))
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Attention RPC helpers
// ---------------------------------------------------------------------------

pub(crate) async fn list_attention_groups(
    client: &mut BossClient,
    product_id: &str,
    project_id: Option<String>,
    task_id: Option<String>,
    kind: Option<String>,
    state: Option<String>,
) -> Result<Vec<AttentionGroup>, CliError> {
    rpc_call!(
        client,
        FrontendRequest::ListAttentionGroups {
            product_id: product_id.to_owned(),
            project_id,
            task_id,
            kind,
            state,
        },
        "attention list",
        FrontendEvent::AttentionGroupsList { groups, .. } => groups,
    )
}

pub(crate) async fn get_attention_group(client: &mut BossClient, id: &str) -> Result<AttentionGroup, CliError> {
    rpc_call!(
        client,
        FrontendRequest::GetAttentionGroup { id: id.to_owned() },
        "attention show",
        FrontendEvent::AttentionGroupResult { group, .. } => group,
    )
}

pub(crate) async fn create_attention_rpc(
    client: &mut BossClient,
    input: CreateAttentionInput,
) -> Result<(Attention, AttentionGroup), CliError> {
    rpc_call!(
        client,
        FrontendRequest::CreateAttention { input },
        "attention create",
        FrontendEvent::AttentionCreated { attention, group } => (attention, group),
    )
}

pub(crate) async fn answer_attention_rpc(
    client: &mut BossClient,
    id: &str,
    answer: Option<String>,
    skip: bool,
    dismiss: bool,
) -> Result<AttentionGroup, CliError> {
    rpc_call!(
        client,
        FrontendRequest::AnswerAttention {
            id: id.to_owned(),
            answer,
            skip,
            dismiss,
        },
        "attention answer",
        FrontendEvent::AttentionGroupUpdated { group, .. } => group,
    )
}

pub(crate) async fn action_attention_group_rpc(
    client: &mut BossClient,
    id: &str,
    skip_unanswered: bool,
) -> Result<AttentionGroup, CliError> {
    rpc_call!(
        client,
        FrontendRequest::ActionAttentionGroup {
            id: id.to_owned(),
            skip_unanswered,
        },
        "attention action",
        FrontendEvent::AttentionGroupActioned { group, .. } => group,
    )
}

pub(crate) async fn dismiss_attention_rpc(
    client: &mut BossClient,
    id: &str,
    reason: Option<String>,
) -> Result<AttentionGroup, CliError> {
    rpc_call!(
        client,
        FrontendRequest::DismissAttention {
            id: id.to_owned(),
            reason,
        },
        "attention dismiss",
        FrontendEvent::AttentionGroupUpdated { group, .. } => group,
    )
}

// ---------------------------------------------------------------------------
// Attention display helpers
// ---------------------------------------------------------------------------

pub(crate) fn print_attention_groups_table(groups: &[AttentionGroup]) {
    let mut table = new_dynamic_table(["ID", "SHORT", "KIND", "STATE", "ASSOCIATION", "CREATED"]);
    for g in groups {
        let short = g.short_id.map(|n| format!("A{n}")).unwrap_or_default();
        let assoc = g
            .association_project_id
            .as_deref()
            .or(g.association_task_id.as_deref())
            .unwrap_or("-");
        table.add_row([
            g.id.as_str(),
            short.as_str(),
            g.kind.as_str(),
            g.state.as_str(),
            assoc,
            g.created_at.as_str(),
        ]);
    }
    print_table(table);
}

pub(crate) fn print_attention_group_details(label: &str, g: &AttentionGroup) {
    println!("{label}: {}", g.id);
    if let Some(n) = g.short_id {
        println!("  Short ID  : A{n}");
    }
    println!("  Kind      : {}", g.kind);
    println!("  State     : {}", g.state);
    println!("  Source    : {}", g.source_kind);
    if let Some(ref id) = g.association_project_id {
        println!("  Project   : {id}");
    }
    if let Some(ref id) = g.association_task_id {
        println!("  Task      : {id}");
    }
    if let Some(ref path) = g.source_doc_path {
        println!("  Doc path  : {path}");
    }
    if let Some(ref task_id) = g.source_task_id {
        println!("  Source task: {task_id}");
    }
    if let Some(ref kind) = g.produced_artifact_kind {
        println!("  Artifact  : {kind}");
        if let Some(ref r) = g.produced_artifact_ref {
            println!("  Ref       : {r}");
        }
    }
    println!("  Created   : {}", g.created_at);
    if let Some(ref t) = g.actioned_at {
        println!("  Actioned  : {t}");
    }
    if let Some(ref t) = g.dismissed_at {
        println!("  Dismissed : {t}");
    }
}

// ---------------------------------------------------------------------------
// Attention command handler
// ---------------------------------------------------------------------------

pub(crate) async fn run_attention_command(command: AttentionCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        AttentionCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let project_id = if let Some(sel) = args.project {
                Some(resolve_selector_to_primary_id(&mut client, ctx, &sel, None).await?)
            } else {
                None
            };
            let task_id = if let Some(sel) = args.task {
                Some(resolve_selector_to_primary_id(&mut client, ctx, &sel, None).await?)
            } else {
                None
            };
            let groups =
                list_attention_groups(&mut client, &product.id, project_id, task_id, args.kind, args.state).await?;
            print_entity(ctx, &serde_json::json!({ "attention_groups": groups }), || {
                if groups.is_empty() {
                    println!("No attention groups found for product '{}'.", product.slug);
                } else {
                    print_attention_groups_table(&groups);
                }
            })
        }

        AttentionCommand::Show(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let group = resolve_attention_group(&mut client, &args.selector, product.as_ref()).await?;
            print_entity(ctx, &serde_json::json!({ "attention_group": group }), || {
                print_attention_group_details("Attention group", &group);
            })
        }

        AttentionCommand::Create(args) => {
            if args.project.is_none() && args.task.is_none() {
                return Err(CliError::usage("exactly one of --project or --task is required"));
            }
            if args.project.is_some() && args.task.is_some() {
                return Err(CliError::usage("--project and --task are mutually exclusive"));
            }
            let association_project_id = if let Some(sel) = args.project {
                Some(resolve_selector_to_primary_id(&mut client, ctx, &sel, None).await?)
            } else {
                None
            };
            let association_task_id = if let Some(sel) = args.task {
                Some(resolve_selector_to_primary_id(&mut client, ctx, &sel, None).await?)
            } else {
                None
            };
            let choice_options = if args.choices.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&args.choices).map_err(CliError::internal)?)
            };
            let input = CreateAttentionInput::builder()
                .kind(args.kind)
                .maybe_group_id(args.group)
                .maybe_group_key(args.group_key)
                .maybe_association_project_id(association_project_id)
                .maybe_association_task_id(association_task_id)
                .maybe_source_kind(Some("manual".to_owned()))
                .maybe_question_type(args.question_type)
                .maybe_prompt_text(args.prompt)
                .maybe_choice_options(choice_options)
                .maybe_proposed_name(args.name)
                .maybe_proposed_description(args.description)
                .maybe_proposed_effort(args.effort)
                .maybe_proposed_work_kind(args.work_kind)
                .maybe_rationale(args.rationale)
                .build();
            let (attention, group) = create_attention_rpc(&mut client, input).await?;
            print_entity(
                ctx,
                &serde_json::json!({ "attention": attention, "attention_group": group }),
                || {
                    if !ctx.quiet {
                        let short = group
                            .short_id
                            .map(|n| format!("A{n}"))
                            .unwrap_or_else(|| group.id.clone());
                        println!(
                            "Created attention {} in group {short} (state: {})",
                            attention.id, group.state
                        );
                    }
                },
            )
        }

        AttentionCommand::Answer(args) => {
            let flag_count = [
                args.yes,
                args.no,
                args.skip,
                args.choice.is_some(),
                args.answer.is_some(),
            ]
            .iter()
            .filter(|&&b| b)
            .count();
            if flag_count > 1 {
                return Err(CliError::usage(
                    "--yes, --no, --choice, --answer, and --skip are mutually exclusive",
                ));
            }
            if flag_count == 0 {
                return Err(CliError::usage(
                    "one of --yes, --no, --choice <v>, --answer <text>, or --skip is required",
                ));
            }
            let (answer, skip, dismiss) = if args.yes {
                (Some("yes".to_owned()), false, false)
            } else if args.no {
                (Some("no".to_owned()), false, false)
            } else if let Some(choice) = args.choice {
                (Some(choice), false, false)
            } else if let Some(ans) = args.answer {
                (Some(ans), false, false)
            } else {
                (None, true, false)
            };
            let group = answer_attention_rpc(&mut client, &args.id, answer, skip, dismiss).await?;
            print_entity(ctx, &serde_json::json!({ "attention_group": group }), || {
                if !ctx.quiet {
                    println!("Recorded answer for {} (group state: {})", args.id, group.state);
                }
            })
        }

        AttentionCommand::Dismiss(args) => {
            // The engine discriminates atg_… (group) vs atn_… (member) by prefix.
            // A<n> selectors refer to groups and need product resolution.
            let resolved_id = if args.id.starts_with("atg_") || args.id.starts_with("atn_") {
                args.id.clone()
            } else {
                let product = resolve_optional_product(&mut client, args.product.clone(), ctx).await?;
                let group = resolve_attention_group(&mut client, &args.id, product.as_ref()).await?;
                group.id
            };
            let group = dismiss_attention_rpc(&mut client, &resolved_id, args.reason).await?;
            print_entity(ctx, &serde_json::json!({ "attention_group": group }), || {
                if !ctx.quiet {
                    println!("Dismissed {} (group state: {})", resolved_id, group.state);
                }
            })
        }

        AttentionCommand::Action(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let group = resolve_attention_group(&mut client, &args.selector, product.as_ref()).await?;
            if !args.confirm {
                if !ctx.allow_input {
                    return Err(CliError::usage(
                        "pass --confirm to action the group non-interactively (or --no-input is set)",
                    ));
                }
                // Interactive confirmation.
                let short = group
                    .short_id
                    .map(|n| format!("A{n}"))
                    .unwrap_or_else(|| group.id.clone());
                print!(
                    "Action group {short} ({kind}, {state})? [y/N]: ",
                    kind = group.kind,
                    state = group.state
                );
                io::stdout().flush().map_err(CliError::internal)?;
                let mut line = String::new();
                io::stdin().read_line(&mut line).map_err(CliError::internal)?;
                if !matches!(line.trim(), "y" | "Y" | "yes" | "Yes") {
                    if !ctx.quiet {
                        println!("Aborted.");
                    }
                    return Ok(());
                }
            }
            let actioned = action_attention_group_rpc(&mut client, &group.id, args.skip_unanswered).await?;
            let produced_kind = actioned.produced_artifact_kind.clone();
            let produced_ref = actioned.produced_artifact_ref.clone();
            print_entity(
                ctx,
                &serde_json::json!({
                    "attention_group": actioned,
                    "produced": {
                        "kind": produced_kind,
                        "ref": produced_ref,
                    }
                }),
                || {
                    if !ctx.quiet {
                        let artifact = produced_kind.as_deref().unwrap_or("none");
                        let artifact_ref = produced_ref.as_deref().unwrap_or("");
                        println!("Actioned group {} → produced {artifact} {artifact_ref}", actioned.id);
                    }
                },
            )
        }
    }
}
