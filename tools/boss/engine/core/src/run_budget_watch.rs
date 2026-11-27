//! Runaway-run budget alarm — fires a one-time operator-visible attention
//! item when a live run passes its token or round budget.
//!
//! ## Signal only — never interrupt
//!
//! Mirroring [`crate::envelope_watch`], this sweep is detection, not
//! enforcement: it never stops, kills, or reaps the worker. It reads the cost
//! snapshot that `run_cost` already tails onto `work_runs` for live runs only
//! and files at most one `run_budget_overrun` attention item per execution.
//!
//! ## Kill switch
//!
//! Gated by the `run_budget_alarm` feature flag (default ON). Flipping it off
//! makes every pass a no-op with zero DB reads.

use std::sync::Arc;
use std::time::Duration;

use boss_protocol::CreateAttentionItemInput;

use crate::feature_flags::FeatureFlagsStore;
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::work::{LiveRunCostSnapshot, WorkDb};

/// Default token budget per live run (25 million tokens across input, output,
/// and cache). Passing this threshold files a one-time attention item.
pub const DEFAULT_TOKEN_BUDGET: i64 = 25_000_000;

/// Default round budget per live run (150 assistant/turn rounds). Passing this
/// threshold files a one-time attention item.
pub const DEFAULT_ROUND_BUDGET: i64 = 150;

/// Feature flag gating the runaway-run budget alarm (default ON).
pub const RUN_BUDGET_ALARM_FLAG: &str = "run_budget_alarm";

pub use boss_engine_storage_values::attention_kinds::RUN_BUDGET_OVERRUN_ATTENTION_KIND;

/// Token and round thresholds for [`run_one_pass`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunBudgetThresholds {
    pub max_tokens: i64,
    pub max_rounds: i64,
}

impl Default for RunBudgetThresholds {
    fn default() -> Self {
        Self {
            max_tokens: DEFAULT_TOKEN_BUDGET,
            max_rounds: DEFAULT_ROUND_BUDGET,
        }
    }
}

/// Counts from one budget-watch sweep pass; logged at `info` when a new
/// overrun item is filed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RunBudgetSweepOutcome {
    /// Whether the pass was skipped because `run_budget_alarm` is disabled.
    pub disabled: bool,
    /// New budget-overrun attention items filed this pass.
    pub signaled: usize,
    /// Live executions observed over either budget (includes already-signaled).
    pub over_budget: usize,
    /// Over-budget executions that already carried a `run_budget_overrun` item.
    pub already_signaled: usize,
    /// Live executions checked and found within both budgets.
    pub within_budget: usize,
}

impl crate::sweep_loop::SweepOutcome for RunBudgetSweepOutcome {
    fn has_activity(&self) -> bool {
        self.signaled > 0
    }

