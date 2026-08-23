//! `FrontendRequest` handlers — app/boss session registration, engine responses, shutdown.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;
use crate::coordinator_tmux::ClaudeVersionProbe;

const COORDINATOR_UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

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
        //       engine→app RPC (`SpawnWorkerPane`, reveal) dies
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
        // (`SpawnWorkerPane`, BossOnly/AppOrBoss tiers) following
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
            // The relaunched app killed every worker shell that was a
            // child of the prior (now-dead) app process, but their engine
            // slot bindings, pool claims, and DB execution rows survive.
            // Reconcile them now via the dead-PID probe: waiting for the
            // periodic dead-PID sweep would leave the slots bound and the
            // work items stuck "active" for up to a full sweep interval,
            // exactly the 2026-07-03 relaunch-orphan desync. Spawned
            // detached so the app's RegisterAppSession round-trip is not
            // blocked on the DB lookups + coordinator kicks the reconcile
            // performs. Only genuinely-dead PIDs are reaped, so a worker
            // that somehow outlived the relaunch is left untouched.
            let work_db = server_state.work_db.clone();
            let live_worker_states = server_state.live_worker_states.clone();
            let execution_coordinator = server_state.execution_coordinator.clone();
            let dispatch_events = server_state.dispatch_events.clone();
            let cube_client = server_state.cube_client.clone();
            tokio::spawn(async move {
                crate::dead_pid_sweep::reconcile_orphans_on_reattach(
                    work_db,
                    live_worker_states,
                    execution_coordinator,
                    dispatch_events,
                    cube_client,
                    prior,
                    observed,
                )
                .await;
            });
        }
        server_state
            .register_app_session(session_id.clone(), sink.clone())
            .await;
        tracing::info!(session_id = %session_id, "app session registered");
        send_response(&sink, &request_id, FrontendEvent::AppSessionRegistered);
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
            server_state.execution_coordinator.kick();
            server_state.broadcast_engine_health().await;
        }
        // Push pool sizes immediately after registration so the app's
        // WorkersWorkspaceModel can configure its slot ranges before the
        // engine dispatches any SpawnWorkerPane. This is the single source
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
    let tmux = match boss_tmux::private_socket_path()
        .and_then(|socket| boss_tmux::Tmux::from_path_with_socket(program, socket))
    {
        Ok(tmux) => tmux,
        Err(error) => {
            tracing::error!(%error, "coordinator tmux attach skipped: resolved tmux path is invalid");
            return;
        }
    };
    let working_directory = match crate::coordinator_tmux::coordinator_working_directory() {
        Ok(path) => path,
        Err(error) => {
            tracing::error!(error = %format!("{error:#}"), "failed to resolve coordinator session directory");
            return;
        }
    };
    let record = {
        let _guard = server_state.coordinator_tmux_lock.lock().await;
        match crate::coordinator_tmux::ensure_for_attach(
            server_state.work_db.as_ref(),
            &tmux,
            &server_state.coordinator_model,
            &working_directory,
            &crate::coordinator_tmux::RealClaudeVersionProbe,
        )
        .await
        {
            Ok(state) => state,
            Err(error) => {
                tracing::error!(error = %format!("{error:#}"), "failed to create or recover coordinator tmux session");
                return;
            }
        }
    };
    request_coordinator_attachment(server_state, &tmux, record).await;
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
    let tmux_socket_path = match boss_tmux::private_socket_path() {
        Ok(path) => path.display().to_string(),
        Err(error) => {
            tracing::error!(%error, "coordinator tmux attach skipped: socket path is unavailable");
            return;
        }
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
/// `claude` upgrades happen on the order of days, so one short version
/// process per attached session/day is enough to surface an update without
/// turning the supervisor's 10-second health check into recurring process
/// churn. Re-using `request_coordinator_attachment` pushes the same pane
/// descriptor the app already renders, but only after the optional banner
/// value changed; `Some` to `None` clears a banner after a downgrade or reset.
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

    // Only the subprocess spawn needs rate-limiting to once a day; the
    // comparison against the cached installed version below is free, so it
    // always runs. This makes an un-acked push (the app's 5s attach timeout,
    // a busy/disconnected app) self-healing on the very next 10s supervisor
    // pass instead of being deferred by a full day alongside the probe.
    let should_probe = coordinator_update_probe_due(existing_entry.as_ref(), &record.spawn_token);
    let freshly_probed = if should_probe {
        Some(crate::coordinator_tmux::RealClaudeVersionProbe.probe().await)
    } else {
        None
    };

    let decision = refresh_decision(existing_entry.as_ref(), &record, freshly_probed);
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
/// daily probe was not due), determines the installed version to carry
/// forward and whether the advertised banner needs to be pushed again.
struct RefreshDecision {
    installed_version: Option<String>,
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
        assert!(
            decision.should_push,
            "an un-acked push must retry on the next pass instead of waiting a full day"
        );
    }

    #[test]
    fn attached_update_probe_runs_daily_not_on_every_supervisor_pass() {
        let recent = CoordinatorInstalledVersionCacheEntry {
            spawn_token: "token".to_owned(),
            installed_version: Some("2.1.237".to_owned()),
            probed_at: Instant::now(),
            advertised_update_available_version: None,
        };
        assert!(
            !coordinator_update_probe_due(Some(&recent), "token"),
            "a healthy supervisor pass reuses the recent result"
        );

        let old = CoordinatorInstalledVersionCacheEntry {
            probed_at: Instant::now() - COORDINATOR_UPDATE_CHECK_INTERVAL,
            ..recent
        };
        assert!(coordinator_update_probe_due(Some(&old), "token"));
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
    let tmux = match boss_tmux::private_socket_path()
        .and_then(|socket| boss_tmux::Tmux::from_path_with_socket(program, socket))
    {
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
    let replacement = {
        let _guard = server_state.coordinator_tmux_lock.lock().await;
        crate::coordinator_tmux::recreate_after_confirmation(
            server_state.work_db.as_ref(),
            &tmux,
            &server_state.coordinator_model,
            &expected_spawn_token,
            &working_directory,
            reason,
            &crate::coordinator_tmux::RealClaudeVersionProbe,
        )
        .await
    };
    match replacement {
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

/// Handle the app reporting the real shell pid for a worker pane.
///
/// The app returns `shell_pid = 0` from `SpawnWorkerPane` because the
/// libghostty surface is created asynchronously by SwiftUI after the RPC
/// returns. Once the surface attaches and the shell pid is available, the
/// app sends this message so the engine can wire process tracking.
///
/// Registers the pid in both `WorkerRegistry` (for ancestor-walk correlation
/// on hook events) and `LiveWorkerStateRegistry` (for dead-pid sweep and
/// `bossctl agents stop` reaping). Fire-and-forget: the app does not wait
/// for a response.
pub(super) async fn handle_update_worker_shell_pid(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state, peer_pid, ..
    } = ctx;
    let FrontendRequest::UpdateWorkerShellPid { run_id, shell_pid } = req else {
        unreachable!()
    };
    if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
        tracing::warn!(
            peer_pid = ?peer_pid,
            run_id = %run_id,
            "update_worker_shell_pid rejected: caller not in app/Boss subtree",
        );
        return;
    }
    if shell_pid <= 0 {
        tracing::warn!(
            run_id = %run_id,
            shell_pid,
            "update_worker_shell_pid: received non-positive pid; ignoring",
        );
        return;
    }
    // A real shell pid is proof the app's spawn path is working again — reset
    // the spawn-capability breaker so its failure window doesn't carry stale
    // pre-recovery failures into the next outage.
    server_state.spawn_health.record_success();
    // If this run was the half-open recovery probe's canary (see
    // `maybe_admit_recovery_probe`), this is proof the breaker's trip has
    // resolved — auto-resume dispatch. Never auto-resumes an operator pause:
    // `resume_dispatch_after_breaker_recovery` no-ops unless the current
    // pause is Breaker-origin.
    if server_state.spawn_health.record_probe_success(&run_id)
        && crate::spawn_health::resume_dispatch_after_breaker_recovery(
            &server_state.work_db,
            &server_state.execution_coordinator,
            server_state.dispatch_events.as_ref(),
            Some(&run_id),
            "recovery probe reported a real shell pid",
        )
        .await
    {
        server_state.execution_coordinator.kick();
        server_state.broadcast_engine_health().await;
    }
    // Persist the pid to the DB FIRST, keyed by run_id (the execution id).
    // The `work_runs` row always exists by now (inserted synchronously at
    // dispatch, before the pane was spawned), so unlike the in-memory slot
    // registration below this write can never lose to the concurrent-spawn
    // race — even when `update_shell_pid` reports "no live slot found", the
    // durable pid is recorded. This is the restart-robust signal
    // `dead_pane_sweep` probes to detect a pane that died with its host app.
    match server_state
        .work_db
        .set_run_shell_pid_for_execution(&run_id, shell_pid as i64)
    {
        Ok(true) => {}
        Ok(false) => tracing::debug!(
            run_id = %run_id,
            shell_pid,
            "update_worker_shell_pid: no work_runs row for run_id yet; durable pid not stored this pass",
        ),
        Err(err) => tracing::warn!(
            run_id = %run_id,
            shell_pid,
            ?err,
            "update_worker_shell_pid: failed to persist durable shell pid (pane-liveness may be blind after restart)",
        ),
    }
    // Update the pid→run_id registry so hook-event ancestor walk works.
    server_state.worker_registry.register(shell_pid, run_id.clone());
    // Update the live-state registry so dead-pid sweep and bossctl reaping
    // can signal the process when needed. A miss here (the concurrent-spawn
    // race where the app's pid push outran the engine's `register_spawn`, or a
    // late/duplicate report after the slot was released) only affects the
    // in-memory live registry — the durable pid persisted above is the
    // authoritative signal `dead_pane_sweep` reads, and it is never lost — so
    // the miss is logged for observability but is no longer a data-loss event.
    match server_state.live_worker_states.update_shell_pid(&run_id, shell_pid) {
        Some(slot_id) => {
            tracing::info!(
                run_id = %run_id,
                slot_id,
                shell_pid,
                "update_worker_shell_pid: registered real shell pid for worker pane",
            );
            server_state.broadcast_live_worker_states().await;
        }
        None => {
            tracing::warn!(
                run_id = %run_id,
                shell_pid,
                "update_worker_shell_pid: no live slot found for run_id (already released?); \
                 durable pid recorded, in-memory live-state not updated this pass",
            );
        }
    }
}

/// Handle the app reporting that a worker pane died — its child process
/// exited with no app-side restart handler for it (only the Boss pane
/// restarts itself).
///
/// The current app also reports a surface that never attached
/// (`ghostty_surface_new` returned NULL) via `ReportWorkerSpawnFailed`
/// instead, since such a pane never had a child to exit. The classification
/// below does not assume that: an older app build, or any future caller that
/// gets it wrong, is still routed correctly, and an engine that depends on
/// the frontend classifying its own failures is an engine one app regression
/// away from repeating the incident described below.
///
/// Resolves the backing execution immediately instead of waiting for the
/// next periodic dead-PID sweep pass (up to 60s later) or an app restart.
/// Fire-and-forget: the app does not wait for a response.
///
/// # A "death" before the pane ever came up is not a death
///
/// The report is classified before it is acted on, because the two things
/// the app folds into this one message need different handling:
///
/// - **The pane died after running** — a shell pid was reported, or a hook
///   event arrived, or the slot progressed past `Spawning`. A worker really
///   did exist and really did go away. Reaped by
///   [`crate::dead_pid_sweep::reap_reported_pane_death`], as before.
/// - **The pane never came up** ([`crate::spawn_ack_sweep::slot_never_started`])
///   — no pid, no hook event, still `Spawning`. Nothing was started, so
///   nothing died: this is a never-started spawn and is reaped as one, via
///   [`crate::spawn_ack_sweep::reap_never_started_spawn`].
///
/// The distinction is not cosmetic. The never-started path force-releases
/// the cube workspace lease and — decisively — feeds the cross-work-item
/// [`crate::spawn_health`] circuit breaker; the death path does neither.
/// A display-less host fails every spawn identically, spread thinly across
/// many work items, so the per-work-item churn guard cannot see it and only
/// the aggregate breaker can. Routing those failures down the death path is
/// what let the 2026-07 no-active-display incident burn 818 executions
/// across 79 work items: the breaker that exists precisely to stop it was
/// never fed a single failure, and the churn guard — which parks a row
/// rather than naming a cause — was left as the only backstop.
///
/// It also settles a race the app cannot win from its side. A surface that
/// fails to create can produce both this report and the diagnostic
/// `ReportWorkerSpawnFailed` NACK; whichever arrives first releases the
/// slot, and the second is then correctly dropped as stale. Classifying here
/// means the outcome no longer depends on which one that was — both orders
/// end in the same never-started reap.
///
/// **Spawned detached**, for the reason `handle_register_app_session`'s
/// re-attach reconcile is: the macOS app holds ONE frontend connection for its
/// whole lifetime and this loop dispatches its requests serially, so anything
/// awaited here delays every subsequent app RPC. Resolving a pane death does
/// real work (driver-owned state teardown, a recovery-patch capture, DB
/// writes) — bounded, but not instantaneous — and the app is not waiting on
/// the answer.
pub(super) async fn handle_worker_pane_died(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state, peer_pid, ..
    } = ctx;
    let FrontendRequest::WorkerPaneDied { run_id, reason } = req else {
        unreachable!()
    };
    if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
        tracing::warn!(
            peer_pid = ?peer_pid,
            run_id = %run_id,
            "worker_pane_died rejected: caller not in app/Boss subtree",
        );
        return;
    }
    tokio::spawn(async move { resolve_reported_pane_death(&server_state, &run_id, reason).await });
}

