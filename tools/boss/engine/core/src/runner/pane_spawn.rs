//! [`PaneSpawnRunner`]: the libghostty-pane [`ExecutionRunner`], plus the
//! boss-event shim install/resolve helpers it relies on.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;

use crate::config::RuntimeConfig;
use crate::coordinator::slot_id_from_worker_id;
use crate::driver::AgentDriver;
use crate::pane_summary;
use crate::spawn_flow::{StartWorkerInput, start_worker};
use crate::work::{WorkDb, WorkExecution, WorkItem};
use boss_protocol::{ExecutionKind, ExecutionStatus, WorkItemBinding};

use super::work_item::{work_item_id, work_item_name, work_item_task_kind};
use super::worker_spawn::{ComposedWorkerSpawn, compose_worker_spawn};
use super::{ExecutionRunner, RunOutcome, RunWaitState, bound_events_socket_path};

/// `ExecutionRunner` that drives the libghostty pane RPC: writes the
/// per-lease worker config files, asks the macOS app to host a
/// worker pane, and registers the returned shell pid against the
/// run id so events-socket hook deliveries can correlate.
///
/// Returns `WaitingHuman` immediately on a successful spawn — the
/// pane stays alive in the app and the workspace lease is retained
/// until a human or follow-up flow concludes the run. Real lifecycle
/// (the pane signaling "Stop" → run completes) lands once the
/// events-socket consumer drives state transitions.
pub struct PaneSpawnRunner {
    cfg: Arc<RuntimeConfig>,
    /// Backing store for the pane-titlebar summary cache. Looked up
    /// in `run_execution` to compute a 2–4 word label for the work
    /// item before asking the app to spawn the pane.
    work_db: Arc<WorkDb>,
    /// Feature flags store — checked at spawn time to decide whether
    /// editorial controls are active for this execution.
    feature_flags: Arc<crate::feature_flags::FeatureFlagsStore>,
    /// Set after construction via [`PaneSpawnRunner::set_server_state`].
    /// Stored as `Weak` to avoid the runner ↔ ServerState reference
    /// cycle. Resolved each call.
    server_state: std::sync::OnceLock<Weak<dyn crate::spawn_flow::WorkerSpawner>>,
    /// Test-injection override for the boss-event binary path. When set,
    /// `boss_event_binary()` returns this directly without consulting the
    /// environment — so tests don't depend on host PATH/filesystem layout.
    boss_event_path_override: std::sync::OnceLock<PathBuf>,
}

impl PaneSpawnRunner {
    pub fn new(
        cfg: Arc<RuntimeConfig>,
        work_db: Arc<WorkDb>,
        feature_flags: Arc<crate::feature_flags::FeatureFlagsStore>,
    ) -> Self {
        Self {
            cfg,
            work_db,
            feature_flags,
            server_state: std::sync::OnceLock::new(),
            boss_event_path_override: std::sync::OnceLock::new(),
        }
    }

    pub fn set_server_state(&self, server_state: Weak<dyn crate::spawn_flow::WorkerSpawner>) {
        let _ = self.server_state.set(server_state);
    }

    /// Inject a known absolute boss-event path for tests so they don't
    /// depend on the host filesystem or `BOSS_EVENT_BIN` env var.
    #[cfg(test)]
    pub(crate) fn set_boss_event_path(&self, path: PathBuf) {
        let _ = self.boss_event_path_override.set(path);
    }

    fn events_socket_path(&self) -> PathBuf {
        bound_events_socket_path(&self.cfg)
    }

    fn boss_event_binary(&self) -> PathBuf {
        if let Some(injected) = self.boss_event_path_override.get() {
            return injected.clone();
        }
        let engine_path = std::env::current_exe().unwrap_or_default();
        let workspace = std::env::var_os("BUILD_WORKSPACE_DIRECTORY").map(PathBuf::from);
        let env_override = std::env::var_os("BOSS_EVENT_BIN").map(PathBuf::from);
        let boss_bin_dir = std::env::var_os("BOSS_BIN_DIR").map(PathBuf::from);
        let stable_bin_dir = boss_log_files::default_state_root().map(|root| root.join("bin"));
        resolve_boss_event_binary(
            &engine_path,
            workspace.as_deref(),
            env_override.as_deref(),
            boss_bin_dir.as_deref(),
            stable_bin_dir.as_deref(),
        )
        .unwrap_or_else(|| {
            panic!(
                "boss-event binary not found: none of BOSS_EVENT_BIN, BOSS_BIN_DIR, \
                 the stable bin dir, runfiles, bazel-bin, or the engine-sibling resolved \
                 to an existing file. A bare 'boss-event' in hook commands causes silent \
                 event-emission failures when the worker's sanitized PATH does not include it. \
                 Set BOSS_EVENT_BIN to the absolute boss-event path to fix this."
            )
        })
    }
}

