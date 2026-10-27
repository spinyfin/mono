//! The tick loop: sample, observe, persist, prune.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::task::JoinHandle;

use crate::probe::HostProbe;
use crate::sample::HostSample;
use crate::sampler::HostSampler;

pub const DEFAULT_INTERVAL_SECONDS: u64 = 60;
pub const DEFAULT_RETENTION_DAYS: u64 = 7;
/// Floor for the configured interval: faster than this buys nothing and
/// would turn the recorder into a measurable load source.
pub const MIN_INTERVAL_SECONDS: u64 = 5;
pub const MIN_RETENTION_DAYS: u64 = 1;

/// Retention is enforced about this often, regardless of the sample interval.
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);

/// Durable home of the retained series. Implementations do blocking I/O;
/// the loop calls them from `spawn_blocking`.
pub trait SampleStore: Send + Sync + 'static {
    fn insert_host_load_sample(&self, sample: &HostSample) -> Result<()>;
    /// Delete samples with `ts_ms < older_than_ms`; returns how many.
    fn prune_host_load_samples(&self, older_than_ms: i64) -> Result<usize>;
}

/// Called with every sample before it is persisted (the engine publishes
/// gauges from it).
pub type SampleObserver = Arc<dyn Fn(&HostSample) + Send + Sync>;

#[derive(Debug, Clone, Copy)]
pub struct LoopConfig {
    pub interval: Duration,
    pub retention: Duration,
}

impl LoopConfig {
    pub fn new(interval_seconds: u64, retention_days: u64) -> Self {
        Self {
            interval: Duration::from_secs(interval_seconds.max(MIN_INTERVAL_SECONDS)),
            retention: Duration::from_secs(retention_days.max(MIN_RETENTION_DAYS) * 24 * 60 * 60),
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Spawn the sampler on the current tokio runtime.
///
/// The syscalls run inline (microseconds); only the database write goes
/// through `spawn_blocking`, so a contended shared connection can never
/// stall a runtime worker. The first tick fires immediately, recording a
/// sample with no CPU percentages (there is no previous reading yet) and
/// pruning anything past retention.
pub fn spawn_sampler(
    probe: impl HostProbe + 'static,
    store: Arc<dyn SampleStore>,
    observer: SampleObserver,
    config: LoopConfig,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut sampler = HostSampler::new(probe);
        let mut ticker = tokio::time::interval(config.interval);
        // After a stall (machine sleep) one fresh sample is wanted, not a
        // burst of catch-up samples.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let prune_every_ticks = u64::try_from(PRUNE_EVERY.as_millis() / config.interval.as_millis().max(1))
            .unwrap_or(u64::MAX)
            .max(1);
        let mut tick: u64 = 0;
        let mut store_failing = false;
        loop {
            ticker.tick().await;
            let sample = sampler.sample(now_ms());
            observer(&sample);

            let prune_before = tick
                .is_multiple_of(prune_every_ticks)
                .then(|| sample.ts_ms - i64::try_from(config.retention.as_millis()).unwrap_or(i64::MAX));
            tick += 1;

            let store = store.clone();
            let outcome = tokio::task::spawn_blocking(move || -> Result<()> {
                store.insert_host_load_sample(&sample)?;
                if let Some(cutoff) = prune_before {
                    store.prune_host_load_samples(cutoff)?;
                }
                Ok(())
            })
            .await;
            match outcome {
                Ok(Ok(())) => store_failing = false,
                Ok(Err(err)) => {
                    if !store_failing {
                        tracing::warn!(?err, "host_load: persisting sample failed; will keep trying");
                    }
                    store_failing = true;
                }
                Err(join_err) => tracing::warn!(?join_err, "host_load: persist task did not complete"),
            }
        }
    })
}

#[cfg(test)]
mod tests;
