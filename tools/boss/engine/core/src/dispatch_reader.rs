//! Read-side companion to [`crate::dispatch_events`].
//!
//! The dispatch pipeline emits JSONL events into
//! `<state-root>/dispatch-events/current.jsonl` and per-execution
//! mirrors at `<state-root>/executions/<id>/dispatch.jsonl`. The
//! engine itself never reads those back — the writer is fire-and-
//! forget. This module is the read path that `bossctl dispatch tail`
//! / `diagnose` / `ghost-active` go through. It is deliberately
//! file-scan-only: it does NOT touch the engine RPC, so it works
//! when the engine is wedged.
//!
//! All readers are synchronous: the JSONL files are append-only and
//! small enough to scan in one pass per call. Each `read_current` /
//! `read_execution` returns a `Vec<DispatchEvent>` in the order they
//! were appended (lines that fail to parse are skipped with a
//! diagnostic on stderr — a half-written line at the tail of the
//! file is the common failure mode and we'd rather show what we have
//! than blow up).

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome as DispatchOutcome, Stage};

/// Per-stage stalled-detection thresholds. The watchdog used to apply
/// a single global threshold to every stage, but the cube-lease
/// hang incident (`exec_18aec07893bd2e30_29`, 2026-05-12) showed that
/// a 120s default is too coarse for the early dispatch stages — the
/// engine had wedged in `worker_claimed` for 46 seconds without any
/// `stage_stalled` event firing, because the global threshold hadn't
/// elapsed yet. Per-stage overrides let us flag the early handoffs
/// (worker_claimed → cube_repo_ensured → cube_workspace_leased)
/// faster while keeping the longer pane-spawn stages on a generous
/// threshold.
#[derive(Debug, Clone)]
pub struct StageThresholds {
    default_ms: u128,
    overrides: BTreeMap<String, u128>,
}

impl StageThresholds {
    pub fn new(default: Duration) -> Self {
        Self {
            default_ms: default.as_millis(),
            overrides: BTreeMap::new(),
        }
    }

    /// Override the threshold for a specific stage. Pass the wire
    /// stage name (e.g. `"worker_claimed"`) — the watchdog matches
    /// against `DispatchEvent::stage`.
    pub fn with_override(mut self, stage: impl Into<String>, threshold: Duration) -> Self {
        self.overrides.insert(stage.into(), threshold.as_millis());
        self
    }

    pub fn for_stage(&self, stage: &str) -> u128 {
        self.overrides.get(stage).copied().unwrap_or(self.default_ms)
    }

    pub fn default_ms(&self) -> u128 {
        self.default_ms
    }
}

/// Default Boss state root used by the file-scan readers when the
/// caller didn't override it. Mirrors the writer's default (see
/// [`crate::dispatch_events::JsonlFileSink`] callers in `app.rs`).
/// Delegates to `boss-log-files` so the `~/Library/Application Support/Boss`
/// location is defined once and shared with the log-path resolvers.
pub fn default_state_root() -> Option<PathBuf> {
    boss_log_files::default_state_root()
}

/// Path to the flat dispatch-event stream under `root`.
pub fn current_path(root: &Path) -> PathBuf {
    root.join("dispatch-events").join("current.jsonl")
}

/// Path to the per-execution mirror under `root`.
pub fn execution_path(root: &Path, execution_id: &str) -> PathBuf {
    root.join("executions").join(execution_id).join("dispatch.jsonl")
}

/// Read every event currently in `current.jsonl`, in file order.
/// Missing file is treated as "no events" so callers can run against
/// a state root that hasn't been populated yet.
pub fn read_current(root: &Path) -> Result<Vec<DispatchEvent>> {
    read_jsonl(&current_path(root))
}

/// Read every event in the per-execution mirror for `execution_id`.
/// Missing file is treated as "no events".
pub fn read_execution(root: &Path, execution_id: &str) -> Result<Vec<DispatchEvent>> {
    read_jsonl(&execution_path(root, execution_id))
}

fn read_jsonl(path: &Path) -> Result<Vec<DispatchEvent>> {
    match fs::File::open(path) {
        Ok(file) => parse_lines(BufReader::new(file)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).with_context(|| format!("opening {}", path.display())),
    }
}

fn parse_lines<R: BufRead>(reader: R) -> Result<Vec<DispatchEvent>> {
    let mut out = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("reading line {} from dispatch jsonl", idx + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<DispatchEvent>(&line) {
            Ok(event) => out.push(event),
            Err(err) => {
                eprintln!(
                    "warning: dropping unparseable dispatch event on line {}: {err}",
                    idx + 1
                );
            }
        }
    }
    Ok(out)
}

/// One entry in the `ghost-active` listing: an execution whose
/// dispatch timeline started but never reached a terminal stage
/// (`pane_spawned ok|error`, `run_started error`).
///
/// Surfaced by [`ghost_active`] for inspection through `bossctl
/// dispatch ghost-active`. Detection is event-shape only: we don't
/// need DB access to spot a timeline that just stops.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GhostActiveEntry {
    pub execution_id: String,
    pub work_item_id: Option<String>,
    pub last_stage: String,
    pub last_outcome: String,
    pub last_ts_epoch_ms: u128,
    /// Milliseconds elapsed since the last event for this execution
    /// at the time `ghost_active` was called.
    pub elapsed_since_last_ms: u128,
    /// True when [`detect_stalled_stage`] flagged the timeline as
    /// stalled past `stalled_threshold_ms`.
    pub stalled: bool,
}

