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
use boss_engine::dispatch_reader::{self, DamagedLine, is_terminal_event};
use serde::Serialize;
use serde_json::Value;

/// Run the same tmux preflight as engine startup so an operator can diagnose
/// a blocked local dispatcher without first starting the app.
pub async fn run_tmux_preflight(json: bool) -> Result<()> {
    let preflight = boss_engine::tmux_preflight::TmuxPreflight::probe().await;
    match preflight {
        boss_engine::tmux_preflight::TmuxPreflight::Ready { program, version } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ready": true,
                        "program": program,
                        "version": format!("{}.{}", version.major, version.minor),
                    })
                );
            } else {
                println!("tmux ready: {} {}.{}", program.display(), version.major, version.minor);
            }
            Ok(())
        }
        boss_engine::tmux_preflight::TmuxPreflight::Unavailable { reason } => {
            if json {
                println!("{}", serde_json::json!({ "ready": false, "reason": reason }));
            } else {
                eprintln!("tmux unavailable: {reason}");
            }
            anyhow::bail!("tmux preflight failed")
        }
    }
}

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
/// SIG-H recency gate: only `husk_pane_reconcile/skipped/mass_retirement_circuit_breaker`
/// evidence within this window of `now_ms` counts toward a live trip.
/// `fleet_events` spans every retained rotated segment of `current.jsonl`
/// (potentially many days), and the breaker re-trips (and re-logs) every
/// husk-sweep pass — 60s on current main — for as long as the condition
/// actually holds. 15x that interval tolerates missed/slow passes (engine
/// restart, host contention) while still excluding a trip that stopped
/// recurring long ago.
pub const SIG_H_RECENCY_WINDOW_MS: u128 = 900_000; // 15 minutes
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
    /// True when this finding's conclusion rests on an event NOT being present
    /// in the timeline (rather than on the events that ARE there).
    ///
    /// Deliberately a field every construction site must set, not a lookup
    /// table keyed on `sig_id`: a table is something a new signature can be
    /// added without updating, which is the same "proceeds as if nothing is
    /// missing" failure this whole surface exists to remove. Declaring it wrong
    /// is possible; declaring it by accident is not.
    ///
    /// When it is set and the stream had unreadable records overlapping the
    /// finding's window, `run_diagnose` marks the finding unreliable rather
    /// than presenting it as fact — see
    /// [`crate::stream_integrity::ABSENCE_CAVEAT`].
    #[builder(default)]
    pub absence_based: bool,
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
        now_ms,
        db,
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

