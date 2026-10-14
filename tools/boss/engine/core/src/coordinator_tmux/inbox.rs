//! Composer-independent delivery to Claude Code's local inbox.
//!
//! A successful write is not delivery: inbound policy can hold or refuse the
//! message. A correlated transcript input or delivered receipt confirms delivery.
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::{CoordinatorTmuxRecord, Tmux};

#[cfg(test)]
#[path = "inbox_tests.rs"]
mod tests;

#[async_trait::async_trait]
pub(crate) trait HandoffDelivery: Send + Sync {
    async fn deliver(&self, tmux: &Tmux, record: &CoordinatorTmuxRecord, prompt: &str) -> Result<()>;

    /// Resolves once the turn that began with the delivered request has ended.
    /// Implementations that cannot observe turn boundaries never resolve, so
    /// the grace deadline remains the only exit besides a fresh write.
    async fn turn_ended(&self, _tmux: &Tmux, _record: &CoordinatorTmuxRecord) {
        std::future::pending::<()>().await
    }
}

/// Bound on registration lookup, connect and delivery confirmation. Delivery
/// has its own deadline and never consumes the handoff grace period. It is
/// deliberately generous: a coordinator mid-turn may hold the message until
/// the turn reaches an input boundary.
pub const DELIVERY_TIMEOUT: Duration = Duration::from_secs(15);

/// Interval at which the live registration is polled for turn boundaries.
const TURN_POLL: Duration = Duration::from_millis(250);

/// `--settings` JSON the production coordinator launches with. Delivery
/// confirms only when the coordinator accepts cross-session input, so this is
/// set explicitly rather than inherited from user or default configuration.
pub const COORDINATOR_INBOUND_SETTINGS: &str = r#"{"crossSessionInbound":"accept"}"#;

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

/// Returns the pane pid, the Claude config directory, the registration, and
/// the session's reported `status` (`idle` between turns).
async fn read_registration(
    tmux: &Tmux,
    record: &CoordinatorTmuxRecord,
) -> Result<(libc::pid_t, PathBuf, Registration, Option<String>)> {
    let pid = super::pane_pid(tmux, record).await?;
    let config = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
        .context("Claude config directory is unavailable")?;
    let bytes = tokio::fs::read(config.join("sessions").join(format!("{pid}.json")))
        .await
        .context("reading the live coordinator's Claude inbox registration")?;
    let registration: Registration =
        serde_json::from_slice(&bytes).context("coordinator does not advertise a supported local inbox")?;
    let status = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| value["status"].as_str().map(str::to_owned));
    if registration.pid != pid || registration.peer_protocol != 1 || registration.session_id.is_empty() {
        bail!("coordinator inbox registration does not match the live process or supported protocol");
    }
    Ok((pid, config, registration, status))
}

#[async_trait::async_trait]
impl HandoffDelivery for ClaudeInbox {
    async fn deliver(&self, tmux: &Tmux, record: &CoordinatorTmuxRecord, prompt: &str) -> Result<()> {
        tokio::time::timeout(DELIVERY_TIMEOUT, async {
            let (pid, config, registration, _) = read_registration(tmux, record).await?;
            deliver(
                &registration.messaging_socket_path,
                pid,
                &registration.session_id,
                prompt,
                &crate::coordinator_handoff::transcript_projects_dir_in_config(&config, &registration.cwd)
                    .join(format!("{}.jsonl", registration.session_id)),
            )
            .await
        })
        .await
        .with_context(|| {
            format!(
                "coordinator prompt delivery was not acknowledged within {}s; grace period has not started",
                DELIVERY_TIMEOUT.as_secs()
            )
        })?
    }

    async fn turn_ended(&self, tmux: &Tmux, record: &CoordinatorTmuxRecord) {
        // The request is delivered at an input boundary, so the registration
        // goes busy for the turn it starts and returns to idle when it ends.
        // Unreadable registrations are treated as "not yet observed".
        let mut saw_busy = false;
        loop {
            if let Ok((_, _, _, status)) = read_registration(tmux, record).await {
                match status.as_deref() {
                    Some("idle") if saw_busy => return,
                    Some("idle") | None => {}
                    Some(_) => saw_busy = true,
                }
            }
            tokio::time::sleep(TURN_POLL).await;
        }
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
    let mut transcript_file = tokio::fs::File::open(transcript)
        .await
        .context("opening coordinator transcript for delivery confirmation")?;
    transcript_file.seek(std::io::SeekFrom::End(0)).await?;
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
    let _cleanup = ReceiptSocket(receipt_path.clone());
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
        result = wait_for_input_record(transcript_file, &message_id, prompt) => result,
        result = wait_for_receipt(listener, pid, &message_id) => result,
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