/// Classify and resolve one app-reported pane death — the body of
/// [`handle_worker_pane_died`], split out so the routing decision is
/// directly awaitable in tests instead of racing a detached `tokio::spawn`.
async fn resolve_reported_pane_death(
    server_state: &Arc<ServerState>,
    run_id: &str,
    reason: boss_protocol::WorkerPaneDeathReason,
) {
    // Classify against the live slot, not the DB: proof of life lives in the
    // registry. A run with no live slot at all has already been reaped or
    // released, and `reap_reported_pane_death` handles that (and every other
    // terminal race) with its own guards — so anything we cannot positively
    // classify as never-started falls through to it unchanged.
    let never_started = server_state
        .live_worker_states
        .snapshot()
        .into_iter()
        .find(|s| s.run_id == run_id)
        .filter(crate::spawn_ack_sweep::slot_never_started);

    let Some(state) = never_started else {
        let reaped = crate::dead_pid_sweep::reap_reported_pane_death(
            server_state.work_db.as_ref(),
            server_state.live_worker_states.as_ref(),
            server_state.execution_coordinator.clone(),
            server_state.dispatch_events.as_ref(),
            server_state.cube_client.as_ref(),
            run_id,
            reason,
        )
        .await;
        if reaped {
            tracing::info!(run_id, "worker_pane_died: execution reaped immediately");
        }
        return;
    };

    let execution = match server_state.work_db.get_execution(run_id) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(run_id, ?err, "worker_pane_died: failed to look up execution; ignoring");
            return;
        }
    };
    if execution.status.is_terminal() {
        tracing::debug!(run_id, "worker_pane_died: execution already terminal; ignoring");
        return;
    }

    tracing::warn!(
        run_id,
        slot_id = state.slot_id,
        "app reported worker-pane death for a pane that never came up (no shell pid, no hook \
         event, still spawning); reaping as a never-started spawn",
    );
    crate::spawn_ack_sweep::reap_never_started_spawn(
        &spawn_reap_ctx(server_state),
        &execution,
        state.slot_id,
        state.shell_pid,
        crate::spawn_ack_sweep::ReapCause::PaneDiedBeforeStart {
            detail: reason.describe(),
        },
        boss_engine_utils::epoch_time::now_epoch_secs(),
    )
    .await;
}

