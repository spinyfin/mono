use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::app::RunshimError;

const SLOW_BUILD_NOTICE: Duration = Duration::from_secs(3);
const STREAM_BUILD_OUTPUT: Duration = Duration::from_secs(10);

pub trait BazelAdapter {
    fn build(&self, repo_root: &Path, target: &str) -> Result<(), RunshimError>;
    fn resolve_executable(&self, repo_root: &Path, target: &str) -> Result<PathBuf, RunshimError>;
}

#[derive(Debug, Clone, Copy)]
pub struct RealBazel {
    verbose: bool,
}

impl RealBazel {
    pub fn new(verbose: bool) -> Self {
        Self { verbose }
    }
}

impl BazelAdapter for RealBazel {
    fn build(&self, repo_root: &Path, target: &str) -> Result<(), RunshimError> {
        let mut command = Command::new("bazel");
        command
            .arg("build")
            .arg("--color=no")
            .arg("--curses=no")
            .arg("--show_result=0")
            .arg("--noshow_progress")
            .arg("--ui_event_filters=-info")
            .arg(target)
            .current_dir(repo_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|source| RunshimError::SpawnBazel {
            action: "build".to_string(),
            source,
        })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RunshimError::SpawnBazel {
                action: "build".to_string(),
                source: io::Error::other("missing stdout pipe"),
            })?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| RunshimError::SpawnBazel {
                action: "build".to_string(),
                source: io::Error::other("missing stderr pipe"),
            })?;

        let (tx, rx) = mpsc::channel();
        let stdout_handle = spawn_reader(stdout, tx.clone());
        let stderr_handle = spawn_reader(stderr, tx);
        let started_at = Instant::now();
        let mut combined_output = Vec::new();
        let mut printed_notice = self.verbose;
        let mut streaming = self.verbose;
        let mut stderr_writer = io::stderr().lock();

        if self.verbose {
            writeln!(stderr_writer, "runshim: building {target}...").ok();
        }

        let status = loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => {
                    combined_output.extend_from_slice(&chunk);
                    if streaming {
                        stderr_writer.write_all(&chunk).ok();
                        stderr_writer.flush().ok();
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if let Some(status) =
                        child.try_wait().map_err(|source| RunshimError::WaitBazel {
                            action: "build".to_string(),
                            source,
                        })?
                    {
                        break status;
                    }
                }
            }

            if !printed_notice && started_at.elapsed() >= SLOW_BUILD_NOTICE {
                writeln!(stderr_writer, "runshim: building {target}...").ok();
                stderr_writer.flush().ok();
                printed_notice = true;
            }

            if !streaming && started_at.elapsed() >= STREAM_BUILD_OUTPUT {
                writeln!(
                    stderr_writer,
                    "runshim: build still running; streaming Bazel output..."
                )
                .ok();
                if !combined_output.is_empty() {
                    stderr_writer.write_all(&combined_output).ok();
                }
                stderr_writer.flush().ok();
                streaming = true;
            }

            if let Some(status) = child.try_wait().map_err(|source| RunshimError::WaitBazel {
                action: "build".to_string(),
                source,
            })? {
                break status;
            }
        };

        while let Ok(chunk) = rx.try_recv() {
            combined_output.extend_from_slice(&chunk);
            if streaming {
                stderr_writer.write_all(&chunk).ok();
            }
        }

        stdout_handle
            .join()
            .expect("stdout reader thread")
            .map_err(|source| RunshimError::ReadBazelOutput {
                action: "build".to_string(),
                source,
            })?;
        stderr_handle
            .join()
            .expect("stderr reader thread")
            .map_err(|source| RunshimError::ReadBazelOutput {
                action: "build".to_string(),
                source,
            })?;

        if status.success() {
            return Ok(());
        }

        if !streaming && !combined_output.is_empty() {
            stderr_writer.write_all(&combined_output).ok();
            stderr_writer.flush().ok();
        }

        Err(RunshimError::BazelBuildFailed {
            target: target.to_string(),
            status: status.code(),
        })
    }

    fn resolve_executable(&self, repo_root: &Path, target: &str) -> Result<PathBuf, RunshimError> {
        let output = Command::new("bazel")
            .arg("cquery")
            .arg("--color=no")
            .arg("--curses=no")
            .arg(target)
            .arg("--output=starlark")
            .arg("--starlark:expr=target.files_to_run.executable.path if target.files_to_run.executable else ''")
            .current_dir(repo_root)
            .output()
            .map_err(|source| RunshimError::SpawnBazel {
                action: "cquery".to_string(),
                source,
            })?;

        if !output.status.success() {
            return Err(RunshimError::BazelQueryFailed {
                target: target.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        let raw = String::from_utf8_lossy(&output.stdout)
            .trim()
            .trim_matches('"')
            .to_string();
        if raw.is_empty() {
            return Err(RunshimError::TargetNotExecutable {
                target: target.to_string(),
            });
        }

        let path = PathBuf::from(raw);
        if path.is_absolute() {
            Ok(path)
        } else {
            Ok(repo_root.join(path))
        }
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    tx: mpsc::Sender<Vec<u8>>,
) -> thread::JoinHandle<io::Result<()>> {
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                return Ok(());
            }
            if tx.send(buffer[..read].to_vec()).is_err() {
                return Ok(());
            }
        }
    })
}
