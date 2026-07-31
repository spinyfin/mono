//! `bossctl pause`/`resume`/`state` and the per-system `dispatch
//! pause`/`automation pause` verbs they wrap — dispatch-pause and
//! automation-pause state, including the operator-supplied reason a pause
//! must carry (see [`boss_protocol::PauseReason`] at the engine layer).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use boss_engine::dispatch_events::DispatchEvent;
use boss_engine::dispatch_reader;
use boss_protocol::{FrontendEvent, FrontendRequest};

use super::{PauseArg, PauseSystem, connect, resolve_state_root};

/// Require an operator-supplied `--reason` before `bossctl pause`
/// actually pauses anything. `Command::Pause::reason` stays
/// `Option<String>` in the clap struct (so `bossctl pause state`, which
/// only prints status, doesn't need one), but every path that pauses a
/// system genuinely requires a reason — this is where that requirement
/// is enforced, with a clear non-zero-exit error naming the missing
/// flag rather than a fabricated stand-in. `dispatch pause` and
/// `automation pause` enforce the same requirement declaratively via
/// clap instead, since their `--reason` is unconditionally required.
pub(super) fn require_pause_reason(reason: Option<String>) -> Result<String> {
    reason.ok_or_else(|| anyhow::anyhow!("--reason is required to pause (why are these systems being paused?)"))
}

/// Current dispatch-pause state as returned by
/// [`FrontendRequest::SetDispatchPaused`] / [`FrontendRequest::GetDispatchState`].
pub(super) struct DispatchPauseState {
    pub(super) paused: bool,
    pub(super) paused_since_epoch_s: Option<u64>,
    pub(super) reviews_exempt: bool,
    pub(super) reason: Option<String>,
}

/// Current automation-pause state as returned by
/// [`FrontendRequest::SetAutomationPaused`] / [`FrontendRequest::GetAutomationState`].
pub(super) struct AutomationPauseState {
    pub(super) paused: bool,
    pub(super) paused_since_epoch_s: Option<u64>,
    pub(super) reason: Option<String>,
}

