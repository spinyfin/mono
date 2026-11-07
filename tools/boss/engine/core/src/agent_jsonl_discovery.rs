//! Shared host-local rollout discovery.
pub use boss_engine_driver::discovery::*;

/// Local adapter seam: WorkDb is defined in another crate, so its engine-owned
/// implementation must use a local trait. Discovery remains shared below it.
pub trait RolloutProbeSource {
    fn load_rollout_probe_target(&self, run_id: &str) -> Result<Option<RolloutProbeTarget>, String>;
}

pub fn probe_correlated_rollout(store: &dyn RolloutProbeSource, run_id: &str, now_epoch_secs: i64) -> RolloutLiveness {
    struct Source<'a>(&'a dyn RolloutProbeSource);
    impl boss_engine_driver::discovery::RolloutProbeSource for Source<'_> {
        fn load_rollout_probe_target(&self, run_id: &str) -> Result<Option<RolloutProbeTarget>, String> {
            self.0.load_rollout_probe_target(run_id)
        }
    }
    boss_engine_driver::discovery::probe_correlated_rollout(&Source(store), run_id, now_epoch_secs)
}
