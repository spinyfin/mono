//! Cross-item execution diagnostics.
use crate::*;
use boss_protocol::{ExecutionQuery, ExecutionQueryDimension, ExecutionQueryResult};

#[derive(Debug, Subcommand)]
pub(crate) enum ExecutionCommand {
    /// List matching executions, or count them by one or more dimensions.
    List(ExecutionListArgs),
}

#[derive(Debug, Args)]
#[command(
    after_help = "Filters combine with AND; repeated or comma-separated values combine with OR.\nUse '(unset)' for missing values. Model, driver and effort are recorded launch values; reasoning is the current task setting.\nExamples:\n  boss execution list --created-via progression_sweep --status queued,ready,waiting_dependency,claimed,running --group-by status,dispatch_wait_reason\n  boss execution list --since 3d --group-by reasoning,model --json"
)]
pub(crate) struct ExecutionListArgs {
    #[arg(long, value_delimiter = ',')]
    created_via: Vec<String>,
    /// Execution kind (for example chore_implementation).
    #[arg(long, value_delimiter = ',')]
    kind: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    status: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    driver: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    model: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    reasoning: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    effort: Vec<String>,
    /// Inclusive creation-time bound: RFC3339 or duration ago (24h, 3d).
    #[arg(long)]
    since: Option<String>,
    /// Product slug or canonical product id; defaults to all products.
    #[arg(long)]
    product: Option<String>,
    /// Case-sensitive literal substring of execution or run error text.
    #[arg(long)]
    error_match: Option<String>,
    /// status, dispatch_wait_reason, progression_disposition, model, driver, reasoning, effort.
    #[arg(long, value_delimiter = ',')]
    group_by: Vec<ExecutionQueryDimension>,
}

impl ExecutionListArgs {
    fn into_query(self) -> Result<ExecutionQuery, CliError> {
        let since = self
            .since
            .as_deref()
            .map(|since| cost_cmds::parse_epoch_bound(since, boss_engine_utils::epoch_time::now_epoch_secs()))
            .transpose()?;
        Ok(ExecutionQuery::builder()
            .created_via(self.created_via)
            .kind(self.kind)
            .status(self.status)
            .driver(self.driver)
            .model(self.model)
            .reasoning(self.reasoning)
            .effort(self.effort)
            .maybe_since_epoch_s(since)
            .maybe_product(self.product)
            .maybe_error_match(self.error_match)
            .group_by(self.group_by)
            .build())
    }
}

pub(crate) async fn run(command: ExecutionCommand, ctx: &RunContext) -> Result<(), CliError> {
    let ExecutionCommand::List(args) = command;
    let query = args.into_query()?;
    let mut client = connect_for_work(ctx).await?;
    let result = rpc_call!(client, FrontendRequest::QueryExecutions { query }, "execution list",
        FrontendEvent::ExecutionQueryResult { result } => result)?;
    print_entity(ctx, &result, || println!("{}", table(&result)))
}

fn table(result: &ExecutionQueryResult) -> Table {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    let value = |v: &Option<String>| v.clone().unwrap_or_else(|| "(unset)".to_owned());
    if result.group_by.is_empty() {
        table.set_header([
            "Execution",
            "Item",
            "Product",
            "Created (epoch)",
            "Created via",
            "Kind",
            "Status",
            "Wait reason",
            "Disposition",
            "Driver",
            "Model",
            "Reasoning",
            "Effort",
            "Last error",
        ]);
        for row in &result.rows {
            table.add_row(vec![
                row.id.clone(),
                value(&row.work_item_short_id),
                value(&row.product_slug),
                row.created_at_epoch_s.to_string(),
                value(&row.created_via),
                row.kind.clone(),
                row.status.clone(),
                value(&row.dispatch_wait_reason),
                value(&row.progression_disposition),
                value(&row.driver),
                value(&row.model),
                value(&row.reasoning),
                value(&row.effort),
                value(&row.last_error),
            ]);
        }
    } else {
        let mut headers = result.group_by.iter().map(|d| d.as_str()).collect::<Vec<_>>();
        headers.push("count");
        table.set_header(headers);
        for group in &result.groups {
            let mut cells = result
                .group_by
                .iter()
                .map(|d| value(&group.keys[d.as_str()]))
                .collect::<Vec<_>>();
            cells.push(group.count.to_string());
            table.add_row(cells);
        }
    }
    table
}
