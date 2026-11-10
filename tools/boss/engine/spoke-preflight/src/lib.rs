//! Host readiness checks, run as the spoke's service user.
//!
//! A host is only worth scheduling when workers started there can actually run:
//! private storage, the pinned private-home root, the tools a worker shells out
//! to, authenticated driver CLIs, an authenticated GitHub login, a sane clock
//! and room on disk. Every
//! check has a stable name. The daemon publishes the names of failing checks as
//! `Inventory.failed_preflights`, and the installer's `preflight` command prints
//! them for the person setting the host up. Enrollment alone never makes a host
//! ready (hub-and-spoke design, "Enrollment, service user and credential
//! lifecycle").
//!
//! The checks must run in the installed service environment (the service user's
//! `HOME`, `PATH` and `BOSS_CODEX_HOMES_DIR`), not an administrator's shell, so
//! nothing here reads the environment itself: the caller passes the pinned home
//! root in [`Config::homes_root`]. Credential probes never record the probe's
//! output, only whether it succeeded.

use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use boss_host_execution::homes;
use boss_tmux::{MINIMUM_VERSION, Tmux};
use tokio::net::TcpStream;
use tokio::process::Command;

/// The variable the drivers read their per-run home root from. A service
/// definition pins it inside owner-only service state; left unset it would fall
/// back to a shared temp directory.
pub const HOMES_ENV: &str = homes::ROOT_ENV;

const GIB: u64 = 1024 * 1024 * 1024;

/// The 8 GiB spool plus room for workspaces and build caches. Free disk is a
/// hard admission guard (design, "Capacity, clocks and observability").
pub const MIN_FREE_DISK_BYTES: u64 = 16 * GIB;

/// 2026-01-01T00:00:00Z. A clock before the first release of this software is
/// wrong, and TLS certificate validation would reject the hub because of it.
pub const CLOCK_FLOOR_UNIX_SECS: u64 = 1_767_225_600;

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// What to check. `homes_root` is the value of `BOSS_CODEX_HOMES_DIR` as the
/// process sees it.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct Config {
    pub state_dir: PathBuf,
    pub homes_root: Option<PathBuf>,
    #[builder(default = PathBuf::from("cube"))]
    pub cube_bin: PathBuf,
    pub tmux_bin: Option<PathBuf>,
    /// `host:port` of the hub, for a reachability check. Absent skips it.
    pub hub: Option<String>,
    /// Repositories (URLs) the host must be able to read.
    #[builder(default)]
    pub repos: Vec<String>,
    /// Driver CLIs. Defaults match how workers invoke them off `PATH`.
    #[builder(default = PathBuf::from("claude"))]
    pub claude_bin: PathBuf,
    #[builder(default = PathBuf::from("codex"))]
    pub codex_bin: PathBuf,
}

/// One check's result. `Ok` carries a short non-secret description of what was
/// verified; `Err` says what is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub name: &'static str,
    pub result: Result<String, String>,
}

impl Outcome {
    pub fn passed(&self) -> bool {
        self.result.is_ok()
    }

    fn of(name: &'static str, result: Result<String, String>) -> Self {
        Self { name, result }
    }
}

/// The names of the checks that failed, in check order.
pub fn failed(outcomes: &[Outcome]) -> Vec<String> {
    outcomes
        .iter()
        .filter(|outcome| !outcome.passed())
        .map(|outcome| outcome.name.to_owned())
        .collect()
}

/// Only the checks that read the filesystem: private storage and the pinned
/// home root. The installer runs these as the service user before it finishes.
pub fn run_storage(config: &Config) -> Vec<Outcome> {
    vec![
        private_storage(&config.state_dir),
        private_homes(config.homes_root.as_deref(), &config.state_dir),
    ]
}

/// Every check. Probes run one after another, each bounded by [`PROBE_TIMEOUT`].
pub async fn run(config: &Config) -> Vec<Outcome> {
    let mut outcomes = run_storage(config);
    outcomes.push(disk_space(&config.state_dir, MIN_FREE_DISK_BYTES));
    outcomes.push(clock(SystemTime::now()));
    outcomes.push(tmux(config.tmux_bin.as_deref(), &config.state_dir.join("tmux.sock")).await);
    outcomes.push(tool("tool_cube", config.cube_bin.as_os_str()).await);
    for (name, program) in [("tool_jj", "jj"), ("tool_git", "git"), ("tool_gh", "gh")] {
        outcomes.push(tool(name, OsStr::new(program)).await);
    }
    outcomes.push(tool("tool_claude", config.claude_bin.as_os_str()).await);
    outcomes.push(tool("tool_codex", config.codex_bin.as_os_str()).await);
    outcomes.push(python_pty().await);
    outcomes.push(github_credentials().await);
    outcomes.push(claude_credentials(&config.claude_bin).await);
    outcomes.push(codex_credentials(&config.codex_bin).await);
    if let Some(hub) = &config.hub {
        outcomes.push(hub_reachable(hub).await);
    }
    if !config.repos.is_empty() {
        outcomes.push(repo_access(&config.repos).await);
    }
    outcomes
}

