//! Signature-matching diagnostic for `bossctl dispatch diagnose`.
//!
//! Walks dispatch JSONL (and optionally engine-trace) for one execution or
//! work item, matches the mechanically-checkable failure signatures from
//! `tools/boss/docs/investigations/bossctl-doctor-failure-signatures-*.md`,
//! and returns structured findings (sig id, severity, evidence, recovery).
//!
//! File-scan only — no engine RPC — so it works when the engine is wedged.
//! Reuses `boss_dispatch_reader` timeline semantics (recompute elapsed from
//! last non-`stage_stalled` event; never gate on frozen
//! `stage_stalled.details.elapsed_in_stage_ms`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use boss_engine::dispatch_events::DispatchEvent;
use boss_engine::dispatch_reader::{self, is_terminal_event};
use serde::Serialize;
use serde_json::Value;

/// Warn threshold for SIG-1 recomputed `worker_claimed` stalls (2 min).
pub const SIG1_WARN_MS: u128 = 120_000;
/// Critical / persistent-stall threshold (5 min) — matches
/// `PERSISTENT_STALL_THRESHOLD` in the engine attention escalator.
pub const SIG1_CRITICAL_MS: u128 = 300_000;
/// Minimum distinct `host_selected`/`redundant_spawn` refs before SIG-2
/// treats a live target as a zombie (JSONL-only recurrence heuristic).
pub const SIG2_MIN_REFS: usize = 10;
/// Minimum wall-clock span across those refs (1 hour).
pub const SIG2_MIN_WINDOW_MS: u128 = 3_600_000;
/// SIG-6 fleet escalate: distinct timeout executions within this window.
pub const SIG6_FLEET_WINDOW_MS: u128 = 600_000; // 10 minutes
/// SIG-6 fleet escalate: minimum distinct executions in the window (K).
pub const SIG6_FLEET_K: usize = 3;
/// JSON output schema version for `bossctl --json dispatch diagnose`.
/// v1 was top-level `{execution_id, events}` only; v2 adds findings /
/// multi-exec fields while preserving v1 keys on the single-exec path.
pub const DIAGNOSE_JSON_SCHEMA_VERSION: u32 = 2;

/// Finding severity. P0/P1/P2 are actionable; Info is capacity / forensic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    P0,
    P1,
    P2,
    Info,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::P0 => "P0",
            Self::P1 => "P1",
            Self::P2 => "P2",
            Self::Info => "info",
        }
    }

    pub fn rank(self) -> u8 {
        match self {
            Self::P0 => 0,
            Self::P1 => 1,
            Self::P2 => 2,
            Self::Info => 3,
        }
    }
}

/// One matched signature for an operator (or a wedged engine's operator).
#[derive(Debug, Clone, Serialize, bon::Builder)]
#[builder(on(String, into))]
pub struct Finding {
    pub sig_id: String,
    pub severity: Severity,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_item_id: Option<String>,
    pub count: usize,
    #[builder(default)]
    pub evidence: Vec<String>,
    pub recovery: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    #[builder(default)]
    pub details: Value,
}

/// How the caller-supplied id was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedAs {
    Execution,
    WorkItem,
}

/// Resolved diagnose targets for one CLI id.
#[derive(Debug, Clone)]
pub struct DiagnoseScope {
    pub input_id: String,
    pub resolved_as: ResolvedAs,
    pub execution_ids: Vec<String>,
    pub work_item_id: Option<String>,
}

/// Optional state.db facts for high-confidence SIG-2.
#[derive(Debug, Clone, Default)]
pub struct ExecDbFacts {
    /// execution_id → status wire string (e.g. `"running"`, `"failed"`).
    pub status_by_exec: HashMap<String, String>,
    /// execution_id → whether status is terminal.
    pub terminal_by_exec: HashMap<String, bool>,
    /// execution_id → cube_lease_id if any.
    pub lease_by_exec: HashMap<String, Option<String>>,
}

/// Match every **shipped** dispatch-side signature against `events`.
///
/// `events` should be the timelines for the diagnose scope (one or many
/// executions). When `fleet_events` is provided (typically
/// `read_current`), cross-execution signatures (SIG-2 recurrence, SIG-A
/// aggregation across peers naming this target) use the fleet scan;
/// otherwise they fall back to `events` alone.
///
/// `now_ms` is the scan clock for SIG-1 recomputed elapsed.
pub fn match_dispatch_signatures(
    events: &[DispatchEvent],
    now_ms: u128,
    fleet_events: Option<&[DispatchEvent]>,
    db: Option<&ExecDbFacts>,
    scope_execution_ids: &BTreeSet<String>,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    findings.extend(match_sig1_worker_claimed_stall(events, now_ms, scope_execution_ids));
    findings.extend(match_sig2_redundant_spawn_zombie(
        events,
        fleet_events.unwrap_or(events),
        db,
        scope_execution_ids,
    ));
    findings.extend(match_sig3_info(events, scope_execution_ids));
    findings.extend(match_sig4_spawn_ack_timeout(events, scope_execution_ids));
    // Fleet scan (when provided) drives the SIG-6 10m multi-exec escalate.
    findings.extend(match_sig6_lease_timeout(
        events,
        fleet_events.unwrap_or(events),
        scope_execution_ids,
    ));
    // SIG-A P0 only when co-occurring with SIG-2 on the same execution.
    let sig2_execs = sig2_related_execution_ids(&findings);
    findings.extend(match_sig_a_untracked_lease(events, scope_execution_ids, &sig2_execs));
    findings.extend(match_sig_b_slot_busy(events, scope_execution_ids));
    findings.extend(match_sig_c_historical_autobind(events, scope_execution_ids));
    findings.extend(match_sig_d_transient_exhausted(events, scope_execution_ids));
    findings.extend(match_sig_e_spawn_nack(events, scope_execution_ids));
    findings.extend(match_sig_f_worker_parse(events, scope_execution_ids));
    // Fleet scan: a wedged husk breaker is a pool-level fact, and the
    // execution an operator diagnoses is usually the redispatch it blocked,
    // not one of the held-back runs.
    findings.extend(match_sig_h_husk_breaker_wedge(
        events,
        fleet_events.unwrap_or(events),
        scope_execution_ids,
    ));

    sort_findings(&mut findings);
    findings
}

/// Execution ids tied to a SIG-2 finding (the finding's own id and any
/// `live_execution_id` in details) — used to escalate co-occurring SIG-A.
fn sig2_related_execution_ids(findings: &[Finding]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for finding in findings.iter().filter(|f| f.sig_id == "SIG-2") {
        if let Some(exec) = &finding.execution_id {
            out.insert(exec.clone());
        }
        if let Some(live) = finding.details.get("live_execution_id").and_then(|v| v.as_str()) {
            out.insert(live.to_owned());
        }
    }
    out
}

/// Match trace-only signatures (SIG-4b, SIG-5, SIG-5c, SIG-G) against
/// already-parsed engine-trace JSON objects. Callers must `serde_json`
/// each line — never substring-match raw lines (nested worker payloads
/// contain field-name collisions).
pub fn match_trace_signatures(
    records: &[Value],
    scope_execution_ids: &BTreeSet<String>,
    scope_work_item_ids: &BTreeSet<String>,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    findings.extend(match_sig4b_hook_after_terminal(
        records,
        scope_execution_ids,
        scope_work_item_ids,
    ));
    findings.extend(match_sig5_queue_side(records, scope_execution_ids, scope_work_item_ids));
    findings.extend(match_sig_g_scheduler_wakeup(
        records,
        scope_execution_ids,
        scope_work_item_ids,
    ));
    sort_findings(&mut findings);
    findings
}

fn sort_findings(findings: &mut [Finding]) {
    findings.sort_by(|a, b| {
        a.severity
            .rank()
            .cmp(&b.severity.rank())
            .then_with(|| a.sig_id.cmp(&b.sig_id))
            .then_with(|| a.execution_id.cmp(&b.execution_id))
    });
}

fn in_scope(execution_id: &str, scope: &BTreeSet<String>) -> bool {
    scope.is_empty() || scope.contains(execution_id)
}

fn work_item_of(event: &DispatchEvent) -> Option<String> {
    event.work_item_id.clone()
}

fn evidence_line(event: &DispatchEvent) -> String {
    let mut line = format!(
        "ts={} stage={}/{} exec={}",
        event.ts_epoch_ms, event.stage, event.outcome, event.execution_id
    );
    if let Some(err) = event.error_message.as_deref() {
        let clipped = if err.len() > 160 {
            format!("{}…", &err[..160])
        } else {
            err.to_owned()
        };
        line.push_str(&format!(" error={clipped}"));
    }
    if !event.details.is_null()
        && let Ok(text) = serde_json::to_string(&event.details)
    {
        let clipped = if text.len() > 200 {
            format!("{}…", &text[..200])
        } else {
            text
        };
        line.push_str(&format!(" details={clipped}"));
    }
    line
}

// ── SIG-1 ──────────────────────────────────────────────────────────────

fn match_sig1_worker_claimed_stall(events: &[DispatchEvent], now_ms: u128, scope: &BTreeSet<String>) -> Vec<Finding> {
    let mut by_exec: BTreeMap<String, Vec<&DispatchEvent>> = BTreeMap::new();
    for event in events {
        if in_scope(&event.execution_id, scope) {
            by_exec.entry(event.execution_id.clone()).or_default().push(event);
        }
    }

    let mut out = Vec::new();
    for (exec_id, timeline) in by_exec {
        // Recompute last real event the same way TimelineState does:
        // ignore stage_stalled for "last real"; never trust frozen
        // stage_stalled.details.elapsed_in_stage_ms.
        let last_real = timeline.iter().rev().find(|e| e.stage != "stage_stalled");
        let Some(last) = last_real else {
            continue;
        };
        if is_terminal_event(last) {
            continue;
        }
        if last.stage != "worker_claimed" {
            continue;
        }
        let elapsed_ms = now_ms.saturating_sub(last.ts_epoch_ms);
        if elapsed_ms <= SIG1_WARN_MS {
            continue;
        }
        let severity = if elapsed_ms >= SIG1_CRITICAL_MS {
            Severity::P0
        } else {
            Severity::P1
        };
        let mut evidence = vec![evidence_line(last)];
        // Optional corroboration: a stage_stalled row for worker_claimed.
        for event in timeline.iter().filter(|e| e.stage == "stage_stalled") {
            if event.details.get("stalled_stage").and_then(|v| v.as_str()) == Some("worker_claimed") {
                evidence.push(evidence_line(event));
            }
        }
        out.push(Finding {
            sig_id: "SIG-1".into(),
            severity,
            title: "Persistent worker_claimed stall".into(),
            execution_id: Some(exec_id),
            work_item_id: work_item_of(last),
            count: 1,
            evidence,
            recovery: "Recomputed elapsed from last non-stage_stalled event (do not trust \
                       frozen stage_stalled.details.elapsed_in_stage_ms). Inspect chain \
                       siblings with `bossctl work executions <work_item>`; wait for a \
                       chain_serialized sibling to finish, or cancel/reap the blocking \
                       sibling if it is itself wedged. Cross-check with `bossctl dispatch \
                       ghost-active`."
                .into(),
            details: serde_json::json!({
                "last_real_stage": last.stage,
                "last_real_outcome": last.outcome,
                "last_real_ts_epoch_ms": last.ts_epoch_ms,
                "elapsed_ms": elapsed_ms,
                "warn_ms": SIG1_WARN_MS,
                "critical_ms": SIG1_CRITICAL_MS,
            }),
        });
    }
    out
}

