//! In-memory record of PR chains the engine is rewriting in the shared jj
//! store (the conflict ladder's rebase and push, and the speculative
//! no-push rebase).
//!
//! A live worker on the chain and an engine rebase must never overlap: a
//! worker's workspace follows rewrites of the PR branch concurrently, and
//! overlapping operations on one branch produce divergence. The ladder checks
//! for live workers before it starts, but a queued revision can be claimed,
//! leased and positioned while the rebase is still running, and the dispatch
//! chain guard only sees database liveness and dispatch reservations. This
//! table closes that gap: the ladder takes a hold before it looks at live
//! writers, and the dispatch guard treats a held chain as occupied.
//!
//! Ordering is what makes the two checks race-free. The ladder inserts its
//! hold and then reads the database; dispatch claims its execution in the
//! database and then reads this table. Whichever side moves second sees the
//! other.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use boss_protocol::WorkExecution;

use super::{WorkDb, chain_helpers::chain_root};

#[derive(Default)]
pub(crate) struct EngineRebaseHolds {
    next_token: u64,
    held: Vec<HeldChain>,
}

struct HeldChain {
    token: u64,
    chain_root_id: String,
    work_item_id: String,
}

/// Why the engine may not rewrite a chain right now.
#[derive(Debug)]
pub enum EngineRebaseBlocker {
    /// Workers are live on the chain (`id (status)`, comma separated).
    LiveWriters(String),
    /// Another engine rebase already holds the chain.
    EngineRebase(String),
}

impl std::fmt::Display for EngineRebaseBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LiveWriters(writers) => f.write_str(writers),
            Self::EngineRebase(work_item_id) => write!(f, "engine rebase of {work_item_id} in progress"),
        }
    }
}

/// RAII claim on a chain for an engine rebase. Dropping it releases the chain,
/// so every exit path of the rebase (including a panic) frees it.
#[must_use = "dropping the hold immediately releases the chain"]
pub struct EngineRebaseHold {
    holds: Arc<Mutex<EngineRebaseHolds>>,
    token: u64,
}

impl Drop for EngineRebaseHold {
    fn drop(&mut self) {
        let mut holds = self.holds.lock().unwrap_or_else(|e| e.into_inner());
        holds.held.retain(|held| held.token != self.token);
    }
}

impl WorkDb {
    /// Claim the chain containing `work_item_id` for an engine rebase, or say
    /// why that is not allowed. A failed live-writer lookup also refuses: it
    /// is unsafe to rewrite a branch a worker may be on because the check
    /// could not run.
    pub fn try_hold_chain_for_engine_rebase(
        &self,
        work_item_id: &str,
    ) -> std::result::Result<EngineRebaseHold, EngineRebaseBlocker> {
        let root = {
            let conn = self
                .connect()
                .map_err(|_| EngineRebaseBlocker::LiveWriters("lookup failed".to_owned()))?;
            chain_root(&conn, work_item_id).map_err(|err| {
                tracing::warn!(
                    work_item_id,
                    ?err,
                    "could not resolve chain root; refusing engine rebase"
                );
                EngineRebaseBlocker::LiveWriters("lookup failed".to_owned())
            })?
        };
        // Insert before reading the database: a dispatch that claims its
        // execution after this point sees the hold, and one that claimed
        // earlier is seen by the read below.
        let hold = {
            let mut holds = self.engine_rebase_holds.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(other) = holds.held.iter().find(|held| held.chain_root_id == root) {
                return Err(EngineRebaseBlocker::EngineRebase(other.work_item_id.clone()));
            }
            holds.next_token += 1;
            let token = holds.next_token;
            holds.held.push(HeldChain {
                token,
                chain_root_id: root,
                work_item_id: work_item_id.to_owned(),
            });
            EngineRebaseHold {
                holds: Arc::clone(&self.engine_rebase_holds),
                token,
            }
        };
        match self.live_writers_in_chain(work_item_id) {
            Ok(writers) if writers.is_empty() => Ok(hold),
            Ok(writers) => Err(EngineRebaseBlocker::LiveWriters(describe_writers(&writers))),
            Err(err) => {
                tracing::warn!(
                    work_item_id,
                    ?err,
                    "could not list live writers; refusing engine rebase"
                );
                Err(EngineRebaseBlocker::LiveWriters("lookup failed".to_owned()))
            }
        }
    }

    /// The work item an engine rebase of `work_item_id`'s chain was started
    /// for, while one is in progress.
    pub fn engine_rebase_in_progress(&self, work_item_id: &str) -> Result<Option<String>> {
        if self
            .engine_rebase_holds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .held
            .is_empty()
        {
            return Ok(None);
        }
        let root = chain_root(&*self.connect()?, work_item_id)?;
        let holds = self.engine_rebase_holds.lock().unwrap_or_else(|e| e.into_inner());
        Ok(holds
            .held
            .iter()
            .find(|held| held.chain_root_id == root)
            .map(|held| held.work_item_id.clone()))
    }
}

fn describe_writers(writers: &[WorkExecution]) -> String {
    writers
        .iter()
        .map(|execution| format!("{} ({})", execution.id, execution.status))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, WorkDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        (dir, db)
    }

    #[test]
    fn a_held_chain_is_visible_through_every_clone_until_the_hold_drops() {
        let (_dir, db) = db();
        let other_handle = db.clone();
        assert_eq!(other_handle.engine_rebase_in_progress("T1").unwrap(), None);
        let hold = db.try_hold_chain_for_engine_rebase("T1").expect("no live writers");
        assert_eq!(
            other_handle.engine_rebase_in_progress("T1").unwrap().as_deref(),
            Some("T1")
        );
        assert_eq!(other_handle.engine_rebase_in_progress("T2").unwrap(), None);
        drop(hold);
        assert_eq!(other_handle.engine_rebase_in_progress("T1").unwrap(), None);
    }

    #[test]
    fn a_second_engine_rebase_of_the_same_chain_is_refused_and_leaves_the_first_held() {
        let (_dir, db) = db();
        let _first = db.try_hold_chain_for_engine_rebase("T1").expect("first hold");
        let refused = db
            .try_hold_chain_for_engine_rebase("T1")
            .err()
            .expect("second hold refused");
        assert!(matches!(refused, EngineRebaseBlocker::EngineRebase(_)), "{refused}");
        assert!(db.engine_rebase_in_progress("T1").unwrap().is_some());
    }
}
