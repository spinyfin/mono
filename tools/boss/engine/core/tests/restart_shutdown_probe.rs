//! Hold a real merge-poller pass asleep across the shutdown request.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use boss_engine::merge_poller::{MergeProbe, PrLifecycleProbe};
use tokio::sync::Notify;

#[derive(Default)]
pub struct SleepingProbe {
    pub entered: Notify,
    pub dropped: Arc<AtomicBool>,
}

struct MarkDropped(Arc<AtomicBool>);

impl Drop for MarkDropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl MergeProbe for SleepingProbe {
    async fn probe(&self, _: &str) -> Result<PrLifecycleProbe> {
        let _guard = MarkDropped(self.dropped.clone());
        self.entered.notify_one();
        tokio::time::sleep(Duration::from_secs(3600)).await;
        panic!("sleeping sweep must be cancelled before runtime shutdown");
    }
}
