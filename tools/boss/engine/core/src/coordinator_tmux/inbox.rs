//! Composer-independent delivery to Claude Code's local inbox.
//!
//! A successful write is not delivery: inbound policy can hold or refuse the
//! message. A correlated transcript input or delivered receipt confirms delivery.
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::{CoordinatorTmuxRecord, Tmux};

#[path = "inbox_turn.rs"]
mod turn;
pub use turn::TurnObserver;

#[cfg(test)]
#[path = "inbox_tests.rs"]
mod tests;

#[async_trait::async_trait]
pub(crate) trait HandoffDelivery: Send + Sync {
    async fn deliver(&self, tmux: &Tmux, record: &CoordinatorTmuxRecord, prompt: &str) -> Result<()>;

    async fn deliver_locked<'a>(
        &'a self,
        tmux: &'a Tmux,
        record: &'a CoordinatorTmuxRecord,
        prompt: &str,
        guard: tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>> {
        self.deliver(tmux, record, prompt).await?;
        drop(guard);
        Ok(Box::pin(self.turn_ended(tmux, record)))
    }

    /// Resolves once the turn that began with the delivered request has ended.
    /// Implementations that cannot observe turn boundaries never resolve, so
    /// the grace deadline remains the only exit besides a fresh write.
    async fn turn_ended(&self, _tmux: &Tmux, _record: &CoordinatorTmuxRecord) {
        std::future::pending::<()>().await
    }

    /// Deliver a prompt whose acceptance matters now but whose confirmation
    /// may legitimately outlast [`DELIVERY_TIMEOUT`]: the coordinator reads
    /// its inbox at an input boundary, so a request it made itself mid-turn
    /// (`boss handoff show`) is only confirmed when that turn ends. Resolves
    /// once the inbox has accepted the frame, releasing `guard`, and returns
    /// the confirmation for the caller to await separately. Implementations
    /// without a separate acceptance step confirm in full before returning.
    async fn deliver_accepted<'a>(
        &'a self,
        tmux: &'a Tmux,
        record: &'a CoordinatorTmuxRecord,
        prompt: &str,
        guard: tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<PendingConfirmation> {
        self.deliver(tmux, record, prompt).await?;
        drop(guard);
        Ok(Box::pin(std::future::ready(Ok(()))))
    }
}

/// Confirmation of a delivery whose frame the coordinator's inbox has already
/// accepted; see [`HandoffDelivery::deliver_accepted`].
pub type PendingConfirmation = std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>>;

/// Bound on registration lookup, connect and delivery confirmation. Delivery
/// has its own deadline and never consumes the handoff grace period. It is
/// deliberately generous: a coordinator mid-turn may hold the message until
/// the turn reaches an input boundary.
pub const DELIVERY_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound on confirming a credential delivery after its frame was accepted
/// (see [`HandoffDelivery::deliver_accepted`]). A coordinator that requested
/// the credential itself mid-turn reads it only when that turn ends, so this
/// is far longer than [`DELIVERY_TIMEOUT`]; expiry is audited, never retried
/// blindly.
pub const AUTHORIZATION_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// `--settings` JSON the production coordinator launches with. Delivery
/// confirms only when the coordinator accepts cross-session input, so this is
/// set explicitly rather than inherited from user or default configuration.
pub const COORDINATOR_INBOUND_SETTINGS: &str = r#"{"crossSessionInbound":"accept"}"#;

/// Shared production command. The brief is never part of it: the terminal
/// relay delivers it from a private file (see [`relay_launch_command`]).
fn coordinator_launch_command(executable: &str, model: &str) -> String {
    format!(
        "exec {} --model {} --permission-mode auto --settings {}",
        boss_ssh_transport::shell_quote(executable),
        boss_ssh_transport::shell_quote(model),
        boss_ssh_transport::shell_quote(COORDINATOR_INBOUND_SETTINGS)
    )
}

/// The exact pane command `start_new` runs: the coordinator behind the terminal
/// relay that delivers the private brief from `input_dir`.
pub fn relay_launch_command(input_dir: &Path, executable: &str, model: &str) -> String {
    let driver_command = coordinator_launch_command(executable, model);
    format!(
        "exec {}",
        crate::driver::initial_prompt::launcher_command(
            &boss_ssh_transport::shell_quote(&input_dir.to_string_lossy()),
            driver_command
                .strip_prefix("exec ")
                .expect("coordinator command starts with exec"),
        )
    )
}

