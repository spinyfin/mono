//! `bossctl agents` — control verbs for live worker panes (status,
//! focus, send, interrupt, stop, reap, retire-pane, transcript,
//! launch, list, pools), the reference-resolution helpers shared with
//! `bossctl probe`, and the small neighboring `work start` / `work
//! cancel` / `reveal` / `open` verbs that were interleaved with them in
//! `main.rs`.
//!
//! Split out of `main.rs` for file-size hygiene; behavior is
//! unchanged from when these lived inline. Uses `use super::*` (like
//! the `app/*.rs` submodule split in `boss-engine`) rather than
//! explicit imports, since this is a large mechanical extraction of
//! already-reviewed code rather than a fresh module boundary.

use super::*;

/// Resolve a positional `agent` argument to a live worker entry.
///
/// Tries, in order: (a) exact match on `run_id`, (b) exact match on
/// numeric `slot_id`, (c) case-insensitive exact match on crew
/// `name`. The first non-empty tier wins; an ambiguous tier (more
/// than one match) errors with the candidate list.
///
/// Names resolve only over currently-live slots — historical run
/// ids stay run-id-only on purpose, so a typo'd crew name doesn't
/// silently match a closed run.
pub(crate) fn resolve_agent_ref<'a>(reference: &str, states: &'a [LiveWorkerState]) -> Result<&'a LiveWorkerState> {
    let by_run: Vec<&LiveWorkerState> = states.iter().filter(|s| s.run_id == reference).collect();
    if !by_run.is_empty() {
        return pick_unique(reference, by_run, states);
    }
    if let Ok(slot) = reference.parse::<u8>() {
        let by_slot: Vec<&LiveWorkerState> = states.iter().filter(|s| s.slot_id == slot).collect();
        if !by_slot.is_empty() {
            return pick_unique(reference, by_slot, states);
        }
    }
    let by_name: Vec<&LiveWorkerState> = states
        .iter()
        .filter(|s| s.name.eq_ignore_ascii_case(reference))
        .collect();
    if !by_name.is_empty() {
        return pick_unique(reference, by_name, states);
    }
    bail!(
        "no live worker matches `{reference}`. {}",
        live_candidates_summary(states),
    )
}

pub(crate) fn pick_unique<'a>(
    reference: &str,
    matches: Vec<&'a LiveWorkerState>,
    states: &'a [LiveWorkerState],
) -> Result<&'a LiveWorkerState> {
    if matches.len() == 1 {
        return Ok(matches[0]);
    }
    bail!(
        "`{reference}` matches multiple live workers: {}. {}",
        matches
            .iter()
            .map(|s| format!("slot {} ({}) run {}", s.slot_id, s.name, s.run_id))
            .collect::<Vec<_>>()
            .join(", "),
        live_candidates_summary(states),
    )
}

pub(crate) fn live_candidates_summary(states: &[LiveWorkerState]) -> String {
    if states.is_empty() {
        return "no live workers".into();
    }
    let mut sorted: Vec<&LiveWorkerState> = states.iter().collect();
    sorted.sort_by_key(|s| s.slot_id);
    let labels: Vec<String> = sorted
        .iter()
        .map(|s| format!("slot {} ({})", s.slot_id, s.name))
        .collect();
    format!("Live: {}", labels.join(", "))
}

/// True if `reference` looks like a name or numeric slot id (so a
/// resolver miss should be terminal rather than falling back to a
/// historical run-id lookup). A run id like `exec_18ad...` falls
/// through both checks.
pub(crate) fn looks_like_name_or_slot(reference: &str) -> bool {
    if reference.parse::<u8>().is_ok() {
        return true;
    }
    ROSTER.iter().any(|name| name.eq_ignore_ascii_case(reference))
}

/// Resolve `reference` to a work item when it looks like one: a friendly
/// short id (`T42`, `t42`, `P7`, `p7`) or a primary `task_…` / `proj_…` /
/// `prod_…` id. Returns `Ok(None)` for anything else (run ids, slot
/// numbers, crew names) so callers can fall through to their own
/// resolution — this never errors on a selector that simply isn't a work
/// item reference.
pub(crate) async fn resolve_work_item_ref(client: &mut BossClient, reference: &str) -> Result<Option<WorkItem>> {
    if reference.starts_with("task_") || reference.starts_with("proj_") || reference.starts_with("prod_") {
        return match client
            .send_request(&FrontendRequest::GetWorkItem {
                id: reference.to_owned(),
            })
            .await
            .context("resolving work item")?
        {
            FrontendEvent::WorkItemResult { item } => Ok(Some(item)),
            _ => Ok(None),
        };
    }
    if reference.len() < 2 {
        return Ok(None);
    }
    let first = reference.as_bytes()[0];
    if first != b'T' && first != b't' && first != b'P' && first != b'p' {
        return Ok(None);
    }
    let n: i64 = match reference[1..].parse() {
        Ok(n) if n > 0 => n,
        _ => return Ok(None),
    };
    let products = match client
        .send_request(&FrontendRequest::ListProducts)
        .await
        .context("listing products for friendly-id resolution")?
    {
        FrontendEvent::ProductsList { products } => products,
        _ => return Ok(None),
    };
    for product in &products {
        if let FrontendEvent::WorkItemResult { item } = client
            .send_request(&FrontendRequest::GetWorkItemByShortId {
                product_id: product.id.clone(),
                short_id: n,
            })
            .await
            .context("resolving friendly id")?
        {
            return Ok(Some(item));
        }
    }
    Ok(None)
}

