//! On-demand reclaim of recorded Boss-owned per-run driver homes.
//!
//! `bossctl {codex,grok,agy}-homes sweep` — never scans any
//! interactive provider home. A running engine already sweeps on a schedule;
//! these verbs are for cleanup between sweeps or while the engine is stopped.

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

use crate::open_state_db;

#[derive(Subcommand, Debug)]
pub enum CodexHomesAction {
    /// Reclaim recorded Boss-owned `CODEX_HOME` trees past the retention
    /// policy (age, with a total-bytes backstop). Only roots stored on
    /// execution rows are candidates; live executions are never touched.
    Sweep {
        /// Only reclaim homes whose execution age anchor (`finished_at`,
        /// else `created_at`) is older than this many days.
        #[arg(long, default_value_t = boss_engine::codex_rollout_retention::DEFAULT_MAX_AGE_DAYS)]
        older_than_days: u64,
        /// Total-bytes backstop across retained terminal homes: once
        /// exceeded, the oldest are reclaimed first regardless of age.
        #[arg(long, default_value_t = boss_engine::codex_rollout_retention::DEFAULT_MAX_TOTAL_BYTES)]
        max_total_bytes: u64,
        /// Preview what would be deleted without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
pub enum GrokHomesAction {
    /// Reclaim recorded Boss-owned Grok run-container trees past the
    /// retention policy (age, with a total-bytes backstop). Only roots
    /// stored on execution rows are candidates; live executions are never
    /// touched.
    Sweep {
        /// Only reclaim homes whose execution age anchor (`finished_at`,
        /// else `created_at`) is older than this many days.
        #[arg(long, default_value_t = boss_engine::grok_home_retention::DEFAULT_MAX_AGE_DAYS)]
        older_than_days: u64,
        /// Total-bytes backstop across retained terminal homes: once
        /// exceeded, the oldest are reclaimed first regardless of age.
        #[arg(long, default_value_t = boss_engine::grok_home_retention::DEFAULT_MAX_TOTAL_BYTES)]
        max_total_bytes: u64,
        /// Preview what would be deleted without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
pub enum AgyHomesAction {
    /// Reclaim recorded Boss-owned agy run-container trees past the
    /// retention policy (age, with a total-bytes backstop). Only roots
    /// stored on execution rows are candidates; live executions are never
    /// touched.
    Sweep {
        /// Only reclaim homes whose execution age anchor (`finished_at`,
        /// else `created_at`) is older than this many days.
        #[arg(long, default_value_t = boss_engine::agy_home_retention::DEFAULT_MAX_AGE_DAYS)]
        older_than_days: u64,
        /// Total-bytes backstop across retained terminal homes: once
        /// exceeded, the oldest are reclaimed first regardless of age.
        #[arg(long, default_value_t = boss_engine::agy_home_retention::DEFAULT_MAX_TOTAL_BYTES)]
        max_total_bytes: u64,
        /// Preview what would be deleted without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
}

/// `bossctl codex-homes sweep` — on-demand reclaim of recorded Boss-owned
/// CODEX_HOME trees past retention. Never scans `~/.codex`.
pub async fn codex_homes_sweep(
    json: bool,
    state_root: Option<PathBuf>,
    older_than_days: u64,
    max_total_bytes: u64,
    dry_run: bool,
) -> Result<()> {
    let db = open_state_db(state_root)?;
    let policy = boss_engine::codex_rollout_retention::CodexHomeRetentionPolicy::new(
        std::time::Duration::from_secs(older_than_days.saturating_mul(24 * 60 * 60)),
        max_total_bytes,
    );
    let outcome = boss_engine::codex_home_retention_sweep::run_one_pass_with_policy(&db, &policy, dry_run).await;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "scanned": outcome.scanned,
                "deleted": outcome.deleted,
                "deleted_bytes": outcome.deleted_bytes,
                "skipped_live": outcome.skipped_live,
                "kept_in_policy": outcome.kept_in_policy,
                "errors": outcome.errors,
                "dry_run": dry_run,
                "older_than_days": older_than_days,
                "max_total_bytes": max_total_bytes,
            })
        );
    } else if dry_run {
        println!(
            "would reclaim {} recorded Codex home(s) ({} bytes); scanned={}, skipped_live={}, kept_in_policy={}",
            outcome.deleted, outcome.deleted_bytes, outcome.scanned, outcome.skipped_live, outcome.kept_in_policy
        );
    } else {
        println!(
            "reclaimed {} recorded Codex home(s) ({} bytes); scanned={}, skipped_live={}, kept_in_policy={}, errors={}",
            outcome.deleted,
            outcome.deleted_bytes,
            outcome.scanned,
            outcome.skipped_live,
            outcome.kept_in_policy,
            outcome.errors
        );
    }
    Ok(())
}

/// `bossctl grok-homes sweep` — on-demand reclaim of recorded Boss-owned
/// Grok run-container trees past retention. Never scans `~/.grok`. Mirrors
/// [`codex_homes_sweep`].
pub async fn grok_homes_sweep(
    json: bool,
    state_root: Option<PathBuf>,
    older_than_days: u64,
    max_total_bytes: u64,
    dry_run: bool,
) -> Result<()> {
    let db = open_state_db(state_root)?;
    let policy = boss_engine::grok_home_retention::GrokHomeRetentionPolicy::new(
        std::time::Duration::from_secs(older_than_days.saturating_mul(24 * 60 * 60)),
        max_total_bytes,
    );
    let outcome = boss_engine::grok_home_retention_sweep::run_one_pass_with_policy(&db, &policy, dry_run).await;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "scanned": outcome.scanned,
                "deleted": outcome.deleted,
                "deleted_bytes": outcome.deleted_bytes,
                "skipped_live": outcome.skipped_live,
                "kept_in_policy": outcome.kept_in_policy,
                "errors": outcome.errors,
                "dry_run": dry_run,
                "older_than_days": older_than_days,
                "max_total_bytes": max_total_bytes,
            })
        );
    } else if dry_run {
        println!(
            "would reclaim {} recorded Grok home(s) ({} bytes); scanned={}, skipped_live={}, kept_in_policy={}",
            outcome.deleted, outcome.deleted_bytes, outcome.scanned, outcome.skipped_live, outcome.kept_in_policy
        );
    } else {
        println!(
            "reclaimed {} recorded Grok home(s) ({} bytes); scanned={}, skipped_live={}, kept_in_policy={}, errors={}",
            outcome.deleted,
            outcome.deleted_bytes,
            outcome.scanned,
            outcome.skipped_live,
            outcome.kept_in_policy,
            outcome.errors
        );
    }
    Ok(())
}

