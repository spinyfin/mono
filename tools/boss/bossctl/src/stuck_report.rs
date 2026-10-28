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