    fn log(&self) {
        tracing::info!(
            signaled = self.signaled,
            over_budget = self.over_budget,
            already_signaled = self.already_signaled,
            within_budget = self.within_budget,
            "run-budget-watch: filed budget-overrun signal(s)",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`, firing
/// once immediately on boot.
///
/// Each pass runs its synchronous SQLite work on the blocking pool.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    live_states: Arc<LiveWorkerStateRegistry>,
    feature_flags: Arc<FeatureFlagsStore>,
    thresholds: RunBudgetThresholds,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let live_states = Arc::clone(&live_states);
        let feature_flags = Arc::clone(&feature_flags);
        async move {
            let pass = crate::work::db_diagnostics::spawn_blocking(move || {
                run_one_pass(
                    work_db.as_ref(),
                    live_states.as_ref(),
                    feature_flags.as_ref(),
                    &thresholds,
                )
            })
            .await;
            match pass {
                Ok(outcome) => outcome,
                Err(err) => {
                    tracing::error!(?err, "run-budget-watch: sweep pass did not complete");
                    RunBudgetSweepOutcome::default()
                }
            }
        }
    })
}

/// Run a single budget-watch pass over live worker slots.
pub fn run_one_pass(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    feature_flags: &FeatureFlagsStore,
    thresholds: &RunBudgetThresholds,
) -> RunBudgetSweepOutcome {
    if !feature_flags.is_enabled(RUN_BUDGET_ALARM_FLAG) {
        return RunBudgetSweepOutcome {
            disabled: true,
            ..Default::default()
        };
    }

    let states = live_states.snapshot();
    if states.is_empty() {
        return RunBudgetSweepOutcome::default();
    }

    let mut execution_ids: Vec<String> = states.into_iter().map(|s| s.run_id).collect();
    execution_ids.sort_unstable();
    execution_ids.dedup();

    let snapshots = match work_db.live_run_cost_snapshots(&execution_ids) {
        Ok(snapshots) => snapshots,
        Err(err) => {
            tracing::warn!(
                ?err,
                "run-budget-watch: failed to load live run cost snapshots; skipping pass"
            );
            return RunBudgetSweepOutcome::default();
        }
    };

    let mut outcome = RunBudgetSweepOutcome::default();
    for snapshot in &snapshots {
        let tokens_over = snapshot.total_tokens.is_some_and(|t| t > thresholds.max_tokens);
        let rounds_over = snapshot.rounds.is_some_and(|r| r > thresholds.max_rounds);

        if !tokens_over && !rounds_over {
            outcome.within_budget += 1;
            continue;
        }

        outcome.over_budget += 1;
        if crate::envelope_watch::execution_has_attention(
            work_db,
            &snapshot.execution_id,
            RUN_BUDGET_OVERRUN_ATTENTION_KIND,
            false,
            "run-budget-watch: attention lookup failed; skipping signal this pass",
        ) {
            outcome.already_signaled += 1;
            continue;
        }

        file_budget_attention(work_db, snapshot, thresholds, tokens_over, rounds_over);
        outcome.signaled += 1;
    }

    outcome
}

fn file_budget_attention(
    work_db: &WorkDb,
    snapshot: &LiveRunCostSnapshot,
    thresholds: &RunBudgetThresholds,
    tokens_over: bool,
    rounds_over: bool,
) {
    let reason = match (tokens_over, rounds_over) {
        (true, true) => "token and round budgets",
        (true, false) => "token budget",
        (false, true) => "round budget",
        (false, false) => return,
    };
    let title = format!("Worker passed its {reason}");
    let tokens_line = snapshot.total_tokens.map_or_else(
        || "unavailable".to_owned(),
        |t| format!("{t} (budget {})", thresholds.max_tokens),
    );
    let rounds_line = snapshot.rounds.map_or_else(
        || "unavailable".to_owned(),
        |r| format!("{r} (budget {})", thresholds.max_rounds),
    );
    let body = format!(
        "This live execution has passed its {reason}.\n\n\
         This is an informational signal, not an interruption: the worker keeps running. \
         Review the live run to confirm it is making forward progress rather than looping.\n\n\
         - work item: `{}`\n\
         - execution: `{}`\n\
         - tokens: {}\n\
         - rounds: {}\n",
        snapshot.work_item_id, snapshot.execution_id, tokens_line, rounds_line,
    );

    if let Err(err) = work_db.create_attention_item(
        CreateAttentionItemInput::builder()
            .kind(RUN_BUDGET_OVERRUN_ATTENTION_KIND)
            .title(title)
            .body_markdown(body)
            .execution_id(snapshot.execution_id.clone())
            .status("open")
            .build(),
    ) {
        tracing::warn!(
            execution_id = %snapshot.execution_id,
            ?err,
            "run-budget-watch: failed to file budget-overrun attention item (non-fatal)"
        );
    }
}

#[cfg(test)]
mod tests {
    use boss_engine_storage_values::RunCostSnapshot;
    use boss_protocol::WorkItemBinding;

    use super::*;
    use crate::test_support::{create_active_chore, create_product, create_spawned_execution, open_db};

    fn make_flags(enabled: bool) -> (tempfile::TempDir, FeatureFlagsStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FeatureFlagsStore::new(dir.path().join("feature-flags.toml"));
        store.set(RUN_BUDGET_ALARM_FLAG, enabled).unwrap();
        (dir, store)
    }

    fn register_live_slot(live_states: &LiveWorkerStateRegistry, slot_id: u8, execution_id: &str, work_item_id: &str) {
        live_states.register_spawn(
            slot_id,
            execution_id,
            "claude-opus-4-8",
            std::process::id() as i32,
            Some(WorkItemBinding {
                work_item_id: work_item_id.to_owned(),
                work_item_name: "test chore".to_owned(),
                execution_id: execution_id.to_owned(),
            }),
        );
    }

    fn set_cost(
        db: &WorkDb,
        execution_id: &str,
        input_tokens: Option<i64>,
        cache_read: Option<i64>,
        rounds: Option<i64>,
    ) {
        let updated = db
            .set_run_cost_snapshot(
                execution_id,
                RunCostSnapshot::builder()
                    .maybe_input_tokens(input_tokens)
                    .maybe_cache_read_tokens(cache_read)
                    .maybe_rounds(rounds)
                    .build(),
            )
            .unwrap();
        assert!(updated, "expected work_runs row for {execution_id}");
    }

    fn budget_items_count(db: &WorkDb, execution_id: &str) -> usize {
        db.list_attention_items(execution_id)
            .unwrap()
            .into_iter()
            .filter(|i| i.kind == RUN_BUDGET_OVERRUN_ATTENTION_KIND)
            .count()
    }

    #[test]
    fn token_budget_exceeded_files_once_per_execution() {
        let (_dir, db) = open_db();
        let (_flag_dir, flags) = make_flags(true);
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "token heavy chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, 60);
        set_cost(&db, &execution_id, Some(10_000_000), Some(15_000_001), Some(20));

        let live_states = LiveWorkerStateRegistry::new();
        register_live_slot(&live_states, 1, &execution_id, &work_item_id);

        let thresholds = RunBudgetThresholds::default();
        let outcome = run_one_pass(&db, &live_states, &flags, &thresholds);
        assert_eq!(outcome.signaled, 1);
        assert_eq!(outcome.over_budget, 1);
        assert_eq!(budget_items_count(&db, &execution_id), 1);

        // Once-only rule: subsequent passes do not file a second item.
        let outcome2 = run_one_pass(&db, &live_states, &flags, &thresholds);
        assert_eq!(outcome2.signaled, 0);
        assert_eq!(outcome2.already_signaled, 1);
        assert_eq!(budget_items_count(&db, &execution_id), 1);
    }

    #[test]
    fn round_budget_exceeded_files_once_per_execution() {
        let (_dir, db) = open_db();
        let (_flag_dir, flags) = make_flags(true);
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "round heavy chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, 60);
        set_cost(
            &db,
            &execution_id,
            Some(1_000_000),
            None,
            Some(DEFAULT_ROUND_BUDGET + 1),
        );

        let live_states = LiveWorkerStateRegistry::new();
        register_live_slot(&live_states, 1, &execution_id, &work_item_id);

        let thresholds = RunBudgetThresholds::default();
        let outcome = run_one_pass(&db, &live_states, &flags, &thresholds);
        assert_eq!(outcome.signaled, 1);
        assert_eq!(outcome.over_budget, 1);
        assert_eq!(budget_items_count(&db, &execution_id), 1);

        // Even if the item is resolved while the run stays live, never re-file.
        db.resolve_attention_kind_for_execution(&execution_id, RUN_BUDGET_OVERRUN_ATTENTION_KIND)
            .unwrap();

        let outcome2 = run_one_pass(&db, &live_states, &flags, &thresholds);
        assert_eq!(outcome2.signaled, 0);
        assert_eq!(outcome2.already_signaled, 1);
        assert_eq!(budget_items_count(&db, &execution_id), 1);
    }

    #[test]
    fn at_threshold_is_within_budget() {
        let (_dir, db) = open_db();
        let (_flag_dir, flags) = make_flags(true);
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "boundary chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, 60);
        set_cost(
            &db,
            &execution_id,
            Some(DEFAULT_TOKEN_BUDGET),
            Some(0),
            Some(DEFAULT_ROUND_BUDGET),
        );

        let live_states = LiveWorkerStateRegistry::new();
        register_live_slot(&live_states, 1, &execution_id, &work_item_id);

        let outcome = run_one_pass(&db, &live_states, &flags, &RunBudgetThresholds::default());
        assert_eq!(outcome.signaled, 0);
        assert_eq!(outcome.within_budget, 1);
        assert_eq!(budget_items_count(&db, &execution_id), 0);
    }

    #[test]
    fn flag_disabled_skips_pass_even_when_over_budget() {
        let (_dir, db) = open_db();
        let (_flag_dir, flags) = make_flags(false);
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "flag off chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, 60);
        set_cost(&db, &execution_id, Some(DEFAULT_TOKEN_BUDGET + 100), None, Some(200));

        let live_states = LiveWorkerStateRegistry::new();
        register_live_slot(&live_states, 1, &execution_id, &work_item_id);

        let outcome = run_one_pass(&db, &live_states, &flags, &RunBudgetThresholds::default());
        assert!(outcome.disabled);
        assert_eq!(outcome.signaled, 0);
        assert_eq!(budget_items_count(&db, &execution_id), 0);
    }

    #[test]
    fn terminal_execution_is_not_signaled() {
        let (_dir, db) = open_db();
        let (_flag_dir, flags) = make_flags(true);
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "terminal chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, 60);
        set_cost(&db, &execution_id, Some(DEFAULT_TOKEN_BUDGET + 1), None, Some(200));
        db.mark_execution_orphaned(&execution_id, "test").unwrap();

        let live_states = LiveWorkerStateRegistry::new();
        register_live_slot(&live_states, 1, &execution_id, &work_item_id);

        let outcome = run_one_pass(&db, &live_states, &flags, &RunBudgetThresholds::default());
        assert_eq!(outcome.signaled, 0);
        assert_eq!(budget_items_count(&db, &execution_id), 0);
    }
}