/// `reason` is `Some` to pause (with that reason) and `None` to resume —
/// mirrors the required-only-when-pausing contract on the wire request.
async fn set_dispatch_paused_raw(socket_path: &Option<String>, reason: Option<String>) -> Result<DispatchPauseState> {
    let paused = reason.is_some();
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::SetDispatchPaused { paused, reason })
        .await
        .context("sending SetDispatchPaused")?;
    match response {
        FrontendEvent::DispatchStateResult {
            paused,
            paused_since_epoch_s,
            reviews_exempt,
            reason,
        } => Ok(DispatchPauseState {
            paused,
            paused_since_epoch_s,
            reviews_exempt,
            reason,
        }),
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected SetDispatchPaused: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn get_dispatch_state_raw(socket_path: &Option<String>) -> Result<DispatchPauseState> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::GetDispatchState)
        .await
        .context("sending GetDispatchState")?;
    match response {
        FrontendEvent::DispatchStateResult {
            paused,
            paused_since_epoch_s,
            reviews_exempt,
            reason,
        } => Ok(DispatchPauseState {
            paused,
            paused_since_epoch_s,
            reviews_exempt,
            reason,
        }),
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected GetDispatchState: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// `reason` is `Some` to pause (with that reason) and `None` to resume.
async fn set_automation_paused_raw(
    socket_path: &Option<String>,
    reason: Option<String>,
) -> Result<AutomationPauseState> {
    let paused = reason.is_some();
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::SetAutomationPaused { paused, reason })
        .await
        .context("sending SetAutomationPaused")?;
    match response {
        FrontendEvent::AutomationStateResult {
            paused,
            paused_since_epoch_s,
            reason,
        } => Ok(AutomationPauseState {
            paused,
            paused_since_epoch_s,
            reason,
        }),
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected SetAutomationPaused: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn get_automation_state_raw(socket_path: &Option<String>) -> Result<AutomationPauseState> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::GetAutomationState)
        .await
        .context("sending GetAutomationState")?;
    match response {
        FrontendEvent::AutomationStateResult {
            paused,
            paused_since_epoch_s,
            reason,
        } => Ok(AutomationPauseState {
            paused,
            paused_since_epoch_s,
            reason,
        }),
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected GetAutomationState: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// One-line summary printed by `dispatch pause`/`dispatch resume` and
/// reused by the unified `bossctl pause`/`bossctl resume`.
pub(super) fn format_dispatch_set_line(state: &DispatchPauseState) -> String {
    if state.paused {
        let since_str = state
            .paused_since_epoch_s
            .map(|s| format!(" (since epoch {s})"))
            .unwrap_or_default();
        let exempt_str = if state.reviews_exempt {
            " — PR reviews are exempt and keep dispatching"
        } else {
            " — PR reviews are held too (spawn-capability breaker)"
        };
        let reason_str = state
            .reason
            .as_deref()
            .map(|r| format!(" — reason: {r}"))
            .unwrap_or_default();
        format!("dispatch paused{since_str}{exempt_str}{reason_str}")
    } else {
        "dispatch resumed".to_string()
    }
}

/// One-line summary printed by `automation pause`/`automation resume` and
/// reused by the unified `bossctl pause`/`bossctl resume`.
pub(super) fn format_automation_set_line(state: &AutomationPauseState) -> String {
    if state.paused {
        let since_str = state
            .paused_since_epoch_s
            .map(|s| format!(" (since epoch {s})"))
            .unwrap_or_default();
        let reason_str = state
            .reason
            .as_deref()
            .map(|r| format!(" — reason: {r}"))
            .unwrap_or_default();
        format!(
            "automation paused{since_str}{reason_str} — new triage passes and automation-pool spawns \
             are held; already-running automation workers finish normally"
        )
    } else {
        "automation resumed".to_string()
    }
}

pub(super) async fn dispatch_set_paused(
    socket_path: &Option<String>,
    json: bool,
    reason: Option<String>,
) -> Result<()> {
    let state = set_dispatch_paused_raw(socket_path, reason).await?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "paused": state.paused,
                "paused_since_epoch_s": state.paused_since_epoch_s,
                "reviews_exempt": state.reviews_exempt,
                "reason": state.reason,
            })
        );
    } else {
        println!("{}", format_dispatch_set_line(&state));
    }
    Ok(())
}

pub(super) async fn dispatch_state(socket_path: &Option<String>, json: bool) -> Result<()> {
    let state = get_dispatch_state_raw(socket_path).await?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "paused": state.paused,
                "paused_since_epoch_s": state.paused_since_epoch_s,
                "reviews_exempt": state.reviews_exempt,
                "reason": state.reason,
            })
        );
    } else if state.paused {
        let since_str = state
            .paused_since_epoch_s
            .map(|s| format!("  paused_since: epoch {s}"))
            .unwrap_or_default();
        println!("state: paused");
        if !since_str.is_empty() {
            println!("{since_str}");
        }
        if let Some(reason) = &state.reason {
            println!("  reason: {reason}");
        }
        if state.reviews_exempt {
            println!("  reviews: exempt — PR-review executions keep dispatching");
        } else {
            println!("  reviews: held — spawn-capability breaker pause");
        }
    } else {
        println!("state: running");
    }
    Ok(())
}

pub(super) async fn automation_set_paused(
    socket_path: &Option<String>,
    json: bool,
    reason: Option<String>,
) -> Result<()> {
    let state = set_automation_paused_raw(socket_path, reason).await?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "paused": state.paused,
                "paused_since_epoch_s": state.paused_since_epoch_s,
                "reason": state.reason,
            })
        );
    } else {
        println!("{}", format_automation_set_line(&state));
    }
    Ok(())
}

pub(super) async fn automation_state(socket_path: &Option<String>, json: bool) -> Result<()> {
    let state = get_automation_state_raw(socket_path).await?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "paused": state.paused,
                "paused_since_epoch_s": state.paused_since_epoch_s,
                "reason": state.reason,
            })
        );
    } else if state.paused {
        let since_str = state
            .paused_since_epoch_s
            .map(|s| format!("  paused_since: epoch {s}"))
            .unwrap_or_default();
        println!("state: paused");
        if !since_str.is_empty() {
            println!("{since_str}");
        }
        if let Some(reason) = &state.reason {
            println!("  reason: {reason}");
        }
    } else {
        println!("state: running");
    }
    Ok(())
}