/// If `selector` looks like a friendly work-item id (`T42`, `t42`, `P7`,
/// `p7`), resolve it to the primary id via the engine and search `states`
/// for a live worker running that work item. Returns the matching state,
/// or `None` when the selector isn't a friendly-id form or no live worker
/// is found for the resolved item.
async fn resolve_tnnn_to_live_worker<'a>(
    client: &mut BossClient,
    selector: &str,
    states: &'a [LiveWorkerState],
) -> Result<Option<&'a LiveWorkerState>> {
    let Some(item) = resolve_work_item_ref(client, selector).await? else {
        return Ok(None);
    };
    let primary_id = item.primary_id();
    Ok(states.iter().find(|s| s.work_item_id.as_deref() == Some(primary_id)))
}

/// Resolve `reference` to a live worker's run id, accepting run ids,
/// slot ids, crew names, and friendly work-item ids (T42, P7). Falls
/// back to the original `resolve_agent_ref` error when no match is found.
async fn resolve_agent_ref_or_work_item(
    client: &mut BossClient,
    reference: &str,
    states: &[LiveWorkerState],
) -> Result<String> {
    match resolve_agent_ref(reference, states) {
        Ok(state) => Ok(state.run_id.clone()),
        Err(agent_err) => {
            if let Some(state) = resolve_tnnn_to_live_worker(client, reference, states).await? {
                return Ok(state.run_id.clone());
            }
            Err(agent_err)
        }
    }
}

pub(crate) async fn fetch_live_states(client: &mut BossClient) -> Result<Vec<LiveWorkerState>> {
    match client
        .send_request(&FrontendRequest::ListWorkerLiveStates)
        .await
        .context("sending ListWorkerLiveStates")?
    {
        FrontendEvent::WorkerLiveStatesList { states } => Ok(states),
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected list: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}
/// Show live runtime status for the worker referenced by `agent`
/// (run id, slot id, or crew name). Falls back to the finalised
/// `WorkRun` record (the historical snapshot the engine persists
/// once the run row finalises) when the reference looks like a
/// run id and no matching live entry is found — so the verb still
/// works for runs that have already terminated. Crew-name and
/// slot-id references that miss are *not* fall through to the
/// historical lookup; they error with the live candidate list to
/// avoid silently matching a typo against a closed run.
pub(crate) async fn agents_status(socket_path: &Option<String>, json: bool, agent: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;

    match resolve_agent_ref(&agent, &states) {
        Ok(state) => {
            print_live_state(json, state);
            return Ok(());
        }
        Err(err) if looks_like_name_or_slot(&agent) => return Err(err),
        Err(_) => {}
    }

    // Not a live worker. If the reference resolves to a work item (T42,
    // P7, or a primary task_/proj_/prod_ id), report on it directly. This
    // is the only path available for a work item the engine has *parked*
    // rather than dispatched — e.g. the orphan-sweep / pr_review-recovery
    // churn guard: there is no live worker and (if it never got far enough
    // to spawn one) no `work_runs` row either, so the `GetRun` fallback
    // below would just error with "no such run".
    if let Some(work_item) = resolve_work_item_ref(&mut client, &agent).await? {
        let primary_id = work_item.primary_id().to_owned();
        if let Some(state) = states
            .iter()
            .find(|s| s.work_item_id.as_deref() == Some(primary_id.as_str()))
        {
            print_live_state(json, state);
            return Ok(());
        }
        return print_parked_work_item_status(&mut client, json, &work_item).await;
    }

    // No live entry and the reference doesn't look like a name or
    // slot — assume it's a historical run id.
    let response = client
        .send_request(&FrontendRequest::GetRun { id: agent.clone() })
        .await
        .context("sending GetRun")?;
    let run = match response {
        FrontendEvent::RunResult { run } => run,
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected status: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    };

    // `run` is the `work_runs` row for the pane-spawn task, not the
    // worker's lifecycle — every healthy spawn finalises that row
    // within ~5-8s regardless of how long the worker actually runs
    // (see the module docs on `LiveWorkerState`). Reporting `run`
    // alone reads as "an 8-second run" even when the worker is alive
    // and working minutes later. Resolve the owning execution and
    // report on *that*: prefer the live worker state if the worker is
    // still up, otherwise the execution's own status/timestamps.
    let live = states
        .iter()
        .find(|s| s.execution_id.as_deref() == Some(run.execution_id.as_str()) || s.run_id == run.execution_id);

    let execution = if live.is_some() {
        None
    } else {
        match client
            .send_request(&FrontendRequest::GetExecution {
                id: run.execution_id.clone(),
            })
            .await
            .context("sending GetExecution")?
        {
            FrontendEvent::ExecutionResult { execution } => Some(execution),
            _ => None,
        }
    };

    print_run_lifecycle(json, &run, live, execution.as_ref());
    Ok(())
}

/// Report on a work item that resolved from `bossctl agents status`'s
/// argument but has no live worker backing it — the case a bare `GetRun`
/// lookup can't handle because the item may never have gotten far enough
/// to spawn one (e.g. parked by the orphan-sweep / pr_review-recovery churn
/// guard). Prints the item's own status plus its current execution
/// (`GetTaskRuntime`) and any open operational attention items
/// (`ListAttentionItemsForWorkItem`) — the `churn_guard_parked` kind in
/// particular is exactly the "why is this active with no worker" signal
/// the coordinator previously had to dig out of the engine trace.
async fn print_parked_work_item_status(client: &mut BossClient, json: bool, work_item: &WorkItem) -> Result<()> {
    let primary_id = work_item.primary_id().to_owned();

    let runtime = match client
        .send_request(&FrontendRequest::GetTaskRuntime {
            work_item_id: primary_id.clone(),
        })
        .await
        .context("sending GetTaskRuntime")?
    {
        FrontendEvent::TaskRuntimeResult { runtime } => Some(runtime),
        _ => None,
    };
    let attention_items = match client
        .send_request(&FrontendRequest::ListAttentionItemsForWorkItem {
            work_item_id: primary_id.clone(),
        })
        .await
        .context("sending ListAttentionItemsForWorkItem")?
    {
        FrontendEvent::AttentionItemsForWorkItemList { items, .. } => items,
        _ => Vec::new(),
    };
    let open_attention_items: Vec<_> = attention_items
        .into_iter()
        .filter(|item| item.status == "open")
        .collect();

    if json {
        println!(
            "{}",
            serde_json::json!({
                "work_item": work_item,
                "live_worker_state": serde_json::Value::Null,
                "task_runtime": runtime,
                "open_attention_items": open_attention_items,
            })
        );
        return Ok(());
    }

    let (status, name) = match work_item {
        WorkItem::Task(t) | WorkItem::Chore(t) => (t.status.as_str().to_owned(), t.name.as_str()),
        WorkItem::Project(p) => (p.status.as_str().to_owned(), p.name.as_str()),
        WorkItem::Product(p) => (p.status.clone(), p.name.as_str()),
    };
    println!("{primary_id} \"{name}\" — no live worker");
    println!("  status: {status}");
    if let Some(runtime) = &runtime {
        println!(
            "  current_execution: {} [{}]",
            runtime.execution_id.as_deref().unwrap_or("-"),
            runtime
                .execution_status
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "-".to_owned())
        );
    }
    if open_attention_items.is_empty() {
        println!("  (no open attention items)");
    } else {
        println!("  open attention items:");
        for item in &open_attention_items {
            println!("    [{}] {} (since {})", item.kind, item.title, item.created_at);
        }
    }
    Ok(())
}