/// Pure resolver for the absolute path of the `boss-event` shim
/// that the worker pane invokes from `settings.json`. Pulled out
/// as a free function so tests can pass synthetic `engine_path` /
/// `workspace_dir` / env values without monkey-patching globals.
///
/// Returns `Some(path)` when a candidate exists on disk, `None` when no
/// candidate resolves. The caller is responsible for treating `None` as a
/// hard error — a bare `boss-event` in hook commands causes silent
/// event-emission failures because the worker's sanitized PATH does not
/// include bazel-out or other non-standard directories.
///
/// Resolution order:
///   1. `BOSS_EVENT_BIN` env override (caller-controlled).
///   2. `$BOSS_BIN_DIR/boss-event` — installed-mode path. The app
///      sets `BOSS_BIN_DIR` to `Boss.app/Contents/Resources/bin/` and
///      passes it to the engine; all bundled CLIs and the shim live
///      there. This is checked ahead of the dev-mode paths so an
///      installed bundle never falls through to a workspace clone.
///   3. `stable_bin_dir/boss-event` — the copy installed by the engine
///      at startup into `~/Library/Application Support/Boss/bin/`. In
///      dev mode the engine copies boss-event there on every startup so
///      the path baked into worker settings.json is stable across
///      `bazel clean` and workspace re-leases.
///   4. Bazel runfiles next to the engine binary
///      (`<engine_path>.runfiles/_main/tools/boss/event-shim/boss-event`).
///      Requires the engine `rust_binary` to declare a `data` dep
///      on `//tools/boss/event-shim:boss-event` — without it bazel
///      doesn't include the shim in the engine's runfiles.
///   5. Workspace `bazel-bin` symlink
///      (`<workspace>/bazel-bin/tools/boss/event-shim/boss-event`)
///      when `BUILD_WORKSPACE_DIRECTORY` is set (i.e., the engine
///      was launched via `bazel run` from a checkout).
///   6. Cargo / hand-built sibling: `<engine_dir>/boss-event`.
pub(crate) fn resolve_boss_event_binary(
    engine_path: &Path,
    workspace_dir: Option<&Path>,
    env_override: Option<&Path>,
    boss_bin_dir: Option<&Path>,
    stable_bin_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(override_path) = env_override {
        return Some(override_path.to_path_buf());
    }

    // Installed mode: BOSS_BIN_DIR is Boss.app/Contents/Resources/bin/.
    if let Some(bin_dir) = boss_bin_dir {
        let candidate = bin_dir.join("boss-event");
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // Stable dev-mode location. The engine copies boss-event here at
    // startup so hook paths baked into worker settings.json survive
    // `bazel clean` and workspace re-leases.
    if let Some(bin_dir) = stable_bin_dir {
        let candidate = bin_dir.join("boss-event");
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // Bazel constructs runfiles at `<binary>.runfiles/_main/<workspace_relative_path>`.
    let mut runfiles_root = engine_path.as_os_str().to_owned();
    runfiles_root.push(".runfiles");
    let runfiles_candidate = PathBuf::from(runfiles_root)
        .join("_main")
        .join("tools/boss/event-shim/boss-event");
    if runfiles_candidate.exists() {
        return Some(runfiles_candidate);
    }

    if let Some(workspace) = workspace_dir {
        let candidate = workspace.join("bazel-bin/tools/boss/event-shim/boss-event");
        if candidate.exists() {
            return Some(candidate);
        }
    }

    if let Some(engine_dir) = engine_path.parent() {
        let sibling = engine_dir.join("boss-event");
        if sibling.exists() {
            return Some(sibling);
        }
    }

    None
}

/// Copy the boss-event shim binary to a stable location in the Boss
/// support directory. Called at engine startup so the path baked into
/// new worker settings.json files remains valid after a `bazel clean`.
///
/// `source_shim` is the currently-valid binary (from the runfiles tree
/// or bazel-bin). `stable_bin_dir` is the target directory
/// (`~/Library/Application Support/Boss/bin/`). Returns the stable path
/// on success. If `source_shim` is already inside `stable_bin_dir`,
/// returns `Ok(source_shim)` without copying (no-op for installed mode).
pub(crate) fn install_boss_event_to_stable_bin(source_shim: &Path, stable_bin_dir: &Path) -> io::Result<PathBuf> {
    let stable_path = stable_bin_dir.join("boss-event");
    if stable_path == source_shim {
        return Ok(stable_path);
    }
    std::fs::create_dir_all(stable_bin_dir)?;
    std::fs::copy(source_shim, &stable_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&stable_path)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(&stable_path, perms)?;
    }
    Ok(stable_path)
}

#[async_trait]
impl ExecutionRunner for PaneSpawnRunner {
    async fn run_execution(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &Path,
        cube_change_id: Option<&str>,
    ) -> Result<RunOutcome> {
        let weak = self
            .server_state
            .get()
            .ok_or_else(|| anyhow!("PaneSpawnRunner not bound to ServerState"))?;
        let spawner = weak
            .upgrade()
            .ok_or_else(|| anyhow!("ServerState dropped before run_execution"))?;

        let lease_id = execution
            .cube_lease_id
            .clone()
            .context("execution missing cube_lease_id; coordinator must lease before spawn")?;

        // The coordinator already claimed a slot via WorkerPool —
        // `worker_id` is `worker-{N}` (main pool), `auto-worker-{N}`
        // (automation pool), or `review-{N}` (review pool); N is the slot
        // the engine owns. Decode it here and thread it into the spawn so
        // the app hosts the pane in this exact slot rather than running its
        // own (now-deleted) firstIndex(where:) heuristic.
        let slot_id = slot_id_from_worker_id(worker_id).ok_or_else(|| {
            anyhow!(
                "PaneSpawnRunner received worker_id {worker_id:?} that does not parse as worker-{{N}}, auto-worker-{{N}}, or review-{{N}}"
            )
        })?;

        // Compose the worker prompt and stash it on disk so the
        // libghostty pane can `claude "$(cat .claude/initial-prompt.txt)"`
        // — Claude Code's positional arg is treated as the first user
        // message, which gets the worker working without us having to
        // wait for a "Claude is ready" signal and then SendToPane.
        // Going through a file (rather than embedding the prompt in
        // the typed command) avoids shell quoting hell on multi-line,
        // backtick-bearing markdown.
        //
        // Prompt composition + effort/model resolution live in the
        // shared `compose_worker_spawn` so the SSH-remote adapter
        // (`SshHostAdapter::spawn_worker`) launches workers with a
        // byte-identical prompt; see that function for the per-execution
        // collaborator lookups (parent project, conflict / CI attempt,
        // crash-recovery branch, automation-triage preamble).
        let editorial_enabled = self.feature_flags.is_enabled("editorial_controls");
        // `worker_proposals` is the master kill switch for every proposal-backed
        // seam; `worker_signal_proposals_seam` is this seam's own flag. Both
        // must be on for the worker prompt to teach the `boss propose` verbs —
        // mirrors the read-path gate in
        // `completion::WorkerCompletionHandler::detect_and_file_worker_signals`
        // so the two halves of the migration move together.
        let worker_signal_proposals_seam_enabled = self.feature_flags.is_enabled("worker_proposals")
            && self.feature_flags.is_enabled("worker_signal_proposals_seam");
        let ComposedWorkerSpawn {
            prompt_text,
            spawn_config,
        } = compose_worker_spawn(
            &self.work_db,
            worker_id,
            execution,
            work_item,
            workspace_path,
            cube_change_id,
            (
                editorial_enabled,
                self.cfg.work.max_review_embed_diff_lines,
                worker_signal_proposals_seam_enabled,
            ),
        )
        .await?;

        // Write the initial prompt (and gitignore + pre-trust) via the driver's
        // WorkspaceProvisioning capability. The driver's config_dir and
        // initial_prompt_filename (e.g. `.claude/initial-prompt.txt`) determine
        // the exact path the spawn_invocation `$(cat ...)` reads from.
        crate::driver::ClaudeDriver
            .provision_workspace(workspace_path, &prompt_text, &execution.id)
            .await
            .with_context(|| {
                format!(
                    "provisioning workspace {} for execution {}",
                    workspace_path.display(),
                    execution.id,
                )
            })?;

        // Structured-output artifact (review findings / task followups): create
        // the engine-owned scratch dir and clear any stale file from a prior
        // run of this exact execution id, then hand the worker its absolute
        // path via `BOSS_STRUCTURED_OUTPUT`. The same path is embedded in the
        // worker prompt (see `compose_worker_spawn`); the completion handler
        // reads + validates it. Best-effort: a prepare failure is non-fatal
        // (the worker falls back to the transcript-scrape contract).
        let structured_output_dir = crate::structured_output::default_dir();
        let structured_output_path = match crate::structured_output::prepare(&structured_output_dir, &execution.id) {
            Ok(path) => Some(path.display().to_string()),
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    dir = %structured_output_dir.display(),
                    ?err,
                    "spawn: could not prepare structured-output dir; worker will rely on \
                     the transcript-scrape fallback",
                );
                None
            }
        };

        // Scrub ANTHROPIC_API_KEY from the worker shell's environment before
        // invoking claude. The engine needs the var in its own process for
        // pane-summary LLM calls; workers must authenticate via OAuth
        // credentials (~/.claude/.credentials.json) and inherit nothing.
        // Without this unset, a user who sets ANTHROPIC_API_KEY in their
        // shell profile (or via `launchctl setenv`) causes every worker
        // spawn to show: "Auth conflict: Using ANTHROPIC_API_KEY instead of
        // Anthropic Console key."
        // The worker's session settings (boss-event hooks, deny rules)
        // live outside the workspace tree; point claude at them with
        // `--settings`. `write_workspace_files` writes the same path.
        let worker_settings_path = crate::worker_setup::worker_settings_path(workspace_path);
        // Re-prepend BOSS_BIN_DIR to PATH in the worker's first shell line,
        // mirroring the Boss/coordinator pane (see BossPaneModel.swift and
        // the feba26d2 fix). `spawn_flow` already sets PATH with
        // BOSS_BIN_DIR ahead of a sanitized PATH in the pane *surface*
        // env, but the worker pane runs a login shell whose init scripts
        // (.zprofile, .zshrc) rebuild PATH from /etc/paths and the user's
        // dotfiles — which re-prepends `~/bin`, where a `repobin` shim of
        // `cube` / `boss` / `bossctl` typically lives. That shim is
        // independently versioned and has drifted from the bundled CLI
        // (e.g. it lacks `cube pr create`), so a worker that resolves the
        // shim instead of the bundled binary silently breaks. BOSS_BIN_DIR
        // itself survives init (init scripts don't unset custom env vars),
        // so we re-prepend it here: this line runs *after* init completes
        // and *before* claude launches, so claude — and every tool-issued
        // `cube`/`boss` subshell it spawns — inherits the bundled-first
        // PATH. The `[ -n "$BOSS_BIN_DIR" ]` guard is a no-op in dev /
        // bazel-run mode where BOSS_BIN_DIR is unset.
        // The answer agent is capability-restricted: force deny-by-default
        // `dontAsk` so its `permissions.allow` allowlist is authoritative and
        // cannot be downgraded to `auto` / `--dangerously-skip-permissions`
        // (which would bypass the settings rules). Every other kind keeps the
        // model-derived permission mode.
        //
        // Derive the worker kind ONCE and use it for BOTH the settings posture
        // (StartWorkerInput.worker_kind below) and the forced CLI mode, so the
        // two switches can never diverge — the exhaustive `WorkerKind` matches
        // force a new restricted kind to decide both.
        let worker_kind = crate::worker_setup::worker_kind_for_execution(&execution.kind);
        let permission_mode_override = worker_kind.forced_permission_mode();
        let initial_input = format!(
            "[ -n \"$BOSS_BIN_DIR\" ] && export PATH=\"$BOSS_BIN_DIR:$PATH\"; unset ANTHROPIC_API_KEY; {}",
            crate::driver::ClaudeDriver.spawn_invocation(
                &spawn_config.model,
                spawn_config.claude_effort,
                Some(&worker_settings_path),
                spawner.non_opus_auto_mode(),
                permission_mode_override,
            ),
        );

        // Look up (or generate) a 2–4 word pane-titlebar summary for
        // this work item. The full run id is still used for logs and
        // every other identifier — this label is purely visual. We
        // resolve the API key lazily and let the helper handle every
        // failure mode (missing key, API error, cache miss) so a
        // slow or unreachable Anthropic never blocks the spawn.
        let api_key = self.cfg.agent().ok().and_then(|agent| agent.anthropic_api_key.clone());
        let title_summary = if execution.kind == ExecutionKind::CiRemediation {
            pane_summary::ci_remediation_summary(work_item_name(work_item))
        } else {
            pane_summary::get_or_generate(&self.work_db, api_key.as_deref(), work_item).await
        };

        let work_item_binding = Some(WorkItemBinding {
            work_item_id: work_item_id(work_item).to_owned(),
            work_item_name: work_item_name(work_item).to_owned(),
            execution_id: execution.id.clone(),
        });

        let started = start_worker(
            spawner.as_ref(),
            StartWorkerInput {
                run_id: execution.id.clone(),
                lease_id,
                slot_id,
                workspace_path: workspace_path.to_path_buf(),
                events_socket_path: self.events_socket_path(),
                boss_event_path: self.boss_event_binary(),
                initial_input,
                extra_env: structured_output_path
                    .map(|p| vec![(crate::structured_output::STRUCTURED_OUTPUT_ENV.to_owned(), p)])
                    .unwrap_or_default(),
                title_summary,
                task_title: Some(work_item_name(work_item).to_owned()),
                work_item_binding,
                model: spawn_config.model.clone(),
                draft_pr_mode: spawner.draft_pr_mode(),
                execution_kind: execution.kind.as_str().to_owned(),
                task_kind: work_item_task_kind(work_item).map(str::to_owned),
                // Per-kind worker posture (reviewer/triage/answer-agent are
                // restricted; everything else is a Standard implementer),
                // derived once above via the shared `worker_kind_for_execution`
                // so the settings posture and the forced CLI permission mode
                // are driven by one value and cannot diverge.
                worker_kind,
            },
            StdDuration::from_secs(30),
        )
        .await
        .with_context(|| format!("spawning worker pane for run {}", execution.id))?;

        tracing::info!(
            worker_id,
            execution_id = %execution.id,
            slot_id = started.slot_id,
            shell_pid = started.shell_pid,
            effort_level = spawn_config
                .effort_level
                .map(|level| level.as_str())
                .unwrap_or("none"),
            claude_effort = spawn_config.claude_effort.unwrap_or("default"),
            model = %spawn_config.model,
            ack_timed_out = started.ack_timed_out,
            "pane spawned for execution",
        );

        // Provisional spawn: the `SpawnWorkerPane` ack timed out, so the
        // app may or may not have hosted the pane. We deliberately do NOT
        // treat this as a failure (which would release the lease under a
        // possibly-live pane and duplicate-dispatch the work item — a
        // prior incident). The execution stays tracked in `waiting_human`
        // with the slot registered; the spawn-ack sweep confirms liveness
        // (a hook/pid arrives) or reaps on total silence past the grace
        // window. Surface it loudly so the provisional state is visible in
        // the engine log and the run's result summary.
        if started.ack_timed_out {
            tracing::warn!(
                worker_id,
                execution_id = %execution.id,
                slot_id = started.slot_id,
                "spawn ack timed out; worker registered provisionally (shell_pid 0). \
                 Deferring to the spawn-ack sweep to confirm liveness or reap — the \
                 execution stays tracked and the workspace lease is retained.",
            );
        } else if started.shell_pid == 0 {
            // A SUCCESSFUL ack that reports shell_pid 0 (the app hosted the
            // pane but its surface hasn't published the shell pid yet; the
            // real pid arrives shortly via `update_worker_shell_pid`). This
            // is the exact `shell_pid: 0, ack_timed_out: false` state seen in
            // the field, and until the pid lands the slot looks identical to
            // an ack-timeout provisional spawn (activity=Spawning, pid 0) to
            // the sweeps. It was previously silent — only the ack-timeout
            // branch warned — so the window did not appear in the trace.
            // Surface it explicitly so a run that misbehaves during this
            // window is diagnosable. This is instrumentation only: the pid is
            // reconciled by `update_worker_shell_pid`, and the sweeps already
            // protect a hooking/pid-reporting worker.
            tracing::warn!(
                worker_id,
                execution_id = %execution.id,
                slot_id = started.slot_id,
                "pane spawned on a successful ack but with shell_pid 0 — provisional \
                 liveness window: awaiting update_worker_shell_pid from the app before \
                 the pid→run mapping is registered. The execution stays tracked and the \
                 slot is registered; no reap is warranted while it hooks or reports a pid.",
            );
        }

        // Mid-spawn cancel reconciliation. A cancel / force-stop
        // can land while we were awaiting the `SpawnWorkerPane`
        // round-trip: it marks the execution row `cancelled` but, with
        // no pid yet materialized, cannot reap the worker and
        // deliberately leaves the cube lease held (see
        // `WorkerCompletionHandler::force_release`). Now that the spawn
        // has returned — pid registered, slot mapped, live state stamped
        // — reap the just-spawned pane so it cannot outlive its
        // cancellation, and signal the coordinator to release the lease
        // the cancel path left for us. Without this the worker survives
        // unreaped in a workspace the engine believes is free.
        match self.work_db.get_execution(&execution.id) {
            Ok(exec) if exec.status == ExecutionStatus::Cancelled => {
                tracing::warn!(
                    worker_id,
                    execution_id = %execution.id,
                    slot_id = started.slot_id,
                    shell_pid = started.shell_pid,
                    "spawn completed after the execution was cancelled mid-spawn; reaping the worker pane and releasing the deferred lease",
                );
                spawner.reap_worker_pane(&execution.id).await;
                return Ok(RunOutcome {
                    wait_state: RunWaitState::CancelledDuringSpawn,
                    result_summary: Some(format!(
                        "Execution cancelled during spawn; reaped worker pane in slot {} (shell pid {}).",
                        started.slot_id, started.shell_pid,
                    )),
                    attention: None,
                    // The pane is already torn down — don't ask the
                    // coordinator to keep the pool slot claimed for it.
                    slot_id: None,
                    spawn_config: Some(spawn_config),
                });
            }
            Ok(_) => {}
            Err(err) => {
                // A read failure here is non-fatal: fall through to the
                // normal completion path. The worst case is the existing
                // pre-fix behaviour, not a regression.
                tracing::warn!(
                    execution_id = %execution.id,
                    ?err,
                    "post-spawn cancel re-check failed; proceeding with normal completion",
                );
            }
        }

        // A `pr_review` reviewer pane stays in `running` after spawn so that
        // the "AI reviewing" kanban badge remains visible while the reviewer
        // agent is actively working. `waiting_human` is only correct once the
        // review is done and a human must act; the execution transitions to
        // `completed` when the Stop hook fires and `finalize_pr_review_pass`
        // calls `record_worker_pr_completion`. All other execution kinds use
        // `WaitingHuman` — the normal post-spawn park state.
        let wait_state = if execution.kind == ExecutionKind::PrReview {
            RunWaitState::ReviewerPaneAlive
        } else {
            RunWaitState::WaitingHuman
        };
        let result_summary = if started.ack_timed_out {
            format!(
                "Spawned worker pane in slot {} PROVISIONALLY — the SpawnWorkerPane ack timed out, \
                 so the pane's liveness is unconfirmed (shell pid {}). The slot is registered and \
                 the spawn-ack sweep will confirm it via the first hook event or reap it on total \
                 silence. Hook events from this run will surface on the engine events socket.",
                started.slot_id, started.shell_pid,
            )
        } else {
            format!(
                "Spawned worker pane in slot {} (shell pid {}). Hook events from this run will surface on the engine events socket.",
                started.slot_id, started.shell_pid,
            )
        };
        Ok(RunOutcome {
            wait_state,
            result_summary: Some(result_summary),
            attention: None,
            slot_id: Some(started.slot_id),
            spawn_config: Some(spawn_config),
        })
    }
}

