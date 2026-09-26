//! `FrontendRequest` handlers — app/boss session registration, engine responses, shutdown.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;
use crate::coordinator_tmux::ClaudeVersionProbe;

const COORDINATOR_UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(15 * 60);

pub(super) async fn handle_register_app_session(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        session_id,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::RegisterAppSession = req else {
        unreachable!()
    };
    {
        // Trust the peer if any of:
        //   (a) it matches the declared app pid exactly. The
        //       engine reads `BOSS_APP_PID` at startup; the
        //       macOS app sets this before spawning the engine
        //       (necessary because `bazel run` daemonizes,
        //       which severs the engine's process tree from
        //       the app and breaks ancestor-walk auth).
        //   (b) the peer pid appears in the engine's ancestor
        //       chain (covers direct-launch scenarios like
        //       `swift run` where no daemonizing wrapper
        //       exists).
        //   (c) APP RESTART against a surviving engine: the
        //       trusted app pid belongs to a now-dead process
        //       and a fresh app instance is connecting. The
        //       engine correctly stays up on a same-version
        //       relaunch, so the relaunched app must be able to
        //       re-attach its session — otherwise the stale pid
        //       rejects `RegisterAppSession` forever, no
        //       `app_session` is registered, and every
        //       engine→app RPC (`AttachWorkerPane`, reveal) dies
        //       silently. This mirrors engine restart re-attaching
        //       surviving panes: there the app survives and the
        //       engine restarts; here the engine survives and the
        //       app restarts. We require
        //       the old pid to be genuinely dead so a second
        //       live app can't hijack the trust root from the
        //       real one.
        let engine_pid = std::process::id() as libc::pid_t;
        let current_app_pid = server_state.current_app_pid();
        let trust_ok = register_app_session_trust_ok(current_app_pid, peer_pid, engine_pid);
        if !trust_ok {
            tracing::warn!(
                peer_pid = ?peer_pid,
                engine_pid,
                expected_app_pid = ?current_app_pid,
                "register_app_session rejected: peer pid neither matches BOSS_APP_PID nor is an engine ancestor",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "register_app_session: peer pid does not match app_pid".to_owned(),
                },
            );
            return;
        }
        // Re-pin the trust root to the (re)connecting app when it
        // differs from the stale pid. Keeps RPC authorization
        // (`AttachWorkerPane`, BossOnly/AppOrBoss tiers) following
        // the live app across restarts. Only when a real trust
        // root was configured — test mode (`None`) stays
        // permissive so unit tests aren't pinned to a live pid.
        if let (Some(prior), Some(observed)) = (current_app_pid, peer_pid)
            && prior != observed
        {
            server_state.set_app_pid(observed);
            tracing::info!(
                prior_app_pid = prior,
                new_app_pid = observed,
                "app session re-attached: trust root re-pinned to relaunched app",
            );
            // Worker shells are children of the tmux server, not the app
            // process, so a relaunched app never kills any in-flight
            // worker — there is nothing here for the dead-PID sweep to
            // reconcile ahead of its normal periodic pass.
        }
        server_state
            .register_app_session(session_id.clone(), sink.clone())
            .await;
        tracing::info!(session_id = %session_id, "app session registered");
        send_response(&sink, &request_id, FrontendEvent::AppSessionRegistered);
        // Engine restart can land between `run_started` and `spawn_requested`.
        // Retry only the startup rows for which pane presence was
        // Undetermined: re-sweeping all in-flight rows could duplicate a
        // healthy dispatch still inside its initial spawn round-trip.
        let pane_reconcile_state = server_state.clone();
        tokio::spawn(async move {
            pane_reconcile_state.retry_startup_pane_reconcile().await;
        });
        // A fresh app session is the operator's natural recovery action
        // (e.g. relaunching the app after waking the display) — clear the
        // spawn-capability breaker's failure window and any half-open probe
        // state left over from before, and auto-resume dispatch if it's
        // currently Breaker-paused. Never touches an operator pause:
        // `resume_dispatch_after_breaker_recovery` no-ops unless the
        // current pause is Breaker-origin.
        server_state.spawn_health.record_success();
        server_state.spawn_health.reset_probe();
        if crate::spawn_health::resume_dispatch_after_breaker_recovery(
            &server_state.work_db,
            &server_state.execution_coordinator,
            server_state.dispatch_events.as_ref(),
            None,
            "fresh app session registered",
        )
        .await
        {
            // No explicit health broadcast: the `resume_dispatch` inside
            // `resume_dispatch_after_breaker_recovery` already notified the
            // pause-state transition, and the pause-state broadcaster turns
            // that into the push. See
            // `ServerState::spawn_pause_state_health_broadcaster`.
            server_state.execution_coordinator.kick();
        }
        // Push pool sizes immediately after registration so the app's
        // WorkersWorkspaceModel can configure its slot ranges before the
        // engine dispatches any AttachWorkerPane. This is the single source
        // of truth: the engine's runtime pool config drives the app's
        // capacity check, so they can never be independently out of sync.
        send_push(
            &sink,
            FrontendEvent::EnginePoolConfig {
                worker_slots: server_state.worker_pool_size,
                automation_slots: server_state.automation_pool_size,
                review_slots: server_state.review_pool_size,
                coordinator_model: server_state.coordinator_model.clone(),
            },
        );
        // The engine, rather than the app, owns the coordinator's detached
        // tmux session. Attachment is retried by the supervisor until the
        // current app session acknowledges its viewer.
        let state = server_state.clone();
        tokio::spawn(async move {
            attach_coordinator_to_registered_app(state).await;
        });
        // Mirror the coordinator re-attach for worker panes: a run this
        // engine process spawned under a prior app session (now dead —
        // an app restart) never received `AttachWorkerPane` from anywhere
        // else, so every live tmux-hosted worker needs one now.
        let state = server_state.clone();
        tokio::spawn(async move {
            state.reattach_worker_panes_to_registered_app().await;
        });
    }
}

