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

/// True if `reference` looks like a name or numeric slot-shaped reference (so a
/// resolver miss should be terminal rather than falling back to a
/// historical run-id lookup). A run id like `exec_18ad...` falls
/// through both checks.
pub(crate) fn looks_like_name_or_slot(reference: &str) -> bool {
    if !reference.is_empty() && reference.bytes().all(|byte| byte.is_ascii_digit()) {
        return true;
    }
    ROSTER.iter().any(|name| name.eq_ignore_ascii_case(reference))
}

/// Whether a worker-resolution miss may fall back to a friendly work-item
/// selector. Slot-shaped references and crew names must remain worker errors.
fn work_item_fallback_eligible(reference: &str) -> bool {
    !looks_like_name_or_slot(reference)
}

/// Resolve `reference` to a work item via the engine's shared id-resolution
/// choke point (`GetWorkItem` → `WorkDb::resolve_work_item_ref`).
///
/// Accepts friendly short ids (`T42`, `t42`, `P7`, `#42`, bare `42`,
/// `slug/n`) and primary `task_…` / `proj_…` / `prod_…` ids. Ambiguous
/// short ids hard-error with every candidate listed — never silently
/// pick the first product match.
///
/// Returns `Ok(None)` for anything that is not a work-item selector form
/// (run ids, slot numbers, crew names) so callers can fall through to
/// their own resolution. A friendly form that matches nothing also
/// returns `Ok(None)` (not an error) so agent verbs can report "no
/// worker matches" (with live / durable candidate lists) rather than
/// "no such work item". Ambiguity still hard-errors — it must never
/// fall through to the first-product-match path.
pub(crate) async fn resolve_work_item_ref(client: &mut BossClient, reference: &str) -> Result<Option<WorkItem>> {
    let looks_like_work_item =
        boss_protocol::is_friendly_work_item_selector(reference) || boss_protocol::is_typed_work_item_id(reference);
    if !looks_like_work_item {
        return Ok(None);
    }
    // Normalize bare/hash short ids to the T{n} wire form so GetWorkItem
    // hits the shared engine choke point uniformly.
    let wire_id = match boss_protocol::parse_work_item_selector(reference) {
        boss_protocol::WorkItemSelector::ShortId(n) => boss_protocol::short_id_wire_form(n),
        _ => reference.to_owned(),
    };
    match client
        .send_request(&FrontendRequest::GetWorkItem { id: wire_id })
        .await
        .context("resolving work item")?
    {
        FrontendEvent::WorkItemResult { item } => Ok(Some(item)),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            // Discriminate via protocol markers (not free-form English) so
            // a reworded engine message cannot silently flip this back to
            // "pick the first product". Ambiguity must surface; not-found
            // falls through to no_worker_matches_error.
            if message.contains(boss_protocol::WORK_ITEM_ID_AMBIGUOUS_MARKER) {
                bail!("{message}");
            }
            if message.contains(boss_protocol::WORK_ITEM_ID_NOT_FOUND_MARKER) {
                return Ok(None);
            }
            // Unknown error class from the engine — surface it rather
            // than swallow.
            bail!("{message}");
        }
        _ => Ok(None),
    }
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