/// The state directory (and its credential directory, once enrolled) belong to
/// the service user and nobody else.
pub fn private_storage(state_dir: &Path) -> Outcome {
    let result = (|| {
        for dir in [state_dir.to_path_buf(), state_dir.join("credentials")] {
            let metadata = match std::fs::symlink_metadata(&dir) {
                Ok(metadata) => metadata,
                // Not enrolled yet: there is no credential directory to protect.
                Err(error) if dir != state_dir && error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("{} is unreadable: {error}", dir.display())),
            };
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!("{} must be a real directory", dir.display()));
            }
            if metadata.mode() & 0o077 != 0 {
                return Err(format!("{} must be owner-only (0700)", dir.display()));
            }
            // SAFETY: geteuid has no preconditions and cannot fail.
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(format!("{} is not owned by the service user", dir.display()));
            }
        }
        Ok("state directory is owner-only and owned by the service user".to_owned())
    })();
    Outcome::of("private_storage", result)
}

/// `BOSS_CODEX_HOMES_DIR` must be set (an unset variable would fall back to a
/// shared temp directory), sit inside the service's own state, and be a private
/// directory the service user can write.
pub fn private_homes(homes_root: Option<&Path>, state_dir: &Path) -> Outcome {
    let result = (|| {
        let root = homes::preflight(homes_root)
            .map_err(|error| format!("{}: {error}; refusing the shared-temp default", homes::ROOT_ENV))?;
        let inside_state = std::fs::canonicalize(&root)
            .and_then(|root| std::fs::canonicalize(state_dir.join("homes")).map(|homes| root == homes))
            .map_err(|error| format!("{} cannot be resolved: {error}", homes::ROOT_ENV))?;
        if !inside_state {
            return Err(format!(
                "{} must be the homes directory of the service state ({})",
                homes::ROOT_ENV,
                state_dir.join("homes").display()
            ));
        }
        Ok("private-home root is pinned in service state, owner-only and writable".to_owned())
    })();
    Outcome::of("private_homes", result)
}

pub fn disk_space(state_dir: &Path, minimum: u64) -> Outcome {
    let result = match fs4::available_space(state_dir) {
        Ok(free) if free >= minimum => Ok(format!("{} GiB free", free / GIB)),
        Ok(free) => Err(format!(
            "{} GiB free; at least {} GiB is required",
            free / GIB,
            minimum / GIB
        )),
        Err(error) => Err(format!("free space is unreadable: {error}")),
    };
    Outcome::of("disk_space", result)
}

pub fn clock(now: SystemTime) -> Outcome {
    let result = match now.duration_since(UNIX_EPOCH) {
        Ok(since) if since.as_secs() >= CLOCK_FLOOR_UNIX_SECS => Ok("wall clock is plausible".to_owned()),
        _ => Err("wall clock is before 2026-01-01; certificate validation would fail".to_owned()),
    };
    Outcome::of("clock", result)
}

async fn tmux(program: Option<&Path>, socket: &Path) -> Outcome {
    let result = async {
        let tmux = match program {
            Some(path) => Tmux::from_path_with_socket(path, socket),
            None => Tmux::resolve(socket),
        }
        .map_err(|error| error.to_string())?;
        let version = tokio::time::timeout(PROBE_TIMEOUT, tmux.version())
            .await
            .map_err(|_| "tmux -V timed out".to_owned())?
            .map_err(|error| error.to_string())?;
        if version < MINIMUM_VERSION {
            return Err(format!(
                "tmux {}.{} is older than the required {}.{}",
                version.major, version.minor, MINIMUM_VERSION.major, MINIMUM_VERSION.minor
            ));
        }
        Ok(format!("tmux {}.{}", version.major, version.minor))
    }
    .await;
    Outcome::of("tmux", result)
}