/// Return every per-execution timeline that hasn't reached a
/// terminal stage. `now_ms` is the wall-clock anchor used for
/// `elapsed_since_last_ms`; `stalled_threshold_ms` flips the
/// per-entry `stalled` field once the gap since the last event
/// exceeds it.
///
/// Scans `<root>/executions/<id>/dispatch.jsonl` for every
/// execution_id directory. The `current.jsonl` could be used too,
/// but the per-execution mirrors are cheaper to bucket and survive
/// rotation of the flat stream (if we ever add that).
pub fn ghost_active(root: &Path, now_ms: u128, stalled_threshold_ms: u128) -> Result<Vec<GhostActiveEntry>> {
    let executions_dir = root.join("executions");
    let read_dir = match fs::read_dir(&executions_dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("opening {}", executions_dir.display()));
        }
    };

    let mut entries = Vec::new();
    for dirent in read_dir {
        let dirent = dirent.with_context(|| format!("reading {}", executions_dir.display()))?;
        let path = dirent.path();
        if !path.is_dir() {
            continue;
        }
        let Some(execution_id) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let dispatch_path = path.join("dispatch.jsonl");
        if !dispatch_path.exists() {
            continue;
        }
        let events = read_jsonl(&dispatch_path)?;
        let Some(last) = events.last() else {
            continue;
        };
        if is_terminal_event(last) {
            continue;
        }
        let elapsed = now_ms.saturating_sub(last.ts_epoch_ms);
        entries.push(GhostActiveEntry {
            execution_id: execution_id.to_owned(),
            work_item_id: last.work_item_id.clone(),
            last_stage: last.stage.clone(),
            last_outcome: last.outcome.clone(),
            last_ts_epoch_ms: last.ts_epoch_ms,
            elapsed_since_last_ms: elapsed,
            stalled: elapsed >= stalled_threshold_ms,
        });
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.elapsed_since_last_ms));
    Ok(entries)
}

/// An event is "terminal" — i.e., the dispatch timeline for that
/// execution is officially over — when it is either a successful
/// `pane_spawned` (the slot is up and the worker is now driving),
/// or any explicit error (we won't get a follow-up; the
/// `record_start_failure` / `pane_spawn_failed` paths have run).
pub fn is_terminal_event(event: &DispatchEvent) -> bool {
    if event.outcome == "error" {
        return true;
    }
    if event.stage == "pane_spawned" && event.outcome == "ok" {
        return true;
    }
    false
}

/// Per-execution duration breakdown: time spent in each stage,
/// computed as `next_event.ts - this_event.ts`. The last stage's
/// duration is `now - last.ts` **only** when the last event is
/// non-terminal (see [`is_terminal_event`]); for terminal timelines
/// the final entry is `0` so the report doesn't grow forever after
/// dispatch has finished.
///
/// Used by `bossctl dispatch diagnose <id>` to render the timeline
/// without re-doing the math in main.
pub fn stage_durations_ms(events: &[DispatchEvent], now_ms: u128) -> Vec<u128> {
    let mut out = Vec::with_capacity(events.len());
    for i in 0..events.len() {
        let cur = events[i].ts_epoch_ms;
        let next = match events.get(i + 1) {
            Some(next_event) => next_event.ts_epoch_ms,
            None => {
                if is_terminal_event(&events[i]) {
                    cur
                } else {
                    now_ms
                }
            }
        };
        out.push(next.saturating_sub(cur));
    }
    out
}

/// Roll-up of (stage, outcome) → count over a slice of events.
/// `BTreeMap` so callers get stable ordering when they iterate.
pub fn count_by_stage_outcome(events: &[DispatchEvent]) -> BTreeMap<(String, String), usize> {
    let mut out = BTreeMap::new();
    for event in events {
        *out.entry((event.stage.clone(), event.outcome.clone())).or_insert(0) += 1;
    }
    out
}

/// One execution that made it off the ready queue onto a worker slot:
/// the time from its first `request_recorded` event (dispatch became
/// ready) to the `worker_claimed`/`ok` event (a slot was actually
/// claimed). `reason` is the `details.reason` of the last
/// `worker_claimed`/`skipped` event seen before the claim — i.e. the
/// defer reason that was finally cleared — or `"none"` when the
/// execution claimed a slot on its first attempt.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct ResolvedWait {
    pub execution_id: String,
    pub work_item_id: Option<String>,
    pub ready_ts_epoch_ms: u128,
    pub dispatched_ts_epoch_ms: u128,
    pub wait_ms: u128,
    pub reason: String,
}

/// One execution that is `ready` right now but hasn't claimed a slot
/// yet (and hasn't hit a terminal error) — a currently-blocked item
/// for `bossctl dispatch stats`'s "top blocked" listing.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct BlockedNow {
    pub execution_id: String,
    pub work_item_id: Option<String>,
    pub ready_ts_epoch_ms: u128,
    pub wait_so_far_ms: u128,
    pub reason: String,
    /// Dispatch pool this execution targets (`"main"` / `"automation"` /
    /// `"review"`), read from a `request_recorded` event's
    /// `details.pool`. `"unknown"` when no `request_recorded` event has
    /// fired yet — i.e. exactly the never-picked-up class this report
    /// used to drop entirely (see [`compute_wait_stats`]). Feeds
    /// `bossctl dispatch stats`'s per-pool queue summary.
    pub pool: String,
}

/// count / p50 / p95 / max dispatch wait, bucketed by the defer
/// reason that finally cleared (see [`ResolvedWait::reason`]).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReasonWaitStats {
    pub reason: String,
    pub count: usize,
    pub p50_ms: u128,
    pub p95_ms: u128,
    pub max_ms: u128,
}

/// Full report for `bossctl dispatch stats`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct DispatchWaitReport {
    pub by_reason: Vec<ReasonWaitStats>,
    /// Currently-blocked executions, longest-waiting first.
    pub blocked_now: Vec<BlockedNow>,
}

/// Reason string used for a [`BlockedNow`] entry that is ready but
/// hasn't been through a single defer/claim attempt yet (the drain
/// loop hasn't reached it since it became ready).
const PENDING_FIRST_ATTEMPT: &str = "pending_first_attempt";

/// Reason string used for a [`ResolvedWait`] entry that claimed a
/// slot on its very first attempt (no `worker_claimed`/`skipped`
/// event preceded the claim).
const NO_DEFERRAL: &str = "none";