/// Resolve `reference` to a live worker's run id.
///
/// Disambiguation order (first tier with any match wins; an ambiguous tier
/// still errors rather than falling through):
///
/// 1. Live registry: run id, then numeric slot id, then crew name (see
///    [`resolve_agent_ref`]).
/// 2. Engine's durable/hosted-pane roster, same run-id/slot-id/crew-name
///    order (a worker the live registry has dropped — crash, terminal-fail
///    path, spawn-ack timeout — but the app and durable state still
///    account for).
/// 3. Friendly work-item id (`T42`, `P7`) — but only when `reference`
///    does not itself look like a slot id or a crew name. A bare decimal
///    integer or a roster name is far more likely to be a worker
///    reference than a work-item short id here, since every verb sharing
///    this resolver operates on workers and an operator reading a slot
///    number off `agents list` expects it to resolve as a slot, not get
///    reinterpreted as `T<n>` against an unrelated product. See
///    [`looks_like_name_or_slot`].
///
/// Errors with a combined candidate list (live AND durable-tracked-but-not-
/// live) when nothing matches at all.
///
/// Shared by every `agents` verb whose engine RPC takes a bare run id and
/// has no raw-passthrough escape hatch of its own (`stop`/`reap` layer
/// their own on top — see [`agents_stop`]/[`agents_reap`]).
async fn resolve_agent_ref_or_work_item(
    client: &mut BossClient,
    reference: &str,
    states: &[LiveWorkerState],
) -> Result<String> {
    if let Ok(state) = resolve_agent_ref(reference, states) {
        return Ok(state.run_id.clone());
    }
    let hosted = fetch_hosted_pane_statuses(client).await?;
    if let Some(pane) = resolve_hosted_pane_ref(reference, &hosted)? {
        return Ok(pane.run_id.clone());
    }
    if !work_item_fallback_eligible(reference) {
        return Err(no_worker_matches_error(reference, states, &hosted));
    }
    if let Some(state) = resolve_tnnn_to_live_worker(client, reference, states).await? {
        return Ok(state.run_id.clone());
    }
    Err(no_worker_matches_error(reference, states, &hosted))
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

/// Fetch every pane the app hosts, classified against the engine's live
/// registry and durable state (live / terminal-entry-with-live-process /
/// husk) — see [`HostedPaneState`]. This is the durable-state fallback
/// every `agents` verb consults once a plain live-registry lookup misses,
/// so a crew name or slot id the operator can see in the app still
/// resolves after the engine drops the live registry entry.
pub(crate) async fn fetch_hosted_pane_statuses(client: &mut BossClient) -> Result<Vec<HostedPaneStatus>> {
    match client
        .send_request(&FrontendRequest::ListHostedPaneStatuses)
        .await
        .context("sending ListHostedPaneStatuses")?
    {
        FrontendEvent::HostedPaneStatusList { panes } => Ok(panes),
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected list: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// Resolve `reference` against the durable/hosted-pane roster: exact
/// `run_id`, then numeric `slot_id`, then case-insensitive `crew_name` —
/// the same tier order as [`resolve_agent_ref`]. Returns `Ok(None)` on a
/// total miss (callers chain further fallbacks), and errors only when
/// `reference` matches more than one pane in the same tier.
pub(crate) fn resolve_hosted_pane_ref<'a>(
    reference: &str,
    panes: &'a [HostedPaneStatus],
) -> Result<Option<&'a HostedPaneStatus>> {
    let by_run: Vec<&HostedPaneStatus> = panes.iter().filter(|p| p.run_id == reference).collect();
    if !by_run.is_empty() {
        return pick_unique_pane(reference, by_run).map(Some);
    }
    if let Ok(slot) = reference.parse::<u8>() {
        let by_slot: Vec<&HostedPaneStatus> = panes.iter().filter(|p| p.slot_id == slot).collect();
        if !by_slot.is_empty() {
            return pick_unique_pane(reference, by_slot).map(Some);
        }
    }
    let by_name: Vec<&HostedPaneStatus> = panes
        .iter()
        .filter(|p| p.crew_name.eq_ignore_ascii_case(reference))
        .collect();
    if !by_name.is_empty() {
        return pick_unique_pane(reference, by_name).map(Some);
    }
    Ok(None)
}

fn pick_unique_pane<'a>(reference: &str, matches: Vec<&'a HostedPaneStatus>) -> Result<&'a HostedPaneStatus> {
    if matches.len() == 1 {
        return Ok(matches[0]);
    }
    bail!(
        "`{reference}` matches multiple tracked panes: {}",
        matches
            .iter()
            .map(|p| format!(
                "slot {} ({}) run {} [{}]",
                p.slot_id,
                p.crew_name,
                p.run_id,
                pane_state_label(&p.state)
            ))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn pane_state_label(state: &HostedPaneState) -> &'static str {
    match state {
        HostedPaneState::Live => "live",
        HostedPaneState::LiveProcessNoRegistry { .. } => "terminal entry, live process",
        HostedPaneState::Husk => "husk",
    }
}