/// True when `event` happened within `window_ms` of `now_ms`.
///
/// Shared recency gate for any signature whose count is a scan over
/// `fleet_events` (every retained rotated segment of `current.jsonl`,
/// potentially days of history) rather than a bounded per-execution
/// timeline. Without this, matching that kind of cumulative count reports a
/// condition that stopped recurring days ago with the same certainty as one
/// that is actively recurring right now — see SIG-H. Signatures that already
/// carry their own dedicated window (SIG-2's recurrence span, SIG-6's fleet
/// escalate) do not need this on top; it exists for signatures with no
/// window at all.
fn within_recency_window(event: &DispatchEvent, now_ms: u128, window_ms: u128) -> bool {
    now_ms.saturating_sub(event.ts_epoch_ms) <= window_ms
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
            // "This stage stalled" is entirely a claim about what did NOT
            // follow it: the last real event is non-terminal and nothing came
            // after. An unreadable record in the gap could be the very event
            // that moved the stage on.
            absence_based: true,
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
                absence_based: false,
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
            absence_based: false,
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
            absence_based: false,
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
            absence_based: false,
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
            absence_based: false,
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
                absence_based: false,
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
                absence_based: false,
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
            absence_based: false,
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
///
/// Two defects this now guards against, both found from a stale live trip
/// reported as current nine days after the last real occurrence:
///
/// - `fleet_events` is every retained rotated segment of `current.jsonl`, so
///   an unwindowed scan treats a breaker trip from days ago exactly like one
///   from the last minute. [`within_recency_window`] gates both `count` and
///   the printed evidence on [`SIG_H_RECENCY_WINDOW_MS`], and the finding
///   does not fire at all when nothing recent matches.
/// - The recovery text names concrete `retire-pane` commands. A slot whose
///   occupying execution is still LIVE in state.db must never appear in one
///   of those commands, even "for confirmation" — so slots are partitioned
///   by current DB liveness and only confirmed-terminal ones get a command.
fn match_sig_h_husk_breaker_wedge(
    events: &[DispatchEvent],
    fleet_events: &[DispatchEvent],
    scope: &BTreeSet<String>,
    now_ms: u128,
    db: Option<&ExecDbFacts>,
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

    // Recency gate: a trip that stopped recurring is not a live condition.
    // The breaker re-trips (and re-logs) every husk-sweep pass for as long
    // as the condition actually holds, so genuine live evidence is never
    // more than a pass or two old.
    let mut recent: Vec<&DispatchEvent> = held_back
        .iter()
        .copied()
        .filter(|event| within_recency_window(event, now_ms, SIG_H_RECENCY_WINDOW_MS))
        .collect();
    if recent.is_empty() {
        return Vec::new();
    }
    // Newest first: both the printed evidence and the per-slot liveness
    // lookup below must reflect the latest record for a slot, not whichever
    // happened to sort first in the file.
    recent.sort_by_key(|e| std::cmp::Reverse(e.ts_epoch_ms));

    let mut slot_latest: BTreeMap<u64, &DispatchEvent> = BTreeMap::new();
    for event in &recent {
        if let Some(slot) = event.details.get("slot_id").and_then(|v| v.as_u64()) {
            // `recent` is newest-first, so the first insert per slot wins.
            slot_latest.entry(slot).or_insert(*event);
        }
    }
    // Did the breaker escalate? A trip that could not file its attention item
    // is strictly worse, and the operator should be told which they are in.
    let escalated = recent
        .iter()
        .any(|event| event.details.get("escalated").and_then(|v| v.as_bool()) == Some(true));
    let scoped_slot_busy = events
        .iter()
        .any(|event| in_scope(&event.execution_id, scope) && e_is_slot_busy(event));

    // Partition by CURRENT liveness of the execution occupying the slot
    // (state.db, not the dispatch log): only a slot whose row is confirmed
    // terminal is safe to name in a `retire-pane` command. A slot whose row
    // is confirmed still running, and a slot whose liveness this scan simply
    // could not determine (no db, or the exec row wasn't found), are two
    // different facts — conflating them into one "do not retire" bucket
    // would let a genuinely unknown case get described as definitely live.
    let mut confirmed_dead: Vec<u64> = Vec::new();
    let mut confirmed_live: Vec<u64> = Vec::new();
    let mut unknown_liveness: Vec<u64> = Vec::new();
    for (&slot, event) in &slot_latest {
        match db.and_then(|d| d.terminal_by_exec.get(&event.execution_id).copied()) {
            Some(true) => confirmed_dead.push(slot),
            Some(false) => confirmed_live.push(slot),
            None => unknown_liveness.push(slot),
        }
    }
    let slots: Vec<u64> = slot_latest.keys().copied().collect();

    let reclaim_text = if confirmed_dead.is_empty() {
        "No held-back slot has a state.db row confirmed terminal yet — do not retire any of \
         them by hand until one does."
            .to_owned()
    } else {
        let retire_commands = confirmed_dead
            .iter()
            .map(|slot| format!("bossctl agents retire-pane {slot}"))
            .collect::<Vec<_>>()
            .join("; ");
        format!("Confirmed-terminal slot(s) are safe to reclaim by hand: {retire_commands}.")
    };
    let mut caveat_text = String::new();
    if !confirmed_live.is_empty() {
        caveat_text.push_str(&format!(
            " Slot(s) {confirmed_live:?} have a state.db row that says the run is LIVE — do NOT \
             retire them."
        ));
    }
    if !unknown_liveness.is_empty() {
        caveat_text.push_str(&format!(
            " Slot(s) {unknown_liveness:?} could not be checked ({}) — do NOT retire them \
             without confirming liveness some other way.",
            if db.is_none() {
                "state.db was unavailable to this scan"
            } else {
                "no state.db row found for the occupying execution"
            },
        ));
    }

    vec![Finding {
        sig_id: "SIG-H".into(),
        absence_based: false,
        severity: if scoped_slot_busy { Severity::P0 } else { Severity::P1 },
        title: "Husk-retirement circuit breaker is wedging worker slots".into(),
        execution_id: recent.first().map(|e| e.execution_id.clone()),
        work_item_id: recent.first().and_then(|e| work_item_of(e)),
        count: recent.len(),
        evidence: recent.iter().take(5).map(|e| evidence_line(e)).collect(),
        recovery: format!(
            "The husk-pane sweep confirmed more husks than its per-pass mass-retirement limit \
             allows for candidates it cannot independently confirm dead, so it retired NONE of \
             them within the last {window_min} minute(s). Those slots stay claimed by panes the \
             pool believes are free, which is what produces the co-occurring SIG-B `SlotBusy` \
             rejections. Held-back slot(s): {slots:?}. {reclaim_text}{caveat_text} {escalation}",
            window_min = SIG_H_RECENCY_WINDOW_MS / 60_000,
            escalation = if escalated {
                "An attention item was filed for this."
            } else {
                "NO attention item was filed — escalation itself failed, so nothing but the engine log recorded this."
            },
        ),
        details: serde_json::json!({
            "held_back_events_in_window": recent.len(),
            "held_back_events_lifetime": held_back.len(),
            "recency_window_ms": SIG_H_RECENCY_WINDOW_MS,
            "slots": slots,
            "slots_confirmed_dead": confirmed_dead,
            "slots_confirmed_live": confirmed_live,
            "slots_unknown_liveness": unknown_liveness,
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
        absence_based: false,
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
            absence_based: false,
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
        // Both stages are the same failure class: the app could not stand up
        // a worker shell for a pane it accepted. Which one lands depends on
        // whether the app NACKed the spawn or the pane-death report won the
        // race (and an older app build only ever sends the latter), so
        // matching just `spawn_nack` would leave the incident undiagnosed.
        if event.stage != "spawn_nack" && event.stage != "pane_death_before_start" {
            continue;
        }
        out.push(Finding {
            sig_id: "SIG-E".into(),
            absence_based: false,
            severity: Severity::P2,
            title: "spawn_nack / pane_death_before_start / libghostty surface failure".into(),
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
            absence_based: false,
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
            // The recovery text tells the reader to conclude from the absence
            // of a paired `live_worker_readopted` / `husk_pane_reconcile` that
            // the convergence path did not run. That inference is only as sound
            // as the stream's completeness.
            absence_based: true,
            severity: Severity::P2,
            title: "Hook after terminal (ack-timeout / stale-reap contradiction)".into(),
            execution_id: run_id,
            work_item_id: fields.get("work_item_id").and_then(|v| v.as_str()).map(str::to_owned),
            count: 1,
            evidence: vec![format!(
                "target={target} kind=session_end message={}",
                &msg[..msg.len().min(120)]
            )],
            recovery: "A terminalized execution still has a worker emitting hooks. The engine \
                       now resolves this itself — it re-adopts the run when the terminal status \
                       was only an inference (orphaned/abandoned), or reaps the surviving \
                       process when it was a decision (cancelled/completed/failed). Look for the \
                       paired `live_worker_readopted` or `husk_pane_reconcile` event in \
                       `bossctl dispatch tail`: if neither is present the convergence path did \
                       not run and this IS still an open contradiction. Either way, the \
                       underlying question stands — why was the run terminalized early? Check \
                       the spawn_ack_timeout path and the network conditions around it."
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
            absence_based: false,
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
            absence_based: false,
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

    // Fallback: scan current.jsonl for matching work_item_id. Damage is
    // deliberately discarded here — this is id resolution, not evidence, and
    // the same file is read again (with its integrity reported) by
    // `run_diagnose` moments later.
    let events = dispatch_reader::read_current(root)
        .unwrap_or_default()
        .into_events_ignoring_damage();
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

/// Events for a diagnose scope, together with which files they came from could
/// not be read in full.
///
/// Damage is kept split by source rather than pooled, because the two sources
/// mean different things for a single execution's timeline. A per-execution
/// mirror holds only that execution's records, so damage in it is unambiguously
/// damage to that timeline. `current.jsonl` is the whole fleet's stream, so a
/// damaged line in it belongs to *some* execution — only when a timeline was
/// reconstructed from it (its mirror was missing or pruned) is that damage
/// definitely this timeline's. Pooling the two would either over-attribute
/// fleet damage to every execution or under-attribute real mirror damage.
#[derive(Debug, Clone, Default)]
pub struct ScopeEvents {
    pub events: Vec<DispatchEvent>,
    /// Damage in each execution's own mirror, keyed by execution id.
    pub mirror_damage: BTreeMap<String, Vec<DamagedLine>>,
    /// Damage in the flat `current.jsonl` stream.
    pub flat_damage: Vec<DamagedLine>,
    /// Executions whose timeline had to be reconstructed from the flat stream,
    /// making `flat_damage` their timeline's damage too.
    pub reconstructed_from_flat: BTreeSet<String>,
    /// Executions this load actually covered, i.e. the ones for which
    /// [`ScopeEvents::damage_for`] is an answer rather than a default. An
    /// execution outside this set (a `live_execution_id` pulled in by the fleet
    /// scan, say) has no mirror read for it, so an empty `damage_for` must not be
    /// mistaken for "its timeline is intact".
    pub covered: BTreeSet<String>,
}

impl ScopeEvents {
    /// Damage that bears on `execution_id`'s own timeline.
    pub fn damage_for(&self, execution_id: &str) -> Vec<DamagedLine> {
        let mut out = self.mirror_damage.get(execution_id).cloned().unwrap_or_default();
        if self.reconstructed_from_flat.contains(execution_id) {
            out.extend(self.flat_damage.iter().cloned());
        }
        out
    }

    /// True when this load read `execution_id`'s own dispatch source, so
    /// [`ScopeEvents::damage_for`] is authoritative for it.
    pub fn covers(&self, execution_id: &str) -> bool {
        self.covered.contains(execution_id)
    }

    /// Every damaged line seen while loading the scope.
    pub fn all_damage(&self) -> Vec<DamagedLine> {
        let mut out: Vec<DamagedLine> = self.mirror_damage.values().flatten().cloned().collect();
        out.extend(self.flat_damage.iter().cloned());
        out
    }
}

/// Load dispatch events for the scope: prefer per-exec mirrors; for any
/// execution whose mirror is missing/pruned (empty), reconstruct that
/// execution from `current.jsonl`. Partial mirror sets still get filled —
/// not only when *all* mirrors are empty.
pub fn load_scope_events(root: &Path, scope: &DiagnoseScope) -> Result<ScopeEvents> {
    let mut loaded = ScopeEvents::default();
    let mut missing_execs: BTreeSet<String> = BTreeSet::new();
    loaded.covered = scope.execution_ids.iter().cloned().collect();
    for exec_id in &scope.execution_ids {
        let read = dispatch_reader::read_execution(root, exec_id)?;
        if !read.damage.is_empty() {
            loaded.mirror_damage.insert(exec_id.clone(), read.damage);
        }
        if read.events.is_empty() {
            missing_execs.insert(exec_id.clone());
        }
        loaded.events.extend(read.events);
    }
    if missing_execs.is_empty() {
        return Ok(loaded);
    }

    // Some (or all) per-exec mirrors missing/pruned — reconstruct those
    // executions from the flat current.jsonl stream. Its damage now bears on
    // those timelines, since it is the only record of them we have.
    let current = dispatch_reader::read_current(root)?;
    loaded.flat_damage = current.damage;
    loaded.reconstructed_from_flat = missing_execs.clone();
    let mirrors_all_empty = loaded.events.is_empty();
    let mut from_current: Vec<DispatchEvent> = current
        .events
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
        return Ok(loaded);
    }
    if mirrors_all_empty {
        loaded.events = from_current;
        return Ok(loaded);
    }
    // Merge: keep present mirrors, add reconstructed events for missing execs
    // (dedupe by ts+stage+exec is good enough).
    let mut keys: BTreeSet<(u128, String, String)> = loaded
        .events
        .iter()
        .map(|e| (e.ts_epoch_ms, e.stage.clone(), e.execution_id.clone()))
        .collect();
    for event in from_current.drain(..) {
        let key = (event.ts_epoch_ms, event.stage.clone(), event.execution_id.clone());
        if keys.insert(key) {
            loaded.events.push(event);
        }
    }
    loaded.events.sort_by_key(|e| e.ts_epoch_ms);
    Ok(loaded)
}

/// Parse engine-trace live + rotated segments, returning JSON objects that
/// touch the scope. Tolerates torn lines (skips unparseable).
///
/// Unlike the dispatch stream, this one is read as-is: the same survey that
/// found 25 damaged lines in a single rotated `current.jsonl` segment found no
/// damage at all across the engine-trace segments (6 files, 102,066 records —
/// zero blank lines, zero concatenated records, zero torn records), so there is
/// nothing here to salvage or account for.
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

/// Mark every absence-based finding whose evidence could be hiding the event it
/// says is missing as unreliable, rather than letting it be read as established
/// fact.
///
/// **Attribution matters as much as conservatism.** The damage tested is the
/// damage that bears on *this* finding, not the pooled fleet view:
///
/// - A finding about an execution `loaded` covered is tested against
///   [`ScopeEvents::damage_for`] — its own mirror, plus the flat stream only when
///   its timeline was reconstructed from it. The sink writes every event to both
///   the flat stream and the mirror, so a torn line in `current.jsonl` does not
///   imply this execution's mirror is missing anything, and neither does damage
///   in some other execution's mirror. Stamping `UNRELIABLE:` on a timeline the
///   same command just reported `complete: true` is a precision loss, not
///   caution — and a caveat that appears on findings it does not apply to is a
///   caveat readers learn to skip.
/// - A finding with no execution id, or about an execution outside the load, has
///   no narrower window available, so it falls back to the pooled view.
///
/// The window is `[first event of this finding's execution, now]` — the span
/// over which the missing event could have occurred.
pub fn qualify_absence_findings(
    findings: &mut [Finding],
    events: &[DispatchEvent],
    loaded: &ScopeEvents,
    integrity: &crate::stream_integrity::IntegrityReport,
    now_ms: u128,
) -> usize {
    if integrity.lost_lines().next().is_none() {
        return 0;
    }
    let mut qualified = 0;
    for finding in findings.iter_mut().filter(|f| f.absence_based) {
        let window_start = finding
            .execution_id
            .as_deref()
            .and_then(|exec| {
                events
                    .iter()
                    .filter(|e| e.execution_id == exec)
                    .map(|e| e.ts_epoch_ms)
                    .min()
            })
            .unwrap_or(0);
        let could_hide = match finding.execution_id.as_deref() {
            Some(exec) if loaded.covers(exec) => loaded
                .damage_for(exec)
                .iter()
                .filter(|line| line.shape.lost_records())
                .any(|line| line.overlaps_window(window_start, now_ms)),
            _ => integrity.could_hide_event_in(window_start, now_ms),
        };
        if !could_hide {
            continue;
        }
        finding
            .evidence
            .push(crate::stream_integrity::ABSENCE_CAVEAT.to_owned());
        if let Some(obj) = finding.details.as_object_mut() {
            obj.insert("absence_unreliable".into(), Value::Bool(true));
        } else {
            finding.details = serde_json::json!({ "absence_unreliable": true });
        }
        qualified += 1;
    }
    qualified
}

/// Format findings for human-readable CLI output.
///
/// `integrity` is not optional: "no matches" printed over a stream with holes
/// in it is not a clean bill of health, and this is the last place that
/// distinction can still be made before an operator reads it as one.
pub fn render_findings(findings: &[Finding], integrity: &crate::stream_integrity::IntegrityReport) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    if findings.is_empty() {
        let _ = writeln!(out, "signatures: no matches");
        if !integrity.is_intact() {
            let _ = writeln!(out, "  {}", crate::stream_integrity::NO_MATCHES_CAVEAT);
        }
        return out;
    }
    let _ = writeln!(out, "signatures: {} finding(s)", findings.len());
    for finding in findings {
        let _ = writeln!(
            out,
            "\n{}  {}  {}",
            finding.severity.as_str(),
            finding.sig_id,
            finding.title
        );
        if let Some(exec) = &finding.execution_id {
            let _ = write!(out, "    execution_id={exec}");
            if let Some(wi) = &finding.work_item_id {
                let _ = write!(out, "  work_item_id={wi}");
            }
            let _ = writeln!(out);
        } else if let Some(wi) = &finding.work_item_id {
            let _ = writeln!(out, "    work_item_id={wi}");
        }
        if finding.count > 1 {
            let _ = writeln!(out, "    count={}", finding.count);
        }
        for line in &finding.evidence {
            let _ = writeln!(out, "    evidence: {line}");
        }
        let _ = writeln!(out, "    recovery: {}", finding.recovery);
    }
    out
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

    let loaded = load_scope_events(root, &scope)?;
    let events = loaded.events.clone();
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
    let fleet_read = dispatch_reader::read_current(root).unwrap_or_default();
    let fleet = fleet_read.events;

    // One deduplicated integrity view over every file this command read: the
    // per-execution mirrors, the flat stream loaded for reconstruction, and the
    // flat stream loaded for the fleet scan (the last two are the same file, so
    // dedupe by file+line is what stops it being double-reported).
    let mut all_damage = loaded.all_damage();
    all_damage.extend(fleet_read.damage);

    let integrity = crate::stream_integrity::IntegrityReport::new(all_damage);

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
        // SIG-H needs current liveness for every husk-occupied slot it might
        // name in a `retire-pane` command — the dispatch log alone cannot
        // tell it whether that execution is still running.
        for event in &fleet {
            if event.stage == "husk_pane_reconcile"
                && event.outcome == "skipped"
                && event.details.get("skipped_reason").and_then(|v| v.as_str())
                    == Some("mass_retirement_circuit_breaker")
            {
                interested.insert(event.execution_id.clone());
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
    let qualified = qualify_absence_findings(&mut findings, &events, &loaded, &integrity, now_ms);

    if json {
        let mut timelines = serde_json::Map::new();
        if !signatures_only {
            for exec_id in &scope.execution_ids {
                let owned: Vec<DispatchEvent> = events.iter().filter(|e| e.execution_id == *exec_id).cloned().collect();
                let durations = dispatch_reader::stage_durations_ms(&owned, now_ms);
                let mut timeline = build_timeline_json(exec_id, &owned, &durations);
                // Per-timeline, not only in the top-level block: an automated
                // caller that reads one timeline out of `timelines` must not be
                // able to consume it without the reason it may be incomplete.
                if let Some(obj) = timeline.as_object_mut() {
                    let damage = loaded.damage_for(exec_id);
                    obj.insert(
                        "unreadable_records".into(),
                        serde_json::to_value(&damage).unwrap_or(Value::Array(Vec::new())),
                    );
                    obj.insert(
                        "complete".into(),
                        Value::Bool(!damage.iter().any(|d| d.shape.lost_records())),
                    );
                }
                timelines.insert(exec_id.clone(), timeline);
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
        let mut integrity_json = integrity.to_json();
        if let Some(obj) = integrity_json.as_object_mut() {
            obj.insert("absence_findings_qualified".into(), Value::from(qualified));
        }
        payload.insert("stream_integrity".into(), integrity_json);
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

    // Before the timeline, not after: an operator who reads a timeline and then
    // learns it had holes has already formed the conclusion this is meant to
    // prevent.
    integrity.print_banner();

    if events.is_empty() && findings.is_empty() {
        // "Nothing here" is precisely the conclusion damage invalidates, so it
        // is the one case that must never be stated bare.
        if integrity.is_intact() {
            println!("no dispatch events or signature matches for {id}");
        } else {
            println!("no READABLE dispatch events or signature matches for {id}");
            println!("  {}", crate::stream_integrity::NO_EVENTS_CAVEAT);
        }
        integrity.print_footer();
        return Ok(());
    }

    if !signatures_only {
        for exec_id in &scope.execution_ids {
            let exec_events: Vec<DispatchEvent> =
                events.iter().filter(|e| e.execution_id == *exec_id).cloned().collect();
            let damage = loaded.damage_for(exec_id);
            if exec_events.is_empty() && damage.is_empty() {
                continue;
            }
            println!("\ntimeline for execution {exec_id}");
            print!("{}", render_timeline(&exec_events, &damage, now_ms));
        }
        if events.is_empty() {
            println!("\n(no dispatch events recorded for this id)");
        }
    }

    println!();
    print!("{}", render_findings(&findings, &integrity));
    integrity.print_footer();
    Ok(())
}

/// Render one execution's timeline with its unreadable regions marked in place.
///
/// A marker is emitted at the position the damaged bytes actually occupied —
/// after the last record read before them — so the gap is visible where it
/// happened rather than as a note at the end. Damage that no record precedes
/// leads the timeline.
///
/// Returns the rendered text rather than printing, so a test can assert the
/// marker really is *inside* the timeline: "the timeline carries a visible
/// marker" is a claim about the rendered output, and a printer proves nothing.
pub fn render_timeline(events: &[DispatchEvent], damage: &[DamagedLine], now_ms: u128) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    let durations = dispatch_reader::stage_durations_ms(events, now_ms);
    let markers = crate::stream_integrity::timeline_markers(damage);
    let mut next_marker = 0usize;

    // Leading markers: damage with no preceding record.
    while let Some(marker) = markers.get(next_marker) {
        if marker.after_ts_epoch_ms.is_some() {
            break;
        }
        let _ = writeln!(out, "  {}", marker.text);
        next_marker += 1;
    }

    for (event, duration_ms) in events.iter().zip(durations.iter()) {
        write_timeline_event(&mut out, event, *duration_ms);
        while let Some(marker) = markers.get(next_marker) {
            match marker.after_ts_epoch_ms {
                Some(after) if after <= event.ts_epoch_ms => {
                    let _ = writeln!(out, "  {}", marker.text);
                    next_marker += 1;
                }
                _ => break,
            }
        }
    }

    // Anything left belongs after the last record we could read.
    for marker in &markers[next_marker.min(markers.len())..] {
        let _ = writeln!(out, "  {}", marker.text);
    }
    out
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

fn write_timeline_event(out: &mut String, event: &DispatchEvent, stage_duration_ms: u128) {
    use std::fmt::Write;

    let worker = event.worker_id.as_deref().unwrap_or("-");
    let _ = writeln!(
        out,
        "  {}  {}/{}  +{}ms  worker={}",
        event.ts_epoch_ms, event.stage, event.outcome, stage_duration_ms, worker,
    );
    // Decode the claimed slot + interactive page from the worker id so a
    // Lower Decks spawn/claim failure is distinguishable from a Bridge Crew
    // one at a glance. Automation/review workers resolve a slot but no page.
    if let Some(worker_id) = &event.worker_id
        && let Some(slot) = boss_engine::coordinator::slot_id_from_worker_id(worker_id)
    {
        let _ = match boss_engine::coordinator::worker_page_label(slot) {
            Some(page) => writeln!(out, "    page:      {page} (slot {slot})"),
            None => writeln!(out, "    slot:      {slot}"),
        };
    }
    if let Some(lease) = &event.cube_lease_id {
        let _ = writeln!(out, "    lease:     {lease}");
    }
    if let Some(workspace) = &event.cube_workspace_id {
        let _ = writeln!(out, "    workspace: {workspace}");
    }
    if let Some(err) = &event.error_message {
        let _ = writeln!(out, "    error:     {err}");
    }
    if !event.details.is_null() {
        let _ = match serde_json::to_string(&event.details) {
            Ok(text) => writeln!(out, "    details:   {text}"),
            Err(_) => writeln!(out, "    details:   <unserializable>"),
        };
    }
}

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod tests;