/// Resolve `bossctl pause`'s positional [`PauseArg`]s to the
/// [`PauseSystem`]s to act on — empty input means "every system"
/// ([`PauseSystem::all`]). Callers must handle [`PauseArg::State`]
/// (the `bossctl state` alias) before reaching here; it maps to no
/// system.
pub(super) fn pause_arg_targets(systems: &[PauseArg]) -> Vec<PauseSystem> {
    if systems.is_empty() {
        PauseSystem::all()
    } else {
        systems.iter().filter_map(|s| s.as_system()).collect()
    }
}

/// Pause or resume every system in `targets`, calling the same
/// per-system RPCs `dispatch pause`/`automation pause` use, then print
/// one line (or one JSON object) per system touched. Backs both
/// `bossctl pause`/`bossctl resume` and their `[SYSTEMS]`-restricted
/// forms.
///
/// `reason` is `Some` to pause (with that single reason applied
/// identically to every system this call touches — one reason per
/// invocation, not per system; see `Command::Pause`) and `None` to
/// resume.
pub(super) async fn set_paused_for_systems(
    socket_path: &Option<String>,
    json: bool,
    targets: &[PauseSystem],
    reason: Option<String>,
) -> Result<()> {
    let mut dispatch_result = None;
    let mut automation_result = None;
    for system in targets {
        match system {
            PauseSystem::Dispatch => {
                dispatch_result = Some(set_dispatch_paused_raw(socket_path, reason.clone()).await?)
            }
            PauseSystem::Automation => {
                automation_result = Some(set_automation_paused_raw(socket_path, reason.clone()).await?)
            }
        }
    }

    if json {
        let mut systems = serde_json::Map::new();
        if let Some(d) = &dispatch_result {
            systems.insert(
                "dispatch".to_string(),
                serde_json::json!({
                    "paused": d.paused,
                    "paused_since_epoch_s": d.paused_since_epoch_s,
                    "reviews_exempt": d.reviews_exempt,
                    "reason": d.reason,
                }),
            );
        }
        if let Some(a) = &automation_result {
            systems.insert(
                "automation".to_string(),
                serde_json::json!({
                    "paused": a.paused,
                    "paused_since_epoch_s": a.paused_since_epoch_s,
                    "reason": a.reason,
                }),
            );
        }
        println!("{}", serde_json::Value::Object(systems));
    } else {
        if let Some(d) = &dispatch_result {
            println!("{}", format_dispatch_set_line(d));
        }
        if let Some(a) = &automation_result {
            println!("{}", format_automation_set_line(a));
        }
    }
    Ok(())
}

/// One-line status used by the unified `bossctl state` / `bossctl pause
/// state` view — deliberately more compact than the detailed multi-line
/// `dispatch state` / `automation state` output, since this view shows
/// every system at once.
pub(super) fn format_state_summary(paused: bool, paused_since_epoch_s: Option<u64>) -> String {
    if paused {
        match paused_since_epoch_s {
            Some(s) => format!("paused (since epoch {s})"),
            None => "paused".to_string(),
        }
    } else {
        "running".to_string()
    }
}

/// Show pause status for every system in the registry in one view.
/// Backs both `bossctl state` and `bossctl pause state`.
pub(super) async fn unified_state(socket_path: &Option<String>, json: bool) -> Result<()> {
    let dispatch = get_dispatch_state_raw(socket_path).await?;
    let automation = get_automation_state_raw(socket_path).await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "dispatch": {
                    "paused": dispatch.paused,
                    "paused_since_epoch_s": dispatch.paused_since_epoch_s,
                    "reviews_exempt": dispatch.reviews_exempt,
                    "reason": dispatch.reason,
                },
                "automation": {
                    "paused": automation.paused,
                    "paused_since_epoch_s": automation.paused_since_epoch_s,
                    "reason": automation.reason,
                },
            })
        );
    } else {
        println!(
            "dispatch:   {}",
            format_state_summary(dispatch.paused, dispatch.paused_since_epoch_s)
        );
        if dispatch.paused {
            if let Some(reason) = &dispatch.reason {
                println!("            reason: {reason}");
            }
            let reviews = if dispatch.reviews_exempt {
                "reviews: exempt — PR-review executions keep dispatching"
            } else {
                "reviews: held — spawn-capability breaker pause"
            };
            println!("            {reviews}");
        }
        println!(
            "automation: {}",
            format_state_summary(automation.paused, automation.paused_since_epoch_s)
        );
        if automation.paused
            && let Some(reason) = &automation.reason
        {
            println!("            reason: {reason}");
        }
    }
    Ok(())
}