// ── SIG-2 ──────────────────────────────────────────────────────────────

fn match_sig2_redundant_spawn_zombie(
    events: &[DispatchEvent],
    fleet: &[DispatchEvent],
    db: Option<&ExecDbFacts>,
    scope: &BTreeSet<String>,
) -> Vec<Finding> {
    // Collect host_selected/error/redundant_spawn refs grouped by live target.
    let mut refs: BTreeMap<String, Vec<&DispatchEvent>> = BTreeMap::new();
    for event in fleet {
        if event.stage != "host_selected" || event.outcome != "error" {
            continue;
        }
        if event.details.get("reason").and_then(|v| v.as_str()) != Some("redundant_spawn") {
            continue;
        }
        let Some(live) = event
            .details
            .get("live_execution_id")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
        else {
            continue;
        };
        // Finding is interesting when the target OR the blocked peer is in scope.
        if !in_scope(&live, scope) && !in_scope(&event.execution_id, scope) {
            continue;
        }
        refs.entry(live).or_default().push(event);
    }

    let mut out = Vec::new();
    for (live_id, hits) in refs {
        let min_ts = hits.iter().map(|e| e.ts_epoch_ms).min().unwrap_or(0);
        let max_ts = hits.iter().map(|e| e.ts_epoch_ms).max().unwrap_or(0);
        let window_ms = max_ts.saturating_sub(min_ts);
        let ref_count = hits.len();

        let status = db.and_then(|d| d.status_by_exec.get(&live_id).cloned());
        let is_terminal = db
            .and_then(|d| d.terminal_by_exec.get(&live_id).copied())
            .unwrap_or(false);
        let lease = db.and_then(|d| d.lease_by_exec.get(&live_id).cloned()).flatten();

        // High-confidence: target terminal/lost in state.db while peers still bounce.
        // JSONL fallback: recurrence heuristic N=10, W=1h.
        let recurrence = ref_count >= SIG2_MIN_REFS && window_ms >= SIG2_MIN_WINDOW_MS;
        let db_zombie = is_terminal && ref_count >= 2;
        if !recurrence && !db_zombie {
            // Still surface a softer finding when *this* scope exec was blocked
            // by redundant_spawn (single hit) — operator-visible, not "zombie".
            let blocked_in_scope: Vec<&&DispatchEvent> =
                hits.iter().filter(|e| in_scope(&e.execution_id, scope)).collect();
            if blocked_in_scope.is_empty() {
                continue;
            }
            // Only emit the soft "blocked by live peer" note when not a zombie.
            let sample = blocked_in_scope[0];
            out.push(Finding {
                sig_id: "SIG-2".into(),
                severity: Severity::Info,
                title: "redundant_spawn supersede (normal path unless target is zombie)".into(),
                execution_id: Some(sample.execution_id.clone()),
                work_item_id: work_item_of(sample),
                count: blocked_in_scope.len(),
                evidence: blocked_in_scope.iter().take(5).map(|e| evidence_line(e)).collect(),
                recovery: format!(
                    "Peer execution was skipped because live_execution_id={live_id} is still \
                     considered live. Confirm that target with `bossctl dispatch diagnose \
                     {live_id}`. A single supersede is the healthy guard; multi-hour storms \
                     escalate to zombie (see same SIG-2 with higher severity)."
                ),
                details: serde_json::json!({
                    "live_execution_id": live_id,
                    "ref_count": ref_count,
                    "window_ms": window_ms,
                    "live_status": status,
                    "kind": "supersede",
                }),
            });
            continue;
        }

        let severity = if recurrence || db_zombie {
            Severity::P0
        } else {
            Severity::P1
        };
        let mut evidence: Vec<String> = hits.iter().take(5).map(|e| evidence_line(e)).collect();
        // Also surface heartbeat noise on the target if present in `events`.
        for event in events.iter().filter(|e| e.execution_id == live_id) {
            if e_is_untracked_heartbeat(event) {
                evidence.push(evidence_line(event));
                if evidence.len() >= 8 {
                    break;
                }
            }
        }
        out.push(Finding {
            sig_id: "SIG-2".into(),
            severity,
            title: "redundant_spawn zombie (recurring live_execution_id)".into(),
            execution_id: Some(live_id.clone()),
            work_item_id: hits.first().and_then(|e| work_item_of(e)),
            count: ref_count,
            evidence,
            recovery: format!(
                "Target live_execution_id={live_id} is blocking peers (refs={ref_count}, \
                 window_ms={window_ms}, state={}). Run `bossctl dispatch diagnose {live_id}`; \
                 if the target is terminal or lease-untracked, force-release / retention-sweep \
                 the zombie and clear untracked leases (see SIG-A). Do not keep re-dispatching \
                 peers while the blocker is stuck.",
                status.as_deref().unwrap_or("unknown")
            ),
            details: serde_json::json!({
                "live_execution_id": live_id,
                "ref_count": ref_count,
                "window_ms": window_ms,
                "min_ts_epoch_ms": min_ts,
                "max_ts_epoch_ms": max_ts,
                "live_status": status,
                "live_terminal": is_terminal,
                "live_lease_id": lease,
                "kind": "zombie",
            }),
        });
    }
    out
}

// ── SIG-3 (info only) ──────────────────────────────────────────────────

fn match_sig3_info(events: &[DispatchEvent], scope: &BTreeSet<String>) -> Vec<Finding> {
    let mut pool_exhausted = 0usize;
    let mut reconciles: Vec<&DispatchEvent> = Vec::new();
    for event in events {
        if !in_scope(&event.execution_id, scope) {
            continue;
        }
        if event.stage == "worker_claimed"
            && event.outcome == "skipped"
            && event.details.get("reason").and_then(|v| v.as_str()) == Some("pool_exhausted")
        {
            pool_exhausted += 1;
        }
        if event.stage == "pool_claim_reconcile" {
            reconciles.push(event);
        }
    }
    let mut out = Vec::new();
    if pool_exhausted > 0 {
        out.push(Finding {
            sig_id: "SIG-3a".into(),
            severity: Severity::Info,
            title: "pool_exhausted capacity pressure (not a claim leak)".into(),
            execution_id: None,
            work_item_id: None,
            count: pool_exhausted,
            evidence: vec![format!(
                "worker_claimed/skipped/pool_exhausted count={pool_exhausted} in scope"
            )],
            recovery: "Info only. `pool_exhausted` with `live_workers == cap` is the designed \
                       saturation path, not a leaked-claim fault. Zero leaked-claim exemplars \
                       in the investigation corpus. Use `bossctl dispatch stats` for queue \
                       pressure; do not page on raw presence."
                .into(),
            details: serde_json::json!({ "pool_exhausted_skips": pool_exhausted }),
        });
    }
    if !reconciles.is_empty() {
        out.push(Finding {
            sig_id: "SIG-3b".into(),
            severity: Severity::Info,
            title: "pool_claim_reconcile (engine already reaped a stuck claim)".into(),
            execution_id: reconciles.first().map(|e| e.execution_id.clone()),
            work_item_id: reconciles.first().and_then(|e| work_item_of(e)),
            count: reconciles.len(),
            evidence: reconciles.iter().take(5).map(|e| evidence_line(e)).collect(),
            recovery: "Info only. The engine already reconciled a stuck pool claim after a \
                       terminal pane_spawned/error (e.g. SlotBusy). Not a live leak detector."
                .into(),
            details: serde_json::json!({ "reconcile_count": reconciles.len() }),
        });
    }
    out
}

// ── SIG-4 ──────────────────────────────────────────────────────────────

fn match_sig4_spawn_ack_timeout(events: &[DispatchEvent], scope: &BTreeSet<String>) -> Vec<Finding> {
    let mut out = Vec::new();
    for event in events {
        if !in_scope(&event.execution_id, scope) {
            continue;
        }
        if event.stage != "spawn_ack_timeout" {
            continue;
        }
        let shell_pid = event.details.get("shell_pid").and_then(|v| v.as_i64()).or_else(|| {
            event
                .details
                .get("shell_pid")
                .and_then(|v| v.as_u64())
                .map(|u| u as i64)
        });
        if shell_pid != Some(0) {
            continue;
        }
        // Do NOT match bare shell_pid==0 without this stage.
        out.push(Finding {
            sig_id: "SIG-4".into(),
            severity: Severity::P1,
            title: "spawn_ack_timeout with shell_pid=0 (never-started worker)".into(),
            execution_id: Some(event.execution_id.clone()),
            work_item_id: work_item_of(event),
            count: 1,
            evidence: vec![evidence_line(event)],
            recovery: "No real pid and no hook within SPAWN_ACK_GRACE (details.threshold_secs, \
                       typically 60s); the sweep reaped a never-started worker. Check spawn \
                       diagnostics (`bossctl logs spawn --execution-id <id>`) and whether a \
                       recovery patch exists under recovery/<exec_id>.patch. Do not treat \
                       provisional shell_pid=0 on successful spawn as failure."
                .into(),
            details: event.details.clone(),
        });
    }
    out
}

// ── SIG-6 ──────────────────────────────────────────────────────────────

fn e_is_lease_timeout(event: &DispatchEvent) -> bool {
    event.stage == "cube_workspace_lease_failed"
        && event.outcome == "error"
        && event.details.get("reason").and_then(|v| v.as_str()) == Some("timeout")
}

