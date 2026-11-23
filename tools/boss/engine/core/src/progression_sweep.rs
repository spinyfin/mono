//! Periodic sweep that spends an administrative `progression` agent on every
//! work item that is stuck with nothing working it.
//!
//! Design: `tools/boss/docs/designs/task-persistence-parking-and-autonomous-progression.md`
//! section 2.2. A pass reads the `progression_candidates` view (the one
//! definition of "candidate" that the stuck report and `bossctl dispatch
//! diagnose` also read) and mints one `ready` `progression` execution per
//! candidate, oldest stuck first, bound to the stuck item itself
//! (`work_executions.work_item_id`). The execution's own liveness excludes the
//! item from the view (clause 2), so a second pass cannot mint a second run for
//! an item that already has one.
//!
//! ## The escalation ladder
//!
//! Before minting, a pass asks the escalation ladder (`progression_ladder` in
//! work-db) about every candidate, and about every item the view stopped
//! nominating because its last two runs died. An item whose attempts have run
//! out is **not** given another run: the engine files a multiple-choice question
//! ("Park it, archive it, or keep trying?") on it, parks it
//! `awaiting_operator_answer`, and the pass counts it as `asked`. The question
//! is the escalation, so a pass never gives up silently and never loops.
//!
//! ## What the sweep does not do
//!
//! - **It does not decide anything.** Whether an item is a candidate is the
//!   view's business, and whether its attempts have run out is the ladder's; the
//!   sweep neither filters nor second-guesses either.
//! - **It does not spawn anything.** Minting a row only queues it. The dispatcher
//!   routes it to the progression pool (`progression.pool_size` slots, invisible
//!   to the interactive cap and to Lower Decks spillover) and holds it behind both
//!   the operator and the breaker pause. Until the engine has a progression
//!   worker kind, the dispatcher holds every such row `ready` and none can spawn.
//! - **It does not cap itself.** When more items are stuck than the pool can
//!   run, the surplus waits as queued `ready` rows, oldest first, rather than
//!   being dropped or deferred by a limit of the sweep's own; the progression
//!   queue gauges (`dispatch.queue_depth.progression`,
//!   `dispatch.queue_oldest_wait_seconds.progression`) show the backlog.
//!
//! ## Failed launches and the backoff
//!
//! A progression run whose agent never started is not an attempt on its item
//! (`progression_ladder`, "Failed launches"): it starts a separate launch
//! backoff (2 minutes, doubling to a 30-minute cap), but it never files the
//! ladder's question, so a driver whose launch is broken cannot park the items
//! it is given on the owner. Each item is nominated again when its launch
//! backoff ends, with no limit on how many times. An agent starting resets it.
//!
//! That wait must not be silent. After every pass the sweep reads
//! [`WorkDb::progression_launch_backoff`] and publishes it on the
//! [`LaunchBackoffSignal`] the engine health report renders
//! ([`launch_backoff_health_issue`]): how many items are waiting, the error,
//! and when the next attempt is due. It is derived from the database on each
//! pass, so an engine restart does not lose it, and it clears on the first pass
//! after an agent starts or the last such backoff ends.
//!
//! A pass also withdraws any infrastructure-failure question that was filed
//! for failed launches before they stopped being counted, so those items are
//! nominated again without the owner answering each one.
//!
//! ## Gating
//!
//! The `progression_agents` feature flag (default off) is re-checked on every
//! pass, so turning it off stops minting within one interval without a restart.
//! The limits the view reads are calibrated at startup and daily, whatever the flag says,
//! so the report and `diagnose` apply this engine's numbers either way.
use std::sync::Arc;
use std::time::Duration;

use boss_engine_storage_values::progression::{
    METADATA_KEY_SWEEP_INTERVAL_SECS, PROGRESSION_AGENTS_FLAG, ProgressionSettings,
};
use boss_protocol::EngineHealthIssue;

use crate::work::{ProgressionLaunchBackoff, WorkDb};