/// Compute dispatch-wait statistics over `events` (typically the full
/// `current.jsonl` stream from [`read_current`]). `now_ms` anchors the
/// "wait so far" for still-blocked executions. `since_ms`, when set,
/// drops any event older than it before grouping — the CLI's
/// `--since` filter.
///
/// This is read-only over the existing dispatch-events stream: it
/// derives wait time and defer reason from events the coordinator
/// already emits (`request_recorded`, `worker_claimed` ok/skipped);
/// it does not change dispatch behavior.
pub fn compute_wait_stats(events: &[DispatchEvent], now_ms: u128, since_ms: Option<u128>) -> DispatchWaitReport {
    let mut by_execution: BTreeMap<&str, Vec<&DispatchEvent>> = BTreeMap::new();
    for event in events {
        if since_ms.is_some_and(|since| event.ts_epoch_ms < since) {
            continue;
        }
        by_execution.entry(event.execution_id.as_str()).or_default().push(event);
    }

    let mut resolved: Vec<ResolvedWait> = Vec::new();
    let mut blocked: Vec<BlockedNow> = Vec::new();

    for (execution_id, evs) in &by_execution {
        // `request_recorded` marks the execution coming off the ready
        // queue for a dispatch attempt — the closest existing event to
        // "became ready to dispatch". Previously, a timeline with none
        // was dropped entirely — but that is exactly the worst-waiter
        // class (an execution stuck BEFORE its first dispatch attempt,
        // e.g. the `status_transition` -> `request_recorded` gap; see
        // T2692, 2026-07-15's post-pause backlog). Anchor on the
        // earliest event we DO have instead of skipping — `evs` is
        // non-empty (it only exists in `by_execution` because at least
        // one event referenced this execution_id) and preserves file
        // order, so `evs[0]` is the earliest surviving event.
        let (ready_ts, work_item_id) = match evs.iter().find(|e| e.stage == "request_recorded") {
            Some(ready) => (ready.ts_epoch_ms, ready.work_item_id.clone()),
            None => (evs[0].ts_epoch_ms, evs[0].work_item_id.clone()),
        };
        let pool = evs
            .iter()
            .find(|e| e.stage == "request_recorded")
            .and_then(|e| e.details.get("pool"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_owned();

        let last_defer_reason_upto = |upto_ts: Option<u128>| -> String {
            evs.iter()
                .rev()
                .find(|e| {
                    e.stage == "worker_claimed" && e.outcome == "skipped" && upto_ts.is_none_or(|t| e.ts_epoch_ms <= t)
                })
                .and_then(|e| e.details.get("reason").and_then(|v| v.as_str()))
                .unwrap_or(NO_DEFERRAL)
                .to_owned()
        };

        match evs.iter().find(|e| e.stage == "worker_claimed" && e.outcome == "ok") {
            Some(claimed) => {
                resolved.push(ResolvedWait {
                    execution_id: (*execution_id).to_owned(),
                    work_item_id,
                    ready_ts_epoch_ms: ready_ts,
                    dispatched_ts_epoch_ms: claimed.ts_epoch_ms,
                    wait_ms: claimed.ts_epoch_ms.saturating_sub(ready_ts),
                    reason: last_defer_reason_upto(Some(claimed.ts_epoch_ms)),
                });
            }
            None if !evs.iter().any(|e| e.outcome == "error") => {
                let reason = last_defer_reason_upto(None);
                let reason = if reason == NO_DEFERRAL {
                    PENDING_FIRST_ATTEMPT.to_owned()
                } else {
                    reason
                };
                blocked.push(BlockedNow {
                    execution_id: (*execution_id).to_owned(),
                    work_item_id,
                    ready_ts_epoch_ms: ready_ts,
                    wait_so_far_ms: now_ms.saturating_sub(ready_ts),
                    reason,
                    pool,
                });
            }
            // A terminal error before ever claiming a slot means dispatch
            // gave up rather than waited — excluded from wait stats.
            None => {}
        }
    }

    let mut grouped: BTreeMap<String, Vec<u128>> = BTreeMap::new();
    for r in &resolved {
        grouped.entry(r.reason.clone()).or_default().push(r.wait_ms);
    }
    let mut by_reason: Vec<ReasonWaitStats> = grouped
        .into_iter()
        .map(|(reason, mut durations)| {
            durations.sort_unstable();
            ReasonWaitStats {
                count: durations.len(),
                p50_ms: percentile_ms(&durations, 50.0),
                p95_ms: percentile_ms(&durations, 95.0),
                max_ms: durations.last().copied().unwrap_or(0),
                reason,
            }
        })
        .collect();
    by_reason.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.reason.cmp(&b.reason)));

    blocked.sort_by_key(|b| std::cmp::Reverse(b.wait_so_far_ms));

    DispatchWaitReport {
        by_reason,
        blocked_now: blocked,
    }
}

/// Per-pool rollup of the ready queue, for `bossctl dispatch stats`'s
/// consolidated summary line: how many executions are currently blocked
/// in each pool, and how long the oldest of them has been waiting.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PoolQueueSummary {
    pub pool: String,
    pub queued: usize,
    pub oldest_wait_ms: u128,
}

/// Roll [`DispatchWaitReport::blocked_now`] up by pool, sorted by `queued`
/// descending then pool name ascending. `pool` is `"unknown"` for entries
/// whose timeline has no `request_recorded` event yet — see
/// [`compute_wait_stats`].
pub fn summarize_queue_by_pool(blocked_now: &[BlockedNow]) -> Vec<PoolQueueSummary> {
    let mut by_pool: BTreeMap<&str, (usize, u128)> = BTreeMap::new();
    for entry in blocked_now {
        let bucket = by_pool.entry(entry.pool.as_str()).or_insert((0, 0));
        bucket.0 += 1;
        bucket.1 = bucket.1.max(entry.wait_so_far_ms);
    }
    let mut out: Vec<PoolQueueSummary> = by_pool
        .into_iter()
        .map(|(pool, (queued, oldest_wait_ms))| PoolQueueSummary {
            pool: pool.to_owned(),
            queued,
            oldest_wait_ms,
        })
        .collect();
    out.sort_by(|a, b| b.queued.cmp(&a.queued).then_with(|| a.pool.cmp(&b.pool)));
    out
}

/// Count `worker_claimed`/`ok` events (successful dispatch completions)
/// within `[now_ms - window_ms, now_ms]` — the dispatch-completion rate
/// for `bossctl dispatch stats`'s "dispatch rate over last N min" line.
pub fn dispatches_in_window(events: &[DispatchEvent], now_ms: u128, window_ms: u128) -> usize {
    let cutoff = now_ms.saturating_sub(window_ms);
    events
        .iter()
        .filter(|e| {
            e.stage == "worker_claimed" && e.outcome == "ok" && e.ts_epoch_ms >= cutoff && e.ts_epoch_ms <= now_ms
        })
        .count()
}

