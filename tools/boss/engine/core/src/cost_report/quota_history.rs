//! Quota intervals measured against immutable, sparse boundary counters.
use super::*;
use boss_engine_storage_values::{QuotaRunCounterSnapshot, QuotaSampleRow};
use boss_protocol::DriverQuotaReading;

/// Pair consecutive successful quota readings per driver and window, and
/// subtract immutable per-run counter captures paired by `sample_id`.
///
/// Failures stay in `samples` and are not interval endpoints; a later
/// reading of the same window pairs across a failure. Additional readings for
/// the same `(driver, window)` in the same epoch second stay in `samples`
/// without overwriting the earlier reading's anchor or mutating any prior
/// interval. Token totals are filtered to the interval's driver.
pub fn build_quota_history(
    samples: &[QuotaSampleRow],
    captures: &BTreeMap<i64, Vec<QuotaRunCounterSnapshot>>,
) -> DriverQuotaHistory {
    let deltas = token_deltas(samples, captures);
    let mut intervals = Vec::new();
    let mut last_reading: HashMap<(String, DriverQuotaWindow), (usize, i64, i64, &DriverQuotaReading)> = HashMap::new();
    for (sample_idx, row) in samples.iter().enumerate() {
        let sample = &row.entry;
        let DriverQuotaOutcome::Reading(reading) = &sample.outcome else {
            continue;
        };
        let key = (sample.driver.clone(), reading.window);
        if let Some((start_idx, start_sample_id, start_epoch_s, start_reading)) = last_reading.get(&key).copied() {
            // Multiple readings in the same second for the same driver and
            // window keep the first reading as the interval anchor so prior
            // intervals stay immutable and no subsequent delta is dropped.
            if sample.observed_at_epoch_s <= start_epoch_s {
                continue;
            }
            let mut fold = RunFold::default();
            let mut by_model: BTreeMap<String, RunFold> = BTreeMap::new();
            let baseline = captures.get(&start_sample_id);
            let endpoint = captures.get(&row.sample_id);
            let tokens_unknown = baseline.is_none() || endpoint.is_none();
            let records = deltas
                .get(sample.driver.as_str())
                .map(Vec::as_slice)
                .unwrap_or_default();
            let first = records.partition_point(|(idx, _)| *idx <= start_idx);
            let last = records.partition_point(|(idx, _)| *idx <= sample_idx);
            for (_, delta) in &records[first..last] {
                fold_quota_delta(&mut fold, delta);
                fold_quota_delta(
                    by_model.entry(label_or_none(delta.model.as_deref())).or_default(),
                    delta,
                );
            }
            intervals.push(
                DriverQuotaHistoryInterval::builder()
                    .driver(&sample.driver)
                    .tokens_unknown(tokens_unknown)
                    .crossed_reset(crossed_reset(start_reading, reading, sample.observed_at_epoch_s))
                    .by_model(finish_buckets(by_model))
                    .end_epoch_s(sample.observed_at_epoch_s)
                    .end_used_percent(reading.used_percent)
                    .start_epoch_s(start_epoch_s)
                    .start_used_percent(start_reading.used_percent)
                    .tokens(fold.finish())
                    .window(reading.window)
                    .build(),
            );
        }
        last_reading.insert(key, (sample_idx, row.sample_id, sample.observed_at_epoch_s, reading));
    }
    DriverQuotaHistory {
        intervals,
        samples: samples.iter().map(|row| row.entry.clone()).collect(),
    }
}

fn crossed_reset(start: &DriverQuotaReading, end: &DriverQuotaReading, end_observed_at: i64) -> bool {
    if start.resets_at_epoch_s.is_some_and(|reset| reset <= end_observed_at) {
        return true;
    }
    if let (Some(start_reset), Some(end_reset)) = (start.resets_at_epoch_s, end.resets_at_epoch_s)
        && start_reset != end_reset
    {
        return true;
    }
    if let (Some(start_text), Some(end_text)) = (start.resets_at_text.as_deref(), end.resets_at_text.as_deref())
        && start_text != end_text
    {
        return true;
    }
    if start.resets_at_epoch_s.is_none() && end.resets_at_epoch_s.is_none() && end.used_percent < start.used_percent {
        return true;
    }
    false
}

