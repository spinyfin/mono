//! Standalone Node provisioning. Pins are from
//! https://nodejs.org/dist/v24.8.0/SHASUMS256.txt.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, TryLockError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::external::timeout::{CheckDeadline, output_with_timeout};

const VERSION: &str = "24.8.0";

fn distribution() -> Result<(&'static str, &'static str)> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok((
            "linux-x64",
            "daf68404b478b4c3616666580d02500a24148c0f439e4d0134d65ce70e90e655",
        )),
        ("linux", "aarch64") => Ok((
            "linux-arm64",
            "5eb16b14af5a5f494ed54770822144e847c744fe590f8df093ad4927cf3dd7fd",
        )),
        ("macos", "aarch64") => Ok((
            "darwin-arm64",
            "d81191a1866760eb918caa976c023036bc1fc7405ea31b148905211522045767",
        )),
        ("macos", "x86_64") => Ok((
            "darwin-x64",
            "6fd8496b59baa8f86a24e3eb03308b763091716ffc6b6e1094d1a5e5696dd6dd",
        )),
        (os, arch) => bail!("pinned Node is unavailable for {os}/{arch}"),
    }
}

pub(super) fn provision(check_id: &str, deadline: CheckDeadline) -> Result<PathBuf> {
    ensure_supported_libc(check_id, deadline)?;
    let (platform, digest) = distribution()?;
    let cache = directories::ProjectDirs::from("", "", "checkleft")
        .context("cannot locate checkleft cache for pinned Node")?
        .cache_dir()
        .join("node");
    fs::create_dir_all(&cache)?;
    let name = format!("node-v{VERSION}-{platform}");
    let destination = cache.join(format!("{name}-{digest}"));
    let npx = destination.join("bin/npx");
    if npx.is_file() && destination.join("bin/node").is_file() {
        return Ok(npx);
    }
    // One release per process/platform; the file lock also coordinates other processes.
    static PROVISION: Mutex<()> = Mutex::new(());
    let _guard = loop {
        match PROVISION.try_lock() {
            Ok(guard) => break guard,
            Err(TryLockError::Poisoned(error)) => break error.into_inner(),
            Err(TryLockError::WouldBlock) => wait_for_lock(deadline)?,
        }
    };
    let _lock = cache_lock(&cache.join(format!("{name}-{digest}.lock")), deadline)?;
    // A preceding provisioner may have populated the cache while we waited.
    if npx.is_file() && destination.join("bin/node").is_file() {
        return Ok(npx);
    }
    let url = format!("https://nodejs.org/dist/v{VERSION}/{name}.tar.gz");
    // Resolution is synchronous, and may itself be called inside a Tokio runtime.
    // Use an owned runtime on a scoped thread, as opposed to nesting block_on.
    let bytes = std::thread::scope(|scope| {
        scope
            .spawn(|| -> Result<Vec<u8>> {
                let _ = rustls::crypto::ring::default_provider().install_default();
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(async {
                        let client = reqwest::Client::builder().build()?;
                        for attempt in 0..4 {
                            let result = async {
                                Ok::<_, reqwest::Error>(
                                    client
                                        .get(&url)
                                        .timeout(Duration::from_millis(deadline.remaining_ms().max(1)))
                                        .send()
                                        .await?
                                        .error_for_status()?
                                        .bytes()
                                        .await?
                                        .to_vec(),
                                )
                            }
                            .await;
                            match result {
                                Ok(bytes) => return Ok(bytes),
                                Err(error) => {
                                    let delay_ms = 500 * (1 << attempt);
                                    if attempt == 3 || deadline.remaining_ms() <= delay_ms {
                                        return Err(error).context("download pinned Node");
                                    }
                                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                                }
                            }
                        }
                        unreachable!("bounded download attempts")
                    })
            })
            .join()
            .map_err(|_| anyhow::anyhow!("Node download thread panicked"))?
    })?;
    publish_archive(&bytes, digest, &name, &cache, check_id, deadline)
}

fn publish_archive(
    bytes: &[u8],
    digest: &str,
    name: &str,
    cache: &Path,
    check_id: &str,
    deadline: CheckDeadline,
) -> Result<PathBuf> {
    verify(bytes, digest)?;
    let staging = tempfile::tempdir_in(cache)?;
    let destination = cache.join(format!("{name}-{digest}"));
    let npx = destination.join("bin/npx");
    let archive = staging.path().join("node.tar.gz");
    fs::write(&archive, bytes)?;
    let mut extract = Command::new("tar");
    extract.arg("-xzf").arg(&archive).arg("-C").arg(staging.path());
    let output = output_with_timeout(&mut extract, check_id, "extract pinned Node", deadline.remaining_ms())?;
    if !output.status.success() {
        bail!("Node extraction failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    let tree = staging.path().join(name);
    if !tree.join("bin/node").is_file() || !tree.join("bin/npx").is_file() {
        bail!("verified Node archive lacks node/npx");
    }
    if let Err(error) = fs::rename(&tree, &destination) {
        // Another check may have atomically published the same verified release.
        if !npx.is_file() || !destination.join("bin/node").is_file() {
            return Err(error).context("publish pinned Node cache entry");
        }
    }
    Ok(npx)
}

fn wait_for_lock(deadline: CheckDeadline) -> Result<()> {
    let remaining = deadline.remaining_ms();
    anyhow::ensure!(remaining > 0, "deadline exceeded waiting for pinned Node provisioning");
    std::thread::sleep(Duration::from_millis(remaining.min(25)));
    Ok(())
}

fn cache_lock(path: &Path, deadline: CheckDeadline) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) => wait_for_lock(deadline)?,
            Err(fs::TryLockError::Error(error)) => return Err(error).context("lock pinned Node cache"),
        }
    }
}

fn ensure_supported_libc(check_id: &str, deadline: CheckDeadline) -> Result<()> {
    if std::env::consts::OS != "linux" {
        return Ok(());
    }
    let output = output_with_timeout(
        Command::new("getconf").arg("GNU_LIBC_VERSION"),
        check_id,
        "detect glibc for pinned Node",
        deadline.remaining_ms(),
    )
    .context("pinned Node is unavailable: Linux requires glibc >= 2.28 and getconf; use a supported glibc host")?;
    anyhow::ensure!(
        output.status.success() && supported_glibc(&String::from_utf8_lossy(&output.stdout)),
        "pinned Node is unavailable: Linux requires glibc >= 2.28; musl/Alpine and older glibc hosts are unsupported; use a supported glibc host"
    );
    Ok(())
}

fn supported_glibc(value: &str) -> bool {
    value
        .trim()
        .strip_prefix("glibc ")
        .and_then(|v| v.split_once('.'))
        .and_then(|(major, minor)| Some((major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?)))
        .is_some_and(|version| version >= (2, 28))
}

fn verify(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        bail!("Node archive sha256 mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

#[cfg(all(test, unix))]
#[path = "tests_node.rs"]
mod tests;
