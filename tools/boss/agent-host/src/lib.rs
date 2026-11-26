//! `boss-agent-host`: keeps a line-speaking child alive and reachable.
//!
//! The host is the process a tmux pane runs. It starts a child with piped
//! stdio and relays newline-delimited lines between the child and at most one
//! client on a Unix socket. Everything either side says is appended to a
//! journal, so a client that dies and comes back, or a different process that
//! takes over, can ask for what it missed and continue.
//!
//! The host knows nothing about what the lines mean. It does not parse them,
//! frame them or answer for the child.
//!
//! # Wire contract
//!
//! This is the host's whole surface. A host outlives engine updates, so a
//! change to any of it has to be one a client of the previous version can
//! still use.
//!
//! - A client connects and sends one line, `{"replay_from":N}` (see
//!   [`Handshake`]). A connection that sends anything else, or nothing for a
//!   few seconds, is closed and displaces nobody.
//! - The host answers with journal records (see [`journal`]), one JSON object
//!   per line, starting at sequence number `N`, then every later record as it
//!   is written. Records for both directions arrive, including the client's
//!   own lines, so a client's view is the journal's.
//! - After the handshake every line the client sends is a line for the child.
//!   A line without its terminator, cut off by the client dying, is dropped.
//! - One client at a time. A connection that completes its handshake closes
//!   the one before it, whether or not the older one was still alive.
//! - The child's stderr is the host's stderr: a person attached to the pane
//!   sees it. It is not journalled.
//! - When the child exits, the host delivers what is queued to the client,
//!   removes its socket and exits with the child's status (`128 + signal`
//!   when a signal ended it).

pub mod journal;
mod relay;

use std::ffi::OsString;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::signal::unix::{SignalKind, signal};

pub use relay::Handshake;

/// How long, after the child exits, the host waits for its remaining output
/// and for the client to take what is queued for it. A grandchild that kept
/// the child's stdout open must not hold the host up.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// What to run and where to put the socket and journal.
#[derive(Debug, Clone)]
pub struct Config {
    /// Unix socket the client connects to. Kept short: the path limit is
    /// about 100 bytes.
    pub socket_path: PathBuf,
    /// The journal file. Created with mode `0600`, replacing any file there.
    pub journal_path: PathBuf,
    /// The child's program and arguments.
    pub command: Vec<OsString>,
}

/// Runs the host until the child exits, and returns the exit code the host
/// should exit with.
pub async fn run(config: Config) -> io::Result<i32> {
    let (program, args) = config
        .command
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no child command given"))?;

    let journal = journal::Journal::create(&config.journal_path)?;
    // Bind before spawning, so a host that cannot be reached never starts a
    // child nobody can talk to.
    let listener = bind_socket(&config.socket_path).await?;
    let result = run_bound(program, args, journal, listener).await;
    let _ = std::fs::remove_file(&config.socket_path);
    result
}

async fn run_bound(
    program: &OsString,
    args: &[OsString],
    journal: journal::Journal,
    listener: UnixListener,
) -> io::Result<i32> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child.stdin.take().expect("child stdin is piped");
    let stdout = child.stdout.take().expect("child stdout is piped");

    let (relay, mut fatal) = relay::Relay::new(journal, stdin);
    let pump = tokio::spawn(relay.clone().pump_child_output(stdout));
    let accept = {
        let relay = relay.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        tokio::spawn(relay.clone().serve(stream));
                    }
                    Err(err) => {
                        eprintln!("boss-agent-host: accept failed: {err}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        })
    };

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let code = tokio::select! {
        status = child.wait() => exit_code(status?),
        Some(err) = fatal.recv() => {
            let _ = child.kill().await;
            accept.abort();
            return Err(err);
        }
        _ = sigterm.recv() => stop_child(&mut child, 15).await,
        _ = sigint.recv() => stop_child(&mut child, 2).await,
        _ = sighup.recv() => stop_child(&mut child, 1).await,
    };

    // Let the rest of the child's output reach the journal and the client
    // before the socket goes away.
    let _ = tokio::time::timeout(DRAIN_GRACE, pump).await;
    accept.abort();
    relay.finish_client(DRAIN_GRACE).await;
    Ok(code)
}

async fn stop_child(child: &mut tokio::process::Child, signal_number: i32) -> i32 {
    let _ = child.kill().await;
    128 + signal_number
}

/// The code the host exits with for the child's status.
pub fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

/// Binds the client socket, replacing a socket file a dead host left behind
/// but never one a live host is listening on.
async fn bind_socket(path: &Path) -> io::Result<UnixListener> {
    let listener = match UnixListener::bind(path) {
        Ok(listener) => listener,
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).await.is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("another host is already listening on {}", path.display()),
                ));
            }
            std::fs::remove_file(path)?;
            UnixListener::bind(path)?
        }
        Err(err) => return Err(err),
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}
