//! Signed, independently installable spoke packages.
//!
//! A release for one platform is three files: `boss-spoke-<platform>.tar.gz`
//! (the `boss-spoke` daemon and this crate's installer), a manifest naming the
//! package's digest, and a detached Ed25519 signature over the manifest. The
//! release step builds and signs them ([`build`], [`manifest::SigningKey`]); a
//! person on the worker host verifies and installs them ([`install::install`]).
//! Installing files enrolls nothing: see [`install`] and the README.

pub mod archive;
pub mod install;
pub mod manifest;
pub mod service;
pub mod system;
pub mod update;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result, ensure};

use archive::{Entry, INSTALLER_BINARY, SPOKE_BINARY};
use manifest::{Manifest, Platform};

/// A freshly built, unsigned platform release.
pub struct Built {
    pub file_name: String,
    pub package: Vec<u8>,
    pub manifest: Manifest,
}

/// Packs the two binaries and describes the result. `url_base` is where the
/// package will be downloadable from; the manifest records the full URL.
pub fn build(
    spoke: Vec<u8>,
    installer: Vec<u8>,
    release_id: &str,
    platform: Platform,
    url_base: &str,
) -> Result<Built> {
    let file_name = format!("boss-spoke-{}.tar.gz", platform.as_str());
    let package = archive::pack(vec![
        Entry {
            path: SPOKE_BINARY,
            bytes: spoke,
        },
        Entry {
            path: INSTALLER_BINARY,
            bytes: installer.clone(),
        },
    ])?;
    let manifest = Manifest::describe(release_id, platform, url_base, &file_name, &package, &installer);
    Ok(Built {
        file_name,
        package,
        manifest,
    })
}

/// Reads a file holding a secret, refusing one other users can read.
pub fn read_owner_only(path: &Path) -> Result<String> {
    let metadata = fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    ensure!(
        metadata.mode() & 0o077 == 0,
        "{} must be readable by its owner only (mode 0600)",
        path.display()
    );
    fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))
}
