//! Host-private path helpers for run-scoped worker files.
use std::path::{Path, PathBuf};

pub(crate) fn hex_token(spawn_token: &str) -> String {
    spawn_token
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn outputs_root(homes_root: &Path, spawn_token: &str) -> PathBuf {
    homes_root.join(format!("outputs-{}", hex_token(spawn_token)))
}