/// Renders a historical `GetRun` lookup alongside the worker's actual
/// lifecycle rather than just the pane-spawn task row. See the
/// `agents_status` doc comment above for why the two can diverge
/// wildly (a `completed`, 8-second `run` next to a worker still alive
/// 13+ minutes later). When `live` is `Some`, the worker is still up
/// and its `LiveWorkerState` (with an authoritative `shell_pid`, not
/// the possibly-stale `shell_pid 0` baked into the spawn row's
/// `result_summary` text) is the source of truth. Otherwise `execution`
/// carries the execution's own terminal status/timestamps, when the
/// engine could resolve it.
fn print_run_lifecycle(json: bool, run: &WorkRun, live: Option<&LiveWorkerState>, execution: Option<&WorkExecution>) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "pane_spawn_run": run,
                "note": "pane_spawn_run is the pane-spawn task record only; it finalises within \
                         seconds of every healthy spawn and does not reflect the worker's \
                         lifecycle. Use live_worker_state (if present) or execution for that.",
                "live_worker_state": live,
                "execution": execution,
            })
        );
        return;
    }

    println!("run {} (pane-spawn step only — not the worker lifecycle)", run.id);
    println!("  execution:     {}", run.execution_id);
    println!("  spawn status:  {}", run.status);
    if let Some(s) = &run.started_at {
        println!("  spawn started: {s}");
    }
    if let Some(f) = &run.finished_at {
        println!("  spawn finished:{f}");
    }

    match live {
        Some(state) => {
            println!();
            println!("worker is live — actual state:");
            print_live_state(false, state);
        }
        None => match execution {
            Some(exec) => {
                println!();
                println!("worker lifecycle (execution {}):", exec.id);
                println!("  status:   {}", exec.status.as_str());
                if let Some(s) = &exec.started_at {
                    println!("  started:  {s}");
                }
                if let Some(f) = &exec.finished_at {
                    println!("  finished: {f}");
                }
            }
            None => {
                println!(
                    "  (could not resolve owning execution {} for worker lifecycle)",
                    run.execution_id
                );
            }
        },
    }
}

/// List every live worker slot (model, activity, current tool, last
/// event time). Unlike the previous `agents list`, this is sourced
/// from the engine's in-memory LiveWorkerState rather than from the
/// finalised WorkRun records — those finalise within ~1s of spawn
/// and don't reflect the live worker.
pub(crate) async fn agents_list_live(socket_path: &Option<String>, json: bool, all: bool) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;

    let husk_panes = if all {
        let response = client
            .send_request(&FrontendRequest::ListHuskPanes)
            .await
            .context("sending ListHuskPanes")?;
        match response {
            FrontendEvent::HuskPanesList { panes } => panes,
            FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
                bail!("engine rejected list-husk-panes: {message}")
            }
            other => bail!("engine returned unexpected response: {other:?}"),
        }
    } else {
        Vec::new()
    };

    if json {
        println!(
            "{}",
            serde_json::json!({
                "live_worker_states": states,
                "husk_panes": husk_panes,
            })
        );
    } else {
        if states.is_empty() {
            println!("no active workers");
        } else {
            for state in &states {
                print_live_state_short(state);
            }
        }
        if all {
            if husk_panes.is_empty() {
                println!("no husk panes");
            } else {
                for pane in &husk_panes {
                    println!(
                        "slot {}  run={}  HUSK (app-hosted, no engine-tracked run — retire with `bossctl agents retire-pane {}`)",
                        pane.slot_id, pane.run_id, pane.slot_id,
                    );
                }
            }
        }
    }
    Ok(())
}