async fn attach_coordinator_to_registered_app(server_state: Arc<ServerState>) {
    // This is a genuine attach entry point (app launch/relaunch/reconnect),
    // not the supervisor's flat 10s unattached-retry loop that
    // `coordinator_installed_version_cache` exists to rate-limit. Clear it
    // so registration always re-probes `claude --version`, otherwise an
    // upgrade installed after the cache was first populated is never
    // observed for the lifetime of this engine process.
    *server_state
        .coordinator_installed_version_cache
        .lock()
        .expect("coordinator installed-version cache mutex poisoned") = None;
    let program = match server_state.tmux_preflight.read() {
        Ok(guard) => match &*guard {
            crate::tmux_preflight::TmuxPreflight::Ready { program, .. } => program.clone(),
            crate::tmux_preflight::TmuxPreflight::Unavailable { reason } => {
                tracing::warn!(%reason, "coordinator tmux attach skipped: tmux preflight is unavailable");
                return;
            }
        },
        Err(_) => {
            tracing::error!("coordinator tmux attach skipped: preflight lock poisoned");
            return;
        }
    };
    let tmux = match server_state.tmux_from_program(program) {
        Ok(tmux) => tmux,
        Err(error) => {
            tracing::error!(%error, "coordinator tmux attach skipped: resolved tmux path is invalid");
            return;
        }
    };
    let legacy_tmux = boss_tmux::Tmux::for_legacy_label_server(tmux.program().to_path_buf()).ok();
    let working_directory = match crate::coordinator_tmux::coordinator_working_directory() {
        Ok(path) => path,
        Err(error) => {
            tracing::error!(error = %format!("{error:#}"), "failed to resolve coordinator session directory");
            return;
        }
    };
    let (record, active_tmux) = {
        let _guard = server_state.coordinator_tmux_lock.lock().await;
        let active_tmux = crate::coordinator_tmux::resolve_active_handle(&tmux, legacy_tmux.as_ref())
            .await
            .clone();
        let record = match crate::coordinator_tmux::ensure_for_attach(&crate::coordinator_tmux::CoordinatorSpawn {
            work_db: server_state.work_db.as_ref(),
            tmux: &active_tmux,
            create_tmux: &tmux,
            model: &server_state.coordinator_model,
            working_directory: &working_directory,
            version_probe: &crate::coordinator_tmux::RealClaudeVersionProbe,
        })
        .await
        {
            Ok(state) => state,
            Err(error) => {
                tracing::error!(error = %format!("{error:#}"), "failed to create or recover coordinator tmux session");
                return;
            }
        };
        (record, active_tmux)
    };
    request_coordinator_attachment(server_state, &active_tmux, record).await;
}

