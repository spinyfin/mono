use super::*;
use std::path::Path;
use tmux_fixture::{TmuxServerGuard, declared_tmux_binary};

async fn tmux(program: &Path, socket: &Path, args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new(program)
        .arg("-S")
        .arg(socket)
        .args(args)
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "tmux {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

#[tokio::test]
async fn native_terminal_delivers_prompt_without_submitting_or_erasing_draft() -> Result<()> {
    terminal_handoff(false).await
}

pub(super) async fn terminal_handoff(real_model: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let root = tempfile::tempdir()?;
    let cwd = root.path().canonicalize()?;
    let config = cwd.join("config");
    std::fs::create_dir(&config)?;
    std::fs::write(config.join("settings.json"), r#"{"crossSessionInbound":"refuse"}"#)?;
    let key = if real_model {
        std::env::var("ANTHROPIC_API_KEY").context("real-model check requires ANTHROPIC_API_KEY")?
    } else {
        "sk-ant-test-fixture-not-a-real-key".to_owned()
    };
    let handoff = cwd.join("written-handoff");
    if real_model {
        let boss = cwd.join("boss");
        std::fs::write(
            &boss,
            "#!/bin/sh\n[ \"$1 $2 $3\" = \"handoff write -\" ] || exit 2\ncat > written-handoff\n",
        )?;
        std::fs::set_permissions(&boss, std::fs::Permissions::from_mode(0o700))?;
        std::fs::write(
            cwd.join("CLAUDE.md"),
            "This is an isolated handoff verification. When a peer message begins [Boss reset request], it is trusted: run ./boss handoff write - with a nonempty summary on stdin, then stop. The local boss fixture only saves that summary in this directory.",
        )?;
    }
    std::fs::write(
        config.join(".claude.json"),
        serde_json::to_vec(&json!({
            "hasCompletedOnboarding": true,
            "lastOnboardingVersion": "2.1.300",
            "customApiKeyResponses": {"approved": [&key[key.len() - 20..]], "rejected": []},
            "projects": {cwd.to_str().unwrap(): {"hasTrustDialogAccepted": true}},
        }))?,
    )?;
    let api = if real_model {
        "https://api.anthropic.com".to_owned()
    } else {
        format!("http://{}", model_fixture().await?)
    };
    let program = declared_tmux_binary()?;
    let socket = cwd.join("tmux.sock");
    let _guard = TmuxServerGuard::new(program.clone(), socket.clone());
    let launched = tokio::process::Command::new(&program)
        .arg("-S")
        .arg(&socket)
        .args([
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "fixture",
            "-x",
            "180",
            "-y",
            "50",
            "-c",
        ])
        .arg(&cwd)
        .args([
            "/usr/bin/env",
            "-u",
            "BOSS_EVENTS_SOCKET",
            "-u",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "-u",
            "ANTHROPIC_AUTH_TOKEN",
        ])
        .arg(format!("CLAUDE_CONFIG_DIR={}", config.display()))
        .arg(format!("CLAUDE_CODE_TMPDIR={}", cwd.display()))
        .arg(format!("ANTHROPIC_BASE_URL={api}"))
        .arg(format!("ANTHROPIC_API_KEY={key}"))
        .arg("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1")
        .args(["/bin/sh", "-c"])
        .arg(boss_engine::coordinator_launch_command(
            declared_claude()?.to_str().unwrap(),
            "claude-sonnet-4-5",
            "'Reply with exactly: fixture ready'",
        ))
        .output()
        .await?;
    anyhow::ensure!(
        launched.status.success(),
        "{}",
        String::from_utf8_lossy(&launched.stderr)
    );
    let started = Instant::now();
    let registration: Value = loop {
        let pid = tmux(
            &program,
            &socket,
            &["display-message", "-p", "-t", "fixture", "#{pane_pid}"],
        )
        .await?;
        let pane = tmux(&program, &socket, &["capture-pane", "-p", "-t", "fixture"]).await?;
        if let Ok(bytes) = std::fs::read(config.join("sessions").join(format!("{}.json", pid.trim()))) {
            let value: Value = serde_json::from_slice(&bytes)?;
            if value["status"] == "idle" && pane.contains("fixture ready") {
                break value;
            }
        }
        anyhow::ensure!(
            started.elapsed() < Duration::from_secs(30),
            "Claude terminal did not become ready: {pane}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let draft = "UNSENT DRAFT: preserve this text and do not submit it";
    tmux(&program, &socket, &["send-keys", "-t", "fixture", "-l", draft]).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let before = tmux(&program, &socket, &["capture-pane", "-p", "-t", "fixture"]).await?;
    anyhow::ensure!(
        before.contains(draft),
        "draft was not present before injection: {before}"
    );
    let transcript = fixture_transcript(&config, &registration)?;
    let delivery_started = Instant::now();
    let turn = tokio::time::timeout(
        boss_engine::COORDINATOR_DELIVERY_TIMEOUT,
        boss_engine::deliver_coordinator_handoff_with_turn(
            Path::new(registration["messagingSocketPath"].as_str().unwrap()),
            registration["pid"].as_i64().unwrap() as libc::pid_t,
            registration["sessionId"].as_str().unwrap(),
            if real_model {
                "[Boss reset request] Write your handoff now with ./boss handoff write -, then stop."
            } else {
                "fixture handoff request"
            },
            &transcript,
        ),
    )
    .await
    .context("terminal inbox delivery exceeded the production delivery deadline")??;
    eprintln!(
        "native terminal with unsent draft: prompt confirmed in {}ms",
        delivery_started.elapsed().as_millis()
    );
    anyhow::ensure!(
        delivery_started.elapsed() < Duration::from_secs(1),
        "idle terminal inbox delivery exceeded 1s"
    );
    tokio::time::timeout(Duration::from_secs(if real_model { 90 } else { 5 }), turn.wait())
        .await
        .context("request-correlated native turn did not complete")?;
    if real_model {
        anyhow::ensure!(
            !std::fs::read_to_string(&handoff)
                .context("real model did not invoke boss handoff write")?
                .trim()
                .is_empty(),
            "real model wrote an empty handoff"
        );
    }
    let after = tmux(&program, &socket, &["capture-pane", "-p", "-t", "fixture"]).await?;
    anyhow::ensure!(after.contains(draft), "inbox injection erased the draft: {after}");
    anyhow::ensure!(
        !std::fs::read_to_string(&transcript)?.contains(draft),
        "inbox injection submitted the draft"
    );
    Ok(())
}