/// Where the sweep publishes progression's launch backoff for the engine
/// health report: `Some` while stuck items are waiting out the backoff a
/// failed launch started, `None` otherwise. A watch channel so the health
/// broadcaster pushes a fresh report on every change without polling.
pub type LaunchBackoffSignal = tokio::sync::watch::Sender<Option<ProgressionLaunchBackoff>>;

/// A [`LaunchBackoffSignal`] that reports no backoff until a pass says so.
pub fn launch_backoff_signal() -> Arc<LaunchBackoffSignal> {
    Arc::new(tokio::sync::watch::channel(None).0)
}

/// Wake health rendering on changes, and once a minute during an unchanged
/// backoff so relative retry and failure ages keep advancing.
pub async fn wait_for_backoff_health_refresh(
    changes: &mut tokio::sync::watch::Receiver<Option<ProgressionLaunchBackoff>>,
) -> Result<(), tokio::sync::watch::error::RecvError> {
    let active = changes.borrow().is_some();
    tokio::select! {
        changed = changes.changed() => changed,
        _ = tokio::time::sleep(Duration::from_secs(60)), if active => Ok(()),
    }
}

/// `EngineHealthIssue::kind` of the backoff notice in the engine health report.
pub const LAUNCH_BACKOFF_ISSUE_KIND: &str = "progression_launch_backoff";

/// "in about 3 hours", "in about 5 minutes", or "now" for a moment that has
/// arrived. Banner text never carries a raw timestamp.
fn due_phrase(epoch_s: i64, now_epoch_secs: i64) -> String {
    let secs = epoch_s.saturating_sub(now_epoch_secs);
    let plural = |n: i64, unit: &str| format!("in about {n} {unit}{}", if n == 1 { "" } else { "s" });
    match secs {
        ..=0 => "now".to_owned(),
        1..=59 => "in under a minute".to_owned(),
        60..=5399 => plural((secs + 30) / 60, "minute"),
        _ => plural((secs + 1800) / 3600, "hour"),
    }
}

/// Render the backoff as the engine-health issue the app banner and `bossctl`
/// show. The engine owns the wording; the app renders it verbatim.
pub fn launch_backoff_health_issue(backoff: &ProgressionLaunchBackoff, now_epoch_secs: i64) -> EngineHealthIssue {
    let items = match backoff.waiting_items {
        1 => "1 stuck item".to_owned(),
        n => format!("{n} stuck items"),
    };
    let next = due_phrase(backoff.next_attempt_epoch_s, now_epoch_secs);
    let failed = boss_engine_utils::iso8601::format_paused_since_phrase(backoff.latest_failure_epoch_s, now_epoch_secs);
    let failed = if failed.is_empty() {
        "just now".to_owned()
    } else {
        failed
    };
    EngineHealthIssue {
        kind: LAUNCH_BACKOFF_ISSUE_KIND.to_owned(),
        severity: "warning".to_owned(),
        title: format!("Progression is backing off: its agent failed to start for {items}; next attempt {next}"),
        body: format!(
            "Latest error ({failed}, execution {execution}):\n{error}\n\n\
             Boss could not start a progression agent for {items}, so nothing is working them. No agent \
             looked at those items, so the failed starts are not counted against them and nothing is asked \
             of you. Each item is tried again when its launch backoff ends; the next attempt \
             is due {next}. This notice clears itself once an agent starts, or when no item is waiting.",
            execution = backoff.latest_execution_id,
            error = backoff.latest_error,
        ),
    }
}

/// Counters from one sweep pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProgressionSweepOutcome {
    /// Executions minted: one per candidate the view named.
    pub minted: usize,
    /// Candidates the mint failed for; logged, and retried on the next pass.
    pub failed: usize,
    /// Items the escalation ladder asked the owner about instead of minting a
    /// run for.
    pub asked: usize,
    /// Items whose infrastructure-failure question the engine withdrew because
    /// the runs it counted never launched.
    pub retracted: usize,
    /// Progression's launch backoff after this pass, if any item is waiting on
    /// one.
    pub launch_backoff: Option<ProgressionLaunchBackoff>,
}

impl crate::sweep_loop::SweepOutcome for ProgressionSweepOutcome {
    fn has_activity(&self) -> bool {
        self.minted > 0 || self.failed > 0 || self.asked > 0 || self.retracted > 0
    }