pub(super) async fn request_coordinator_attachment(
    server_state: Arc<ServerState>,
    tmux: &boss_tmux::Tmux,
    record: crate::work::CoordinatorTmuxRecord,
) {
    match crate::coordinator_tmux::pane_pid(tmux, &record).await {
        Ok(pid) => server_state.set_boss_pid(pid),
        Err(error) => tracing::warn!(%error, "could not refresh coordinator trust-root pid"),
    }
    let tmux_program = tmux.program().display().to_string();
    let Some(tmux_socket_path) = tmux.socket_path().map(|path| path.display().to_string()) else {
        tracing::error!("coordinator tmux attach skipped: handle has no socket path");
        return;
    };
    // This function has three call sites: app registration (above, via
    // `attach_coordinator_to_registered_app`), the coordinator supervisor's
    // restart branch (server.rs, exponential backoff), and its healthy
    // `Ok(None)` branch (server.rs, a flat 10s retry while the app has
    // registered but not yet acknowledged this spawn token). That last one
    // repeats indefinitely whenever the app keeps failing to attach —
    // exactly the degraded state where the engine can least afford an
    // extra subprocess every pass. The *installed* claude version can
    // change at any time (an upgrade), so this cache exists purely to
    // rate-limit that retry loop, not to assert the value is immutable —
    // `attach_coordinator_to_registered_app` clears it on every genuine
    // attach entry point so registration always re-probes.
    let cached_installed_version = {
        let cache = server_state
            .coordinator_installed_version_cache
            .lock()
            .expect("coordinator installed-version cache mutex poisoned");
        cache
            .as_ref()
            .filter(|entry| entry.spawn_token == record.spawn_token)
            .map(|entry| entry.installed_version.clone())
    };
    let installed_claude_version = match cached_installed_version {
        Some(installed) => installed,
        None => {
            let probed = crate::coordinator_tmux::RealClaudeVersionProbe.probe().await;
            *server_state
                .coordinator_installed_version_cache
                .lock()
                .expect("coordinator installed-version cache mutex poisoned") =
                Some(CoordinatorInstalledVersionCacheEntry {
                    spawn_token: record.spawn_token.clone(),
                    installed_version: probed.clone(),
                    probed_at: Instant::now(),
                    advertised_update_available_version: None,
                });
            probed
        }
    };
    let coordinator_update_available_version =
        crate::coordinator_tmux::coordinator_update_available(&record, installed_claude_version.as_deref());
    match server_state
        .send_to_app(
            EngineToAppRequest::AttachCoordinatorPane(boss_protocol::AttachCoordinatorPaneInput {
                session_name: record.session_name.clone(),
                spawn_token: record.spawn_token.clone(),
                model: record.model.clone(),
                tmux_program,
                tmux_socket_path,
                coordinator_update_available_version: coordinator_update_available_version.clone(),
            }),
            Duration::from_secs(5),
        )
        .await
    {
        Ok(EngineToAppResponse::AttachCoordinatorPane { result: Ok(_) }) => {
            *server_state
                .coordinator_attached_spawn_token
                .lock()
                .expect("coordinator attached token mutex poisoned") = Some(record.spawn_token.clone());
            if let Some(entry) = server_state
                .coordinator_installed_version_cache
                .lock()
                .expect("coordinator installed-version cache mutex poisoned")
                .as_mut()
                .filter(|entry| entry.spawn_token == record.spawn_token)
            {
                entry.advertised_update_available_version = coordinator_update_available_version;
            }
            tracing::info!("attached app Boss pane to coordinator tmux session");
        }
        Ok(response) => tracing::warn!(
            ?response,
            "coordinator session exists but app did not attach its viewer"
        ),
        Err(error) => tracing::debug!(%error, "coordinator session exists without an app viewer"),
    }
}

