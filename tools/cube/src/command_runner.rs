use std::path::{Path, PathBuf};
use std::process::Command;

use crate::app::CubeError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInvocation {
    pub cwd: PathBuf,
    pub program: String,
    pub args: Vec<String>,
}

pub trait CommandRunner {
    fn run(&self, invocation: &CommandInvocation) -> Result<String, CubeError>;
}

pub struct RealCommandRunner;

impl RealCommandRunner {
    pub fn invocation(cwd: &Path, program: &str, args: &[&str]) -> CommandInvocation {
        CommandInvocation {
            cwd: cwd.to_path_buf(),
            program: program.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
        }
    }
}

impl CommandRunner for RealCommandRunner {
    fn run(&self, invocation: &CommandInvocation) -> Result<String, CubeError> {
        let output = Command::new(&invocation.program)
            .args(&invocation.args)
            .current_dir(&invocation.cwd)
            .output()?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            Err(CubeError::CommandFailed {
                program: invocation.program.clone(),
                args: invocation.args.clone(),
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }
}
