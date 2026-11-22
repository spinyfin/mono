//! Inspect or set the driver traffic split through the same engine RPC used by Settings.

use anyhow::{Context, Result, bail};
use boss_protocol::{
    DRIVER_SLUG_AGY, DRIVER_SLUG_CLAUDE, DRIVER_SLUG_CODEX, DRIVER_SLUG_GROK, DriverTrafficSplit, FrontendEvent,
    FrontendRequest,
};

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TrafficSplitArgs {
    pub(crate) set: Option<String>,
    pub(crate) grok: Option<u8>,
    pub(crate) claude: Option<u8>,
    pub(crate) codex: Option<u8>,
    pub(crate) agy: Option<u8>,
}

impl TrafficSplitArgs {
    /// Returns `Some(split)` when any setter flag was supplied, or `None` for
    /// a read-only query. Validates client-side so an invalid total never hits
    /// the engine socket.
    fn into_target_split(self) -> Result<Option<DriverTrafficSplit>> {
        if let Some(spec) = self.set {
            let split = parse_set_spec(&spec)?;
            split.validate()?;
            return Ok(Some(split));
        }
        if self.grok.is_none() && self.claude.is_none() && self.codex.is_none() && self.agy.is_none() {
            return Ok(None);
        }
        let split = DriverTrafficSplit::new(
            self.grok.unwrap_or(0),
            self.claude.unwrap_or(0),
            self.codex.unwrap_or(0),
            self.agy.unwrap_or(0),
        );
        split.validate()?;
        Ok(Some(split))
    }
}

fn parse_set_spec(spec: &str) -> Result<DriverTrafficSplit> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        bail!("--set requires comma-separated driver=percentage pairs (e.g. grok=10,claude=60,codex=30,agy=0)");
    }
    let mut grok: Option<u8> = None;
    let mut claude: Option<u8> = None;
    let mut codex: Option<u8> = None;
    let mut agy: Option<u8> = None;
    for entry in trimmed.split(',') {
        let Some((raw_driver, raw_pct)) = entry.split_once('=') else {
            bail!("invalid driver share '{entry}'; expected driver=percentage");
        };
        let driver = raw_driver.trim();
        let pct: u8 = raw_pct
            .trim()
            .parse()
            .with_context(|| format!("invalid percentage for '{driver}': '{raw_pct}'"))?;
        let slot = match driver {
            DRIVER_SLUG_GROK => &mut grok,
            DRIVER_SLUG_CLAUDE => &mut claude,
            DRIVER_SLUG_CODEX => &mut codex,
            DRIVER_SLUG_AGY => &mut agy,
            _ => bail!("unknown driver '{driver}'; expected one of grok, claude, codex, agy"),
        };
        if slot.is_some() {
            bail!("duplicate driver '{driver}' in --set");
        }
        *slot = Some(pct);
    }
    Ok(DriverTrafficSplit::new(
        grok.unwrap_or(0),
        claude.unwrap_or(0),
        codex.unwrap_or(0),
        agy.unwrap_or(0),
    ))
}

async fn read(socket_path: &Option<String>) -> Result<DriverTrafficSplit> {
    request(socket_path, FrontendRequest::GetDriverTrafficSplit).await
}

