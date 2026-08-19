//! `FrontendRequest` handlers — engine version/health, feature flags, settings, misc.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

use crate::protocol::PauseReason;

pub(super) async fn handle_workspace_pool_summary(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::WorkspacePoolSummary = req else {
        unreachable!()
    };
    {
        // Read-only view of `cube workspace list` plus engine
        // annotations. The coordinator contract documents this
        // as a bossctl verb, and any user who can run `cube
        // workspace list` directly already has the same view
        // — so an extra subtree gate buys no security and just
        // breaks legitimate calls (the live coordinator
        // session repro: bossctl invoked from a shell that's
        // neither an app nor a Boss descendant fell through
        // AppOrBoss). User tier is the right level.
        if !server_state.authorize_rpc(RpcTier::User, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                "workspace_pool_summary rejected: caller failed user tier",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "workspace_pool_summary failed user-tier check".to_owned(),
                },
            );
            return;
        }
        match server_state.cube_client.list_workspaces().await {
            Ok(rows) => {
                // Annotate each entry with the engine's view: which
                // execution row (if any) currently records this
                // workspace's lease. Drift (cube reports a lease the
                // engine has no execution for) shows as `None`.
                let lease_to_execution = match server_state.work_db.lease_to_execution_map() {
                    Ok(map) => map,
                    Err(err) => {
                        tracing::warn!(
                            ?err,
                            "workspace_pool_summary: lease lookup failed; emitting cube view only",
                        );
                        std::collections::HashMap::new()
                    }
                };
                let workspaces = rows
                    .into_iter()
                    .map(|w| {
                        let execution_id = w
                            .lease_id
                            .as_ref()
                            .and_then(|lease_id| lease_to_execution.get(lease_id).cloned());
                        crate::protocol::WorkspacePoolEntry {
                            workspace_id: w.workspace_id,
                            workspace_path: w.workspace_path.display().to_string(),
                            state: w.state,
                            lease_id: w.lease_id,
                            holder: w.holder,
                            task: w.task,
                            leased_at_epoch_s: w.leased_at_epoch_s,
                            lease_expires_at_epoch_s: w.lease_expires_at_epoch_s,
                            execution_id,
                        }
                    })
                    .collect();
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkspacePoolSummaryResult { workspaces },
                );
            }
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("cube workspace list failed: {err}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_worker_pool_summary(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::WorkerPoolSummary = req else {
        unreachable!()
    };
    {
        // Read-only diagnostic surface — same rationale as
        // `workspace_pool_summary`: no extra subtree gate buys any
        // security here, and User tier avoids breaking legitimate
        // callers that aren't app/Boss descendants.
        if !server_state.authorize_rpc(RpcTier::User, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                "worker_pool_summary rejected: caller failed user tier",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "worker_pool_summary failed user-tier check".to_owned(),
                },
            );
            return;
        }

        let coordinator = &server_state.execution_coordinator;
        // Live-backed execution ids, same cross-check `pool_claim_sweep`
        // uses: a claim with no matching live-state entry has outlived
        // its execution and is either mid-reconciliation or a leak.
        let live_run_ids: std::collections::HashSet<String> = server_state
            .live_worker_states
            .snapshot()
            .into_iter()
            .map(|state| state.run_id)
            .collect();

        let mut pools = Vec::with_capacity(3);
        for (pool, name) in [
            (coordinator.worker_pool(), "main"),
            (coordinator.automation_worker_pool(), "automation"),
            (coordinator.review_worker_pool(), "review"),
        ] {
            let capacity = pool.capacity().await;
            let mut claims = Vec::new();
            for claim in pool.claims().await {
                // `spilled_from_pool` keeps automation identifiable once it
                // spills into a Lower Decks slot: the claim then sits in the
                // main pool under an ordinary `worker-N` id, so without an
                // explicit attribution this listing would report it as
                // mainline work. Compare the execution's attributed pool
                // against the pool actually holding the slot; a mismatch is
                // by definition a spill.
                let (execution_status, work_item_id, spilled_from_pool) =
                    match server_state.work_db.get_execution(&claim.execution_id) {
                        Ok(execution) => {
                            let attributed = coordinator.attributed_pool_label(&execution);
                            let spilled = (attributed != name).then(|| attributed.to_owned());
                            (
                                Some(execution.status.to_string()),
                                Some(execution.work_item_id),
                                spilled,
                            )
                        }
                        Err(err) => {
                            tracing::warn!(
                                worker_id = %claim.worker_id,
                                execution_id = %claim.execution_id,
                                ?err,
                                "worker_pool_summary: failed to look up claimed execution",
                            );
                            (None, None, None)
                        }
                    };
                claims.push(boss_protocol::WorkerPoolClaimEntry {
                    worker_id: claim.worker_id.clone(),
                    execution_id: claim.execution_id.clone(),
                    execution_status,
                    work_item_id,
                    live: live_run_ids.contains(&claim.execution_id),
                    spilled_from_pool,
                });
            }
            let idle = capacity.saturating_sub(claims.len());
            let effective_cap = (name == "main").then(|| coordinator.max_concurrent_interactive_workers());
            pools.push(
                boss_protocol::WorkerPoolEntry::builder()
                    .name(name)
                    .capacity(capacity)
                    .idle(idle)
                    .claims(claims)
                    .maybe_effective_cap(effective_cap)
                    .build(),
            );
        }

        send_response(&sink, &request_id, FrontendEvent::WorkerPoolSummaryResult { pools });
    }
}