/// Nearest-rank percentile over an already-ascending-sorted slice.
/// Empty input yields `0`.
fn percentile_ms(sorted_ascending: &[u128], pct: f64) -> u128 {
    if sorted_ascending.is_empty() {
        return 0;
    }
    let rank = ((pct / 100.0) * sorted_ascending.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted_ascending.len() - 1);
    sorted_ascending[idx]
}

/// One stage stall the detector wants to surface as a
/// `stage_stalled` event. Carries enough context for the writer to
/// emit a fully-populated `DispatchEvent` without re-reading the
/// timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalledStage {
    pub execution_id: String,
    pub work_item_id: Option<String>,
    /// The dispatch stage that hasn't progressed (e.g.
    /// `cube_change_created`). This is the last non-`stage_stalled`
    /// stage in the timeline — a previously-emitted stage_stalled
    /// event doesn't itself count as the stage that's stuck.
    pub stalled_stage: String,
    pub stalled_outcome: String,
    pub last_ts_epoch_ms: u128,
    pub elapsed_in_stage_ms: u128,
}

/// Walk every per-execution mirror under `root` and return the
/// stalls that haven't yet been surfaced. An execution is stalled
/// when its last "real" stage event (any non-`stage_stalled` event)
/// is non-terminal AND older than the per-stage threshold from
/// `thresholds`. To avoid duplicate `stage_stalled` lines for the
/// same wedge, we skip executions whose timeline already contains a
/// `stage_stalled` line referencing the current stalled stage.
pub fn pending_stalls(root: &Path, now_ms: u128, thresholds: &StageThresholds) -> Result<Vec<StalledStage>> {
    let executions_dir = root.join("executions");
    let read_dir = match fs::read_dir(&executions_dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("opening {}", executions_dir.display()));
        }
    };

    let mut out = Vec::new();
    for dirent in read_dir {
        let dirent = dirent.with_context(|| format!("reading {}", executions_dir.display()))?;
        let path = dirent.path();
        if !path.is_dir() {
            continue;
        }
        let Some(execution_id) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let dispatch_path = path.join("dispatch.jsonl");
        if !dispatch_path.exists() {
            continue;
        }
        let events = read_jsonl(&dispatch_path)?;
        let Some(stall) = stall_to_emit_for(execution_id, &events, now_ms, thresholds) else {
            continue;
        };
        out.push(stall);
    }
    Ok(out)
}

/// Walk every per-execution mirror under `root` and return timelines
/// whose current (non-`stage_stalled`) stage has been stuck for at least
/// `threshold_ms` — independent of the smaller per-stage
/// [`StageThresholds`] used to decide when to emit the `stage_stalled`
/// event itself (see [`pending_stalls`]).
///
/// Unlike `pending_stalls`, this does NOT dedupe against a prior
/// `stage_stalled` line: `stage_stalled` is write-only telemetry with no
/// alert or attention item behind it (`dispatch_events.rs`'s own doc
/// calls this out — "does NOT auto-remediate"), so a stall that has sat
/// past a much larger, operator-facing threshold needs to keep
/// surfacing on every pass. The caller (the attention-escalation sweep)
/// owns its own idempotency via a DB-side open-attention-item upsert, so
/// re-including an already-surfaced stall here is what lets that sweep
/// refresh the item's elapsed-time text on each tick instead of freezing
/// it at the first trip.
pub fn persistently_stalled(root: &Path, now_ms: u128, threshold_ms: u128) -> Result<Vec<StalledStage>> {
    let executions_dir = root.join("executions");
    let read_dir = match fs::read_dir(&executions_dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("opening {}", executions_dir.display()));
        }
    };

    let mut out = Vec::new();
    for dirent in read_dir {
        let dirent = dirent.with_context(|| format!("reading {}", executions_dir.display()))?;
        let path = dirent.path();
        if !path.is_dir() {
            continue;
        }
        let Some(execution_id) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let dispatch_path = path.join("dispatch.jsonl");
        if !dispatch_path.exists() {
            continue;
        }
        let events = read_jsonl(&dispatch_path)?;
        let Some(last_real) = events.iter().rev().find(|e| e.stage != "stage_stalled") else {
            continue;
        };
        if is_terminal_event(last_real) {
            continue;
        }
        let elapsed = now_ms.saturating_sub(last_real.ts_epoch_ms);
        if elapsed < threshold_ms {
            continue;
        }
        out.push(StalledStage {
            execution_id: execution_id.to_owned(),
            work_item_id: last_real.work_item_id.clone(),
            stalled_stage: last_real.stage.clone(),
            stalled_outcome: last_real.outcome.clone(),
            last_ts_epoch_ms: last_real.ts_epoch_ms,
            elapsed_in_stage_ms: elapsed,
        });
    }
    Ok(out)
}

/// Convert a [`StalledStage`] into a fully-populated
/// `stage_stalled` dispatch event for the writer to emit. Kept here
/// (next to the detector) so the wire shape stays in one place.
pub fn build_stalled_event(stall: &StalledStage) -> DispatchEvent {
    let mut event = DispatchEvent::new(Stage::StageStalled, DispatchOutcome::Ok, &stall.execution_id);
    if let Some(work_item_id) = &stall.work_item_id {
        event = event.with_work_item(work_item_id.clone());
    }
    event.with_details(serde_json::json!({
        "stalled_stage": stall.stalled_stage,
        "stalled_outcome": stall.stalled_outcome,
        "stalled_at_ts_epoch_ms": stall.last_ts_epoch_ms as u64,
        "elapsed_in_stage_ms": stall.elapsed_in_stage_ms as u64,
    }))
}

/// Run one pass of [`pending_stalls`] and emit a `stage_stalled`
/// event per stall via `sink`. Designed to be called on a cadence by
/// [`spawn_stage_stalled_detector`].
pub async fn run_stage_stalled_pass(
    root: &Path,
    thresholds: &StageThresholds,
    sink: &dyn DispatchEventSink,
) -> Result<usize> {
    let now_ms = boss_engine_utils::epoch_time::now_epoch_ms();
    let stalls = pending_stalls(root, now_ms, thresholds)?;
    let count = stalls.len();
    for stall in stalls {
        sink.emit(build_stalled_event(&stall)).await;
    }
    Ok(count)
}

