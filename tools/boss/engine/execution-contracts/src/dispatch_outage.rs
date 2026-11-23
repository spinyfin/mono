//! Global dispatch outage tracking: the condition where every registered host
//! is excluded for every dispatchable item, so no new worker can start.
//!
//! Three pieces, all pure of storage so they test without a database:
//!
//! - [`classify_host_exclusions`] decides whether one refused host selection
//!   is global. It is only global when every host is excluded for a reason
//!   that does not depend on the item (disabled, or the local host held by
//!   the startup quarantine), or when a failed startup worker scan holds every
//!   work item ([`scan_error_exclusions`]). An item that merely does not fit
//!   one host — missing capability, pinned elsewhere, pool full — is not an
//!   outage. This is deliberately narrower than "no queued item can be placed
//!   anywhere": an enabled host that lacks what every queued item needs is
//!   not reported (see `tools/boss/docs/dispatch-outage.md`).
//! - [`DispatchOutageTracker`] is the one outage record. Refusals during an
//!   outage fold into it; it ends only when a new dispatch actually claims a
//!   host ([`DispatchOutageTracker::record_claim`]), never because a flag
//!   cleared. It does not touch the refused items: each keeps its own retry.
//! - [`health_issue_for`] renders the record as the engine-health issue the
//!   app banner shows. It is deliberately a health issue and not an attention
//!   item: it stays until the outage ends and has no dismissal.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use boss_dispatch_events::DispatchEvent;
use boss_engine_storage_values::Host;
use boss_protocol::{DispatchOutage, DispatchOutageHostExclusion, EngineHealthIssue};

/// `EngineHealthIssue::kind` of the outage banner entry.
pub const DISPATCH_OUTAGE_ISSUE_KIND: &str = "dispatch_outage";

/// `DispatchOutageHostExclusion::kind` of the local host held by the startup quarantine.
pub const EXCLUSION_LOCAL_QUARANTINE: &str = "local_quarantine";
/// `DispatchOutageHostExclusion::kind` of a non-local host that cannot take any
/// work item because a failed startup worker scan holds every one.
pub const EXCLUSION_ALL_ITEMS_HELD: &str = "all_items_held";
/// `DispatchOutageHostExclusion::kind` of a host the registry has disabled.
pub const EXCLUSION_HOST_DISABLED: &str = "host_disabled";

/// The local-dispatch hold as the coordinator reads it from the quarantine
/// record: the engine's refusal text plus what it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalHold {
    /// The engine's refusal text, verbatim.
    pub reason: String,
    /// Executions whose workers the hold could not prove dead.
    pub held_executions: Vec<String>,
    /// The error that aborted the startup scan, if one did.
    pub scan_error: Option<String>,
}

/// Per-host exclusions when this refusal is global, `None` otherwise.
///
/// `hosts` is the whole registry, before any host was filtered out for the
/// attempt. The refusal is global only when every host has an item-independent
/// exclusion; a registry with no hosts is never an outage (there is nothing to
/// recover).
pub fn classify_host_exclusions(
    hosts: &[Host],
    local_hold: Option<&LocalHold>,
) -> Option<Vec<DispatchOutageHostExclusion>> {
    if hosts.is_empty() {
        return None;
    }
    hosts
        .iter()
        .map(|host| match local_hold {
            Some(hold) if host.id == "local" => Some(local_quarantine_exclusion(hold)),
            _ if !host.enabled => Some(disabled_exclusion(host)),
            _ => None,
        })
        .collect()
}

/// Exclusions when a failed startup worker scan holds every work item. That
/// hold refuses every item on every host, so each host is listed: the local
/// host as a quarantine exclusion, any other as held for every work item.
pub fn scan_error_exclusions(hosts: &[Host], hold: &LocalHold) -> Vec<DispatchOutageHostExclusion> {
    let mut exclusions: Vec<DispatchOutageHostExclusion> = Vec::new();
    for host in hosts {
        if host.id == "local" {
            exclusions.push(local_quarantine_exclusion(hold));
        } else {
            exclusions.push(DispatchOutageHostExclusion {
                host_id: host.id.clone(),
                kind: EXCLUSION_ALL_ITEMS_HELD.to_owned(),
                cause: format!(
                    "every work item is held because the startup worker scan failed: {}",
                    hold.scan_error.as_deref().unwrap_or("unknown error")
                ),
                reason: hold.reason.clone(),
                recovery_action: Some(LOCAL_QUARANTINE_RECOVERY.to_owned()),
            });
        }
    }
    if !hosts.iter().any(|host| host.id == "local") {
        exclusions.insert(0, local_quarantine_exclusion(hold));
    }
    exclusions
}

fn local_quarantine_exclusion(hold: &LocalHold) -> DispatchOutageHostExclusion {
    let cause = match (hold.held_executions.is_empty(), hold.scan_error.as_deref()) {
        (false, _) => format!(
            "local dispatch is held by the startup quarantine of historical workers (executions: {})",
            hold.held_executions.join(", ")
        ),
        (true, Some(error)) => format!("local dispatch is held because the startup worker scan failed: {error}"),
        (true, None) => "local dispatch is held by the startup quarantine of historical workers".to_owned(),
    };
    DispatchOutageHostExclusion {
        host_id: "local".to_owned(),
        kind: EXCLUSION_LOCAL_QUARANTINE.to_owned(),
        cause,
        reason: hold.reason.clone(),
        recovery_action: Some(LOCAL_QUARANTINE_RECOVERY.to_owned()),
    }
}

