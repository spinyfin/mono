//! Pool-qualified identities, independent of runtime capacity and legacy slot offsets.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use crate::{DispatchPool, REMOTE_SLOT_BASE};

/// Remote virtual slots are not a dispatch pool: their execution can be of any kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerSlotPool {
    Local(DispatchPool),
    Remote,
}

/// A positive index within a pool, never a global numeric slot or persona lease.
/// The same index in two pools denotes two different identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSlotIdentity {
    pub pool: WorkerSlotPool,
    pub index: NonZeroU32,
}

impl WorkerSlotIdentity {
    pub fn new(pool: WorkerSlotPool, index: u32) -> Option<Self> {
        Some(Self {
            pool,
            index: NonZeroU32::new(index)?,
        })
    }

    /// Decode only the frozen legacy allocation ranges. Gaps are invalid.
    pub fn from_legacy_slot(slot: u8) -> Option<Self> {
        let (pool, offset) = match slot {
            1..=16 => (WorkerSlotPool::Local(DispatchPool::Main), 0),
            17..=24 => (WorkerSlotPool::Local(DispatchPool::Automation), 16),
            25..=40 => (WorkerSlotPool::Local(DispatchPool::Review), 24),
            41..=44 => (WorkerSlotPool::Local(DispatchPool::Progression), 40),
            REMOTE_SLOT_BASE..=u8::MAX => (WorkerSlotPool::Remote, REMOTE_SLOT_BASE - 1),
            _ => return None,
        };
        Self::new(pool, u32::from(slot - offset))
    }

    /// Larger canonical identities cannot be squeezed into another pool's range.
    pub fn to_legacy_slot(self) -> Option<u8> {
        let (offset, count) = match self.pool {
            WorkerSlotPool::Local(DispatchPool::Main) => (0, 16),
            WorkerSlotPool::Local(DispatchPool::Automation) => (16, 8),
            WorkerSlotPool::Local(DispatchPool::Review) => (24, 16),
            WorkerSlotPool::Local(DispatchPool::Progression) => (40, 4),
            WorkerSlotPool::Remote => (u32::from(REMOTE_SLOT_BASE) - 1, 256 - u32::from(REMOTE_SLOT_BASE)),
        };
        (self.index.get() <= count).then(|| (offset + self.index.get()) as u8)
    }

    /// Parse pool worker IDs, plus `worker-200` through `worker-255` for remote
    /// virtual slots. `worker-17` through `worker-199` are ambiguous or unallocated,
    /// not aliases for another pool. Noncanonical decimal spellings are rejected.
    pub fn from_legacy_worker_id(id: &str) -> Option<Self> {
        let (prefix, pool) = [
            ("worker-", WorkerSlotPool::Local(DispatchPool::Main)),
            ("auto-worker-", WorkerSlotPool::Local(DispatchPool::Automation)),
            ("review-", WorkerSlotPool::Local(DispatchPool::Review)),
            ("progression-", WorkerSlotPool::Local(DispatchPool::Progression)),
        ]
        .into_iter()
        .find(|(prefix, _)| id.starts_with(prefix))?;
        let suffix = id.strip_prefix(prefix)?;
        let index = suffix.parse::<u32>().ok()?;
        if suffix != index.to_string() {
            return None;
        }
        if prefix == "worker-" && index >= u32::from(REMOTE_SLOT_BASE) {
            return Self::from_legacy_slot(u8::try_from(index).ok()?);
        }
        let identity = Self::new(pool, index)?;
        identity.to_legacy_slot()?;
        Some(identity)
    }

    pub fn to_legacy_worker_id(self) -> Option<String> {
        let slot = self.to_legacy_slot()?;
        let prefix = match self.pool {
            WorkerSlotPool::Local(DispatchPool::Main) => "worker-",
            WorkerSlotPool::Local(DispatchPool::Automation) => "auto-worker-",
            WorkerSlotPool::Local(DispatchPool::Review) => "review-",
            WorkerSlotPool::Local(DispatchPool::Progression) => "progression-",
            WorkerSlotPool::Remote => return Some(format!("worker-{slot}")),
        };
        Some(format!("{prefix}{}", self.index))
    }
}

#[cfg(test)]
mod tests;
