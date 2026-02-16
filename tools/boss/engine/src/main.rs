use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use boss_engine::app;
use boss_engine::cli::Cli;

const DEFAULT_LOG_PATH: &str = "/tmp/boss-engine.log";

struct DualLogWriter {
    stderr: io::Stderr,
    file: Option<Arc<Mutex<File>>>,
}

impl DualLogWriter {
    fn new(file: Option<Arc<Mutex<File>>>) -> Self {
        Self {
            stderr: io::stderr(),
            file,
        }
    }
}

impl Write for DualLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stderr.write_all(buf)?;
        if let Some(file) = &self.file {
            if let Ok(mut file) = file.lock() {
                let _ = file.write_all(buf);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stderr.flush()?;
        if let Some(file) = &self.file {
            if let Ok(mut file) = file.lock() {
                let _ = file.flush();
            }
        }
        Ok(())
    }
}

fn resolve_log_path() -> PathBuf {
    std::env::var("BOSS_ENGINE_LOG_PATH")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_LOG_PATH))
}

fn open_log_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create log directory {}", parent.display()))?;
        }
    }

    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open engine log file {}", path.display()))
}

#[tokio::main]
async fn main() -> Result<()> {
    let log_path = resolve_log_path();
    let file_writer = match open_log_file(&log_path) {
        Ok(file) => Some(Arc::new(Mutex::new(file))),
        Err(err) => {
            eprintln!("boss-engine: could not enable file logging at {}: {err}", log_path.display());
            None
        }
    };

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,acp_stderr=debug"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .compact()
        .with_writer(move || DualLogWriter::new(file_writer.clone()))
        .init();

    tracing::info!(log_path = %log_path.display(), "boss-engine logging initialized");

    let cli = Cli::parse();
    app::run(cli).await
}