#[cfg(test)]
mod pane_spawn_tests {
    //! End-to-end-ish tests for `PaneSpawnRunner`: drive `run_execution`
    //! against a stub `WorkerSpawner`, then assert on what was actually
    //! sent to the app and what files were written into the workspace.
    //! These tests would have caught the bugs surfaced manually:
    //!   - missing prompt injection (worker idle at bash prompt),
    //!   - boss-event resolved to bare relative path (hooks fail),
    //!   - sanitized PATH not threaded through to the app.
    //!
    //! Anything reachable via `WorkerSpawner` is fair game without
    //! standing up a full engine; the broadcast / coordinator side
    //! lives in `coordinator.rs` tests.
    use super::super::engine_events_socket_path;
    use super::*;
    use crate::app::SendToAppError;
    use crate::live_worker_state::LiveWorkerStateRegistry;
    use crate::protocol::{
        EngineToAppRequest, EngineToAppResponse, EnvVar, SpawnWorkerPaneInput, SpawnWorkerPaneResult,
    };
    use crate::test_support::*;
    use crate::work::{
        CreateChoreInput, CreateProjectInput, CreateTaskInput, EffortLevel, Task, WorkExecution, WorkItem,
    };
    use crate::worker_registry::WorkerRegistry;
    use boss_protocol::{ExecutionKind, ExecutionStatus, TaskKind, TaskStatus};
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    /// Records the spawn request the runner sent so tests can assert
    /// on env, initial_input, etc.
    struct CapturingSpawner {
        registry: WorkerRegistry,
        live_states: LiveWorkerStateRegistry,
        last: StdMutex<Option<SpawnWorkerPaneInput>>,
        /// Run ids passed to `reap_worker_pane` — lets the mid-spawn
        /// cancel test assert the runner reaped the just-spawned pane.
        reaped: StdMutex<Vec<String>>,
    }

    impl CapturingSpawner {
        fn new() -> Self {
            Self {
                registry: WorkerRegistry::new(),
                live_states: LiveWorkerStateRegistry::new(),
                last: StdMutex::new(None),
                reaped: StdMutex::new(Vec::new()),
            }
        }

        fn spawn_input(&self) -> SpawnWorkerPaneInput {
            self.last
                .lock()
                .unwrap()
                .clone()
                .expect("expected SpawnWorkerPane to be sent")
        }