struct QuotaCounterDelta {
    model: Option<String>,
    tokens: RunTokens,
    agent_active_ms: Option<i64>,
    usage_unavailable: bool,
}

fn fold_quota_delta(fold: &mut RunFold, delta: &QuotaCounterDelta) {
    fold.add(&RunSample {
        tokens: delta.tokens,
        agent_active_ms: delta.agent_active_ms,
        usage_unavailable: delta.usage_unavailable.then_some("usage unavailable"),
        usd: UsdOutcome::NotRequested,
    });
}

/// Index per-step deltas once per driver in `samples` order. Captures are
/// sparse: a run absent from a capture is treated as unchanged, and a finished
/// run whose counters did not change emits no delta even when some counter
/// classes are absent or usage is unavailable.
fn token_deltas<'a>(
    samples: &'a [QuotaSampleRow],
    captures: &'a BTreeMap<i64, Vec<QuotaRunCounterSnapshot>>,
) -> HashMap<&'a str, Vec<(usize, QuotaCounterDelta)>> {
    let mut deltas: HashMap<&'a str, Vec<(usize, QuotaCounterDelta)>> = HashMap::new();
    let mut driver_state: HashMap<&'a str, (i64, HashMap<&'a str, &'a QuotaRunCounterSnapshot>)> = HashMap::new();

    for (sample_idx, row) in samples.iter().enumerate() {
        let Some(records) = captures.get(&row.sample_id) else {
            continue;
        };
        let driver = row.entry.driver.as_str();
        let observed_at = row.entry.observed_at_epoch_s;
        if let Some((prev_at, last_state)) = driver_state.get_mut(driver) {
            let driver_deltas = deltas.entry(driver).or_default();
            for current in records {
                let baseline = last_state.get(current.run_id.as_str()).copied();
                let could_have_spent = match baseline {
                    None => true,
                    Some(prev) => !prev.finished || !prev.counters_equal(current),
                };
                if could_have_spent {
                    driver_deltas.push((sample_idx, quota_counter_delta(current, baseline, *prev_at)));
                }
                last_state.insert(current.run_id.as_str(), current);
            }
            *prev_at = observed_at;
        } else {
            let initial = records.iter().map(|record| (record.run_id.as_str(), record)).collect();
            driver_state.insert(driver, (observed_at, initial));
        }
    }
    deltas
}

fn quota_counter_delta(
    current: &QuotaRunCounterSnapshot,
    previous: Option<&QuotaRunCounterSnapshot>,
    start: i64,
) -> QuotaCounterDelta {
    let new_run = previous.is_none() && current.created_at_epoch_s >= start;
    fn subtract(end: Option<i64>, start: Option<i64>, new_run: bool) -> Option<i64> {
        let start = if new_run { Some(0) } else { start }?;
        end.and_then(|end| end.checked_sub(start)).filter(|delta| *delta >= 0)
    }
    QuotaCounterDelta {
        model: current.model.clone(),
        tokens: RunTokens {
            input: subtract(current.input_tokens, previous.and_then(|p| p.input_tokens), new_run),
            output: subtract(current.output_tokens, previous.and_then(|p| p.output_tokens), new_run),
            cache_creation: subtract(
                current.cache_creation_tokens,
                previous.and_then(|p| p.cache_creation_tokens),
                new_run,
            ),
            cache_read: subtract(
                current.cache_read_tokens,
                previous.and_then(|p| p.cache_read_tokens),
                new_run,
            ),
        },
        agent_active_ms: subtract(
            current.agent_active_ms,
            previous.and_then(|p| p.agent_active_ms),
            new_run,
        ),
        usage_unavailable: current.usage_unavailable,
    }
}