pub(crate) async fn agents_retire_pane(socket_path: &Option<String>, json: bool, slot_id: u8) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::RetirePane { slot_id })
        .await
        .context("sending RetirePane")?;
    match response {
        FrontendEvent::PaneRetired { slot_id } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "retired",
                        "slot_id": slot_id,
                    })
                );
            } else {
                println!("retired pane in slot {slot_id}");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected retire-pane: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

pub(crate) async fn agents_stop(socket_path: &Option<String>, json: bool, agent: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;
    let run_id = resolve_agent_ref_or_work_item(&mut client, &agent, &states).await?;
    let response = client
        .send_request(&FrontendRequest::StopRun { run_id: run_id.clone() })
        .await
        .context("sending StopRun")?;
    match response {
        FrontendEvent::RunStopped { run_id: returned } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "stopped",
                        "run_id": returned,
                    })
                );
            } else {
                println!("stopped run {returned}");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected stop: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// Place an explicit hold on the worker referenced by `agent`, exempting
/// it from the idle-park and auto-reap sweeps until released (`agents
/// release-hold`) or the run ends.
pub(crate) async fn agents_hold(
    socket_path: &Option<String>,
    json: bool,
    agent: String,
    reason: Option<String>,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;
    let run_id = resolve_agent_ref_or_work_item(&mut client, &agent, &states).await?;
    let response = client
        .send_request(&FrontendRequest::HoldRun {
            run_id: run_id.clone(),
            reason,
        })
        .await
        .context("sending HoldRun")?;
    match response {
        FrontendEvent::RunHeld {
            run_id: returned,
            reason,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "held",
                        "run_id": returned,
                        "reason": reason,
                    })
                );
            } else {
                match reason {
                    Some(reason) => println!("held run {returned} ({reason})"),
                    None => println!("held run {returned}"),
                }
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected hold: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// Release a hold previously placed by `agents hold` on the worker
/// referenced by `agent`.
pub(crate) async fn agents_release_hold(socket_path: &Option<String>, json: bool, agent: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;
    let run_id = resolve_agent_ref_or_work_item(&mut client, &agent, &states).await?;
    let response = client
        .send_request(&FrontendRequest::ReleaseHoldRun { run_id: run_id.clone() })
        .await
        .context("sending ReleaseHoldRun")?;
    match response {
        FrontendEvent::RunHoldReleased { run_id: returned } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "hold_released",
                        "run_id": returned,
                    })
                );
            } else {
                println!("released hold on run {returned}");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected release-hold: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

pub(crate) async fn agents_focus(socket_path: &Option<String>, json: bool, agent: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;
    let run_id = resolve_agent_ref_or_work_item(&mut client, &agent, &states).await?;
    let response = client
        .send_request(&FrontendRequest::FocusWorkerPane { run_id: run_id.clone() })
        .await
        .context("sending FocusWorkerPane")?;
    match response {
        FrontendEvent::WorkerPaneFocused {
            run_id: returned,
            slot_id,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "focused",
                        "run_id": returned,
                        "slot_id": slot_id,
                    })
                );
            } else {
                println!("focused slot {slot_id} (run {returned})");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected focus: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

pub(crate) async fn reveal_work_item(socket_path: &Option<String>, json: bool, id: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::RevealWorkItem { id: id.clone() })
        .await
        .context("sending RevealWorkItem")?;
    match response {
        FrontendEvent::WorkItemRevealed { id: canonical_id } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "revealed",
                        "id": canonical_id,
                    })
                );
            } else {
                println!("revealed {canonical_id}");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected reveal: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// Open a markdown file in the Boss UI (the coordinator-invocable
/// equivalent of File ▸ Open). `path` is resolved against this
/// process's current directory before it goes on the wire — the
/// engine and the app each have their own working directory, so a
/// relative path is only unambiguous here, at the caller. Path
/// existence/readability/markdown-ness is validated engine-side (see
/// [`FrontendRequest::OpenDocument`]); this function's own error
/// handling only covers `std::env::current_dir` failing and the
/// engine's rejection responses (not found, not markdown, no app
/// session registered — the last one arrives with an actionable
/// "launch/relaunch Boss" message baked in by the engine).
/// Resolve `path` against `cwd` if it isn't already absolute. Split out
/// of [`open_document`] so the relative-path case can be tested
/// headlessly, without a socket connection.
pub(crate) fn resolve_document_path(cwd: &Path, path: &str) -> String {
    if Path::new(path).is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path).to_string_lossy().into_owned()
    }
}