        fn reaped_run_ids(&self) -> Vec<String> {
            self.reaped.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl crate::spawn_flow::WorkerSpawner for CapturingSpawner {
        async fn send_to_app_request(
            &self,
            request: EngineToAppRequest,
            _timeout: tokio::time::Duration,
        ) -> Result<EngineToAppResponse, SendToAppError> {
            match request {
                EngineToAppRequest::SpawnWorkerPane(input) => {
                    // Echo the slot the engine claimed; the
                    // engine-owns-slots refactor makes the response
                    // slot a confirmation echo rather than an
                    // independent allocator pick.
                    let slot_id = input.slot_id;
                    *self.last.lock().unwrap() = Some(input);
                    Ok(EngineToAppResponse::SpawnWorkerPane {
                        result: Ok(SpawnWorkerPaneResult { slot_id, shell_pid: 0 }),
                    })
                }
                other => panic!("unexpected request kind: {other:?}"),
            }
        }

        fn worker_registry(&self) -> &WorkerRegistry {
            &self.registry
        }

        async fn reap_worker_pane(&self, run_id: &str) {
            self.reaped.lock().unwrap().push(run_id.to_owned());
            // Mirror production teardown enough for the test: drop the
            // slot mapping so a follow-up release is a no-op.
            let _ = self.registry.take_slot_for_run(run_id);
        }

        fn live_worker_state_registry(&self) -> Option<&LiveWorkerStateRegistry> {
            Some(&self.live_states)
        }
    }

    fn sample_execution(workspace_path: &Path) -> WorkExecution {
        WorkExecution::builder()
            .id("exec-test-1")
            .work_item_id("task-1")
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@example.com:foo.git")
            .cube_repo_id("foo")
            .cube_lease_id("lease-1")
            .cube_workspace_id("foo-agent-001")
            .workspace_path(workspace_path.display().to_string())
            .created_at("2026-05-06T20:00:00Z")
            .started_at("2026-05-06T20:00:00Z")
            .build()
    }

    fn sample_chore() -> WorkItem {
        WorkItem::Chore(
            Task::builder()
                .id("task-1")
                .product_id("prod-1")
                .kind(TaskKind::Chore)
                .name("Improve top header (agent card) styling")
                .description("The gray header at the top is too cramped.")
                .status(TaskStatus::Todo)
                .created_at("2026-05-06T20:00:00Z")
                .updated_at("2026-05-06T20:00:00Z")
                .build(),
        )
    }

    /// Build the standard worker-spawn test scaffolding from a workspace
    /// tempdir: a `CapturingSpawner`, a `Weak<dyn WorkerSpawner>` the
    /// runner can upgrade, a default `RuntimeConfig` pointed at the
    /// workspace, and an open `WorkDb` over `state.db`. Call sites that
    /// need bespoke `WorkConfig` options (e.g. custom pool sizes) build
    /// these inline instead.
    fn spawn_test_env(
        workspace: &TempDir,
    ) -> (
        Arc<CapturingSpawner>,
        Weak<dyn crate::spawn_flow::WorkerSpawner>,
        Arc<crate::config::RuntimeConfig>,
        Arc<WorkDb>,
    ) {
        let spawner: Arc<CapturingSpawner> = Arc::new(CapturingSpawner::new());
        let weak: Weak<dyn crate::spawn_flow::WorkerSpawner> =
            Arc::downgrade(&spawner) as Weak<dyn crate::spawn_flow::WorkerSpawner>;
        let cfg = Arc::new(crate::config::RuntimeConfig::from_parts(
            crate::config::WorkConfig::builder()
                .cwd(workspace.path().to_path_buf())
                .db_path(workspace.path().join("state.db"))
                .build(),
            None,
        ));
        let work_db = Arc::new(WorkDb::open(workspace.path().join("state.db")).unwrap());
        (spawner, weak, cfg, work_db)
    }

    /// Build a runner already bound to a `CapturingSpawner` and drive a
    /// run_execution against `workspace`. Returns the spawner so tests
    /// can inspect the captured request.
    ///
    /// `boss_event_path`: when `Some`, injects a known absolute path for
    /// the boss-event binary so the test is independent of host
    /// filesystem layout / env vars. Pass `None` for tests that don't
    /// inspect the hook command.
    async fn run_once(workspace: &TempDir, boss_event_path: Option<&Path>) -> Result<Arc<CapturingSpawner>> {
        let (spawner, weak, cfg, work_db) = spawn_test_env(workspace);
        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, work_db, flags);
        runner.set_server_state(weak);
        if let Some(path) = boss_event_path {
            runner.set_boss_event_path(path.to_path_buf());
        }

        runner
            .run_execution(
                "worker-1",
                &sample_execution(workspace.path()),
                &sample_chore(),
                workspace.path(),
                Some("change-1"),
            )
            .await?;

        Ok(spawner)
    }

    #[tokio::test]
    async fn writes_initial_prompt_to_workspace_dot_claude() {
        let workspace = TempDir::new().unwrap();
        let _spawner = run_once(&workspace, None).await.unwrap();

        let prompt_path = workspace.path().join(".claude").join("initial-prompt.txt");
        assert!(prompt_path.exists(), "expected {} to exist", prompt_path.display());
        let prompt = std::fs::read_to_string(&prompt_path).unwrap();
        // Spot-check: the prompt should mention the work item title and
        // execution id so the worker actually has its task in hand.
        assert!(prompt.contains("Improve top header"), "prompt missing work item name");
        assert!(prompt.contains("exec-test-1"), "prompt missing execution id");
        assert!(
            prompt.contains("## Summary"),
            "prompt missing required output section header"
        );
    }

    #[tokio::test]
    async fn implementation_prompt_states_pr_url_acceptance_criterion() {
        // Workers that stop without producing a PR are now blocked
        // from completing — they get probed to push and open one. The
        // dispatch prompt must telegraph that up front so the worker
        // doesn't waste a round-trip discovering it from the probe.
        let workspace = TempDir::new().unwrap();
        let _spawner = run_once(&workspace, None).await.unwrap();
        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
        assert!(
            prompt.contains("the deliverable is a PR URL"),
            "implementation prompt must state the PR-URL acceptance criterion: {prompt}",
        );
        assert!(
            prompt.contains("on its own line"),
            "implementation prompt must tell the worker to print the URL on its own line: {prompt}",
        );
        assert!(
            prompt.contains("gh pr create") || prompt.contains("gh pr view") || prompt.contains("cube pr create"),
            "implementation prompt must mention gh pr commands or cube pr create: {prompt}",
        );
        assert!(
            prompt.contains("jj diff -r @"),
            "implementation prompt must tell the worker to verify the diff before pushing: {prompt}",
        );
    }

    /// AI #6 (incident 001): the prompt must name the engine-supplied
    /// branch the worker is expected to push to. The detector reads
    /// this same name back out of `state.db` (via
    /// `completion::expected_branch_name`) and queries
    /// `gh pr list --head <branch>` against it. If a worker pushes to
    /// a different bookmark, the fallback returns `None` instead of
    /// misbinding — but the happy path requires the worker to follow
    /// the engine's name, so the prompt must state it.
    #[tokio::test]
    async fn implementation_prompt_dictates_engine_supplied_branch_name() {
        let workspace = TempDir::new().unwrap();
        let _spawner = run_once(&workspace, None).await.unwrap();
        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
        let expected_branch =
            crate::completion::expected_branch_name("exec-test-1", &boss_protocol::BranchNaming::BossExecPrefix, None);
        assert!(
            prompt.contains(&expected_branch),
            "prompt must name the engine-supplied branch `{expected_branch}`, got: {prompt}",
        );
        assert!(
            prompt.contains("expected branch name"),
            "prompt must include the `expected branch name` context line, got: {prompt}",
        );
    }

    #[tokio::test]
    async fn initial_input_reads_prompt_from_disk() {
        let workspace = TempDir::new().unwrap();
        let spawner = run_once(&workspace, None).await.unwrap();
        let input = spawner.spawn_input();

        // The pane needs to type a `claude` invocation that picks up
        // the rendered prompt as its first user message — going
        // through a file avoids shell-quoting issues with multi-line
        // markdown. Without this, the worker just sits at the bash
        // prompt forever (as it did before #174).
        assert!(
            input.initial_input.contains(".claude/initial-prompt.txt"),
            "expected initial_input to read from prompt file, got: {:?}",
            input.initial_input
        );
        // The first shell line re-prepends BOSS_BIN_DIR to PATH (so the
        // bundled `cube`/`boss`/`bossctl` win over any `~/bin` repobin
        // shim the login-shell init re-prepends), then unsets the API key
        // and invokes claude. See the comment at the construction site.
        assert!(
            input.initial_input.starts_with(
                "[ -n \"$BOSS_BIN_DIR\" ] && export PATH=\"$BOSS_BIN_DIR:$PATH\"; unset ANTHROPIC_API_KEY; claude"
            ),
            "expected initial_input to re-prepend BOSS_BIN_DIR, unset ANTHROPIC_API_KEY, and invoke claude, got: {:?}",
            input.initial_input
        );
    }

    /// Build a runner driven against a real product + chore row so
    /// the dispatcher's effort/model lookup hits actual SQLite rather
    /// than the synthetic `sample_chore` fixture. Returns the spawner
    /// and the created chore id so the caller can re-use the row.
    async fn run_once_with_chore(
        workspace: &TempDir,
        chore_input: CreateChoreInput,
        product_default_model: Option<&str>,
    ) -> Result<(Arc<CapturingSpawner>, Task)> {
        let (spawner, weak, cfg, work_db) = spawn_test_env(workspace);

        let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
        if let Some(model) = product_default_model {
            work_db.set_product_default_model(&product.id, Some(model)).unwrap();
        }
        let mut chore_input = chore_input;
        chore_input.product_id = product.id.clone();
        let chore = work_db.create_chore(chore_input).unwrap();

        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, work_db, flags);
        runner.set_server_state(weak);

        let mut execution = sample_execution(workspace.path());
        execution.work_item_id = chore.id.clone();

        runner
            .run_execution(
                "worker-1",
                &execution,
                &WorkItem::Chore(chore.clone()),
                workspace.path(),
                Some("change-1"),
            )
            .await?;

        Ok((spawner, chore))
    }

    /// Untagged row (NULL effort_level, NULL model_override, no
    /// product default) must produce the same spawn line today's
    /// engine produces — minus the implicit `claude` model selection,
    /// plus an explicit `--model <engine-default-slug>`. No
    /// `--effort` flag, no prompt addendum. Design §Q2 / task spec
    /// regression test: "byte-equivalent to today's `claude
    /// "$(cat .claude/initial-prompt.txt)"` plus the explicit
    /// `--model <engine-default-slug>`."
    #[tokio::test]
    async fn untagged_row_spawn_matches_engine_default() {
        let workspace = TempDir::new().unwrap();
        let chore_input = CreateChoreInput::builder()
            .product_id(String::new())
            .name("Untagged chore")
            .description("plain row, no effort/model")
            .build();
        let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, None).await.unwrap();
        let input = spawner.spawn_input();

        // The worker settings file lives outside the workspace; the
        // engine points claude at it with `--settings '<abs-path>'`,
        // positioned before the positional prompt arg.
        let settings_path = crate::worker_setup::worker_settings_path(workspace.path());
        assert_eq!(
            input.initial_input,
            format!(
                "[ -n \"$BOSS_BIN_DIR\" ] && export PATH=\"$BOSS_BIN_DIR:$PATH\"; unset ANTHROPIC_API_KEY; claude --model {} --permission-mode auto --settings '{}' \"$(cat .claude/initial-prompt.txt)\"\n",
                crate::driver::ClaudeDriver.descriptor().model_menu.engine_default,
                settings_path.display(),
            ),
            "untagged row should re-prepend BOSS_BIN_DIR to PATH, then spawn with the engine default model, --permission-mode auto (Opus), --settings <worker file>, and no --effort",
        );