/// Re-probe an already attached coordinator at a deliberately low cadence.
/// `claude` upgrades happen at most a few times a week, so one short version
/// process every [`COORDINATOR_UPDATE_CHECK_INTERVAL`] per attached session is
/// enough to surface an update without turning the supervisor's 10-second
/// health check into recurring process churn. Re-using
/// `request_coordinator_attachment` pushes the same pane descriptor the app
/// already renders, but only after the optional banner value changed; `Some`
/// to `None` clears a banner after a downgrade or reset.
pub(super) async fn refresh_coordinator_update_available(
    server_state: Arc<ServerState>,
    tmux: &boss_tmux::Tmux,
    record: crate::work::CoordinatorTmuxRecord,
) {
    let existing_entry = server_state
        .coordinator_installed_version_cache
        .lock()
        .expect("coordinator installed-version cache mutex poisoned")
        .clone()
        .filter(|entry| entry.spawn_token == record.spawn_token);

    // Only the subprocess spawn needs rate-limiting to
    // `COORDINATOR_UPDATE_CHECK_INTERVAL`; the comparison against the cached
    // installed version below is free, so it always runs. This makes an
    // un-acked push (the app's 5s attach timeout, a busy/disconnected app)
    // self-healing on the very next 10s supervisor pass instead of being
    // deferred by a full interval alongside the probe.
    let should_probe = coordinator_update_probe_due(existing_entry.as_ref(), &record.spawn_token);
    let freshly_probed = if should_probe {
        let probed = crate::coordinator_tmux::RealClaudeVersionProbe.probe().await;
        tracing::info!(
            spawn_token = %record.spawn_token,
            installed_version = ?probed,
            "coordinator update probe ran",
        );
        Some(probed)
    } else {
        None
    };

    let decision = refresh_decision(existing_entry.as_ref(), &record, freshly_probed);
    if decision.should_push {
        tracing::info!(
            spawn_token = %record.spawn_token,
            previous_advertised = ?existing_entry
                .as_ref()
                .and_then(|entry| entry.advertised_update_available_version.as_deref()),
            advertised = ?decision.update_available_version,
            "coordinator update availability changed; pushing to app",
        );
    }
    let probed_at = if should_probe {
        Instant::now()
    } else {
        existing_entry
            .as_ref()
            .map_or_else(Instant::now, |entry| entry.probed_at)
    };
    *server_state
        .coordinator_installed_version_cache
        .lock()
        .expect("coordinator installed-version cache mutex poisoned") = Some(CoordinatorInstalledVersionCacheEntry {
        spawn_token: record.spawn_token.clone(),
        installed_version: decision.installed_version,
        probed_at,
        advertised_update_available_version: existing_entry.and_then(|entry| entry.advertised_update_available_version),
    });

    if decision.should_push {
        request_coordinator_attachment(server_state, tmux, record).await;
    }
}

fn coordinator_update_probe_due(cache: Option<&CoordinatorInstalledVersionCacheEntry>, spawn_token: &str) -> bool {
    cache.is_none_or(|entry| {
        entry.spawn_token != spawn_token || entry.probed_at.elapsed() >= COORDINATOR_UPDATE_CHECK_INTERVAL
    })
}

/// Pure decision core of [`refresh_coordinator_update_available`]: given the
/// cached entry (if any, already filtered to this spawn token), the
/// coordinator record, and this pass's fresh probe result (`None` when the
/// probe was not yet due), determines the installed version to carry
/// forward, the banner value this pass would advertise, and whether that
/// advertised value needs to be pushed again.
struct RefreshDecision {
    installed_version: Option<String>,
    update_available_version: Option<String>,
    should_push: bool,
}

fn refresh_decision(
    existing_entry: Option<&CoordinatorInstalledVersionCacheEntry>,
    record: &crate::work::CoordinatorTmuxRecord,
    freshly_probed: Option<Option<String>>,
) -> RefreshDecision {
    let installed_version = match freshly_probed {
        Some(probed) => probed,
        None => existing_entry.and_then(|entry| entry.installed_version.clone()),
    };
    let advertised_update_available_version =
        existing_entry.and_then(|entry| entry.advertised_update_available_version.clone());
    let update_available_version =
        crate::coordinator_tmux::coordinator_update_available(record, installed_version.as_deref());
    let should_push = advertised_update_available_version != update_available_version;
    RefreshDecision {
        installed_version,
        update_available_version,
        should_push,
    }
}

#[cfg(test)]
mod update_available_tests {
    use super::*;

    fn record_with_launched_version(version: &str) -> crate::work::CoordinatorTmuxRecord {
        crate::work::CoordinatorTmuxRecord {
            session_name: "boss-coordinator".to_owned(),
            spawn_token: "token".to_owned(),
            spawn_state: "created".to_owned(),
            model: "opus".to_owned(),
            launched_claude_version: Some(version.to_owned()),
            spawned_at: None,
            pane_id: None,
            liveness_passed_at: None,
        }
    }