/// Print recent dispatch pause/resume episodes with their full audit
/// evidence — file-scan only over `dispatch-events/current.jsonl` (same
/// pattern as `dispatch tail`/`diagnose`), so it works even when the engine
/// is wedged and doesn't depend on the live RPC state `dispatch state`
/// reads. Surfaces both operator (`dispatch_paused`/`dispatch_resumed`,
/// `origin: "operator"`) and breaker-originated (`origin: "breaker"`,
/// carrying the full `trigger` evidence — which executions/work
/// items/slots failed to spawn, over what window, against which
/// threshold) episodes in one place.
pub(super) fn dispatch_pause_history(json: bool, state_root: Option<PathBuf>, n: usize) -> Result<()> {
    let root = resolve_state_root(state_root)?;
    let read = dispatch_reader::read_current(&root)?;
    // A pause episode is read as a pair; an unreadable record can hide the
    // `dispatch_resumed` half and make a closed episode look open.
    let integrity = crate::stream_integrity::IntegrityReport::new(read.damage);
    let events = read.events;
    let mut episodes: Vec<&DispatchEvent> = events
        .iter()
        .filter(|e| e.stage == "dispatch_paused" || e.stage == "dispatch_resumed")
        .collect();
    episodes.reverse();
    episodes.truncate(n);

    if json {
        let value: Vec<serde_json::Value> = episodes
            .iter()
            .map(|e| {
                serde_json::json!({
                    "ts_epoch_ms": e.ts_epoch_ms,
                    "stage": e.stage,
                    "execution_id": e.execution_id,
                    "work_item_id": e.work_item_id,
                    "details": e.details,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "episodes": value,
                "stream_integrity": integrity.to_json(),
            })
        );
        return Ok(());
    }
    integrity.print_notice();

    if episodes.is_empty() {
        println!("no pause/resume episodes recorded");
        return Ok(());
    }

    for event in episodes {
        let kind = if event.stage == "dispatch_paused" {
            "PAUSED"
        } else {
            "RESUMED"
        };
        let origin = event.details["origin"].as_str().unwrap_or("unknown");
        let actor = event.details["actor"].as_str().unwrap_or("unknown");
        println!(
            "{kind}  ts_epoch_ms={} origin={origin} actor={actor}",
            event.ts_epoch_ms
        );
        if let Some(reason) = event.details["reason"].as_str() {
            println!("  reason: {reason}");
        }
        if let Some(scope) = event.details["scope"].as_array() {
            let scope: Vec<&str> = scope.iter().filter_map(|v| v.as_str()).collect();
            println!("  scope: {}", scope.join(","));
        }
        if let Some(duration) = event.details["pause_duration_secs"].as_u64() {
            println!("  pause_duration_secs: {duration}");
        }
        let trigger = &event.details["trigger"];
        if !trigger.is_null() {
            println!("  trigger rule: {}", trigger["rule"].as_str().unwrap_or(""));
            if let Some(triggering) = trigger["triggering_events"].as_array() {
                println!("  triggering events ({}):", triggering.len());
                for ev in triggering {
                    println!(
                        "    execution_id={} work_item_id={} slot_id={} shell_pid={} epoch_secs={}",
                        ev["execution_id"].as_str().unwrap_or(""),
                        ev["work_item_id"].as_str().unwrap_or(""),
                        ev["slot_id"].as_str().unwrap_or(""),
                        ev["shell_pid"].as_i64().unwrap_or(0),
                        ev["epoch_secs"].as_i64().unwrap_or(0),
                    );
                }
            }
        }
    }
    Ok(())
}