/// Build the "no worker matches" error for a reference that resolved in
/// neither the live registry nor the durable/hosted-pane roster, listing
/// both so the operator can see what was actually searched — not just the
/// live crew (mirrors what `live_candidates_summary` alone used to show).
fn no_worker_matches_error(reference: &str, states: &[LiveWorkerState], hosted: &[HostedPaneStatus]) -> anyhow::Error {
    let live = live_candidates_summary(states);
    let non_live: Vec<String> = hosted
        .iter()
        .filter(|p| !matches!(p.state, HostedPaneState::Live))
        .map(|p| {
            format!(
                "slot {} ({}) run {} [{}]",
                p.slot_id,
                p.crew_name,
                p.run_id,
                pane_state_label(&p.state)
            )
        })
        .collect();
    if non_live.is_empty() {
        anyhow::anyhow!("no worker matches `{reference}`. {live}")
    } else {
        anyhow::anyhow!(
            "no worker matches `{reference}`. {live}. Also tracked (not live): {}",
            non_live.join(", "),
        )
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
        Err(_) => {
            // Durable-state fallback: a crew name or slot id the operator can
            // see in the app must resolve even after the engine has dropped
            // the live registry entry (crash, terminal-fail path, spawn-ack
            // timeout).
            let hosted = fetch_hosted_pane_statuses(&mut client).await?;
            if let Some(pane) = resolve_hosted_pane_ref(&agent, &hosted)? {
                print_hosted_pane_status(json, pane);
                return Ok(());
            }
            if looks_like_name_or_slot(&agent) {
                return Err(no_worker_matches_error(&agent, &states, &hosted));
            }
        }
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
/// (`ListAttentionItemsForWorkItem`).
///
/// Two representations carry the "why is this active/todo with no worker"
/// signal, and this prints both: a `churn_guard_parked` open attention item
/// (still how `pr_review_recovery` files it) via `open_attention_items`
/// below, and `tasks.dispatch_failed_reason` / `dispatch_failed_error` (how
/// `orphan_sweep`'s churn guard and a pre-spawn dispatch failure park a
/// `Task`/`Chore` since `docs/designs/dispatch-halt-state-vs-attention-items.md`)
/// printed alongside `status`, mirroring `print_task_details` in
/// `cli/src/output.rs`. Without the latter, a churn-parked task/chore would
/// print `status: todo` followed by "(no open attention items)" with no clue
/// why — the same read-surface gap `boss task/chore show` already closed.
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
    if let WorkItem::Task(t) | WorkItem::Chore(t) = work_item
        && let Some(reason) = t.dispatch_failed_reason.as_deref()
    {
        println!("  dispatch_failed_reason: {reason}");
        if let Some(error) = t.dispatch_failed_error.as_deref() {
            println!("    {error}");
        }
    }
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

    let hosted = if all {
        fetch_hosted_pane_statuses(&mut client).await?
    } else {
        Vec::new()
    };

    if json {
        println!(
            "{}",
            serde_json::json!({
                "live_worker_states": states,
                "hosted_pane_statuses": hosted,
            })
        );
        return Ok(());
    }

    if states.is_empty() {
        println!("no active workers");
    } else {
        for state in &states {
            print_live_state_short(state);
        }
    }
    if all {
        // Panes already shown above via `states` are `Live`-classified here
        // too — only render the ones the primary live list can't show:
        // a worker the engine lost live-track of but durable state still
        // corroborates (`LiveProcessNoRegistry`), and true husks.
        let additional: Vec<&HostedPaneStatus> = hosted
            .iter()
            .filter(|p| !matches!(p.state, HostedPaneState::Live))
            .collect();
        if additional.is_empty() {
            println!("no additional hosted panes (no terminal-entry-with-live-process panes or husks)");
        } else {
            for pane in &additional {
                match &pane.state {
                    HostedPaneState::LiveProcessNoRegistry { evidence } => println!(
                        "slot {}  {}  run={}  TERMINAL ENTRY, LIVE PROCESS ({evidence}) — \
                         `bossctl agents stop {}` or `bossctl agents retire-pane {}` to reap it",
                        pane.slot_id, pane.crew_name, pane.run_id, pane.run_id, pane.slot_id,
                    ),
                    HostedPaneState::Husk => println!(
                        "slot {}  {}  run={}  HUSK (app-hosted, no engine-tracked run, no live process — \
                         retire with `bossctl agents retire-pane {}`)",
                        pane.slot_id, pane.crew_name, pane.run_id, pane.slot_id,
                    ),
                    HostedPaneState::Live => unreachable!("filtered out above"),
                }
            }
        }
    }
    Ok(())
}

/// Resolve `agent` (run id, slot id, or crew name) to the slot to retire.
/// A bare numeric reference is always accepted directly as a slot id with
/// no lookup — retire-pane's break-glass contract has never required the
/// slot to be resolvable client-side (the engine handles an unknown or
/// never-allocated slot idempotently), and preserving that means a slot
/// invisible to both the live registry and the hosted-pane roster (e.g.
/// the app itself is unreachable) can still be targeted by number. A crew
/// name or run id, which are never bare numbers, always goes through
/// resolution against the live registry and then the durable/hosted-pane
/// roster — the fix this verb exists for.
async fn resolve_retire_pane_slot(client: &mut BossClient, agent: &str) -> Result<u8> {
    if let Ok(slot) = agent.parse::<u8>() {
        return Ok(slot);
    }
    let states = fetch_live_states(client).await?;
    if let Ok(state) = resolve_agent_ref(agent, &states) {
        return Ok(state.slot_id);
    }
    let hosted = fetch_hosted_pane_statuses(client).await?;
    if let Some(pane) = resolve_hosted_pane_ref(agent, &hosted)? {
        return Ok(pane.slot_id);
    }
    Err(no_worker_matches_error(agent, &states, &hosted))
}

pub(crate) async fn agents_retire_pane(socket_path: &Option<String>, json: bool, agent: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let slot_id = resolve_retire_pane_slot(&mut client, &agent).await?;
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

/// True when `reference` has the shape of an execution id (`exec_…`).
///
/// Used by `agents stop`/`agents reap` to decide whether a resolver miss
/// should still be forwarded to the engine raw — see [`agents_stop`] for
/// why. Deliberately narrow: a crew name or slot number that misses must
/// still fail loudly with the candidate list rather than being posted to
/// the engine as a run id.
fn looks_like_execution_id(reference: &str) -> bool {
    reference.starts_with("exec_")
}

/// Stop the worker referenced by `agent`.
///
/// Resolution goes through the live-worker list, then the engine's
/// durable/hosted-pane roster (`resolve_agent_ref_or_work_item`) — which
/// covers a worker the engine has LOST TRACK of. That is not a
/// hypothetical: a run whose execution was wrongly terminalized has its
/// `LiveWorkerState` cleared while its process keeps running, so on
/// 2026-07-28 `bossctl agents stop <exec-id>` answered `no live worker
/// matches` for all six stranded workers and an operator had to `kill`
/// them by pid. Being unable to reap a running worker is its own defect,
/// independent of how it got stranded.
///
/// As a last resort, an `exec_…` reference that misses BOTH the live list
/// and the durable/hosted-pane roster (a pane already torn down
/// everywhere but the DB row) is forwarded to the engine raw. The
/// engine's stop path resolves the worker from durable state
/// (`work_runs.shell_pid`) and reaps both the app-hosted pane and the OS
/// process tree. Other reference forms (crew name, slot id) still fail
/// with the candidate list — a typo'd name must not be posted to the
/// engine as a run id.
pub(crate) async fn agents_stop(socket_path: &Option<String>, json: bool, agent: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;
    let run_id = match resolve_agent_ref_or_work_item(&mut client, &agent, &states).await {
        Ok(run_id) => run_id,
        Err(_) if looks_like_execution_id(&agent) => {
            eprintln!(
                "warning: {agent} is not tracked live or in durable pane state; forwarding it to the \
                 engine as a raw run id anyway (this is the shape a pane torn down everywhere but its \
                 own DB row takes)"
            );
            agent.clone()
        }
        Err(err) => return Err(err),
    };
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
    host: Option<String>,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::RequestExecution {
            input: RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .maybe_preferred_workspace_id(preferred_workspace_id)
                .maybe_requested_host_id(host)
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
    host: Option<String>,
    force: bool,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let input = RequestExecutionInput::builder()
        .work_item_id(work_item_id.clone())
        .maybe_priority(priority)
        .maybe_preferred_workspace_id(preferred_workspace_id)
        .maybe_requested_host_id(host)
        .bypass_dispatch_pause(force)
        .maybe_entry_point(force.then_some(DispatchAdmissionEntryPoint::Cli))
        .build();
    let response = client
        .send_request(&FrontendRequest::RequestExecution { input })
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
            // Surface the engine's refusal reason in both human (stderr,
            // via `main`'s `bail!` propagation) and `--json` output — a
            // forced refusal names the specific non-overridable blocker
            // (interactive cap, unmet dependency, ineligible status, or a
            // non-overridable breaker pause) rather than silently no-op'ing.
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "refused",
                        "work_item_id": work_item_id,
                        "forced": force,
                        "reason": message,
                    })
                );
            }
            bail!("engine rejected work start: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

pub(crate) async fn work_cancel(socket_path: &Option<String>, json: bool, execution_id: String) -> Result<()> {
    // Broad cancel: any non-terminal row (including running). For
    // never-started rows only, prefer `executions_cancel` /
    // `bossctl executions cancel`, which refuses live workers and
    // records an operator reason.
    cancel_execution_rpc(
        socket_path,
        json,
        execution_id,
        /* reason */ None,
        /* queued_only */ false,
        "work cancel",
    )
    .await
}

/// `bossctl executions cancel` — cancel never-started (`queued` /
/// `ready` / `dispatching` / `waiting_dependency`) executions only.
/// Refuses live / mid-flight rows so operators don't confuse this with
/// `agents stop`.
///
/// `execution_id` and `work_item_id` are mutually exclusive selectors:
/// by work item cancels every never-started execution currently on that
/// row (the usual "this item's queued work is moot" operator path).
pub(crate) async fn executions_cancel(
    socket_path: &Option<String>,
    json: bool,
    execution_id: Option<String>,
    work_item_id: Option<String>,
    reason: Option<String>,
) -> Result<()> {
    match (execution_id, work_item_id) {
        (Some(execution_id), None) => {
            cancel_execution_rpc(
                socket_path,
                json,
                execution_id,
                reason,
                /* queued_only */ true,
                "executions cancel",
            )
            .await
        }
        (None, Some(work_item_id)) => executions_cancel_for_work_item(socket_path, json, work_item_id, reason).await,
        (Some(_), Some(_)) => {
            bail!("pass either an execution id or --work-item, not both")
        }
        (None, None) => {
            bail!("pass an execution id or --work-item <id>")
        }
    }
}

async fn executions_cancel_for_work_item(
    socket_path: &Option<String>,
    json: bool,
    work_item_id: String,
    reason: Option<String>,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    // Resolve friendly short ids (`T42`) to the canonical task id —
    // `ListExecutions` filters on the primary key and does not do this
    // itself. Mirrors `GetWorkItem`'s resolving contract.
    let resolved_work_item_id = {
        let response = client
            .send_request(&FrontendRequest::GetWorkItem {
                id: work_item_id.clone(),
            })
            .await
            .context("sending GetWorkItem")?;
        match response {
            FrontendEvent::WorkItemResult { item } => item.primary_id().to_owned(),
            FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
                bail!("engine rejected work-item lookup for {work_item_id}: {message}")
            }
            other => bail!("engine returned unexpected response: {other:?}"),
        }
    };
    let response = client
        .send_request(&FrontendRequest::ListExecutions {
            work_item_id: Some(resolved_work_item_id.clone()),
            include_revision_chain: false,
        })
        .await
        .context("sending ListExecutions")?;
    let executions = match response {
        FrontendEvent::ExecutionsList { executions, .. } => executions,
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected list executions: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    };
    // Prefer the operator-supplied label in user-facing output, but use
    // the resolved id for RPC filters above.
    let work_item_id = resolved_work_item_id;
    // Never-started only — same gate the engine enforces under
    // `queued_only`. Filter client-side so we don't spam WorkErrors for
    // every historical terminal/running row on the item.
    let candidates: Vec<_> = executions
        .into_iter()
        .filter(|e| e.status.can_reconcile() || e.status == ExecutionStatus::Dispatching)
        .collect();
    if candidates.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "work_item_id": work_item_id,
                    "cancelled": [],
                    "count": 0,
                })
            );
        } else {
            println!("no never-started (queued/ready/dispatching/waiting_dependency) executions for {work_item_id}");
        }
        return Ok(());
    }

    let mut cancelled = Vec::new();
    for exec in candidates {
        let response = client
            .send_request(&FrontendRequest::CancelExecution {
                execution_id: exec.id.clone(),
                reason: reason.clone(),
                queued_only: true,
            })
            .await
            .with_context(|| format!("sending CancelExecution for {}", exec.id))?;
        match response {
            FrontendEvent::ExecutionCancelled { execution } => {
                cancelled.push(execution);
            }
            FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
                bail!("engine rejected executions cancel for {}: {message}", exec.id)
            }
            other => bail!("engine returned unexpected response: {other:?}"),
        }
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "work_item_id": work_item_id,
                "cancelled": cancelled,
                "count": cancelled.len(),
            })
        );
    } else {
        println!(
            "cancelled {} never-started execution(s) for {work_item_id}:",
            cancelled.len()
        );
        for execution in &cancelled {
            println!("  {} ({})", execution.id, execution.status);
        }
    }
    Ok(())
}

