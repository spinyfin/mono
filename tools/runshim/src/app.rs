use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::Parser;
use thiserror::Error;

use crate::bazel::RealBazel;
use crate::cli::{Cli, Command as CliCommand};
use crate::config::{CONFIG_FILE_NAME, load_repo_config};
use crate::dispatch::{DispatchPlan, prepare_dispatch};
use crate::install::{current_home_dir, install, resolve_bin_dir};

const RUNSHIM_BINARY_NAME: &str = "runshim";

#[derive(Debug, Error)]
pub enum RunshimError {
    #[error("no {CONFIG_FILE_NAME} found from `{}` upward", start_dir.display())]
    ConfigNotFound { start_dir: PathBuf },
    #[error("failed to read config `{}`", path.display())]
    ReadConfig {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config `{}`", path.display())]
    ParseConfig {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("unsupported {CONFIG_FILE_NAME} version `{version}`")]
    UnsupportedConfigVersion { version: u32 },
    #[error("{0}")]
    InvalidConfig(String),
    #[error("tool `{tool}` is not configured in `{}`", config_path.display())]
    ToolNotConfigured { tool: String, config_path: PathBuf },
    #[error("HOME is not set and no --bin-dir override was provided")]
    MissingHomeDirectory,
    #[error("failed to create bin directory `{}`", path.display())]
    CreateBinDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read runshim binary `{}`", path.display())]
    ReadInstalledBinary {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to copy runshim binary from `{}` to `{}`", from.display(), to.display())]
    CopyInstalledBinary {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write installed runshim binary `{}`", path.display())]
    WriteInstalledBinary {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create tool symlink `{}`", path.display())]
    CreateToolSymlink {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to start bazel {action}")]
    SpawnBazel {
        action: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed while waiting for bazel {action}")]
    WaitBazel {
        action: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed while reading bazel {action} output")]
    ReadBazelOutput {
        action: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "bazel build failed for `{target}`{}",
        status
            .map(|code| format!(" with exit code {code}"))
            .unwrap_or_default()
    )]
    BazelBuildFailed { target: String, status: Option<i32> },
    #[error("failed to resolve executable path for `{target}`: {stderr}")]
    BazelQueryFailed { target: String, stderr: String },
    #[error("configured target `{target}` is not executable")]
    TargetNotExecutable { target: String },
    #[error("failed to exec `{}`", path.display())]
    ExecTool {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl RunshimError {
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::ConfigNotFound { .. }
            | Self::ParseConfig { .. }
            | Self::UnsupportedConfigVersion { .. }
            | Self::InvalidConfig(_)
            | Self::ToolNotConfigured { .. }
            | Self::MissingHomeDirectory => ExitCode::from(2),
            _ => ExitCode::FAILURE,
        }
    }
}

pub fn run_from_env() -> Result<ExitCode, RunshimError> {
    let args = env::args_os().collect::<Vec<_>>();
    let argv0 = args
        .first()
        .cloned()
        .unwrap_or_else(|| OsString::from(RUNSHIM_BINARY_NAME));
    let invocation_name = invocation_name(&argv0);
    let cwd = env::current_dir()?;

    if invocation_name != RUNSHIM_BINARY_NAME {
        let forwarded_args = args.get(1..).unwrap_or(&[]).to_vec();
        dispatch_tool(&cwd, &invocation_name, &forwarded_args)?;
        return Ok(ExitCode::SUCCESS);
    }

    let cli = Cli::parse_from(args);
    let current_executable = env::current_exe()?;
    run_cli(&cwd, &current_executable, cli)
}

fn run_cli(cwd: &Path, current_executable: &Path, cli: Cli) -> Result<ExitCode, RunshimError> {
    match cli.command {
        CliCommand::Install(args) => {
            let repo_config = load_repo_config(cwd)?;
            let home_dir = current_home_dir();
            let bin_dir =
                resolve_bin_dir(args.bin_dir.bin_dir.as_deref(), cwd, home_dir.as_deref())?;
            let report = install(
                current_executable,
                &repo_config,
                &bin_dir,
                env::var_os("PATH").as_deref(),
                env::var_os("SHELL").as_deref(),
                home_dir.as_deref(),
            )?;

            println!("Installed runshim to {}", report.installed_binary.display());
            for tool in &report.installed_tools {
                println!(
                    "Installed {} -> runshim",
                    report.bin_dir.join(tool).display()
                );
            }

            if let Some(warning) = report.path_warning {
                eprintln!("warning: `{}` is not on PATH", warning.bin_dir.display());
                if let Some(config_hint) = warning.fragment.config_hint {
                    eprintln!("Add this to {config_hint}:");
                } else {
                    eprintln!("Add this to your shell config:");
                }
                eprintln!();
                eprintln!("{}", warning.fragment.fragment);
            }

            Ok(ExitCode::SUCCESS)
        }
        CliCommand::Doctor(args) => {
            let repo_config = load_repo_config(cwd)?;
            let home_dir = current_home_dir();
            let bin_dir =
                resolve_bin_dir(args.bin_dir.bin_dir.as_deref(), cwd, home_dir.as_deref())?;
            let on_path = crate::shell::bin_dir_on_path(&bin_dir, env::var_os("PATH").as_deref());

            println!("Repo root: {}", repo_config.repo_root.display());
            println!("Config: {}", repo_config.config_path.display());
            println!("Version: {}", repo_config.config.version);
            println!("Bin dir: {}", bin_dir.display());
            println!("On PATH: {}", if on_path { "yes" } else { "no" });
            println!("Tools:");
            for (name, tool) in &repo_config.config.tools {
                println!("  {name} -> {}", tool.target);
            }

            if !on_path {
                let fragment = crate::shell::path_update_fragment(
                    &bin_dir,
                    env::var_os("SHELL").as_deref(),
                    home_dir.as_deref(),
                );
                println!("Suggested PATH fragment:");
                println!("{}", fragment.fragment);
            }

            Ok(ExitCode::SUCCESS)
        }
        CliCommand::List => {
            let repo_config = load_repo_config(cwd)?;
            for (name, tool) in &repo_config.config.tools {
                println!("{name} -> {}", tool.target);
            }
            Ok(ExitCode::SUCCESS)
        }
        CliCommand::Exec(args) => {
            dispatch_tool(cwd, &args.tool, &args.args)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn dispatch_tool(
    cwd: &Path,
    tool_name: &str,
    forwarded_args: &[OsString],
) -> Result<(), RunshimError> {
    let bazel = RealBazel::new(env::var_os("RUNSHIM_VERBOSE").is_some());
    let plan = prepare_dispatch(&bazel, cwd, tool_name, forwarded_args)?;
    exec_dispatch(plan)
}

fn exec_dispatch(plan: DispatchPlan) -> Result<(), RunshimError> {
    use std::os::unix::process::CommandExt;

    let error = Command::new(&plan.executable_path)
        .arg0(&plan.tool_name)
        .args(&plan.forwarded_args)
        .current_dir(&plan.original_cwd)
        .exec();
    Err(RunshimError::ExecTool {
        path: plan.executable_path,
        source: error,
    })
}

fn invocation_name(argv0: &OsString) -> String {
    Path::new(argv0)
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| RUNSHIM_BINARY_NAME.to_string())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use super::invocation_name;

    #[test]
    fn invocation_name_uses_basename() {
        assert_eq!(
            invocation_name(&OsString::from("/Users/test/bin/boss")),
            "boss"
        );
        assert_eq!(invocation_name(&OsString::from("runshim")), "runshim");
        assert_eq!(
            invocation_name(&OsString::from(Path::new("").as_os_str())),
            "runshim"
        );
    }
}
