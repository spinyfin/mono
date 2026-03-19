use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use checkleft_package::{find_checkleft_repo_root, package_checkleft_source_archive};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "checkleft-package")]
#[command(about = "Package checkleft into a standalone source archive")]
struct Cli {
    #[arg(long)]
    output: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let current_dir = std::env::current_dir()?;
    let repo_root = find_checkleft_repo_root(&current_dir)?;
    let archive = package_checkleft_source_archive(&repo_root, cli.output)?;
    println!("{}", archive.display());
    Ok(())
}