async fn cancel_execution_rpc(
    socket_path: &Option<String>,
    json: bool,
    execution_id: String,
    reason: Option<String>,
    queued_only: bool,
    verb_label: &str,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::CancelExecution {
            execution_id: execution_id.clone(),
            reason,
            queued_only,
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
            bail!("engine rejected {verb_label}: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// Parse a transcript tail (`joined` JSONL lines) into normalized events.
///
/// Tries the raw Claude Code / Codex rollout schema first — both dialects'
/// raw on-disk records are directly schema-detectable, so this succeeds for
/// them without touching `driver_slug`. When that fails (e.g. Grok's ACP
/// `session/update` envelope, which carries no top-level `type` field at
/// all), falls back to reshaping through the run's own driver — the same
/// normalizer `driver_transcript::parse_execution_transcript` already uses
/// for the app UI's transcript viewer and the Stop-boundary marker scans —
/// before parsing again. `driver_slug` unresolvable (unknown/missing driver)
/// surfaces the original schema error rather than silently rendering empty.
fn parse_transcript_tail_events(
    joined: &str,
    driver_slug: Option<&str>,
    transcript_path: &str,
) -> Result<Vec<boss_engine::transcript_markdown::TranscriptEvent>> {
    match boss_engine::transcript_markdown::parse_transcript_checked(joined) {
        Ok(events) => Ok(events),
        Err(err) => {
            let normalizer =
                driver_slug.and_then(|slug| boss_engine::driver::DriverRegistry::default().require(slug).ok());
            match normalizer {
                Some(driver) => {
                    let events =
                        boss_engine::driver_transcript::parse_transcript_with_driver(Some(driver.as_ref()), joined);
                    let has_content = joined.lines().any(|line| !line.trim().is_empty());
                    if events.is_empty() && has_content {
                        Err(anyhow::anyhow!("{err}")).with_context(|| format!("rendering transcript {transcript_path}"))
                    } else {
                        Ok(events)
                    }
                }
                None => {
                    Err(anyhow::anyhow!("{err}")).with_context(|| format!("rendering transcript {transcript_path}"))
                }
            }
        }
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

    // For live workers resolve via the registry. For a worker the engine has
    // lost live-track of but durable state / the app still account for,
    // resolve via the hosted-pane roster — this needs no liveness at all,
    // since TailRunTranscript reads `work_runs.transcript_path` from the DB
    // regardless of whether the process is still running. For completed/
    // terminal executions absent from both, fall through and let the engine
    // query the DB directly. The engine's resolve_transcript_for_tail
    // handles both the exec_* and run_* namespaces, so passing the raw ref
    // works for either id form. Friendly ids are tried as live-worker
    // references after the live registry and hosted-pane roster, while
    // slot-shaped references and crew names remain worker-resolution errors.
    let run_id = match resolve_agent_ref(&agent, &states) {
        Ok(state) => state.run_id.clone(),
        Err(err) => {
            let hosted = fetch_hosted_pane_statuses(&mut client).await?;
            if let Some(pane) = resolve_hosted_pane_ref(&agent, &hosted)? {
                pane.run_id.clone()
            } else if !work_item_fallback_eligible(&agent) {
                return Err(err);
            } else if let Some(state) = resolve_tnnn_to_live_worker(&mut client, &agent, &states).await? {
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
            driver,
        } => {
            let render_opts = boss_engine::transcript_markdown::RenderOpts {
                hide_tools: no_tools,
                ..Default::default()
            };
            if format == TranscriptFormat::Text || format == TranscriptFormat::Markdown {
                let joined = tail.join("\n");
                let events = parse_transcript_tail_events(&joined, driver.as_deref(), &transcript_path)?;
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

/// Mark the execution behind `agent` as `orphaned` (terminal) without
/// releasing its cube workspace lease.
///
/// Resolves `agent` (run id, slot id, or crew name) the same way every
/// other `agents` verb does: the live-worker list, then the engine's
/// durable/hosted-pane roster — the fix this whole change is about, since
/// `reap` exists precisely for a worker the live registry has already
/// lost. As a last resort, a reference that misses both but has the
/// shape of a run/execution id is forwarded to the engine raw (mirrors
/// `agents stop`) — a pane already torn down everywhere but its own DB
/// row has no name or slot left to resolve by.
pub(crate) async fn agents_reap(socket_path: &Option<String>, json: bool, agent: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;
    let run_id = match resolve_agent_ref_or_work_item(&mut client, &agent, &states).await {
        Ok(run_id) => run_id,
        Err(_) if looks_like_execution_id(&agent) => agent.clone(),
        Err(err) => return Err(err),
    };
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
                    let cap_suffix = pool
                        .effective_cap
                        .map(|cap| format!("  [concurrency cap: {cap}]"))
                        .unwrap_or_default();
                    println!(
                        "{}: {}/{} claimed ({} idle){}",
                        pool.name,
                        pool.claims.len(),
                        pool.capacity,
                        pool.idle,
                        cap_suffix,
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
    // Attributed pool + execution kind (stamped at spawn). Shown so
    // `agents status`/`list` can diagnose pool routing without joining
    // the execution table — independent of physical slot occupancy
    // (spilled automation still reports pool=automation).
    if let Some(pool) = &state.pool {
        println!("  pool:          {pool}");
    }
    if let Some(kind) = &state.kind {
        println!("  kind:          {kind}");
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
    println!("{}", format_live_state_short(state));
}

/// Render a [`HostedPaneStatus`] resolved from the durable/hosted-pane
/// roster — reached only when the live-registry lookup already missed
/// (see [`agents_status`]), so this covers exactly the case the live
/// `LiveWorkerState` detail (model, activity, current tool) isn't
/// available for.
fn print_hosted_pane_status(json: bool, pane: &HostedPaneStatus) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "slot_id": pane.slot_id,
                "run_id": pane.run_id,
                "crew_name": pane.crew_name,
                "state": pane.state,
                "summary": pane.summary,
                "task_title": pane.task_title,
            })
        );
        return;
    }
    println!("slot {} ({}) — run {}", pane.slot_id, pane.crew_name, pane.run_id);
    match &pane.state {
        HostedPaneState::Live => {
            // Reachable only if the worker went live in the gap between the
            // `ListWorkerLiveStates` and `ListHostedPaneStatuses` round
            // trips — vanishingly rare, and harmless: the caller just
            // re-runs the command a moment later for full live detail.
            println!("  state: live (just resolved via durable state; re-run for full live detail)");
        }
        HostedPaneState::LiveProcessNoRegistry { evidence } => {
            println!("  state: terminal entry, live process ({evidence})");
            println!("  the engine has no live run tracked for this worker, but its process is still running.");
            println!(
                "  `bossctl agents stop {}` or `bossctl agents retire-pane {}` will reap it.",
                pane.run_id, pane.slot_id
            );
        }
        HostedPaneState::Husk => {
            println!("  state: husk (app-hosted, no engine-tracked run, no live process)");
            println!("  retire with `bossctl agents retire-pane {}`", pane.slot_id);
        }
    }
    if let Some(title) = &pane.task_title {
        println!("  work_item:     {title}");
    }
    if let Some(summary) = &pane.summary {
        println!("  summary:       {summary}");
    }
}

/// One-line `agents list` row for a live worker. Pure so tests can pin
/// the pool + exec-kind columns without capturing stdout.
fn format_live_state_short(state: &LiveWorkerState) -> String {
    let tool = state.current_tool.as_deref().unwrap_or("-");
    let work_item = state.work_item_id.as_deref().unwrap_or("-");
    let work_item_name = state.work_item_name.as_deref().unwrap_or("-");
    // `pool` / `kind` always print (as `-` when unset) so a glance at
    // `agents list` shows attributed routing even for test / direct-
    // launch spawns that never stamped them. Values match
    // `LiveWorkerState::{pool,kind}` — `"main"`/`"automation"`/`"review"`
    // and e.g. `"task_implementation"` / `"pr_review"`.
    let pool = state.pool.as_deref().unwrap_or("-");
    let kind = state.kind.as_deref().unwrap_or("-");
    let mut line = format!(
        "slot {}  name={}  run={}  model={}  activity={}  pool={}  kind={}  tool={}  work_item={}  work_item_name=\"{}\"",
        state.slot_id,
        state.name,
        state.run_id,
        state.model,
        state.activity.as_str(),
        pool,
        kind,
        tool,
        work_item,
        work_item_name,
    );
    // Surfaced whenever the transient-recovery sweep is actively nudging
    // this slot — without this an auto-recovering worker prints as plain
    // `activity=idle`, indistinguishable from a normally-finished turn.
    if let Some(recovery) = &state.recovery_status {
        line.push_str(&format!("  recovery=\"{recovery}\""));
    }
    if state.held {
        line.push_str("  held=true");
    }
    line
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

    #[test]
    fn format_live_state_short_shows_pool_and_kind_dashes_when_unset() {
        let state = worker(3, "run_a", "Riker");
        let line = format_live_state_short(&state);
        assert!(
            line.contains("pool=-") && line.contains("kind=-"),
            "expected pool=- and kind=- placeholders when unset: {line}"
        );
        assert!(
            line.starts_with("slot 3  name=Riker  run=run_a  model=opus  activity=spawning  pool=-  kind=-"),
            "unexpected column order: {line}"
        );
    }

    #[test]
    fn format_live_state_short_renders_stamped_pool_and_kind() {
        let mut state = worker(5, "run_b", "Data");
        state.pool = Some("automation".to_owned());
        state.kind = Some("automation_triage".to_owned());
        state.held = true;
        let line = format_live_state_short(&state);
        assert!(
            line.contains("pool=automation") && line.contains("kind=automation_triage"),
            "expected stamped pool+kind: {line}"
        );
        assert!(
            line.ends_with("held=true"),
            "held suffix should still trail the row: {line}"
        );
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

    // ---- agents stop fallback ---------------------------------------------

    /// `agents stop` must remain usable for a worker the engine has lost track
    /// of — the shape whose `LiveWorkerState` was cleared while its process
    /// kept running. An `exec_…` selector that misses the live list is the
    /// signal that lets `agents_stop` forward it to the engine anyway rather
    /// than reporting `no live worker matches` and leaving the operator to
    /// `kill` by pid.
    #[test]
    fn execution_ids_are_recognised_for_the_untracked_stop_fallback() {
        assert!(looks_like_execution_id("exec_18c6a6add38b5fe0_97"));
        assert!(looks_like_execution_id("exec_"));
    }

    /// The fallback must stay narrow. A crew name or slot number that misses
    /// is far more likely a typo than a stranded worker, and forwarding it to
    /// the engine as a run id would turn a clear error into a silent no-op.
    #[test]
    fn names_and_slots_do_not_get_the_untracked_stop_fallback() {
        assert!(!looks_like_execution_id("Worf"));
        assert!(!looks_like_execution_id("3"));
        assert!(!looks_like_execution_id("markdown-striping"));
        assert!(!looks_like_execution_id("task_abc"));
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

    // ---- resolve_hosted_pane_ref / no_worker_matches_error -----------------

    fn hosted_pane(slot_id: u8, run_id: &str, crew_name: &str, state: HostedPaneState) -> HostedPaneStatus {
        HostedPaneStatus {
            slot_id,
            run_id: run_id.to_owned(),
            crew_name: crew_name.to_owned(),
            summary: None,
            task_title: None,
            state,
        }
    }

    /// The exact incident shape: a crew name the operator can see in the
    /// app, backing a run the live registry has dropped entirely, must
    /// still resolve — by name, by slot, and by run id.
    #[test]
    fn resolves_terminal_entry_live_process_pane_by_name_slot_and_run_id() {
        let panes = [hosted_pane(
            1,
            "exec_riker",
            "Riker",
            HostedPaneState::LiveProcessNoRegistry {
                evidence: "durably-recorded shell pid 33112 is alive".to_owned(),
            },
        )];
        let by_name = resolve_hosted_pane_ref("riker", &panes)
            .expect("no ambiguity")
            .expect("name should resolve");
        assert_eq!(by_name.run_id, "exec_riker");
        let by_slot = resolve_hosted_pane_ref("1", &panes)
            .expect("no ambiguity")
            .expect("slot should resolve");
        assert_eq!(by_slot.crew_name, "Riker");
        let by_run = resolve_hosted_pane_ref("exec_riker", &panes)
            .expect("no ambiguity")
            .expect("run id should resolve");
        assert_eq!(by_run.slot_id, 1);
    }

    #[test]
    fn resolve_hosted_pane_ref_misses_return_none_not_error() {
        let panes = [hosted_pane(1, "exec_riker", "Riker", HostedPaneState::Husk)];
        assert!(
            resolve_hosted_pane_ref("nonesuch", &panes)
                .expect("a miss is not an error")
                .is_none()
        );
    }

    #[test]
    fn resolve_hosted_pane_ref_bails_on_ambiguous_name() {
        let panes = [
            hosted_pane(1, "exec_a", "Data", HostedPaneState::Husk),
            hosted_pane(2, "exec_b", "Data", HostedPaneState::Husk),
        ];
        let err = resolve_hosted_pane_ref("data", &panes).expect_err("two panes share a name");
        let msg = err.to_string();
        assert!(msg.contains("matches multiple tracked panes"), "message was: {msg}");
    }

    /// Requirement: a total miss must list what was searched, including
    /// the non-live candidates — not only the live crew.
    #[test]
    fn no_worker_matches_error_lists_non_live_candidates_too() {
        let states = [worker(2, "exec_data", "Data")];
        let hosted = [
            hosted_pane(2, "exec_data", "Data", HostedPaneState::Live),
            hosted_pane(
                1,
                "exec_riker",
                "Riker",
                HostedPaneState::LiveProcessNoRegistry {
                    evidence: "durably-recorded shell pid 33112 is alive".to_owned(),
                },
            ),
            hosted_pane(3, "exec_worf", "Worf", HostedPaneState::Husk),
        ];
        let err = no_worker_matches_error("nonesuch", &states, &hosted);
        let msg = err.to_string();
        assert!(msg.contains("Live: slot 2 (Data)"), "message was: {msg}");
        assert!(msg.contains("Also tracked (not live)"), "message was: {msg}");
        assert!(
            msg.contains("slot 1 (Riker) run exec_riker [terminal entry, live process]"),
            "message was: {msg}"
        );
        assert!(msg.contains("slot 3 (Worf) run exec_worf [husk]"), "message was: {msg}");
        // The Live-classified pane is not double-listed in the "not live" tail.
        assert!(!msg.contains("slot 2 (Data) run exec_data ["), "message was: {msg}");
    }

    #[test]
    fn no_worker_matches_error_omits_tail_when_nothing_non_live() {
        let err = no_worker_matches_error("nonesuch", &[], &[]);
        assert_eq!(err.to_string(), "no worker matches `nonesuch`. no live workers");
    }

    // ---- looks_like_name_or_slot ------------------------------------------

    #[test]
    fn numeric_slot_looks_like_name_or_slot() {
        assert!(looks_like_name_or_slot("5"));
        assert!(looks_like_name_or_slot("0"));
        assert!(looks_like_name_or_slot("300"));
    }

    #[test]
    fn roster_name_looks_like_name_or_slot_case_insensitive() {
        assert!(looks_like_name_or_slot("Riker"));
        assert!(looks_like_name_or_slot("riker"));
        assert!(looks_like_name_or_slot("LA FORGE"));
    }

    #[test]
    fn non_slot_references_do_not_look_like_name_or_slot() {
        assert!(!looks_like_name_or_slot("exec_18ad9f"));
        assert!(!looks_like_name_or_slot("Picard"));
    }

    // ---- work_item_fallback_eligible --------------------------------------

    #[test]
    fn work_item_fallback_excludes_slot_shaped_references_and_crew_names() {
        assert!(!work_item_fallback_eligible("1"));
        assert!(!work_item_fallback_eligible("300"));
        assert!(!work_item_fallback_eligible("riker"));
    }

    #[test]
    fn work_item_fallback_allows_work_item_and_execution_references() {
        assert!(work_item_fallback_eligible("exec_18ad9f"));
        assert!(work_item_fallback_eligible("T1"));
        assert!(work_item_fallback_eligible("task_123"));
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

    // ---- parse_transcript_tail_events --------------------------------------

    use boss_engine::transcript_markdown::TranscriptEventKind;

    fn assistant_texts(events: &[boss_engine::transcript_markdown::TranscriptEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match &event.kind {
                TranscriptEventKind::AssistantText(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn parse_transcript_tail_events_reads_claude_dialect_without_a_driver() {
        let jsonl = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#;
        let events = parse_transcript_tail_events(jsonl, None, "/tmp/t.jsonl").expect("claude dialect must parse");
        assert_eq!(assistant_texts(&events), vec!["hi".to_owned()]);
    }

    #[test]
    fn parse_transcript_tail_events_falls_back_to_the_grok_driver_for_acp_records() {
        // Grok's raw ACP `session/update` envelope carries no top-level
        // `type` field, so the direct schema check must fail before the
        // driver fallback kicks in — this is the exact shape
        // `bossctl agents transcript` received for a Grok run and rendered
        // nothing for.
        let jsonl = concat!(
            r#"{"method":"session/update","params":{"sessionId":"s1","update":{"#,
            r#""sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"[blocked] reason=\"needs a decision\""}}}}"#,
        );
        assert!(
            boss_engine::transcript_markdown::parse_transcript_checked(jsonl).is_err(),
            "raw ACP content must not match the Claude/Codex schema directly"
        );
        let events =
            parse_transcript_tail_events(jsonl, Some("grok"), "/tmp/t.jsonl").expect("grok driver must normalize");
        assert!(
            assistant_texts(&events)
                .iter()
                .any(|text| text.contains("[blocked] reason=\"needs a decision\"")),
            "grok ACP prose must survive the driver-aware fallback parse; got {events:?}",
        );
    }

    #[test]
    fn parse_transcript_tail_events_errors_when_driver_resolves_but_cannot_normalize_content() {
        // A resolvable driver slug that still can't make sense of the tail
        // content (e.g. a Grok tail window landing entirely on a
        // `session_update` variant `parse_acp_envelope` maps to `Unknown`,
        // or any other non-empty content the driver silently drops) must
        // not swallow the original schema error into an empty transcript —
        // the doc comment on `parse_transcript_tail_events` promises the
        // opposite, and this is the exact "renders nothing" symptom this
        // module set out to fix.
        let jsonl = concat!(
            r#"{"method":"session/update","params":{"sessionId":"s1","update":{"#,
            r#""sessionUpdate":"totally_unknown_variant","foo":"bar"}}}"#,
        );
        assert!(
            boss_engine::transcript_markdown::parse_transcript_checked(jsonl).is_err(),
            "raw ACP content must not match the Claude/Codex schema directly"
        );
        let err = parse_transcript_tail_events(jsonl, Some("grok"), "/tmp/t.jsonl")
            .expect_err("a resolvable driver that normalizes to nothing must still surface the schema error");
        assert!(err.to_string().contains("rendering transcript"), "got: {err}");
    }

    #[test]
    fn parse_transcript_tail_events_errors_when_no_driver_can_normalize_unknown_content() {
        let jsonl = r#"{"totally":"unrecognised"}"#;
        let err = parse_transcript_tail_events(jsonl, None, "/tmp/t.jsonl")
            .expect_err("unrecognised content with no driver to fall back on must still error");
        assert!(err.to_string().contains("rendering transcript"));
    }

    #[test]
    fn parse_transcript_tail_events_grok_joins_tool_call_and_surfaces_hook_events() {
        // A `tool_call` / `tool_call_update` pair only joins into one
        // `ToolUse` (no spurious filler for the update line) because
        // `parse_transcript_with_driver` builds a stateful
        // `GrokTranscriptSession` for the whole tail — an isolated
        // per-line normalize (a regression back to
        // `normalize_transcript_entry`) would instead see the update in
        // isolation, fail the `toolCallId` correlation, and emit an
        // extra `{"type":"system"}` filler event with no subtype. A
        // `hook_execution` record must also surface as a system event,
        // per the brief's requirement that hook events render alongside
        // assistant text and tool calls/results.
        let jsonl = concat!(
            r#"{"method":"session/update","params":{"sessionId":"s1","update":{"#,
            r#""sessionUpdate":"tool_call","toolCallId":"call-1","title":"run_terminal_command","rawInput":{"command":"echo hi"}}}}"#,
            "\n",
            r#"{"method":"session/update","params":{"sessionId":"s1","update":{"#,
            r#""sessionUpdate":"tool_call_update","toolCallId":"call-1","rawOutput":{"exit_code":0,"output":"hi"}}}}"#,
            "\n",
            r#"{"method":"session/update","params":{"sessionId":"s1","update":{"#,
            r#""sessionUpdate":"hook_execution","event_name":"Stop"}}}"#,
        );
        let events =
            parse_transcript_tail_events(jsonl, Some("grok"), "/tmp/t.jsonl").expect("grok driver must normalize");

        let tool_uses: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                TranscriptEventKind::ToolUse { name, input } => Some((name.as_str(), input)),
                _ => None,
            })
            .collect();
        assert_eq!(
            tool_uses.len(),
            1,
            "expected exactly one joined tool call; got {events:?}"
        );
        assert_eq!(tool_uses[0].0, "Bash");
        assert_eq!(tool_uses[0].1, &serde_json::json!({"command": "echo hi"}));

        let system_events: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                TranscriptEventKind::System { subtype, .. } => Some(subtype.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            system_events,
            vec![Some("hook_execution".to_owned())],
            "the tool_call_update must join silently (no unmatched filler) and the hook \
             execution must be the only system event; got {events:?}",
        );

        let render_opts = boss_engine::transcript_markdown::RenderOpts {
            hide_tools: true,
            ..Default::default()
        };
        let segments = boss_engine::transcript_markdown::events_to_segments(&events, &render_opts);
        assert!(
            segments
                .iter()
                .all(|segment| segment.role != boss_engine::transcript_markdown::SegmentRole::Tool),
            "--no-tools must drop the Grok tool segment exactly as it does for Claude; got {segments:?}",
        );
    }

    #[test]
    fn parse_transcript_tail_events_errors_when_driver_slug_is_unknown() {
        let jsonl = r#"{"totally":"unrecognised"}"#;
        let err = parse_transcript_tail_events(jsonl, Some("not-a-real-driver"), "/tmp/t.jsonl")
            .expect_err("an unresolvable driver slug must not swallow the schema error");
        assert!(err.to_string().contains("rendering transcript"));
    }
}
