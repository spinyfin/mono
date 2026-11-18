//! The platform package: a deterministic gzip tar of the spoke's binaries.
//!
//! Packing is reproducible (sorted entries, zero timestamps and owners) so a
//! rebuild of the same inputs has the same digest. Unpacking is the dangerous
//! direction: the archive's digest has already been checked against a signed
//! manifest, but extraction still treats every entry as hostile, because the
//! installer runs as root.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};

use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use flate2::{Compression, GzBuilder};

/// What every package carries, relative to the release directory.
pub const SPOKE_BINARY: &str = "bin/boss-spoke";
pub const INSTALLER_BINARY: &str = "bin/boss-spoke-package";

const MAX_ENTRIES: usize = 64;
const MAX_ENTRY_BYTES: u64 = 512 * 1024 * 1024;

pub struct Entry {
    pub path: &'static str,
    pub bytes: Vec<u8>,
}

/// Packs `entries` as executable files.
pub fn pack(mut entries: Vec<Entry>) -> Result<Vec<u8>> {
    entries.sort_by_key(|entry| entry.path);
    let mut builder = tar::Builder::new(Vec::new());
    for entry in &entries {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(entry.bytes.len() as u64);
        header.set_mode(0o755);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        builder.append_data(&mut header, entry.path, entry.bytes.as_slice())?;
    }
    let mut gzip = GzBuilder::new().mtime(0).write(Vec::new(), Compression::default());
    gzip.write_all(&builder.into_inner()?)?;
    Ok(gzip.finish()?)
}

/// Extracts `package` into the existing, empty directory `dest`. Only regular
/// files with plain relative paths are accepted; the result is 0755 for
/// executables and 0644 otherwise, never setuid, never group- or world-writable.
pub fn unpack(package: &[u8], dest: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(GzDecoder::new(package));
    let mut seen = Vec::new();
    for (index, entry) in archive.entries()?.enumerate() {
        ensure!(index < MAX_ENTRIES, "package has too many entries");
        let entry = entry?;
        let kind = entry.header().entry_type();
        ensure!(
            matches!(kind, tar::EntryType::Regular),
            "package entry {index} is not a regular file"
        );
        let relative = entry.path()?.into_owned();
        ensure!(
            relative.components().all(|part| matches!(part, Component::Normal(_))),
            "package entry {} escapes the release directory",
            relative.display()
        );
        let executable = entry.header().mode()? & 0o111 != 0;
        let target = dest.join(&relative);
        let parent = target.parent().context("package entry has no parent")?;
        fs::DirBuilder::new().recursive(true).mode(0o755).create(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)
            .with_context(|| format!("package entry {} appears twice", relative.display()))?;
        let copied = std::io::copy(&mut entry.take(MAX_ENTRY_BYTES + 1), &mut file)?;
        ensure!(
            copied <= MAX_ENTRY_BYTES,
            "package entry {} is too large",
            relative.display()
        );
        file.set_permissions(fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }))?;
        seen.push(relative);
    }
    for required in [SPOKE_BINARY, INSTALLER_BINARY] {
        if !seen.iter().any(|path| path == Path::new(required)) {
            bail!("package is missing {required}");
        }
    }
    Ok(())
}