async fn tool(name: &'static str, program: &OsStr) -> Outcome {
    Outcome::of(name, probe(program, &["--version"], &[], &[]).await)
}

/// Workers talk to their driver through the Python PTY relay.
async fn python_pty() -> Outcome {
    let result = probe(OsStr::new("python3"), &["-c", "import fcntl, pty, termios"], &[], &[]).await;
    Outcome::of(
        "python_pty",
        result.map(|_| "python3 has fcntl, pty and termios".to_owned()),
    )
}

/// Whether `gh` holds a working login. The probe's output names accounts and
/// token prefixes, so it is dropped.
async fn github_credentials() -> Outcome {
    let result = probe(
        OsStr::new("gh"),
        &["auth", "status"],
        &[("GH_PROMPT_DISABLED", "1")],
        &[],
    )
    .await;
    Outcome::of(
        "github_credentials",
        result
            .map(|_| "gh is authenticated".to_owned())
            .map_err(|_| "gh is not authenticated for the service user".to_owned()),
    )
}

/// Same non-interactive `/usage` print-mode probe as `ClaudeQuotaProbe`: no
/// model turn, and the child's output is discarded so a token cannot leak.
async fn claude_credentials(program: &Path) -> Outcome {
    let result = probe(
        program.as_os_str(),
        &["-p", "/usage", "--output-format", "json"],
        &[],
        boss_engine_driver_quota::probes::claude::CLAUDE_UNSET_ENV_VARS,
    )
    .await;
    Outcome::of(
        "claude_credentials",
        result
            .and_then(|output| {
                boss_engine_driver_quota::parse::claude_usage_authenticated(&output)
                    .then(|| "claude is authenticated".to_owned())
                    .ok_or_else(|| "invalid usage response".to_owned())
            })
            .map_err(|_| "claude is not authenticated for the service user".to_owned()),
    )
}

/// Same `account/rateLimits/read` app-server probe as `CodexQuotaProbe`. The
/// handshake matches `codex app-server --listen stdio://` (`initialize`,
/// `notifications/initialized`, then the method). The result payload is
/// dropped: it is a quota figure, not a credential, but it is still not
/// something a preflight line should print.
async fn codex_credentials(program: &Path) -> Outcome {
    codex_credentials_with_timeout(program, PROBE_TIMEOUT).await
}

async fn codex_credentials_with_timeout(program: &Path, timeout: Duration) -> Outcome {
    use boss_engine_codex_hook_trust::{AppServer, AppServerError, ClientInfo};
    let server = AppServer {
        program: program.to_path_buf(),
        extra_env: vec![],
        cwd: Some(PathBuf::from("/")),
        client_info: ClientInfo {
            name: "boss-spoke-preflight".into(),
            title: None,
            version: "1.0.0".into(),
        },
        timeout,
    };
    let result =
        tokio::task::spawn_blocking(move || server.request("account/rateLimits/read", serde_json::json!({}))).await;
    let result = match result {
        Ok(Ok(_)) => Ok("codex is authenticated".to_owned()),
        Ok(Err(AppServerError::NotFound { .. })) => Err("codex is not installed or not on PATH".to_owned()),
        _ => Err("codex is not authenticated for the service user".to_owned()),
    };
    Outcome::of("codex_credentials", result)
}

async fn hub_reachable(authority: &str) -> Outcome {
    let result = match tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(authority)).await {
        Ok(Ok(_)) => Ok("hub accepts connections".to_owned()),
        Ok(Err(error)) => Err(format!("cannot reach {authority}: {error}")),
        Err(_) => Err(format!("connecting to {authority} timed out")),
    };
    Outcome::of("hub_reachable", result)
}

/// Repository URLs can embed credentials, so a failure names only the count.
async fn repo_access(repos: &[String]) -> Outcome {
    let mut unreadable = 0;
    for repo in repos {
        let env = [("GIT_TERMINAL_PROMPT", "0")];
        if probe(
            OsStr::new("git"),
            &["ls-remote", "--exit-code", repo, "HEAD"],
            &env,
            &[],
        )
        .await
        .is_err()
        {
            unreadable += 1;
        }
    }
    let result = if unreadable == 0 {
        Ok(format!("{} repositories readable", repos.len()))
    } else {
        Err(format!("{unreadable} of {} repositories are not readable", repos.len()))
    };
    Outcome::of("repo_access", result)
}

