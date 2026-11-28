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
//! - A client connects and sends one line, `{"replay_from":N}` or
//!   `{"replay_from":N,"takeover":true}` (see [`Handshake`]). `takeover` is
//!   opt-in: it defaults to `false` when omitted and is omitted when
//!   serialized as `false`, so older clients and hosts stay wire-compatible.
//!   A connection that sends anything else, or nothing for a few seconds, is
//!   closed and displaces nobody.
//! - The host answers with journal records (see [`journal`]), one JSON object
//!   per line, starting at sequence number `N`, then every later record as it
//!   is written. Records for both directions arrive, including the client's
//!   own lines, so a client's view is the journal's.
//! - After the handshake every line the client sends is a line for the child.
//!   A line without its terminator, cut off by the client dying, is dropped.
//! - One client at a time, with serialized takeover fencing. Handshakes are
//!   serialized: a connection that completes its handshake takes and closes
//!   the previous client and waits for the previous client's connection task
//!   to finish before sampling the journal end and registering itself. Any
//!   complete (`\n`-terminated) line the previous client had already read is
//!   journalled and queued for the child before the fence completes; no
//!   further lines are read from the kicked client. A concurrent third
//!   handshake waits behind the fence so it cannot register unfenced or
//!   overwrite a client without kicking it.
//! - When `takeover` is `true`, the host sends one socket-only (non-journalled)
//!   takeover boundary line ([`TAKEOVER_BOUNDARY_LINE`], recognized by
//!   [`is_takeover_boundary_line`]) immediately after the fenced replay range
//!   and before any live records. Everything from the previous client appears
//!   before that boundary. Legacy clients (`takeover` omitted or `false`)
//!   never receive the boundary line and continue to see only journal
//!   [`journal::WireRecord`] lines.
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

#[cfg(feature = "test-support")]
pub use relay::{BeforeJournalClientLineHook, TestHooks};
pub use relay::{Handshake, TAKEOVER_BOUNDARY_LINE, TakeoverBoundary, is_takeover_boundary_line};

/// How long, after the child exits, the host waits for its remaining output
/// and for the client to take what is queued for it. A grandchild that kept
/// the child's stdout open must not hold the host up.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// How long a child that was sent a termination signal has to exit before the
/// host kills it.
const STOP_GRACE: Duration = Duration::from_secs(5);

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
    #[cfg(feature = "test-support")]
    {
        run_with_hooks(config, TestHooks::default()).await
    }
    #[cfg(not(feature = "test-support"))]
    {
        run_impl(config).await
    }
}

/// Runs the host with injected test hooks until the child exits.
#[cfg(feature = "test-support")]
pub async fn run_with_hooks(config: Config, hooks: TestHooks) -> io::Result<i32> {
    run_impl(config, hooks).await
}

async fn run_impl(config: Config, #[cfg(feature = "test-support")] hooks: TestHooks) -> io::Result<i32> {
    let (program, args) = config
        .command
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no child command given"))?;

    // Bind first. Binding is where a live host on this socket is refused, and
    // the refused host must not have touched that host's journal. It also
    // means a host that cannot be reached never starts a child nobody can
    // talk to.
    let listener = bind_socket(&config.socket_path).await?;
    let journal = match journal::Journal::create(&config.journal_path) {
        Ok(journal) => journal,
        Err(err) => {
            let _ = std::fs::remove_file(&config.socket_path);
            return Err(err);
        }
    };
    let result = run_bound(
        program,
        args,
        journal,
        listener,
        #[cfg(feature = "test-support")]
        hooks,
    )
    .await;
    let _ = std::fs::remove_file(&config.socket_path);
    result
}

async fn run_bound(
    program: &OsString,
    args: &[OsString],
    journal: journal::Journal,
    listener: UnixListener,
    #[cfg(feature = "test-support")] hooks: TestHooks,
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

    let (relay, mut fatal) = relay::Relay::new(
        journal,
        stdin,
        #[cfg(feature = "test-support")]
        hooks,
    );
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
        _ = sigterm.recv() => stop_child(&mut child, libc::SIGTERM).await,
        _ = sigint.recv() => stop_child(&mut child, libc::SIGINT).await,
        _ = sighup.recv() => stop_child(&mut child, libc::SIGHUP).await,
    };

    // Let the rest of the child's output reach the journal and the client
    // before the socket goes away.
    let pumped = tokio::time::timeout(DRAIN_GRACE, pump).await;
    accept.abort();
    relay.finish_client(DRAIN_GRACE).await;
    // A journal that failed while draining means records are missing, so the
    // child's status is not the whole story.
    if let Ok(Ok(Err(err))) = pumped {
        return Err(err);
    }
    if let Ok(err) = fatal.try_recv() {
        return Err(err);
    }
    Ok(code)
}

/// Forwards `signal_number` to the child so it can clean up (the Claude
/// adapter runs its own child), and kills it if it has not exited after
/// [`STOP_GRACE`]. Returns the exit code the host should use.
async fn stop_child(child: &mut tokio::process::Child, signal_number: i32) -> i32 {
    if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
        // SAFETY: `kill` has no memory-safety preconditions; the pid is a
        // child this process has not yet reaped, so it cannot be recycled.
        unsafe {
            libc::kill(pid, signal_number);
        }
    }
    if tokio::time::timeout(STOP_GRACE, child.wait()).await.is_err() {
        let _ = child.kill().await;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A journal that cannot be written (its file is open read-only), over a
    /// file in `dir`.
    fn unwritable_journal(dir: &Path) -> journal::Journal {
        let path = dir.join("wire.jsonl");
        std::fs::write(&path, b"").unwrap();
        journal::Journal::over_file(&path, std::fs::File::open(&path).unwrap())
    }

    #[tokio::test]
    async fn a_journal_that_fails_while_the_child_output_is_drained_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join("h.sock")).unwrap();
        let program = OsString::from("/bin/sh");
        let args = [OsString::from("-c"), OsString::from("echo hello")];
        let result = run_bound(
            &program,
            &args,
            unwritable_journal(dir.path()),
            listener,
            #[cfg(feature = "test-support")]
            TestHooks::default(),
        )
        .await;
        assert!(result.is_err(), "the child exited 0 but its output was not journalled");
    }
}