pub(crate) struct ClaudeInbox;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Registration {
    pid: libc::pid_t,
    session_id: String,
    messaging_socket_path: PathBuf,
    peer_protocol: u32,
    cwd: PathBuf,
}

/// Accept only the relay's direct grandchild, or a legacy direct pane process.
fn descends_from(
    pid: libc::pid_t,
    ancestor: libc::pid_t,
    parent_of: impl Fn(libc::pid_t) -> Option<libc::pid_t>,
) -> bool {
    pid == ancestor || parent_of(pid).and_then(parent_of) == Some(ancestor)
}

/// The coordinator pane runs the terminal relay, which supervises Claude two
/// forks below it, so the pane pid is not Claude's pid. Claude registers under
/// its own pid: pick the registered session whose process runs below the pane.
pub async fn resolve_claude_pid(sessions: &Path, pane_pid: libc::pid_t) -> Result<libc::pid_t> {
    let mut entries = tokio::fs::read_dir(sessions)
        .await
        .context("reading the Claude session registrations")?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
            .and_then(|stem| stem.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        if descends_from(pid, pane_pid, |pid| {
            crate::worker_registry::parent_pid(pid).ok().flatten()
        }) {
            return Ok(pid);
        }
    }
    bail!("no Claude inbox registration under coordinator pane process {pane_pid}")
}

/// Returns the live Claude pid, its config directory, and its session registration.
async fn read_registration(
    tmux: &Tmux,
    record: &CoordinatorTmuxRecord,
) -> Result<(libc::pid_t, PathBuf, Registration)> {
    let pane_pid = super::pane_pid(tmux, record).await?;
    let config = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
        .context("Claude config directory is unavailable")?;
    let sessions = config.join("sessions");
    let pid = resolve_claude_pid(&sessions, pane_pid).await?;
    let bytes = tokio::fs::read(sessions.join(format!("{pid}.json")))
        .await
        .context("reading the live coordinator's Claude inbox registration")?;
    let registration: Registration =
        serde_json::from_slice(&bytes).context("coordinator does not advertise a supported local inbox")?;
    if registration.pid != pid || registration.peer_protocol != 1 || registration.session_id.is_empty() {
        bail!("coordinator inbox registration does not match the live process or supported protocol");
    }
    Ok((pid, config, registration))
}

#[async_trait::async_trait]
impl HandoffDelivery for ClaudeInbox {
    async fn deliver(&self, tmux: &Tmux, record: &CoordinatorTmuxRecord, prompt: &str) -> Result<()> {
        let stages = Arc::new(Stages::new());
        let delivery = async {
            let (pid, config, registration) = read_registration(tmux, record).await?;
            stages.reached("registration_read");
            inject(
                &registration.messaging_socket_path,
                pid,
                &registration.session_id,
                prompt,
                &crate::coordinator_handoff::transcript_projects_dir_in_config(&config, &registration.cwd)
                    .join(format!("{}.jsonl", registration.session_id)),
                stages.clone(),
                || {},
            )
            .await?
            .confirm()
            .await
            .map(|_| ())
        };
        match tokio::time::timeout(DELIVERY_TIMEOUT, delivery).await {
            Ok(result) => result,
            Err(_) => Err(stages.expiry_error()),
        }
    }

    async fn deliver_locked<'a>(
        &'a self,
        tmux: &'a Tmux,
        record: &'a CoordinatorTmuxRecord,
        prompt: &str,
        guard: tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>> {
        let stages = Arc::new(Stages::new());
        let delivery = async {
            let (pid, config, registration) = read_registration(tmux, record).await?;
            stages.reached("registration_read");
            inject(
                &registration.messaging_socket_path,
                pid,
                &registration.session_id,
                prompt,
                &crate::coordinator_handoff::transcript_projects_dir_in_config(&config, &registration.cwd)
                    .join(format!("{}.jsonl", registration.session_id)),
                stages.clone(),
                || {
                    drop(guard);
                    stages.reached("lifecycle_lock_released");
                },
            )
            .await?
            .confirm()
            .await
        };
        let observer = match tokio::time::timeout(DELIVERY_TIMEOUT, delivery).await {
            Ok(result) => result?,
            Err(_) => return Err(stages.expiry_error()),
        };
        Ok(Box::pin(observer.wait()))
    }

