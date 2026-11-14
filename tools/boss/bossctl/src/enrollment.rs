//! `bossctl enrollment`: the hub operator's side of manual spoke host
//! enrollment.
//!
//! These commands open the hub's host-credential state directly, the way the
//! other `hosts` commands open `state.db`, so they work while the engine is
//! stopped. They add no trust path of their own: tokens come from
//! `HostCredentials::issue_manual_token`, and revocation is the same
//! `HostCredentials::revoke` the stream manager honours. The hub checks its
//! records every few seconds, so a revocation made here closes a host's open
//! stream without any message to the engine.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use boss_engine::hub::CREDENTIALS_DIR_ENV;
use boss_host_enrollment::{CredentialError, Denial, HostCredentials, HostState, HostSummary, Policy, SystemClock};
use clap::Subcommand;

const ACTOR: &str = "bossctl";

#[derive(Subcommand, Debug)]
pub(crate) enum EnrollmentAction {
    /// Mint the single-use enrollment token for one named host.
    ///
    /// The token is bound to that host, works once, and expires after 15
    /// minutes; minting again replaces an unused token. It is shown once. Carry
    /// it to the host by hand (see the manual host enrollment runbook); it is
    /// never needed on a command line there.
    Token {
        /// The host id to enroll: the id its spoke package was installed with.
        host_id: String,
        /// Write the token to this new owner-only file instead of printing it.
        #[arg(long)]
        token_out: Option<PathBuf>,
        /// Open a revoked host again. Without this a revoked host is refused.
        #[arg(long)]
        reauthorize: bool,
        /// The hub's host-credential directory. Defaults to $BOSS_HUB_CREDENTIALS_DIR.
        #[arg(long)]
        credentials_dir: Option<PathBuf>,
    },
    /// List the hosts the hub has authorized, with their enrollment state.
    List {
        /// The hub's host-credential directory. Defaults to $BOSS_HUB_CREDENTIALS_DIR.
        #[arg(long)]
        credentials_dir: Option<PathBuf>,
    },
    /// Revoke a host's identity.
    ///
    /// Its open stream is closed, and it cannot reconnect, renew or enroll
    /// again until `token --reauthorize` opens it. Workers already running on
    /// the host are not stopped.
    Revoke {
        host_id: String,
        /// Why the host is being revoked; recorded in the audit trail.
        #[arg(long)]
        reason: String,
        /// The hub's host-credential directory. Defaults to $BOSS_HUB_CREDENTIALS_DIR.
        #[arg(long)]
        credentials_dir: Option<PathBuf>,
    },
}

pub(crate) fn run(json: bool, action: EnrollmentAction) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match action {
        EnrollmentAction::Token {
            host_id,
            token_out,
            reauthorize,
            credentials_dir,
        } => token(
            &open(credentials_dir)?,
            &host_id,
            token_out.as_deref(),
            reauthorize,
            json,
            &mut out,
        ),
        EnrollmentAction::List { credentials_dir } => list(&open(credentials_dir)?, json, &mut out),
        EnrollmentAction::Revoke {
            host_id,
            reason,
            credentials_dir,
        } => revoke(&open(credentials_dir)?, &host_id, &reason, json, &mut out),
    }
}

/// The hub's existing credential state. Never creates a CA: a path that does
/// not hold one is a mistake to report, not a hub to invent.
fn open(dir: Option<PathBuf>) -> Result<HostCredentials> {
    let dir = dir
        .or_else(|| std::env::var_os(CREDENTIALS_DIR_ENV).map(PathBuf::from))
        .filter(|dir| !dir.as_os_str().is_empty())
        .with_context(|| {
            format!("pass --credentials-dir or set {CREDENTIALS_DIR_ENV} to the hub's host-credential directory")
        })?;
    HostCredentials::open_existing(&dir, Policy::default(), Arc::new(SystemClock))
        .with_context(|| format!("opening host credentials at {}", dir.display()))
}

