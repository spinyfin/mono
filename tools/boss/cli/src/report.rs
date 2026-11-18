use crate::*;

#[derive(Debug, Subcommand)]
pub(crate) enum ReportCommand {
    /// Items without live work, grouped by the reason they are waiting.
    Stuck {
        /// Minimum age in state and question-count window.
        #[arg(long, default_value = "7d")]
        since: String,
    },
}

pub(crate) async fn run(command: ReportCommand, ctx: &RunContext) -> Result<(), CliError> {
    let ReportCommand::Stuck { since } = command;
    boss_client::stuck_report::minimum_age(&since).map_err(|error| CliError::usage(error.to_string()))?;
    let mut client = connect_for_work(ctx).await?;
    let report = boss_client::stuck_report::fetch(&mut client, &since)
        .await
        .map_err(CliError::internal)?;
    print_entity(ctx, &report, || {
        print!("{}", boss_client::stuck_report::render(&report))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stuck_accepts_default_window_and_global_json_after_subcommand() {
        let default = Cli::try_parse_from(["boss", "report", "stuck"]).unwrap();
        assert!(
            matches!(default.command, Commands::Report { command: ReportCommand::Stuck { since } } if since == "7d")
        );
        let json = Cli::try_parse_from(["boss", "report", "stuck", "--since", "2d", "--json"]).unwrap();
        assert!(matches!(json.command, Commands::Report { command: ReportCommand::Stuck { since } } if since == "2d"));
    }
}