    async fn deliver_accepted<'a>(
        &'a self,
        tmux: &'a Tmux,
        record: &'a CoordinatorTmuxRecord,
        prompt: &str,
        guard: tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<PendingConfirmation> {
        let stages = Arc::new(Stages::new());
        let acceptance = async {
            let (pid, config, registration) = read_registration(tmux, record).await?;
            stages.reached("registration_read");
            inject(
                &registration.messaging_socket_path,
                pid,
                &registration.session_id,
                prompt,
                &crate::coordinator_handoff::transcript_projects_dir_in_config(&config, &registration.cwd)
                    .join(format!("{}.jsonl", registration.session_id)),
                stages.clone(),
                || {
                    drop(guard);
                    stages.reached("lifecycle_lock_released");
                },
            )
            .await
        };
        let injected = match tokio::time::timeout(DELIVERY_TIMEOUT, acceptance).await {
            Ok(result) => result?,
            Err(_) => return Err(stages.expiry_error()),
        };
        Ok(Box::pin(async move { injected.confirm().await.map(|_| ()) }))
    }
}

/// Records each delivery stage for audit and remembers the last one reached,
/// so a deadline expiry can say where delivery stopped instead of only that
/// it did.
struct Stages {
    started: std::time::Instant,
    last: std::sync::Mutex<Option<&'static str>>,
}

impl Stages {
    fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            last: std::sync::Mutex::new(None),
        }
    }

    fn reached(&self, stage: &'static str) {
        crate::audit::record_event(
            "coordinator_inbox_delivery_stage",
            &json!({"stage": stage, "elapsed_ms": self.started.elapsed().as_millis(), "ts_epoch_ms": boss_engine_utils::epoch_time::now_epoch_ms()}),
        );
        tracing::info!(
            stage,
            elapsed_ms = self.started.elapsed().as_millis(),
            "coordinator inbox delivery"
        );
        if let Ok(mut last) = self.last.lock() {
            *last = Some(stage);
        }
    }

    fn last(&self) -> Option<&'static str> {
        self.last.lock().ok().and_then(|last| *last)
    }

    /// The error for a [`DELIVERY_TIMEOUT`] expiry, naming the furthest stage
    /// reached so the operator learns which step stalled.
    fn expiry_error(&self) -> anyhow::Error {
        anyhow::anyhow!(
            "coordinator prompt delivery deadline ({}s) expired {}",
            DELIVERY_TIMEOUT.as_secs(),
            expiry_location(self.last())
        )
    }
}

/// Where a delivery stood when its deadline expired, in operator terms.
fn expiry_location(last_stage: Option<&str>) -> &'static str {
    match last_stage {
        None => "before the coordinator's Claude inbox registration could be read",
        Some("registration_read") => "while connecting to the coordinator's inbox socket",
        Some("connected") => "while writing the request to the coordinator's inbox",
        Some("frame_written" | "lifecycle_lock_released") => {
            "after the request was written to the coordinator's inbox: the coordinator did not accept it as input before the deadline (it is mid-turn, or its inbound policy is holding cross-session messages)"
        }
        Some(_) => "after the request was written to the coordinator's inbox",
    }
}

struct ReceiptSocket(PathBuf);

fn receipt_address(path: &Path) -> Result<String> {
    let path = path.to_str().context("Claude inbox paths must be UTF-8")?;
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'/' | b':' => encoded.push(byte as char),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    Ok(format!("uds:{encoded}"))
}

impl Drop for ReceiptSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Deliver to a known local Claude session and confirm its input or receipt.
/// The caller supplies a deadline. Used by the reset adapter and native compatibility test.
pub async fn deliver(path: &Path, pid: libc::pid_t, session_id: &str, prompt: &str, transcript: &Path) -> Result<()> {
    deliver_observed(path, pid, session_id, prompt, transcript, || {})
        .await
        .map(|_| ())
}

