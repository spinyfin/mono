//! The spoke's credential files: an installed identity and, while a request is
//! in flight, the key and request id it was made with.
//!
//! Both are one small text file each, written to a sibling temp file, synced,
//! and renamed over the target, so a reader (or a restart) sees the old file or
//! the new one and never half of either. The certificate and its key live in
//! the same file for that reason: installing them is a single rename.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::error::ClientError;

/// An enrolled host's certificate and the key it was issued for.
#[derive(Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct Identity {
    pub host_id: String,
    pub serial: String,
    pub expires_at: SystemTime,
    pub leaf_der: Vec<u8>,
    /// The trust bundle the hub sent with the certificate, issuing CA first.
    pub chain_der: Vec<Vec<u8>>,
    pub(crate) key_pkcs8_der: Vec<u8>,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("host_id", &self.host_id)
            .field("serial", &self.serial)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// The private key as PKCS#8 PEM, for building a TLS client identity.
    pub fn key_pem(&self) -> String {
        pem("PRIVATE KEY", &self.key_pkcs8_der)
    }

    /// The leaf certificate as PEM.
    pub fn leaf_pem(&self) -> String {
        pem("CERTIFICATE", &self.leaf_der)
    }
}

/// A request the spoke has decided to make: the key it will prove possession
/// of and the id the hub deduplicates retries on. Persisted before the request
/// is sent so a retry after a crash is the same request, not a new one.
pub(crate) struct Pending {
    pub request_id: String,
    pub key_pkcs8_der: Vec<u8>,
}

pub(crate) const IDENTITY: &str = "identity";
pub(crate) const PENDING_ENROLL: &str = "pending-enroll";
pub(crate) const PENDING_RENEW: &str = "pending-renew";

pub(crate) fn pem(label: &str, der: &[u8]) -> String {
    let body = STANDARD.encode(der);
    let lines: Vec<&str> = body
        .as_bytes()
        .chunks(64)
        .map(|chunk| std::str::from_utf8(chunk).expect("base64 is ascii"))
        .collect();
    format!("-----BEGIN {label}-----\n{}\n-----END {label}-----\n", lines.join("\n"))
}

pub(crate) fn write_identity(dir: &Path, identity: &Identity) -> Result<(), ClientError> {
    let expires_ms = identity
        .expires_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let mut text = format!(
        "host_id {}\nserial {}\nexpires_unix_ms {expires_ms}\nkey {}\nleaf {}\n",
        identity.host_id,
        identity.serial,
        STANDARD.encode(&identity.key_pkcs8_der),
        STANDARD.encode(&identity.leaf_der),
    );
    for cert in &identity.chain_der {
        text.push_str(&format!("chain {}\n", STANDARD.encode(cert)));
    }
    write_atomic(dir, IDENTITY, &text)
}

pub(crate) fn read_identity(dir: &Path) -> Result<Option<Identity>, ClientError> {
    let Some(text) = read(dir, IDENTITY)? else {
        return Ok(None);
    };
    let (mut host_id, mut serial, mut expires_ms) = (None, None, None);
    let (mut key, mut leaf, mut chain) = (None, None, Vec::new());
    for line in text.lines() {
        let (name, value) = line.split_once(' ').ok_or_else(corrupt)?;
        match name {
            "host_id" => host_id = Some(value.to_owned()),
            "serial" => serial = Some(value.to_owned()),
            "expires_unix_ms" => expires_ms = Some(value.parse::<u64>().map_err(|_| corrupt())?),
            "key" => key = Some(decode(value)?),
            "leaf" => leaf = Some(decode(value)?),
            "chain" => chain.push(decode(value)?),
            _ => return Err(corrupt()),
        }
    }
    Ok(Some(Identity {
        host_id: host_id.ok_or_else(corrupt)?,
        serial: serial.ok_or_else(corrupt)?,
        expires_at: SystemTime::UNIX_EPOCH + Duration::from_millis(expires_ms.ok_or_else(corrupt)?),
        leaf_der: leaf.ok_or_else(corrupt)?,
        chain_der: chain,
        key_pkcs8_der: key.ok_or_else(corrupt)?,
    }))
}

pub(crate) fn write_pending(dir: &Path, name: &str, pending: &Pending) -> Result<(), ClientError> {
    let text = format!(
        "request_id {}\nkey {}\n",
        pending.request_id,
        STANDARD.encode(&pending.key_pkcs8_der)
    );
    write_atomic(dir, name, &text)
}

pub(crate) fn read_pending(dir: &Path, name: &str) -> Result<Option<Pending>, ClientError> {
    let Some(text) = read(dir, name)? else {
        return Ok(None);
    };
    let (mut request_id, mut key) = (None, None);
    for line in text.lines() {
        let (field, value) = line.split_once(' ').ok_or_else(corrupt)?;
        match field {
            "request_id" => request_id = Some(value.to_owned()),
            "key" => key = Some(decode(value)?),
            _ => return Err(corrupt()),
        }
    }
    Ok(Some(Pending {
        request_id: request_id.ok_or_else(corrupt)?,
        key_pkcs8_der: key.ok_or_else(corrupt)?,
    }))
}

pub(crate) fn remove(dir: &Path, name: &str) -> Result<(), ClientError> {
    match fs::remove_file(dir.join(name)) {
        Ok(()) => sync_dir(dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn read(dir: &Path, name: &str) -> Result<Option<String>, ClientError> {
    match fs::read_to_string(dir.join(name)) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_atomic(dir: &Path, name: &str, text: &str) -> Result<(), ClientError> {
    let temp = dir.join(format!(".{name}.tmp"));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, dir.join(name))?;
    sync_dir(dir)
}

fn sync_dir(dir: &Path) -> Result<(), ClientError> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

pub(crate) fn create_private_dir(dir: &Path) -> Result<(), ClientError> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn decode(value: &str) -> Result<Vec<u8>, ClientError> {
    STANDARD.decode(value).map_err(|_| corrupt())
}

fn corrupt() -> ClientError {
    ClientError::Corrupt
}
