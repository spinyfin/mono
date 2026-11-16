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
    print_home_sweep(
        "Codex",
        json,
        dry_run,
        older_than_days,
        max_total_bytes,
        HomeSweepCounts::builder()
            .scanned(outcome.scanned)
            .deleted(outcome.deleted)
            .deleted_bytes(outcome.deleted_bytes)
            .skipped_live(outcome.skipped_live)
            .kept_in_policy(outcome.kept_in_policy)
            .errors(outcome.errors)
            .already_absent(outcome.already_absent)
            .build(),
    );
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
    print_home_sweep(
        "Grok",
        json,
        dry_run,
        older_than_days,
        max_total_bytes,
        HomeSweepCounts::builder()
            .scanned(outcome.scanned)
            .deleted(outcome.deleted)
            .deleted_bytes(outcome.deleted_bytes)
            .skipped_live(outcome.skipped_live)
            .kept_in_policy(outcome.kept_in_policy)
            .errors(outcome.errors)
            .already_absent(outcome.already_absent)
            .build(),
    );
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
    print_home_sweep(
        "agy",
        json,
        dry_run,
        older_than_days,
        max_total_bytes,
        HomeSweepCounts::builder()
            .scanned(outcome.scanned)
            .deleted(outcome.deleted)
            .deleted_bytes(outcome.deleted_bytes)
            .skipped_live(outcome.skipped_live)
            .kept_in_policy(outcome.kept_in_policy)
            .errors(outcome.errors)
            .already_absent(outcome.already_absent)
            .build(),
    );
    Ok(())
}

#[derive(serde::Serialize, bon::Builder)]
#[builder(on(String, into))]
struct HomeSweepCounts {
    scanned: u64,
    deleted: u64,
    deleted_bytes: u64,
    skipped_live: u64,
    kept_in_policy: u64,
    errors: u64,
    #[builder(default)]
    already_absent: u64,
}

fn print_home_sweep(
    driver: &str,
    json: bool,
    dry_run: bool,
    older_than_days: u64,
    max_total_bytes: u64,
    counts: HomeSweepCounts,
) {
    if json {
        let mut output = serde_json::to_value(&counts).expect("sweep counts serialize");
        output["dry_run"] = dry_run.into();
        output["older_than_days"] = older_than_days.into();
        output["max_total_bytes"] = max_total_bytes.into();
        println!("{output}");
    } else {
        let action = if dry_run { "would reclaim" } else { "reclaimed" };
        println!(
            "{action} {} recorded {driver} home(s) ({} bytes); scanned={}, skipped_live={}, kept_in_policy={}, already_absent={}, errors={}",
            counts.deleted,
            counts.deleted_bytes,
            counts.scanned,
            counts.skipped_live,
            counts.kept_in_policy,
            counts.already_absent,
            counts.errors
        );
    }
}