/// Recovery action for the local startup-quarantine hold. The hold survives an
/// engine restart (the startup scan re-holds the same execution) and
/// cancelling the held execution does not release it, so neither is offered.
/// When a supported clear command exists, point this at it.
const LOCAL_QUARANTINE_RECOVERY: &str = "Restarting Boss does not lift this hold, and neither does cancelling the held execution. \
Before removing it, check each held execution for a live worker with `bossctl agents list --all`, which includes \
lost-track panes the default view hides; a worker missing from that list is unknown, not dead, because the hold exists \
precisely for workers the engine cannot identify. Only when you have confirmed that no held worker is still running, \
delete the `local_worker_startup_quarantine` row from the `metadata` table of the engine's `state.db`; it takes effect \
on the next dispatch, with no restart.";

fn disabled_exclusion(host: &Host) -> DispatchOutageHostExclusion {
    let cause = match host.last_error_text.as_deref().filter(|text| !text.is_empty()) {
        Some(error) => format!("host is disabled (last error: {error})"),
        None => "host is disabled".to_owned(),
    };
    DispatchOutageHostExclusion {
        host_id: host.id.clone(),
        kind: EXCLUSION_HOST_DISABLED.to_owned(),
        cause,
        reason: "disabled".to_owned(),
        recovery_action: Some(format!(
            "Check it with `bossctl hosts probe {id}`, then run `bossctl hosts enable {id}`.",
            id = host.id
        )),
    }
}

/// What recording one refusal did to the outage record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalEffect {
    /// No outage was active: this refusal started one.
    Started,
    /// An outage was already active and absorbed this refusal.
    Folded,
}

/// An outage a successful claim ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedOutage {
    /// The record as it stood when the claim ended it.
    pub outage: DispatchOutage,
    pub recovered_epoch_s: i64,
    pub duration_secs: i64,
}

/// The wire record plus the one thing it cannot hold: which items were
/// refused, so a retry of the same item is not counted as a new one.
struct Active {
    outage: DispatchOutage,
    refused_work_items: BTreeSet<String>,
}

impl Active {
    fn to_wire(&self) -> DispatchOutage {
        DispatchOutage {
            refused_work_items: self.refused_work_items.len() as u64,
            ..self.outage.clone()
        }
    }
}

/// The engine's single global-outage record.
pub struct DispatchOutageTracker {
    active: Mutex<Option<Active>>,
    /// Set by the first claim this process records. A restore of a pre-restart
    /// outage that lands after it is stale: the claim already ended it.
    claimed_since_start: AtomicBool,
    changed: tokio::sync::watch::Sender<u64>,
}

/// What recording one refusal did, and which outage it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordedRefusal {
    pub effect: RefusalEffect,
    /// Start of the outage this refusal is part of. Stamped on the refusal's
    /// own event so a replay attributes it to the right outage whatever order
    /// concurrent dispatchers wrote their events in.
    pub outage_started_epoch_s: i64,
}

impl Default for DispatchOutageTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl DispatchOutageTracker {
    pub fn new() -> Self {
        Self {
            active: Mutex::new(None),
            claimed_since_start: AtomicBool::new(false),
            changed: tokio::sync::watch::channel(0).0,
        }
    }

    /// Fold one refused launch into the outage, starting it if none is
    /// active. Every call counts toward `refused_attempts` and the latest
    /// refusal, so repeated retries of the same item are one record, not a
    /// record each. Subscribers are told only when the outage starts or its
    /// exclusions change; the counters ride along on the next push.
    pub fn record_refusal(
        &self,
        exclusions: Vec<DispatchOutageHostExclusion>,
        work_item_id: &str,
        execution_id: &str,
        now_epoch_secs: i64,
    ) -> RefusalEffect {
        self.fold_refusal(exclusions, work_item_id, execution_id, now_epoch_secs)
            .effect
    }