pub(crate) async fn open_document(socket_path: &Option<String>, json: bool, path: String) -> Result<()> {
    let cwd = std::env::current_dir().context("resolving current directory for a relative path")?;
    let resolved = resolve_document_path(&cwd, &path);
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::OpenDocument { path: resolved.clone() })
        .await
        .context("sending OpenDocument")?;
    match response {
        FrontendEvent::DocumentOpened { path } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "opened",
                        "path": path,
                    })
                );
            } else {
                println!("opened {path}");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected open: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// Inject `text` into the worker pane referenced by `agent`, as if
/// the user had typed it and pressed Return. The submit step is the
/// app-side writer's responsibility: after pasting the body via
/// libghostty's text path it synthesises a Return keystroke, which
/// is what makes the prompt land. Earlier revisions of this CLI
/// appended a trailing `\n` here in the hope that the paste path
/// would treat it as Enter; it does not (the `\n` lands as a literal
/// newline character in the input field), so the writer owns
/// submission now and the CLI ships the text verbatim.
pub(crate) async fn agents_send(socket_path: &Option<String>, json: bool, agent: String, text: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;
    let run_id = resolve_agent_ref_or_work_item(&mut client, &agent, &states).await?;
    let response = client
        .send_request(&FrontendRequest::SendInputToWorker {
            run_id: run_id.clone(),
            text,
        })
        .await
        .context("sending SendInputToWorker")?;
    match response {
        FrontendEvent::WorkerInputSent {
            run_id: returned,
            slot_id,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "sent",
                        "run_id": returned,
                        "slot_id": slot_id,
                    })
                );
            } else {
                println!("sent input to slot {slot_id} (run {returned})");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected send: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// Interrupt the worker referenced by `agent` — equivalent to the
/// human pressing Esc inside that worker's pane. Cancels the
/// in-flight turn without killing the run.
pub(crate) async fn agents_interrupt(socket_path: &Option<String>, json: bool, agent: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;
    let run_id = resolve_agent_ref_or_work_item(&mut client, &agent, &states).await?;
    let response = client
        .send_request(&FrontendRequest::InterruptWorkerPane { run_id: run_id.clone() })
        .await
        .context("sending InterruptWorkerPane")?;
    match response {
        FrontendEvent::WorkerPaneInterrupted {
            run_id: returned,
            slot_id,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "interrupted",
                        "run_id": returned,
                        "slot_id": slot_id,
                    })
                );
            } else {
                println!("interrupted slot {slot_id} (run {returned})");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected interrupt: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// Skip-the-queue spawn for `bossctl agents launch <work-item-id>`.
/// Maps to `RequestExecution { force: true, .. }`: the engine grows
/// the worker pool by one slot up to the hard cap when every
/// configured slot is busy and dispatches the work item immediately,
/// rather than letting the auto-dispatcher defer until a slot frees
/// up.
pub(crate) async fn agents_launch(
    socket_path: &Option<String>,
    json: bool,
    work_item_id: String,
    preferred_workspace_id: Option<String>,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::RequestExecution {
            input: RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .maybe_preferred_workspace_id(preferred_workspace_id)
                .force(true)
                .build(),
        })
        .await
        .context("sending RequestExecution (force)")?;
    match response {
        FrontendEvent::ExecutionRequested { execution }
        | FrontendEvent::ExecutionCreated { execution }
        | FrontendEvent::ExecutionResult { execution } => {
            print_execution(json, &execution);
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected agents launch: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

pub(crate) async fn work_start(
    socket_path: &Option<String>,
    json: bool,
    work_item_id: String,
    priority: Option<i64>,
    preferred_workspace_id: Option<String>,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::RequestExecution {
            input: RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .maybe_priority(priority)
                .maybe_preferred_workspace_id(preferred_workspace_id)
                .build(),
        })
        .await
        .context("sending RequestExecution")?;
    match response {
        FrontendEvent::ExecutionRequested { execution }
        | FrontendEvent::ExecutionCreated { execution }
        | FrontendEvent::ExecutionResult { execution } => {
            print_execution(json, &execution);
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected work start: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

pub(crate) async fn work_cancel(socket_path: &Option<String>, json: bool, execution_id: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::CancelExecution {
            execution_id: execution_id.clone(),
        })
        .await
        .context("sending CancelExecution")?;
    match response {
        FrontendEvent::ExecutionCancelled { execution } => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&execution).expect("WorkExecution serializes")
                );
            } else {
                println!("cancelled execution {}", execution.id);
                println!("  status:    {}", execution.status);
                if let Some(f) = &execution.finished_at {
                    println!("  finished:  {f}");
                }
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected work cancel: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

pub(crate) async fn agents_transcript(
    socket_path: &Option<String>,
    json: bool,
    agent: String,
    lines: usize,
    format: TranscriptFormat,
    no_tools: bool,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;

    // For live workers resolve via the registry. For completed/terminal
    // executions the live registry has no entry — fall through and let
    // the engine query work_runs.transcript_path from the DB. The
    // engine's resolve_transcript_for_tail handles both the exec_* and
    // run_* namespaces, so passing the raw ref works for either id form.
    // Friendly ids (T42) are tried as live-worker references first.
    let run_id = match resolve_agent_ref(&agent, &states) {
        Ok(state) => state.run_id.clone(),
        Err(err) if looks_like_name_or_slot(&agent) => return Err(err),
        Err(_) => {
            if let Some(state) = resolve_tnnn_to_live_worker(&mut client, &agent, &states).await? {
                state.run_id.clone()
            } else {
                agent.clone()
            }
        }
    };

    let response = client
        .send_request(&FrontendRequest::TailRunTranscript {
            run_id: run_id.clone(),
            lines,
        })
        .await
        .context("sending TailRunTranscript")?;
    match response {
        FrontendEvent::RunTranscriptTail {
            run_id: returned,
            transcript_path,
            lines: tail,
            truncated,
        } => {
            let render_opts = boss_engine::transcript_markdown::RenderOpts {
                hide_tools: no_tools,
                ..Default::default()
            };
            if format == TranscriptFormat::Text || format == TranscriptFormat::Markdown {
                let joined = tail.join("\n");
                let events = boss_engine::transcript_markdown::parse_transcript(&joined);
                let rendered = if format == TranscriptFormat::Markdown {
                    let segments = boss_engine::transcript_markdown::events_to_segments(&events, &render_opts);
                    boss_engine::transcript_markdown::segments_to_markdown(&segments)
                } else {
                    boss_engine::transcript_markdown::render_text(&events, &render_opts)
                };
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "run_id": returned,
                            "transcript_path": transcript_path,
                            "rendered": rendered,
                            "truncated": truncated,
                        })
                    );
                } else {
                    if truncated {
                        println!(
                            "transcript {transcript_path} (showing last {} lines; older content omitted)",
                            tail.len()
                        );
                    } else {
                        println!("transcript {transcript_path} ({} lines)", tail.len());
                    }
                    print!("{rendered}");
                }
                return Ok(());
            }
            // TranscriptFormat::Jsonl — dump raw JSONL lines.
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "run_id": returned,
                        "transcript_path": transcript_path,
                        "lines": tail,
                        "truncated": truncated,
                    })
                );
            } else {
                if truncated {
                    println!(
                        "transcript {transcript_path} (showing last {} lines; older content omitted)",
                        tail.len()
                    );
                } else {
                    println!("transcript {transcript_path} ({} lines)", tail.len());
                }
                for line in tail {
                    println!("{line}");
                }
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected transcript tail: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

pub(crate) async fn agents_reap(socket_path: &Option<String>, json: bool, run_id: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::ReapRun { run_id: run_id.clone() })
        .await
        .context("sending ReapRun")?;
    match response {
        FrontendEvent::RunReaped {
            run_id: returned,
            execution,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "reaped",
                        "run_id": returned,
                        "execution": execution,
                    })
                );
            } else {
                println!("reaped run {returned}");
                println!("  execution:        {}", execution.id);
                println!("  status:           {}", execution.status);
                if let Some(ws) = &execution.cube_workspace_id {
                    println!("  workspace_id:     {ws}  (preserved for re-lease)");
                }
                if let Some(path) = &execution.workspace_path {
                    println!("  workspace_path:   {path}");
                }
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected reap: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

pub(crate) async fn agents_pools(socket_path: &Option<String>, json: bool) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::WorkerPoolSummary)
        .await
        .context("sending WorkerPoolSummary")?;
    match response {
        FrontendEvent::WorkerPoolSummaryResult { pools } => {
            if json {
                println!("{}", serde_json::json!({ "pools": pools }));
            } else {
                for pool in &pools {
                    println!(
                        "{}: {}/{} claimed ({} idle)",
                        pool.name,
                        pool.claims.len(),
                        pool.capacity,
                        pool.idle,
                    );
                    for claim in &pool.claims {
                        let status = claim.execution_status.as_deref().unwrap_or("?");
                        let work_item = claim.work_item_id.as_deref().unwrap_or("-");
                        let flag = if claim.live { "" } else { "  <-- LEAKED?" };
                        // A spilled claim sits in this pool's slot but is
                        // someone else's work — say so, or the reader will
                        // miscount per-pool load.
                        let spilled = claim
                            .spilled_from_pool
                            .as_deref()
                            .map(|from| format!("  (spilled from {from})"))
                            .unwrap_or_default();
                        println!(
                            "  {}  execution={}  status={}  work_item={}{}{}",
                            claim.worker_id, claim.execution_id, status, work_item, spilled, flag,
                        );
                    }
                }
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected pool summary: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

fn print_live_state(json: bool, state: &LiveWorkerState) {
    if json {
        println!("{}", serde_json::to_string(state).expect("LiveWorkerState serializes"));
        return;
    }
    println!("slot {} ({})", state.slot_id, state.name);
    println!("  run:           {}", state.run_id);
    println!("  model:         {}", state.model);
    println!("  activity:      {}", state.activity.as_str());
    println!("  shell_pid:     {}", state.shell_pid);
    if state.held {
        println!("  held:          true (exempt from idle-park/auto-reap sweeps)");
    }
    if let Some(recovery) = &state.recovery_status {
        println!("  recovery:      {recovery}");
    }
    if let Some(id) = &state.work_item_id {
        println!("  work_item:     {id}");
    }
    if let Some(name) = &state.work_item_name {
        println!("  work_item_name:{name}");
    }
    if let Some(id) = &state.execution_id {
        println!("  execution:     {id}");
    }
    if let Some(tool) = &state.current_tool {
        println!("  current_tool:  {tool}");
    }
    if let Some(ts) = &state.last_event_at {
        println!("  last_event_at: {ts}");
    }
    if let Some(ts) = &state.last_tool_ended_at {
        println!("  last_tool_end: {ts}");
    }
}

fn print_live_state_short(state: &LiveWorkerState) {
    let tool = state.current_tool.as_deref().unwrap_or("-");
    let work_item = state.work_item_id.as_deref().unwrap_or("-");
    let work_item_name = state.work_item_name.as_deref().unwrap_or("-");
    print!(
        "slot {}  name={}  run={}  model={}  activity={}  tool={}  work_item={}  work_item_name=\"{}\"",
        state.slot_id,
        state.name,
        state.run_id,
        state.model,
        state.activity.as_str(),
        tool,
        work_item,
        work_item_name,
    );
    // Surfaced whenever the transient-recovery sweep is actively nudging
    // this slot — without this an auto-recovering worker prints as plain
    // `activity=idle`, indistinguishable from a normally-finished turn.
    if let Some(recovery) = &state.recovery_status {
        print!("  recovery=\"{recovery}\"");
    }
    if state.held {
        print!("  held=true");
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::{Product, Project, ProjectStatus, Task, TaskKind, TaskStatus};

    /// Build a live-worker fixture with a caller-chosen slot id, run id, and
    /// crew name. Setting `name` explicitly (rather than deriving it from
    /// `slot_id` the way production does) lets the resolver tests target each
    /// match tier — and the ambiguous-slot / ambiguous-name paths —
    /// independently of the roster's slot→name mapping.
    fn worker(slot_id: u8, run_id: &str, name: &str) -> LiveWorkerState {
        let mut state = LiveWorkerState::new_spawning(slot_id, run_id, "opus", 0, None);
        state.name = name.to_owned();
        state
    }

    fn task(id: &str, kind: TaskKind) -> Task {
        Task::builder()
            .id(id)
            .product_id("prod_1")
            .kind(kind)
            .name("n")
            .description("")
            .status(TaskStatus::Todo)
            .created_at("")
            .updated_at("")
            .build()
    }

    fn product(id: &str) -> Product {
        Product::builder()
            .id(id)
            .name("n")
            .slug("n")
            .description("")
            .status("active")
            .created_at("")
            .updated_at("")
            .build()
    }

    fn project(id: &str) -> Project {
        Project::builder()
            .id(id)
            .product_id("prod_1")
            .name("n")
            .slug("n")
            .description("")
            .goal("")
            .status(ProjectStatus::Planned)
            .created_at("")
            .updated_at("")
            .build()
    }

    // ---- resolve_document_path ---------------------------------------------

    #[test]
    fn resolve_document_path_leaves_absolute_path_untouched() {
        let cwd = Path::new("/some/other/dir");
        assert_eq!(resolve_document_path(cwd, "/abs/path/notes.md"), "/abs/path/notes.md");
    }

    #[test]
    fn resolve_document_path_joins_relative_path_against_cwd() {
        let cwd = Path::new("/home/user/project");
        assert_eq!(
            resolve_document_path(cwd, "docs/notes.md"),
            "/home/user/project/docs/notes.md"
        );
    }

    // ---- resolve_agent_ref -------------------------------------------------

    #[test]
    fn resolves_by_exact_run_id() {
        let states = [worker(1, "exec_abc", "Riker"), worker(2, "exec_def", "Data")];
        let resolved = resolve_agent_ref("exec_def", &states).expect("run id should resolve");
        assert_eq!(resolved.run_id, "exec_def");
        assert_eq!(resolved.slot_id, 2);
    }

    #[test]
    fn resolves_by_numeric_slot_id() {
        let states = [worker(1, "exec_abc", "Riker"), worker(7, "exec_def", "Yar")];
        let resolved = resolve_agent_ref("7", &states).expect("slot id should resolve");
        assert_eq!(resolved.slot_id, 7);
        assert_eq!(resolved.run_id, "exec_def");
    }

    #[test]
    fn resolves_by_name_case_insensitive() {
        let states = [worker(1, "exec_abc", "Riker"), worker(2, "exec_def", "Data")];
        let resolved = resolve_agent_ref("dATa", &states).expect("crew name should resolve");
        assert_eq!(resolved.slot_id, 2);
        assert_eq!(resolved.run_id, "exec_def");
    }

    /// Slot 4's crew name is "La Forge" — the space is part of the name,
    /// and the case-insensitive *exact* match honours it.
    #[test]
    fn resolves_multiword_name_with_space() {
        let states = [worker(4, "exec_d", "La Forge")];
        let resolved = resolve_agent_ref("la forge", &states).expect("multi-word name should resolve");
        assert_eq!(resolved.slot_id, 4);
    }

    /// A numeric reference that matches one worker's run id and *also*
    /// another worker's slot resolves to the run-id match — a defensive
    /// case, since real run ids are never bare numbers, but it pins the
    /// tier order (run id before slot).
    #[test]
    fn run_id_match_takes_precedence_over_slot() {
        let states = [worker(2, "1", "Data"), worker(1, "exec_a", "Riker")];
        let resolved = resolve_agent_ref("1", &states).expect("run id tier should win");
        assert_eq!(resolved.run_id, "1");
        assert_eq!(resolved.slot_id, 2, "run-id match must win over the slot match");
    }

    /// A reference that matches one worker's run id and *also* another
    /// worker's name resolves to the run-id match — the run-id tier is
    /// consulted first and short-circuits.
    #[test]
    fn run_id_match_takes_precedence_over_name() {
        let states = [worker(1, "shared", "Riker"), worker(2, "exec_def", "shared")];
        let resolved = resolve_agent_ref("shared", &states).expect("run id tier should win");
        assert_eq!(resolved.slot_id, 1, "run-id match must win over the name match");
        assert_eq!(resolved.run_id, "shared");
    }

    /// A numeric reference that matches one worker's slot and *also*
    /// another worker's (numeric) name resolves to the slot match — the
    /// slot tier is consulted before the name tier.
    #[test]
    fn slot_match_takes_precedence_over_name() {
        let states = [worker(5, "exec_abc", "Data"), worker(2, "exec_def", "5")];
        let resolved = resolve_agent_ref("5", &states).expect("slot tier should win");
        assert_eq!(resolved.slot_id, 5, "slot match must win over the name match");
        assert_eq!(resolved.run_id, "exec_abc");
    }

    #[test]
    fn ambiguous_name_reports_all_candidates() {
        let states = [worker(1, "exec_abc", "Data"), worker(2, "exec_def", "Data")];
        let err = resolve_agent_ref("data", &states).expect_err("two workers share a name");
        let msg = err.to_string();
        assert!(msg.contains("matches multiple live workers"), "message was: {msg}");
        assert!(msg.contains("slot 1 (Data) run exec_abc"), "message was: {msg}");
        assert!(msg.contains("slot 2 (Data) run exec_def"), "message was: {msg}");
    }

    #[test]
    fn ambiguous_slot_reports_all_candidates() {
        let states = [worker(3, "exec_abc", "Worf"), worker(3, "exec_def", "Riker")];
        let err = resolve_agent_ref("3", &states).expect_err("two workers share a slot");
        let msg = err.to_string();
        assert!(msg.contains("matches multiple live workers"), "message was: {msg}");
        assert!(msg.contains("slot 3 (Worf) run exec_abc"), "message was: {msg}");
        assert!(msg.contains("slot 3 (Riker) run exec_def"), "message was: {msg}");
    }

    #[test]
    fn no_match_errors_with_live_candidates() {
        let states = [worker(2, "exec_def", "Data"), worker(1, "exec_abc", "Riker")];
        let err = resolve_agent_ref("nonesuch", &states).expect_err("no worker matches");
        let msg = err.to_string();
        assert!(msg.contains("no live worker matches `nonesuch`"), "message was: {msg}");
        // The candidate summary is appended and sorted by slot id.
        assert!(
            msg.contains("Live: slot 1 (Riker), slot 2 (Data)"),
            "message was: {msg}"
        );
    }

    #[test]
    fn no_match_with_no_live_workers() {
        let err = resolve_agent_ref("anything", &[]).expect_err("no workers at all");
        let msg = err.to_string();
        assert!(msg.contains("no live worker matches `anything`"), "message was: {msg}");
        assert!(msg.contains("no live workers"), "message was: {msg}");
    }

    // ---- pick_unique -------------------------------------------------------

    #[test]
    fn pick_unique_returns_sole_match() {
        let states = [worker(4, "exec_abc", "La Forge")];
        let resolved = pick_unique("La Forge", vec![&states[0]], &states).expect("exactly one match");
        assert_eq!(resolved.slot_id, 4);
        assert_eq!(resolved.run_id, "exec_abc");
    }

    #[test]
    fn pick_unique_bails_on_multiple_matches() {
        let states = [worker(1, "exec_abc", "Riker"), worker(2, "exec_def", "Data")];
        let err = pick_unique("x", vec![&states[0], &states[1]], &states).expect_err("two matches");
        let msg = err.to_string();
        assert!(msg.contains("`x` matches multiple live workers"), "message was: {msg}");
        assert!(msg.contains("slot 1 (Riker) run exec_abc"), "message was: {msg}");
        assert!(msg.contains("slot 2 (Data) run exec_def"), "message was: {msg}");
    }

    // ---- live_candidates_summary ------------------------------------------

    #[test]
    fn summary_reports_no_live_workers_when_empty() {
        assert_eq!(live_candidates_summary(&[]), "no live workers");
    }

    #[test]
    fn summary_lists_workers_sorted_by_slot_id() {
        // Deliberately out of slot order on input to prove the sort.
        let states = [worker(2, "exec_def", "Data"), worker(1, "exec_abc", "Riker")];
        assert_eq!(live_candidates_summary(&states), "Live: slot 1 (Riker), slot 2 (Data)");
    }

    // ---- looks_like_name_or_slot ------------------------------------------

    #[test]
    fn numeric_slot_looks_like_name_or_slot() {
        assert!(looks_like_name_or_slot("5"));
        assert!(looks_like_name_or_slot("0"));
    }

    #[test]
    fn roster_name_looks_like_name_or_slot_case_insensitive() {
        assert!(looks_like_name_or_slot("Riker"));
        assert!(looks_like_name_or_slot("riker"));
        assert!(looks_like_name_or_slot("LA FORGE"));
    }

    #[test]
    fn run_id_does_not_look_like_name_or_slot() {
        assert!(!looks_like_name_or_slot("exec_18ad9f"));
        // Not a roster name, and out of `u8` range so it isn't a slot either.
        assert!(!looks_like_name_or_slot("300"));
        assert!(!looks_like_name_or_slot("Picard"));
    }

    // ---- WorkItem::primary_id ---------------------------------------------

    #[test]
    fn primary_id_for_each_work_item_variant() {
        assert_eq!(WorkItem::Product(product("prod_9")).primary_id(), "prod_9");
        assert_eq!(WorkItem::Project(project("proj_9")).primary_id(), "proj_9");
        assert_eq!(WorkItem::Task(task("task_9", TaskKind::Task)).primary_id(), "task_9");
        assert_eq!(
            WorkItem::Chore(task("chore_9", TaskKind::Chore)).primary_id(),
            "chore_9"
        );
    }
}
