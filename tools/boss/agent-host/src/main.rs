//! The coordinator's tmux pane runs this binary. See the library docs for the
//! contract; this file only parses the command line.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use boss_agent_host::{Config, run};
use clap::Parser;

/// Keep a line-speaking child alive and reachable on a Unix socket.
#[derive(Debug, Parser)]
#[command(name = "boss-agent-host", version)]
struct Args {
    /// Unix socket to listen on.
    #[arg(long)]
    socket: PathBuf,
    /// Journal file to write (mode 0600).
    #[arg(long)]
    journal: PathBuf,
    /// The child to run, after `--`.
    #[arg(last = true, required = true, num_args = 1..)]
    command: Vec<OsString>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let config = Config {
        socket_path: args.socket,
        journal_path: args.journal,
        command: args.command,
    };
    match run(config).await {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(err) => {
            eprintln!("boss-agent-host: {err}");
            ExitCode::from(1)
        }
    }
}