    #[test]
    fn update_banner_transitions_are_pushed_only_when_the_value_changes() {
        let older = record_with_launched_version("2.1.237");
        let shown = crate::coordinator_tmux::coordinator_update_available(&older, Some("2.1.238"));
        assert_eq!(
            shown.as_deref(),
            Some("2.1.238"),
            "an attached session discovers an upgrade without restart"
        );

        // No cache entry yet (nothing advertised): a due probe that finds the
        // upgrade must push.
        let decision = refresh_decision(None, &older, Some(Some("2.1.238".to_owned())));
        assert_eq!(decision.installed_version.as_deref(), Some("2.1.238"));
        assert_eq!(decision.update_available_version.as_deref(), Some("2.1.238"));
        assert!(decision.should_push, "the changed value is pushed to show the banner");

        let advertised_shown = CoordinatorInstalledVersionCacheEntry {
            spawn_token: "token".to_owned(),
            installed_version: Some("2.1.238".to_owned()),
            probed_at: Instant::now(),
            advertised_update_available_version: shown.clone(),
        };
        // A downgrade back to the launched version clears the banner and must
        // still push, since the advertised value changes from Some to None.
        let clearing = refresh_decision(Some(&advertised_shown), &older, Some(Some("2.1.237".to_owned())));
        assert_eq!(clearing.installed_version.as_deref(), Some("2.1.237"));
        assert_eq!(clearing.update_available_version, None);
        assert!(clearing.should_push, "the clear is another push-worthy transition");

        let advertised_cleared = CoordinatorInstalledVersionCacheEntry {
            advertised_update_available_version: None,
            installed_version: Some("2.1.237".to_owned()),
            ..advertised_shown
        };
        // Repeating an unchanged clear on a later pass emits no redundant push.
        let repeat_clear = refresh_decision(Some(&advertised_cleared), &older, Some(Some("2.1.237".to_owned())));
        assert!(
            !repeat_clear.should_push,
            "repeating an unchanged clear emits no redundant update"
        );
    }

    #[test]
    fn unacked_push_is_retried_on_the_next_pass_without_waiting_for_the_next_probe() {
        let older = record_with_launched_version("2.1.237");
        // The app never acked the previous push: `advertised_update_available_version`
        // is still `None` even though the installed version already reflects the
        // upgrade from a prior probe.
        let unacked = CoordinatorInstalledVersionCacheEntry {
            spawn_token: "token".to_owned(),
            installed_version: Some("2.1.238".to_owned()),
            probed_at: Instant::now(),
            advertised_update_available_version: None,
        };
        // No fresh probe this pass (not due yet) — the comparison must still
        // recompute from the cached installed version and push again.
        let decision = refresh_decision(Some(&unacked), &older, None);
        assert_eq!(
            decision.installed_version.as_deref(),
            Some("2.1.238"),
            "the cached installed version is carried forward when no probe ran"
        );
        assert_eq!(
            decision.update_available_version.as_deref(),
            Some("2.1.238"),
            "the advertised banner value is derived from the cached installed version"
        );
        assert!(
            decision.should_push,
            "an un-acked push must retry on the next pass instead of waiting a full day"
        );
    }

    #[test]
    fn attached_update_probe_runs_on_the_configured_interval_not_on_every_supervisor_pass() {
        assert!(
            COORDINATOR_UPDATE_CHECK_INTERVAL <= Duration::from_secs(20 * 60),
            "the probe must fire well within a typical app uptime, or the banner only appears after a UI restart",
        );

        let recent = CoordinatorInstalledVersionCacheEntry {
            spawn_token: "token".to_owned(),
            installed_version: Some("2.1.237".to_owned()),
            probed_at: Instant::now(),
            advertised_update_available_version: None,
        };
        assert!(
            !coordinator_update_probe_due(Some(&recent), "token"),
            "a healthy supervisor pass (10s cadence) reuses the recent result instead of \
             spawning a subprocess every pass"
        );

        let halfway = CoordinatorInstalledVersionCacheEntry {
            probed_at: Instant::now() - COORDINATOR_UPDATE_CHECK_INTERVAL / 2,
            ..recent
        };
        assert!(
            !coordinator_update_probe_due(Some(&halfway), "token"),
            "the probe is not yet due at half the interval"
        );

        let old = CoordinatorInstalledVersionCacheEntry {
            probed_at: Instant::now() - COORDINATOR_UPDATE_CHECK_INTERVAL,
            ..halfway
        };
        assert!(
            coordinator_update_probe_due(Some(&old), "token"),
            "once the configured interval has elapsed, the probe is due again"
        );
        assert!(coordinator_update_probe_due(Some(&old), "replacement-token"));
    }
}

