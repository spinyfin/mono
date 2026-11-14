//! Manual host enrollment: from a hub-minted token to a running, checked
//! service.
//!
//! This is the step between "the signed package is installed" ([`crate::install`])
//! and "the hub may schedule work here". A person runs `boss-spoke-package
//! enroll` as root on the host with the single-use token the hub operator gave
//! them. In order, [`bootstrap`]:
//!
//! 1. confirms the package is installed and the service registered;
//! 2. hands the token to the service user through an owner-only file in its
//!    credentials directory (never argv);
//! 3. runs the enroll step **as the service user** ([`enroll_inline`]): it
//!    generates the key on the host, submits the CSR through the single-use
//!    `Enroll` call, and installs `key.pem` then `cert.pem`;
//! 4. removes the handed-over token file, whatever happened;
//! 5. starts the service through the launchd/systemd definition the package
//!    installed, and waits for it to stay up;
//! 6. runs the as-user preflight in the installed service environment.
//!
//! Every failure is a [`Failure`] with a stable code and exit status. A failed
//! preflight is reported as one and the host stays unschedulable: the daemon
//! reports its own failing checks to the hub as `failed_preflights`, and
//! nothing here records, claims or writes "ready". There is no SSH, and no
//! fallback to anything but the hub's `Enroll` call.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use std::time::{Duration, Instant};

use boss_protocol_spoke::enrollment_token;
use boss_protocol_spoke::enrollment_token::MANUAL_JOB_ID;
use boss_spoke_credentials::{ClientError, EnrollParams, SpokeCredentials, enrollment_client};

use crate::archive::INSTALLER_BINARY;
use crate::install::{self, Health, Host, InstallRecord, Layout};

/// The owner-only file, inside the credentials directory, the token is handed
/// to the service user in.
pub const TOKEN_FILE: &str = "enroll-token";

/// Names the identity being replaced, from the moment a replacement starts
/// until its files are published. Its presence beside a different stored
/// identity is the evidence of an interrupted replacement.
pub const REPLACING_MARKER: &str = "replacing-identity";

/// Longest token file or stdin we will read; a token is 43 characters.
pub const MAX_TOKEN_TEXT: u64 = 1024;

/// Why enrollment did not finish. Each has a stable [`code`](Failure::code) and
/// [`exit_code`](Failure::exit_code); none is ever a success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The package is not installed here, or its pieces are missing.
    NotInstalled(String),
    /// The token is not a well-formed enrollment token. Never echoes the token.
    TokenInvalid(String),
    /// This host already holds an identity and `--replace-identity` was not given.
    AlreadyEnrolled,
    /// The hub could not be reached. The token was not spent: run enroll again.
    HubUnreachable(String),
    /// The hub refused the token or the request. Mint a new token.
    EnrollmentRefused(String),
    /// The credentials directory could not be used.
    CredentialsUnwritable(String),
    /// The host enrolled but its service did not start or stay up.
    ServiceNotRunning(String),
    /// The host enrolled and its service runs, but the as-user checks failed.
    /// The host stays unschedulable until they pass.
    PreflightFailed { failed: Vec<String>, output: String },
    /// The enroll step failed in a way it did not describe.
    Unexpected(String),
}

