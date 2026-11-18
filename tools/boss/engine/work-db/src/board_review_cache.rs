//! Bounded, epoch-validated cache of per-owner AI-review projections.
//!
//! Every board assembly derives `ai_review_state`,
//! `ai_review_findings_revision_id` and `ai_review_badge` for each card that
//! can show them. The verdict-derived part of that projection belongs to the
//! card's PR owner (its chain root) and is shared by the whole revision
//! family, so this cache keeps one [`OwnerReview`] per root and reuses it
//! while the family's `board_review_epoch` key is unchanged. Without it,
//! every assembly re-reads the `tasks` row of every open *and done* card
//! with a PR — thousands of multi-kilobyte rows — to answer a question whose
//! inputs almost never change once a card is done.
//!
//! Invalidation is data-driven: SQLite triggers on `tasks`,
//! `pr_review_verdicts`, `work_executions` and `worker_proposals` bump the
//! written item's epoch and its parent's inside the writer's transaction
//! (`schema_init`'s schema 53), so a write from any connection or process is
//! seen, and the reader folds the epochs in the same snapshot as the data it
//! would otherwise recompute from. Each key contains sorted member IDs and
//! epochs from the same full SQL family as the projection, including archived
//! and deleted revisions, plus any findings revision a family verdict still
//! names after that revision has left the family. Membership and epoch
//! changes both invalidate it. The key read seeds owners from the bound ids
//! and walks children through `idx_tasks_parent_kind_id` (schema 54) so a
//! warm pass does not fetch `tasks` rows.
//!
//! Inputs that are not epoch-covered stay outside the cache: whether a
//! review is running or queued is derived per pass from live executions, and
//! the card's own status and kind come from the fresh board read.
//!
//! Lifecycle matches [`super::board_runtime_cache`]: a pass is one assembly
//! over one product, entries the pass did not touch are pruned when it
//! completes, and a global capacity evicts the least recently touched passes.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::review_badge::{OwnerReview, family_recursive_sql};

/// Exact membership and input versions, ordered by member ID.
pub type FamilyKey = Vec<(String, i64)>;

/// Constant SQL for [`read_family_keys`]: owners from a JSON array (no
/// `tasks` kind check), the family walk shared with the projection
/// ([`family_recursive_sql`]), and verdict-referenced findings revisions
/// folded into the same key through the partial covering index
/// `pr_review_verdicts_revision_idx`, so a warm pass reads only verdict rows
/// that name a revision.
pub const FAMILY_KEY_SQL: &str = concat!(
    "WITH RECURSIVE owners(card, id) AS (
        SELECT value, value FROM json_each(?1)
    ), ",
    family_recursive_sql!(),
    "
    SELECT DISTINCT m.root, m.id, COALESCE(e.epoch, 0)
    FROM (
        SELECT f.root, f.id FROM family f
        UNION
        SELECT f.root, v.revision_task_id
        FROM family f
        JOIN pr_review_verdicts v INDEXED BY pr_review_verdicts_revision_idx
            ON v.work_item_id = f.id
        WHERE v.revision_task_id IS NOT NULL AND v.revision_task_id != ''
    ) m
    LEFT JOIN board_review_epoch e ON e.work_item_id = m.id
    ORDER BY m.root, m.id"
);