/// Spawn a tokio task that runs [`run_stage_stalled_pass`] every
/// `interval`. The task has no shutdown path — engine process exit
/// drops the handle (same pattern as the merge poller).
pub fn spawn_stage_stalled_detector(
    root: PathBuf,
    sink: Arc<dyn DispatchEventSink>,
    thresholds: StageThresholds,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Stagger startup so an engine bring-up isn't paying for this
        // sweep while the rest of init is still running.
        tokio::time::sleep(interval).await;
        loop {
            match run_stage_stalled_pass(&root, &thresholds, sink.as_ref()).await {
                Ok(emitted) if emitted > 0 => {
                    tracing::info!(
                        emitted,
                        default_threshold_ms = thresholds.default_ms() as u64,
                        "stage_stalled detector: emitted events",
                    );
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(?err, "stage_stalled detector: sweep failed");
                }
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// Reusable per-timeline core for [`pending_stalls`]. Returns the
/// `StalledStage` the caller should emit for `events`, or `None`
/// when the timeline is fresh, terminal, or already has a
/// `stage_stalled` line covering the current stalled stage.
fn stall_to_emit_for(
    execution_id: &str,
    events: &[DispatchEvent],
    now_ms: u128,
    thresholds: &StageThresholds,
) -> Option<StalledStage> {
    let last_real = events.iter().rev().find(|e| e.stage != "stage_stalled")?;
    if is_terminal_event(last_real) {
        return None;
    }
    let elapsed = now_ms.saturating_sub(last_real.ts_epoch_ms);
    let threshold_ms = thresholds.for_stage(&last_real.stage);
    if elapsed < threshold_ms {
        return None;
    }
    let already_flagged = events.iter().any(|e| {
        e.stage == "stage_stalled"
            && e.details
                .get("stalled_stage")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s == last_real.stage)
            && e.details
                .get("stalled_at_ts_epoch_ms")
                .and_then(|v| v.as_u64())
                .is_some_and(|t| t as u128 == last_real.ts_epoch_ms)
    });
    if already_flagged {
        return None;
    }
    Some(StalledStage {
        execution_id: execution_id.to_owned(),
        work_item_id: last_real.work_item_id.clone(),
        stalled_stage: last_real.stage.clone(),
        stalled_outcome: last_real.outcome.clone(),
        last_ts_epoch_ms: last_real.ts_epoch_ms,
        elapsed_in_stage_ms: elapsed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch_events::{DispatchEvent, JsonlFileSink, Outcome, Stage};
    use tempfile::TempDir;

    async fn write(sink: &JsonlFileSink, ev: DispatchEvent) {
        use crate::dispatch_events::DispatchEventSink;
        sink.emit(ev).await;
    }

    #[tokio::test]
    async fn read_current_returns_events_in_file_order() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());
        write(&sink, DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-1")).await;
        write(&sink, DispatchEvent::new(Stage::WorkerClaimed, Outcome::Ok, "exec-1")).await;
        write(&sink, DispatchEvent::new(Stage::PaneSpawned, Outcome::Ok, "exec-1")).await;

        let events = read_current(dir.path()).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].stage, "request_recorded");
        assert_eq!(events[2].stage, "pane_spawned");
    }

    #[tokio::test]
    async fn read_execution_filters_to_one_mirror() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());
        write(&sink, DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-a")).await;
        write(&sink, DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-b")).await;
        write(&sink, DispatchEvent::new(Stage::WorkerClaimed, Outcome::Ok, "exec-a")).await;

        let a = read_execution(dir.path(), "exec-a").unwrap();
        let b = read_execution(dir.path(), "exec-b").unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn read_current_on_missing_root_yields_empty() {
        let dir = TempDir::new().unwrap();
        let events = read_current(dir.path()).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn parse_lines_skips_blank_and_unparseable() {
        let input = b"\n\
            {\"ts_epoch_ms\":1,\"stage\":\"request_recorded\",\"outcome\":\"ok\",\"execution_id\":\"e\",\"details\":null}\n\
            not-a-json-line\n\
            {\"ts_epoch_ms\":2,\"stage\":\"worker_claimed\",\"outcome\":\"ok\",\"execution_id\":\"e\",\"details\":null}\n";
        let events = parse_lines(std::io::BufReader::new(&input[..])).unwrap();
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn ghost_active_lists_executions_with_non_terminal_last_event() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());

        // exec-stuck: stops at cube_change_created (non-terminal)
        write(
            &sink,
            DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-stuck"),
        )
        .await;
        let mut event = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-stuck");
        event.ts_epoch_ms = 1000;
        write(&sink, event).await;

        // exec-ok: reaches pane_spawned ok → not ghost-active
        write(
            &sink,
            DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-ok"),
        )
        .await;
        let mut event = DispatchEvent::new(Stage::PaneSpawned, Outcome::Ok, "exec-ok");
        event.ts_epoch_ms = 2000;
        write(&sink, event).await;

        // exec-failed: reaches run_started error → not ghost-active
        write(
            &sink,
            DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-failed"),
        )
        .await;
        let mut event = DispatchEvent::new(Stage::RunStarted, Outcome::Error, "exec-failed");
        event.ts_epoch_ms = 3000;
        write(&sink, event).await;

        let entries = ghost_active(dir.path(), 10_000, 5_000).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].execution_id, "exec-stuck");
        assert_eq!(entries[0].last_stage, "cube_change_created");
        assert_eq!(entries[0].elapsed_since_last_ms, 9_000);
        assert!(entries[0].stalled);
    }

    #[tokio::test]
    async fn ghost_active_stalled_flag_respects_threshold() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());

        // event at t=9000, now=10000 → elapsed=1000 → not stalled
        write(
            &sink,
            DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-fresh"),
        )
        .await;
        let mut event = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-fresh");
        event.ts_epoch_ms = 9_000;
        write(&sink, event).await;

        let entries = ghost_active(dir.path(), 10_000, 5_000).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].elapsed_since_last_ms, 1_000);
        assert!(!entries[0].stalled);
    }

    #[tokio::test]
    async fn stage_durations_ms_uses_now_for_last_non_terminal_event() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());
        let mut a = DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "e");
        a.ts_epoch_ms = 100;
        write(&sink, a).await;
        let mut b = DispatchEvent::new(Stage::WorkerClaimed, Outcome::Ok, "e");
        b.ts_epoch_ms = 250;
        write(&sink, b).await;
        let mut c = DispatchEvent::new(Stage::CubeRepoEnsured, Outcome::Ok, "e");
        c.ts_epoch_ms = 700;
        write(&sink, c).await;

        let events = read_execution(dir.path(), "e").unwrap();
        let durations = stage_durations_ms(&events, 1_500);
        assert_eq!(durations, vec![150, 450, 800]);
    }

    #[tokio::test]
    async fn stage_durations_ms_uses_zero_for_terminal_last_event() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());
        let mut a = DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "e");
        a.ts_epoch_ms = 100;
        write(&sink, a).await;
        let mut b = DispatchEvent::new(Stage::PaneSpawned, Outcome::Ok, "e");
        b.ts_epoch_ms = 250;
        write(&sink, b).await;

        let events = read_execution(dir.path(), "e").unwrap();
        let durations = stage_durations_ms(&events, 9_999_999);
        // Terminal event => duration is 0, not 9_999_999 - 250.
        assert_eq!(durations, vec![150, 0]);
    }

    fn flat_thresholds(ms: u64) -> StageThresholds {
        StageThresholds::new(Duration::from_millis(ms))
    }

    #[tokio::test]
    async fn pending_stalls_emits_when_threshold_passed_and_no_prior_flag() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());
        // Fresh request_recorded then a CubeChangeCreated that hasn't
        // moved on — past the 5s threshold at now=10s.
        let mut a = DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-stuck");
        a.ts_epoch_ms = 0;
        write(&sink, a).await;
        let mut b = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-stuck");
        b.ts_epoch_ms = 1_000;
        write(&sink, b).await;

        let stalls = pending_stalls(dir.path(), 10_000, &flat_thresholds(5_000)).unwrap();
        assert_eq!(stalls.len(), 1);
        assert_eq!(stalls[0].execution_id, "exec-stuck");
        assert_eq!(stalls[0].stalled_stage, "cube_change_created");
        assert_eq!(stalls[0].elapsed_in_stage_ms, 9_000);
    }

    #[tokio::test]
    async fn pending_stalls_skips_terminal_timelines() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());
        // pane_spawned: ok is terminal.
        let mut a = DispatchEvent::new(Stage::PaneSpawned, Outcome::Ok, "exec-done");
        a.ts_epoch_ms = 1_000;
        write(&sink, a).await;

        let stalls = pending_stalls(dir.path(), 9_999_999, &flat_thresholds(5_000)).unwrap();
        assert!(stalls.is_empty());
    }

    #[tokio::test]
    async fn pending_stalls_skips_executions_already_flagged_for_the_same_stage() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());

        let mut a = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-flagged");
        a.ts_epoch_ms = 1_000;
        write(&sink, a).await;
        let mut flag = DispatchEvent::new(Stage::StageStalled, Outcome::Ok, "exec-flagged");
        flag.ts_epoch_ms = 6_500;
        flag.details = serde_json::json!({
            "stalled_stage": "cube_change_created",
            "stalled_outcome": "ok",
            "stalled_at_ts_epoch_ms": 1_000,
        });
        write(&sink, flag).await;

        let stalls = pending_stalls(dir.path(), 10_000, &flat_thresholds(5_000)).unwrap();
        assert!(stalls.is_empty(), "got {stalls:?}");
    }

    #[tokio::test]
    async fn pending_stalls_re_emits_when_stage_advances() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());

        // Stalled at cube_workspace_leased, already flagged.
        let mut a = DispatchEvent::new(Stage::CubeWorkspaceLeased, Outcome::Ok, "exec-x");
        a.ts_epoch_ms = 1_000;
        write(&sink, a).await;
        let mut flag = DispatchEvent::new(Stage::StageStalled, Outcome::Ok, "exec-x");
        flag.ts_epoch_ms = 6_500;
        flag.details = serde_json::json!({
            "stalled_stage": "cube_workspace_leased",
            "stalled_outcome": "ok",
            "stalled_at_ts_epoch_ms": 1_000,
        });
        write(&sink, flag).await;

        // Now stage advances to cube_change_created, but stalls again.
        let mut b = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-x");
        b.ts_epoch_ms = 7_000;
        write(&sink, b).await;

        // At now=15s the new stage has been stuck for 8s.
        let stalls = pending_stalls(dir.path(), 15_000, &flat_thresholds(5_000)).unwrap();
        assert_eq!(stalls.len(), 1);
        assert_eq!(stalls[0].stalled_stage, "cube_change_created");
    }

    #[tokio::test]
    async fn pending_stalls_honours_per_stage_threshold_overrides() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());

        // worker_claimed at t=0, now=35_000 → 35s in stage. Default
        // threshold is 120s (would NOT fire), but worker_claimed has
        // a 30s override → should fire.
        let mut a = DispatchEvent::new(Stage::WorkerClaimed, Outcome::Ok, "exec-claimed");
        a.ts_epoch_ms = 0;
        write(&sink, a).await;

        // pane_spawned-style longer stage: cube_change_created at
        // t=0 (also 35s elapsed) but no override → falls under the
        // 120s default and should NOT fire.
        let mut b = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-changing");
        b.ts_epoch_ms = 0;
        write(&sink, b).await;

        let thresholds =
            StageThresholds::new(Duration::from_secs(120)).with_override("worker_claimed", Duration::from_secs(30));
        let stalls = pending_stalls(dir.path(), 35_000, &thresholds).unwrap();
        assert_eq!(stalls.len(), 1);
        assert_eq!(stalls[0].execution_id, "exec-claimed");
        assert_eq!(stalls[0].stalled_stage, "worker_claimed");
    }

    #[test]
    fn stage_thresholds_falls_back_to_default_for_unknown_stages() {
        let t = StageThresholds::new(Duration::from_secs(120)).with_override("worker_claimed", Duration::from_secs(30));
        assert_eq!(t.for_stage("worker_claimed"), 30_000);
        assert_eq!(t.for_stage("cube_repo_ensured"), 120_000);
        assert_eq!(t.for_stage("anything_else"), 120_000);
    }

    #[test]
    fn is_terminal_event_recognises_terminal_shapes() {
        let req = DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "e");
        assert!(!is_terminal_event(&req));
        let pane_ok = DispatchEvent::new(Stage::PaneSpawned, Outcome::Ok, "e");
        assert!(is_terminal_event(&pane_ok));
        let run_err = DispatchEvent::new(Stage::RunStarted, Outcome::Error, "e");
        assert!(is_terminal_event(&run_err));
        let pane_err = DispatchEvent::new(Stage::PaneSpawned, Outcome::Error, "e");
        assert!(is_terminal_event(&pane_err));
    }

    fn ev(stage: Stage, outcome: Outcome, execution_id: &str, ts: u128, details: serde_json::Value) -> DispatchEvent {
        let mut event = DispatchEvent::new(stage, outcome, execution_id);
        event.ts_epoch_ms = ts;
        event.details = details;
        event
    }

    #[test]
    fn compute_wait_stats_buckets_resolved_wait_by_last_defer_reason() {
        let events = vec![
            ev(
                Stage::RequestRecorded,
                Outcome::Ok,
                "exec-a",
                0,
                serde_json::Value::Null,
            ),
            ev(
                Stage::WorkerClaimed,
                Outcome::Skipped,
                "exec-a",
                100,
                serde_json::json!({"reason": "chain_serialized"}),
            ),
            ev(
                Stage::WorkerClaimed,
                Outcome::Ok,
                "exec-a",
                500,
                serde_json::Value::Null,
            ),
            // exec-b dispatches on its first attempt — no defer.
            ev(
                Stage::RequestRecorded,
                Outcome::Ok,
                "exec-b",
                0,
                serde_json::Value::Null,
            ),
            ev(Stage::WorkerClaimed, Outcome::Ok, "exec-b", 50, serde_json::Value::Null),
        ];
        let report = compute_wait_stats(&events, 1_000, None);
        assert_eq!(report.by_reason.len(), 2);
        let chain = report
            .by_reason
            .iter()
            .find(|r| r.reason == "chain_serialized")
            .unwrap();
        assert_eq!(chain.count, 1);
        assert_eq!(chain.p50_ms, 500);
        assert_eq!(chain.max_ms, 500);
        let none = report.by_reason.iter().find(|r| r.reason == "none").unwrap();
        assert_eq!(none.count, 1);
        assert_eq!(none.max_ms, 50);
        assert!(report.blocked_now.is_empty());
    }

    #[test]
    fn compute_wait_stats_lists_currently_blocked_longest_first() {
        let events = vec![
            ev(
                Stage::RequestRecorded,
                Outcome::Ok,
                "exec-slow",
                0,
                serde_json::Value::Null,
            ),
            ev(
                Stage::WorkerClaimed,
                Outcome::Skipped,
                "exec-slow",
                100,
                serde_json::json!({"reason": "pool_exhausted"}),
            ),
            ev(
                Stage::RequestRecorded,
                Outcome::Ok,
                "exec-fresh",
                900,
                serde_json::Value::Null,
            ),
        ];
        let report = compute_wait_stats(&events, 1_000, None);
        assert!(report.by_reason.is_empty());
        assert_eq!(report.blocked_now.len(), 2);
        assert_eq!(report.blocked_now[0].execution_id, "exec-slow");
        assert_eq!(report.blocked_now[0].reason, "pool_exhausted");
        assert_eq!(report.blocked_now[0].wait_so_far_ms, 1_000);
        assert_eq!(report.blocked_now[1].execution_id, "exec-fresh");
        assert_eq!(report.blocked_now[1].reason, "pending_first_attempt");
        assert_eq!(report.blocked_now[1].wait_so_far_ms, 100);
    }

    /// The pre-request_recorded blind spot (T2692, 2026-07-15): an
    /// execution that never received a `request_recorded` event — e.g. it
    /// is stuck between `status_transition` and its first dispatch
    /// attempt — must still show up as blocked, with its wait measured
    /// from the earliest event we do have, not dropped from the report
    /// entirely.
    #[test]
    fn compute_wait_stats_includes_never_picked_up_executions() {
        let events = vec![ev(
            Stage::StatusTransition,
            Outcome::Ok,
            "exec-stuck-pre-pickup",
            0,
            serde_json::Value::Null,
        )];
        let report = compute_wait_stats(&events, 840_000, None);
        assert_eq!(report.blocked_now.len(), 1);
        let entry = &report.blocked_now[0];
        assert_eq!(entry.execution_id, "exec-stuck-pre-pickup");
        assert_eq!(entry.ready_ts_epoch_ms, 0);
        assert_eq!(entry.wait_so_far_ms, 840_000);
        assert_eq!(entry.reason, "pending_first_attempt");
        assert_eq!(entry.pool, "unknown", "no request_recorded event means pool is unknown");
    }

    /// A never-picked-up execution that DOES eventually claim a slot (some
    /// paths, e.g. force-dispatch, can skip `request_recorded` entirely)
    /// must land in `by_reason`, anchored on its earliest event, rather
    /// than being dropped.
    #[test]
    fn compute_wait_stats_resolves_execution_with_no_request_recorded() {
        let events = vec![
            ev(
                Stage::StatusTransition,
                Outcome::Ok,
                "exec-force",
                0,
                serde_json::Value::Null,
            ),
            ev(
                Stage::WorkerClaimed,
                Outcome::Ok,
                "exec-force",
                300,
                serde_json::Value::Null,
            ),
        ];
        let report = compute_wait_stats(&events, 1_000, None);
        assert!(report.blocked_now.is_empty());
        assert_eq!(report.by_reason.len(), 1);
        assert_eq!(report.by_reason[0].max_ms, 300);
    }

    /// `pool` is read from `request_recorded`'s `details.pool` when that
    /// event exists, powering the per-pool queue summary.
    #[test]
    fn compute_wait_stats_reads_pool_from_request_recorded_details() {
        let events = vec![ev(
            Stage::RequestRecorded,
            Outcome::Ok,
            "exec-auto",
            0,
            serde_json::json!({"pool": "automation"}),
        )];
        let report = compute_wait_stats(&events, 1_000, None);
        assert_eq!(report.blocked_now[0].pool, "automation");
    }

    #[test]
    fn compute_wait_stats_excludes_executions_that_errored_before_claiming() {
        let events = vec![
            ev(
                Stage::RequestRecorded,
                Outcome::Ok,
                "exec-failed",
                0,
                serde_json::Value::Null,
            ),
            ev(
                Stage::RunStarted,
                Outcome::Error,
                "exec-failed",
                200,
                serde_json::Value::Null,
            ),
        ];
        let report = compute_wait_stats(&events, 1_000, None);
        assert!(report.by_reason.is_empty());
        assert!(report.blocked_now.is_empty());
    }

    #[test]
    fn compute_wait_stats_since_filter_drops_older_events() {
        let events = vec![
            ev(
                Stage::RequestRecorded,
                Outcome::Ok,
                "exec-old",
                0,
                serde_json::Value::Null,
            ),
            ev(
                Stage::WorkerClaimed,
                Outcome::Ok,
                "exec-old",
                100,
                serde_json::Value::Null,
            ),
            ev(
                Stage::RequestRecorded,
                Outcome::Ok,
                "exec-new",
                500,
                serde_json::Value::Null,
            ),
            ev(
                Stage::WorkerClaimed,
                Outcome::Ok,
                "exec-new",
                600,
                serde_json::Value::Null,
            ),
        ];
        let report = compute_wait_stats(&events, 1_000, Some(400));
        assert_eq!(report.by_reason.len(), 1);
        assert_eq!(report.by_reason[0].count, 1);
        assert_eq!(report.by_reason[0].max_ms, 100);
    }

    #[test]
    fn percentile_ms_nearest_rank_over_sorted_slice() {
        let sorted = vec![10, 20, 30, 40, 50];
        assert_eq!(percentile_ms(&sorted, 50.0), 30);
        assert_eq!(percentile_ms(&sorted, 95.0), 50);
        assert_eq!(percentile_ms(&[], 50.0), 0);
    }

    fn blocked(execution_id: &str, wait_so_far_ms: u128, pool: &str) -> BlockedNow {
        BlockedNow {
            execution_id: execution_id.to_owned(),
            work_item_id: None,
            ready_ts_epoch_ms: 0,
            wait_so_far_ms,
            reason: "pool_exhausted".to_owned(),
            pool: pool.to_owned(),
        }
    }

    #[test]
    fn summarize_queue_by_pool_groups_and_finds_oldest_per_pool() {
        let entries = vec![
            blocked("exec-a", 1_000, "main"),
            blocked("exec-b", 5_000, "main"),
            blocked("exec-c", 2_000, "automation"),
        ];
        let summary = summarize_queue_by_pool(&entries);
        assert_eq!(summary.len(), 2);
        assert_eq!(summary[0].pool, "main");
        assert_eq!(summary[0].queued, 2);
        assert_eq!(summary[0].oldest_wait_ms, 5_000);
        assert_eq!(summary[1].pool, "automation");
        assert_eq!(summary[1].queued, 1);
        assert_eq!(summary[1].oldest_wait_ms, 2_000);
    }

    #[test]
    fn summarize_queue_by_pool_sorts_by_queued_desc_then_pool_name() {
        let entries = vec![
            blocked("exec-a", 1_000, "review"),
            blocked("exec-b", 1_000, "automation"),
        ];
        let summary = summarize_queue_by_pool(&entries);
        // Tied queue depth (1 each) -> alphabetical.
        assert_eq!(summary[0].pool, "automation");
        assert_eq!(summary[1].pool, "review");
    }

    #[test]
    fn summarize_queue_by_pool_empty_input_yields_empty_output() {
        assert!(summarize_queue_by_pool(&[]).is_empty());
    }

    #[test]
    fn dispatches_in_window_counts_only_worker_claimed_ok_within_window() {
        let events = vec![
            ev(Stage::WorkerClaimed, Outcome::Ok, "e1", 9_000, serde_json::Value::Null),
            ev(Stage::WorkerClaimed, Outcome::Ok, "e2", 9_500, serde_json::Value::Null),
            // Outside the window.
            ev(Stage::WorkerClaimed, Outcome::Ok, "e3", 1_000, serde_json::Value::Null),
            // Skipped, not a completed dispatch.
            ev(
                Stage::WorkerClaimed,
                Outcome::Skipped,
                "e4",
                9_600,
                serde_json::Value::Null,
            ),
        ];
        assert_eq!(dispatches_in_window(&events, 10_000, 5_000), 2);
    }

    #[test]
    fn dispatches_in_window_empty_events_yields_zero() {
        assert_eq!(dispatches_in_window(&[], 10_000, 5_000), 0);
    }

    #[tokio::test]
    async fn persistently_stalled_includes_entries_past_flat_threshold_ignoring_prior_flag() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());

        let mut a = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-x");
        a.ts_epoch_ms = 0;
        write(&sink, a).await;
        // Already flagged by the (smaller-threshold) stage_stalled
        // detector — persistently_stalled must still return this entry,
        // unlike `pending_stalls`.
        let mut flag = DispatchEvent::new(Stage::StageStalled, Outcome::Ok, "exec-x");
        flag.ts_epoch_ms = 6_500;
        flag.details = serde_json::json!({
            "stalled_stage": "cube_change_created",
            "stalled_outcome": "ok",
            "stalled_at_ts_epoch_ms": 0,
        });
        write(&sink, flag).await;

        let stalls = persistently_stalled(dir.path(), 400_000, 300_000).unwrap();
        assert_eq!(stalls.len(), 1);
        assert_eq!(stalls[0].execution_id, "exec-x");
        assert_eq!(stalls[0].stalled_stage, "cube_change_created");
        assert_eq!(stalls[0].elapsed_in_stage_ms, 400_000);
    }

    #[tokio::test]
    async fn persistently_stalled_excludes_entries_under_threshold() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());
        let mut a = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-fresh");
        a.ts_epoch_ms = 0;
        write(&sink, a).await;

        let stalls = persistently_stalled(dir.path(), 100_000, 300_000).unwrap();
        assert!(stalls.is_empty());
    }

    #[tokio::test]
    async fn persistently_stalled_excludes_terminal_timelines() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());
        let mut a = DispatchEvent::new(Stage::PaneSpawned, Outcome::Ok, "exec-done");
        a.ts_epoch_ms = 0;
        write(&sink, a).await;

        let stalls = persistently_stalled(dir.path(), 9_999_999, 300_000).unwrap();
        assert!(stalls.is_empty());
    }

    #[test]
    fn persistently_stalled_on_missing_root_yields_empty() {
        let dir = TempDir::new().unwrap();
        let stalls = persistently_stalled(dir.path(), 1_000_000, 300_000).unwrap();
        assert!(stalls.is_empty());
    }
}