    /// [`Self::record_refusal`], also naming the outage the refusal joined.
    /// An empty `work_item_id` counts an attempt without naming an item.
    pub fn fold_refusal(
        &self,
        exclusions: Vec<DispatchOutageHostExclusion>,
        work_item_id: &str,
        execution_id: &str,
        now_epoch_secs: i64,
    ) -> RecordedRefusal {
        let (effect, notify, outage_started_epoch_s) = {
            let mut slot = self.active.lock().expect("dispatch outage lock poisoned");
            match slot.as_mut() {
                None => {
                    let items: BTreeSet<String> = Some(work_item_id)
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned)
                        .into_iter()
                        .collect();
                    *slot = Some(Active {
                        outage: DispatchOutage::builder()
                            .started_epoch_s(now_epoch_secs)
                            .host_exclusions(exclusions)
                            .refused_attempts(1)
                            .refused_work_items(items.len() as u64)
                            .latest_refusal_epoch_s(now_epoch_secs)
                            .latest_work_item_id(work_item_id)
                            .latest_execution_id(execution_id)
                            .build(),
                        refused_work_items: items,
                    });
                    (RefusalEffect::Started, true, now_epoch_secs)
                }
                Some(active) => {
                    let outage = &mut active.outage;
                    outage.refused_attempts = outage.refused_attempts.saturating_add(1);
                    if !work_item_id.is_empty() {
                        active.refused_work_items.insert(work_item_id.to_owned());
                    }
                    outage.latest_refusal_epoch_s = now_epoch_secs;
                    outage.latest_work_item_id = work_item_id.to_owned();
                    outage.latest_execution_id = execution_id.to_owned();
                    let exclusions_changed = outage.host_exclusions != exclusions;
                    outage.host_exclusions = exclusions;
                    (RefusalEffect::Folded, exclusions_changed, outage.started_epoch_s)
                }
            }
        };
        if notify {
            self.notify_changed();
        }
        RecordedRefusal {
            effect,
            outage_started_epoch_s,
        }
    }

    /// The subset of `work_item_ids` the active outage has not counted yet
    /// (all of them when none is active). A periodic check folds only these,
    /// so re-checking the same held items does not inflate the attempt count.
    pub fn unrefused_work_items(&self, work_item_ids: &[String]) -> Vec<String> {
        let slot = self.active.lock().expect("dispatch outage lock poisoned");
        work_item_ids
            .iter()
            .filter(|id| {
                slot.as_ref()
                    .is_none_or(|active| !active.refused_work_items.contains(id.as_str()))
            })
            .cloned()
            .collect()
    }

    /// Reinstate an outage rebuilt from the dispatch event stream after an
    /// engine restart, so its original start time and counters survive until
    /// a new dispatch actually claims a host. A record already active (a
    /// refusal that raced the restore) is merged into rather than replaced:
    /// it keeps its own latest refusal and exclusions, takes the earlier
    /// start, and adds the restored attempts and items. After a claim the
    /// restore is dropped: a claim this process already recorded ended the
    /// outage being restored.
    pub fn restore(&self, outage: DispatchOutage, refused_work_items: BTreeSet<String>) {
        {
            let mut slot = self.active.lock().expect("dispatch outage lock poisoned");
            if self.claimed_since_start.load(Ordering::SeqCst) {
                return;
            }
            match slot.as_mut() {
                None => {
                    *slot = Some(Active {
                        outage,
                        refused_work_items,
                    });
                }
                Some(live) => {
                    live.outage.started_epoch_s = live.outage.started_epoch_s.min(outage.started_epoch_s);
                    live.outage.refused_attempts = live.outage.refused_attempts.saturating_add(outage.refused_attempts);
                    live.refused_work_items.extend(refused_work_items);
                }
            }
        }
        self.notify_changed();
    }

    /// [`Self::restore`] from the dispatch event stream: replays `events` and
    /// reinstates the outage they leave in progress, if any. Returns the
    /// restored start time. The engine's startup restore and its tests both
    /// go through here.
    pub fn restore_from_events(&self, events: &[DispatchEvent]) -> Option<i64> {
        let replay = replay_outage_events(events);
        let active = replay.active?;
        let started = active.started_epoch_s;
        self.restore(active, replay.refused_work_items);
        Some(started)
    }

    /// A new dispatch claimed a host: the outage, if one was active, is over.
    /// This is the only way it ends.
    pub fn record_claim(&self, now_epoch_secs: i64) -> Option<ResolvedOutage> {
        let mut slot = self.active.lock().expect("dispatch outage lock poisoned");
        // Under the lock, so a restore cannot slip between the claim and the flag.
        self.claimed_since_start.store(true, Ordering::SeqCst);
        let ended = slot.take()?;
        drop(slot);
        self.notify_changed();
        Some(ResolvedOutage {
            outage: ended.to_wire(),
            recovered_epoch_s: now_epoch_secs,
            duration_secs: now_epoch_secs.saturating_sub(ended.outage.started_epoch_s).max(0),
        })
    }

    pub fn active(&self) -> Option<DispatchOutage> {
        self.active
            .lock()
            .expect("dispatch outage lock poisoned")
            .as_ref()
            .map(Active::to_wire)
    }

    /// Subscribe to outage changes. The receiver starts already-seen, so
    /// `changed()` first resolves on the next start, end or exclusion change.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn notify_changed(&self) {
        self.changed
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