/// Read only the requested owners' families, using the projection's own
/// family walk so off-board intermediates cannot hide input writes,
/// plus any findings revision a family verdict still names.
pub fn read_family_keys(conn: &rusqlite::Connection, owners: &[String]) -> anyhow::Result<HashMap<String, FamilyKey>> {
    if owners.is_empty() {
        return Ok(HashMap::new());
    }
    let ids = serde_json::to_string(owners)?;
    let mut stmt = conn.prepare_cached(FAMILY_KEY_SQL)?;
    let rows = stmt.query_map([&ids], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut keys: HashMap<String, FamilyKey> = HashMap::new();
    for row in rows {
        let (owner, member, epoch) = row?;
        keys.entry(owner).or_default().push((member, epoch));
    }
    Ok(keys)
}

/// Entries kept across all products. Far more owners than the largest
/// measured product (about 8k cards) at a few hundred bytes each.
pub const DEFAULT_CAPACITY: usize = 65_536;

pub struct BoardReviewCache {
    capacity: usize,
    state: Mutex<State>,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    next_pass: u64,
}

struct Entry {
    key: FamilyKey,
    product_id: String,
    pass: u64,
    review: Arc<OwnerReview>,
}

/// Outcome of probing a pass's owners: `hits[i]` answers `owners[i]`.
pub struct ReviewLookup {
    pub pass: u64,
    pub hits: Vec<Option<Arc<OwnerReview>>>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewCacheStats {
    pub entries: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

impl Default for BoardReviewCache {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl BoardReviewCache {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::default(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    /// Start a pass over `product_id`. `owners` pairs each chain-root id with
    /// its family key. A hit requires an exact key match; every hit is
    /// stamped into this pass so completion keeps it.
    pub fn lookup(&self, product_id: &str, owners: &[(&str, FamilyKey)]) -> ReviewLookup {
        let mut state = self.state.lock().expect("board review cache poisoned");
        state.next_pass += 1;
        let pass = state.next_pass;
        let mut hits = Vec::with_capacity(owners.len());
        for (id, key) in owners {
            let hit = state.entries.get_mut(*id).and_then(|entry| {
                (entry.key == *key).then(|| {
                    entry.pass = pass;
                    if entry.product_id != product_id {
                        entry.product_id = product_id.to_owned();
                    }
                    Arc::clone(&entry.review)
                })
            });
            hits.push(hit);
        }
        let found = hits.iter().filter(|hit| hit.is_some()).count() as u64;
        self.hits.fetch_add(found, Ordering::Relaxed);
        self.misses.fetch_add(owners.len() as u64 - found, Ordering::Relaxed);
        ReviewLookup { pass, hits }
    }

    /// Finish the pass: store `fresh` projections (owner id, family key,
    /// value), prune this product's entries no pass at or after `pass`
    /// touched, and enforce the global capacity.
    pub fn complete(&self, product_id: &str, pass: u64, fresh: Vec<(String, FamilyKey, Arc<OwnerReview>)>) {
        let mut state = self.state.lock().expect("board review cache poisoned");
        for (id, key, review) in fresh {
            // A concurrent assembly on a newer snapshot may already have
            // completed a later pass; do not overwrite its entry.
            if state.entries.get(&id).is_some_and(|entry| entry.pass > pass) {
                continue;
            }
            state.entries.insert(
                id,
                Entry {
                    key,
                    product_id: product_id.to_owned(),
                    pass,
                    review,
                },
            );
        }
        state
            .entries
            .retain(|_, entry| entry.product_id != product_id || entry.pass >= pass);
        let excess = state.entries.len().saturating_sub(self.capacity);
        if excess > 0 {
            let mut by_age: Vec<(u64, String)> = state
                .entries
                .iter()
                .map(|(id, entry)| (entry.pass, id.clone()))
                .collect();
            by_age.sort_unstable();
            for (_, id) in by_age.into_iter().take(excess) {
                state.entries.remove(&id);
            }
            self.evictions.fetch_add(excess as u64, Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    pub fn stats(&self) -> ReviewCacheStats {
        let state = self.state.lock().expect("board review cache poisoned");
        ReviewCacheStats {
            entries: state.entries.len(),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review() -> Arc<OwnerReview> {
        Arc::new(OwnerReview {
            current_head: None,
            history: Some(super::super::review_badge::ReviewBadgeHistory::builder().build()),
        })
    }

    fn pass(cache: &BoardReviewCache, product: &str, versions: &[(&str, i64)]) -> Vec<bool> {
        let owners: Vec<_> = versions
            .iter()
            .map(|(id, epoch)| (*id, vec![((*id).to_owned(), *epoch)]))
            .collect();
        let lookup = cache.lookup(product, &owners);
        let misses: Vec<_> = owners
            .iter()
            .zip(&lookup.hits)
            .filter(|(_, hit)| hit.is_none())
            .map(|((id, key), _)| ((*id).to_owned(), key.clone(), review()))
            .collect();
        cache.complete(product, lookup.pass, misses);
        lookup.hits.iter().map(Option::is_some).collect()
    }

    #[test]
    fn hits_only_on_an_exact_key_match() {
        let cache = BoardReviewCache::default();
        assert_eq!(pass(&cache, "p", &[("a", 0), ("b", 3)]), [false, false]);
        assert_eq!(pass(&cache, "p", &[("a", 0), ("b", 3)]), [true, true]);
        assert_eq!(pass(&cache, "p", &[("a", 0), ("b", 4)]), [true, false]);
        // Any different key misses, regardless of ordering.
        assert_eq!(pass(&cache, "p", &[("a", 0), ("b", 3)]), [true, false]);
        assert_eq!(cache.stats().hits, 4);
        assert_eq!(cache.stats().misses, 4);
    }

    #[test]
    fn a_pass_prunes_owners_it_no_longer_sees() {
        let cache = BoardReviewCache::default();
        pass(&cache, "p", &[("a", 0), ("b", 0)]);
        pass(&cache, "q", &[("c", 0)]);
        assert_eq!(cache.stats().entries, 3);
        // `b` left product p's board; q's entry is untouched by p's pass.
        pass(&cache, "p", &[("a", 0)]);
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(pass(&cache, "p", &[("a", 0), ("b", 0)]), [true, false]);
        assert_eq!(pass(&cache, "q", &[("c", 0)]), [true]);
    }

    #[test]
    fn capacity_evicts_the_least_recently_touched_pass_first() {
        let cache = BoardReviewCache::with_capacity(2);
        pass(&cache, "p", &[("a", 0)]);
        pass(&cache, "q", &[("b", 0)]);
        pass(&cache, "r", &[("c", 0)]);
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(cache.stats().evictions, 1);
        assert_eq!(pass(&cache, "p", &[("a", 0)]), [false]);
        assert_eq!(pass(&cache, "r", &[("c", 0)]), [true]);
    }

    #[test]
    fn earlier_pass_cannot_overwrite_later_pass_with_a_lower_key() {
        let cache = BoardReviewCache::default();
        let older = cache.lookup("p", &[("a", vec![("a".into(), 2)])]);
        let newer = cache.lookup("p", &[("a", vec![("a".into(), 1)])]);
        cache.complete("p", newer.pass, vec![("a".into(), vec![("a".into(), 1)], review())]);
        cache.complete("p", older.pass, vec![("a".into(), vec![("a".into(), 2)], review())]);
        assert_eq!(pass(&cache, "p", &[("a", 1)]), [true]);
        assert_eq!(pass(&cache, "p", &[("a", 2)]), [false]);
    }
}
