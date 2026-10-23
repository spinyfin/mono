use boss_protocol_spoke::{
    Validate,
    limits::MAX_FRAME_BYTES,
    negotiate::feature,
    v1::{Command, CommandResult, command},
};
use prost::Message;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::oneshot;

use crate::StreamManager;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandOutcome {
    #[error("host unavailable; command not submitted")]
    Unavailable,
    #[error("pending command limit or duplicate active waiter; command not submitted")]
    Busy,
    #[error("invalid or unnegotiated command; command not submitted")]
    Invalid,
    #[error("outcome uncertain; reconcile using the same command ID")]
    OutcomeUncertain,
}

impl StreamManager {
    /// Persist then wake the writer. Only a committed matching result succeeds.
    /// A timeout, cancellation or disconnect never deletes the durable command.
    /// Retry with exactly the same ID and content; never redispatch to a new host.
    pub async fn command(
        &self,
        host_id: &str,
        command: Command,
        deadline: Duration,
    ) -> Result<CommandResult, CommandOutcome> {
        tokio::time::timeout(deadline, self.command_inner(host_id, command))
            .await
            .unwrap_or(Err(CommandOutcome::OutcomeUncertain))
    }

    async fn command_inner(&self, host_id: &str, mut command: Command) -> Result<CommandResult, CommandOutcome> {
        use CommandOutcome::*;
        if command.sequence != 0 {
            return Err(Invalid);
        }
        command.sequence = 1; // Validate everything except the outbox-assigned sequence.
        if command.validate().is_err() || command.encoded_len() + 32 > MAX_FRAME_BYTES as usize {
            return Err(Invalid);
        }
        command.sequence = 0;
        let host = self.0.hosts.lock().await.get(host_id).cloned().ok_or(Unavailable)?;
        let mut guard = host.lock().await;
        let connection = guard
            .as_mut()
            .filter(|c| c.active.load(Ordering::Acquire))
            .ok_or(Unavailable)?;
        if !supported(&command, &connection.session.selection) {
            return Err(Invalid);
        }
        connection.waiters.retain(|_, sender| !sender.is_closed());
        if connection.waiters.len() >= self.0.config.max_pending || connection.waiters.contains_key(&command.command_id)
        {
            return Err(Busy);
        }
        let id = command.command_id.clone();
        // Registration and append share the session gate with result ingestion.
        let (tx, rx) = oneshot::channel();
        connection.waiters.insert(id.clone(), tx);
        let storage = &self.0.storage.outbox;
        if storage.append(host_id, command).await.is_err() {
            connection.waiters.remove(&id);
            return Err(OutcomeUncertain);
        }
        if let Ok(Some(result)) = storage.completed(host_id, &id).await {
            connection.waiters.remove(&id);
            return Ok(result);
        }
        connection.wake.notify_one();
        drop(guard);
        // Ingress removes a successful waiter; disconnect drops the whole map.
        // Cancelled/timed-out receivers are pruned on the next submission.
        rx.await.map_err(|_| OutcomeUncertain)
    }
}

pub(crate) fn supported(command: &Command, selection: &boss_protocol_spoke::negotiate::Selection) -> bool {
    match &command.body {
        Some(command::Body::Spawn(spawn)) => spawn
            .required_features
            .iter()
            .all(|feature| selection.revision(&feature.name) == Some(feature.revision)),
        Some(command::Body::Capture(_)) => selection.has(feature::PANE_CAPTURE),
        Some(command::Body::Upgrade(_)) => selection.has(feature::UPGRADE),
        _ => true,
    }
}
