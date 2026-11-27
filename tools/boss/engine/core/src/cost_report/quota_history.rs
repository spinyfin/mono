//! Quota intervals measured against immutable boundary counters.
use super::*;

/// Pair consecutive successful quota readings per driver and window, and
/// subtract immutable per-run counter captures at the two boundaries.
///
/// Failures stay in `samples` and are not interval endpoints; a later
/// reading of the same window pairs across a failure. Token totals are
/// filtered to the interval's driver.
pub fn build_quota_history(
    samples: &[DriverQuotaEntry],
    captures: &BTreeMap<(String, i64), Vec<CostRunRecord>>,
) -> DriverQuotaHistory {
    let deltas = token_deltas(captures);
    let mut intervals = Vec::new();
    let mut last_reading: HashMap<(String, DriverQuotaWindow), (i64, f64, Option<i64>)> = HashMap::new();
    for sample in samples {
        let DriverQuotaOutcome::Reading(reading) = &sample.outcome else {
            continue;
        };
        let key = (sample.driver.clone(), reading.window);
        if let Some((start_epoch_s, start_used_percent, resets_at)) = last_reading.get(&key).copied()
            && sample.observed_at_epoch_s > start_epoch_s
        {
            let mut fold = RunFold::default();
            let mut by_model: BTreeMap<String, RunFold> = BTreeMap::new();
            let mut gaps = BTreeSet::new();
            let baseline = captures.get(&(sample.driver.clone(), start_epoch_s));
            let endpoint = captures.get(&(sample.driver.clone(), sample.observed_at_epoch_s));
            let tokens_unknown = baseline.is_none() || endpoint.is_none();
            let records = deltas
                .get(sample.driver.as_str())
                .map(Vec::as_slice)
                .unwrap_or_default();
            let first = records.partition_point(|(at, _)| *at <= start_epoch_s);
            let last = records.partition_point(|(at, _)| *at <= sample.observed_at_epoch_s);
            for (_, record) in &records[first..last] {
                fold_record(&mut fold, record, false, &mut gaps);
                fold_record(
                    by_model.entry(label_or_none(record.model.as_deref())).or_default(),
                    record,
                    false,
                    &mut gaps,
                );
            }
            intervals.push(
                DriverQuotaHistoryInterval::builder()
                    .driver(&sample.driver)
                    .tokens_unknown(tokens_unknown)
                    .crossed_reset(resets_at.is_some_and(|reset| reset <= sample.observed_at_epoch_s))
                    .by_model(finish_buckets(by_model))
                    .end_epoch_s(sample.observed_at_epoch_s)
                    .end_used_percent(reading.used_percent)
                    .start_epoch_s(start_epoch_s)
                    .start_used_percent(start_used_percent)
                    .tokens(fold.finish())
                    .window(reading.window)
                    .build(),
            );
        }
        last_reading.insert(
            key,
            (
                sample.observed_at_epoch_s,
                reading.used_percent,
                reading.resets_at_epoch_s,
            ),
        );
    }
    DriverQuotaHistory {
        intervals,
        samples: samples.to_vec(),
    }
}

/// Index changed observations once. Interval folds then visit only changes in
/// that driver's time range, rather than rescanning every retained run.
fn token_deltas(captures: &BTreeMap<(String, i64), Vec<CostRunRecord>>) -> HashMap<&str, Vec<(i64, CostRunRecord)>> {
    let mut deltas: HashMap<&str, Vec<(i64, CostRunRecord)>> = HashMap::new();
    let mut previous_driver = "";
    let mut previous_at = 0;
    let mut previous = HashMap::new();
    for ((driver, at), records) in captures {
        if driver == previous_driver {
            for current in records {
                let baseline = previous.get(current.run_id.as_str()).copied();
                let incomplete = [
                    current.input_tokens,
                    current.output_tokens,
                    current.cache_creation_tokens,
                    current.cache_read_tokens,
                ]
                .contains(&None)
                    || current.usage_unavailable.is_some();
                if baseline != Some(current) || incomplete {
                    deltas
                        .entry(driver)
                        .or_default()
                        .push((*at, quota_counter_delta(current, baseline, previous_at)));
                }
            }
        }
        previous = records.iter().map(|record| (record.run_id.as_str(), record)).collect();
        previous_driver = driver;
        previous_at = *at;
    }
    deltas
}

fn quota_counter_delta(current: &CostRunRecord, previous: Option<&CostRunRecord>, start: i64) -> CostRunRecord {
    let mut delta = current.clone();
    let new_run = previous.is_none() && current.created_at_epoch_s >= start;
    fn subtract(end: Option<i64>, start: Option<i64>, new_run: bool) -> Option<i64> {
        let start = if new_run { Some(0) } else { start }?;
        end.and_then(|end| end.checked_sub(start)).filter(|delta| *delta >= 0)
    }
    delta.input_tokens = subtract(current.input_tokens, previous.and_then(|p| p.input_tokens), new_run);
    delta.output_tokens = subtract(current.output_tokens, previous.and_then(|p| p.output_tokens), new_run);
    delta.cache_creation_tokens = subtract(
        current.cache_creation_tokens,
        previous.and_then(|p| p.cache_creation_tokens),
        new_run,
    );
    delta.cache_creation_5m_tokens = subtract(
        current.cache_creation_5m_tokens,
        previous.and_then(|p| p.cache_creation_5m_tokens),
        new_run,
    );
    delta.cache_creation_1h_tokens = subtract(
        current.cache_creation_1h_tokens,
        previous.and_then(|p| p.cache_creation_1h_tokens),
        new_run,
    );
    delta.cache_read_tokens = subtract(
        current.cache_read_tokens,
        previous.and_then(|p| p.cache_read_tokens),
        new_run,
    );
    delta.agent_active_ms = subtract(
        current.agent_active_ms,
        previous.and_then(|p| p.agent_active_ms),
        new_run,
    );
    delta
}