/// Render the active outage as the engine-health issue the app banner and
/// `bossctl` show. The engine owns the wording; the app shows it verbatim.
///
/// The title carries the duration, passed in as `started_phrase` (a
/// relative phrase such as "13 minutes ago", rendered by the caller with the
/// shared relative-time helper); the body names each host's cause and the
/// recovery action for it, and says what ends the outage.
pub fn health_issue_for(outage: &DispatchOutage, started_phrase: &str) -> EngineHealthIssue {
    let started = if started_phrase.is_empty() {
        "just now"
    } else {
        started_phrase
    };
    let causes = outage
        .host_exclusions
        .iter()
        .map(|exclusion| format!("• {}: {}", exclusion.host_id, exclusion.cause))
        .collect::<Vec<_>>()
        .join("\n");
    let recoveries = outage
        .host_exclusions
        .iter()
        .filter_map(|exclusion| {
            exclusion
                .recovery_action
                .as_ref()
                .map(|action| format!("• {}: {action}", exclusion.host_id))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let recovery_section = if recoveries.is_empty() {
        String::new()
    } else {
        format!("\n\nRecovery:\n{recoveries}")
    };
    EngineHealthIssue {
        kind: DISPATCH_OUTAGE_ISSUE_KIND.to_owned(),
        severity: "error".to_owned(),
        title: format!("Dispatch is down: no host can start new workers (started {started})"),
        body: format!(
            "Every host is excluded, so every new launch is refused. {attempts} refused attempts across {items} work \
             items so far; each item keeps retrying on its own and nothing is reset when this ends. Workers that are \
             already running are not affected.\n\nCause:\n{causes}{recovery_section}\n\nThis stays until a new launch \
             actually claims a host. `bossctl dispatch state` shows the same record.",
            attempts = outage.refused_attempts,
            items = outage.refused_work_items,
        ),
    }
}

const STAGE_STARTED: &str = "dispatch_outage_started";
const STAGE_RECOVERED: &str = "dispatch_outage_recovered";

/// What the dispatch event stream says about outages.
#[derive(Debug, Default)]
pub struct OutageReplay {
    /// The outage still in progress: a start with no claim after it.
    pub active: Option<DispatchOutage>,
    /// The work items behind `active.refused_work_items`, so a restored
    /// tracker keeps deduplicating retries of the same item.
    pub refused_work_items: BTreeSet<String>,
    /// Details of the most recent recovery.
    pub last_recovery: Option<serde_json::Value>,
}

fn event_epoch_s(event: &DispatchEvent) -> i64 {
    i64::try_from(event.ts_epoch_ms / 1000).unwrap_or(i64::MAX)
}

/// Rebuild outage state from dispatch `events` (any order), by the same rules
/// the live [`DispatchOutageTracker`] applies. An outage runs from its first
/// global refusal until a new dispatch claims a host: either the
/// `dispatch_outage_recovered` event or a `host_selected` success that was a
/// fresh selection (an engine restarted mid-outage has no tracker to emit the
/// former). Refusals count only when flagged `global_outage`, and carry the
/// host exclusions as of that refusal.
///
/// Concurrent dispatchers can each write their refusal before either writes
/// the `dispatch_outage_started` event, so the start event does not open the
/// outage on its own and never counts an attempt: every refusal stamps the
/// `outage_started_epoch_s` of the tracker record it joined, and a refusal or a
/// start event with the active outage's start time belongs to it, whichever
/// came first. A different start time is a new outage.
///
/// `bossctl dispatch diagnose` reads this so it answers with the engine down;
/// the engine replays it at startup to restore an outage that was in progress.
pub fn replay_outage_events(events: &[DispatchEvent]) -> OutageReplay {
    let mut ordered: Vec<&DispatchEvent> = events.iter().collect();
    ordered.sort_by_key(|event| event.ts_epoch_ms);
    let mut out = OutageReplay::default();
    // Start times of outages a claim already ended. A refusal or start event
    // can be stamped after the claim that closed its outage (the tracker
    // updates before the event timestamp is taken), so it must not reopen it.
    let mut closed: BTreeSet<i64> = BTreeSet::new();
    for event in ordered {
        match event.stage.as_str() {
            STAGE_STARTED => {
                let started = event
                    .details
                    .get("started_epoch_s")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or_else(|| event_epoch_s(event));
                if closed.contains(&started) {
                    continue;
                }
                let exclusions = event_exclusions(&event.details);
                match out.active.as_mut() {
                    Some(active) if active.started_epoch_s == started => {
                        // The refusals that raised it were replayed already.
                        if active.host_exclusions.is_empty()
                            && let Some(exclusions) = exclusions
                        {
                            active.host_exclusions = exclusions;
                        }
                    }
                    _ => open_outage(&mut out, started, exclusions.unwrap_or_default(), event_epoch_s(event)),
                }
            }
            STAGE_RECOVERED => {
                closed.extend(out.active.as_ref().map(|active| active.started_epoch_s));
                closed.extend(event.details.get("started_epoch_s").and_then(serde_json::Value::as_i64));
                out.active = None;
                out.refused_work_items.clear();
                out.last_recovery = Some(event.details.clone());
            }
            "host_selected" => {
                let details = &event.details;
                if event.outcome == "error"
                    && details.get("global_outage").and_then(serde_json::Value::as_bool) == Some(true)
                {
                    let refused_at = event_epoch_s(event);
                    let started = details
                        .get("outage_started_epoch_s")
                        .and_then(serde_json::Value::as_i64);
                    if started.is_some_and(|started| closed.contains(&started)) {
                        continue;
                    }
                    let exclusions = event_exclusions(details);
                    // A refusal that names no outage joins the active one.
                    let joins_active = out
                        .active
                        .as_ref()
                        .is_some_and(|active| started.is_none_or(|started| active.started_epoch_s == started));
                    if !joins_active {
                        open_outage(
                            &mut out,
                            started.unwrap_or(refused_at),
                            exclusions.clone().unwrap_or_default(),
                            refused_at,
                        );
                    }
                    let Some(active) = out.active.as_mut() else { continue };
                    active.refused_attempts += 1;
                    if let Some(item) = event.work_item_id.as_deref().filter(|item| !item.is_empty()) {
                        out.refused_work_items.insert(item.to_owned());
                        active.latest_work_item_id = item.to_owned();
                    }
                    active.refused_work_items = out.refused_work_items.len() as u64;
                    active.latest_refusal_epoch_s = refused_at;
                    // Refusals before any execution exists are recorded under "engine".
                    active.latest_execution_id = Some(event.execution_id.as_str())
                        .filter(|id| *id != "engine")
                        .unwrap_or_default()
                        .to_owned();
                    if let Some(exclusions) = exclusions {
                        active.host_exclusions = exclusions;
                    }
                } else if event.outcome == "ok"
                    && details.get("existing_assignment").and_then(serde_json::Value::as_bool) != Some(true)
                    && let Some(active) = out.active.as_ref()
                {
                    let recovered = event_epoch_s(event);
                    closed.insert(active.started_epoch_s);
                    out.last_recovery = Some(serde_json::json!({
                        "started_epoch_s": active.started_epoch_s,
                        "recovered_epoch_s": recovered,
                        "duration_secs": recovered.saturating_sub(active.started_epoch_s).max(0),
                        "refused_attempts": active.refused_attempts,
                        "refused_work_items": active.refused_work_items,
                        "claimed_by_execution_id": event.execution_id,
                        "claimed_host_id": details.get("host_id"),
                    }));
                    out.active = None;
                    out.refused_work_items.clear();
                }
            }
            _ => {}
        }
    }
    out
}

fn event_exclusions(details: &serde_json::Value) -> Option<Vec<DispatchOutageHostExclusion>> {
    details
        .get("host_exclusions")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

/// Begin a replayed outage with no attempts: the refusals that belong to it
/// are counted as they are replayed, never by the event that opens it.
fn open_outage(out: &mut OutageReplay, started: i64, exclusions: Vec<DispatchOutageHostExclusion>, now: i64) {
    out.refused_work_items.clear();
    out.active = Some(
        DispatchOutage::builder()
            .started_epoch_s(started)
            .host_exclusions(exclusions)
            .refused_attempts(0)
            .refused_work_items(0)
            .latest_refusal_epoch_s(now)
            .latest_work_item_id("")
            .latest_execution_id("")
            .build(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: &str, enabled: bool) -> Host {
        Host {
            id: id.to_owned(),
            ssh_target: None,
            pool_size: 1,
            enabled,
            last_seen_at: None,
            last_error_text: None,
            consecutive_failures: 0,
            created_at: String::new(),
        }
    }

    fn hold() -> LocalHold {
        LocalHold {
            reason: "local dispatch quarantined: historical workers lack tmux identity; roll back or drain them and restart (executions: exec_1; scan error: none)".to_owned(),
            held_executions: vec!["exec_1".to_owned()],
            scan_error: None,
        }
    }

    fn exclusions() -> Vec<DispatchOutageHostExclusion> {
        classify_host_exclusions(&[host("local", true), host("anaplian", false)], Some(&hold())).unwrap()
    }

    #[test]
    fn every_host_excluded_for_a_host_level_reason_is_global() {
        let exclusions = exclusions();
        assert_eq!(exclusions.len(), 2);
        assert_eq!(exclusions[0].kind, EXCLUSION_LOCAL_QUARANTINE);
        assert_eq!(
            exclusions[0].reason,
            hold().reason,
            "the engine's text is kept verbatim"
        );
        assert!(exclusions[0].cause.contains("exec_1"));
        assert_eq!(exclusions[1].kind, EXCLUSION_HOST_DISABLED);
        assert_eq!(exclusions[1].host_id, "anaplian");
        assert!(
            exclusions[1]
                .recovery_action
                .as_deref()
                .unwrap()
                .contains("bossctl hosts enable anaplian"),
            "{:?}",
            exclusions[1]
        );
    }

    #[test]
    fn one_usable_host_means_the_refusal_is_not_global() {
        // Local is held but the remote is enabled: this item failed on its own
        // merits (a capability, a pin, a full pool), not because dispatch is down.
        assert_eq!(
            classify_host_exclusions(&[host("local", true), host("remote", true)], Some(&hold())),
            None
        );
        // Enabled local with no hold, remote disabled: same.
        assert_eq!(
            classify_host_exclusions(&[host("local", true), host("remote", false)], None),
            None
        );
        assert_eq!(
            classify_host_exclusions(&[], None),
            None,
            "no hosts is not an outage to recover from"
        );
    }

    #[test]
    fn the_quarantine_recovery_does_not_repeat_advice_known_not_to_work() {
        let recovery = exclusions()[0].recovery_action.clone().unwrap();
        assert!(recovery.contains("does not lift this hold"), "{recovery}");
        assert!(!recovery.contains("roll back or drain"), "{recovery}");
        let issue = health_issue_for(&tracker_with_one_refusal().active().unwrap(), "just now");
        assert!(!issue.body.contains("roll back or drain"), "{}", issue.body);
    }

    fn tracker_with_one_refusal() -> DispatchOutageTracker {
        let tracker = DispatchOutageTracker::new();
        assert_eq!(
            tracker.record_refusal(exclusions(), "task_1", "exec_1", 1_000),
            RefusalEffect::Started
        );
        tracker
    }

    #[test]
    fn repeated_refusals_fold_into_one_record() {
        let tracker = tracker_with_one_refusal();
        let changes = tracker.subscribe();
        for n in 0..9 {
            // The same two items retrying over and over.
            let item = if n % 2 == 0 { "task_1" } else { "task_2" };
            assert_eq!(
                tracker.record_refusal(exclusions(), item, &format!("exec_{item}"), 1_010 + n),
                RefusalEffect::Folded
            );
        }
        let outage = tracker.active().unwrap();
        assert_eq!(outage.started_epoch_s, 1_000, "the start never moves");
        assert_eq!(outage.refused_attempts, 10);
        assert_eq!(outage.refused_work_items, 2);
        assert_eq!(outage.latest_refusal_epoch_s, 1_018);
        assert_eq!(outage.latest_work_item_id, "task_1");
        assert!(
            !changes.has_changed().unwrap(),
            "folded refusals must not push a health snapshot each"
        );
    }

    #[test]
    fn a_change_in_exclusions_is_pushed_but_stays_one_outage() {
        let tracker = tracker_with_one_refusal();
        let changes = tracker.subscribe();
        let only_disabled = vec![exclusions().remove(1)];
        tracker.record_refusal(only_disabled.clone(), "task_1", "exec_1", 1_020);
        assert!(changes.has_changed().unwrap());
        let outage = tracker.active().unwrap();
        assert_eq!(outage.host_exclusions, only_disabled);
        assert_eq!(outage.started_epoch_s, 1_000);
    }

    #[test]
    fn the_first_claim_ends_the_outage_and_reports_how_long_it_lasted() {
        let tracker = tracker_with_one_refusal();
        tracker.record_refusal(exclusions(), "task_2", "exec_2", 1_300);
        let resolved = tracker.record_claim(1_780).expect("a claim ends the active outage");
        assert_eq!(resolved.duration_secs, 780);
        assert_eq!(resolved.recovered_epoch_s, 1_780);
        assert_eq!(resolved.outage.refused_attempts, 2);
        assert_eq!(tracker.active(), None);
        assert_eq!(
            tracker.record_claim(1_800),
            None,
            "a claim with no outage is nothing to report"
        );

        // A later outage is a new record, not a continuation.
        tracker.record_refusal(exclusions(), "task_3", "exec_3", 2_000);
        let again = tracker.active().unwrap();
        assert_eq!(again.started_epoch_s, 2_000);
        assert_eq!(again.refused_attempts, 1);
    }

    #[test]
    fn banner_text_names_cause_duration_and_recovery() {
        let issue = health_issue_for(&tracker_with_one_refusal().active().unwrap(), "13 minutes ago");
        assert_eq!(issue.kind, DISPATCH_OUTAGE_ISSUE_KIND);
        assert_eq!(issue.severity, "error");
        assert!(issue.title.contains("13 minutes ago"), "{}", issue.title);
        assert!(
            issue
                .body
                .contains("local: local dispatch is held by the startup quarantine")
        );
        assert!(issue.body.contains("anaplian: host is disabled"));
        assert!(issue.body.contains("Recovery:"));
        assert!(issue.body.contains("bossctl hosts enable anaplian"));
        assert!(issue.body.contains("local_worker_startup_quarantine"));
        assert!(issue.body.contains("claims a host"));
    }

    #[test]
    fn recovery_text_names_the_check_that_covers_lost_track_workers() {
        let recovery = exclusions()[0].recovery_action.clone().unwrap();
        assert!(recovery.contains("bossctl agents list --all"), "{recovery}");
        assert!(recovery.contains("unknown, not dead"), "{recovery}");
    }

    #[test]
    fn a_scan_error_hold_excludes_every_host_even_an_enabled_remote() {
        let hold = LocalHold {
            reason: "scan failed".to_owned(),
            held_executions: Vec::new(),
            scan_error: Some("tmux unreachable".to_owned()),
        };
        let exclusions = scan_error_exclusions(&[host("local", true), host("remote", true)], &hold);
        assert_eq!(exclusions.len(), 2);
        assert_eq!(exclusions[0].kind, EXCLUSION_LOCAL_QUARANTINE);
        assert!(
            exclusions[0].cause.contains("tmux unreachable"),
            "{}",
            exclusions[0].cause
        );
        assert_eq!(exclusions[1].kind, EXCLUSION_ALL_ITEMS_HELD);
        assert!(exclusions[1].cause.contains("tmux unreachable"));
    }

    #[test]
    fn a_restored_outage_keeps_its_start_and_ends_on_the_first_claim() {
        let original = tracker_with_one_refusal();
        original.record_refusal(exclusions(), "task_2", "exec_2", 1_010);
        let tracker = DispatchOutageTracker::new();
        tracker.restore(
            original.active().unwrap(),
            BTreeSet::from(["task_1".to_owned(), "task_2".to_owned()]),
        );
        tracker.record_refusal(exclusions(), "task_2", "exec_2", 1_020);
        let outage = tracker.active().unwrap();
        assert_eq!(outage.started_epoch_s, 1_000);
        assert_eq!(outage.refused_attempts, 3);
        assert_eq!(
            outage.refused_work_items, 2,
            "a retry of a known item is not a new item"
        );
        assert_eq!(tracker.record_claim(1_100).unwrap().duration_secs, 100);
    }

    #[test]
    fn a_restore_after_a_claim_is_dropped_as_stale() {
        let original = tracker_with_one_refusal();
        let tracker = DispatchOutageTracker::new();
        assert_eq!(tracker.record_claim(1_005), None, "nothing to end yet");
        tracker.restore(original.active().unwrap(), BTreeSet::from(["task_1".to_owned()]));
        assert_eq!(
            tracker.active(),
            None,
            "a claim already ended the outage being restored"
        );
    }

    fn stream_event(
        stage: &str,
        outcome: &str,
        execution: &str,
        item: &str,
        ts_ms: u128,
        details: serde_json::Value,
    ) -> DispatchEvent {
        let mut event = DispatchEvent::new(
            serde_json::from_value(serde_json::json!(stage)).unwrap(),
            serde_json::from_value(serde_json::json!(outcome)).unwrap(),
            execution,
        )
        .with_work_item(item)
        .with_details(details);
        event.ts_epoch_ms = ts_ms;
        event
    }

    fn started_event(ts_ms: u128) -> DispatchEvent {
        stream_event(
            "dispatch_outage_started",
            "error",
            "engine",
            "task_1",
            ts_ms,
            serde_json::json!({
                "started_epoch_s": ts_ms / 1000,
                "triggering_execution_id": "exec_1",
                "host_exclusions": exclusions(),
            }),
        )
    }

    fn refusal_event(
        item: &str,
        ts_ms: u128,
        global: bool,
        exclusions: Vec<DispatchOutageHostExclusion>,
    ) -> DispatchEvent {
        stream_event(
            "host_selected",
            "error",
            &format!("exec_{item}"),
            item,
            ts_ms,
            serde_json::json!({
                "reason": "no_eligible_host",
                "global_outage": global,
                "outage_started_epoch_s": 1_000,
                "host_exclusions": exclusions,
            }),
        )
    }

    #[test]
    fn replay_counts_only_global_refusals_and_follows_exclusion_changes() {
        let narrowed = vec![exclusions().remove(1)];
        let replay = replay_outage_events(&[
            refusal_event("task_1", 1_000_000, true, exclusions()),
            started_event(1_000_000),
            refusal_event("task_2", 1_100_000, true, exclusions()),
            refusal_event("task_3", 1_200_000, false, Vec::new()),
            refusal_event("task_2", 1_300_000, true, narrowed.clone()),
        ]);
        let active = replay.active.expect("still active");
        assert_eq!(active.started_epoch_s, 1_000);
        assert_eq!(
            active.refused_attempts, 3,
            "three global refusals; the local one is not counted"
        );
        assert_eq!(active.refused_work_items, 2);
        assert_eq!(active.host_exclusions, narrowed);
        assert_eq!(replay.refused_work_items.len(), 2);
    }

    #[test]
    fn replay_matches_the_live_tracker_when_refusals_precede_the_start_event() {
        // Two dispatchers each wrote their refusal before either wrote the
        // start event; the tracker (which saw both) counts 2 attempts, 2 items.
        let live = DispatchOutageTracker::new();
        let first = live.fold_refusal(exclusions(), "task_1", "exec_task_1", 1_000);
        let second = live.fold_refusal(exclusions(), "task_2", "exec_task_2", 1_000);
        assert_eq!(first.effect, RefusalEffect::Started);
        assert_eq!(second.effect, RefusalEffect::Folded);
        assert_eq!(second.outage_started_epoch_s, 1_000);
        let live = live.active().unwrap();

        for events in [
            // Both refusals, then the start event.
            vec![
                refusal_event("task_1", 1_000_000, true, exclusions()),
                refusal_event("task_2", 1_000_000, true, exclusions()),
                started_event(1_000_000),
            ],
            // A refusal landing after the start event.
            vec![
                refusal_event("task_1", 1_000_000, true, exclusions()),
                started_event(1_000_000),
                refusal_event("task_2", 1_000_001, true, exclusions()),
            ],
        ] {
            let replayed = replay_outage_events(&events).active.expect("active");
            assert_eq!(replayed.refused_attempts, live.refused_attempts);
            assert_eq!(replayed.refused_work_items, live.refused_work_items);
            assert_eq!(replayed.started_epoch_s, live.started_epoch_s);
        }
    }

    #[test]
    fn replay_treats_a_different_start_time_as_a_new_outage() {
        let mut later_start = started_event(2_000_000);
        later_start.details["started_epoch_s"] = serde_json::json!(2_000);
        let replay = replay_outage_events(&[
            refusal_event("task_1", 1_000_000, true, exclusions()),
            started_event(1_000_000),
            later_start,
        ]);
        let active = replay.active.unwrap();
        assert_eq!(active.started_epoch_s, 2_000);
        assert_eq!(
            active.refused_attempts, 0,
            "the earlier outage's attempts are not carried over"
        );
    }

    #[test]
    fn replay_ends_the_outage_on_a_fresh_host_claim_without_a_recovered_event() {
        let claim = |existing: bool, ts| {
            stream_event(
                "host_selected",
                "ok",
                "exec_9",
                "task_9",
                ts,
                serde_json::json!({ "host_id": "anaplian", "existing_assignment": existing }),
            )
        };
        let still_active = replay_outage_events(&[started_event(1_000_000), claim(true, 1_100_000)]);
        assert!(
            still_active.active.is_some(),
            "a held-spoke reconciliation is not a claim"
        );
        let ended = replay_outage_events(&[started_event(1_000_000), claim(false, 1_100_000)]);
        assert!(ended.active.is_none());
        assert_eq!(ended.last_recovery.unwrap()["duration_secs"], 100);
    }
    fn fresh_claim(ts: u128) -> DispatchEvent {
        stream_event(
            "host_selected",
            "ok",
            "exec_9",
            "task_9",
            ts,
            serde_json::json!({ "host_id": "anaplian", "existing_assignment": false }),
        )
    }

    #[test]
    fn replay_does_not_reopen_an_outage_for_a_refusal_stamped_after_its_claim() {
        // The tracker is updated before the event's timestamp is taken, so a
        // refusal that joined outage 1_000 can sort after the claim that ended it.
        let replay = replay_outage_events(&[
            refusal_event("task_1", 1_000_000, true, exclusions()),
            started_event(1_000_000),
            fresh_claim(1_100_000),
            refusal_event("task_2", 1_100_001, true, exclusions()),
        ]);
        assert!(replay.active.is_none(), "{:?}", replay.active);
        assert_eq!(replay.last_recovery.unwrap()["refused_attempts"], 1);
    }

    #[test]
    fn replay_does_not_reopen_an_outage_for_a_start_event_stamped_after_its_claim() {
        let mut late_start = started_event(1_100_001);
        late_start.details["started_epoch_s"] = serde_json::json!(1_000);
        let replay = replay_outage_events(&[
            refusal_event("task_1", 1_000_000, true, exclusions()),
            fresh_claim(1_100_000),
            late_start,
        ]);
        assert!(replay.active.is_none(), "{:?}", replay.active);
    }

    #[test]
    fn replay_does_not_reopen_an_outage_a_recovered_event_ended() {
        let recovered = stream_event(
            "dispatch_outage_recovered",
            "ok",
            "engine",
            "task_9",
            1_100_000,
            serde_json::json!({ "started_epoch_s": 1_000 }),
        );
        let replay = replay_outage_events(&[
            refusal_event("task_1", 1_000_000, true, exclusions()),
            recovered,
            refusal_event("task_2", 1_100_001, true, exclusions()),
        ]);
        assert!(replay.active.is_none(), "{:?}", replay.active);
        // A genuinely new outage after the claim is still reported.
        let mut later = refusal_event("task_3", 1_200_000, true, exclusions());
        later.details["outage_started_epoch_s"] = serde_json::json!(1_200);
        let replay = replay_outage_events(&[
            refusal_event("task_1", 1_000_000, true, exclusions()),
            fresh_claim(1_100_000),
            later,
        ]);
        assert_eq!(replay.active.unwrap().started_epoch_s, 1_200);
    }

    #[test]
    fn unrefused_work_items_skips_what_the_active_outage_already_counted() {
        let tracker = DispatchOutageTracker::new();
        let items = vec!["task_1".to_owned(), "task_2".to_owned()];
        assert_eq!(tracker.unrefused_work_items(&items), items, "no outage: all are new");
        tracker.record_refusal(exclusions(), "task_1", "", 1_000);
        assert_eq!(tracker.unrefused_work_items(&items), vec!["task_2".to_owned()]);
    }

    #[test]
    fn a_restore_landing_after_a_live_refusal_merges_instead_of_being_dropped() {
        let tracker = DispatchOutageTracker::new();
        tracker.record_refusal(exclusions(), "task_new", "exec_new", 2_000);
        let restored = DispatchOutage::builder()
            .started_epoch_s(1_000)
            .host_exclusions(Vec::new())
            .refused_attempts(5)
            .refused_work_items(2)
            .latest_refusal_epoch_s(1_500)
            .latest_work_item_id("task_old")
            .latest_execution_id("exec_old")
            .build();
        let items: BTreeSet<String> = ["task_old", "task_new"].map(str::to_owned).into();
        tracker.restore(restored, items);
        let merged = tracker.active().unwrap();
        assert_eq!(merged.started_epoch_s, 1_000, "the earlier start wins");
        assert_eq!(merged.refused_attempts, 6);
        assert_eq!(merged.refused_work_items, 2, "an item in both is one item");
        assert_eq!(merged.latest_work_item_id, "task_new", "the live refusal stays latest");
        assert_eq!(merged.host_exclusions, exclusions());
        // A claim already recorded still wins over a late restore.
        let claimed = DispatchOutageTracker::new();
        claimed.record_claim(10);
        claimed.restore(merged, BTreeSet::new());
        assert_eq!(claimed.active(), None);
    }
}
