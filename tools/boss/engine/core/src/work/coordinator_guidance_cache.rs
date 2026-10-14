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
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::WorkDb;

const KEY_PREFIX: &str = "coordinator_guidance_cache.";

/// What a successful read of one product's guidance file found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CachedGuidance {
    /// `owner/repo` the read came from. A cached entry is only served for
    /// the repo it was read from, so repointing a product never serves
    /// another repo's rules.
    pub(crate) owner_repo: String,
    /// The commit sha the default branch resolved to.
    pub(crate) git_ref: String,
    /// The file text, or `None` when the file was absent at `git_ref`.
    pub(crate) markdown: Option<String>,
    /// Unix epoch seconds of the successful read.
    pub(crate) read_at: i64,
}

fn key(product_id: &str) -> String {
    format!("{KEY_PREFIX}{product_id}")
}

impl WorkDb {
    /// Replace the product's cached guidance with `entry`.
    pub(crate) fn save_coordinator_guidance_cache(&self, product_id: &str, entry: &CachedGuidance) -> Result<()> {
        let value = serde_json::to_string(entry).context("serialize cached coordinator guidance")?;
        self.connect()?.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key(product_id), value],
        )?;
        Ok(())
    }

    /// The product's cached guidance, if a read ever succeeded.
    pub(crate) fn load_coordinator_guidance_cache(&self, product_id: &str) -> Result<Option<CachedGuidance>> {
        let value: Option<String> = self
            .connect()?
            .query_row("SELECT value FROM metadata WHERE key = ?1", [key(product_id)], |row| {
                row.get(0)
            })
            .optional()?;
        value
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
}