/// Replace the durable coordinator only after the UI has confirmed the loss
/// of the current conversation — either an automatic model-mismatch prompt
/// or an operator-initiated reset (see `reason`). The app cannot choose a
/// session name or model here; both remain engine-owned configuration.
pub(super) async fn handle_recreate_coordinator(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RecreateCoordinator {
        expected_spawn_token,
        reason,
    } = req
    else {
        unreachable!()
    };
    let app_session_id = server_state
        .app_session
        .lock()
        .await
        .as_ref()
        .map(|handle| handle.session_id.clone());
    if app_session_id.as_deref() != Some(session_id.as_str()) {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::Error {
                message: "recreate_coordinator: only the app session may replace the coordinator".to_owned(),
            },
        );
        return;
    }
    let program = match server_state.tmux_preflight.read() {
        Ok(guard) => match &*guard {
            crate::tmux_preflight::TmuxPreflight::Ready { program, .. } => program.clone(),
            crate::tmux_preflight::TmuxPreflight::Unavailable { reason } => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::Error {
                        message: format!("recreate_coordinator: tmux is unavailable: {reason}"),
                    },
                );
                return;
            }
        },
        Err(_) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "recreate_coordinator: tmux preflight lock is unavailable".to_owned(),
                },
            );
            return;
        }
    };
    let tmux = match server_state.tmux_from_program(program) {
        Ok(tmux) => tmux,
        Err(error) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: format!("recreate_coordinator: invalid tmux path: {error}"),
                },
            );
            return;
        }
    };
    let working_directory = match crate::coordinator_tmux::coordinator_working_directory() {
        Ok(path) => path,
        Err(error) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: format!("recreate_coordinator: session directory: {error:#}"),
                },
            );
            return;
        }
    };
    let legacy_tmux = boss_tmux::Tmux::for_legacy_label_server(tmux.program().to_path_buf()).ok();
    let replacement = {
        let _guard = server_state.coordinator_tmux_lock.lock().await;
        let active_tmux = crate::coordinator_tmux::resolve_active_handle(&tmux, legacy_tmux.as_ref()).await;
        crate::coordinator_tmux::recreate_after_confirmation(
            &crate::coordinator_tmux::CoordinatorSpawn {
                work_db: server_state.work_db.as_ref(),
                tmux: active_tmux,
                create_tmux: &tmux,
                model: &server_state.coordinator_model,
                working_directory: &working_directory,
                version_probe: &crate::coordinator_tmux::RealClaudeVersionProbe,
            },
            &expected_spawn_token,
            reason,
        )
        .await
    };
    match replacement {
        // The replacement is always freshly created on the durable socket
        // (`recreate_after_confirmation`'s `create_tmux` argument, above) —
        // it never lands on the legacy `-L boss` server, even when the old
        // session being replaced did, so this attach always targets `tmux`.
        Ok(record) => request_coordinator_attachment(server_state, &tmux, record).await,
        Err(error) => send_response(
            &sink,
            &request_id,
            FrontendEvent::Error {
                message: format!("recreate_coordinator: {error:#}"),
            },
        ),
    }
}

/// App reports that it can once again host worker panes after a
/// sleep/wake cycle (`GhosttyRuntime` confirmed an active display via
/// `NSWorkspace.didWakeNotification` / `screensDidWakeNotification`).
/// Kicks the scheduler immediately so anything stranded by the sleep —
/// a `ready` row that never got a slot while the app couldn't host a
/// surface — redispatches right away instead of waiting for the next
/// periodic sweep or the scheduler heartbeat.
pub(super) async fn handle_spawn_capability_restored(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state, peer_pid, ..
    } = ctx;
    let FrontendRequest::SpawnCapabilityRestored = req else {
        unreachable!()
    };
    if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
        tracing::warn!(
            peer_pid = ?peer_pid,
            "spawn_capability_restored rejected: caller not in app/Boss subtree",
        );
        return;
    }
    tracing::info!("spawn_capability_restored: kicking scheduler");
    server_state.execution_coordinator.kick();
}

