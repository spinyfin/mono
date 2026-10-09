use std::sync::Mutex;

use super::*;
use crate::probe::{CpuTicks, ProbeError, SwapReading, VmReading};

struct ZeroProbe;

impl HostProbe for ZeroProbe {
    fn logical_cpus(&mut self) -> Result<u32, ProbeError> {
        Ok(4)
    }
    fn load_avg(&mut self) -> Result<[f64; 3], ProbeError> {
        Ok([1.0, 2.0, 3.0])
    }
    fn cpu_ticks(&mut self) -> Result<CpuTicks, ProbeError> {
        Ok(CpuTicks {
            user: 0,
            system: 0,
            idle: 0,
            nice: 0,
        })
    }
    fn vm(&mut self) -> Result<VmReading, ProbeError> {
        Err(ProbeError::kern("host_statistics64(HOST_VM_INFO64)", 4))
    }
    fn mem_total_bytes(&mut self) -> Result<u64, ProbeError> {
        Ok(1 << 30)
    }
    fn mem_pressure_level_pct(&mut self) -> Result<i64, ProbeError> {
        Ok(50)
    }
    fn swap(&mut self) -> Result<SwapReading, ProbeError> {
        Ok(SwapReading {
            total_bytes: 0,
            used_bytes: 0,
        })
    }
}

#[derive(Default)]
struct MemStore {
    samples: Mutex<Vec<HostSample>>,
    prunes: Mutex<Vec<i64>>,
}

impl SampleStore for MemStore {
    fn insert_host_load_sample(&self, sample: &HostSample) -> Result<()> {
        self.samples.lock().unwrap().push(sample.clone());
        Ok(())
    }
    fn prune_host_load_samples(&self, older_than_ms: i64) -> Result<usize> {
        self.prunes.lock().unwrap().push(older_than_ms);
        Ok(0)
    }
}

#[test]
fn config_clamps_interval_and_retention_to_sane_floors() {
    let c = LoopConfig::new(1, 0);
    assert_eq!(c.interval, Duration::from_secs(MIN_INTERVAL_SECONDS));
    assert_eq!(c.retention, Duration::from_secs(24 * 60 * 60));
    let c = LoopConfig::new(60, 7);
    assert_eq!(c.interval, Duration::from_secs(60));
    assert_eq!(c.retention, Duration::from_secs(7 * 24 * 60 * 60));
}

#[tokio::test]
async fn each_tick_observes_and_persists_a_sample_and_prunes_at_startup() {
    let store = Arc::new(MemStore::default());
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_in = observed.clone();
    let observer: SampleObserver = Arc::new(move |s: &HostSample| observed_in.lock().unwrap().push(s.ts_ms));

    // Real time with a short interval: paused time can auto-advance while a
    // `spawn_blocking` write is pending, which would make tick counts racy.
    let config = LoopConfig {
        interval: Duration::from_millis(20),
        retention: Duration::from_secs(7 * 24 * 60 * 60),
    };
    let handle = spawn_sampler(ZeroProbe, store.clone(), observer, config);
    tokio::time::timeout(Duration::from_secs(10), async {
        while store.samples.lock().unwrap().len() < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("three samples within 10s");
    handle.abort();

    let samples = store.samples.lock().unwrap();
    assert!(observed.lock().unwrap().len() >= 3);
    // A failed probe call doesn't stop the series.
    assert!(
        samples
            .iter()
            .all(|s| s.load_avg_1m == Some(1.0) && s.mem_used_bytes.is_none())
    );
    // Timestamps advance monotonically.
    assert!(samples.windows(2).all(|w| w[0].ts_ms <= w[1].ts_ms));
    // Retention was enforced on the first tick, and only then (hourly cadence).
    let prunes = store.prunes.lock().unwrap();
    assert_eq!(prunes.len(), 1);
    assert_eq!(prunes[0], samples[0].ts_ms - 7 * 24 * 60 * 60 * 1000);
}