        // No addendum prepended — the existing implementation framing
        // must be the first thing the worker sees.
        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
        assert!(
            prompt.starts_with("You are a reusable Boss worker"),
            "untagged-row prompt must start with the original framing, got: {prompt:?}",
        );
        assert!(
            !prompt.contains("Sketch a brief plan"),
            "untagged-row prompt must not carry the medium addendum",
        );
        assert!(
            !prompt.starts_with("Begin with a written plan"),
            "untagged-row prompt must not carry the large/max addendum",
        );
    }

    /// Smoke test for the design-spec acceptance criterion: a
    /// `trivial` row dispatches with `--model sonnet --effort low`
    /// and no prompt addendum. Per #746 ("don't use haiku") the model
    /// floor is Sonnet, not Haiku, even at the trivial tier — only the
    /// effort value stays `low`. See
    /// [`crate::driver::ClaudeDriver`]'s `claude_default_model_for_level`.
    #[tokio::test]
    async fn trivial_row_spawn_uses_sonnet_at_low_effort() {
        let workspace = TempDir::new().unwrap();
        let chore_input = CreateChoreInput::builder()
            .product_id(String::new())
            .name("Apply resize-cursor fix to nav divider")
            .description("one-line CSS tweak")
            .effort_level(EffortLevel::Trivial)
            .build();
        let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, None).await.unwrap();
        let input = spawner.spawn_input();

        assert!(
            input.initial_input.contains("--model sonnet"),
            "trivial row must spawn Sonnet (#746: never Haiku), got: {:?}",
            input.initial_input,
        );
        assert!(
            !input.initial_input.contains("--model haiku"),
            "trivial row must NOT spawn Haiku (#746), got: {:?}",
            input.initial_input,
        );
        assert!(
            input.initial_input.contains("--effort low"),
            "trivial row must pass --effort low, got: {:?}",
            input.initial_input,
        );
        assert!(
            input.initial_input.contains("--dangerously-skip-permissions"),
            "trivial row (Sonnet, non-Opus) must carry --dangerously-skip-permissions, got: {:?}",
            input.initial_input,
        );
        assert!(
            !input.initial_input.contains("--permission-mode"),
            "trivial row (Sonnet, non-Opus) must NOT carry --permission-mode, got: {:?}",
            input.initial_input,
        );

        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
        assert!(
            !prompt.starts_with("Sketch") && !prompt.starts_with("Begin with"),
            "trivial row prompt must have no addendum prepended, got: {prompt:?}",
        );
    }

    /// Smoke test for the second design-spec acceptance criterion:
    /// `medium` + explicit `model_override = 'opus'` spawns `--model
    /// opus --effort high`, and the medium prompt addendum is
    /// prepended verbatim. Verifies that `model_override` changes only
    /// the model — the effort value and addendum still follow the
    /// row's `effort_level` (design §Q3).
    #[tokio::test]
    async fn medium_with_opus_override_uses_override_model_and_medium_addendum() {
        let workspace = TempDir::new().unwrap();
        let chore_input = CreateChoreInput::builder()
            .product_id(String::new())
            .name("Add created_via provenance to chore/task creates")
            .description("multi-file edit with judgement calls")
            .effort_level(EffortLevel::Medium)
            .model_override("opus")
            .build();
        let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, None).await.unwrap();
        let input = spawner.spawn_input();

        assert!(
            input.initial_input.contains("--model opus"),
            "model_override should win precedence, got: {:?}",
            input.initial_input,
        );
        assert!(
            input.initial_input.contains("--effort high"),
            "medium effort_level must still produce --effort high, got: {:?}",
            input.initial_input,
        );
        assert!(
            input.initial_input.contains("--permission-mode auto"),
            "model_override=opus must carry --permission-mode auto, got: {:?}",
            input.initial_input,
        );
        assert!(
            !input.initial_input.contains("--dangerously-skip-permissions"),
            "model_override=opus must NOT carry --dangerously-skip-permissions, got: {:?}",
            input.initial_input,
        );

        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
        assert!(
            prompt.starts_with("Sketch a brief plan before you start editing."),
            "medium addendum must be prepended verbatim, got: {prompt:?}",
        );
    }

    /// Large rows get Opus at `xhigh` plus the planning-heavy
    /// addendum. Confirms the third level boundary the design pins.
    #[tokio::test]
    async fn large_row_spawn_uses_opus_at_xhigh_with_planning_addendum() {
        let workspace = TempDir::new().unwrap();
        let chore_input = CreateChoreInput::builder()
            .product_id(String::new())
            .name("Investigate isolated test instance")
            .description("multi-subsystem investigation")
            .effort_level(EffortLevel::Large)
            .build();
        let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, None).await.unwrap();
        let input = spawner.spawn_input();

        assert!(
            input.initial_input.contains("--model opus"),
            "large row must spawn Opus, got: {:?}",
            input.initial_input,
        );
        assert!(
            input.initial_input.contains("--effort xhigh"),
            "large row must pass --effort xhigh, got: {:?}",
            input.initial_input,
        );
        assert!(
            input.initial_input.contains("--permission-mode auto"),
            "large row (Opus) must carry --permission-mode auto, got: {:?}",
            input.initial_input,
        );
        assert!(
            !input.initial_input.contains("--dangerously-skip-permissions"),
            "large row (Opus) must NOT carry --dangerously-skip-permissions, got: {:?}",
            input.initial_input,
        );

        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
        assert!(
            prompt.starts_with("Begin with a written plan."),
            "large addendum must be prepended verbatim, got: {prompt:?}",
        );
    }

    /// `products.default_model` only kicks in when both
    /// `model_override` and `effort_level` are unset (design §Q3
    /// step 3). With a product default in place but no effort tag,
    /// the dispatch should pick the product slug rather than the
    /// engine default — and still omit `--effort`.
    #[tokio::test]
    async fn product_default_model_fills_in_when_row_is_untagged() {
        let workspace = TempDir::new().unwrap();
        let chore_input = CreateChoreInput::builder()
            .product_id(String::new())
            .name("Untagged on Sonnet-defaulted product")
            .build();
        let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, Some("claude-sonnet-4-6"))
            .await
            .unwrap();
        let input = spawner.spawn_input();

        assert!(
            input.initial_input.contains("--model claude-sonnet-4-6"),
            "product default_model should fill in, got: {:?}",
            input.initial_input,
        );
        assert!(
            !input.initial_input.contains("--effort"),
            "untagged row must not pass --effort, got: {:?}",
            input.initial_input,
        );
        assert!(
            input.initial_input.contains("--dangerously-skip-permissions"),
            "Sonnet (non-Opus) must carry --dangerously-skip-permissions, got: {:?}",
            input.initial_input,
        );
        assert!(
            !input.initial_input.contains("--permission-mode"),
            "Sonnet (non-Opus) must NOT carry --permission-mode, got: {:?}",
            input.initial_input,
        );
    }

    /// The runner must return the resolved spawn config on
    /// `RunOutcome.spawn_config` so the coordinator can attach it to
    /// the `pane_spawned` dispatch event. Drives `run_execution`
    /// directly (rather than through `run_once_with_chore`, which
    /// drops the outcome) so the returned tuple is observable.
    #[tokio::test]
    async fn run_outcome_carries_resolved_spawn_config() {
        let workspace = TempDir::new().unwrap();
        let (_spawner, weak, cfg, work_db) = spawn_test_env(&workspace);

        let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
        let chore = work_db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Trivial chore")
                    .effort_level(EffortLevel::Trivial)
                    .build(),
            )
            .unwrap();

        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, work_db, flags);
        runner.set_server_state(weak);

        let mut execution = sample_execution(workspace.path());
        execution.work_item_id = chore.id.clone();

        let outcome = runner
            .run_execution(
                "worker-1",
                &execution,
                &WorkItem::Chore(chore),
                workspace.path(),
                Some("change-1"),
            )
            .await
            .unwrap();

        let spawn = outcome
            .spawn_config
            .expect("PaneSpawnRunner should always populate spawn_config");
        assert_eq!(spawn.effort_level, Some(EffortLevel::Trivial));
        assert_eq!(spawn.claude_effort, Some("low"));
        // #746: trivial floors to Sonnet, never Haiku.
        assert_eq!(spawn.model, "sonnet");
        assert_eq!(spawn.prompt_addendum, None);
    }

    /// Regression: `PaneSpawnRunner::run_execution` must return
    /// `ReviewerPaneAlive` (not `WaitingHuman`) for `PrReview` executions so
    /// the execution stays in `running` while the reviewer pane is alive.
    ///
    /// This pins the runner.rs change at the `PaneSpawnRunner` level.
    /// Reverting `run_execution` back to always returning `WaitingHuman`
    /// would cause this test to fail even if the badge-SQL test in t01.rs
    /// still passes.
    #[tokio::test]
    async fn pr_review_execution_yields_reviewer_pane_alive() {
        let workspace = TempDir::new().unwrap();
        let (_spawner, weak, cfg, work_db) = spawn_test_env(&workspace);

        let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
        let chore = create_test_chore_manual(&work_db, product.id.clone(), "Some chore being reviewed");

        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg.clone(), work_db.clone(), flags.clone());
        runner.set_server_state(weak.clone());

        // Build a PrReview execution; no pr_url on the chore is fine —
        // the runner falls back to the generic prompt, which is irrelevant
        // to the wait_state assertion.
        let mut pr_review_exec = sample_execution(workspace.path());
        pr_review_exec.kind = ExecutionKind::PrReview;
        pr_review_exec.work_item_id = chore.id.clone();

        let outcome = runner
            .run_execution(
                "review-1",
                &pr_review_exec,
                &WorkItem::Chore(chore.clone()),
                workspace.path(),
                Some("change-pr-review"),
            )
            .await
            .unwrap();

        assert_eq!(
            outcome.wait_state,
            RunWaitState::ReviewerPaneAlive,
            "PaneSpawnRunner must return ReviewerPaneAlive for PrReview executions so the \
             execution stays in running (not waiting_human) while the reviewer pane is alive"
        );

        // Verify that a non-PrReview kind still yields WaitingHuman.
        let runner2 = PaneSpawnRunner::new(cfg, work_db, flags);
        runner2.set_server_state(weak);
        let mut chore_exec = sample_execution(workspace.path());
        chore_exec.kind = ExecutionKind::ChoreImplementation;
        chore_exec.work_item_id = chore.id.clone();

        let outcome2 = runner2
            .run_execution(
                "worker-1",
                &chore_exec,
                &WorkItem::Chore(chore),
                workspace.path(),
                Some("change-chore"),
            )
            .await
            .unwrap();

        assert_eq!(
            outcome2.wait_state,
            RunWaitState::WaitingHuman,
            "PaneSpawnRunner must return WaitingHuman for non-PrReview executions"
        );
    }

    /// **No env vars related to effort or token caps appear on the
    /// worker subprocess.** Design §Q2 §"Knobs explicitly not in v1"
    /// rejects `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, `MAX_THINKING_TOKENS`,
    /// and any per-execution token cap explicitly — claude's
    /// `--effort` is the canonical control. Locks the rule in via the
    /// captured spawn env.
    #[tokio::test]
    async fn spawn_env_does_not_carry_effort_or_token_cap_env_vars() {
        let workspace = TempDir::new().unwrap();
        let chore_input = CreateChoreInput::builder()
            .product_id(String::new())
            .name("Any chore")
            .effort_level(EffortLevel::Large)
            .build();
        let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, None).await.unwrap();
        let input = spawner.spawn_input();

        // The forbidden list from design §Q2 plus the obvious
        // adjacents an over-eager future patch might add.
        for forbidden in [
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
            "MAX_THINKING_TOKENS",
            "ANTHROPIC_MAX_TOKENS",
            "BOSS_EFFORT_LEVEL",
            "CLAUDE_EFFORT",
        ] {
            assert!(
                !input.env.iter().any(|EnvVar { key, .. }| key == forbidden),
                "env must not carry {forbidden} (design §Q2 forbids token-cap env knobs)",
            );
        }
    }

    #[tokio::test]
    async fn spawn_env_carries_sanitized_path_and_engine_keys() {
        let workspace = TempDir::new().unwrap();
        let spawner = run_once(&workspace, None).await.unwrap();
        let input = spawner.spawn_input();

        let path_var = input
            .env
            .iter()
            .find(|EnvVar { key, .. }| key == "PATH")
            .expect("PATH must be set on every worker spawn");
        assert!(
            !path_var.value.contains("/Users/"),
            "PATH must not contain the user home (would expose ~/bin/bossctl), got: {}",
            path_var.value
        );
        assert!(
            path_var.value.contains("/usr/bin"),
            "PATH must include system bins, got: {}",
            path_var.value
        );

        assert!(
            input.env.iter().any(|EnvVar { key, .. }| key == "BOSS_LEASE_ID"),
            "expected BOSS_LEASE_ID to be set"
        );
        assert!(
            input.env.iter().any(|EnvVar { key, .. }| key == "BOSS_EVENTS_SOCKET"),
            "expected BOSS_EVENTS_SOCKET to be set"
        );
    }

    /// Workers must be told about the socket the engine actually bound, which
    /// lives on the config, and NOT about whatever `$BOSS_EVENTS_SOCKET`
    /// happens to say in the engine's own environment.
    ///
    /// This is the second half of the 2026-07-23 outage: `PaneSpawnRunner`
    /// re-resolved the socket from the environment, so even a fixture that had
    /// correctly isolated its own socket would have baked the production path
    /// into every worker's `settings.json` — sending their hooks to the live
    /// engine. The bazel test env pins `HOME=/tmp` (see `engine_lib_test` in
    /// `BUILD.bazel`) and does not set `BOSS_EVENTS_SOCKET` at all, so
    /// `engine_events_socket_path()` falls back to
    /// `/tmp/Library/Application Support/Boss/events.sock` — a path that
    /// cannot collide with the test's `TempDir`-rooted config path. That is
    /// what makes `assert_ne!` below meaningful: a regression that re-resolved
    /// the socket from the environment would still fail loudly, just because
    /// the two paths live in unrelated directories rather than because the
    /// env var was deliberately mismatched.
    #[tokio::test]
    async fn spawn_env_uses_the_bound_socket_from_config_not_the_environment() {
        let workspace = TempDir::new().unwrap();
        let bound = workspace.path().join("boss-test-fixture.events.sock");

        let (spawner, weak, _cfg, work_db) = spawn_test_env(&workspace);
        let cfg = Arc::new(crate::config::RuntimeConfig::from_parts(
            crate::config::WorkConfig::builder()
                .cwd(workspace.path().to_path_buf())
                .db_path(workspace.path().join("state.db"))
                .events_socket_path(bound.clone())
                .build(),
            None,
        ));
        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, work_db, flags);
        runner.set_server_state(weak);
        runner
            .run_execution(
                "worker-1",
                &sample_execution(workspace.path()),
                &sample_chore(),
                workspace.path(),
                Some("change-1"),
            )
            .await
            .unwrap();

        let input = spawner.spawn_input();
        let socket = input
            .env
            .iter()
            .find(|EnvVar { key, .. }| key == "BOSS_EVENTS_SOCKET")
            .expect("BOSS_EVENTS_SOCKET must be set on every worker spawn");
        assert_eq!(
            socket.value,
            bound.display().to_string(),
            "workers must be pointed at the socket this engine bound",
        );
        assert_ne!(
            socket.value,
            engine_events_socket_path().display().to_string(),
            "the runner must not re-resolve the socket from $BOSS_EVENTS_SOCKET",
        );
    }

    #[test]
    fn bound_events_socket_path_prefers_the_config_over_the_environment() {
        let work = crate::config::WorkConfig::builder()
            .cwd(PathBuf::from("/tmp"))
            .db_path(PathBuf::from("/tmp/state.db"))
            .events_socket_path(PathBuf::from("/tmp/bound.events.sock"))
            .build();
        let cfg = crate::config::RuntimeConfig::from_parts(work, None);
        assert_eq!(bound_events_socket_path(&cfg), PathBuf::from("/tmp/bound.events.sock"));
    }

    /// Only the "this engine bound no events socket" shape (in-process
    /// `serve(..., None, ...)`) falls back to the environment resolver.
    #[test]
    fn bound_events_socket_path_falls_back_when_nothing_was_bound() {
        let work = crate::config::WorkConfig::builder()
            .cwd(PathBuf::from("/tmp"))
            .db_path(PathBuf::from("/tmp/state.db"))
            .build();
        let cfg = crate::config::RuntimeConfig::from_parts(work, None);
        assert_eq!(bound_events_socket_path(&cfg), engine_events_socket_path());
    }

    /// The engine is now the source of truth for which slot a
    /// worker lands in. The runner derives the slot from the
    /// `worker-{N}` id the coordinator passes in and forwards it on
    /// `SpawnWorkerPaneInput.slot_id`. The app honors that slot
    /// rather than running its own allocator. This test pins down
    /// that wiring so a regression that drops the slot from the
    /// request (or computes it wrong) doesn't silently re-introduce
    /// the dual-allocator bug.
    #[tokio::test]
    async fn spawn_request_includes_engine_claimed_slot() {
        let workspace = TempDir::new().unwrap();
        let spawner: Arc<CapturingSpawner> = Arc::new(CapturingSpawner::new());
        let weak: Weak<dyn crate::spawn_flow::WorkerSpawner> =
            Arc::downgrade(&spawner) as Weak<dyn crate::spawn_flow::WorkerSpawner>;
        let cfg = Arc::new(crate::config::RuntimeConfig::from_parts(
            crate::config::WorkConfig::builder()
                .cwd(workspace.path().to_path_buf())
                .db_path(workspace.path().join("state.db"))
                .worker_pool_size(8)
                .automation_pool_size(3)
                .build(),
            None,
        ));
        let work_db = Arc::new(WorkDb::open(workspace.path().join("state.db")).unwrap());
        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, work_db, flags);
        runner.set_server_state(weak);

        // Engine claimed slot 6 (i.e. handed `worker-6` to the
        // runner). The spawn request must carry slot 6 — not 1, not
        // some random pick, not the lowest free.
        runner
            .run_execution(
                "worker-6",
                &sample_execution(workspace.path()),
                &sample_chore(),
                workspace.path(),
                Some("change-1"),
            )
            .await
            .unwrap();

        let input = spawner.spawn_input();
        assert_eq!(
            input.slot_id, 6,
            "engine-claimed slot must reach the app verbatim, got {}",
            input.slot_id,
        );
    }

    #[tokio::test]
    async fn run_execution_stamps_work_item_binding_on_live_state() {
        // The bossctl coordinator joins `agents list` output back to a
        // chore via these fields — without them, asking "stop the
        // worker on chore X" forces the user to disambiguate slot
        // numbers manually.
        let workspace = TempDir::new().unwrap();
        let spawner = run_once(&workspace, None).await.unwrap();

        let state = spawner
            .live_states
            .get(1)
            .expect("expected live state for slot 1 after run_execution");
        assert_eq!(
            state.work_item_id.as_deref(),
            Some("task-1"),
            "work_item_id should match the chore the runner was driven against"
        );
        assert_eq!(
            state.work_item_name.as_deref(),
            Some("Improve top header (agent card) styling"),
            "work_item_name should be the chore's display name"
        );
        assert_eq!(
            state.execution_id.as_deref(),
            Some("exec-test-1"),
            "execution_id should match the WorkExecution row id"
        );
    }

    /// Regression — the mid-spawn cancel reconciliation. When the
    /// execution row is cancelled while the `SpawnWorkerPane` round-trip
    /// is in flight, `run_execution` must, on return, (i) reap the
    /// just-spawned pane (the pid is now known, so the reap is no longer
    /// a no-op) and (ii) report `CancelledDuringSpawn` so the coordinator
    /// releases the cube lease the cancel path deliberately left held.
    /// Without this the worker survives unreaped in a workspace the
    /// engine believes is free, which is what produced the duplicate
    /// dispatch into a shared workspace.
    #[tokio::test]
    async fn run_execution_reaps_and_signals_when_cancelled_mid_spawn() {
        let workspace = TempDir::new().unwrap();
        let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);

        let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
        let chore = create_test_chore(&work_db, product.id.clone(), "Sort struct definitions");
        let ready = create_ready_chore_execution(&work_db, chore.id.clone());
        // Start the run (ready → running, lease attached) — this is the
        // exact state the row is in when the spawn round-trip is in
        // flight — then cancel it, mirroring a kanban drag-to-Backlog
        // landing inside the spawn window.
        let (execution, _run) = work_db
            .start_execution_run(
                &ready.id,
                "worker-1",
                "foo",
                "lease-1",
                "foo-agent-001",
                workspace.path().to_str().unwrap(),
            )
            .unwrap();
        assert!(work_db.cancel_running_execution(&execution.id).unwrap());

        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, work_db.clone(), flags);
        runner.set_server_state(weak);

        let chore_item = work_db.get_work_item(&chore.id).unwrap();
        let outcome = runner
            .run_execution("worker-1", &execution, &chore_item, workspace.path(), Some("change-1"))
            .await
            .unwrap();

        assert_eq!(
            outcome.wait_state,
            RunWaitState::CancelledDuringSpawn,
            "a cancel that races the spawn window must yield CancelledDuringSpawn",
        );
        assert!(
            outcome.slot_id.is_none(),
            "the pane was reaped, so the coordinator must not keep the pool slot claimed",
        );
        assert_eq!(
            spawner.reaped_run_ids().as_slice(),
            [execution.id.as_str()],
            "the runner must reap the just-spawned pane for the cancelled execution",
        );
    }

    /// Any task whose `project_id` is set must surface the parent
    /// project's name/description/goal in its spawn prompt — the
    /// task row itself is intentionally a thin handle (the design
    /// task starts with `description = ''`; ordinary `project_task`
    /// rows often only carry an implementation brief that omits the
    /// project's *why*). Without the spawn-time walk the worker
    /// boots with no project context and has to ask, which defeats
    /// the point of having a project record at all.
    #[tokio::test]
    async fn spawn_prompt_for_project_scoped_task_includes_parent_project_context() {
        let workspace = TempDir::new().unwrap();
        let (_spawner, weak, cfg, work_db) = spawn_test_env(&workspace);

        // Stand up a real product → project → task chain so the
        // runner's `get_project` lookup hits a row with the
        // description/goal we want to assert on. `--no-autostart` on
        // the project keeps the auto-spawned design task parked so
        // it doesn't compete with our explicit run_execution call.
        let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
        let project = work_db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Engine dispatch instrumentation".to_owned(),
                description: Some("Instrument the auto-dispatcher so every spawn decision is traceable.".to_owned()),
                goal: Some("Operators can answer 'why did this task spawn now' from logs alone.".to_owned()),
                autostart: false,
                no_design_task: false,
            })
            .unwrap();
        let task = work_db
            .create_task(
                CreateTaskInput::builder()
                    .product_id(product.id.clone())
                    .project_id(project.id.clone())
                    .name("Tag dispatch logs with execution kind")
                    .build(),
            )
            .unwrap();

        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, work_db, flags);
        runner.set_server_state(weak);

        let mut execution = sample_execution(workspace.path());
        execution.kind = ExecutionKind::TaskImplementation;
        execution.work_item_id = task.id.clone();

        runner
            .run_execution(
                "worker-1",
                &execution,
                &WorkItem::Task(task),
                workspace.path(),
                Some("change-1"),
            )
            .await
            .unwrap();

        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
        assert!(
            prompt.contains("parent project: `Engine dispatch instrumentation`"),
            "prompt missing parent project name line:\n{prompt}",
        );
        assert!(
            prompt.contains("Instrument the auto-dispatcher"),
            "prompt missing parent project description:\n{prompt}",
        );
        assert!(
            prompt.contains("'why did this task spawn now'"),
            "prompt missing parent project goal:\n{prompt}",
        );
    }

    /// `boss project create` auto-files a `kind = 'design'` task as
    /// ordinal-0 of every new project. When that task dispatches it
    /// becomes a `project_design` execution. The worker prompt must
    /// state up front that the deliverable is a design document — not
    /// an implementation. Without this guard the worker has only the
    /// project's name/goal to go on and frequently starts coding;
    /// observed against worker O'Brien (exec_18aebf0caa1187e8_b).
    #[tokio::test]
    async fn spawn_prompt_for_auto_design_task_states_design_only_directive() {
        let workspace = TempDir::new().unwrap();
        let (_spawner, weak, cfg, work_db) = spawn_test_env(&workspace);

        let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
        let project = work_db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Worker live-status dashboard".to_owned(),
                description: Some(
                    "Surface every running worker's live state on the kanban without polling.".to_owned(),
                ),
                goal: Some("Operators can see what every active worker is doing without opening panes.".to_owned()),
                autostart: false,
                no_design_task: false,
            })
            .unwrap();

        // Find the design task `create_project` auto-filed for this
        // project. It sorts ordinal-0 with `kind = 'design'`.
        let design_task = work_db
            .list_tasks(&product.id, Some(&project.id), None, false)
            .unwrap()
            .into_iter()
            .find(|t| t.kind == TaskKind::Design)
            .expect("create_project should auto-file a kind='design' task");

        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, work_db, flags);
        runner.set_server_state(weak);

        let mut execution = sample_execution(workspace.path());
        execution.kind = ExecutionKind::ProjectDesign;
        execution.work_item_id = design_task.id.clone();

        runner
            .run_execution(
                "worker-1",
                &execution,
                &WorkItem::Task(design_task),
                workspace.path(),
                Some("change-1"),
            )
            .await
            .unwrap();

        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();

        // The deliverable directive must be unmistakable.
        assert!(
            prompt.contains("the deliverable is a **design document**"),
            "design prompt must state the deliverable is a design doc:\n{prompt}",
        );
        assert!(
            prompt.contains("only the design doc"),
            "design prompt must scope the PR to the design doc only:\n{prompt}",
        );
        assert!(
            prompt.contains("Do not edit code"),
            "design prompt must forbid code edits:\n{prompt}",
        );

        // Canonical path uses the project slug since no design_doc_path
        // pointer is configured on this brand-new project.
        assert!(
            prompt.contains(&format!("docs/designs/{}.md", project.slug)),
            "design prompt must include the canonical doc path derived from the project slug `{}`:\n{prompt}",
            project.slug,
        );

        // Required section shape — all five anchors must be named so
        // the worker doesn't invent its own headings.
        for heading in [
            "**Goals**",
            "**Non-goals**",
            "**Alternatives considered**",
            "**Chosen approach**",
            "**Risks / open questions**",
        ] {
            assert!(
                prompt.contains(heading),
                "design prompt missing required section `{heading}`:\n{prompt}",
            );
        }

        // The parent project's goal must come through verbatim — that
        // is the whole point of pulling project context at spawn time.
        assert!(
            prompt.contains("Operators can see what every active worker is doing without opening panes."),
            "design prompt must surface the parent project's goal verbatim:\n{prompt}",
        );

        // The PR-URL acceptance criterion still applies to design
        // runs — they produce a PR, it just contains the doc only.
        assert!(
            prompt.contains("the deliverable is a PR URL"),
            "design prompt must keep the PR-URL acceptance criterion:\n{prompt}",
        );

        // Deliverable 2 — the breakdown sizing contract: the design worker
        // must be told to pre-split its breakdown to one-PR-per-session
        // granularity, so breakdowns arrive pre-decomposed and the planner
        // gate rarely fires.
        assert!(
            prompt.contains("size each entry to one reviewable PR by one worker in one session"),
            "design prompt must carry the one-PR-per-session sizing contract:\n{prompt}",
        );
        assert!(
            prompt.contains("single-subsystem and single-PR"),
            "design prompt must require single-subsystem, single-PR entries:\n{prompt}",
        );
        assert!(
            prompt.contains("sweeps and validation campaigns"),
            "design prompt must split sweeps/validation campaigns into separate dependent entries:\n{prompt}",
        );
        assert!(
            prompt.contains("unknown-format discovery"),
            "design prompt must route unknown-format discovery to its own investigation entry:\n{prompt}",
        );
    }

    /// When the project already has a `design_doc_path` pointer set
    /// (the resumed-design-pass case — a doc was filed, then the
    /// engine respawned the design task to revise it), the canonical
    /// path in the worker prompt must come from that pointer verbatim
    /// instead of the slug-derived default. Otherwise the worker
    /// could write to two different files across runs.
    #[tokio::test]
    async fn spawn_prompt_for_design_task_uses_explicit_design_doc_path() {
        use crate::work::SetProjectDesignDocInput;

        let workspace = TempDir::new().unwrap();
        let (_spawner, weak, cfg, work_db) = spawn_test_env(&workspace);

        let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
        let project = work_db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Merge poller cadence tuning".to_owned(),
                description: Some("Pick the right merge-poller cadence.".to_owned()),
                goal: Some("Reduce GitHub API spend without lagging merges.".to_owned()),
                autostart: false,
                no_design_task: false,
            })
            .unwrap();

        work_db
            .set_project_design_doc(SetProjectDesignDocInput {
                project_id: project.id.clone(),
                design_doc_repo_remote_url: None,
                design_doc_branch: None,
                design_doc_path: Some("tools/boss/docs/designs/merge-poller-cadence.md".into()),
                unset: false,
            })
            .unwrap();

        let design_task = work_db
            .list_tasks(&product.id, Some(&project.id), None, false)
            .unwrap()
            .into_iter()
            .find(|t| t.kind == TaskKind::Design)
            .expect("create_project should auto-file a kind='design' task");

        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, work_db, flags);
        runner.set_server_state(weak);

        let mut execution = sample_execution(workspace.path());
        execution.kind = ExecutionKind::ProjectDesign;
        execution.work_item_id = design_task.id.clone();

        runner
            .run_execution(
                "worker-1",
                &execution,
                &WorkItem::Task(design_task),
                workspace.path(),
                Some("change-1"),
            )
            .await
            .unwrap();

        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();

        assert!(
            prompt.contains("`tools/boss/docs/designs/merge-poller-cadence.md`"),
            "design prompt must use the project's explicit design_doc_path pointer:\n{prompt}",
        );
        // And it should NOT also fall through to the slug-derived
        // suggestion line — that would be ambiguous.
        assert!(
            !prompt.contains("`design_doc_path` pointer is not yet set"),
            "design prompt should not emit the pointer-missing fallback when the pointer is set:\n{prompt}",
        );
    }

    #[tokio::test]
    async fn settings_json_uses_absolute_boss_event_path() {
        // Inject a fake boss-event at a known absolute temp path so this
        // test is deterministic on every agent — no host PATH lookup, no
        // BOSS_EVENT_BIN env var, no runfiles, no bazel-bin dependency.
        let fake_bin_dir = TempDir::new().unwrap();
        let fake_boss_event = fake_bin_dir.path().join("boss-event");
        std::fs::write(&fake_boss_event, b"").unwrap();

        let workspace = TempDir::new().unwrap();
        let _spawner = run_once(&workspace, Some(&fake_boss_event)).await.unwrap();

        // The settings file lives outside the workspace tree, keyed by
        // workspace name (see worker_setup); it must NOT be written into
        // the workspace `.claude/`.
        let settings_path = crate::worker_setup::worker_settings_path(workspace.path());
        assert!(
            !workspace.path().join(".claude").join("settings.json").exists(),
            "engine must not write .claude/settings.json into the workspace",
        );
        let settings = std::fs::read_to_string(&settings_path).unwrap();

        // Hooks must invoke an absolute path; the bare name
        // `boss-event` is what produced the production
        // `command not found` failures because the worker's sanitized
        // PATH doesn't include the bazel-out directory.
        let expected_path = fake_boss_event.to_str().unwrap();
        assert!(
            settings.contains(expected_path),
            "expected absolute boss-event path {} in settings file, got: {}",
            expected_path,
            settings,
        );
        assert!(
            !settings.contains("'boss-event'") && !settings.contains("\"boss-event\""),
            "settings file must not invoke `boss-event` as a bare name, got: {}",
            settings,
        );
    }

    /// `BOSS_EVENT_BIN` short-circuits everything else.
    #[test]
    fn resolve_boss_event_prefers_env_override() {
        let dir = TempDir::new().unwrap();
        let engine = dir.path().join("engine");
        std::fs::write(&engine, b"").unwrap();
        let override_path = PathBuf::from("/opt/whatever/boss-event");
        let resolved = resolve_boss_event_binary(&engine, None, Some(&override_path), None, None);
        assert_eq!(resolved, Some(override_path));
    }

    /// `BOSS_BIN_DIR` is the installed-mode path; it wins over the
    /// dev-mode runfiles and workspace-bazel-bin candidates so a
    /// deployed Boss.app never silently falls through to a workspace clone.
    #[test]
    fn resolve_boss_event_prefers_boss_bin_dir_over_runfiles() {
        let dir = TempDir::new().unwrap();
        let engine = dir.path().join("engine");
        std::fs::write(&engine, b"").unwrap();

        // Synthesize the bundle bin/ directory (installed mode).
        let bundle_bin = dir.path().join("bundle-bin");
        std::fs::create_dir_all(&bundle_bin).unwrap();
        let bundle_shim = bundle_bin.join("boss-event");
        std::fs::write(&bundle_shim, b"").unwrap();

        // Also synthesize runfiles (dev mode) — must NOT be picked.
        let runfiles = dir.path().join("engine.runfiles/_main/tools/boss/event-shim");
        std::fs::create_dir_all(&runfiles).unwrap();
        std::fs::write(runfiles.join("boss-event"), b"").unwrap();

        let resolved = resolve_boss_event_binary(&engine, None, None, Some(&bundle_bin), None);
        assert_eq!(resolved, Some(bundle_shim));
    }

    /// When the engine binary has runfiles at the bazel-conventional
    /// path, the resolver must pick that up — this is the production
    /// path under `bazel run //tools/boss/engine:engine` once the
    /// engine `rust_binary` has the `data` dep on
    /// `//tools/boss/event-shim:boss-event`. The original #174 fix
    /// only covered the BOSS_EVENT_BIN branch; this test covers the
    /// branch that actually fires in real launches.
    #[test]
    fn resolve_boss_event_uses_runfiles_when_present() {
        let dir = TempDir::new().unwrap();
        let engine = dir.path().join("engine");
        std::fs::write(&engine, b"").unwrap();

        // Synthesize the bazel runfiles tree the data dep produces.
        let runfiles = dir.path().join("engine.runfiles/_main/tools/boss/event-shim");
        std::fs::create_dir_all(&runfiles).unwrap();
        let shim = runfiles.join("boss-event");
        std::fs::write(&shim, b"").unwrap();

        let resolved = resolve_boss_event_binary(&engine, None, None, None, None);
        assert_eq!(resolved, Some(shim));
    }

    /// Workspace `bazel-bin` symlink path is the secondary candidate
    /// — covers `bazel build` + non-`bazel run` scenarios where the
    /// engine binary is invoked directly but `BUILD_WORKSPACE_DIRECTORY`
    /// is set.
    #[test]
    fn resolve_boss_event_falls_back_to_workspace_bazel_bin() {
        let dir = TempDir::new().unwrap();
        let engine = dir.path().join("engine");
        std::fs::write(&engine, b"").unwrap();

        let workspace = dir.path().join("workspace");
        let bazel_bin = workspace.join("bazel-bin/tools/boss/event-shim");
        std::fs::create_dir_all(&bazel_bin).unwrap();
        let shim = bazel_bin.join("boss-event");
        std::fs::write(&shim, b"").unwrap();

        let resolved = resolve_boss_event_binary(&engine, Some(&workspace), None, None, None);
        assert_eq!(resolved, Some(shim));
    }

    /// When nothing resolves the function returns `None` — the caller
    /// (`boss_event_binary`) turns this into a hard panic rather than
    /// silently baking a bare `boss-event` into hook commands (which
    /// causes `command not found` in the worker's sanitized PATH).
    #[test]
    fn resolve_boss_event_returns_none_when_nothing_resolves() {
        let dir = TempDir::new().unwrap();
        let engine = dir.path().join("engine");
        std::fs::write(&engine, b"").unwrap();
        let resolved = resolve_boss_event_binary(&engine, None, None, None, None);
        assert_eq!(resolved, None);
    }

    /// The stable bin dir (installed by the engine at startup) is
    /// preferred over bazel runfiles and bazel-bin so a `bazel clean`
    /// doesn't break hook paths already baked into worker settings.json.
    #[test]
    fn resolve_boss_event_prefers_stable_bin_dir_over_runfiles() {
        let dir = TempDir::new().unwrap();
        let engine = dir.path().join("engine");
        std::fs::write(&engine, b"").unwrap();

        // Synthesize the stable bin dir (engine startup installs it here).
        let stable_bin = dir.path().join("stable-bin");
        std::fs::create_dir_all(&stable_bin).unwrap();
        let stable_shim = stable_bin.join("boss-event");
        std::fs::write(&stable_shim, b"stable").unwrap();

        // Also synthesize runfiles — must NOT be picked when stable exists.
        let runfiles = dir.path().join("engine.runfiles/_main/tools/boss/event-shim");
        std::fs::create_dir_all(&runfiles).unwrap();
        std::fs::write(runfiles.join("boss-event"), b"runfiles").unwrap();

        let resolved = resolve_boss_event_binary(&engine, None, None, None, Some(&stable_bin));
        assert_eq!(resolved, Some(stable_shim));
    }

    /// `install_boss_event_to_stable_bin` copies the shim and marks it
    /// executable so workers can invoke it directly.
    #[test]
    fn install_boss_event_to_stable_bin_copies_and_makes_executable() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("boss-event-source");
        std::fs::write(&source, b"#!/bin/sh\necho ok\n").unwrap();

        let stable_bin = dir.path().join("stable/bin");
        let result = install_boss_event_to_stable_bin(&source, &stable_bin);
        assert!(result.is_ok(), "install should succeed: {result:?}");
        let stable = result.unwrap();
        assert_eq!(stable, stable_bin.join("boss-event"));
        assert!(stable.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&stable).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "boss-event must be executable after install");
        }
    }

    /// Installing when src == dst is a no-op (doesn't fail or corrupt the file).
    #[test]
    fn install_boss_event_to_stable_bin_no_op_when_already_stable() {
        let dir = TempDir::new().unwrap();
        let stable_bin = dir.path().join("bin");
        std::fs::create_dir_all(&stable_bin).unwrap();
        let stable = stable_bin.join("boss-event");
        std::fs::write(&stable, b"#!/bin/sh\n").unwrap();

        let result = install_boss_event_to_stable_bin(&stable, &stable_bin);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), stable);
    }
}
