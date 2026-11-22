//! Observe repeated Stop submissions even when their durable receipt prevents
//! sending the same command over the stream a second time.
use super::*;
use boss_spoke_stream::store::{CommandAdmission, CommandJournal, CommandOutbox, PendingCommand};

pub struct ObservedOutbox {
    pub inner: WorkDbOutbox,
    pub stops: Arc<AtomicU64>,
}

#[async_trait]
impl CommandOutbox for ObservedOutbox {
    async fn resume(&self, host: &str, receipt: Option<&Cursor>) -> Result<CommandJournal, StoreError> {
        self.inner.resume(host, receipt).await
    }

    async fn append(&self, host: &str, admission: CommandAdmission, command: Command) -> Result<Command, StoreError> {
        let stop = matches!(command.body, Some(command::Body::Stop(_)));
        let result = self.inner.append(host, admission, command).await;
        if stop && result.is_ok() {
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
        result
    }

    async fn next(&self, host: &str, after: u64) -> Result<Option<PendingCommand>, StoreError> {
        self.inner.next(host, after).await
    }

    async fn reject(&self, host: &str, sequence: u64, reason: &str) -> Result<CommandResult, StoreError> {
        self.inner.reject(host, sequence, reason).await
    }

    async fn receipt(&self, host: &str, receipt: &Cursor) -> Result<(), StoreError> {
        self.inner.receipt(host, receipt).await
    }

    async fn command(&self, host: &str, id: &str) -> Result<Option<Command>, StoreError> {
        self.inner.command(host, id).await
    }

    async fn completed(&self, host: &str, id: &str) -> Result<Option<CommandResult>, StoreError> {
        self.inner.completed(host, id).await
    }
}
