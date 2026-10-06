//! Types for the spawn-admission boundary shared by every spawn path; see
//! [`crate::live_worker_state::LiveWorkerStateRegistry::try_admit_spawn`].

/// Held for the whole window in which a spawn is committed but not yet visible to the
/// idle-shutdown check. See [`crate::live_worker_state::LiveWorkerStateRegistry::try_admit_spawn`].
pub type SpawnAdmission = tokio::sync::OwnedRwLockReadGuard<bool>;

/// Why [`crate::live_worker_state::LiveWorkerStateRegistry::try_admit_spawn`] refused. Both are retriable deferrals,
/// never spawn failures: the caller reverts its claim and a later dispatch retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionRefusal {
    /// An idle-guarded shutdown was accepted; this engine will exit.
    ShuttingDown,
    /// An idle-guarded shutdown is deciding right now.
    ShutdownDeciding,
}

impl std::fmt::Display for AdmissionRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ShuttingDown => f.write_str("engine is shutting down"),
            Self::ShutdownDeciding => f.write_str("engine shutdown check in progress"),
        }
    }
}

impl std::error::Error for AdmissionRefusal {}
