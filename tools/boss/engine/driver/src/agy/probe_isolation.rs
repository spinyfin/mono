//! Shared isolation setup for standalone agy evidence binaries.
//!
//! These binaries run before any threads start and redirect HOME-derived
//! paths into a caller-selected evidence directory so a diagnostic never
//! writes into the developer's live Boss state.

use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use super::environment::AGY_SKIP_BINARY_ASSERT_ENV;
use super::home::{AGY_HOMES_ROOT_ENV, AGY_SKIP_POSTURE_ASSERT_ENV};
use crate::transcript_store::WORKER_TRANSCRIPTS_ROOT_ENV;

/// Create `directory` mode 0700 and point every agy home / transcript override
/// at children of it. Also clears the test-only skip flags so a live probe
/// cannot inherit a loosened posture from the parent environment.
///
/// # Safety
///
/// Mutates process environment. Call only from a standalone binary `main`
/// before any threads exist.
pub fn prepare_evidence_dir(directory: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(directory.is_absolute(), "evidence directory must be absolute");
    DirBuilder::new().mode(0o700).create(directory)?;
    // This standalone process has no threads yet; never mutate engine globals.
    unsafe {
        std::env::set_var(AGY_HOMES_ROOT_ENV, directory.join("homes"));
        // Provisioning of worker homes links transcripts into the durable store;
        // keep the probe's inside its own evidence directory, never Boss's.
        std::env::set_var(WORKER_TRANSCRIPTS_ROOT_ENV, directory.join("transcripts"));
        std::env::remove_var(AGY_SKIP_BINARY_ASSERT_ENV);
        std::env::remove_var(AGY_SKIP_POSTURE_ASSERT_ENV);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_directory_must_be_absolute() {
        let err = prepare_evidence_dir(Path::new("relative")).unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
    }
}