pub(super) async fn handle_engine_response(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        session_id,
        ..
    } = ctx;
    let FrontendRequest::EngineResponse {
        request_id: response_request_id,
        response,
    } = req
    else {
        unreachable!()
    };
    {
        server_state
            .deliver_app_response(&session_id, &response_request_id, response)
            .await;
    }
}

pub(super) async fn handle_shutdown(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::Shutdown { token } = req else {
        unreachable!()
    };
    {
        // The token written to disk at startup is the auth
        // credential — there is no pid-based tier check on
        // purpose. The whole point of the token gate (issue
        // #705) is that "same user / same machine" doesn't
        // separate the legitimate caller (macOS app, boss CLI)
        // from the accidental caller (a `bazel test` that
        // resolved the production socket). The bazel sandbox
        // already denies access to `~/Library/Application
        // Support/`, so a test that lands here without the
        // file in scope will fail with `token_missing` rather
        // than killing a 9-hour-old engine.
        let outcome = match server_state.control_token.as_deref() {
            None => {
                // In-process serve() without a control token —
                // shouldn't happen for any process that has a
                // dialable frontend socket, but the dispatcher
                // is the wrong place to assume that. Reject
                // explicitly rather than panic.
                "token_missing"
            }
            Some(expected) => {
                if constant_time_eq(expected.as_bytes(), token.as_bytes()) {
                    "accepted"
                } else {
                    "token_mismatch"
                }
            }
        };
        crate::audit::record_shutdown_rpc(outcome, peer_pid);
        if outcome == "accepted" {
            tracing::info!(
                peer_pid = ?peer_pid,
                "shutdown rpc: token accepted — graceful exit pending",
            );
            send_response(&sink, &request_id, FrontendEvent::ShutdownAccepted);
            // Defer the actual notify so the writer task has a
            // chance to drain the ShutdownAccepted frame into
            // the kernel socket buffer before the accept loop
            // breaks. 50 ms is well under the shutdown_workers
            // grace window and well over the time it takes the
            // dispatcher to enqueue + the writer task to flush.
            let trigger = server_state.shutdown_trigger.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                trigger.notify_one();
            });
        } else {
            tracing::warn!(
                peer_pid = ?peer_pid,
                outcome,
                "shutdown rpc: rejected",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::ShutdownRejected {
                    reason: outcome.to_owned(),
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{make_session_sink, test_server_state};
    use super::*;

    /// `ServerState::coordinator_model` — sourced from
    /// `WorkConfig::coordinator_model` (`BOSS_COORDINATOR_MODEL`, default
    /// `"opus"`) — is independent of the worker effort→model table: a
    /// change to the worker dispatch table must never silently change what
    /// model the coordinator launches on.
    #[test]
    fn coordinator_model_defaults_to_opus_independent_of_effort_table() {
        let (server_state, _temp) = test_server_state();
        assert_eq!(server_state.coordinator_model, "opus");
    }

    /// Pins the `EnginePoolConfig.coordinator_model` push itself: driving
    /// `handle_register_app_session` end-to-end and asserting on the
    /// `FrontendEvent::EnginePoolConfig` it enqueues, rather than only the
    /// `ServerState` default the field is read from. Guards against a
    /// regression that reverts the push's source back to the effort table
    /// while leaving `ServerState::coordinator_model` (and the test above)
    /// untouched.
    #[tokio::test]
    async fn coordinator_model_push_reflects_server_state() {
        let (server_state, _temp) = test_server_state();
        let sink = make_session_sink();
        let ctx = dispatch_ctx(&server_state, &sink);

        handle_register_app_session(ctx, FrontendRequest::RegisterAppSession).await;

        // First envelope is the AppSessionRegistered response; the pool
        // config is pushed immediately after.
        sink.next().await.expect("AppSessionRegistered response");
        let pushed = sink.next().await.expect("EnginePoolConfig push");
        match pushed.payload {
            FrontendEvent::EnginePoolConfig { coordinator_model, .. } => {
                assert_eq!(coordinator_model, server_state.coordinator_model);
                assert_eq!(coordinator_model, "opus");
            }
            other => panic!("expected EnginePoolConfig, got {other:?}"),
        }
    }

    fn dispatch_ctx(server_state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
        Dispatch::builder()
            .server_state(server_state.clone())
            .work_db(server_state.work_db.clone())
            .sink(sink.clone())
            .session_id("s1")
            .request_id("req-1")
            .recv_instant(std::time::Instant::now())
            .decode_ms(0.0)
            .build()
    }
}