/// Deliver and retain the pre-injection transcript cursor for turn completion.
pub async fn deliver_with_turn(
    path: &Path,
    pid: libc::pid_t,
    session_id: &str,
    prompt: &str,
    transcript: &Path,
) -> Result<TurnObserver> {
    deliver_observed(path, pid, session_id, prompt, transcript, || {}).await
}

async fn deliver_observed(
    path: &Path,
    pid: libc::pid_t,
    session_id: &str,
    prompt: &str,
    transcript: &Path,
    sent: impl FnOnce() + Send,
) -> Result<TurnObserver> {
    inject(path, pid, session_id, prompt, transcript, Arc::new(Stages::new()), sent)
        .await?
        .confirm()
        .await
}

/// A frame the coordinator's inbox has accepted but not yet confirmed as
/// input. [`Self::confirm`] waits for the correlated transcript record or
/// receipt; dropping it withdraws nothing (the message stays queued).
struct Injected {
    transcript: TranscriptCursors,
    stream: UnixStream,
    receipt: ReceiptListener,
    frame: FrameIdentity,
    stages: Arc<Stages>,
}

/// Two reads of the transcript positioned at the pre-injection cursor: one
/// consumed by delivery confirmation, one kept for turn completion.
struct TranscriptCursors {
    confirmation: tokio::fs::File,
    turn: tokio::fs::File,
}

/// The private receipt socket the frame named as its reply address, removed
/// when dropped.
struct ReceiptListener {
    listener: UnixListener,
    _cleanup: ReceiptSocket,
}

/// What a confirmation must correlate with: the unpredictable message id,
/// the exact prompt, and the pid the frame was written to.
struct FrameIdentity {
    message_id: String,
    prompt: String,
    pid: libc::pid_t,
}

/// Capture the transcript cursor, connect to the inbox and write the frame.
/// `sent` runs once the frame is written, before any confirmation wait.
async fn inject(
    path: &Path,
    pid: libc::pid_t,
    session_id: &str,
    prompt: &str,
    transcript: &Path,
    stages: Arc<Stages>,
    sent: impl FnOnce() + Send,
) -> Result<Injected> {
    let mut transcript_file = tokio::fs::File::open(transcript)
        .await
        .context("opening coordinator transcript for delivery confirmation")?;
    let cursor = transcript_file.seek(std::io::SeekFrom::End(0)).await?;
    let mut turn_file = tokio::fs::File::open(transcript).await?;
    turn_file.seek(std::io::SeekFrom::Start(cursor)).await?;
    let directory = path.parent().context("inbox has no parent directory")?;
    let metadata = std::fs::symlink_metadata(directory)?;
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.permissions().mode() & 0o077 != 0 {
        bail!("coordinator inbox directory must be private and owned by the engine user");
    }
    let mut stream = UnixStream::connect(path)
        .await
        .context("connecting to coordinator inbox")?;
    if crate::events_socket::peer_pid(&stream)? != pid {
        bail!("coordinator inbox peer does not match the live pane pid");
    }

    stages.reached("connected");
    let token = crate::engine_control::generate_token();
    let message_id = format!(
        "{}-{}-4{}-8{}-{}",
        &token[..8],
        &token[8..12],
        &token[13..16],
        &token[17..20],
        &token[20..32]
    );
    let receipt_path = directory.join(format!("{}.sock", &message_id[..8]));
    let listener = UnixListener::bind(&receipt_path)?;
    let cleanup = ReceiptSocket(receipt_path.clone());
    std::fs::set_permissions(&receipt_path, std::fs::Permissions::from_mode(0o600))?;
    let frame = json!({
        "msgV": 1,
        "msg_id": message_id,
        "uuid": message_id,
        "session_id": session_id,
        "type": "user",
        "message": { "role": "user", "content": prompt },
        "from": receipt_address(&receipt_path)?,
        "priority": "now",
    });
    stream.write_all(format!("{frame}\n").as_bytes()).await?;
    stages.reached("frame_written");
    sent();
    Ok(Injected {
        transcript: TranscriptCursors {
            confirmation: transcript_file,
            turn: turn_file,
        },
        stream,
        receipt: ReceiptListener {
            listener,
            _cleanup: cleanup,
        },
        frame: FrameIdentity {
            message_id,
            prompt: prompt.to_owned(),
            pid,
        },
        stages,
    })
}