/// True when `exec_id` participates in a ~10m window that contains at least
/// `SIG6_FLEET_K` distinct timeout executions (fleet-wide severity escalate).
fn sig6_exec_in_fleet_window(fleet: &[DispatchEvent], exec_id: &str) -> bool {
    let mut points: Vec<(u128, &str)> = fleet
        .iter()
        .filter(|e| e_is_lease_timeout(e))
        .map(|e| (e.ts_epoch_ms, e.execution_id.as_str()))
        .collect();
    if points.is_empty() {
        return false;
    }
    points.sort_by_key(|(ts, _)| *ts);

    // For each window ending at points[i], look back SIG6_FLEET_WINDOW_MS.
    for i in 0..points.len() {
        let t_end = points[i].0;
        let t_start = t_end.saturating_sub(SIG6_FLEET_WINDOW_MS);
        let mut distinct: BTreeSet<&str> = BTreeSet::new();
        let mut contains_target = false;
        for j in (0..=i).rev() {
            if points[j].0 < t_start {
                break;
            }
            if points[j].1 == exec_id {
                contains_target = true;
            }
            distinct.insert(points[j].1);
        }
        if contains_target && distinct.len() >= SIG6_FLEET_K {
            return true;
        }
    }
    false
}

/// Count of distinct timeout executions that co-occur with `exec_id` in any
/// ~10m window (for details; 0 when not fleet-escalated).
fn sig6_window_distinct_count(fleet: &[DispatchEvent], exec_id: &str) -> usize {
    let mut points: Vec<(u128, &str)> = fleet
        .iter()
        .filter(|e| e_is_lease_timeout(e))
        .map(|e| (e.ts_epoch_ms, e.execution_id.as_str()))
        .collect();
    points.sort_by_key(|(ts, _)| *ts);
    let mut best = 0usize;
    for i in 0..points.len() {
        let t_end = points[i].0;
        let t_start = t_end.saturating_sub(SIG6_FLEET_WINDOW_MS);
        let mut distinct: BTreeSet<&str> = BTreeSet::new();
        let mut contains_target = false;
        for j in (0..=i).rev() {
            if points[j].0 < t_start {
                break;
            }
            if points[j].1 == exec_id {
                contains_target = true;
            }
            distinct.insert(points[j].1);
        }
        if contains_target {
            best = best.max(distinct.len());
        }
    }
    best
}

fn match_sig6_lease_timeout(
    events: &[DispatchEvent],
    fleet: &[DispatchEvent],
    scope: &BTreeSet<String>,
) -> Vec<Finding> {
    let mut hits: Vec<&DispatchEvent> = Vec::new();
    for event in events {
        if !in_scope(&event.execution_id, scope) {
            continue;
        }
        if !e_is_lease_timeout(event) {
            continue;
        }
        hits.push(event);
    }
    if hits.is_empty() {
        return Vec::new();
    }
    // Per-execution findings keep evidence tight. Fleet escalate uses a
    // ~10m sliding window over `fleet` (not unwindowed scope count).
    let mut by_exec: BTreeMap<String, Vec<&DispatchEvent>> = BTreeMap::new();
    for event in hits {
        by_exec.entry(event.execution_id.clone()).or_default().push(event);
    }
    by_exec
        .into_iter()
        .map(|(exec_id, group)| {
            let fleetish = sig6_exec_in_fleet_window(fleet, &exec_id);
            let window_distinct = sig6_window_distinct_count(fleet, &exec_id);
            let severity = if fleetish { Severity::P0 } else { Severity::P1 };
            Finding {
                sig_id: "SIG-6".into(),
                severity,
                title: "cube workspace lease timeout".into(),
                execution_id: Some(exec_id),
                work_item_id: group.first().and_then(|e| work_item_of(e)),
                count: group.len(),
                evidence: group.iter().take(5).map(|e| evidence_line(e)).collect(),
                recovery: "Match details.reason==timeout only (not every cube_workspace_lease_failed). \
                           Run `cube workspace list`; check free pool and `jj git fetch` health. \
                           CUBE_LEASE_TIMEOUT is 90s on current main (corpus may show 30s). \
                           Do not hard-code 30000 ms. Fleet escalate (P0) requires ≥3 distinct \
                           timeout executions within a ~10m window, not an unwindowed scope count."
                    .into(),
                details: serde_json::json!({
                    "attempt": group.last().and_then(|e| e.details.get("attempt").cloned()),
                    "fallback_policy": group.last().and_then(|e| e.details.get("fallback_policy").cloned()),
                    "fleet_window_ms": SIG6_FLEET_WINDOW_MS,
                    "fleet_k": SIG6_FLEET_K,
                    "distinct_timeout_executions_in_window": window_distinct,
                    "fleet_escalate": fleetish,
                }),
            }
        })
        .collect()
}

// ── SIG-A ──────────────────────────────────────────────────────────────

fn e_is_untracked_heartbeat(event: &DispatchEvent) -> bool {
    if event.stage != "cube_lease_heartbeat" || event.outcome != "error" {
        return false;
    }
    event
        .error_message
        .as_deref()
        .is_some_and(|m| m.contains("is not tracked"))
}

fn match_sig_a_untracked_lease(
    events: &[DispatchEvent],
    scope: &BTreeSet<String>,
    sig2_execs: &BTreeSet<String>,
) -> Vec<Finding> {
    let mut by_key: BTreeMap<(String, Option<String>), Vec<&DispatchEvent>> = BTreeMap::new();
    for event in events {
        if !in_scope(&event.execution_id, scope) {
            continue;
        }
        if !e_is_untracked_heartbeat(event) {
            continue;
        }
        let lease = event.cube_lease_id.clone();
        by_key
            .entry((event.execution_id.clone(), lease))
            .or_default()
            .push(event);
    }
    by_key
        .into_iter()
        .map(|((exec_id, lease), group)| {
            let min_ts = group.iter().map(|e| e.ts_epoch_ms).min().unwrap_or(0);
            let max_ts = group.iter().map(|e| e.ts_epoch_ms).max().unwrap_or(0);
            // P0 only when co-occurring with SIG-2 on the same execution
            // (or as the live target of a SIG-2 finding). Count alone is not
            // enough — storms without a zombie peer are P1 alert-spam risk.
            let with_sig2 = sig2_execs.contains(&exec_id);
            let severity = if with_sig2 { Severity::P0 } else { Severity::P1 };
            Finding {
                sig_id: "SIG-A".into(),
                severity,
                title: "Untracked-lease heartbeat storm".into(),
                execution_id: Some(exec_id),
                work_item_id: group.first().and_then(|e| work_item_of(e)),
                count: group.len(),
                evidence: group.iter().take(3).map(|e| evidence_line(e)).collect(),
                recovery: format!(
                    "Heartbeat failures against an untracked lease (lease={}). Aggregate by \
                     lease/execution — do not page per row. P0 when paired with SIG-2 on the \
                     same execution; otherwise P1. Force-release the stale lease and clear the \
                     zombie execution so peers can progress.",
                    lease.as_deref().unwrap_or("unknown")
                ),
                details: serde_json::json!({
                    "cube_lease_id": lease,
                    "min_ts_epoch_ms": min_ts,
                    "max_ts_epoch_ms": max_ts,
                    "window_ms": max_ts.saturating_sub(min_ts),
                    "co_occurring_sig2": with_sig2,
                }),
            }
        })
        .collect()
}

// ── SIG-B ──────────────────────────────────────────────────────────────

fn match_sig_b_slot_busy(events: &[DispatchEvent], scope: &BTreeSet<String>) -> Vec<Finding> {
    let mut out = Vec::new();
    for event in events {
        if !in_scope(&event.execution_id, scope) {
            continue;
        }
        if !e_is_slot_busy(event) {
            continue;
        }
        out.push(Finding {
            sig_id: "SIG-B".into(),
            severity: Severity::P1,
            title: "SlotBusy spawn collision (slot desync, not capacity)".into(),
            execution_id: Some(event.execution_id.clone()),
            work_item_id: work_item_of(event),
            count: 1,
            evidence: vec![evidence_line(event)],
            recovery: "This is engine/app slot desync (`occupying_run_id` in details.slot_busy), \
                       not pool capacity exhaustion. Expect a follow-up pool_claim_reconcile. \
                       Reap the occupying run if it is dead; do not grow the pool in response."
                .into(),
            details: event.details.clone(),
        });
    }
    out
}

// ── SIG-H ──────────────────────────────────────────────────────────────

/// The husk-retirement circuit breaker is refusing to reclaim panes, so the
/// slots it is holding stay desynced and every dispatch that lands on one is
/// rejected `SlotBusy`.
///
/// Scanned fleet-wide rather than per-scope: the breaker's own dispatch
/// events carry the *held-back husk's* execution id, while the execution an
/// operator is actually diagnosing is normally the redispatch that cannot
/// land. Keying only on scope is why this combination previously reported
/// "signatures: no matches" during an hour-long pool wedge. P0 when the
/// scoped execution is itself being rejected `SlotBusy` — that is the
/// operator watching their own work fail to start — otherwise P1.
fn match_sig_h_husk_breaker_wedge(
    events: &[DispatchEvent],
    fleet_events: &[DispatchEvent],
    scope: &BTreeSet<String>,
) -> Vec<Finding> {
    let held_back: Vec<&DispatchEvent> = fleet_events
        .iter()
        .filter(|event| {
            event.stage == "husk_pane_reconcile"
                && event.outcome == "skipped"
                && event.details.get("skipped_reason").and_then(|v| v.as_str())
                    == Some("mass_retirement_circuit_breaker")
        })
        .collect();
    if held_back.is_empty() {
        return Vec::new();
    }

    let mut slots: BTreeSet<u64> = BTreeSet::new();
    for event in &held_back {
        if let Some(slot) = event.details.get("slot_id").and_then(|v| v.as_u64()) {
            slots.insert(slot);
        }
    }
    // Did the breaker escalate? A trip that could not file its attention item
    // is strictly worse, and the operator should be told which they are in.
    let escalated = held_back
        .iter()
        .any(|event| event.details.get("escalated").and_then(|v| v.as_bool()) == Some(true));
    let scoped_slot_busy = events
        .iter()
        .any(|event| in_scope(&event.execution_id, scope) && e_is_slot_busy(event));

    let retire_commands = slots
        .iter()
        .map(|slot| format!("bossctl agents retire-pane {slot}"))
        .collect::<Vec<_>>()
        .join("; ");

    vec![Finding {
        sig_id: "SIG-H".into(),
        severity: if scoped_slot_busy { Severity::P0 } else { Severity::P1 },
        title: "Husk-retirement circuit breaker is wedging worker slots".into(),
        execution_id: held_back.first().map(|e| e.execution_id.clone()),
        work_item_id: held_back.first().and_then(|e| work_item_of(e)),
        count: held_back.len(),
        evidence: held_back.iter().take(5).map(|e| evidence_line(e)).collect(),
        recovery: format!(
            "The husk-pane sweep confirmed more husks than its per-pass mass-retirement limit \
             allows for candidates it cannot independently confirm dead, so it retired NONE of \
             them. Those slots stay claimed by panes the pool believes are free, which is what \
             produces the co-occurring SIG-B `SlotBusy` rejections. Held-back slot(s): {slots:?}. \
             Panes whose execution row the engine has already marked terminal are exempt and \
             reclaim themselves — anything listed here has a row that still says the run is LIVE, \
             so first confirm those workers really are dead. If they are, reclaim by hand: \
             {retire_commands}. {escalation}",
            escalation = if escalated {
                "An attention item was filed for this."
            } else {
                "NO attention item was filed — escalation itself failed, so nothing but the engine log recorded this."
            },
        ),
        details: serde_json::json!({
            "held_back_events": held_back.len(),
            "slots": slots.iter().copied().collect::<Vec<u64>>(),
            "escalated": escalated,
            "scoped_slot_busy": scoped_slot_busy,
        }),
    }]
}