/// Runs `program`, returning its stdout. Failure carries no
/// process output.
async fn probe(program: &OsStr, args: &[&str], envs: &[(&str, &str)], unset: &[&str]) -> Result<String, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir("/")
        .envs(envs.iter().copied())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    for key in unset {
        command.env_remove(*key);
    }
    let name = Path::new(program).display().to_string();
    match tokio::time::timeout(PROBE_TIMEOUT, command.output()).await {
        Err(_) => Err(format!("{name} timed out")),
        Ok(Err(error)) => Err(format!("{name} cannot run: {error}")),
        Ok(Ok(output)) if output.status.success() => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
        Ok(Ok(output)) => Err(format!("{name} exited with {}", output.status)),
    }
}

/// `host:port` of an `https://` hub URL (443 when it names no port).
pub fn hub_authority(url: &str) -> Option<String> {
    let authority = url.strip_prefix("https://")?.split('/').next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    if authority
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
    {
        Some(authority.to_owned())
    } else {
        Some(format!("{authority}:443"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};

    fn private_dir(path: &Path) {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(path).unwrap();
    }

    fn message(outcome: &Outcome) -> String {
        outcome.result.clone().unwrap_err()
    }

    #[test]
    fn private_storage_accepts_owner_only_state_with_an_owner_only_credential_dir() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("spoke");
        private_dir(&state);
        assert!(private_storage(&state).passed());
        private_dir(&state.join("credentials"));
        assert!(private_storage(&state).passed());
    }

    #[test]
    fn private_storage_rejects_readable_missing_and_symlinked_state() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("spoke");
        private_dir(&state);
        fs::set_permissions(&state, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(message(&private_storage(&state)).contains("owner-only"));

        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let credentials = state.join("credentials");
        private_dir(&credentials);
        fs::set_permissions(&credentials, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(message(&private_storage(&state)).contains("credentials"));

        assert!(!private_storage(&temp.path().join("absent")).passed());
        let link = temp.path().join("link");
        symlink(&state, &link).unwrap();
        assert!(message(&private_storage(&link)).contains("real directory"));
    }

    #[test]
    fn private_homes_requires_the_pinned_root_inside_service_state() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("spoke");
        let homes = state.join("homes");
        private_dir(&homes);

        assert!(private_homes(Some(&homes), &state).passed());

        let unset = message(&private_homes(None, &state));
        assert!(unset.contains("BOSS_CODEX_HOMES_DIR") && unset.contains("shared-temp"));

        let elsewhere = temp.path().join("elsewhere");
        private_dir(&elsewhere);
        assert!(message(&private_homes(Some(&elsewhere), &state)).contains("homes directory of the service state"));

        fs::set_permissions(&homes, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(message(&private_homes(Some(&homes), &state)).contains("owner-only"));
    }

    #[test]
    fn disk_and_clock_report_the_threshold_they_missed() {
        let temp = tempfile::tempdir().unwrap();
        assert!(disk_space(temp.path(), 1).passed());
        assert!(message(&disk_space(temp.path(), u64::MAX)).contains("required"));

        assert!(clock(SystemTime::now()).passed());
        assert!(!clock(UNIX_EPOCH + Duration::from_secs(3600)).passed());
    }

    #[tokio::test]
    async fn probe_reports_stdout_and_never_the_failure_output() {
        let ok = probe(OsStr::new("/bin/sh"), &["-c", "echo first; echo second"], &[], &[]).await;
        assert_eq!(ok.unwrap(), "first\nsecond\n");
        let failed = probe(OsStr::new("/bin/sh"), &["-c", "echo secret >&2; exit 3"], &[], &[])
            .await
            .unwrap_err();
        assert!(failed.contains("exited") && !failed.contains("secret"));
        assert!(
            probe(OsStr::new("/nonexistent/tool"), &[], &[], &[])
                .await
                .unwrap_err()
                .contains("cannot run")
        );
    }

    #[test]
    fn failed_lists_only_failing_check_names() {
        let outcomes = [
            Outcome::of("a", Ok("fine".into())),
            Outcome::of("b", Err("broken".into())),
        ];
        assert_eq!(failed(&outcomes), ["b"]);
    }

    #[test]
    fn hub_authority_defaults_the_port_and_refuses_credentials() {
        assert_eq!(hub_authority("https://hub.example:7443").unwrap(), "hub.example:7443");
        assert_eq!(hub_authority("https://hub.example/path").unwrap(), "hub.example:443");
        assert_eq!(hub_authority("http://hub.example"), None);
        assert_eq!(hub_authority("https://user@hub.example"), None);
    }

    fn stub_script(name: &str, body: &str) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        (temp, path)
    }

    #[tokio::test]
    async fn driver_tools_and_credentials_distinguish_missing_invalid_and_working() {
        let missing = PathBuf::from("/nonexistent/definitely-not-a-driver");
        assert!(!tool("tool_claude", missing.as_os_str()).await.passed());
        assert!(!claude_credentials(&missing).await.passed());
        assert!(!tool("tool_codex", missing.as_os_str()).await.passed());
        assert!(!codex_credentials(&missing).await.passed());

        let (_claude_bad, claude_bad) = stub_script(
            "claude",
            r#"
if [ "$1" = --version ]; then echo claude 1.0; exit 0; fi
echo 'please login' >&2
exit 1
"#,
        );
        assert!(tool("tool_claude", claude_bad.as_os_str()).await.passed());
        let failed = claude_credentials(&claude_bad).await;
        assert!(!failed.passed());
        assert_eq!(failed.name, "claude_credentials");
        assert!(!message(&failed).contains("please login"));

        let (_claude_ok, claude_ok) = stub_script(
            "claude",
            r#"
if [ "$1" = --version ]; then echo claude 1.0; exit 0; fi
echo '{"is_error":false,"result":"Current week (all models): 3% used"}'
"#,
        );
        assert!(claude_credentials(&claude_ok).await.passed());

        let (_codex_bad, codex_bad) = stub_script(
            "codex",
            r#"
if [ "$1" = --version ]; then echo codex 1.0; exit 0; fi
read l1
echo '{"method":"configWarning","params":{}}'
echo '{"id":1,"result":{}}'
read l2
read l3
echo '{"method":"configWarning","params":{}}'
echo '{"id":2,"error":{"message":"not logged in; run login"}}'
"#,
        );
        assert!(tool("tool_codex", codex_bad.as_os_str()).await.passed());
        let failed = codex_credentials(&codex_bad).await;
        assert!(!failed.passed());
        assert_eq!(failed.name, "codex_credentials");
        assert!(!message(&failed).contains("not logged in"));

        let (_codex_ok, codex_ok) = stub_script(
            "codex",
            r#"
if [ "$1" = --version ]; then echo codex 1.0; exit 0; fi
read l1
echo '{"method":"configWarning","params":{}}'
echo '{"id":1,"result":{}}'
read l2
read l3
echo '{"method":"configWarning","params":{}}'
echo '{"id":2,"result":{}}'
"#,
        );
        assert!(codex_credentials(&codex_ok).await.passed());
    }
    #[tokio::test]
    async fn claude_rejects_successful_exit_with_invalid_credentials_or_output() {
        for output in [
            r#"{"is_error":true,"result":"Current week (all models): 3% used"}"#,
            r#"{"is_error":false,"result":"Not logged in · Please run /login"}"#,
            r#"{"is_error":false}"#,
            "malformed",
            "",
        ] {
            let (_temp, program) = stub_script("claude", &format!("echo '{output}'"));
            assert!(!claude_credentials(&program).await.passed(), "{output}");
        }
    }

    #[tokio::test]
    async fn claude_login_does_not_require_a_weekly_quota_reading() {
        for report in [
            "Current session: 3% used",
            "Current week (all models): three percent used",
        ] {
            let output = serde_json::json!({"is_error": false, "result": report});
            let (_temp, program) = stub_script("claude", &format!("echo '{output}'"));
            assert!(claude_credentials(&program).await.passed(), "{report}");
        }
    }

    #[tokio::test]
    async fn stalled_codex_is_bounded_and_reaped() {
        let (temp, program) = stub_script(
            "codex",
            r#"[ "$1" = --version ] && exit 0
echo $$ > "$0.pid"; read request; read request; read request"#,
        );
        // Warm the freshly written executable before timing the stall: macOS
        // first-exec scanning can otherwise consume the entire probe deadline.
        assert!(tool("tool_codex", program.as_os_str()).await.passed());
        // Cleanup must still finish within 3s and leave no live child.
        let started = std::time::Instant::now();
        assert!(
            !codex_credentials_with_timeout(&program, Duration::from_secs(1))
                .await
                .passed()
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        let pid: libc::pid_t = fs::read_to_string(temp.path().join("codex.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal zero checks existence without delivering a signal.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
}