impl Injected {
    /// Wait for the coordinator to take the frame as input: a correlated
    /// transcript record or a `delivered` receipt. Only an explicit,
    /// correlated non-delivery receipt fails.
    async fn confirm(self) -> Result<TurnObserver> {
        let Injected {
            transcript:
                TranscriptCursors {
                    confirmation: transcript_file,
                    turn: turn_file,
                },
            mut stream,
            receipt: ReceiptListener { listener, _cleanup },
            frame:
                FrameIdentity {
                    message_id,
                    prompt,
                    pid,
                },
            stages,
        } = self;
        // Match Claude's macOS transport: immediate half-close can lose the last frame.
        #[cfg(target_os = "macos")]
        tokio::time::sleep(Duration::from_millis(150)).await;
        // The receiver may close after consuming the newline-framed message.
        // A closed write half is harmless; confirmation below still decides success.
        if let Err(error) = stream.shutdown().await
            && error.kind() != std::io::ErrorKind::NotConnected
        {
            return Err(error).context("closing the coordinator inbox write half");
        }
        tokio::select! {
            result = wait_for_input_record(transcript_file, &message_id, &prompt) => { stages.reached(if result.is_ok() { "transcript_confirmed" } else { "transcript_failed" }); result },
            result = wait_for_receipt(listener, pid, &message_id) => { stages.reached(if result.is_ok() { "receipt_confirmed" } else { "receipt_failed" }); result },
        }?;
        Ok(TurnObserver {
            file: turn_file,
            message_id,
        })
    }
}

async fn wait_for_input_record(file: tokio::fs::File, message_id: &str, prompt: &str) -> Result<()> {
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        reader.read_until(b'\n', &mut line).await?;
        if line.last() == Some(&b'\n') {
            if let Ok(value) = serde_json::from_slice::<Value>(&line)
                && input_record_matches(&value, message_id, prompt)
            {
                return Ok(());
            }
            line.clear();
        } else {
            // EOF can split an append. Retain the partial record for the next read.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// Waits for a correlated terminal receipt. `held` is pending, not failure:
/// the coordinator may release a message at its next input boundary, and the
/// transcript or a later `delivered` receipt decides. Stray connections and
/// unparseable lines are skipped; only an explicit, correlated non-delivery
/// status fails.
async fn wait_for_receipt(listener: UnixListener, pid: libc::pid_t, message_id: &str) -> Result<()> {
    loop {
        let Ok((receipt, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };
        // On macOS LOCAL_PEERPID may be unavailable after the sender closes
        // its one-shot connection. The private callback socket and correlated,
        // unpredictable message ID still authenticate that receipt. Reject a
        // positively identified different process, but do not require the
        // coordinator to keep its connection open until our accept runs.
        if crate::events_socket::peer_pid(&receipt).is_ok_and(|receipt_pid| receipt_pid != pid) {
            continue;
        }
        let mut lines = BufReader::new(receipt).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value["action"] != "peer_message_status" || value["orig_msg_id"] != message_id {
                continue;
            }
            match value["status"].as_str() {
                Some("delivered") => return Ok(()),
                Some("denied" | "expired" | "refused" | "dropped") => {
                    bail!("coordinator prompt was not delivered: {}", value["status"])
                }
                _ => continue,
            }
        }
    }
}

fn input_record_matches(value: &Value, message_id: &str, prompt: &str) -> bool {
    let content = value["message"]["content"].as_str().unwrap_or_default();
    // Claude adds its peer-message attribution to the stored input. Match the
    // complete prompt at that boundary, never an arbitrary substring or draft.
    let attributed = value["origin"]["kind"] == "peer"
        && value["origin"]["msg_id"] == message_id
        && content.starts_with(&format!("Another Claude session sent a message:\n{prompt}\n\n"));
    value["type"] == "user" && value["uuid"] == message_id && (content == prompt || attributed)
}