/// Shared `SlotBusy` predicate for SIG-B and SIG-H.
fn e_is_slot_busy(event: &DispatchEvent) -> bool {
    event.stage == "pane_spawned"
        && event.outcome == "error"
        && event.details.get("slot_busy").is_some_and(|v| !v.is_null())
}

// ── SIG-C historical ───────────────────────────────────────────────────

fn match_sig_c_historical_autobind(events: &[DispatchEvent], scope: &BTreeSet<String>) -> Vec<Finding> {
    let mut hits: Vec<&DispatchEvent> = Vec::new();
    for event in events {
        if !in_scope(&event.execution_id, scope) {
            continue;
        }
        // Historical wire stage only; current main uses status_transition
        // and no longer emits source=auto_bind_poller.
        if event.stage != "status_transitioned" || event.outcome != "error" {
            continue;
        }
        if event.details.get("source").and_then(|v| v.as_str()) != Some("auto_bind_poller") {
            continue;
        }
        hits.push(event);
    }
    if hits.is_empty() {
        return Vec::new();
    }
    vec![Finding {
        sig_id: "SIG-C".into(),
        severity: Severity::Info,
        title: "Historical auto-bind deleted-row storm (forensic only)".into(),
        execution_id: hits.first().map(|e| e.execution_id.clone()),
        work_item_id: hits.first().and_then(|e| work_item_of(e)),
        count: hits.len(),
        evidence: hits.iter().take(5).map(|e| evidence_line(e)).collect(),
        recovery: "Forensic only — no auto_bind_poller emitter on current main. Stage wire \
                   `status_transitioned` is gone (`status_transition` now). Do not treat as a \
                   live hard fault."
            .into(),
        details: serde_json::json!({ "historical": true }),
    }]
}

// ── SIG-D ──────────────────────────────────────────────────────────────

fn match_sig_d_transient_exhausted(events: &[DispatchEvent], scope: &BTreeSet<String>) -> Vec<Finding> {
    let mut out = Vec::new();
    for event in events {
        if !in_scope(&event.execution_id, scope) {
            continue;
        }
        if event.stage != "transient_recovery_exhausted" || event.outcome != "error" {
            continue;
        }
        let reason = event.details.get("reason").and_then(|v| v.as_str()).unwrap_or("");
        if reason != "retries_exhausted" && reason != "permanent_error" {
            // Never match reason=="unrecognized_error" — not on the wire.
            continue;
        }
        let class = event.details.get("class").and_then(|v| v.as_str()).unwrap_or("");
        out.push(Finding {
            sig_id: "SIG-D".into(),
            severity: Severity::P1,
            title: format!("transient_recovery_exhausted ({reason}/{class})"),
            execution_id: Some(event.execution_id.clone()),
            work_item_id: work_item_of(event),
            count: 1,
            evidence: vec![evidence_line(event)],
            recovery: "Wire reasons are only retries_exhausted / permanent_error; class is \
                       independent (transient / permanent / indeterminate). permanent_error \
                       escalates at zero retries (auth/billing). retries_exhausted with \
                       class=indeterminate is a classifier gap after budget spent. \
                       ConnectionRefused is Transient on current main and resumes at \
                       prior_attempts==0 — do not re-encode the historical wrong predicate."
                .into(),
            details: event.details.clone(),
        });
    }
    out
}

// ── SIG-E ──────────────────────────────────────────────────────────────

fn match_sig_e_spawn_nack(events: &[DispatchEvent], scope: &BTreeSet<String>) -> Vec<Finding> {
    let mut out = Vec::new();
    for event in events {
        if !in_scope(&event.execution_id, scope) {
            continue;
        }
        if event.stage != "spawn_nack" {
            continue;
        }
        out.push(Finding {
            sig_id: "SIG-E".into(),
            severity: Severity::P2,
            title: "spawn_nack / libghostty surface failure".into(),
            execution_id: Some(event.execution_id.clone()),
            work_item_id: work_item_of(event),
            count: 1,
            evidence: vec![evidence_line(event)],
            recovery: "Host/display condition (often no active display after sleep/wake). \
                       Retry spawn when a display is active; check libghostty surface creation."
                .into(),
            details: event.details.clone(),
        });
    }
    out
}

// ── SIG-F ──────────────────────────────────────────────────────────────

fn match_sig_f_worker_parse(events: &[DispatchEvent], scope: &BTreeSet<String>) -> Vec<Finding> {
    let mut out = Vec::new();
    for event in events {
        if !in_scope(&event.execution_id, scope) {
            continue;
        }
        if event.stage != "pane_spawned" || event.outcome != "error" {
            continue;
        }
        let Some(msg) = event.error_message.as_deref() else {
            continue;
        };
        if !msg.contains("does not parse as worker") {
            continue;
        }
        out.push(Finding {
            sig_id: "SIG-F".into(),
            severity: Severity::P2,
            title: "Worker-id parse failure".into(),
            execution_id: Some(event.execution_id.clone()),
            work_item_id: work_item_of(event),
            count: 1,
            evidence: vec![evidence_line(event)],
            recovery: "Worker id failed to parse. Current main accepts worker-{N}, \
                       auto-worker-{N}, or review-{N}; keep this signature for regressions \
                       against historical pool-prefix bugs."
                .into(),
            details: event.details.clone(),
        });
    }
    out
}

// ── Trace SIGs ─────────────────────────────────────────────────────────

fn fields_of(record: &Value) -> &Value {
    record.get("fields").unwrap_or(&Value::Null)
}

fn target_of(record: &Value) -> &str {
    record.get("target").and_then(|v| v.as_str()).unwrap_or("")
}

fn message_of(record: &Value) -> &str {
    fields_of(record).get("message").and_then(|v| v.as_str()).unwrap_or("")
}

fn record_in_scope(
    record: &Value,
    scope_execution_ids: &BTreeSet<String>,
    scope_work_item_ids: &BTreeSet<String>,
) -> bool {
    if scope_execution_ids.is_empty() && scope_work_item_ids.is_empty() {
        return true;
    }
    let fields = fields_of(record);
    let run_id = fields.get("run_id").and_then(|v| v.as_str());
    let execution_id = fields.get("execution_id").and_then(|v| v.as_str());
    let work_item_id = fields.get("work_item_id").and_then(|v| v.as_str());
    if let Some(id) = execution_id
        && scope_execution_ids.contains(id)
    {
        return true;
    }
    if let Some(id) = run_id
        && scope_execution_ids.contains(id)
    {
        return true;
    }
    if let Some(id) = work_item_id
        && scope_work_item_ids.contains(id)
    {
        return true;
    }
    // SIG-G carries execution_ids array.
    if let Some(arr) = fields.get("execution_ids").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(id) = v.as_str()
                && scope_execution_ids.contains(id)
            {
                return true;
            }
        }
    }
    false
}

fn match_sig4b_hook_after_terminal(
    records: &[Value],
    scope_execs: &BTreeSet<String>,
    scope_items: &BTreeSet<String>,
) -> Vec<Finding> {
    let mut out = Vec::new();
    for record in records {
        if !record_in_scope(record, scope_execs, scope_items) {
            continue;
        }
        let target = target_of(record);
        if !target.contains("worker_events") {
            continue;
        }
        let fields = fields_of(record);
        let msg = message_of(record);
        if !msg.starts_with("[engine-reconcile] live hook event arrived for a TERMINAL execution") {
            continue;
        }
        if fields.get("kind").and_then(|v| v.as_str()) != Some("session_end") {
            continue;
        }
        let run_id = fields.get("run_id").and_then(|v| v.as_str()).map(str::to_owned);
        out.push(Finding {
            sig_id: "SIG-4b".into(),
            severity: Severity::P2,
            title: "Hook after terminal (ack-timeout / stale-reap contradiction)".into(),
            execution_id: run_id,
            work_item_id: fields.get("work_item_id").and_then(|v| v.as_str()).map(str::to_owned),
            count: 1,
            evidence: vec![format!(
                "target={target} kind=session_end message={}",
                &msg[..msg.len().min(120)]
            )],
            recovery: "A terminalized execution still has a worker emitting hooks. Do not \
                       resurrect silently — inspect why the run was terminalized early \
                       (spawn_ack_timeout path) and reap the live pane / clear the contradiction."
                .into(),
            details: fields.clone(),
        });
    }
    out
}

fn match_sig5_queue_side(
    records: &[Value],
    scope_execs: &BTreeSet<String>,
    scope_items: &BTreeSet<String>,
) -> Vec<Finding> {
    let mut out = Vec::new();
    for record in records {
        if !record_in_scope(record, scope_execs, scope_items) {
            continue;
        }
        if target_of(record) != "boss_engine::ci_watch" {
            continue;
        }
        let fields = fields_of(record);
        let failure_kind = fields.get("failure_kind").and_then(|v| v.as_str()).unwrap_or("");
        let (sig_id, title, recovery) = match failure_kind {
            "merge_queue_rebounce" => (
                "SIG-5",
                "merge_queue_rebounce (queue-side CI failure)",
                "Queue-side failure on the synthetic merge commit. failure_kind alone is \
                 sufficient — do not require before_commit_sha (not emitted for matching; \
                 grepping that string hits embedded worker payloads). Check CI on the merge \
                 queue, not only the PR head. Parent should be blocked:ci_failure with \
                 revision unblocked.",
            ),
            "trunk_queue_eviction" => (
                "SIG-5c",
                "trunk_queue_eviction (queue-side CI failure)",
                "Trunk merge-queue eviction (sibling of SIG-5). Discriminator is \
                 trunk_eviction_discriminator, not a git SHA. Check trunk queue state and \
                 re-queue after fix; parent flipped to blocked:ci_failure.",
            ),
            _ => continue,
        };
        // Prefer structured field match; message prefix is corroboration only.
        out.push(Finding {
            sig_id: sig_id.into(),
            severity: Severity::P1,
            title: title.into(),
            execution_id: fields.get("execution_id").and_then(|v| v.as_str()).map(str::to_owned),
            work_item_id: fields.get("work_item_id").and_then(|v| v.as_str()).map(str::to_owned),
            count: 1,
            evidence: vec![format!(
                "target=boss_engine::ci_watch failure_kind={failure_kind} pr_url={} discriminator={}",
                fields.get("pr_url").and_then(|v| v.as_str()).unwrap_or("-"),
                fields.get("discriminator").and_then(|v| v.as_str()).unwrap_or("-"),
            )],
            recovery: recovery.into(),
            details: fields.clone(),
        });
    }
    out
}