/// `bossctl agy-homes sweep` — on-demand reclaim of recorded Boss-owned
/// Antigravity run-container trees past retention. Never scans `~/.gemini`.
/// Mirrors [`grok_homes_sweep`].
pub async fn agy_homes_sweep(
    json: bool,
    state_root: Option<PathBuf>,
    older_than_days: u64,
    max_total_bytes: u64,
    dry_run: bool,
) -> Result<()> {
    let db = open_state_db(state_root)?;
    let policy = boss_engine::agy_home_retention::AgyHomeRetentionPolicy::new(
        std::time::Duration::from_secs(older_than_days.saturating_mul(24 * 60 * 60)),
        max_total_bytes,
    );
    let outcome = boss_engine::agy_home_retention_sweep::run_one_pass_with_policy(&db, &policy, dry_run).await;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "scanned": outcome.scanned,
                "deleted": outcome.deleted,
                "deleted_bytes": outcome.deleted_bytes,
                "skipped_live": outcome.skipped_live,
                "kept_in_policy": outcome.kept_in_policy,
                "errors": outcome.errors,
                "dry_run": dry_run,
                "older_than_days": older_than_days,
                "max_total_bytes": max_total_bytes,
            })
        );
    } else if dry_run {
        println!(
            "would reclaim {} recorded agy home(s) ({} bytes); scanned={}, skipped_live={}, kept_in_policy={}",
            outcome.deleted, outcome.deleted_bytes, outcome.scanned, outcome.skipped_live, outcome.kept_in_policy
        );
    } else {
        println!(
            "reclaimed {} recorded agy home(s) ({} bytes); scanned={}, skipped_live={}, kept_in_policy={}, errors={}",
            outcome.deleted,
            outcome.deleted_bytes,
            outcome.scanned,
            outcome.skipped_live,
            outcome.kept_in_policy,
            outcome.errors
        );
    }
    Ok(())
}
