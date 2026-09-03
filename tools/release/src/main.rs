use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use release::{ProcessCommandRunner, commands};

#[derive(Debug, Parser)]
#[command(name = "release", about = "Prepare, upload, verify, and publish GitHub releases")]
struct Cli {
    #[command(subcommand)]
    command: ReleaseCommand,
}

#[derive(Debug, Subcommand)]
enum ReleaseCommand {
    /// Resolve release state, create a draft release, and record its tag.
    Prepare(ConfigArgs),
    /// Read the tag recorded by prepare for a later Buildkite phase.
    ///
    /// Prints the tag on stdout when prepare recorded one. When prepare
    /// skipped (or never ran), prints nothing on stdout, logs an informational
    /// line on stderr, and exits 0 so a statically declared build step can
    /// no-op cleanly.
    Tag,
    /// Stage assets and checksum sidecars, then upload them to a draft release.
    Upload {
        #[command(flatten)]
        config: ConfigArgs,
        #[arg(long)]
        tag: String,
        /// Release asset in <name>=<path> form. May be passed more than once.
        #[arg(long = "asset", required = true)]
        assets: Vec<String>,
    },
    /// Verify remote assets and checksum sidecars, then publish the draft.
    Publish {
        #[command(flatten)]
        config: ConfigArgs,
        #[arg(long)]
        tag: String,
    },
}

#[derive(Debug, Args)]
struct ConfigArgs {
    /// Release manifest shared by all phases of one product's workflow.
    #[arg(long)]
    config: PathBuf,
}

fn config_path(path: &std::path::Path) -> PathBuf {
    let workspace = env::var_os("BUILD_WORKSPACE_DIRECTORY").map(PathBuf::from);
    commands::resolve_config_path(path, workspace.as_deref())
}

fn run(cli: Cli) -> Result<()> {
    let runner = ProcessCommandRunner;
    match cli.command {
        ReleaseCommand::Prepare(args) => {
            let config = commands::load_config(&config_path(&args.config))?;
            let trigger_source = env::var("BUILDKITE_SOURCE").unwrap_or_default();
            let buildkite_job_id = env::var("BUILDKITE_JOB_ID").ok();
            // Prefer the product-neutral name; keep the checkleft-era spelling
            // as a fallback so existing scheduled-build recovery still works.
            let resume_draft = env::var("RELEASE_RESUME_DRAFT")
                .or_else(|_| env::var("CHECKLEFT_RESUME_DRAFT"))
                .is_ok_and(|value| value == "1");
            match commands::prepare(
                &runner,
                &config,
                &trigger_source,
                buildkite_job_id.as_deref(),
                resume_draft,
            )? {
                commands::PrepareOutcome::Created { tag } | commands::PrepareOutcome::Resumed { tag } => {
                    println!("{tag}")
                }
                commands::PrepareOutcome::Skipped => {}
            }
        }
        ReleaseCommand::Tag => {
            let buildkite_job_id = env::var("BUILDKITE_JOB_ID").ok();
            match commands::read_tag(&runner, buildkite_job_id.as_deref())? {
                Some(tag) => println!("{tag}"),
                None => {
                    eprintln!("no release tag from the prepare phase (it skipped or did not run) — nothing to do");
                }
            }
        }
        ReleaseCommand::Upload {
            config: args,
            tag,
            assets,
        } => {
            let config = commands::load_config(&config_path(&args.config))?;
            let assets = assets
                .iter()
                .map(|asset| commands::Asset::parse(asset))
                .collect::<Result<Vec<_>>>()?;
            commands::upload(&runner, &config, &tag, &assets)?;
        }
        ReleaseCommand::Publish { config: args, tag } => {
            let config = commands::load_config(&config_path(&args.config))?;
            commands::publish(&runner, &config, &tag)?;
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