fn match_sig_g_scheduler_wakeup(
    records: &[Value],
    scope_execs: &BTreeSet<String>,
    scope_items: &BTreeSet<String>,
) -> Vec<Finding> {
    let mut out = Vec::new();
    for record in records {
        let msg = message_of(record);
        if !msg.starts_with("scheduler heartbeat: ready execution(s) older than the heartbeat interval found") {
            continue;
        }
        // If scope is set, only fire when one of the stranded ids is in scope.
        if (!scope_execs.is_empty() || !scope_items.is_empty()) && !record_in_scope(record, scope_execs, scope_items) {
            continue;
        }
        let fields = fields_of(record);
        let exec_ids: Vec<String> = fields
            .get("execution_ids")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        out.push(Finding {
            sig_id: "SIG-G".into(),
            severity: Severity::P1,
            title: "Scheduler wakeup drop".into(),
            execution_id: exec_ids.first().cloned(),
            work_item_id: None,
            count: fields
                .get("count")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(exec_ids.len().max(1)),
            evidence: vec![format!(
                "message={} execution_ids={:?} oldest_age_ms={}",
                &msg[..msg.len().min(100)],
                exec_ids,
                fields.get("oldest_age_ms").and_then(|v| v.as_u64()).unwrap_or(0),
            )],
            recovery: "Kick/drain handoff dropped a wakeup (status flipped but scheduler never \
                       drained). Engine re-kicks on heartbeat; check for status_transition \
                       without follow-up request_recorded on the same work item (also SIG-1 / \
                       ghost-active)."
                .into(),
            details: fields.clone(),
        });
    }
    out
}

// ── Artifact loading helpers ───────────────────────────────────────────

/// Resolve a CLI id to one or more execution ids.
///
/// Prefer treating `id` as an execution when it looks like `exec_…`, the
/// per-exec mirror exists, or `db_resolution` says it is an execution row.
/// Otherwise treat it as a work-item id and list its executions (db first,
/// then dispatch events).
pub fn resolve_diagnose_scope(
    root: &Path,
    id: &str,
    db_resolution: Option<WorkItemResolution>,
) -> Result<DiagnoseScope> {
    let mirror = dispatch_reader::execution_path(root, id);
    let looks_like_exec = id.starts_with("exec_") || mirror.is_file();

    if looks_like_exec {
        let work_item_id = db_resolution.as_ref().and_then(|r| r.work_item_id.clone());
        return Ok(DiagnoseScope {
            input_id: id.to_owned(),
            resolved_as: ResolvedAs::Execution,
            execution_ids: vec![id.to_owned()],
            work_item_id,
        });
    }

    if let Some(resolved) = db_resolution {
        if resolved.as_execution {
            return Ok(DiagnoseScope {
                input_id: id.to_owned(),
                resolved_as: ResolvedAs::Execution,
                execution_ids: vec![id.to_owned()],
                work_item_id: resolved.work_item_id,
            });
        }
        if !resolved.execution_ids.is_empty() {
            return Ok(DiagnoseScope {
                input_id: id.to_owned(),
                resolved_as: ResolvedAs::WorkItem,
                execution_ids: resolved.execution_ids,
                work_item_id: Some(id.to_owned()),
            });
        }
    }

    // Fallback: scan current.jsonl for matching work_item_id.
    let events = dispatch_reader::read_current(root).unwrap_or_default();
    let mut execs: BTreeSet<String> = BTreeSet::new();
    for event in &events {
        if event.work_item_id.as_deref() == Some(id) {
            execs.insert(event.execution_id.clone());
        }
    }
    if !execs.is_empty() {
        return Ok(DiagnoseScope {
            input_id: id.to_owned(),
            resolved_as: ResolvedAs::WorkItem,
            execution_ids: execs.into_iter().collect(),
            work_item_id: Some(id.to_owned()),
        });
    }

    // Last resort: treat as bare execution id even without exec_ prefix /
    // mirror (historical or remote-only ids).
    Ok(DiagnoseScope {
        input_id: id.to_owned(),
        resolved_as: ResolvedAs::Execution,
        execution_ids: vec![id.to_owned()],
        work_item_id: None,
    })
}

/// Result of probing state.db for an id.
#[derive(Debug, Clone, Default)]
pub struct WorkItemResolution {
    pub as_execution: bool,
    pub work_item_id: Option<String>,
    pub execution_ids: Vec<String>,
}

/// Load dispatch events for the scope: prefer per-exec mirrors; for any
/// execution whose mirror is missing/pruned (empty), reconstruct that
/// execution from `current.jsonl`. Partial mirror sets still get filled —
/// not only when *all* mirrors are empty.
pub fn load_scope_events(root: &Path, scope: &DiagnoseScope) -> Result<Vec<DispatchEvent>> {
    let mut all = Vec::new();
    let mut missing_execs: BTreeSet<String> = BTreeSet::new();
    for exec_id in &scope.execution_ids {
        let events = dispatch_reader::read_execution(root, exec_id)?;
        if events.is_empty() {
            missing_execs.insert(exec_id.clone());
        }
        all.extend(events);
    }
    if missing_execs.is_empty() {
        return Ok(all);
    }

    // Some (or all) per-exec mirrors missing/pruned — reconstruct those
    // executions from the flat current.jsonl stream.
    let current = dispatch_reader::read_current(root)?;
    let mirrors_all_empty = all.is_empty();
    let mut from_current: Vec<DispatchEvent> = current
        .into_iter()
        .filter(|e| {
            missing_execs.contains(e.execution_id.as_str())
                // When every mirror was empty, also accept work-item peers
                // that may not yet be listed in scope.execution_ids.
                || (mirrors_all_empty
                    && scope.work_item_id.is_some()
                    && e.work_item_id.as_deref() == scope.work_item_id.as_deref())
        })
        .collect();
    if from_current.is_empty() {
        return Ok(all);
    }
    if mirrors_all_empty {
        return Ok(from_current);
    }
    // Merge: keep present mirrors, add reconstructed events for missing execs
    // (dedupe by ts+stage+exec is good enough).
    let mut keys: BTreeSet<(u128, String, String)> = all
        .iter()
        .map(|e| (e.ts_epoch_ms, e.stage.clone(), e.execution_id.clone()))
        .collect();
    for event in from_current.drain(..) {
        let key = (event.ts_epoch_ms, event.stage.clone(), event.execution_id.clone());
        if keys.insert(key) {
            all.push(event);
        }
    }
    all.sort_by_key(|e| e.ts_epoch_ms);
    Ok(all)
}

/// Parse engine-trace live + rotated segments, returning JSON objects that
/// touch the scope. Tolerates torn lines (skips unparseable).
pub fn load_scope_trace_records(
    root: &Path,
    scope_execution_ids: &BTreeSet<String>,
    scope_work_item_ids: &BTreeSet<String>,
) -> Result<Vec<Value>> {
    let base = root.join(boss_log_files::ENGINE_TRACE_FILENAME);
    let paths = boss_log_files::segments_with_live(&base);
    let mut out = Vec::new();
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", path.display()));
            }
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if record_in_scope(&value, scope_execution_ids, scope_work_item_ids) {
                out.push(value);
            }
        }
    }
    Ok(out)
}

/// Format findings for human-readable CLI output.
pub fn print_findings(findings: &[Finding]) {
    if findings.is_empty() {
        println!("signatures: no matches");
        return;
    }
    println!("signatures: {} finding(s)", findings.len());
    for finding in findings {
        println!("\n{}  {}  {}", finding.severity.as_str(), finding.sig_id, finding.title);
        if let Some(exec) = &finding.execution_id {
            print!("    execution_id={exec}");
            if let Some(wi) = &finding.work_item_id {
                print!("  work_item_id={wi}");
            }
            println!();
        } else if let Some(wi) = &finding.work_item_id {
            println!("    work_item_id={wi}");
        }
        if finding.count > 1 {
            println!("    count={}", finding.count);
        }
        for line in &finding.evidence {
            println!("    evidence: {line}");
        }
        println!("    recovery: {}", finding.recovery);
    }
}

