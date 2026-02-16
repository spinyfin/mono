use std::io::Write;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::acp::{AcpClient, AcpEvent};
use crate::cli::{Cli, Mode};
use crate::config::RuntimeConfig;

pub async fn run(cli: Cli) -> Result<()> {
    let cfg = RuntimeConfig::load_from_env()?;
    cfg.preflight()?;

    match cli.mode {
        Mode::Cli => run_cli(cli, &cfg).await,
        Mode::Server => {
            println!("boss-engine server mode scaffold initialized");
            Ok(())
        }
    }
}

async fn run_cli(cli: Cli, cfg: &RuntimeConfig) -> Result<()> {
    let acp = AcpClient::connect(cfg).await?;
    acp.initialize().await?;
    let session_id = acp.new_session(&cfg.cwd).await?;

    println!("Connected to ACP adapter. Session: {session_id}");

    if let Some(prompt) = cli.prompt {
        run_prompt(&acp, &session_id, &prompt).await?;
        return Ok(());
    }

    println!("Enter a prompt (Ctrl-D to exit):");
    print!("> ");
    std::io::stdout().flush()?;

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let prompt = line.trim();
        if prompt.is_empty() {
            print!("> ");
            std::io::stdout().flush()?;
            continue;
        }

        run_prompt(&acp, &session_id, prompt).await?;
        println!();
        print!("> ");
        std::io::stdout().flush()?;
    }

    Ok(())
}

async fn run_prompt(acp: &AcpClient, session_id: &str, prompt: &str) -> Result<()> {
    let response = acp
        .prompt_streaming(session_id, prompt, |event| match event {
            AcpEvent::AgentMessageChunk { text, .. } => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            AcpEvent::ToolCall { title, status, .. } => {
                eprintln!(
                    "\n[tool] {title} ({})",
                    status.unwrap_or_else(|| "started".to_owned())
                );
            }
            AcpEvent::ToolCallUpdate {
                tool_call_id,
                title,
                status,
                ..
            } => {
                let label =
                    title.unwrap_or_else(|| tool_call_id.unwrap_or_else(|| "tool".to_owned()));
                eprintln!(
                    "\n[tool-update] {label} ({})",
                    status.unwrap_or_else(|| "update".to_owned())
                );
            }
            AcpEvent::PermissionRequest { title, .. } => {
                eprintln!("\n[permission] auto-approving: {title}");
            }
        })
        .await?;

    eprintln!("\n[done] {}", response.stop_reason);
    Ok(())
}
