//! The credential database: hosts, enrollment grants, issued certificates and
//! an audit trail, in one private SQLite file.
//!
//! Every state change runs in a single `BEGIN IMMEDIATE` transaction, which is
//! what makes "consume the token and bind it to the certificate" atomic and
//! serializes two racing enrollments of the same token. Times are Unix
//! seconds. Nothing secret is stored: tokens as digests, certificates as the
//! public bytes that were sent. The records still decide who may connect (a
//! revoked host flipped back to enrolled would reconnect), so the database and
//! its directory are held to the same owner-only rule as the CA's.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use rusqlite::{Connection, OptionalExtension as _, Transaction, TransactionBehavior, params};

use crate::error::CredentialError;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS hosts (
    host_id TEXT PRIMARY KEY,
    state TEXT NOT NULL CHECK (state IN ('pending','enrolled','revoked')),
    generation INTEGER NOT NULL,
    current_serial TEXT,
    previous_serial TEXT,
    previous_valid_until INTEGER
) STRICT;
CREATE TABLE IF NOT EXISTS enrollment_grants (
    token_hash BLOB PRIMARY KEY,
    host_id TEXT NOT NULL REFERENCES hosts(host_id),
    job_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('issued','consumed','replaced','cancelled')),
    consumed_at INTEGER,
    -- The certificate a consumed token produced: the only one a retry of it
    -- may be handed back.
    certificate_serial TEXT
) STRICT;
CREATE INDEX IF NOT EXISTS enrollment_grants_by_host ON enrollment_grants (host_id, state);
CREATE TABLE IF NOT EXISTS certificates (
    serial TEXT PRIMARY KEY,
    host_id TEXT NOT NULL REFERENCES hosts(host_id),
    kind TEXT NOT NULL CHECK (kind IN ('enroll','renew')),
    job_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    key_digest BLOB NOT NULL,
    leaf_der BLOB NOT NULL,
    not_after INTEGER NOT NULL,
    UNIQUE (host_id, kind, job_id, request_id)
) STRICT;
CREATE TABLE IF NOT EXISTS audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    host_id TEXT NOT NULL,
    event TEXT NOT NULL,
    detail TEXT NOT NULL
) STRICT;
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostState {
    /// Authorized by the hub, no certificate issued yet. Not schedulable.
    Pending,
    Enrolled,
    /// Closed to reconnect, renewal and re-enrollment until explicitly
    /// authorized again.
    Revoked,
}

impl HostState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Enrolled => "enrolled",
            Self::Revoked => "revoked",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "pending" => Some(Self::Pending),
            "enrolled" => Some(Self::Enrolled),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GrantState {
    Issued,
    /// Consumed at this Unix second, producing the certificate with this
    /// serial: the only one a retry of the token may be handed back.
    Consumed(i64, String),
    Replaced,
    Cancelled,
}

impl GrantState {
    fn parse(text: &str, consumed_at: Option<i64>, serial: Option<String>) -> Option<Self> {
        match text {
            "issued" => Some(Self::Issued),
            "consumed" => consumed_at.zip(serial).map(|(at, serial)| Self::Consumed(at, serial)),
            "replaced" => Some(Self::Replaced),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CertKind {
    Enroll,
    Renew,
}

impl CertKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Enroll => "enroll",
            Self::Renew => "renew",
        }
    }
}

/// A host's credential state. The current certificate is the one the spoke
/// should be using; the previous one stays valid only until
/// `previous_valid_until`.
#[derive(Debug, Clone)]
pub(crate) struct HostRow {
    pub state: HostState,
    pub generation: i64,
    pub current_serial: Option<String>,
    pub previous_serial: Option<String>,
    pub previous_valid_until: Option<i64>,
}

pub(crate) struct GrantRow {
    pub host_id: String,
    pub job_id: String,
    pub generation: i64,
    pub expires_at: i64,
    pub state: GrantState,
}

/// An issued certificate as recorded.
#[derive(Debug, Clone)]
pub(crate) struct CertRow {
    pub serial: String,
    pub key_digest: Vec<u8>,
    pub leaf_der: Vec<u8>,
    pub not_after: i64,
}