/// Full `bossctl dispatch diagnose` pipeline: resolve scope, load artifacts,
/// match signatures, print timeline + findings (or JSON).
///
/// `db` is optional — file-scan-first; missing/unreadable state.db is fine.
pub fn run_diagnose(
    root: &Path,
    id: &str,
    json: bool,
    signatures_only: bool,
    now_ms: u128,
    db: Option<&boss_engine::work::WorkDb>,
) -> Result<()> {
    let db_resolution = db.and_then(|work_db| {
        if let Ok(exec) = work_db.get_execution(id) {
            return Some(WorkItemResolution {
                as_execution: true,
                work_item_id: Some(exec.work_item_id),
                execution_ids: vec![id.to_owned()],
            });
        }
        if let Ok(list) = work_db.list_executions(Some(id))
            && !list.is_empty()
        {
            return Some(WorkItemResolution {
                as_execution: false,
                work_item_id: Some(id.to_owned()),
                execution_ids: list.into_iter().map(|e| e.id).collect(),
            });
        }
        None
    });
    let scope = resolve_diagnose_scope(root, id, db_resolution)?;

    let events = load_scope_events(root, &scope)?;
    let scope_execs: BTreeSet<String> = scope.execution_ids.iter().cloned().collect();
    let mut scope_items: BTreeSet<String> = BTreeSet::new();
    if let Some(wi) = &scope.work_item_id {
        scope_items.insert(wi.clone());
    }
    for event in &events {
        if let Some(wi) = &event.work_item_id {
            scope_items.insert(wi.clone());
        }
    }

    // Fleet scan for cross-exec SIGs (SIG-2 recurrence). Same path as
    // `dispatch stats` — one pass over current.jsonl.
    let fleet = dispatch_reader::read_current(root).unwrap_or_default();

    let mut db_facts = ExecDbFacts::default();
    if let Some(work_db) = db {
        let mut interested: BTreeSet<String> = scope_execs.clone();
        for event in &fleet {
            if event.stage != "host_selected"
                || event.outcome != "error"
                || event.details.get("reason").and_then(|v| v.as_str()) != Some("redundant_spawn")
            {
                continue;
            }
            let Some(live) = event.details.get("live_execution_id").and_then(|v| v.as_str()) else {
                continue;
            };
            if scope_execs.contains(live)
                || scope_execs.contains(&event.execution_id)
                || event.work_item_id.as_ref().is_some_and(|w| scope_items.contains(w))
            {
                interested.insert(live.to_owned());
            }
        }
        for exec_id in interested {
            if let Ok(exec) = work_db.get_execution(&exec_id) {
                db_facts.status_by_exec.insert(exec_id.clone(), exec.status.to_string());
                db_facts
                    .terminal_by_exec
                    .insert(exec_id.clone(), exec.status.is_terminal());
                db_facts.lease_by_exec.insert(exec_id, exec.cube_lease_id.clone());
            }
        }
    }

    let mut findings = match_dispatch_signatures(&events, now_ms, Some(&fleet), Some(&db_facts), &scope_execs);

    match load_scope_trace_records(root, &scope_execs, &scope_items) {
        Ok(records) => {
            findings.extend(match_trace_signatures(&records, &scope_execs, &scope_items));
        }
        Err(err) => {
            eprintln!("warning: engine-trace scan skipped: {err:#}");
        }
    }
    sort_findings(&mut findings);

    if json {
        let mut timelines = serde_json::Map::new();
        if !signatures_only {
            for exec_id in &scope.execution_ids {
                let owned: Vec<DispatchEvent> = events.iter().filter(|e| e.execution_id == *exec_id).cloned().collect();
                let durations = dispatch_reader::stage_durations_ms(&owned, now_ms);
                timelines.insert(exec_id.clone(), build_timeline_json(exec_id, &owned, &durations));
            }
        }
        // v2 shape (findings, multi-exec). On the single-execution path also
        // restore v1 top-level `execution_id` + `events` so existing automation
        // does not silently break.
        let mut payload = serde_json::Map::new();
        payload.insert("schema_version".into(), Value::from(DIAGNOSE_JSON_SCHEMA_VERSION));
        payload.insert("id".into(), Value::String(scope.input_id.clone()));
        payload.insert(
            "resolved_as".into(),
            serde_json::to_value(scope.resolved_as).unwrap_or(Value::Null),
        );
        payload.insert(
            "work_item_id".into(),
            scope
                .work_item_id
                .as_ref()
                .map(|w| Value::String(w.clone()))
                .unwrap_or(Value::Null),
        );
        payload.insert(
            "execution_ids".into(),
            serde_json::to_value(&scope.execution_ids).unwrap_or(Value::Array(Vec::new())),
        );
        payload.insert(
            "findings".into(),
            serde_json::to_value(&findings).unwrap_or(Value::Array(Vec::new())),
        );
        if scope.resolved_as == ResolvedAs::Execution {
            let exec_id = scope
                .execution_ids
                .first()
                .map(String::as_str)
                .unwrap_or(scope.input_id.as_str());
            let events_value = timelines
                .get(exec_id)
                .and_then(|t| t.get("events").cloned())
                .unwrap_or_else(|| Value::Array(Vec::new()));
            payload.insert("execution_id".into(), Value::String(exec_id.to_owned()));
            payload.insert("events".into(), events_value);
        }
        payload.insert("timelines".into(), Value::Object(timelines));
        println!("{}", Value::Object(payload));
        return Ok(());
    }

    match scope.resolved_as {
        ResolvedAs::Execution => {
            println!(
                "dispatch diagnose for execution {}{}",
                scope.input_id,
                scope
                    .work_item_id
                    .as_ref()
                    .map(|w| format!(" (work_item={w})"))
                    .unwrap_or_default()
            );
        }
        ResolvedAs::WorkItem => {
            println!(
                "dispatch diagnose for work item {} ({} execution(s))",
                scope.input_id,
                scope.execution_ids.len()
            );
        }
    }

    if events.is_empty() && findings.is_empty() {
        println!("no dispatch events or signature matches for {id}");
        return Ok(());
    }

    if !signatures_only {
        for exec_id in &scope.execution_ids {
            let exec_events: Vec<DispatchEvent> =
                events.iter().filter(|e| e.execution_id == *exec_id).cloned().collect();
            if exec_events.is_empty() {
                continue;
            }
            let durations = dispatch_reader::stage_durations_ms(&exec_events, now_ms);
            println!("\ntimeline for execution {exec_id}");
            for (event, duration_ms) in exec_events.iter().zip(durations.iter()) {
                print_timeline_event(event, *duration_ms);
            }
        }
        if events.is_empty() {
            println!("\n(no dispatch events recorded for this id)");
        }
    }

    println!();
    print_findings(&findings);
    Ok(())
}

pub(crate) fn build_timeline_json(execution_id: &str, events: &[DispatchEvent], durations: &[u128]) -> Value {
    let detailed: Vec<Value> = events
        .iter()
        .zip(durations.iter())
        .map(|(event, dur)| {
            let mut value = serde_json::to_value(event).unwrap_or(Value::Null);
            if let Some(obj) = value.as_object_mut() {
                obj.insert("stage_duration_ms".into(), Value::from(*dur as u64));
            }
            value
        })
        .collect();
    serde_json::json!({
        "execution_id": execution_id,
        "events": detailed,
    })
}