pub(crate) fn token(
    credentials: &HostCredentials,
    host_id: &str,
    token_out: Option<&Path>,
    reauthorize: bool,
    json: bool,
    out: &mut dyn std::io::Write,
) -> Result<()> {
    let grant = match credentials.issue_manual_token(host_id, ACTOR, reauthorize) {
        Ok(grant) => grant,
        Err(CredentialError::Denied(Denial::Revoked)) => bail!(
            "host {host_id} is revoked; its old certificates stay dead. Pass --reauthorize to open it again and mint a token"
        ),
        Err(error) => return Err(error).with_context(|| format!("minting a token for {host_id}")),
    };
    let text = grant.token.encode();
    if let Some(path) = token_out {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("{} must not already exist", path.display()))?;
        writeln!(file, "{text}")?;
    }
    let expires = unix_secs(grant.expires_at);
    if json {
        let mut value = serde_json::json!({ "host_id": host_id, "expires_at": expires });
        match token_out {
            Some(path) => value["token_file"] = path.display().to_string().into(),
            None => value["token"] = text.into(),
        }
        writeln!(out, "{value}")?;
        return Ok(());
    }
    writeln!(
        out,
        "enrollment token for host {host_id}: single use, expires in {}",
        age(grant.expires_at.duration_since(SystemTime::now()).unwrap_or_default())
    )?;
    match token_out {
        Some(path) => writeln!(out, "written to {} (owner-only); it is not shown again", path.display())?,
        None => writeln!(out, "{text}\n(shown once; it is not stored and cannot be shown again)")?,
    }
    Ok(())
}

pub(crate) fn list(credentials: &HostCredentials, json: bool, out: &mut dyn std::io::Write) -> Result<()> {
    let hosts = credentials.list_hosts().context("listing hosts")?;
    if json {
        let rows: Vec<_> = hosts.iter().map(row_json).collect();
        writeln!(out, "{}", serde_json::json!({ "hosts": rows }))?;
        return Ok(());
    }
    if hosts.is_empty() {
        writeln!(out, "no hosts authorized")?;
        return Ok(());
    }
    let now = SystemTime::now();
    for host in &hosts {
        let certificate = match &host.current {
            Some(cert) => match cert.valid_until.duration_since(now) {
                Ok(left) => format!("certificate valid for {}", age(left)),
                Err(_) => "certificate expired".to_owned(),
            },
            None => "no certificate".to_owned(),
        };
        let token = host
            .token_expires_at
            .and_then(|at| at.duration_since(now).ok())
            .map_or(String::new(), |left| format!(", token expires in {}", age(left)));
        writeln!(
            out,
            "{}  {}  {certificate}{token}",
            host.host_id,
            state_name(host.state)
        )?;
    }
    Ok(())
}

pub(crate) fn revoke(
    credentials: &HostCredentials,
    host_id: &str,
    reason: &str,
    json: bool,
    out: &mut dyn std::io::Write,
) -> Result<()> {
    match credentials.revoke(host_id, ACTOR, reason) {
        Ok(()) => {}
        Err(CredentialError::UnknownHost) => {
            bail!("host {host_id} was never authorized; `bossctl enrollment list` shows the known hosts")
        }
        Err(error) => return Err(error).with_context(|| format!("revoking {host_id}")),
    }
    if json {
        writeln!(out, "{}", serde_json::json!({ "host_id": host_id, "state": "revoked" }))?;
    } else {
        writeln!(
            out,
            "revoked {host_id}: its stream closes within seconds and it cannot reconnect, renew or enroll again until \
             `bossctl enrollment token {host_id} --reauthorize`. Workers already running on the host are not stopped."
        )?;
    }
    Ok(())
}

fn row_json(host: &HostSummary) -> serde_json::Value {
    serde_json::json!({
        "host_id": host.host_id,
        "state": state_name(host.state),
        "certificate_serial": host.current.as_ref().map(|c| c.serial.clone()),
        "certificate_valid_until": host.current.as_ref().map(|c| unix_secs(c.valid_until)),
        "token_expires_at": host.token_expires_at.map(unix_secs),
    })
}

fn state_name(state: HostState) -> &'static str {
    match state {
        HostState::Pending => "pending",
        HostState::Enrolled => "enrolled",
        HostState::Revoked => "revoked",
    }
}

fn unix_secs(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn age(span: Duration) -> String {
    let secs = span.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

#[cfg(test)]
#[path = "enrollment_tests.rs"]
mod tests;
