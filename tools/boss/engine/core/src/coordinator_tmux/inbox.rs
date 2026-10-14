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

#[async_trait::async_trait]
impl HandoffDelivery for ClaudeInbox {
    async fn deliver(&self, tmux: &Tmux, record: &CoordinatorTmuxRecord, prompt: &str) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(1), async {
            let pid = super::pane_pid(tmux, record).await?;
            let config = std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
                .context("Claude config directory is unavailable")?;
            let registration: Registration = serde_json::from_slice(
                &tokio::fs::read(config.join("sessions").join(format!("{pid}.json")))
                    .await
                    .context("reading the live coordinator's Claude inbox registration")?,
            )
            .context("coordinator does not advertise a supported local inbox")?;
            if registration.pid != pid || registration.peer_protocol != 1 || registration.session_id.is_empty() {
                bail!("coordinator inbox registration does not match the live process or supported protocol");
            }
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
        .context("coordinator prompt delivery was not acknowledged within 1s; grace period has not started")?
    }
}

struct ReceiptSocket(PathBuf);

fn receipt_address(path: &Path) -> Result<String> {
    let path = path.to_str().context("Claude inbox paths must be UTF-8")?;
    let encoded = crate::driver::grok::percent_encode_path_component(path)
        .replace("%2F", "/")
        .replace("%3A", ":")
        .replace('~', "%7E");
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

async fn wait_for_receipt(listener: UnixListener, pid: libc::pid_t, message_id: &str) -> Result<()> {
    loop {
        let (receipt, _) = listener.accept().await?;
        // On macOS LOCAL_PEERPID may be unavailable after the sender closes
        // its one-shot connection. The private callback socket and correlated,
        // unpredictable message ID still authenticate that receipt. Reject a
        // positively identified different process, but do not require the
        // coordinator to keep its connection open until our accept runs.
        if crate::events_socket::peer_pid(&receipt).is_ok_and(|receipt_pid| receipt_pid != pid) {
            continue;
        }
        let mut lines = BufReader::new(receipt).lines();
        while let Some(line) = lines.next_line().await? {
            let value: Value = serde_json::from_str(&line)?;
            if value["action"] != "peer_message_status" || value["orig_msg_id"] != message_id {
                continue;
            }
            match value["status"].as_str() {
                Some("delivered") => return Ok(()),
                Some("held" | "denied" | "expired" | "refused" | "dropped") => {
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