    fn log(&self) {
        tracing::info!(
            minted = self.minted,
            failed = self.failed,
            asked = self.asked,
            retracted = self.retracted,
            "progression sweep: pass complete",
        );
    }
}

/// Spawn the sweep: publish the view's limits, then run [`run_one_pass`] every
/// `settings.sweep_interval_secs`, firing immediately so candidates left behind
/// by a previous engine run are queued at boot.
///
/// `kick_fn` runs whenever a pass minted at least one execution, so the
/// scheduler picks it up at once rather than on its next heartbeat.
/// `feature_flags` is re-checked every pass. `launch_backoff` receives the
/// backoff each pass found (`None` while the flag is off: nothing is being
/// retried, so nothing is backing off).
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    settings: ProgressionSettings,
    kick_fn: Arc<dyn Fn() + Send + Sync>,
    feature_flags: Arc<crate::feature_flags::FeatureFlagsStore>,
    launch_backoff: Arc<LaunchBackoffSignal>,
) -> tokio::task::JoinHandle<()> {
    let initialized = false;
    let cadence_db = work_db.clone();
    let initialized = Arc::new(std::sync::atomic::AtomicBool::new(initialized));
    crate::sweep_loop::spawn_adaptive_sweep_loop(
        move || {
            let work_db = work_db.clone();
            let feature_flags = feature_flags.clone();
            let kick_fn = kick_fn.clone();
            let initialized = initialized.clone();
            let launch_backoff = launch_backoff.clone();
            async move {
                use std::sync::atomic::Ordering;
                if !initialized.load(Ordering::Relaxed) {
                    let db = work_db.clone();
                    let result =
                        tokio::task::spawn_blocking(move || db.configure_progression_calibration(&settings)).await;
                    match result {
                        Ok(Ok(())) => {
                            initialized.store(true, Ordering::Relaxed);
                            let db = work_db.clone();
                            tokio::task::spawn_blocking(move || {
                                if let Err(error) =
                                    db.calibrate_progression(boss_engine_utils::epoch_time::now_epoch_secs())
                                {
                                    tracing::warn!(
                                        ?error,
                                        "initial progression measurement failed; using published limits"
                                    );
                                }
                            });
                        }
                        error => {
                            tracing::error!(?error, "progression configuration retry failed; skipping nominations")
                        }
                    }
                }
                let outcome =
                    if initialized.load(Ordering::Relaxed) && feature_flags.is_enabled(PROGRESSION_AGENTS_FLAG) {
                        run_one_pass(work_db.clone()).await
                    } else {
                        ProgressionSweepOutcome::default()
                    };
                if outcome.minted > 0 {
                    kick_fn();
                }
                let changed = launch_backoff.send_if_modified(|published| {
                    let changed = *published != outcome.launch_backoff;
                    if changed {
                        published.clone_from(&outcome.launch_backoff);
                    }
                    changed
                });
                if let Some(backoff) = &outcome.launch_backoff {
                    if changed {
                        tracing::warn!(
                            waiting_items = backoff.waiting_items,
                            next_attempt_epoch_s = backoff.next_attempt_epoch_s,
                            latest_execution_id = %backoff.latest_execution_id,
                            latest_error = %backoff.latest_error,
                            "progression sweep: backing off after failed agent launches; each waiting item is \
                             retried when its launch backoff ends",
                        );
                    } else {
                        tracing::debug!(?backoff, "progression sweep: launch backoff unchanged");
                    }
                }
                let interval = work_db
                    .progression_limit(METADATA_KEY_SWEEP_INTERVAL_SECS, settings.sweep_interval_secs)
                    .unwrap_or_else(|error| {
                        tracing::warn!(?error, "failed to read progression cadence");
                        settings.sweep_interval_secs
                    });
                (outcome, Duration::from_secs(interval.max(1)))
            }
        },
        move || {
            let db = cadence_db.clone();
            async move {
                match tokio::task::spawn_blocking(move || {
                    db.progression_limit(METADATA_KEY_SWEEP_INTERVAL_SECS, settings.sweep_interval_secs)
                })
                .await
                {
                    Ok(Ok(seconds)) => Some(Duration::from_secs(seconds.max(1))),
                    error => {
                        tracing::warn!(?error, "failed to refresh progression cadence");
                        None
                    }
                }
            }
        },
    )
}