impl Failure {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotInstalled(_) => "not_installed",
            Self::TokenInvalid(_) => "token_invalid",
            Self::AlreadyEnrolled => "already_enrolled",
            Self::HubUnreachable(_) => "hub_unreachable",
            Self::EnrollmentRefused(_) => "enrollment_refused",
            Self::CredentialsUnwritable(_) => "credentials_unwritable",
            Self::ServiceNotRunning(_) => "service_not_running",
            Self::PreflightFailed { .. } => "preflight_failed",
            Self::Unexpected(_) => "enrollment_failed",
        }
    }

    /// The process exit status for this failure.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::NotInstalled(_) => 10,
            Self::TokenInvalid(_) => 11,
            Self::AlreadyEnrolled => 12,
            Self::HubUnreachable(_) => 13,
            Self::EnrollmentRefused(_) => 14,
            Self::CredentialsUnwritable(_) => 15,
            Self::ServiceNotRunning(_) => 16,
            Self::PreflightFailed { .. } => 17,
            Self::Unexpected(_) => 18,
        }
    }

    /// Whether the token is spent or dead after this failure. When it is not,
    /// the person can run enroll again with the same token.
    pub fn token_spent(&self) -> bool {
        matches!(
            self,
            Self::EnrollmentRefused(_) | Self::ServiceNotRunning(_) | Self::PreflightFailed { .. }
        )
    }

    pub fn detail(&self) -> String {
        match self {
            Self::NotInstalled(text)
            | Self::TokenInvalid(text)
            | Self::HubUnreachable(text)
            | Self::EnrollmentRefused(text)
            | Self::CredentialsUnwritable(text)
            | Self::ServiceNotRunning(text)
            | Self::Unexpected(text) => text.clone(),
            Self::AlreadyEnrolled => {
                "this host already has an identity; to replace it, mint a new token and pass --replace-identity"
                    .to_owned()
            }
            Self::PreflightFailed { failed, .. } => format!("checks failed: {}", failed.join(", ")),
        }
    }

    /// The one line the as-user enroll step prints for a failure.
    pub fn to_line(&self) -> String {
        format!("failure: {}: {}", self.code(), self.detail().replace('\n', " "))
    }

    /// Reads a [`to_line`](Self::to_line) back out of the enroll step's output.
    fn from_output(output: &str) -> Self {
        for line in output.lines() {
            let Some(rest) = line.strip_prefix("failure: ") else {
                continue;
            };
            let (code, detail) = rest.split_once(": ").unwrap_or((rest, ""));
            let detail = detail.to_owned();
            return match code {
                "not_installed" => Self::NotInstalled(detail),
                "token_invalid" => Self::TokenInvalid(detail),
                "already_enrolled" => Self::AlreadyEnrolled,
                "hub_unreachable" => Self::HubUnreachable(detail),
                "enrollment_refused" => Self::EnrollmentRefused(detail),
                "credentials_unwritable" => Self::CredentialsUnwritable(detail),
                _ => Self::Unexpected(detail),
            };
        }
        Self::Unexpected(format!("the enroll step failed without a reason: {}", output.trim()))
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.detail())
    }
}

impl std::error::Error for Failure {}

/// Decodes an enrollment token's text form (URL-safe base64 of 32 bytes,
/// surrounding whitespace ignored). The hub's `EnrollmentToken::encode` is the
/// other half.
pub fn decode_token(text: &str) -> Result<Vec<u8>, Failure> {
    enrollment_token::decode_token(text)
        .map(|bytes| bytes.to_vec())
        .ok_or_else(|| {
            Failure::TokenInvalid(
                "not an enrollment token: it must be the 43 characters `bossctl enrollment token` printed".to_owned(),
            )
        })
}

/// Loads the install record the host was provisioned with.
pub fn load_record(config: &Path) -> Result<InstallRecord, Failure> {
    InstallRecord::read(config).map_err(|error| Failure::NotInstalled(format!("{error:#}")))
}

// ---------------------------------------------------------------------------
// As the service user.

/// What the as-user enroll step needs.
#[derive(Debug, Clone, Copy)]
pub struct InlineEnroll<'a> {
    pub state_dir: &'a Path,
    pub hub_url: &'a str,
    pub host_id: &'a str,
    pub replace_identity: bool,
}

/// The identity the host now holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrolled {
    pub serial: String,
    pub expires_unix_ms: u64,
}

impl Enrolled {
    /// The one line the as-user enroll step prints for success.
    pub fn to_line(&self) -> String {
        format!(
            "enrolled serial={} expires_unix_ms={}",
            self.serial, self.expires_unix_ms
        )
    }

