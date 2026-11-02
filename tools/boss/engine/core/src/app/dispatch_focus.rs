//! `GetDispatchFocus` / `UpdateDispatchFocus`: the dispatch focus set behind
//! `bossctl dispatch focus`. The set is persisted in the metadata KV (so it
//! survives an engine restart) and mirrored into the coordinator, whose
//! `drain_ready_queue` applies it as a pure admission gate. Nothing here
//! touches a work item.

use boss_protocol::{DispatchFocus, DispatchFocusAction, DispatchFocusState};

use super::*;

/// Build the reply from the coordinator's current set plus a live count of
/// the ready rows it holds back.
fn focus_state(server_state: &ServerState, work_db: &WorkDb) -> Result<DispatchFocusState> {
    let coordinator = &server_state.execution_coordinator;
    let focus = coordinator.dispatch_focus();
    Ok(DispatchFocusState {
        active: focus.is_active(),
        targets: work_db.describe_dispatch_focus(&focus)?,
        held_count: coordinator.focus_held_count()?,
    })
}

/// Seed the coordinator from the persisted set at startup. An unreadable
/// value is logged loudly and treated as no focus: the set cannot be
/// recovered from it, and holding every row until someone notices would be
/// the worse failure.
pub(super) fn restore_persisted_dispatch_focus(server_state: &ServerState) {
    match server_state.work_db.load_dispatch_focus() {
        Ok(focus) if focus.is_active() => {
            tracing::info!(
                projects = focus.project_ids.len(),
                products = focus.product_ids.len(),
                "dispatch: restoring persisted dispatch focus from state.db — dispatch stays restricted \
                 to the focus set until `bossctl dispatch focus clear`",
            );
            server_state.execution_coordinator.restore_dispatch_focus(focus);
        }
        Ok(_) => {}
        Err(err) => tracing::error!(
            ?err,
            "dispatch: persisted dispatch focus is unreadable — starting with focus mode off",
        ),
    }
}

pub(super) async fn handle_get_dispatch_focus(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetDispatchFocus = req else {
        unreachable!()
    };
    match focus_state(&server_state, &work_db) {
        Ok(focus) => send_response(&sink, &request_id, FrontendEvent::DispatchFocusResult { focus }),
        Err(err) => send_work_error(&sink, &request_id, format!("{err:#}")),
    }
}

/// Apply `action` to `current`, resolving every target first so one bad
/// target rejects the whole request with nothing changed.
fn apply_focus_action(
    work_db: &WorkDb,
    current: &DispatchFocus,
    action: DispatchFocusAction,
    targets: &[String],
) -> Result<DispatchFocus> {
    if action == DispatchFocusAction::Clear {
        anyhow::ensure!(targets.is_empty(), "`clear` takes no targets");
        return Ok(DispatchFocus::default());
    }
    anyhow::ensure!(!targets.is_empty(), "at least one project or product is required");
    let mut resolved = Vec::with_capacity(targets.len());
    for raw in targets {
        let raw = raw.trim();
        // Removing an id that is already in the set must work even after
        // its row is gone, or a deleted project could never be dropped.
        if action == DispatchFocusAction::Remove {
            if current.project_ids.contains(raw) {
                resolved.push((boss_protocol::DispatchFocusTargetKind::Project, raw.to_owned()));
                continue;
            }
            if current.product_ids.contains(raw) {
                resolved.push((boss_protocol::DispatchFocusTargetKind::Product, raw.to_owned()));
                continue;
            }
        }
        resolved.push(work_db.resolve_dispatch_focus_target(raw)?);
    }
    let mut next = match action {
        DispatchFocusAction::Set => DispatchFocus::default(),
        _ => current.clone(),
    };
    for (kind, id) in resolved {
        match action {
            DispatchFocusAction::Remove => next.remove(kind, &id),
            _ => next.insert(kind, id),
        }
    }
    Ok(next)
}

pub(super) async fn handle_update_dispatch_focus(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::UpdateDispatchFocus { action, targets } = req else {
        unreachable!()
    };
    let coordinator = &server_state.execution_coordinator;
    let next = match apply_focus_action(&work_db, &coordinator.dispatch_focus(), action, &targets) {
        Ok(next) => next,
        Err(err) => {
            send_work_error(&sink, &request_id, format!("{err:#}"));
            return;
        }
    };
    // Persist before applying: a focus that silently reverts on restart would
    // dispatch everything the operator meant to hold back, so a failed write
    // fails the request instead of leaving memory and disk disagreeing.
    if let Err(err) = work_db.save_dispatch_focus(&next) {
        send_work_error(
            &sink,
            &request_id,
            format!("could not persist the dispatch focus, nothing changed: {err:#}"),
        );
        return;
    }
    tracing::info!(
        ?action,
        projects = next.project_ids.len(),
        products = next.product_ids.len(),
        "dispatch: focus set updated",
    );
    coordinator.set_dispatch_focus(next);
    // The board's queue positions mirror the drain's gates, focus included.
    server_state.republish_queued_dispatch_if_changed().await;
    match focus_state(&server_state, &work_db) {
        Ok(focus) => send_response(&sink, &request_id, FrontendEvent::DispatchFocusResult { focus }),
        Err(err) => send_work_error(&sink, &request_id, format!("{err:#}")),
    }
}