pub(crate) struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub(crate) fn open(path: &Path) -> Result<Self, CredentialError> {
        // The mode below applies only when the file is created, so anything
        // that already exists is checked rather than trusted.
        let dir = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        require_owner_only(dir)?;
        for existing in [path.to_owned(), sidecar(path, "-wal"), sidecar(path, "-shm")] {
            require_owner_only(&existing)?;
        }
        // Create the file owner-only before SQLite does. SQLite gives its -wal
        // and -shm files the same mode.
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(|error| CredentialError::Internal(format!("{}: {error}", path.display())))?;
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // A credential that was handed out must survive a power cut: the
        // spoke will present it, and the hub has to still know it.
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(CredentialError::Internal(format!(
                "credential database is schema {version}, newer than this build understands ({SCHEMA_VERSION})"
            )));
        }
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Runs `f` in an immediate transaction, committing only on `Ok`.
    pub(crate) fn write<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, CredentialError>,
    ) -> Result<T, CredentialError> {
        let mut conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    pub(crate) fn read<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, CredentialError>,
    ) -> Result<T, CredentialError> {
        f(&self.conn.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Refuses a file or directory that group or others can access. A path that
/// does not exist yet passes: it is about to be created owner-only.
fn require_owner_only(path: &Path) -> Result<(), CredentialError> {
    match fs::metadata(path) {
        Ok(meta) if meta.mode() & 0o077 == 0 => Ok(()),
        Ok(meta) => Err(CredentialError::Internal(format!(
            "{} is accessible to other users (mode {:o}); refusing to trust it",
            path.display(),
            meta.mode() & 0o777
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CredentialError::Internal(format!("{}: {error}", path.display()))),
    }
}

pub(crate) fn host(conn: &Connection, host_id: &str) -> Result<Option<HostRow>, CredentialError> {
    conn.query_row(
        "SELECT state, generation, current_serial, previous_serial, previous_valid_until
         FROM hosts WHERE host_id = ?1",
        [host_id],
        |row| {
            let state: String = row.get(0)?;
            Ok(HostRow {
                state: HostState::parse(&state).unwrap_or(HostState::Revoked),
                generation: row.get(1)?,
                current_serial: row.get(2)?,
                previous_serial: row.get(3)?,
                previous_valid_until: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn insert_host(tx: &Transaction<'_>, host_id: &str) -> Result<(), CredentialError> {
    tx.execute(
        "INSERT INTO hosts (host_id, state, generation) VALUES (?1, 'pending', 1)",
        [host_id],
    )?;
    Ok(())
}

/// Replaces a host's whole credential state.
pub(crate) fn set_host(
    tx: &Transaction<'_>,
    host_id: &str,
    state: HostState,
    serials: (Option<&str>, Option<(&str, i64)>),
) -> Result<(), CredentialError> {
    let (current, previous) = serials;
    tx.execute(
        "UPDATE hosts SET state = ?2, current_serial = ?3, previous_serial = ?4, previous_valid_until = ?5
         WHERE host_id = ?1",
        params![
            host_id,
            state.as_str(),
            current,
            previous.map(|p| p.0),
            previous.map(|p| p.1)
        ],
    )?;
    Ok(())
}

pub(crate) fn set_state(tx: &Transaction<'_>, host_id: &str, state: HostState) -> Result<(), CredentialError> {
    tx.execute(
        "UPDATE hosts SET state = ?2 WHERE host_id = ?1",
        params![host_id, state.as_str()],
    )?;
    Ok(())
}

/// Starts a new authorization of `host_id`: a higher generation invalidates
/// every grant issued under the previous one.
pub(crate) fn bump_generation(tx: &Transaction<'_>, host_id: &str) -> Result<(), CredentialError> {
    tx.execute(
        "UPDATE hosts SET generation = generation + 1 WHERE host_id = ?1",
        [host_id],
    )?;
    Ok(())
}

pub(crate) fn grant(conn: &Connection, token_hash: &[u8]) -> Result<Option<GrantRow>, CredentialError> {
    conn.query_row(
        "SELECT host_id, job_id, generation, expires_at, state, consumed_at, certificate_serial
         FROM enrollment_grants WHERE token_hash = ?1",
        [token_hash],
        |row| {
            let state: String = row.get(4)?;
            Ok(GrantRow {
                host_id: row.get(0)?,
                job_id: row.get(1)?,
                generation: row.get(2)?,
                expires_at: row.get(3)?,
                // An unreadable state fails closed.
                state: GrantState::parse(&state, row.get(5)?, row.get(6)?).unwrap_or(GrantState::Cancelled),
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn insert_grant(
    tx: &Transaction<'_>,
    token_hash: &[u8],
    host_id: &str,
    job_id: &str,
    when: (i64, i64),
) -> Result<(), CredentialError> {
    let (generation, expires_at) = when;
    tx.execute(
        "INSERT INTO enrollment_grants (token_hash, host_id, job_id, generation, expires_at, state)
         VALUES (?1, ?2, ?3, ?4, ?5, 'issued')",
        params![token_hash, host_id, job_id, generation, expires_at],
    )?;
    Ok(())
}

/// Spends the grant and binds it to the certificate it produced.
pub(crate) fn consume_grant(
    tx: &Transaction<'_>,
    token_hash: &[u8],
    now: i64,
    serial: &str,
) -> Result<(), CredentialError> {
    tx.execute(
        "UPDATE enrollment_grants SET state = 'consumed', consumed_at = ?2, certificate_serial = ?3
         WHERE token_hash = ?1",
        params![token_hash, now, serial],
    )?;
    Ok(())
}

/// Moves a host's `issued` grants (every job's, or just `job_id`'s) to
/// `new_state`; with a job, `consumed` grants move too, which closes that job's
/// retry window.
pub(crate) fn retire_grants(
    tx: &Transaction<'_>,
    host_id: &str,
    job_id: Option<&str>,
    new_state: &str,
) -> Result<usize, CredentialError> {
    let changed = match job_id {
        Some(job) => tx.execute(
            "UPDATE enrollment_grants SET state = ?3
             WHERE host_id = ?1 AND job_id = ?2 AND state IN ('issued','consumed')",
            params![host_id, job, new_state],
        )?,
        None => tx.execute(
            "UPDATE enrollment_grants SET state = ?2 WHERE host_id = ?1 AND state = 'issued'",
            params![host_id, new_state],
        )?,
    };
    Ok(changed)
}

fn cert_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CertRow> {
    Ok(CertRow {
        serial: row.get(0)?,
        key_digest: row.get(1)?,
        leaf_der: row.get(2)?,
        not_after: row.get(3)?,
    })
}

const CERT_COLUMNS: &str = "serial, key_digest, leaf_der, not_after";

pub(crate) fn certificate(conn: &Connection, serial: &str) -> Result<Option<CertRow>, CredentialError> {
    conn.query_row(
        &format!("SELECT {CERT_COLUMNS} FROM certificates WHERE serial = ?1"),
        [serial],
        cert_row,
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn certificate_for_request(
    conn: &Connection,
    host_id: &str,
    kind: CertKind,
    ids: (&str, &str),
) -> Result<Option<CertRow>, CredentialError> {
    let (job_id, request_id) = ids;
    conn.query_row(
        &format!(
            "SELECT {CERT_COLUMNS} FROM certificates
             WHERE host_id = ?1 AND kind = ?2 AND job_id = ?3 AND request_id = ?4"
        ),
        params![host_id, kind.as_str(), job_id, request_id],
        cert_row,
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn insert_certificate(
    tx: &Transaction<'_>,
    host_id: &str,
    kind: CertKind,
    ids: (&str, &str),
    cert: &CertRow,
) -> Result<(), CredentialError> {
    let (job_id, request_id) = ids;
    tx.execute(
        "INSERT INTO certificates (serial, host_id, kind, job_id, request_id, key_digest, leaf_der, not_after)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            cert.serial,
            host_id,
            kind.as_str(),
            job_id,
            request_id,
            cert.key_digest,
            cert.leaf_der,
            cert.not_after
        ],
    )?;
    Ok(())
}

/// Whether `digest` is the key of the host's current or previous certificate.
pub(crate) fn key_in_use(conn: &Connection, host: &HostRow, digest: &[u8]) -> Result<bool, CredentialError> {
    for serial in [&host.current_serial, &host.previous_serial].into_iter().flatten() {
        if certificate(conn, serial)?.is_some_and(|cert| cert.key_digest == digest) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Each enrolled host with the certificate it should currently be using,
/// as `(host_id, leaf_der)`, ordered by host id.
pub(crate) fn enrolled_current_certificates(conn: &Connection) -> Result<Vec<(String, Vec<u8>)>, CredentialError> {
    let mut statement = conn.prepare(
        "SELECT hosts.host_id, certificates.leaf_der
         FROM hosts JOIN certificates ON certificates.serial = hosts.current_serial
         WHERE hosts.state = 'enrolled' ORDER BY hosts.host_id",
    )?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect::<Result<_, _>>().map_err(Into::into)
}

/// Records a state change. `detail` must never contain a secret.
pub(crate) fn audit(
    tx: &Transaction<'_>,
    at: i64,
    host_id: &str,
    event: &str,
    detail: &str,
) -> Result<(), CredentialError> {
    tx.execute(
        "INSERT INTO audit (at, host_id, event, detail) VALUES (?1, ?2, ?3, ?4)",
        params![at, host_id, event, detail],
    )?;
    Ok(())
}

/// Audit rows for `host_id`, oldest first, as `(event, detail)`.
pub(crate) fn audit_trail(conn: &Connection, host_id: &str) -> Result<Vec<(String, String)>, CredentialError> {
    let mut statement = conn.prepare("SELECT event, detail FROM audit WHERE host_id = ?1 ORDER BY id")?;
    let rows = statement.query_map([host_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect::<Result<_, _>>().map_err(Into::into)
}