    fn from_output(output: &str) -> Option<Self> {
        let rest = output.lines().find_map(|line| line.strip_prefix("enrolled "))?;
        let field = |name: &str| {
            rest.split_whitespace()
                .find_map(|part| part.strip_prefix(name))
                .map(str::to_owned)
        };
        Some(Self {
            serial: field("serial=")?,
            expires_unix_ms: field("expires_unix_ms=")?.parse().ok()?,
        })
    }
}

/// Enrolls this host with the hub and installs its identity. Runs as the
/// service user: the key is made here and never leaves, and every file lands in
/// the owner-only credentials directory.
///
/// Safe to repeat. An identity already installed without the daemon's files (a
/// crash between the two) is finished, not replaced, and spends no token. The
/// token file is removed once it has been used.
pub fn enroll_inline(args: &InlineEnroll<'_>) -> Result<Enrolled, Failure> {
    let dir = boss_spoke_preflight::credentials_dir(args.state_dir);
    let credentials = SpokeCredentials::open(&dir).map_err(unwritable)?;
    let existing = credentials.identity().map_err(unwritable)?;
    let marker_path = dir.join(REPLACING_MARKER);
    if let Some(identity) = &existing {
        if !credentials.daemon_files_present() {
            // Without --replace-identity, a stored identity without its files is
            // a crash between storing and publishing: finish it. Under
            // --replace-identity that is only so when the marker left by an
            // earlier replacement names a different identity than the stored
            // one, which proves the new identity was stored. Otherwise the
            // daemon files were lost and the fresh token must be spent.
            let interrupted = std::fs::read_to_string(&marker_path).is_ok_and(|old| old.trim() != identity.serial);
            if !args.replace_identity || interrupted {
                let enrolled = publish(&credentials)?;
                let _ = std::fs::remove_file(&marker_path);
                return Ok(enrolled);
            }
        } else if !args.replace_identity {
            return Err(Failure::AlreadyEnrolled);
        }
        if args.replace_identity {
            std::fs::write(&marker_path, &identity.serial)
                .map_err(|error| Failure::CredentialsUnwritable(error.to_string()))?;
        }
    }
    let token_path = dir.join(TOKEN_FILE);
    let token = std::fs::read_to_string(&token_path)
        .map_err(|error| Failure::TokenInvalid(format!("no enrollment token at {}: {error}", token_path.display())))
        .and_then(|text| decode_token(&text))?;
    let ca = std::fs::read(dir.join("ca.pem")).map_err(|error| {
        Failure::NotInstalled(format!(
            "the hub trust anchor {} is missing: {error}",
            dir.join("ca.pem").display()
        ))
    })?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| Failure::Unexpected(format!("cannot start a runtime: {error}")))?;
    runtime
        .block_on(async {
            let exchange = enrollment_client(args.hub_url, &ca)?;
            credentials
                .enroll(
                    &exchange,
                    &EnrollParams {
                        host_id: args.host_id,
                        job_id: MANUAL_JOB_ID,
                        token: &token,
                    },
                )
                .await
        })
        .map_err(client_failure)?;
    let enrolled = publish(&credentials)?;
    let _ = std::fs::remove_file(&marker_path);
    let _ = std::fs::remove_file(&token_path);
    Ok(enrolled)
}

fn publish(credentials: &SpokeCredentials) -> Result<Enrolled, Failure> {
    let identity = credentials
        .publish_daemon_files()
        .map_err(unwritable)?
        .ok_or_else(|| Failure::Unexpected("enrollment finished without an identity".to_owned()))?;
    let expires = identity
        .expires_at
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    Ok(Enrolled {
        serial: identity.serial,
        expires_unix_ms: expires,
    })
}

fn unwritable(error: ClientError) -> Failure {
    Failure::CredentialsUnwritable(error.to_string())
}

fn client_failure(error: ClientError) -> Failure {
    match error {
        ClientError::Unavailable { code, message } => Failure::HubUnreachable(format!(
            "{code:?}: {message}. Check the hub address, that the hub is up, and that ca.pem is the CA of the hub's TLS certificate"
        )),
        ClientError::Refused { code, message } => Failure::EnrollmentRefused(format!(
            "{code:?}: {message}. The token may be spent, expired or for another host; mint a new one"
        )),
        ClientError::BadCertificate(why) => Failure::EnrollmentRefused(why.to_owned()),
        ClientError::Endpoint(why) => Failure::NotInstalled(format!("the hub address or its CA is unusable: {why}")),
        other => unwritable(other),
    }
}