/// Build the shared [`crate::spawn_ack_sweep::SpawnReapCtx`] every
/// never-started reap needs from `server_state`. One helper so the two
/// app-report handlers cannot drift into passing different collaborators for
/// what is meant to be the identical teardown.
fn spawn_reap_ctx(server_state: &Arc<ServerState>) -> crate::spawn_ack_sweep::SpawnReapCtx<'_> {
    crate::spawn_ack_sweep::SpawnReapCtx::builder()
        .work_db(server_state.work_db.as_ref())
        .coordinator(server_state.execution_coordinator.clone())
        .dispatch_events(server_state.dispatch_events.as_ref())
        .reaper(server_state.as_ref())
        .spawn_health(server_state.spawn_health.as_ref())
        .cube_client(server_state.cube_client.as_ref())
        .build()
}

/// App reports that it can once again host worker panes after a
/// sleep/wake cycle (`GhosttyRuntime` confirmed an active display via
/// `NSWorkspace.didWakeNotification` / `screensDidWakeNotification`).
/// Kicks the scheduler immediately so anything stranded by the sleep —
/// an execution orphaned via `WorkerPaneDied`, or a `ready` row that
/// never got a slot while the app couldn't host a surface — redispatches
/// right away instead of waiting for the next periodic sweep or the
/// scheduler heartbeat.
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

