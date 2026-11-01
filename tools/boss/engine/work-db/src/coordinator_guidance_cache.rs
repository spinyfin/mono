//! Last-known-good coordinator guidance, one entry per product.
//!
//! `BOSS_COORDINATOR.md` is read from GitHub (see
//! [`crate::coordinator_guidance`]); when that read fails the engine falls
//! back to what the last successful read saw rather than leaving the
//! coordinator with no product rules. The entry lives in the engine's own
//! state DB, in the `metadata` key-value table (the same home as the
//! coordinator tmux record), as one JSON value per product. There is no
//! expiry: last-known-good is always better than nothing, and the read
//! time travels with the entry so its age stays visible.

use anyhow::{Context, Result};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::WorkDb;

const KEY_PREFIX: &str = "coordinator_guidance_cache.";

/// What a successful read of one product's guidance file found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedGuidance {
    /// `owner/repo` the read came from. A cached entry is only served for
    /// the repo it was read from, so repointing a product never serves
    /// another repo's rules.
    pub owner_repo: String,
    /// The commit sha the default branch resolved to.
    pub git_ref: String,
    /// The file text, or `None` when the file was absent at `git_ref`.
    pub markdown: Option<String>,
    /// Unix epoch seconds of the successful read.
    pub read_at: i64,
}

fn key(product_id: &str) -> String {
    format!("{KEY_PREFIX}{product_id}")
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Save `entry` as the product's cached guidance unless a read that
    /// started later has already saved one. Concurrent reads of the same
    /// product can finish out of order; a slow, older read must not put
    /// superseded rules back over a newer entry. The comparison happens
    /// inside the one upsert so it is atomic. Equal read times replace.
    /// Returns whether the entry was written.
    pub fn save_coordinator_guidance_cache(&self, product_id: &str, entry: &CachedGuidance) -> Result<bool> {
        let value = serde_json::to_string(entry).context("serialize cached coordinator guidance")?;
        let changed = self.connect()?.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value
             WHERE json_extract(metadata.value, '$.read_at') <= json_extract(excluded.value, '$.read_at')",
            params![key(product_id), value],
        )?;
        Ok(changed > 0)
    }

    /// The product's cached guidance, if a read ever succeeded.
    pub fn load_coordinator_guidance_cache(&self, product_id: &str) -> Result<Option<CachedGuidance>> {
        self.get_metadata(&key(product_id))?
            .map(|value| serde_json::from_str(&value).context("parse cached coordinator guidance"))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(git_ref: &str, markdown: Option<&str>) -> CachedGuidance {
        CachedGuidance {
            owner_repo: "acme/alpha".to_owned(),
            git_ref: git_ref.to_owned(),
            markdown: markdown.map(str::to_owned),
            read_at: 1_759_968_000,
        }
    }

    #[test]
    fn round_trips_per_product_and_a_newer_save_replaces_the_entry() {
        let db = WorkDb::open_in_memory().unwrap();
        assert_eq!(db.load_coordinator_guidance_cache("prod_a").unwrap(), None);

        db.save_coordinator_guidance_cache("prod_a", &entry("sha1", Some("# v1")))
            .unwrap();
        db.save_coordinator_guidance_cache("prod_b", &entry("shaB", None))
            .unwrap();
        assert_eq!(
            db.load_coordinator_guidance_cache("prod_a").unwrap(),
            Some(entry("sha1", Some("# v1")))
        );

        db.save_coordinator_guidance_cache("prod_a", &entry("sha2", Some("# v2")))
            .unwrap();
        assert_eq!(
            db.load_coordinator_guidance_cache("prod_a").unwrap(),
            Some(entry("sha2", Some("# v2")))
        );
        assert_eq!(
            db.load_coordinator_guidance_cache("prod_b").unwrap(),
            Some(entry("shaB", None)),
            "another product's entry is untouched"
        );
    }

    #[test]
    fn an_older_read_finishing_late_does_not_replace_a_newer_entry() {
        let db = WorkDb::open_in_memory().unwrap();
        let newer = CachedGuidance {
            read_at: 2_000,
            ..entry("shaB", Some("# B"))
        };
        let older = CachedGuidance {
            read_at: 1_000,
            ..entry("shaA", Some("# A"))
        };
        assert!(db.save_coordinator_guidance_cache("prod_a", &newer).unwrap());
        assert!(!db.save_coordinator_guidance_cache("prod_a", &older).unwrap());
        assert_eq!(db.load_coordinator_guidance_cache("prod_a").unwrap(), Some(newer));
    }
}