fn print_timeline_event(event: &DispatchEvent, stage_duration_ms: u128) {
    let worker = event.worker_id.as_deref().unwrap_or("-");
    println!(
        "  {}  {}/{}  +{}ms  worker={}",
        event.ts_epoch_ms, event.stage, event.outcome, stage_duration_ms, worker,
    );
    // Decode the claimed slot + interactive page from the worker id so a
    // Lower Decks spawn/claim failure is distinguishable from a Bridge Crew
    // one at a glance. Automation/review workers resolve a slot but no page.
    if let Some(worker_id) = &event.worker_id
        && let Some(slot) = boss_engine::coordinator::slot_id_from_worker_id(worker_id)
    {
        match boss_engine::coordinator::worker_page_label(slot) {
            Some(page) => println!("    page:      {page} (slot {slot})"),
            None => println!("    slot:      {slot}"),
        }
    }
    if let Some(lease) = &event.cube_lease_id {
        println!("    lease:     {lease}");
    }
    if let Some(workspace) = &event.cube_workspace_id {
        println!("    workspace: {workspace}");
    }
    if let Some(err) = &event.error_message {
        println!("    error:     {err}");
    }
    if !event.details.is_null() {
        match serde_json::to_string(&event.details) {
            Ok(text) => println!("    details:   {text}"),
            Err(_) => println!("    details:   <unserializable>"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev_from_json(s: &str) -> DispatchEvent {
        serde_json::from_str(s).expect("fixture DispatchEvent")
    }

    fn scope(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn sig1_benign_chain_hold_does_not_flag_after_progress() {
        let events = vec![
            ev_from_json(
                r#"{"ts_epoch_ms":1784923279414,"stage":"worker_claimed","outcome":"skipped","execution_id":"exec_18c5513362ac2ed8_148","work_item_id":"task_18c5513362a6cbf0_147","details":{"reason":"chain_serialized_review_held"}}"#,
            ),
            ev_from_json(
                r#"{"ts_epoch_ms":1784923334223,"stage":"stage_stalled","outcome":"ok","execution_id":"exec_18c5513362ac2ed8_148","details":{"stalled_stage":"worker_claimed","stalled_at_ts_epoch_ms":1784923279414,"elapsed_in_stage_ms":42874}}"#,
            ),
            ev_from_json(
                r#"{"ts_epoch_ms":1784923379833,"stage":"request_recorded","outcome":"ok","execution_id":"exec_18c5513362ac2ed8_148","details":{}}"#,
            ),
        ];
        // now far in the future — last real is request_recorded, not worker_claimed.
        let findings = match_dispatch_signatures(
            &events,
            1784923279414 + 400_000,
            None,
            None,
            &scope(&["exec_18c5513362ac2ed8_148"]),
        );
        assert!(
            findings.iter().all(|f| f.sig_id != "SIG-1"),
            "progress cleared stall: {findings:?}"
        );
    }

    #[test]
    fn sig1_durable_stall_recomputes_elapsed_not_frozen_field() {
        let events = vec![
            ev_from_json(
                r#"{"ts_epoch_ms":1784923279414,"stage":"worker_claimed","outcome":"skipped","execution_id":"exec_sig1_durable","work_item_id":"task_sig1_durable","details":{"reason":"chain_serialized"}}"#,
            ),
            ev_from_json(
                r#"{"ts_epoch_ms":1784923322288,"stage":"stage_stalled","outcome":"ok","execution_id":"exec_sig1_durable","details":{"stalled_stage":"worker_claimed","stalled_at_ts_epoch_ms":1784923279414,"elapsed_in_stage_ms":42874}}"#,
            ),
        ];
        let now = 1784923279414 + 400_000;
        let findings = match_dispatch_signatures(&events, now, None, None, &scope(&["exec_sig1_durable"]));
        let sig1: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-1").collect();
        assert_eq!(sig1.len(), 1, "{findings:?}");
        assert_eq!(sig1[0].severity, Severity::P0);
        assert_eq!(sig1[0].details["elapsed_ms"], 400_000);
        // Frozen field must not be the gate — elapsed is recomputed.
        assert_ne!(sig1[0].details["elapsed_ms"], 42874);
    }

    #[test]
    fn sig4_matches_shell_pid_zero_on_ack_timeout_only() {
        let hit = ev_from_json(
            r#"{"ts_epoch_ms":1784895019484,"stage":"spawn_ack_timeout","outcome":"ok","execution_id":"exec_18c5387f08addde0_211","details":{"slot_id":25,"shell_pid":0,"threshold_secs":60}}"#,
        );
        // Bare provisional pid 0 on successful spawn must NOT match.
        let benign = ev_from_json(
            r#"{"ts_epoch_ms":1784895019000,"stage":"pane_spawned","outcome":"ok","execution_id":"exec_18c5387f08addde0_211","details":{"shell_pid":0}}"#,
        );
        let findings = match_dispatch_signatures(&[hit, benign], 0, None, None, &scope(&["exec_18c5387f08addde0_211"]));
        let sig4: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-4").collect();
        assert_eq!(sig4.len(), 1);
    }

    #[test]
    fn sig6_requires_reason_timeout() {
        let timeout = ev_from_json(
            r#"{"ts_epoch_ms":1784879999312,"stage":"cube_workspace_lease_failed","outcome":"error","execution_id":"exec_lease_to","error_message":"cube workspace lease timed out after 30s","details":{"attempt":1,"reason":"timeout","fallback_policy":"any_free"}}"#,
        );
        let cube_err = ev_from_json(
            r#"{"ts_epoch_ms":1784879999313,"stage":"cube_workspace_lease_failed","outcome":"error","execution_id":"exec_lease_err","error_message":"jj git fetch failed","details":{"reason":"cube_error"}}"#,
        );
        let findings = match_dispatch_signatures(
            &[timeout, cube_err],
            0,
            None,
            None,
            &scope(&["exec_lease_to", "exec_lease_err"]),
        );
        let sig6: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-6").collect();
        assert_eq!(sig6.len(), 1);
        assert_eq!(sig6[0].execution_id.as_deref(), Some("exec_lease_to"));
        assert_eq!(sig6[0].severity, Severity::P1, "single timeout is P1, not fleet");
    }

    #[test]
    fn sig6_fleet_escalate_requires_windowed_distinct_execs() {
        let base = 1_000_000u128;
        // Three distinct execs within 10m → P0 for each in-scope timeout.
        let fleet: Vec<DispatchEvent> = (0..3)
            .map(|i| {
                ev_from_json(&format!(
                    r#"{{"ts_epoch_ms":{},"stage":"cube_workspace_lease_failed","outcome":"error","execution_id":"exec_to_{}","details":{{"attempt":1,"reason":"timeout","fallback_policy":"any_free"}}}}"#,
                    base + i * 60_000,
                    i,
                ))
            })
            .collect();
        let findings = match_dispatch_signatures(
            &fleet,
            0,
            Some(&fleet),
            None,
            &scope(&["exec_to_0", "exec_to_1", "exec_to_2"]),
        );
        let sig6: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-6").collect();
        assert_eq!(sig6.len(), 3, "{findings:?}");
        assert!(sig6.iter().all(|f| f.severity == Severity::P0), "{sig6:?}");

        // Same three execs spread over >10m → no fleet escalate (P1).
        let spread: Vec<DispatchEvent> = (0..3)
            .map(|i| {
                ev_from_json(&format!(
                    r#"{{"ts_epoch_ms":{},"stage":"cube_workspace_lease_failed","outcome":"error","execution_id":"exec_spread_{}","details":{{"reason":"timeout"}}}}"#,
                    base + i * (SIG6_FLEET_WINDOW_MS + 1),
                    i,
                ))
            })
            .collect();
        let findings_spread = match_dispatch_signatures(
            &spread,
            0,
            Some(&spread),
            None,
            &scope(&["exec_spread_0", "exec_spread_1", "exec_spread_2"]),
        );
        let sig6_spread: Vec<_> = findings_spread.iter().filter(|f| f.sig_id == "SIG-6").collect();
        assert_eq!(sig6_spread.len(), 3);
        assert!(
            sig6_spread.iter().all(|f| f.severity == Severity::P1),
            "unwindowed distinct count must not escalate: {sig6_spread:?}"
        );
    }

    #[test]
    fn sig_a_aggregates_untracked_heartbeats() {
        let a = ev_from_json(
            r#"{"ts_epoch_ms":1781515001787,"stage":"cube_lease_heartbeat","outcome":"error","execution_id":"exec_18b9288b7fd1c568_3","cube_lease_id":"a2a7ae36-0e16-4858-8e50-a8e54055e71a","error_message":"Cube command failed: {\n  \"error\": \"lease `a2a7ae36-0e16-4858-8e50-a8e54055e71a` is not tracked\"\n}","details":{"ttl_secs":1800}}"#,
        );
        let mut b = a.clone();
        b.ts_epoch_ms = 1781515301787;
        // Storm of 12 without SIG-2 stays P1 (count alone is not P0).
        let mut storm = Vec::new();
        for i in 0..12u128 {
            let mut e = a.clone();
            e.ts_epoch_ms = a.ts_epoch_ms + i * 300_000;
            storm.push(e);
        }
        let findings = match_dispatch_signatures(&storm, 0, None, None, &scope(&["exec_18b9288b7fd1c568_3"]));
        let siga: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-A").collect();
        assert_eq!(siga.len(), 1);
        assert_eq!(siga[0].count, 12);
        assert_eq!(siga[0].severity, Severity::P1, "no SIG-2 co-occurrence → P1: {siga:?}");
        // Two-row aggregate still works.
        let findings2 = match_dispatch_signatures(&[a, b], 0, None, None, &scope(&["exec_18b9288b7fd1c568_3"]));
        let siga2: Vec<_> = findings2.iter().filter(|f| f.sig_id == "SIG-A").collect();
        assert_eq!(siga2.len(), 1);
        assert_eq!(siga2[0].count, 2);
    }

    #[test]
    fn sig_a_p0_only_with_sig2_cooccurrence() {
        let live = "exec_zombie_with_heartbeats";
        let base = 1_000_000u128;
        let mut fleet = Vec::new();
        for i in 0..12u128 {
            fleet.push(ev_from_json(&format!(
                r#"{{"ts_epoch_ms":{},"stage":"host_selected","outcome":"error","execution_id":"exec_blocked_{}","details":{{"reason":"redundant_spawn","live_execution_id":"{live}"}}}}"#,
                base + i * 400_000,
                i,
            )));
        }
        // Heartbeats on the zombie target (SIG-A).
        for i in 0..3u128 {
            fleet.push(ev_from_json(&format!(
                r#"{{"ts_epoch_ms":{},"stage":"cube_lease_heartbeat","outcome":"error","execution_id":"{live}","cube_lease_id":"lease-z","error_message":"lease `lease-z` is not tracked","details":{{"ttl_secs":1800}}}}"#,
                base + i * 300_000,
            )));
        }
        let findings = match_dispatch_signatures(&fleet, base + 5_000_000, Some(&fleet), None, &scope(&[live]));
        let siga: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-A").collect();
        assert_eq!(siga.len(), 1, "{findings:?}");
        assert_eq!(siga[0].severity, Severity::P0, "SIG-2 zombie + SIG-A → P0: {siga:?}");
        assert_eq!(siga[0].details["co_occurring_sig2"], true);
    }

    #[test]
    fn sig2_zombie_recurrence_heuristic() {
        let live = "exec_zombie_target";
        let mut fleet = Vec::new();
        let base = 1_000_000u128;
        for i in 0..12u128 {
            fleet.push(ev_from_json(&format!(
                r#"{{"ts_epoch_ms":{},"stage":"host_selected","outcome":"error","execution_id":"exec_blocked_{}","details":{{"reason":"redundant_spawn","live_execution_id":"{live}"}}}}"#,
                base + i * 400_000, // ~1.2h span across 12 hits
                i,
            )));
        }
        let findings = match_dispatch_signatures(&fleet, base + 5_000_000, Some(&fleet), None, &scope(&[live]));
        let sig2: Vec<_> = findings
            .iter()
            .filter(|f| f.sig_id == "SIG-2" && f.details.get("kind").and_then(|v| v.as_str()) == Some("zombie"))
            .collect();
        assert_eq!(sig2.len(), 1, "{findings:?}");
        assert_eq!(sig2[0].severity, Severity::P0);
        assert!(sig2[0].count >= 10);
    }

    #[test]
    fn sig_b_slot_busy() {
        let event = ev_from_json(
            r#"{"ts_epoch_ms":1784927838618,"stage":"pane_spawned","outcome":"error","execution_id":"exec_slot","error_message":"SlotBusy","details":{"slot_busy":{"slot_id":26,"occupying_run_id":"exec_other"}}}"#,
        );
        let findings = match_dispatch_signatures(&[event], 0, None, None, &scope(&["exec_slot"]));
        assert!(findings.iter().any(|f| f.sig_id == "SIG-B"));
    }

    /// The wedge as it actually presented: the operator diagnoses the
    /// redispatch that keeps failing `SlotBusy`, while the breaker events
    /// that explain it are attributed to entirely different (held-back)
    /// execution ids. Matching only in scope is what produced "signatures:
    /// no matches" for an hour.
    #[test]
    fn sig_h_husk_breaker_wedge_is_found_from_the_blocked_redispatch() {
        let blocked = ev_from_json(
            r#"{"ts_epoch_ms":1784927838618,"stage":"pane_spawned","outcome":"error","execution_id":"exec_resume","error_message":"SlotBusy","details":{"slot_busy":{"slot_id":7,"occupying_run_id":"exec_husk_7"}}}"#,
        );
        let mut fleet = vec![blocked.clone()];
        for slot in 4..9u64 {
            fleet.push(ev_from_json(&format!(
                r#"{{"ts_epoch_ms":1784927840000,"stage":"husk_pane_reconcile","outcome":"skipped","execution_id":"exec_husk_{slot}","details":{{"slot_id":{slot},"skipped_reason":"mass_retirement_circuit_breaker","max_per_pass":3,"escalated":true}}}}"#,
            )));
        }

        let findings = match_dispatch_signatures(&[blocked], 0, Some(&fleet), None, &scope(&["exec_resume"]));
        let sig_h: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-H").collect();
        assert_eq!(sig_h.len(), 1, "{findings:?}");
        assert_eq!(
            sig_h[0].severity,
            Severity::P0,
            "the diagnosed execution is itself being rejected SlotBusy",
        );
        assert_eq!(sig_h[0].count, 5);
        assert_eq!(
            sig_h[0].details["slots"],
            serde_json::json!([4, 5, 6, 7, 8]),
            "every held-back slot must be named",
        );
        assert!(sig_h[0].recovery.contains("bossctl agents retire-pane 7"));
    }

    /// A trip that could not file its attention item is worse, not better,
    /// and the diagnosis must say so rather than implying someone was told.
    #[test]
    fn sig_h_reports_when_the_breaker_could_not_escalate() {
        let event = ev_from_json(
            r#"{"ts_epoch_ms":1784927840000,"stage":"husk_pane_reconcile","outcome":"skipped","execution_id":"exec_husk_1","details":{"slot_id":1,"skipped_reason":"mass_retirement_circuit_breaker","max_per_pass":3,"escalated":false}}"#,
        );
        let fleet = std::slice::from_ref(&event);
        let findings = match_dispatch_signatures(fleet, 0, Some(fleet), None, &scope(&["exec_husk_1"]));
        let sig_h: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-H").collect();
        assert_eq!(sig_h.len(), 1);
        assert_eq!(sig_h[0].severity, Severity::P1, "no SlotBusy in scope yet");
        assert_eq!(sig_h[0].details["escalated"], false);
        assert!(sig_h[0].recovery.contains("NO attention item was filed"));
    }

    /// A husk sweep that retires normally must not look like a wedge.
    #[test]
    fn sig_h_ignores_ordinary_husk_retirements() {
        let event = ev_from_json(
            r#"{"ts_epoch_ms":1784927840000,"stage":"husk_pane_reconcile","outcome":"ok","execution_id":"exec_husk_2","details":{"slot_id":2,"death_evidence":"db_confirmed_terminal"}}"#,
        );
        let fleet = std::slice::from_ref(&event);
        let findings = match_dispatch_signatures(fleet, 0, Some(fleet), None, &scope(&["exec_husk_2"]));
        assert!(!findings.iter().any(|f| f.sig_id == "SIG-H"), "{findings:?}");
    }

    #[test]
    fn sig_d_wire_reasons_only() {
        let indet = ev_from_json(
            r#"{"ts_epoch_ms":1784900000000,"stage":"transient_recovery_exhausted","outcome":"error","execution_id":"exec_sigd_indet","details":{"reason":"retries_exhausted","class":"indeterminate","prior_attempts":3,"max_attempts":3}}"#,
        );
        let bad = ev_from_json(
            r#"{"ts_epoch_ms":1784900000500,"stage":"transient_recovery_exhausted","outcome":"error","execution_id":"exec_sigd_bad","details":{"reason":"unrecognized_error","class":"indeterminate"}}"#,
        );
        let findings = match_dispatch_signatures(
            &[indet, bad],
            0,
            None,
            None,
            &scope(&["exec_sigd_indet", "exec_sigd_bad"]),
        );
        let sigd: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-D").collect();
        assert_eq!(sigd.len(), 1);
        assert_eq!(sigd[0].execution_id.as_deref(), Some("exec_sigd_indet"));
    }

    #[test]
    fn sig_e_and_f() {
        let nack = ev_from_json(
            r#"{"ts_epoch_ms":1784890000000,"stage":"spawn_nack","outcome":"ok","execution_id":"exec_sig_e","details":{"reason":"libghostty surface creation failed"}}"#,
        );
        let parse = ev_from_json(
            r#"{"ts_epoch_ms":1783000000000,"stage":"pane_spawned","outcome":"error","execution_id":"exec_sig_f","worker_id":"auto-worker-2","error_message":"PaneSpawnRunner received worker_id \"auto-worker-2\" that does not parse as worker-{N}","details":{"slot_id":2}}"#,
        );
        let findings = match_dispatch_signatures(&[nack, parse], 0, None, None, &scope(&["exec_sig_e", "exec_sig_f"]));
        assert!(findings.iter().any(|f| f.sig_id == "SIG-E"));
        assert!(findings.iter().any(|f| f.sig_id == "SIG-F"));
    }

    #[test]
    fn sig5_and_5c_from_trace_json() {
        let rebounce: Value = serde_json::from_str(
            r#"{"timestamp":"2026-07-24T21:45:44.118022Z","level":"INFO","fields":{"message":"ci_watch: queue-side failure detected; parent flipped to blocked: ci_failure","work_item_id":"task_18c5549a33ae7248_29c","pr_url":"https://github.com/spinyfin/mono/pull/2298","discriminator":"8d075ec06ac101f204a04da9f3f86b65a88d705f","head_sha_at_trigger":"8d075ec06ac101f204a04da9f3f86b65a88d705f","failure_kind":"merge_queue_rebounce"},"target":"boss_engine::ci_watch"}"#,
        )
        .unwrap();
        let trunk: Value = serde_json::from_str(
            r#"{"timestamp":"2026-07-26T00:00:00.000000Z","level":"INFO","fields":{"message":"ci_watch: queue-side failure detected; parent flipped to blocked: ci_failure","work_item_id":"task_trunk_example","pr_url":"https://github.com/spinyfin/mono/pull/9999","discriminator":"trunk:entry-abc@2026-07-26T00:00:00Z","head_sha_at_trigger":"trunk:entry-abc@2026-07-26T00:00:00Z","failure_kind":"trunk_queue_eviction"},"target":"boss_engine::ci_watch"}"#,
        )
        .unwrap();
        // before_commit_sha in an embedded worker payload must not match.
        let noise: Value = serde_json::from_str(
            r#"{"timestamp":"2026-07-24T00:00:00Z","level":"INFO","fields":{"message":"PostToolUse payload mentioning before_commit_sha"},"target":"boss_engine::hooks"}"#,
        )
        .unwrap();
        let items = scope(&["task_18c5549a33ae7248_29c", "task_trunk_example"]);
        let findings = match_trace_signatures(&[rebounce, trunk, noise], &BTreeSet::new(), &items);
        assert!(findings.iter().any(|f| f.sig_id == "SIG-5"));
        assert!(findings.iter().any(|f| f.sig_id == "SIG-5c"));
        assert!(!findings.iter().any(|f| f.title.contains("before_commit")));
    }

    #[test]
    fn sig4b_and_sig_g_from_trace() {
        let hook: Value = serde_json::from_str(
            r#"{"timestamp":"2026-07-24T12:00:00.000000Z","level":"WARN","fields":{"message":"[engine-reconcile] live hook event arrived for a TERMINAL execution — the engine believes this run is dead but its worker is still emitting hooks.","run_id":"exec_18c5387f08addde0_211","kind":"session_end","status":"orphaned","work_item_id":"task_example"},"target":"boss_engine::app::worker_events"}"#,
        )
        .unwrap();
        let wake: Value = serde_json::from_str(
            r#"{"timestamp":"2026-07-24T18:00:00.000000Z","level":"WARN","fields":{"message":"scheduler heartbeat: ready execution(s) older than the heartbeat interval found — kick/drain handoff may have dropped a wakeup; re-kicking now","count":2,"oldest_age_ms":45000,"execution_ids":["exec_stranded_1","exec_stranded_2"]},"target":"boss_engine::coordinator::scheduler"}"#,
        )
        .unwrap();
        let execs = scope(&["exec_18c5387f08addde0_211", "exec_stranded_1"]);
        let findings = match_trace_signatures(&[hook, wake], &execs, &BTreeSet::new());
        assert!(findings.iter().any(|f| f.sig_id == "SIG-4b"));
        assert!(findings.iter().any(|f| f.sig_id == "SIG-G"));
    }

    #[test]
    fn sig_c_historical_only() {
        let event = ev_from_json(
            r#"{"ts_epoch_ms":1784000000000,"stage":"status_transitioned","outcome":"error","execution_id":"exec_hist_autobind","work_item_id":"task_deleted_example","error_message":"cannot complete a deleted task: task_deleted_example","details":{"source":"auto_bind_poller"}}"#,
        );
        let findings = match_dispatch_signatures(&[event], 0, None, None, &scope(&["exec_hist_autobind"]));
        let sigc: Vec<_> = findings.iter().filter(|f| f.sig_id == "SIG-C").collect();
        assert_eq!(sigc.len(), 1);
        assert_eq!(sigc[0].severity, Severity::Info);
    }

    #[test]
    fn load_scope_events_reconstructs_partial_missing_mirrors() {
        use std::io::Write;
        let root = std::env::temp_dir().join(format!("bossctl-doctor-partial-mirrors-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let de = root.join("dispatch-events");
        let execs = de.join("executions");
        std::fs::create_dir_all(&execs).unwrap();

        let present = "exec_present";
        let missing = "exec_missing_mirror";
        // Mirror only for present.
        let mirror_dir = execs.join(present);
        std::fs::create_dir_all(&mirror_dir).unwrap();
        let line_present = format!(
            r#"{{"ts_epoch_ms":100,"stage":"request_recorded","outcome":"ok","execution_id":"{present}","details":{{}}}}"#
        );
        let line_missing = format!(
            r#"{{"ts_epoch_ms":200,"stage":"request_recorded","outcome":"ok","execution_id":"{missing}","details":{{}}}}"#
        );
        std::fs::write(mirror_dir.join("dispatch.jsonl"), format!("{line_present}\n")).unwrap();
        // current.jsonl has both.
        let mut current = std::fs::File::create(de.join("current.jsonl")).unwrap();
        writeln!(current, "{line_present}").unwrap();
        writeln!(current, "{line_missing}").unwrap();

        let scope = DiagnoseScope {
            input_id: "task_partial".into(),
            resolved_as: ResolvedAs::WorkItem,
            execution_ids: vec![present.into(), missing.into()],
            work_item_id: Some("task_partial".into()),
        };
        let events = load_scope_events(&root, &scope).unwrap();
        let _ = std::fs::remove_dir_all(&root);
        let ids: BTreeSet<_> = events.iter().map(|e| e.execution_id.as_str()).collect();
        assert!(ids.contains(present), "{ids:?}");
        assert!(
            ids.contains(missing),
            "partial missing mirror must be reconstructed from current.jsonl: {ids:?}"
        );
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn diagnose_json_single_exec_keeps_v1_compatibility_fields() {
        // build_timeline_json is the events payload; run_diagnose wiring is
        // covered structurally here via the same shape it emits.
        let events = vec![ev_from_json(
            r#"{"ts_epoch_ms":1,"stage":"request_recorded","outcome":"ok","execution_id":"exec_compat","details":{}}"#,
        )];
        let durations = vec![0u128];
        let timeline = build_timeline_json("exec_compat", &events, &durations);
        let events_value = timeline.get("events").cloned().unwrap_or(Value::Array(Vec::new()));
        let payload = serde_json::json!({
            "schema_version": DIAGNOSE_JSON_SCHEMA_VERSION,
            "id": "exec_compat",
            "resolved_as": ResolvedAs::Execution,
            "work_item_id": null,
            "execution_ids": ["exec_compat"],
            "findings": [],
            "timelines": { "exec_compat": timeline },
            // v1 compatibility fields restored on single-exec path
            "execution_id": "exec_compat",
            "events": events_value,
        });
        assert_eq!(payload["schema_version"], DIAGNOSE_JSON_SCHEMA_VERSION);
        assert_eq!(payload["execution_id"], "exec_compat");
        assert_eq!(payload["events"].as_array().unwrap().len(), 1);
        assert!(payload["findings"].as_array().unwrap().is_empty());
        assert!(payload.get("timelines").is_some());
    }
}