/// Handle the app proactively reporting that a worker pane's shell never came
/// up — the `ReportWorkerSpawnFailed` NACK (see the wire-type docs).
///
/// This is the fast-fail path for the post-wake false-live spawn. The spawn
/// RPC was already answered `Ok(shell_pid: 0)` synchronously (the surface is
/// created asynchronously), so without this the engine would only learn the
/// shell never started after the 60s [`crate::spawn_ack_sweep`] grace window.
/// Here we reap the execution the instant the app tells us — the identical
/// teardown the sweep performs (orphan → pane release → slot release), routed
/// through the shared [`crate::spawn_ack_sweep::reap_never_started_spawn`] —
/// and feed the same spawn-capability circuit breaker, so a systemic outage is
/// caught in seconds instead of churning for hours. Fire-and-forget; no
/// response.
pub(super) async fn handle_report_worker_spawn_failed(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state, peer_pid, ..
    } = ctx;
    let FrontendRequest::ReportWorkerSpawnFailed { run_id, reason } = req else {
        unreachable!()
    };
    if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
        tracing::warn!(
            peer_pid = ?peer_pid,
            run_id = %run_id,
            "report_worker_spawn_failed rejected: caller not in app/Boss subtree",
        );
        return;
    }

    // Find the live slot this run was spawning into. A NACK for a run with no
    // live slot (already reaped by the 60s sweep, or released) — or one that
    // has since shown proof of life (a pid reported, or a hook event, or it
    // already progressed past `Spawning`) — is stale. Skip it so we never
    // double-reap or tear down a pane that actually came up.
    let Some(state) = server_state
        .live_worker_states
        .snapshot()
        .into_iter()
        .find(|s| s.run_id == run_id)
    else {
        tracing::info!(
            run_id = %run_id,
            reason = %reason,
            "report_worker_spawn_failed: no live slot for run (already reaped/released?); ignoring stale NACK",
        );
        return;
    };
    if !crate::spawn_ack_sweep::slot_never_started(&state) {
        tracing::info!(
            run_id = %run_id,
            slot_id = state.slot_id,
            shell_pid = state.shell_pid,
            activity = ?state.activity,
            "report_worker_spawn_failed: slot already showed proof of life or progressed; ignoring stale NACK",
        );
        return;
    }

    let execution = match server_state.work_db.get_execution(&run_id) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(
                run_id = %run_id,
                ?err,
                "report_worker_spawn_failed: failed to look up execution; ignoring",
            );
            return;
        }
    };
    if execution.status.is_terminal() {
        tracing::debug!(
            run_id = %run_id,
            "report_worker_spawn_failed: execution already terminal; ignoring",
        );
        return;
    }

    tracing::warn!(
        run_id = %run_id,
        slot_id = state.slot_id,
        reason = %reason,
        "app reported worker-pane spawn failure (no shell); reaping execution immediately",
    );

    let now_epoch_secs = boss_engine_utils::epoch_time::now_epoch_secs();
    crate::spawn_ack_sweep::reap_never_started_spawn(
        &spawn_reap_ctx(&server_state),
        &execution,
        state.slot_id,
        state.shell_pid,
        crate::spawn_ack_sweep::ReapCause::AppNack { reason: &reason },
        now_epoch_secs,
    )
    .await;
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
    use crate::test_support::{create_active_chore, create_product};

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

    fn create_ready_execution(server_state: &Arc<ServerState>) -> String {
        let product_id = create_product(&server_state.work_db);
        let work_item_id = create_active_chore(&server_state.work_db, &product_id, "test chore");
        server_state
            .work_db
            .request_execution(
                boss_protocol::RequestExecutionInput::builder()
                    .work_item_id(work_item_id)
                    .build(),
            )
            .unwrap()
            .id
    }

    /// A pane-death report for a slot that never showed proof of life is a
    /// never-started spawn, not a death. It must be reaped through the
    /// never-started path — which is the ONLY one that feeds the
    /// cross-work-item spawn-capability breaker. A display-less host fails
    /// every spawn identically across many work items, so no per-work-item
    /// guard can ever see it; starving the breaker is what let the 2026-07
    /// incident burn 818 executions across 79 work items.
    #[tokio::test]
    async fn pane_death_before_start_feeds_the_spawn_capability_breaker() {
        let (server_state, _dir) = test_server_state();
        let execution_id = create_ready_execution(&server_state);
        let work_item_id = server_state.work_db.get_execution(&execution_id).unwrap().work_item_id;
        server_state
            .live_worker_states
            .register_spawn(1, &execution_id, "claude-opus-4-7", 0, None);

        resolve_reported_pane_death(
            &server_state,
            &execution_id,
            boss_protocol::WorkerPaneDeathReason::SurfaceCreationFailed,
        )
        .await;

        assert_eq!(
            server_state.work_db.get_execution(&execution_id).unwrap().status,
            boss_protocol::ExecutionStatus::Orphaned,
            "a pane that never came up must still be reaped",
        );
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let evidence = server_state.spawn_health.evidence_in_window(now);
        assert_eq!(
            evidence.iter().map(|e| e.work_item_id.as_str()).collect::<Vec<_>>(),
            vec![work_item_id.as_str()],
            "the never-started reap must record spawn-health evidence; the death path does not, \
             and that omission is the defect",
        );
    }

    /// The durable explanation must name what actually happened. The death
    /// path's audit line ("worker pane died") describes a worker that ran and
    /// stopped — the opposite of the truth — leaving an operator reading the
    /// work item with nothing to diagnose from. The `[engine-reconcile]` line
    /// appended to the work item's description is checked here because it is
    /// a durable surface that lands unconditionally; the `work_runs` orphan
    /// reason is guarded separately and is covered by its own work item.
    #[tokio::test]
    async fn pane_death_before_start_records_a_never_started_audit_line() {
        for (reason, expected_detail, unexpected_detail) in [
            (
                boss_protocol::WorkerPaneDeathReason::SurfaceCreationFailed,
                "surface creation failed before a child process attached",
                "child process exited",
            ),
            (
                boss_protocol::WorkerPaneDeathReason::ChildProcessExited,
                "attached child process exited",
                "surface creation failed",
            ),
        ] {
            let (server_state, _dir) = test_server_state();
            let execution_id = create_ready_execution(&server_state);
            let work_item_id = server_state.work_db.get_execution(&execution_id).unwrap().work_item_id;
            server_state
                .live_worker_states
                .register_spawn(1, &execution_id, "claude-opus-4-7", 0, None);

            resolve_reported_pane_death(&server_state, &execution_id, reason).await;

            let description = match server_state.work_db.get_work_item(&work_item_id).unwrap() {
                boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => t.description,
                other => panic!("expected a chore work item, got {other:?}"),
            };
            assert!(
                description.contains("death before start"),
                "the audit line must say the pane never came up; got: {description}",
            );
            assert!(
                description.contains(expected_detail),
                "the audit line must retain the reported cause; got: {description}",
            );
            assert!(
                !description.contains(unexpected_detail),
                "the audit line must not narrate a different cause; got: {description}",
            );
        }
    }

    /// The other side of the classification: a pane that reported a real
    /// shell pid genuinely hosted a worker, so its death is a death and must
    /// keep taking the pane-death path. Reaping it as a never-started spawn
    /// would feed the breaker a failure that is not a spawn failure at all,
    /// and could trip the fleet on three unrelated crashed workers.
    #[tokio::test]
    async fn pane_death_after_a_live_shell_stays_on_the_death_path() {
        let (server_state, _dir) = test_server_state();
        let execution_id = create_ready_execution(&server_state);
        server_state
            .live_worker_states
            .register_spawn(1, &execution_id, "claude-opus-4-7", 4242, None);

        resolve_reported_pane_death(
            &server_state,
            &execution_id,
            boss_protocol::WorkerPaneDeathReason::ChildProcessExited,
        )
        .await;

        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        assert!(
            server_state.spawn_health.evidence_in_window(now).is_empty(),
            "a pane that hosted a live shell is not a spawn failure and must not feed the breaker",
        );
    }

    async fn call_nack(server_state: &Arc<ServerState>, run_id: &str) {
        let sink = make_session_sink();
        let ctx = dispatch_ctx(server_state, &sink);
        handle_report_worker_spawn_failed(
            ctx,
            FrontendRequest::ReportWorkerSpawnFailed {
                run_id: run_id.to_owned(),
                reason: "test-nack".to_owned(),
            },
        )
        .await;
    }

    /// Guard 1: no live slot at all for `run_id` (already reaped/released,
    /// or the app raced a NACK for a run the engine never registered) — the
    /// NACK must be a pure no-op, never touching the execution.
    #[tokio::test]
    async fn nack_ignored_when_no_live_slot() {
        let (server_state, _dir) = test_server_state();
        let execution_id = create_ready_execution(&server_state);

        call_nack(&server_state, &execution_id).await;

        assert_eq!(
            server_state.work_db.get_execution(&execution_id).unwrap().status,
            boss_protocol::ExecutionStatus::Ready,
            "no live slot must leave the execution untouched",
        );
    }

    /// Guard 2a: the slot already reported a real shell pid — proof the
    /// pane actually came up. Reaping here would tear down a live worker.
    #[tokio::test]
    async fn nack_ignored_when_shell_pid_already_reported() {
        let (server_state, _dir) = test_server_state();
        let execution_id = create_ready_execution(&server_state);
        server_state
            .live_worker_states
            .register_spawn(1, &execution_id, "claude-opus-4-7", 4242, None);

        call_nack(&server_state, &execution_id).await;

        assert_eq!(
            server_state.work_db.get_execution(&execution_id).unwrap().status,
            boss_protocol::ExecutionStatus::Ready,
            "a slot with a reported pid must not be reaped",
        );
        assert!(
            server_state.live_worker_states.get(1).is_some(),
            "the live slot must not be torn down",
        );
    }

    /// Guard 2b: the slot has seen a hook event (proof of life) even though
    /// it hasn't reported a pid or left `Spawning` yet.
    #[tokio::test]
    async fn nack_ignored_when_hook_event_already_seen() {
        let (server_state, _dir) = test_server_state();
        let execution_id = create_ready_execution(&server_state);
        server_state
            .live_worker_states
            .register_spawn(1, &execution_id, "claude-opus-4-7", 0, None);
        // Resume source is proof of life without flipping activity away
        // from Spawning, isolating this guard from guard 2c below.
        server_state.live_worker_states.apply_event(
            1,
            &boss_protocol::WorkerEvent::SessionStart {
                session_id: "s".to_owned(),
                source: boss_protocol::SessionStartSource::Resume,
                model: None,
            },
        );

        call_nack(&server_state, &execution_id).await;

        assert_eq!(
            server_state.work_db.get_execution(&execution_id).unwrap().status,
            boss_protocol::ExecutionStatus::Ready,
            "a slot with any hook event must not be reaped",
        );
        assert!(server_state.live_worker_states.get(1).is_some());
    }

    /// Guard 2c: the slot's activity already progressed past `Spawning`.
    #[tokio::test]
    async fn nack_ignored_when_activity_past_spawning() {
        let (server_state, _dir) = test_server_state();
        let execution_id = create_ready_execution(&server_state);
        server_state
            .live_worker_states
            .register_spawn(1, &execution_id, "claude-opus-4-7", 0, None);
        server_state.live_worker_states.apply_event(
            1,
            &boss_protocol::WorkerEvent::SessionStart {
                session_id: "s".to_owned(),
                source: boss_protocol::SessionStartSource::Startup,
                model: None,
            },
        );
        assert_ne!(
            server_state.live_worker_states.get(1).unwrap().activity,
            boss_protocol::WorkerActivity::Spawning,
            "precondition: Startup source must move activity off Spawning",
        );

        call_nack(&server_state, &execution_id).await;

        assert_eq!(
            server_state.work_db.get_execution(&execution_id).unwrap().status,
            boss_protocol::ExecutionStatus::Ready,
            "a slot that progressed past Spawning must not be reaped",
        );
        assert!(server_state.live_worker_states.get(1).is_some());
    }

    /// Guard 3: the execution is already terminal (e.g. a duplicate or
    /// very-late NACK arriving after some other path already finished the
    /// execution) — must never re-reap a terminal execution.
    #[tokio::test]
    async fn nack_ignored_when_execution_already_terminal() {
        let (server_state, _dir) = test_server_state();
        let execution_id = create_ready_execution(&server_state);
        server_state
            .live_worker_states
            .register_spawn(1, &execution_id, "claude-opus-4-7", 0, None);
        let work_item_id = server_state.work_db.get_execution(&execution_id).unwrap().work_item_id;
        server_state
            .work_db
            .force_execution_status_for_test(&work_item_id, boss_protocol::ExecutionStatus::Completed)
            .unwrap();

        call_nack(&server_state, &execution_id).await;

        assert_eq!(
            server_state.work_db.get_execution(&execution_id).unwrap().status,
            boss_protocol::ExecutionStatus::Completed,
            "an already-terminal execution must not be reaped again",
        );
        assert!(
            server_state.live_worker_states.get(1).is_some(),
            "the live slot must not be torn down for a stale NACK",
        );
    }
}