async fn request(socket_path: &Option<String>, request: FrontendRequest) -> Result<DriverTrafficSplit> {
    let mut client = super::connect(socket_path).await?;
    match client
        .send_request(&request)
        .await
        .context("sending driver traffic split request")?
    {
        FrontendEvent::DriverTrafficSplitResult { split } => {
            split
                .validate()
                .context("engine returned an invalid driver traffic split")?;
            Ok(split)
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("driver traffic split request failed: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

fn render(split: DriverTrafficSplit, json: bool) -> Result<String> {
    if json {
        let mut result = serde_json::to_value(split)?;
        result["exclusions"] = serde_json::to_value(split.zero_share_exclusions())?;
        Ok(serde_json::to_string(&result)?)
    } else {
        let mut lines = vec![
            "Driver traffic split (0% is off for allocated work; work-kind/model restrictions may narrow further):"
                .to_owned(),
        ];
        for driver in DriverTrafficSplit::DRIVERS_IN_BUCKET_ORDER {
            lines.push(format!("{driver}: {}%", split.share_for(driver).unwrap()));
        }
        lines.extend(split.zero_share_exclusions());
        Ok(lines.join("\n"))
    }
}

pub(crate) async fn run(socket_path: &Option<String>, json: bool, args: TrafficSplitArgs) -> Result<()> {
    let split = match args.into_target_split()? {
        Some(target) => request(socket_path, FrontendRequest::SetDriverTrafficSplit { split: target }).await?,
        None => read(socket_path).await?,
    };
    println!("{}", render(split, json)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn zero_share_exclusions_are_visible() {
        assert!(crate::Cli::try_parse_from(["bossctl", "dispatch", "traffic-split", "--disable", "agy"]).is_err());
        assert!(crate::Cli::try_parse_from(["bossctl", "dispatch", "traffic-split", "--enable", "agy"]).is_err());
        let split = DriverTrafficSplit::new(54, 16, 30, 0);
        let text = render(split, false).unwrap();
        assert!(text.contains("agy: 0%"));
        assert!(text.contains("agy excluded: 0% share"));
        let json: serde_json::Value = serde_json::from_str(&render(split, true).unwrap()).unwrap();
        assert_eq!(json["agy"], 0);
        assert_eq!(json["exclusions"][0], "agy excluded: 0% share");
        assert!(json.get("launch_enabled").is_none());
        assert!(json.get("effective_split").is_none());
    }

    #[test]
    fn command_and_output_include_every_share_including_zero() {
        let cli = crate::Cli::try_parse_from(["bossctl", "--json", "dispatch", "traffic-split"]).unwrap();
        assert!(matches!(
            cli.command,
            crate::Command::Dispatch {
                action: crate::DispatchAction::TrafficSplit {
                    set: None,
                    grok: None,
                    claude: None,
                    codex: None,
                    agy: None,
                }
            }
        ));
        let split = DriverTrafficSplit::new(0, 30, 70, 0);
        let rendered = render(split, false).unwrap();
        for (driver, share) in [("grok", 0), ("claude", 30), ("codex", 70), ("agy", 0)] {
            assert!(rendered.contains(&format!("{driver}: {share}%")));
        }
        assert_eq!(
            serde_json::from_str::<DriverTrafficSplit>(&render(split, true).unwrap()).unwrap(),
            split
        );
    }

    #[test]
    fn parser_accepts_set_and_per_driver_flags_and_rejects_bad_inputs() {
        let cli = crate::Cli::try_parse_from([
            "bossctl",
            "dispatch",
            "traffic-split",
            "--set",
            "grok=10,claude=60,codex=30,agy=0",
        ])
        .unwrap();
        let crate::Command::Dispatch {
            action:
                crate::DispatchAction::TrafficSplit {
                    set,
                    grok,
                    claude,
                    codex,
                    agy,
                },
        } = cli.command
        else {
            panic!("unexpected command");
        };
        assert_eq!(
            TrafficSplitArgs {
                set,
                grok,
                claude,
                codex,
                agy,
            }
            .into_target_split()
            .unwrap(),
            Some(DriverTrafficSplit::new(10, 60, 30, 0))
        );

        let cli = crate::Cli::try_parse_from([
            "bossctl",
            "dispatch",
            "traffic-split",
            "--claude",
            "30",
            "--codex",
            "70",
        ])
        .unwrap();
        let crate::Command::Dispatch {
            action:
                crate::DispatchAction::TrafficSplit {
                    set,
                    grok,
                    claude,
                    codex,
                    agy,
                },
        } = cli.command
        else {
            panic!("unexpected command");
        };
        assert_eq!(
            TrafficSplitArgs {
                set,
                grok,
                claude,
                codex,
                agy,
            }
            .into_target_split()
            .unwrap(),
            Some(DriverTrafficSplit::new(0, 30, 70, 0))
        );

        // Mixing --set and per-driver flags is rejected by clap.
        assert!(
            crate::Cli::try_parse_from([
                "bossctl",
                "dispatch",
                "traffic-split",
                "--set",
                "claude=100",
                "--grok",
                "0",
            ])
            .is_err()
        );

        // Invalid --set specs and non-100 totals are rejected client-side.
        for bad in [
            "",
            "claude",
            "unknown=100",
            "claude=50,claude=50",
            "claude=abc",
            "grok=50,claude=40",
        ] {
            let err = TrafficSplitArgs {
                set: Some(bad.to_owned()),
                ..Default::default()
            }
            .into_target_split()
            .unwrap_err();
            assert!(!err.to_string().is_empty(), "{bad}");
        }
        let err = TrafficSplitArgs {
            grok: Some(50),
            claude: Some(50),
            codex: Some(50),
            agy: Some(0),
            set: None,
        }
        .into_target_split()
        .unwrap_err();
        assert!(err.to_string().contains("got 150"), "{err}");
    }

    #[tokio::test]
    async fn sets_split_over_socket_and_rejects_invalid_total_before_connecting() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("set-split.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let line = BufReader::new(reader).lines().next_line().await.unwrap().unwrap();
            let request: boss_protocol::FrontendRequestEnvelope = serde_json::from_str(&line).unwrap();
            let FrontendRequest::SetDriverTrafficSplit { split } = request.payload else {
                panic!("expected SetDriverTrafficSplit, got {:?}", request.payload);
            };
            assert_eq!(split, DriverTrafficSplit::new(10, 60, 30, 0));
            let event = boss_protocol::FrontendEventEnvelope::response(
                request.request_id,
                FrontendEvent::DriverTrafficSplitResult { split },
            );
            let mut bytes = serde_json::to_vec(&event).unwrap();
            bytes.push(b'\n');
            writer.write_all(&bytes).await.unwrap();
        });

        let sock_str = Some(socket.to_string_lossy().into_owned());
        run(
            &sock_str,
            true,
            TrafficSplitArgs {
                set: Some("grok=10,claude=60,codex=30,agy=0".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        server.await.unwrap();

        // An invalid total must fail client-side without touching any socket.
        let missing_socket = Some(dir.path().join("never-connected.sock").to_string_lossy().into_owned());
        let err = run(
            &missing_socket,
            false,
            TrafficSplitArgs {
                set: Some("grok=10,claude=20,codex=30,agy=0".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("sum to exactly 100"), "{err}");
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