// ---------------------------------------------------------------------------
// As root.

/// Everything [`bootstrap`] needs besides the host.
#[derive(bon::Builder)]
pub struct Options<'a> {
    /// The install record (`/etc/boss-spoke/install.json`).
    pub config: &'a Path,
    /// Stage against this directory instead of `/`. Tests and inspection only.
    pub root: Option<PathBuf>,
    /// The token's text form.
    pub token: &'a str,
    /// Enroll again over an existing identity.
    #[builder(default)]
    pub replace_identity: bool,
    /// Repositories (URLs) the preflight must be able to read.
    #[builder(default)]
    pub repos: &'a [String],
    /// How long the started service has to stay up.
    #[builder(default)]
    pub health: Health,
    /// The installer to run as the service user when `current/` has none.
    pub fallback_installer: &'a Path,
}

/// What a finished bootstrap established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub host_id: String,
    pub serial: String,
    pub expires_unix_ms: u64,
    /// The preflight's `PASS` lines.
    pub preflight_output: String,
}

/// Enrolls the host, starts its service and checks it as the service user.
/// `Ok` means the checks passed; it still does not make the host schedulable,
/// which requires separate hub registration and scheduling support.
pub fn bootstrap(host: &dyn Host, options: &Options<'_>) -> Result<Report, Failure> {
    let record = load_record(options.config)?;
    let mut layout = record
        .layout()
        .map_err(|error| Failure::NotInstalled(format!("{error:#}")))?;
    layout.root = options.root.clone();
    let account = host
        .lookup_user(&layout.user)
        .map_err(|error| Failure::NotInstalled(format!("{error:#}")))?
        .ok_or_else(|| Failure::NotInstalled(format!("service user {} does not exist", layout.user)))?;
    if !host
        .service_registered()
        .map_err(|error| Failure::NotInstalled(format!("{error:#}")))?
    {
        return Err(Failure::NotInstalled(format!(
            "the spoke service is not registered; run `{} install` first",
            layout.on_disk(&layout.current()).join(INSTALLER_BINARY).display()
        )));
    }
    decode_token(options.token)?;
    let credentials = layout.on_disk(&layout.credentials_dir());
    if !credentials.join("ca.pem").is_file() {
        return Err(Failure::NotInstalled(format!(
            "{} is missing; run `{} install` first",
            credentials.join("ca.pem").display(),
            layout.on_disk(&layout.current()).join(INSTALLER_BINARY).display()
        )));
    }

    let program = {
        let installed = layout.on_disk(&layout.current()).join(INSTALLER_BINARY);
        if installed.is_file() {
            installed
        } else {
            options.fallback_installer.to_path_buf()
        }
    };
    let spec = layout.spec(&record.host_id, &record.hub_url);
    let mut args = vec![
        "enroll".to_owned(),
        "--inline".into(),
        "--state-dir".into(),
        layout.on_disk(&layout.state_dir()).display().to_string(),
        "--hub-url".into(),
        record.hub_url.clone(),
        "--host-id".into(),
        record.host_id.clone(),
    ];
    if options.replace_identity {
        args.push("--replace-identity".into());
    }

    install::write_credential_file(&layout, &account, TOKEN_FILE, options.token.trim().as_bytes())
        .map_err(|error| Failure::CredentialsUnwritable(format!("{error:#}")))?;
    let ran = host.run_as(&account, &layout.disk_environment(&spec), &program, &args);
    // The token is spent or not needed whatever happened; the person keeps
    // their own copy for a retry. A copy left behind would outlive its use.
    let removed = install::remove_credential_file(&layout, &account, TOKEN_FILE);
    let ran = ran.map_err(|error| Failure::Unexpected(format!("{error:#}")))?;
    removed.map_err(|error| Failure::CredentialsUnwritable(format!("{error:#}")))?;
    if !ran.success {
        return Err(Failure::from_output(&ran.output));
    }
    let enrolled = Enrolled::from_output(&ran.output).ok_or_else(|| {
        Failure::Unexpected(format!(
            "the enroll step did not report an identity: {}",
            ran.output.trim()
        ))
    })?;

    // Repositories persist across enrollments; only a non-empty list replaces
    // them, so certificate maintenance without `--repo` keeps the checks.
    if !options.repos.is_empty() {
        install::write_credential_file(
            &layout,
            &account,
            boss_spoke_preflight::REPOS_FILE,
            options.repos.join("\0").as_bytes(),
        )
        .map_err(|error| Failure::CredentialsUnwritable(format!("{error:#}")))?;
    }
    install::write_credential_file(
        &layout,
        &account,
        boss_spoke_preflight::BOOTSTRAP_PENDING_FILE,
        b"pending",
    )
    .map_err(|error| Failure::CredentialsUnwritable(format!("{error:#}")))?;
    host.restart_service()
        .map_err(|error| Failure::ServiceNotRunning(format!("{error:#}")))?;
    wait_running(host, &options.health, &layout)?;

    let run = preflight_as_service_user(host, &record, &layout, false, options.repos, options.fallback_installer)
        .map_err(|error| Failure::PreflightFailed {
            failed: vec!["preflight_run".to_owned()],
            output: format!("{error:#}"),
        })?;
    if !run.success {
        let failed = if run.failed.is_empty() {
            vec!["preflight_run".to_owned()]
        } else {
            run.failed
        };
        return Err(Failure::PreflightFailed {
            failed,
            output: run.output,
        });
    }
    Ok(Report {
        host_id: record.host_id,
        serial: enrolled.serial,
        expires_unix_ms: enrolled.expires_unix_ms,
        preflight_output: run.output,
    })
}

