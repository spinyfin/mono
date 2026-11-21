//! Inspect the split through the same engine RPC used by Settings.

use anyhow::{Context, Result, bail};
use boss_protocol::{DriverTrafficSplit, FrontendEvent, FrontendRequest};

async fn read(socket_path: &Option<String>) -> Result<DriverTrafficSplit> {
    request(socket_path, FrontendRequest::GetDriverTrafficSplit).await
}

async fn request(socket_path: &Option<String>, request: FrontendRequest) -> Result<DriverTrafficSplit> {
    let mut client = super::connect(socket_path).await?;
    match client
        .send_request(&request)
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
    let effective = split.effective_launch_split();
    if json {
        let mut result = serde_json::to_value(split)?;
        result["effective_split"] = serde_json::to_value(effective.as_ref().ok())?;
        result["exclusions"] = serde_json::to_value(split.launch_exclusions())?;
        result["allocation_error"] = serde_json::to_value(effective.err().map(|err| err.to_string()))?;
        Ok(serde_json::to_string(&result)?)
    } else {
        let mut lines = vec!["Driver traffic split (effective shares after launch flags; work-kind/model restrictions may narrow further):".to_owned()];
        for driver in DriverTrafficSplit::DRIVERS_IN_BUCKET_ORDER {
            let enabled = split.launch_enabled.enabled(driver);
            let share = effective
                .as_ref()
                .ok()
                .and_then(|value| value.share_for(driver))
                .unwrap_or(0);
            lines.push(format!(
                "{driver}: configured={}%, effective={share}%, launch={}",
                split.share_for(driver).unwrap(),
                if enabled { "on" } else { "off" }
            ));
        }
        lines.extend(split.launch_exclusions());
        if let Err(err) = effective {
            lines.push(format!("Allocation unavailable: {err}"));
        }
        Ok(lines.join("\n"))
    }
}

pub(crate) async fn run(
    socket_path: &Option<String>,
    json: bool,
    enable: Option<String>,
    disable: Option<String>,
) -> Result<()> {
    let mut split = read(socket_path).await?;
    if let Some((driver, enabled)) = enable
        .map(|driver| (driver, true))
        .or_else(|| disable.map(|driver| (driver, false)))
    {
        split.launch_enabled.set(&driver, enabled).map_err(anyhow::Error::msg)?;
        split = request(socket_path, FrontendRequest::SetDriverTrafficSplit { split }).await?;
    }
    println!("{}", render(split, json)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn launch_commands_and_exclusions_are_visible() {
        for flag in ["--enable", "--disable"] {
            assert!(crate::Cli::try_parse_from(["bossctl", "dispatch", "traffic-split", flag, "agy"]).is_ok());
        }
        assert!(crate::Cli::try_parse_from(["bossctl", "dispatch", "traffic-split", "--disable", "unknown"]).is_err());
        assert!(
            crate::Cli::try_parse_from([
                "bossctl",
                "dispatch",
                "traffic-split",
                "--disable",
                "agy",
                "--enable",
                "agy"
            ])
            .is_err()
        );
        let mut split = DriverTrafficSplit::new(35, 10, 20, 35);
        split.launch_enabled.agy = false;
        let text = render(split, false).unwrap();
        assert!(text.contains("agy: configured=35%, effective=0%, launch=off"));
        assert!(text.contains("agy excluded: launch flag off"));
        let json: serde_json::Value = serde_json::from_str(&render(split, true).unwrap()).unwrap();
        assert_eq!(json["effective_split"]["grok"], 54);
        assert_eq!(json["exclusions"][0], "agy excluded: launch flag off");
        split.claude = 0;
        split.codex = 0;
        split.grok = 0;
        split.agy = 100;
        assert!(render(split, false).unwrap().contains("Allocation unavailable"));
    }

    #[tokio::test]
    async fn set_launch_flag_round_trips_without_changing_shares() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("launch.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let original = DriverTrafficSplit::new(35, 10, 20, 35);
            let mut requests = 0;
            while requests < 2 {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let Some(line) = BufReader::new(reader).lines().next_line().await.unwrap() else {
                    continue;
                };
                let request: boss_protocol::FrontendRequestEnvelope = serde_json::from_str(&line).unwrap();
                let split = match request.payload {
                    FrontendRequest::GetDriverTrafficSplit => original,
                    FrontendRequest::SetDriverTrafficSplit { split } => {
                        let mut expected = original;
                        expected.launch_enabled.agy = false;
                        assert_eq!(split, expected);
                        split
                    }
                    other => panic!("unexpected request: {other:?}"),
                };
                requests += 1;
                let response = boss_protocol::FrontendEventEnvelope::response(
                    request.request_id,
                    FrontendEvent::DriverTrafficSplitResult { split },
                );
                let mut bytes = serde_json::to_vec(&response).unwrap();
                bytes.push(b'\n');
                writer.write_all(&bytes).await.unwrap();
            }
        });
        run(
            &Some(socket.to_string_lossy().into_owned()),
            true,
            None,
            Some("agy".to_owned()),
        )
        .await
        .unwrap();
        server.await.unwrap();
    }

    #[test]
    fn command_and_output_include_every_share_including_zero() {
        let cli = crate::Cli::try_parse_from(["bossctl", "--json", "dispatch", "traffic-split"]).unwrap();
        assert!(matches!(
            cli.command,
            crate::Command::Dispatch {
                action: crate::DispatchAction::TrafficSplit {
                    enable: None,
                    disable: None
                }
            }
        ));
        let split = DriverTrafficSplit::new(0, 30, 70, 0);
        let rendered = render(split, false).unwrap();
        for (driver, share) in [("grok", 0), ("claude", 30), ("codex", 70), ("agy", 0)] {
            assert!(rendered.contains(&format!("{driver}: configured={share}%, effective={share}%, launch=on")));
        }
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