/// Run one pass: mint a `progression` execution for each current candidate,
/// oldest stuck first. The work is database-bound (the view is a join over the
/// task graph), so it runs off the async runtime's threads.
pub async fn run_one_pass(work_db: Arc<WorkDb>) -> ProgressionSweepOutcome {
    match tokio::task::spawn_blocking(move || run_one_pass_blocking(&work_db)).await {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(?error, "progression sweep: pass task failed");
            ProgressionSweepOutcome::default()
        }
    }
}

fn run_one_pass_blocking(work_db: &WorkDb) -> ProgressionSweepOutcome {
    let mut outcome = ProgressionSweepOutcome::default();
    let mint = match work_db.mint_progression_executions() {
        Ok(mint) => mint,
        Err(error) => {
            tracing::warn!(
                ?error,
                "progression sweep: failed to queue progression runs; skipping pass"
            );
            return outcome;
        }
    };
    for (candidate, execution) in &mint.minted {
        outcome.minted += 1;
        tracing::info!(
            work_item_id = %candidate.work_item_id,
            execution_id = %execution.id,
            stuck_kind = %candidate.stuck_kind,
            stuck_since = candidate.stuck_since,
            grace_waived = candidate.grace_waived,
            "progression sweep: queued a progression run",
        );
    }
    for question in &mint.asked {
        outcome.asked += 1;
        tracing::info!(
            work_item_id = %question.work_item_id,
            question_id = %question.question_id,
            reason = question.reason.as_str(),
            "progression sweep: the ladder ran out; asked the owner instead of queueing another run",
        );
    }
    for (item, reason) in &mint.ladder_failed {
        outcome.failed += 1;
        tracing::warn!(
            work_item_id = %item,
            %reason,
            "progression sweep: the ladder could not file or withdraw its question for an item; will retry",
        );
    }
    for (candidate, reason) in &mint.failed {
        outcome.failed += 1;
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            %reason,
            "progression sweep: failed to queue a progression run",
        );
    }
    for item in &mint.retracted {
        outcome.retracted += 1;
        tracing::info!(
            work_item_id = %item,
            "progression sweep: withdrew the question filed for runs that never launched; the item is \
             nominated again",
        );
    }
    // Read after the mint: an item just given a run is no longer waiting.
    outcome.launch_backoff = work_db.progression_launch_backoff().unwrap_or_else(|error| {
        tracing::warn!(?error, "progression sweep: failed to read the launch backoff");
        None
    });
    outcome
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use boss_engine_storage_values::progression::{METADATA_KEY_COOLDOWN_SECS, METADATA_KEY_STUCK_AFTER_SECS};
    use boss_protocol::{ExecutionKind, ExecutionStatus};

    use super::*;
    use crate::feature_flags::FeatureFlagsStore;
    use crate::test_support::{create_test_chore_manual, create_test_product, open_db};

    /// A chore stuck on a merge conflict for `hours` hours.
    fn stuck(db: &WorkDb, product: &str, name: &str, hours: i64) -> String {
        let id = create_test_chore_manual(db, product, name).id;
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'merge_conflict' WHERE id = ?1",
            [&id],
        )
        .unwrap();
        conn.execute(
            "UPDATE task_status_audit SET changed_at = CAST(changed_at AS INTEGER) - ?2 WHERE work_item_id = ?1",
            rusqlite::params![id, hours * 3600],
        )
        .unwrap();
        id
    }

    fn progression_runs(db: &WorkDb, id: &str) -> Vec<boss_protocol::WorkExecution> {
        db.list_executions(Some(id))
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == ExecutionKind::Progression)
            .collect()
    }

    fn flags(enabled: bool) -> (tempfile::TempDir, Arc<FeatureFlagsStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FeatureFlagsStore::new(dir.path().join("feature-flags.toml")));
        if enabled {
            store.set(PROGRESSION_AGENTS_FLAG, true).unwrap();
        }
        (dir, store)
    }

    #[tokio::test]
    async fn a_pass_queues_one_run_per_item_and_never_a_second_while_one_is_live() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db).id;
        let oldest = stuck(&db, &product, "Oldest", 30);
        let newest = stuck(&db, &product, "Newest", 3);
        let young = stuck(&db, &product, "Inside the grace", 0);
        let healthy = create_test_chore_manual(&db, &product, "Healthy").id;
        let db = Arc::new(db);

        let outcome = run_one_pass(db.clone()).await;
        assert_eq!(
            outcome,
            ProgressionSweepOutcome {
                minted: 2,
                ..Default::default()
            }
        );
        for id in [&oldest, &newest] {
            let runs = progression_runs(&db, id);
            assert_eq!(runs.len(), 1, "{id}");
            assert_eq!(runs[0].status, ExecutionStatus::Ready);
            assert_eq!(runs[0].work_item_id, *id, "bound to the stuck item");
        }
        for id in [&young, &healthy] {
            assert!(progression_runs(&db, id).is_empty(), "{id} is not a candidate");
        }
        // Oldest stuck is first in the queue the dispatcher walks.
        let ready: Vec<_> = db
            .list_ready_executions()
            .unwrap()
            .into_iter()
            .map(|e| e.work_item_id)
            .collect();
        assert_eq!(ready, vec![oldest.clone(), newest.clone()]);

        // Nothing is minted while a run is queued, running, or inside its cooldown.
        assert_eq!(run_one_pass(db.clone()).await, ProgressionSweepOutcome::default());
        let run = progression_runs(&db, &oldest).remove(0);
        for status in ["running", "completed"] {
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE work_executions SET status = ?2, finished_at = strftime('%s','now') WHERE id = ?1",
                    rusqlite::params![run.id, status],
                )
                .unwrap();
            assert_eq!(run_one_pass(db.clone()).await.minted, 0, "{status}");
        }
        assert_eq!(progression_runs(&db, &oldest).len(), 1);
    }

    /// A stuck item whose progression agent failed to start a minute ago is
    /// neither given up on nor silently skipped: the pass reports the backoff,
    /// the loop publishes it for the health report, and it reads as a plain
    /// sentence with no raw timestamp.
    #[tokio::test]
    async fn a_pass_reports_the_launch_backoff_and_the_loop_publishes_it() {
        const ERROR: &str = "no driver hook or session event arrived within 120s after prompt delivery";
        let (_dir, db) = open_db();
        let product = create_test_product(&db).id;
        let id = stuck(&db, &product, "Its agent failed to start", 3);
        let run = db
            .create_execution(
                boss_protocol::CreateExecutionInput::builder()
                    .created_via("test")
                    .work_item_id(&id)
                    .kind(ExecutionKind::Progression)
                    .status(ExecutionStatus::Failed)
                    .build(),
            )
            .unwrap();
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET pre_start_failure_count = 1, last_error = ?3,
                    created_at = ?2, started_at = ?2, finished_at = ?2 WHERE id = ?1",
                rusqlite::params![run.id, now - 60, ERROR],
            )
            .unwrap();
        let db = Arc::new(db);

        let outcome = run_one_pass(db.clone()).await;

        assert_eq!(
            (outcome.minted, outcome.asked),
            (0, 0),
            "inside the launch backoff: {outcome:?}"
        );
        let backoff = outcome.launch_backoff.clone().expect("the pass reports the backoff");
        assert_eq!(
            (backoff.waiting_items, backoff.latest_execution_id.as_str()),
            (1, run.id.as_str())
        );
        assert!(!crate::sweep_loop::SweepOutcome::has_activity(&outcome));

        let issue = launch_backoff_health_issue(&backoff, now);
        assert_eq!(
            (issue.kind.as_str(), issue.severity.as_str()),
            (LAUNCH_BACKOFF_ISSUE_KIND, "warning")
        );
        assert_eq!(
            issue.title,
            "Progression is backing off: its agent failed to start for 1 stuck item; next attempt in about 1 minute"
        );
        for expected in [ERROR, run.id.as_str(), "1 minute ago", "nothing is asked of you"] {
            assert!(issue.body.contains(expected), "{expected}: {}", issue.body);
        }
        assert!(!issue.title.contains('Z') && !issue.body.contains("1970"), "{issue:?}");

        // On: the loop's first pass publishes it. Off: nothing is being
        // retried, so nothing is backing off.
        let signal = launch_backoff_signal();
        let mut changes = signal.subscribe();
        for (enabled, published) in [(true, Some(backoff)), (false, None)] {
            let (_flags_dir, store) = flags(enabled);
            let handle = spawn_loop(
                db.clone(),
                ProgressionSettings::default(),
                Arc::new(|| {}),
                store,
                signal.clone(),
            );
            tokio::time::timeout(Duration::from_secs(10), changes.changed())
                .await
                .expect("the loop publishes the change")
                .unwrap();
            handle.abort();
            assert_eq!(*signal.borrow(), published, "flag on: {enabled}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unchanged_backoff_refreshes_the_relative_retry_estimate() {
        let now = 1_000_000;
        let backoff = ProgressionLaunchBackoff {
            waiting_items: 1,
            next_attempt_epoch_s: now + 30 * 60,
            latest_execution_id: "exec_waiting".to_owned(),
            latest_failure_epoch_s: now,
            latest_error: "slot occupied".to_owned(),
        };
        let signal = launch_backoff_signal();
        signal.send_replace(Some(backoff.clone()));
        let mut changes = signal.subscribe();
        let before = launch_backoff_health_issue(&backoff, now);
        let started = tokio::time::Instant::now();
        for _ in 0..10 {
            wait_for_backoff_health_refresh(&mut changes).await.unwrap();
        }
        assert_eq!(*changes.borrow(), Some(backoff.clone()));
        assert!(!changes.has_changed().unwrap(), "no sweep update was needed");
        let after = launch_backoff_health_issue(&backoff, now + started.elapsed().as_secs() as i64);
        assert!(before.title.contains("in about 30 minutes"), "{before:?}");
        assert!(after.title.contains("in about 20 minutes"), "{after:?}");
        assert!(after.body.contains("10 minutes ago"), "{after:?}");

        signal.send_replace(None);
        wait_for_backoff_health_refresh(&mut changes).await.unwrap();
        assert!(changes.borrow().is_none(), "clearing the notice wakes immediately");
        assert!(
            tokio::time::timeout(Duration::from_secs(120), wait_for_backoff_health_refresh(&mut changes))
                .await
                .is_err(),
            "inactive backoff does not refresh periodically"
        );
    }

    #[test]
    fn the_next_attempt_reads_as_a_rounded_wait_never_a_timestamp() {
        for (secs, phrase) in [
            (-5, "now"),
            (0, "now"),
            (20, "in under a minute"),
            (60, "in about 1 minute"),
            (300, "in about 5 minutes"),
            (5399, "in about 90 minutes"),
            (5400, "in about 2 hours"),
            (6 * 3600, "in about 6 hours"),
        ] {
            assert_eq!(due_phrase(1_000 + secs, 1_000), phrase, "{secs}s");
        }
    }

    #[tokio::test]
    async fn a_pass_asks_the_owner_instead_of_queueing_a_run_the_ladder_has_ruled_out() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db).id;
        let spent = stuck(&db, &product, "Two attempts spent", 30);
        let fresh = stuck(&db, &product, "Not yet tried", 3);
        for ago in [20 * 3600, 10 * 3600] {
            let run = db
                .create_execution(
                    boss_protocol::CreateExecutionInput::builder()
                        .created_via("test")
                        .work_item_id(&spent)
                        .kind(ExecutionKind::Progression)
                        .status(ExecutionStatus::Completed)
                        .build(),
                )
                .unwrap();
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE work_executions SET progression_disposition = 'progressed', started_at = ?2,
                        created_at = ?2, finished_at = ?2 WHERE id = ?1",
                    rusqlite::params![run.id, boss_engine_utils::epoch_time::now_epoch_secs() - ago],
                )
                .unwrap();
        }
        let db = Arc::new(db);

        let outcome = run_one_pass(db.clone()).await;

        // The item with nothing left to try is asked about; the other gets its first run.
        assert_eq!(
            outcome,
            ProgressionSweepOutcome {
                minted: 1,
                asked: 1,
                ..Default::default()
            }
        );
        assert_eq!(progression_runs(&db, &spent).len(), 2, "no third run");
        assert_eq!(progression_runs(&db, &fresh).len(), 1);
        let task = db.get_work_item(&spent).unwrap();
        let boss_protocol::WorkItem::Chore(task) = task else {
            panic!("a chore");
        };
        assert_eq!(task.blocked_reason.as_deref(), Some("awaiting_operator_answer"));
        // A pass is idempotent: the open question is the hold.
        assert_eq!(run_one_pass(db).await.asked, 0);
    }

    #[tokio::test]
    async fn the_loop_mints_only_while_the_flag_is_on_and_kicks_the_scheduler() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db).id;
        let id = stuck(&db, &product, "Stuck", 3);
        let db = Arc::new(db);
        let kicks = Arc::new(AtomicUsize::new(0));
        let kick: Arc<dyn Fn() + Send + Sync> = {
            let kicks = Arc::clone(&kicks);
            Arc::new(move || {
                kicks.fetch_add(1, Ordering::SeqCst);
            })
        };

        // Off (the default): nothing is minted and the scheduler is not kicked.
        let (_off_dir, off) = flags(false);
        assert!(!off.is_enabled(PROGRESSION_AGENTS_FLAG));
        let handle = spawn_loop(
            db.clone(),
            ProgressionSettings::default(),
            kick.clone(),
            off,
            launch_backoff_signal(),
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.abort();
        assert!(progression_runs(&db, &id).is_empty());
        assert_eq!(kicks.load(Ordering::SeqCst), 0);

        // On: the first pass fires immediately.
        let (_on_dir, on) = flags(true);
        let handle = spawn_loop(
            db.clone(),
            ProgressionSettings::default(),
            kick,
            on,
            launch_backoff_signal(),
        );
        for _ in 0..100 {
            // The kick follows the mint, so wait for both before aborting.
            if !progression_runs(&db, &id).is_empty() && kicks.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        handle.abort();
        assert_eq!(progression_runs(&db, &id).len(), 1);
        assert_eq!(kicks.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn measurement_failure_does_not_disable_nominations() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db).id;
        let id = stuck(&db, &product, "Stuck despite calibration failure", 3);
        db.connect()
            .unwrap()
            .execute_batch("DROP TABLE task_stuck_daily_snapshots")
            .unwrap();
        let db = Arc::new(db);
        let (_flags_dir, on) = flags(true);
        let handle = spawn_loop(
            db.clone(),
            ProgressionSettings::default(),
            Arc::new(|| {}),
            on,
            launch_backoff_signal(),
        );
        for _ in 0..100 {
            if !progression_runs(&db, &id).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        handle.abort();
        assert_eq!(progression_runs(&db, &id).len(), 1);
    }

    #[tokio::test]
    async fn spawning_publishes_the_limits_the_views_read_whatever_the_flag_says() {
        let (_dir, db) = open_db();
        let db = Arc::new(db);
        let settings = ProgressionSettings::builder()
            .stuck_after_secs(7 * 3600)
            .cooldown_secs(11 * 3600)
            .build();
        let (_flags_dir, off) = flags(false);
        let handle = spawn_loop(db.clone(), settings, Arc::new(|| {}), off, launch_backoff_signal());
        for _ in 0..100 {
            if db.get_metadata(METADATA_KEY_COOLDOWN_SECS).unwrap().as_deref() == Some("39600") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        handle.abort();
        assert_eq!(
            db.get_metadata(METADATA_KEY_STUCK_AFTER_SECS).unwrap().as_deref(),
            Some("25200")
        );
        assert_eq!(
            db.get_metadata(METADATA_KEY_COOLDOWN_SECS).unwrap().as_deref(),
            Some("39600")
        );
    }
}