/// Waits for the started service to hold one instance for `health.stable`.
fn wait_running(host: &dyn Host, health: &Health, layout: &Layout) -> Result<(), Failure> {
    let started = Instant::now();
    let failed = |error: anyhow::Error| Failure::ServiceNotRunning(format!("{error:#}"));
    loop {
        if let Some(instance) = host.service_instance().map_err(failed)? {
            std::thread::sleep(health.stable);
            if host.service_instance().map_err(failed)? == Some(instance) {
                return Ok(());
            }
        }
        if started.elapsed() >= health.deadline {
            return Err(Failure::ServiceNotRunning(format!(
                "the {} service did not stay running within {:?} of starting. The host is enrolled; read the service \
                 log, fix the cause, and restart the service",
                if layout.platform.is_macos() {
                    "launchd"
                } else {
                    "systemd"
                },
                health.deadline
            )));
        }
        std::thread::sleep(health.poll.max(Duration::from_millis(1)));
    }
}

// ---------------------------------------------------------------------------
// The as-user preflight.
//
// The checks themselves live in `boss-spoke-preflight`. Running them as root or
// in an administrator's shell would prove nothing about what the daemon will
// see: its `PATH`, its `HOME`, its pinned driver-home root and its credentials.

/// What one as-user run reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightRun {
    pub success: bool,
    /// One `PASS`/`FAIL` line per check, then `failed_preflights: …` on failure.
    pub output: String,
    /// The names of the checks that failed, empty on success.
    pub failed: Vec<String>,
}