pub(super) async fn handle_get_engine_version(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch { sink, request_id, .. } = ctx;
    let FrontendRequest::GetEngineVersion = req else {
        unreachable!()
    };
    {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::EngineVersionResult {
                git_sha: crate::build_info::git_sha().to_owned(),
                build_time: crate::build_info::build_time().to_owned(),
                binary_fingerprint: crate::build_info::binary_fingerprint().to_owned(),
            },
        );
    }
}

pub(super) async fn handle_get_engine_health(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetEngineHealth = req else {
        unreachable!()
    };
    {
        let report = build_engine_health_report(&server_state);
        send_response(&sink, &request_id, FrontendEvent::EngineHealthResult { report });
    }
}

pub(super) async fn handle_list_feature_flags(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListFeatureFlags = req else {
        unreachable!()
    };
    {
        let flags = feature_flags_snapshot_to_wire(&server_state);
        send_response(&sink, &request_id, FrontendEvent::FeatureFlagsList { flags });
    }
}

pub(super) async fn handle_set_feature_flag(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetFeatureFlag { name, enabled } = req else {
        unreachable!()
    };
    {
        match server_state.feature_flags.set(&name, enabled) {
            Ok(()) => {
                // Warn the operator when a flag is enabled but its
                // backing capability is absent from this build.
                if enabled
                    && let Some(spec) = crate::feature_flags::REGISTRY.iter().find(|s| s.name == name)
                    && let Some(cap_id) = spec.capability_id
                    && !server_state.capability_registry.is_present(cap_id)
                {
                    tracing::warn!(
                        flag = %name,
                        capability = %cap_id,
                        "feature-flags: flag enabled but its backing capability \
                         is absent from this build — the flag will have no effect",
                    );
                }
                tracing::info!(
                    flag = %name,
                    enabled,
                    "feature-flags: toggled via macOS debug pane",
                );
                send_response(&sink, &request_id, FrontendEvent::FeatureFlagSet { name, enabled });
            }
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}

/// Update the engine's capability registry with the IDs reported by the
/// macOS app and reply with the updated flag list so the debug pane
/// reflects accurate `capability_present` values immediately.
pub(super) async fn handle_register_capabilities(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RegisterCapabilities { capability_ids } = req else {
        unreachable!()
    };
    server_state.capability_registry.replace_all(capability_ids);
    let flags = feature_flags_snapshot_to_wire(&server_state);
    send_response(&sink, &request_id, FrontendEvent::FeatureFlagsList { flags });
}

/// Build the wire-protocol flag list from the live store + capability
/// registry. Extracted so both `ListFeatureFlags` and
/// `RegisterCapabilities` share the same mapping code.
fn feature_flags_snapshot_to_wire(server_state: &ServerState) -> Vec<boss_protocol::FeatureFlagSnapshot> {
    server_state
        .feature_flags
        .snapshot_all(Some(&server_state.capability_registry))
        .into_iter()
        .map(|snap| boss_protocol::FeatureFlagSnapshot {
            name: snap.name,
            description: snap.description,
            category: snap.category,
            default_enabled: snap.default_enabled,
            enabled: snap.enabled,
            capability_present: snap.capability_present,
        })
        .collect()
}

pub(super) async fn handle_get_settings(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetSettings = req else {
        unreachable!()
    };
    {
        let settings = server_state
            .settings
            .snapshot_all()
            .into_iter()
            .map(|snap| boss_protocol::SettingSnapshot {
                key: snap.key,
                description: snap.description,
                default_enabled: snap.default_enabled,
                enabled: snap.enabled,
            })
            .collect();
        send_response(&sink, &request_id, FrontendEvent::SettingsList { settings });
    }
}

pub(super) async fn handle_set_setting(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetSetting { key, enabled } = req else {
        unreachable!()
    };
    {
        match server_state.settings.set(&key, enabled) {
            Ok(()) => {
                tracing::info!(
                    %key,
                    enabled,
                    "settings: toggled via macOS Settings window",
                );
                send_response(&sink, &request_id, FrontendEvent::SettingSet { key, enabled });
            }
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}

pub(super) async fn handle_kick_pr_reconcilers(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::KickPrReconcilers = req else {
        unreachable!()
    };
    {
        server_state.pr_reconciler_kick.notify_one();
        tracing::debug!("merge poller: activation kick received from app");
        send_response(&sink, &request_id, FrontendEvent::PrReconcilersKicked { kicked: true });
    }
}

pub(super) async fn handle_get_dispatch_concurrency(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetDispatchConcurrency = req else {
        unreachable!()
    };
    let limit = server_state.execution_coordinator.max_concurrent_interactive_workers();
    let max = server_state
        .execution_coordinator
        .worker_pool()
        .capacity_sync()
        .min(crate::coordinator::MAX_WORKER_POOL_SIZE);
    send_response(
        &sink,
        &request_id,
        FrontendEvent::DispatchConcurrencyResult {
            limit,
            max,
            clamped_from: None,
        },
    );
}

pub(super) async fn handle_set_dispatch_concurrency(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetDispatchConcurrency { limit } = req else {
        unreachable!()
    };
    let coordinator = &server_state.execution_coordinator;
    let previous = coordinator.max_concurrent_interactive_workers();
    let outcome = match coordinator.set_max_concurrent_interactive_workers(limit) {
        Ok(outcome) => outcome,
        Err(message) => {
            send_work_error(&sink, &request_id, message);
            return;
        }
    };
    let max = coordinator
        .worker_pool()
        .capacity_sync()
        .min(crate::coordinator::MAX_WORKER_POOL_SIZE);
    // Persist so the cap survives an engine restart — same pattern as
    // `handle_set_dispatch_paused`.
    if let Err(err) = work_db.set_metadata(METADATA_KEY_DISPATCH_CONCURRENCY_LIMIT, &outcome.applied.to_string()) {
        tracing::warn!(
            requested = limit,
            applied = outcome.applied,
            ?err,
            "dispatch_concurrency: failed to persist to state.db — state is \
             applied in-memory but will revert on engine restart",
        );
    }
    if outcome.applied > previous {
        // A bare store on the coordinator doesn't itself wake
        // `drain_ready_queue`; kick it so newly-available capacity is used
        // immediately instead of sitting idle until the next
        // naturally-triggered drain pass.
        coordinator.kick();
        tracing::info!(
            previous,
            applied = outcome.applied,
            "dispatch: interactive concurrency cap raised — scheduler kicked to use new capacity",
        );
    } else {
        tracing::info!(
            previous,
            applied = outcome.applied,
            "dispatch: interactive concurrency cap lowered"
        );
    }
    if let Some(requested) = outcome.clamped_from {
        tracing::warn!(
            requested,
            applied = outcome.applied,
            max,
            "dispatch_concurrency: requested value exceeded the worker-pool ceiling — clamped",
        );
    }
    send_response(
        &sink,
        &request_id,
        FrontendEvent::DispatchConcurrencyResult {
            limit: outcome.applied,
            max,
            clamped_from: outcome.clamped_from,
        },
    );
}

/// Serve the per-driver provider quota snapshot.
///
/// Read-only and cheap in the common case: the cache answers from memory
/// unless its TTL has expired or the operator asked for a refresh, so
/// opening Preferences does not fan out three provider calls every time.
/// The figures here are each driver's *provider's* own view — never Boss's
/// internal token accounting, which measures a different thing.
///
/// A driver that cannot be read comes back as an explicit failure entry, not
/// as an omission: the snapshot always describes every implemented driver.
pub(super) async fn handle_get_driver_quota_usage(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetDriverQuotaUsage { refresh } = req else {
        unreachable!()
    };
    let snapshot = server_state.driver_quota.snapshot(refresh).await;
    send_response(&sink, &request_id, FrontendEvent::DriverQuotaUsageResult { snapshot });
}

pub(super) async fn handle_get_driver_traffic_split(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetDriverTrafficSplit = req else {
        unreachable!()
    };
    match work_db.get_driver_traffic_split() {
        Ok(split) => send_response(&sink, &request_id, FrontendEvent::DriverTrafficSplitResult { split }),
        Err(err) => send_work_error(&sink, &request_id, err),
    }
}

pub(super) async fn handle_set_driver_traffic_split(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetDriverTrafficSplit { split } = req else {
        unreachable!()
    };
    // `set_driver_traffic_split` validates (shares must sum to exactly 100)
    // and persists to `state.db` itself — see its doc comment. A split that
    // does not validate is rejected here as a `WorkError`, never repaired
    // into something valid-looking. Nothing in-memory to update: every
    // dispatch reads the metadata KV fresh at `insert_execution` time, so an
    // accepted split takes effect on the very next execution created,
    // without disturbing anything already dispatched.
    match work_db.set_driver_traffic_split(split) {
        Ok(applied) => {
            tracing::info!(
                grok = applied.grok,
                claude = applied.claude,
                codex = applied.codex,
                "driver_traffic_split: updated",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::DriverTrafficSplitResult { split: applied },
            );
        }
        Err(err) => {
            tracing::warn!(
                grok = split.grok,
                claude = split.claude,
                codex = split.codex,
                %err,
                "driver_traffic_split: rejected",
            );
            send_work_error(&sink, &request_id, err);
        }
    }
}

pub(super) async fn handle_set_dispatch_paused(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetDispatchPaused { paused, reason } = req else {
        unreachable!()
    };
    {
        let coordinator = &server_state.execution_coordinator;
        let already = coordinator.is_dispatch_paused();
        if already == paused {
            // Idempotent: no-op but still respond with the current state.
            tracing::debug!(paused, "set_dispatch_paused: idempotent no-op");
            send_response(&sink, &request_id, dispatch_state_result(coordinator.dispatch_pause()));
            return;
        }
        // A pause with no usable reason is rejected outright — dispatch must
        // never be found paused with no record of who paused it or why.
        // Ignored (never required) when resuming.
        let pause_reason = if paused {
            match reason.and_then(|r| PauseReason::new(r).ok()) {
                Some(reason) => Some(reason),
                None => {
                    send_work_error(
                        &sink,
                        &request_id,
                        "SetDispatchPaused { paused: true } requires a non-empty `reason`".to_owned(),
                    );
                    return;
                }
            }
        } else {
            None
        };
        // Snapshot the pause start (if any) before `resume_dispatch`
        // zeroes it on resume, so a resume's audit record can carry how
        // long the episode actually lasted.
        let paused_since_before = coordinator.dispatch_paused_since_epoch_s();
        let now_epoch_s = boss_engine_utils::epoch_time::now_epoch_secs() as u64;
        // A human toggling `bossctl dispatch pause` / the app's pause switch
        // is always an operator-originated pause, so PR-review executions —
        // the lifecycle of a change already in flight, not new work — stay
        // exempt from it. See `DispatchPauseOrigin`.
        let origin = crate::coordinator::DispatchPauseOrigin::Operator;
        let reason_str = if paused {
            let pause_reason = pause_reason.expect("validated above");
            let reason_str = pause_reason.as_str().to_owned();
            coordinator.pause_dispatch(now_epoch_s, origin, pause_reason);
            Some(reason_str)
        } else {
            coordinator.resume_dispatch();
            None
        };
        // Persist the new state to the metadata table so it survives a restart.
        let db_result = if paused {
            work_db
                .set_metadata(METADATA_KEY_DISPATCH_PAUSED, "1")
                .and_then(|()| work_db.set_metadata(METADATA_KEY_DISPATCH_PAUSED_SINCE, &now_epoch_s.to_string()))
                .and_then(|()| work_db.set_metadata(METADATA_KEY_DISPATCH_PAUSE_ORIGIN, origin.as_metadata_str()))
                .and_then(|()| {
                    work_db.set_metadata(
                        METADATA_KEY_DISPATCH_PAUSE_REASON,
                        reason_str.as_deref().unwrap_or_default(),
                    )
                })
        } else {
            work_db
                .set_metadata(METADATA_KEY_DISPATCH_PAUSED, "0")
                .and_then(|()| work_db.set_metadata(METADATA_KEY_DISPATCH_PAUSED_SINCE, "0"))
                .and_then(|()| work_db.set_metadata(METADATA_KEY_DISPATCH_PAUSE_REASON, ""))
        };
        if let Err(err) = db_result {
            tracing::warn!(
                paused,
                ?err,
                "dispatch_pause: failed to persist to state.db — state is \
                 applied in-memory but will revert on engine restart",
            );
        }
        if paused {
            tracing::info!(
                reason = reason_str.as_deref().unwrap_or_default(),
                "dispatch: globally paused (operator) — PR-review executions remain exempt"
            );
            server_state
                .dispatch_events
                .emit(
                    crate::dispatch_events::DispatchEvent::new(
                        crate::dispatch_events::Stage::DispatchPaused,
                        crate::dispatch_events::Outcome::Ok,
                        "engine",
                    )
                    .with_details(serde_json::json!({
                        "origin": "operator",
                        "actor": "operator",
                        "paused_since_epoch_s": now_epoch_s,
                        "reviews_held": false,
                        "scope": ["dispatch"],
                        "reason": reason_str,
                    })),
                )
                .await;
        } else {
            // Re-kick the scheduler so anything that queued while paused is
            // drained immediately without waiting for the next external event.
            coordinator.kick();
            tracing::info!("dispatch: resumed — scheduler kicked to drain queued executions");
            let pause_duration_secs = paused_since_before.map(|since| now_epoch_s.saturating_sub(since));
            server_state
                .dispatch_events
                .emit(
                    crate::dispatch_events::DispatchEvent::new(
                        crate::dispatch_events::Stage::DispatchResumed,
                        crate::dispatch_events::Outcome::Ok,
                        "engine",
                    )
                    .with_details(serde_json::json!({
                        "origin": "operator",
                        "actor": "operator",
                        "resumed_at_epoch_s": now_epoch_s,
                        "pause_duration_secs": pause_duration_secs,
                        "reason": "operator requested resume via bossctl dispatch resume / the app's dispatch toggle",
                    })),
                )
                .await;
        }
        send_response(&sink, &request_id, dispatch_state_result(coordinator.dispatch_pause()));
        // Broadcast the new health report to all connected app clients so
        // the pause banner updates live without requiring an app restart.
        server_state.broadcast_engine_health().await;
    }
}

/// Build the wire reply from ONE pause snapshot.
///
/// Every field here describes the same pause episode or none at all. Reading
/// them through four separate accessors instead would let a concurrent
/// pause/resume land between two of them and produce a reply that describes
/// no state the engine was ever actually in — e.g. `paused: false` carrying
/// the previous episode's `reviews_exempt`, which is exactly the stale-scope
/// report this refactor exists to make impossible.
fn dispatch_state_result(pause: Option<crate::coordinator::DispatchPause>) -> FrontendEvent {
    FrontendEvent::DispatchStateResult {
        paused: pause.is_some(),
        paused_since_epoch_s: pause.as_ref().map(|p| p.since_epoch_s),
        reviews_exempt: pause.as_ref().is_some_and(|p| !p.reviews_held()),
        reason: pause.as_ref().map(|p| p.reason.clone()),
    }
}

pub(super) async fn handle_get_dispatch_state(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetDispatchState = req else {
        unreachable!()
    };
    let pause = server_state.execution_coordinator.dispatch_pause();
    send_response(&sink, &request_id, dispatch_state_result(pause));
}

pub(super) async fn handle_set_automation_paused(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetAutomationPaused { paused, reason } = req else {
        unreachable!()
    };
    {
        let coordinator = &server_state.execution_coordinator;
        let already = coordinator.is_automation_paused();
        if already == paused {
            // Idempotent: no-op but still respond with the current state.
            let paused_since_epoch_s = coordinator.automation_paused_since_epoch_s();
            tracing::debug!(paused, "set_automation_paused: idempotent no-op");
            send_response(
                &sink,
                &request_id,
                FrontendEvent::AutomationStateResult {
                    paused,
                    paused_since_epoch_s,
                    reason: coordinator.automation_paused_reason(),
                },
            );
            return;
        }
        // A pause with no usable reason is rejected outright — see
        // `handle_set_dispatch_paused` for why. Ignored when resuming.
        let pause_reason = if paused {
            match reason.and_then(|r| PauseReason::new(r).ok()) {
                Some(reason) => Some(reason),
                None => {
                    send_work_error(
                        &sink,
                        &request_id,
                        "SetAutomationPaused { paused: true } requires a non-empty `reason`".to_owned(),
                    );
                    return;
                }
            }
        } else {
            None
        };
        let now_epoch_s = boss_engine_utils::epoch_time::now_epoch_secs() as u64;
        let reason_str = if paused {
            let pause_reason = pause_reason.expect("validated above");
            let reason_str = pause_reason.as_str().to_owned();
            coordinator.pause_automation(now_epoch_s, pause_reason);
            Some(reason_str)
        } else {
            coordinator.resume_automation();
            None
        };
        let db_result = if paused {
            work_db
                .set_metadata(METADATA_KEY_AUTOMATION_PAUSED, "1")
                .and_then(|()| work_db.set_metadata(METADATA_KEY_AUTOMATION_PAUSED_SINCE, &now_epoch_s.to_string()))
                .and_then(|()| {
                    work_db.set_metadata(
                        METADATA_KEY_AUTOMATION_PAUSE_REASON,
                        reason_str.as_deref().unwrap_or_default(),
                    )
                })
        } else {
            work_db
                .set_metadata(METADATA_KEY_AUTOMATION_PAUSED, "0")
                .and_then(|()| work_db.set_metadata(METADATA_KEY_AUTOMATION_PAUSED_SINCE, "0"))
                .and_then(|()| work_db.set_metadata(METADATA_KEY_AUTOMATION_PAUSE_REASON, ""))
        };
        if let Err(err) = db_result {
            tracing::warn!(
                paused,
                ?err,
                "automation_pause: failed to persist to state.db — state is \
                 applied in-memory but will revert on engine restart",
            );
        }
        // Both transitions publish to the automation scheduler's subscription,
        // which now consults this flag before evaluating anything. Pausing
        // lets it drop straight into its idle sleep instead of finishing out
        // the current interval.
        //
        // Resuming is the load-bearing one: a paused scheduler sleeps up to
        // AUTOMATION_SCHEDULER_MAX_SLEEP_SECS (one hour), so without this
        // publish `bossctl automation resume` would appear to do nothing for
        // up to an hour. `coordinator.kick()` alone is not enough — that
        // wakes the execution dispatcher, which is a different loop.
        server_state
            .event_bus
            .publish(boss_event_bus::Event::AutomationMutation);
        if paused {
            tracing::info!(
                reason = reason_str.as_deref().unwrap_or_default(),
                "automation: globally paused (operator) — the scheduler stops evaluating \
                 occurrences, new triage passes and automation-pool spawns are held; \
                 already-running automation workers finish normally",
            );
        } else {
            // Re-kick the scheduler so anything that queued while paused is
            // drained immediately without waiting for the next external event.
            coordinator.kick();
            tracing::info!("automation: resumed — automation scheduler and execution dispatcher both kicked",);
        }
        let paused_since_epoch_s = coordinator.automation_paused_since_epoch_s();
        send_response(
            &sink,
            &request_id,
            FrontendEvent::AutomationStateResult {
                paused,
                paused_since_epoch_s,
                reason: coordinator.automation_paused_reason(),
            },
        );
        // Broadcast the new health report to all connected app clients so
        // the pause banner updates live without requiring an app restart.
        server_state.broadcast_engine_health().await;
    }
}

pub(super) async fn handle_get_automation_state(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetAutomationState = req else {
        unreachable!()
    };
    {
        let coordinator = &server_state.execution_coordinator;
        let paused = coordinator.is_automation_paused();
        let paused_since_epoch_s = coordinator.automation_paused_since_epoch_s();
        send_response(
            &sink,
            &request_id,
            FrontendEvent::AutomationStateResult {
                paused,
                paused_since_epoch_s,
                reason: coordinator.automation_paused_reason(),
            },
        );
    }
}
