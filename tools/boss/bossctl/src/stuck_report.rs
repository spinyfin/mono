//! Render the engine's stuck-work report for dispatch diagnostics.
use anyhow::Result;

pub(crate) async fn run(socket_path: &Option<String>, json: bool, since: &str) -> Result<()> {
    let mut client = super::connect(socket_path).await?;
    let report = boss_client::stuck_report::fetch(&mut client, since).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", boss_client::stuck_report::render(&report));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{Cli, Command, DispatchAction};
    use clap::Parser;

    #[test]
    fn dispatch_stuck_accepts_window_and_json() {
        let cli = Cli::try_parse_from(["bossctl", "dispatch", "stuck", "--since", "2d", "--json"]).unwrap();
        assert!(cli.json);
        assert!(matches!(cli.command, Command::Dispatch { action: DispatchAction::Stuck { since } } if since == "2d"));
    }
}