/// Re-runs the **installed** `boss-spoke-package` as the service user with
/// exactly the installed service environment. `fallback` is used only when
/// `current/` holds no installer yet.
pub fn preflight_as_service_user(
    host: &dyn Host,
    record: &InstallRecord,
    layout: &Layout,
    storage_only: bool,
    repos: &[String],
    fallback: &Path,
) -> anyhow::Result<PreflightRun> {
    let spec = layout.spec(&record.host_id, &record.hub_url);
    let account = host
        .lookup_user(&layout.user)?
        .with_context(|| format!("service user {} does not exist", layout.user))?;
    let mut args = vec![
        "preflight".to_owned(),
        "--inline".into(),
        "--state-dir".into(),
        layout.on_disk(&layout.state_dir()).display().to_string(),
        "--hub-url".into(),
        record.hub_url.clone(),
    ];
    if storage_only {
        args.push("--storage-only".into());
    }
    for repo in repos {
        args.extend(["--repo".to_owned(), repo.clone()]);
    }
    let installed = layout.on_disk(&layout.current()).join(INSTALLER_BINARY);
    let program = if installed.is_file() {
        installed.as_path()
    } else {
        fallback
    };
    let output = host
        .run_as(&account, &layout.disk_environment(&spec), program, &args)
        .context("run preflight as the service user")?;
    if output.success && !storage_only {
        install::remove_credential_file(layout, &account, boss_spoke_preflight::BOOTSTRAP_PENDING_FILE)?;
    }
    let failed = failed_names(&output.output);
    Ok(PreflightRun {
        success: output.success,
        output: output.output,
        failed,
    })
}

/// The check names on the `failed_preflights:` line of a run's output.
pub fn failed_names(output: &str) -> Vec<String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("failed_preflights:"))
        .map(|names| {
            names
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_survives_the_line_the_enroll_step_prints() {
        for failure in [
            Failure::NotInstalled("no record".into()),
            Failure::TokenInvalid("bad".into()),
            Failure::AlreadyEnrolled,
            Failure::HubUnreachable("down".into()),
            Failure::EnrollmentRefused("spent".into()),
            Failure::CredentialsUnwritable("read-only".into()),
        ] {
            let printed = format!("noise\n{}\nmore noise", failure.to_line());
            assert_eq!(Failure::from_output(&printed).code(), failure.code(), "{failure}");
        }
    }

    #[test]
    fn output_with_no_failure_line_is_unexpected_not_a_success() {
        assert_eq!(Failure::from_output("segfault").code(), "enrollment_failed");
    }

    #[test]
    fn only_a_32_byte_url_safe_token_decodes() {
        let good = enrollment_token::encode_token(&[7u8; 32]);
        assert_eq!(decode_token(&format!("{good}\n")).expect("decodes"), vec![7u8; 32]);
        for bad in ["", "not base64 !", "AAAA"] {
            assert_eq!(decode_token(bad).expect_err("rejected").code(), "token_invalid");
        }
    }

    #[test]
    fn the_enrolled_line_round_trips() {
        let enrolled = Enrolled {
            serial: "ab12".into(),
            expires_unix_ms: 1_700_000_000_123,
        };
        assert_eq!(
            Enrolled::from_output(&format!("x\n{}\n", enrolled.to_line())),
            Some(enrolled)
        );
        assert_eq!(Enrolled::from_output("nothing"), None);
    }

    #[test]
    fn every_failure_has_its_own_exit_status() {
        let all = [
            Failure::NotInstalled(String::new()),
            Failure::TokenInvalid(String::new()),
            Failure::AlreadyEnrolled,
            Failure::HubUnreachable(String::new()),
            Failure::EnrollmentRefused(String::new()),
            Failure::CredentialsUnwritable(String::new()),
            Failure::ServiceNotRunning(String::new()),
            Failure::PreflightFailed {
                failed: vec![],
                output: String::new(),
            },
            Failure::Unexpected(String::new()),
        ];
        let mut codes: Vec<u8> = all.iter().map(Failure::exit_code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), all.len());
        assert!(
            codes.iter().all(|code| *code >= 10),
            "below 10 stays free for usage errors"
        );
    }

    #[test]
    fn failed_checks_are_read_from_the_summary_line() {
        let output = "PASS tmux: 3.4\nFAIL python_pty: no termios\nfailed_preflights: python_pty, claude_credentials\n";
        assert_eq!(failed_names(output), ["python_pty", "claude_credentials"]);
        assert!(failed_names("PASS tmux: 3.4\n").is_empty());
    }
}
