//! Inspect the split through the same engine RPC used by Settings.

use anyhow::{Context, Result, bail};
use boss_protocol::{DriverTrafficSplit, FrontendEvent, FrontendRequest};

async fn read(socket_path: &Option<String>) -> Result<DriverTrafficSplit> {
    let mut client = super::connect(socket_path).await?;
    match client
        .send_request(&FrontendRequest::GetDriverTrafficSplit)
        .await
        .context("reading driver traffic split")?
    {
        FrontendEvent::DriverTrafficSplitResult { split } => {
            split
                .validate()
                .context("engine returned an invalid driver traffic split")?;
            Ok(split)
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("cannot read driver traffic split: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

fn render(split: DriverTrafficSplit, json: bool) -> Result<String> {
    if json {
        Ok(serde_json::to_string(&split)?)
    } else {
        Ok(format!(
            "Driver traffic split: grok={}%, claude={}%, codex={}%, agy={}%",
            split.grok, split.claude, split.codex, split.agy
        ))
    }
}

pub(crate) async fn run(socket_path: &Option<String>, json: bool) -> Result<()> {
    println!("{}", render(read(socket_path).await?, json)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn command_and_output_include_every_share_including_zero() {
        let cli = crate::Cli::try_parse_from(["bossctl", "--json", "dispatch", "traffic-split"]).unwrap();
        assert!(matches!(
            cli.command,
            crate::Command::Dispatch {
                action: crate::DispatchAction::TrafficSplit
            }
        ));
        let split = DriverTrafficSplit::new(0, 30, 70, 0);
        assert_eq!(
            render(split, false).unwrap(),
            "Driver traffic split: grok=0%, claude=30%, codex=70%, agy=0%"
        );
        assert_eq!(
            serde_json::from_str::<DriverTrafficSplit>(&render(split, true).unwrap()).unwrap(),
            split
        );
    }

    #[tokio::test]
    async fn reads_engine_response_and_surfaces_errors() {
        for response in [
            FrontendEvent::DriverTrafficSplitResult {
                split: DriverTrafficSplit::new(0, 30, 70, 0),
            },
            FrontendEvent::WorkError {
                message: "persisted split is invalid".to_owned(),
            },
        ] {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("split.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let expected = response.clone();
            let server = tokio::spawn(async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (reader, mut writer) = stream.into_split();
                    let Some(line) = BufReader::new(reader).lines().next_line().await.unwrap() else {
                        continue;
                    };
                    let request: boss_protocol::FrontendRequestEnvelope = serde_json::from_str(&line).unwrap();
                    assert!(matches!(request.payload, FrontendRequest::GetDriverTrafficSplit));
                    let event = boss_protocol::FrontendEventEnvelope::response(request.request_id, response);
                    let mut bytes = serde_json::to_vec(&event).unwrap();
                    bytes.push(b'\n');
                    writer.write_all(&bytes).await.unwrap();
                    break;
                }
            });
            let result = read(&Some(socket.to_string_lossy().into_owned())).await;
            server.await.unwrap();
            match expected {
                FrontendEvent::DriverTrafficSplitResult { split } => assert_eq!(result.unwrap(), split),
                FrontendEvent::WorkError { message } => assert!(result.unwrap_err().to_string().contains(&message)),
                _ => unreachable!(),
            }
        }
    }
}
