use super::*;

impl WorkDb {
    pub fn create_run(&self, input: CreateRunInput) -> Result<WorkRun> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_execution_exists(&tx, &input.execution_id)?;

        let id = next_id("run");
        let now = now_string();
        let status = input.status.unwrap_or_else(|| "starting".to_owned());
        let error_text = normalize_optional_text(input.error_text);
        let result_summary = normalize_optional_text(input.result_summary);
        let transcript_path = normalize_optional_text(input.transcript_path);
        let artifacts_path = normalize_optional_text(input.artifacts_path);
        let started_at = normalize_optional_text(input.started_at);
        let finished_at = normalize_optional_text(input.finished_at);

        tx.execute(
            "INSERT INTO work_runs (
                id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                artifacts_path, created_at, started_at, finished_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                id,
                input.execution_id,
                input.agent_id,
                status,
                error_text,
                result_summary,
                transcript_path,
                artifacts_path,
                now,
                started_at,
                finished_at,
            ],
        )?;

        let run = query_run(&tx, &id)?.with_context(|| format!("missing run after insert: {id}"))?;
        tx.commit()?;
        Ok(run)
    }

    pub fn list_runs(&self, execution_id: &str) -> Result<Vec<WorkRun>> {
        let conn = self.connect()?;
        ensure_execution_exists(&conn, execution_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                    artifacts_path, created_at, started_at, finished_at
             FROM work_runs
             WHERE execution_id = ?1
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([execution_id], map_run)?;
        collect_rows(rows)
    }

    pub fn get_run(&self, id: &str) -> Result<WorkRun> {
        let conn = self.connect()?;
        query_run(&conn, id).require("run", id)
    }

    /// Persist the verbatim `transcript_path` we learned from a hook
    /// event payload.
    ///
    /// **Namespace warning.** The dispatcher's `_boss_run_id` carries
    /// the `work_executions.id` (`exec_*`), not a `work_runs.id`
    /// (`run_*`) — `runner.rs::run_execution` plumbs `execution.id`
    /// through to `BOSS_RUN_ID` for the worker shim, and the engine's
    /// `WorkerRegistry` keys its slot map on the same identifier. The
    /// pre-2026-05-12 version of this function joined `WHERE id = ?1`
    /// on `work_runs.id`, which never matched — every hook quietly
    /// returned "0 rows updated" and the `transcript_path` column
    /// stayed NULL forever. PR #366 and PR #372 both shipped trying
    /// to fix the symptom without spotting the cross-namespace join.
    /// This implementation resolves a target `work_runs` row for the
    /// execution and writes against its `id`, so the caller can keep
    /// handing us an execution id without worrying about the
    /// run/execution split.
    ///
    /// **Target selection.** An execution can have multiple `work_runs`
    /// rows: pre-start failure bookkeeping (`record_pre_start_failure` /
    /// `fail_execution_start`) inserts `status='failed'` rows that never
    /// host a worker, then a later `start_execution_run` inserts the
    /// real agent-session row. Pure `(created_at DESC, id DESC)` always
    /// picks the most recent insert, which is correct while that row is
    /// the live agent session — but after a re-queue / re-dispatch the
    /// newest row can be a pre-start failure sibling that will never
    /// receive hooks, while the real session row (still the one the
    /// worker's BOSS_RUN_ID belongs to) sits one row back with
    /// `transcript_path` still NULL. Hooks arriving for that execution
    /// then either stamp the path onto the bookkeeping row or, once
    /// that row is terminal-failed and a newer session starts, leave
    /// the prior session row unfilled forever.
    ///
    /// Order of preference (see [`resolve_run_id_for_execution_hooks`]):
    /// 1. unfinished rows (`finished_at IS NULL`) — the live session;
    /// 2. non-`failed` rows — pane-spawn parks the dispatch/spawn run as
    ///    `completed` within ~1s while the worker keeps running;
    /// 3. rows with a non-NULL `transcript_path` — so a failed agent session
    ///    beats a newer path-less permanent pre-start bookkeeping sibling;
    /// 4. newest by `(created_at DESC, id DESC)` as a last resort.
    ///
    /// Idempotent for the first writer per run (the
    /// `WHERE transcript_path IS NULL` clause keeps every subsequent
    /// hook event from rewriting the same value, and also keeps a
    /// later SessionStart/resume from clobbering the path the
    /// summarizer's tail watcher has already opened).
    ///
    /// Returns:
    /// - `Updated` — the row's `transcript_path` was just written.
    /// - `AlreadySet` — the selected run for this execution already
    ///   has a non-NULL `transcript_path`; legitimate steady-state
    ///   no-op.
    /// - `RowMissing` — no `work_runs` row exists yet for this
    ///   execution. Split out from `AlreadySet` because that
    ///   conflation is precisely what hid the wrong-namespace bug:
    ///   on the wire, "0 rows updated" looked identical between
    ///   "run already populated" and "the join never matched in the
    ///   first place".
    pub fn set_run_transcript_path_if_unset(
        &self,
        execution_id: &str,
        transcript_path: &str,
    ) -> Result<SetRunTranscriptPathOutcome> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(SetRunTranscriptPathOutcome::RowMissing);
        };
        let updated = conn.execute(
            "UPDATE work_runs
             SET transcript_path = ?2
             WHERE id = ?1 AND transcript_path IS NULL",
            params![run_id, transcript_path],
        )?;
        if updated > 0 {
            Ok(SetRunTranscriptPathOutcome::Updated)
        } else {
            Ok(SetRunTranscriptPathOutcome::AlreadySet)
        }
    }

    /// Persist the cumulative raw-usage snapshot for the latest run of an
    /// execution.
    ///
    /// The dispatcher derives this snapshot by incrementally tailing the
    /// transcript on every hook. Values are assignments, not SQL increments:
    /// retrying a hook or rebuilding the in-memory tail after an engine
    /// restart is therefore idempotent. Incremental snapshots use `COALESCE`
    /// so a transcript that has not yet emitted (for example) a turn-duration
    /// record cannot erase a value captured by an earlier hook. A snapshot
    /// rebuilt after transcript replacement assigns every field verbatim,
    /// including NULL, so values removed from the new generation cannot
    /// survive. Cache-write TTL splits additionally carry an observed/known
    /// signal because NULL also means "usage observed but the provider omitted
    /// its TTL breakdown".
    pub(crate) fn set_run_cost_snapshot(
        &self,
        execution_id: &str,
        snapshot: crate::run_cost::RunCostSnapshot,
    ) -> Result<bool> {
        let conn = self.connect()?;
        // Same target selection as `set_run_transcript_path_if_unset`: cost
        // and path ride the same hook seam, so they must land on the same
        // run row (the agent-session run, not a pre-start failure sibling).
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(false);
        };
        let updated = conn.execute(
            "UPDATE work_runs
             SET model = CASE WHEN ?12 = 1 THEN ?2 ELSE COALESCE(?2, model) END,
                 output_tokens = CASE WHEN ?12 = 1 THEN ?3 ELSE COALESCE(?3, output_tokens) END,
                 input_tokens = CASE WHEN ?12 = 1 THEN ?4 ELSE COALESCE(?4, input_tokens) END,
                 cache_creation_tokens = CASE
                     WHEN ?12 = 1 THEN ?5
                     ELSE COALESCE(?5, cache_creation_tokens)
                 END,
                 cache_read_tokens = CASE
                     WHEN ?12 = 1 THEN ?6
                     ELSE COALESCE(?6, cache_read_tokens)
                 END,
                 cache_creation_5m_tokens = CASE
                     WHEN ?12 = 1 THEN ?7
                     WHEN ?9 IS NULL THEN cache_creation_5m_tokens
                     WHEN ?9 = 0 THEN NULL
                     ELSE ?7
                 END,
                 cache_creation_1h_tokens = CASE
                     WHEN ?12 = 1 THEN ?8
                     WHEN ?9 IS NULL THEN cache_creation_1h_tokens
                     WHEN ?9 = 0 THEN NULL
                     ELSE ?8
                 END,
                 rounds = CASE WHEN ?12 = 1 THEN ?10 ELSE COALESCE(?10, rounds) END,
                 agent_active_ms = CASE
                     WHEN ?12 = 1 THEN ?11
                     ELSE COALESCE(?11, agent_active_ms)
                 END
             WHERE id = ?1",
            params![
                run_id,
                snapshot.model,
                snapshot.output_tokens,
                snapshot.input_tokens,
                snapshot.cache_creation_tokens,
                snapshot.cache_read_tokens,
                snapshot.cache_creation_5m_tokens,
                snapshot.cache_creation_1h_tokens,
                snapshot.cache_creation_ttl_split_known,
                snapshot.rounds,
                snapshot.agent_active_ms,
                snapshot.full_replacement,
            ],
        )?;
        Ok(updated > 0)
    }

    /// Read-side companion to [`set_run_transcript_path_if_unset`].
    ///
    /// **Namespace warning — same trap as the write side.** Every
    /// caller in the engine that previously did
    /// `work_db.get_run(run_id).transcript_path` was actually handing
    /// in an `exec_*` (`work_executions.id`) and joining it against
    /// `work_runs.id`, so the lookup never matched and the path
    /// stayed NULL on the wire. The write-side path was fixed in PR
    /// #384; the read side kept the same shape, which is why
    /// `bossctl live-status debug --json` reported `transcript_path:
    /// null` for live slots even when the underlying `work_runs` row
    /// had the column populated. This helper closes that gap by
    /// keying on `execution_id` and resolving the target `work_runs`
    /// row the same way the write side does (see
    /// [`Self::set_run_transcript_path_if_unset`]).
    ///
    /// Returns `Ok(None)` when either the execution has no
    /// `work_runs` row yet, or the selected row's `transcript_path`
    /// column is still NULL — both are legitimate steady states
    /// while a worker is still booting. Returns `Err` only on a real
    /// SQL failure; callers should log-and-default rather than abort.
    pub fn transcript_path_for_execution(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(None);
        };
        let path: Option<String> = conn
            .query_row(
                "SELECT transcript_path FROM work_runs WHERE id = ?1",
                params![run_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(path)
    }

    /// Atomically claim the one current provider session id for an execution.
    ///
    /// Returns `true` when the selected agent-session run already held
    /// `session_id` (resume) and `false` when this call installed a new
    /// identity (startup). The value lives in engine-owned SQLite, not the
    /// agent-writable provider home, and is bounded to one string on the
    /// agent-session run row (same target selection as hook-driven path/cost
    /// writes — see [`resolve_run_id_for_execution_hooks`]).
    pub fn claim_run_progress_session_identity(&self, execution_id: &str, session_id: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&tx, execution_id)? else {
            bail!("no work_runs row for execution {execution_id}");
        };
        let prior: Option<String> = tx
            .query_row(
                "SELECT progress_session_id FROM work_runs WHERE id = ?1",
                params![run_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let resumed = prior.as_deref() == Some(session_id);
        if !resumed {
            tx.execute(
                "UPDATE work_runs SET progress_session_id = ?2 WHERE id = ?1",
                params![run_id, session_id],
            )?;
        }
        tx.commit()?;
        Ok(resumed)
    }

    /// Persist the run's file-progress ingress resume point.
    ///
    /// Targets the same agent-session row as
    /// [`Self::claim_run_progress_session_identity`], so the resume point and
    /// the session identity can never end up describing different runs.
    /// Errors when the execution has no run row: a checkpoint nobody can read
    /// back is indistinguishable from no ingress at all, and the caller has to
    /// know which of those it is.
    pub fn set_run_progress_ingress_checkpoint(&self, execution_id: &str, checkpoint_json: &str) -> Result<()> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            bail!("no work_runs row for execution {execution_id}");
        };
        conn.execute(
            "UPDATE work_runs SET progress_ingress_checkpoint = ?2 WHERE id = ?1",
            params![run_id, checkpoint_json],
        )?;
        Ok(())
    }

    /// Resolve the `work_runs` row an execution's agent-session state lives
    /// on, for a caller that is about to write to it repeatedly.
    ///
    /// Same row [`Self::set_run_progress_ingress_checkpoint`] picks, exposed
    /// separately so the file-progress ingress — which writes after every
    /// dispatched event — resolves it once at attach time instead of on each
    /// write. The derivation is an ordered scan over `work_runs` under the
    /// shared connection lock, and its answer is fixed for the life of a run.
    pub fn resolve_run_row_for_execution(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        resolve_run_id_for_execution_hooks(&conn, execution_id)
    }

    /// [`Self::set_run_progress_ingress_checkpoint`] against an already
    /// resolved [`Self::resolve_run_row_for_execution`] row: one keyed UPDATE,
    /// one acquisition of the shared connection.
    ///
    /// Errors when the row has gone (an execution whose run rows were pruned
    /// mid-flight), for the same reason the run-id keyed form does: a resume
    /// point that silently went nowhere reads back exactly like a run that
    /// never had an ingress.
    pub fn set_run_progress_ingress_checkpoint_by_row(&self, run_row_id: &str, checkpoint_json: &str) -> Result<()> {
        let conn = self.connect()?;
        let updated = conn.execute(
            "UPDATE work_runs SET progress_ingress_checkpoint = ?2 WHERE id = ?1",
            params![run_row_id, checkpoint_json],
        )?;
        if updated == 0 {
            bail!("no work_runs row {run_row_id}");
        }
        Ok(())
    }

    /// Read back the run's file-progress ingress resume point.
    ///
    /// `Ok(None)` means "this run never recorded one" — a legitimate answer
    /// for a run dispatched by an engine that predates the column, and a
    /// distinct one from a read failure, which surfaces as `Err`.
    pub fn get_run_progress_ingress_checkpoint(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(None);
        };
        Ok(conn
            .query_row(
                "SELECT progress_ingress_checkpoint FROM work_runs WHERE id = ?1",
                params![run_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Drop the ingress resume point during normal execution teardown.
    /// Returns `false` when no run row exists.
    pub fn clear_run_progress_ingress_checkpoint(&self, execution_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(false);
        };
        let updated = conn.execute(
            "UPDATE work_runs SET progress_ingress_checkpoint = NULL WHERE id = ?1",
            params![run_id],
        )?;
        Ok(updated > 0)
    }

    /// Clear the engine-owned provider session identity during normal
    /// execution teardown. Returns `false` when no run row exists.
    ///
    /// Targets the same agent-session row as
    /// [`Self::claim_run_progress_session_identity`] so a newer pre-start
    /// failure bookkeeping sibling cannot capture the clear.
    pub fn clear_run_progress_session_identity(&self, execution_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(false);
        };
        let updated = conn.execute(
            "UPDATE work_runs SET progress_session_id = NULL WHERE id = ?1",
            params![run_id],
        )?;
        Ok(updated > 0)
    }

    /// `true` when at least one `work_runs` row exists for `execution_id`,
    /// regardless of its `transcript_path` or `status`.
    ///
    /// Used by the `TailRunTranscript` handler to distinguish between
    /// "execution was abandoned before dispatch (no work_runs row)" and
    /// "worker ran but transcript_path was not recorded (row exists, column
    /// is NULL)". The two cases warrant different diagnostic messages.
    pub fn has_run_row_for_execution(&self, execution_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM work_runs WHERE execution_id = ?1",
            params![execution_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Host id of the agent-session `work_runs` row for `execution_id`,
    /// or `None` when the execution has no run yet.
    ///
    /// The distributed-execution dispatch path stamps `host_id` on the
    /// run at start (`'local'` for local runs, the scheduler-selected
    /// id for remote ones). The transcript-tail RPC reads this to decide
    /// whether the recorded `transcript_path` lives on the local
    /// filesystem (`host_id = 'local'`) or must be pulled over SSH, and
    /// the live-status dispatcher reads it to decide whether a slotless
    /// run is a remote worker that warrants a virtual slot.
    ///
    /// Resolves the target run with the same preference order as
    /// [`Self::transcript_path_for_execution`] /
    /// [`resolve_run_id_for_execution_hooks`] so host and path stay
    /// paired when a newer pre-start failure bookkeeping sibling exists.
    pub fn latest_run_host_for_execution(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(None);
        };
        let host: Option<String> = conn
            .query_row("SELECT host_id FROM work_runs WHERE id = ?1", params![run_id], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(host)
    }

    /// Worker id (`work_runs.agent_id`, e.g. `worker-3`, `review-1`,
    /// `auto-worker-2`) of the agent-session `work_runs` row for
    /// `execution_id`, or `None` when the execution has no run yet.
    ///
    /// This is the *spawned worker's* id, not the reviewed/automated row's
    /// own `driver` column — [`crate::driver_transcript::driver_for_execution`]
    /// uses it to detect a pool-dispatched run (review/automation pool
    /// workers always run [`crate::coordinator::pool_dispatch_policy_for_worker_id`]'s
    /// fixed driver, overriding whatever `tasks.driver` the row carries,
    /// exactly as `SpawnResolutionInput::pool_policy_driver` does at
    /// spawn time).
    ///
    /// Resolves the target run with the same preference order as
    /// [`Self::transcript_path_for_execution`] /
    /// [`resolve_run_id_for_execution_hooks`] so the worker id and the
    /// transcript it produced stay paired.
    pub fn latest_run_agent_id_for_execution(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(None);
        };
        let agent_id: Option<String> = conn
            .query_row("SELECT agent_id FROM work_runs WHERE id = ?1", params![run_id], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(agent_id)
    }

    /// Persist the remote worker pid onto the agent-session `work_runs` row
    /// for `execution_id`. The SSH spawn path captures the pid from the
    /// wrapper handshake (`parse_remote_pid`) and stamps it here so the
    /// design's "Storage Additions" `work_runs.remote_pid` — the
    /// addressing key for control-channel signal delivery — is durable.
    ///
    /// Mirrors [`Self::set_run_transcript_path_if_unset`]'s namespace and
    /// target selection: `execution_id` is the `exec_*` id the spawn path
    /// holds, resolved to the agent-session `work_runs.id` via
    /// [`resolve_run_id_for_execution_hooks`]. Returns `true` when a row
    /// was updated, `false` when no run exists yet (benign — the caller
    /// logs and moves on; the pid is informational, not a spawn
    /// precondition).
    pub fn set_run_remote_pid_for_execution(&self, execution_id: &str, remote_pid: i64) -> Result<bool> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(false);
        };
        let updated = conn.execute(
            "UPDATE work_runs SET remote_pid = ?2 WHERE id = ?1",
            params![run_id, remote_pid],
        )?;
        Ok(updated > 0)
    }

    /// Persist the real OS shell pid of a *local* libghostty worker pane onto
    /// the agent-session `work_runs` row for `execution_id`. The macOS app
    /// reports this via the `UpdateWorkerShellPid` RPC once the pane's
    /// surface attaches; the engine stamps it here so the pid is durable
    /// across an engine restart (the in-memory
    /// [`crate::live_worker_state::LiveWorkerStateRegistry`] is empty on
    /// boot). [`crate::dead_pane_sweep`] then probes this pid with
    /// `kill(pid, 0)` to detect a pane that died with its host app while the
    /// execution row is still `waiting_human`.
    ///
    /// Keyed by `execution_id` (the app's `run_id`), which the `work_runs` row
    /// always exists for by the time the pid arrives (the run row is inserted
    /// synchronously at dispatch, before the pane is spawned). This makes the
    /// write **race-free** with respect to in-memory slot registration — the
    /// concurrent-spawn race that could drop the pid from the live registry
    /// ("no live slot found for run_id") cannot drop it from the DB. Mirrors
    /// [`Self::set_run_remote_pid_for_execution`] (same
    /// [`resolve_run_id_for_execution_hooks`] target). Returns `true` when a
    /// row was updated, `false` when no run exists yet (benign).
    pub fn set_run_shell_pid_for_execution(&self, execution_id: &str, shell_pid: i64) -> Result<bool> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(false);
        };
        let updated = conn.execute(
            "UPDATE work_runs SET shell_pid = ?2 WHERE id = ?1",
            params![run_id, shell_pid],
        )?;
        Ok(updated > 0)
    }

    /// Record the durable intent to create a tmux session for this execution's
    /// current run. The tmux spawn path will call this before it invokes tmux,
    /// making a session whose token is absent from the DB unambiguously leaked.
    ///
    /// No production path calls this yet; tmux identity is written only once
    /// tmux-hosted spawning is enabled.
    pub fn record_tmux_spawn_intent_for_execution(
        &self,
        execution_id: &str,
        server_label: &str,
        session_name: &str,
        spawn_token: &str,
    ) -> Result<bool> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(false);
        };
        let updated = conn.execute(
            "UPDATE work_runs
             SET tmux_server_label = ?2,
                 tmux_session_name = ?3,
                 tmux_spawn_token = ?4,
                 tmux_spawn_state = 'intended',
                 tmux_pane_pid = NULL
             WHERE id = ?1",
            params![run_id, server_label, session_name, spawn_token],
        )?;
        Ok(updated > 0)
    }

    /// Confirm that tmux created the session identified by `spawn_token` and
    /// report the initial pane pid it returned. The token, rather than a fresh
    /// resolution of `execution_id`, identifies the row that received the
    /// intent write; an execution can acquire sibling run rows between the two
    /// writes. Kept separate from intent persistence so a crash between the DB
    /// commit and `tmux new-session` remains visible as
    /// `tmux_spawn_state = 'intended'` to the adoption pass.
    pub fn record_tmux_session_created_for_execution(
        &self,
        execution_id: &str,
        spawn_token: &str,
        pane_pid: i64,
    ) -> Result<bool> {
        let conn = self.connect()?;
        let updated = conn.execute(
            "UPDATE work_runs
             SET tmux_spawn_state = 'created', tmux_pane_pid = ?3
             WHERE execution_id = ?1 AND tmux_spawn_token = ?2",
            params![execution_id, spawn_token, pane_pid],
        )?;
        Ok(updated > 0)
    }

    /// Stamp `at` (ISO-8601 UTC) as the moment this execution's current run
    /// delivered a turn boundary — the durable "the worker produced a terminal
    /// result" record.
    ///
    /// Last-write-wins on purpose: for a driver that serves several turns from
    /// one process the newest boundary is the interesting one, and for a
    /// one-turn-per-process driver there is only ever one. Targets the same
    /// [`resolve_run_id_for_execution_hooks`] row every other hook-driven write
    /// uses, so the record is scoped to the process currently attached to the
    /// run and a resumed execution starts from NULL again.
    ///
    /// Returns `true` when a row was updated, `false` when no run exists yet
    /// (benign — a boundary cannot precede the run row, which is inserted
    /// synchronously at dispatch).
    pub fn record_run_turn_boundary_for_execution(&self, execution_id: &str, at: &str) -> Result<bool> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(false);
        };
        let updated = conn.execute(
            "UPDATE work_runs SET turn_boundary_at = ?2 WHERE id = ?1",
            params![run_id, at],
        )?;
        Ok(updated > 0)
    }

    /// The turn boundary this execution's current run delivered, if any.
    ///
    /// `None` means no terminal result has ever been observed for the process
    /// currently attached to the run. Keyed on the same row
    /// [`Self::record_run_turn_boundary_for_execution`] writes, so a prior
    /// run's boundary can never vouch for a later process.
    pub fn latest_run_turn_boundary_for_execution(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(None);
        };
        conn.query_row(
            "SELECT turn_boundary_at FROM work_runs WHERE id = ?1",
            params![run_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map(Option::flatten)
        .map_err(Into::into)
    }

    /// The shell pid of the **latest** `work_runs` row for `execution_id`,
    /// returned only when that latest run ran locally and recorded a pid;
    /// `None` when the latest run ran on a remote host, has no pid yet, or no
    /// run exists. Used by [`crate::dead_pane_sweep`] to probe pane liveness
    /// after a restart.
    ///
    /// Deliberately keys on the *latest* run's id (via subquery), NOT "the
    /// latest run that happens to have a pid": on a resumed execution the
    /// current run is what matters, and falling back to a prior run's stale
    /// (now-dead) pid while the current pane is merely still reporting would
    /// risk reaping a live worker. The `host_id = 'local'` gate is a hard
    /// safety rail — a `kill(pid, 0)` probe on the engine host is meaningless
    /// for a remote worker whose pid lives on another machine.
    pub fn latest_local_shell_pid_for_execution(&self, execution_id: &str) -> Result<Option<i64>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT shell_pid FROM work_runs
             WHERE id = (
                 SELECT id FROM work_runs
                 WHERE execution_id = ?1
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1
             ) AND host_id = 'local'",
            params![execution_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()
        .map(Option::flatten)
        .map_err(Into::into)
    }

    /// `created_at` of the **latest** `work_runs` row for `execution_id`,
    /// parsed as Unix epoch seconds. `None` when no run exists or the
    /// column is unparseable.
    ///
    /// Unlike [`WorkExecution::started_epoch`], which reflects only the
    /// *first* run (`start_execution_run` stamps `started_at =
    /// COALESCE(started_at, now)`, so a resumed execution's `started_at`
    /// never advances), this ages off the current run's own start —
    /// exactly what a pane-attach-deadline check on a *resumed* execution
    /// needs, so a fresh run's not-yet-attached pane is never judged
    /// against an ancient first-run timestamp.
    pub fn latest_run_started_epoch_for_execution(&self, execution_id: &str) -> Result<Option<i64>> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(None);
        };
        let created_at: Option<String> = conn
            .query_row(
                "SELECT created_at FROM work_runs WHERE id = ?1",
                params![run_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(created_at.and_then(|s| s.parse::<i64>().ok()))
    }

    /// [`Self::latest_local_shell_pid_for_execution`], restricted to a run row
    /// whose most recent timestamp falls within `max_age_secs` of
    /// `now_epoch_secs`.
    ///
    /// Same row selection and same `host_id = 'local'` rail; the only addition
    /// is the pid-reuse bound that
    /// [`Self::latest_local_worker_process_for_work_item`] already applies, so
    /// the destructive caller
    /// ([`crate::app::ServerState::release_worker_pane`]'s durable-pid reap)
    /// cannot act on a pid this table can no longer vouch for.
    ///
    /// The bound is on `COALESCE(finished_at, created_at)` rather than
    /// `created_at`: it exists to bound how long ago the engine last had
    /// first-hand knowledge of the process, not how long the run has been
    /// going. A six-hour worker whose run was terminalized a minute ago is
    /// precisely what the reap path is for, and a `created_at` anchor would
    /// exempt it.
    pub fn latest_local_shell_pid_for_execution_within(
        &self,
        execution_id: &str,
        max_age_secs: i64,
        now_epoch_secs: i64,
    ) -> Result<Option<i64>> {
        let conn = self.connect()?;
        let cutoff = now_epoch_secs - max_age_secs;
        conn.query_row(
            "SELECT shell_pid FROM work_runs
             WHERE id = (
                 SELECT id FROM work_runs
                 WHERE execution_id = ?1
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1
             ) AND host_id = 'local'
               AND CAST(COALESCE(finished_at, created_at) AS INTEGER) >= ?2",
            params![execution_id, cutoff],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()
        .map(Option::flatten)
        .map_err(Into::into)
    }

    /// The newest LOCAL worker process this work item ever recorded: the
    /// `(execution_id, shell_pid)` of the most recently created `work_runs`
    /// row across ALL of the item's executions — terminal ones included —
    /// that ran locally and reported a pid, restricted to rows created within
    /// `max_age_secs`.
    ///
    /// This is the work-item-scoped sibling of
    /// [`Self::latest_local_shell_pid_for_execution`], and it exists for the
    /// one question that helper cannot answer: *before I dispatch a second
    /// worker onto this row, is the previous one still running?* The
    /// re-dispatchers ([`crate::orphan_sweep`]) reach that decision point
    /// holding only a `work_item_id` — the execution they would be duplicating
    /// is already terminal, so no "live execution" lookup finds it, and the
    /// in-memory `LiveWorkerStateRegistry` entry that used to hold its pid was
    /// dropped by `release_worker_pane`. The durable `work_runs.shell_pid` is
    /// the only surviving handle on that process.
    ///
    /// **Terminal executions are deliberately included.** Excluding them would
    /// make this blind to the exact failure it guards: an execution the engine
    /// wrongly terminalized while its worker kept running.
    ///
    /// `max_age_secs` bounds pid reuse. A recorded pid is only meaningful while
    /// the OS has not recycled it, so a caller must state how far back it is
    /// willing to trust the number; see
    /// [`crate::durable_liveness::REDISPATCH_PID_TRUST_SECS`] for the value the
    /// re-dispatch guard uses and why. Rows older than that are not returned at
    /// all, so a caller can never act on a pid this table can no longer vouch
    /// for.
    ///
    /// The `host_id = 'local'` gate is the same hard safety rail
    /// [`Self::latest_local_shell_pid_for_execution`] applies: `kill(pid, 0)`
    /// on the engine host says nothing about a pid on another machine.
    pub fn latest_local_worker_process_for_work_item(
        &self,
        work_item_id: &str,
        max_age_secs: i64,
        now_epoch_secs: i64,
    ) -> Result<Option<(String, i64)>> {
        let conn = self.connect()?;
        let cutoff = now_epoch_secs - max_age_secs;
        conn.query_row(
            "SELECT r.execution_id, r.shell_pid
             FROM work_runs r
             JOIN work_executions e ON e.id = r.execution_id
             WHERE e.work_item_id = ?1
               AND r.host_id = 'local'
               AND r.shell_pid IS NOT NULL
               AND r.shell_pid > 0
               AND CAST(r.created_at AS INTEGER) >= ?2
             ORDER BY r.created_at DESC, r.id DESC
             LIMIT 1",
            params![work_item_id, cutoff],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Active runs on a non-local host whose backing execution is still
    /// non-terminal — the set of detached remote workers the engine
    /// should re-attach to after a restart.
    ///
    /// A remote worker is launched detached (`nohup`) and survives the
    /// engine restarting, but the reverse events-socket forward that
    /// carries its hook stream rides the engine's `ControlMaster` and
    /// dies with the old engine process. On startup the engine queries
    /// this set and re-establishes each forward (see
    /// [`crate::remote_reattach`]) so the still-running worker's events
    /// — and its eventual `Stop` / PR-URL completion — reach the engine
    /// again. Local runs are excluded (`host_id != 'local'`): a local
    /// worker is a child of the previous engine and is already gone.
    /// Terminal executions are excluded so a settled run is never
    /// re-attached.
    pub fn list_reattachable_remote_runs(&self) -> Result<Vec<RemoteRunHandle>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT r.id, r.execution_id, r.host_id, r.remote_pid
             FROM work_runs r
             JOIN work_executions e ON e.id = r.execution_id
             WHERE r.status = 'active'
               AND r.host_id != 'local'
               AND e.status NOT IN
                   ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
             ORDER BY r.created_at ASC, r.id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(RemoteRunHandle {
                run_id: row.get(0)?,
                execution_id: row.get(1)?,
                host_id: row.get(2)?,
                remote_pid: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Local, non-terminal worker runs whose tmux identity was durably
    /// recorded. The startup adoption pass enumerates tmux separately and
    /// performs an exact token match against this set; neither a session name
    /// nor a pane pid is sufficient to adopt a process safely.
    ///
    /// A run remains eligible while its spawn state is `intended`, because a
    /// crash after `tmux new-session` but before its confirmation write leaves
    /// precisely that recoverable signature.
    pub fn list_adoptable_tmux_runs(&self) -> Result<Vec<TmuxRunHandle>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT r.id, r.execution_id, r.agent_id, r.transcript_path,
                    r.tmux_server_label, r.tmux_session_name, r.tmux_spawn_token,
                    r.tmux_spawn_state, r.tmux_pane_pid
             FROM work_runs r
             JOIN work_executions e ON e.id = r.execution_id
             WHERE r.status = 'active'
               AND r.host_id = 'local'
               AND r.tmux_spawn_token IS NOT NULL
               AND r.tmux_server_label IS NOT NULL
               AND r.tmux_session_name IS NOT NULL
               AND r.tmux_spawn_state IS NOT NULL
               AND e.status NOT IN
                   ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
             ORDER BY r.created_at ASC, r.id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(TmuxRunHandle {
                run_id: row.get(0)?,
                execution_id: row.get(1)?,
                agent_id: row.get(2)?,
                transcript_path: row.get(3)?,
                tmux_server_label: row.get(4)?,
                tmux_session_name: row.get(5)?,
                tmux_spawn_token: row.get(6)?,
                tmux_spawn_state: row.get(7)?,
                tmux_pane_pid: row.get(8)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Test-only helper: force `transcript_path` back to NULL on an
    /// existing row. Used by the dispatcher regression test to model
    /// the production race where a SessionStart's payload-driven
    /// persist fired against a work_runs row that did not exist
    /// yet, leaving the column NULL after the row was later
    /// inserted. The cache fallback (this PR) is what allows a
    /// subsequent hook to finally win.
    #[cfg(test)]
    pub fn force_updated_at_for_test(&self, work_item_id: &str, epoch_secs: i64) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE tasks SET updated_at = ?2 WHERE id = ?1",
            params![work_item_id, epoch_secs.to_string()],
        )?;
        Ok(())
    }

    /// Force `completed_at` to an exact epoch-seconds value on an existing
    /// row, bypassing the normal `COALESCE(completed_at, now)` write-once
    /// semantics. `completed_at`'s one-second resolution makes tests that
    /// depend on strict before/after ordering between two rows racy against
    /// real wall-clock time (two DB writes in the same in-process test can
    /// easily land in the same second); this lets a test establish a
    /// deterministic ordering directly instead.
    #[cfg(test)]
    pub fn force_completed_at_for_test(&self, work_item_id: &str, epoch_secs: i64) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE tasks SET completed_at = ?2 WHERE id = ?1",
            params![work_item_id, epoch_secs.to_string()],
        )?;
        Ok(())
    }

    /// Inserts a terminal `work_executions` row with the given `kind` and
    /// `status` — used to build up churn-guard fixtures. Callers testing
    /// the kind-scoped guard (e.g. [`crate::pr_review_recovery`]) pass
    /// `kind = "pr_review"`; callers testing the unscoped guard pass
    /// whatever kind, typically `"chore_implementation"`.
    #[cfg(test)]
    pub fn insert_terminal_execution_for_test(
        &self,
        work_item_id: &str,
        kind: &str,
        status: &str,
        created_at_epoch: i64,
    ) -> Result<()> {
        let conn = self.connect()?;
        let id = format!("exec-test-{}-{}-{}", kind, work_item_id, created_at_epoch);
        conn.execute(
            "INSERT INTO work_executions
               (id, work_item_id, kind, status, repo_remote_url,
                priority, created_at)
             VALUES (?1, ?2, ?3, ?4,
                     'https://github.com/test/repo', 0, ?5)",
            params![id, work_item_id, kind, status, created_at_epoch.to_string()],
        )?;
        Ok(())
    }

    /// Rewrite `created_at` on every terminal execution for a work item to
    /// the given epoch. Lets a test simulate the churn guard's trailing
    /// window draining (a real re-check would just wait for wall-clock time
    /// to pass) by moving previously-recent terminal executions outside the
    /// window deterministically.
    #[cfg(test)]
    pub fn backdate_terminal_executions_for_test(&self, work_item_id: &str, created_at_epoch: i64) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE work_executions
                SET created_at = ?2
              WHERE work_item_id = ?1
                AND status IN ('orphaned', 'abandoned', 'failed')",
            params![work_item_id, created_at_epoch.to_string()],
        )?;
        Ok(())
    }

    /// Mark a task `done` without running `cascade_dependents_after_prereq_status_change`.
    /// Used in tests that need to simulate the engine being offline when a
    /// prereq transitions, so the sweeper can be exercised as the recovery path.
    #[cfg(test)]
    pub fn mark_task_done_for_test_no_cascade(&self, task_id: &str) -> Result<()> {
        let conn = self.connect()?;
        let now = now_string();
        conn.execute(
            "UPDATE tasks
             SET status = 'done', last_status_actor = 'engine', updated_at = ?2
             WHERE id = ?1 AND deleted_at IS NULL",
            params![task_id, now],
        )?;
        Ok(())
    }

    /// Overwrite `last_status_actor` for a task without touching any other
    /// column. Used in tests to simulate a concurrent update that reset the
    /// actor (the scenario that previously caused the cascade to skip an item).
    #[cfg(test)]
    pub fn force_last_status_actor_for_test(&self, task_id: &str, actor: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE tasks SET last_status_actor = ?2 WHERE id = ?1",
            params![task_id, actor],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn force_execution_status_for_test(&self, work_item_id: &str, status: ExecutionStatus) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE work_executions SET status = ?2 WHERE work_item_id = ?1",
            params![work_item_id, status.as_str()],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn force_task_status_for_test(&self, task_id: &str, status: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE tasks SET status = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![task_id, status],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn force_started_at_for_test(&self, execution_id: &str, epoch_secs: i64) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE work_executions SET started_at = ?2 WHERE id = ?1",
            params![execution_id, epoch_secs.to_string()],
        )?;
        Ok(())
    }

    /// Backdate the *latest run's* `created_at`/`started_at`, as distinct
    /// from [`Self::force_started_at_for_test`] which backdates
    /// `work_executions.started_at` (frozen at the first run by
    /// `start_execution_run`'s `COALESCE`). Lets a test model a resumed
    /// execution whose first-ever `started_at` is ancient while its current
    /// run is fresh, or vice versa.
    #[cfg(test)]
    pub fn force_latest_run_started_at_for_test(&self, execution_id: &str, epoch_secs: i64) -> Result<()> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            bail!("no run exists for execution {execution_id}");
        };
        conn.execute(
            "UPDATE work_runs SET created_at = ?2, started_at = ?2 WHERE id = ?1",
            params![run_id, epoch_secs.to_string()],
        )?;
        Ok(())
    }

    pub fn force_transient_failure_count_for_test(&self, execution_id: &str, count: i64) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE work_executions SET transient_failure_count = ?2 WHERE id = ?1",
            params![execution_id, count],
        )?;
        Ok(())
    }

    /// Overwrite `branch_naming` for an execution row. Used in tests to
    /// verify that the detector reconstructs the correct branch name from
    /// the snapshotted strategy without needing to re-create the full
    /// product/editorial-rules fixture.
    #[cfg(test)]
    pub fn force_branch_naming_for_test(&self, execution_id: &str, naming: &BranchNaming) -> Result<()> {
        let json = serde_json::to_string(naming)
            .with_context(|| format!("failed to serialise BranchNaming for {execution_id}"))?;
        let conn = self.connect()?;
        conn.execute(
            "UPDATE work_executions SET branch_naming = ?2 WHERE id = ?1",
            params![execution_id, json],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn clear_run_transcript_path_for_test(&self, run_id: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE work_runs SET transcript_path = NULL WHERE id = ?1",
            params![run_id],
        )?;
        Ok(())
    }

    /// Stamp the actual pane-slot identity onto an existing run record.
    /// The coordinator inserts the run with the worker-pool placeholder
    /// (`worker-N` from capacity tracking), then calls this once the
    /// app has reported the real slot allocation back from
    /// `SpawnWorkerPane`. After this point `agent_id` is treated as
    /// immutable for the run's lifetime — re-spawning into a different
    /// slot would create a new run rather than mutate this one.
    pub fn set_run_agent_id(&self, run_id: &str, agent_id: &str) -> Result<WorkRun> {
        let conn = self.connect()?;
        let updated = conn.execute(
            "UPDATE work_runs SET agent_id = ?2 WHERE id = ?1",
            params![run_id, agent_id],
        )?;
        if updated == 0 {
            bail!("unknown run: {run_id}");
        }
        query_run(&conn, run_id).require("run", run_id)
    }
}

/// Pick the `work_runs` row that hook-driven writes (transcript path,
/// cost snapshot), progress-session claim/clear, remote/shell pid stamps,
/// and the live-status path/host read path should target for an execution.
///
/// Preference order (see `WorkDb::set_run_transcript_path_if_unset`):
/// 1. unfinished (`finished_at IS NULL`) — the live agent session;
/// 2. non-`failed` — pane-spawn parks the dispatch run as `completed`
///    almost immediately while the worker keeps emitting hooks;
/// 3. rows with a non-NULL `transcript_path` — when both candidates are
///    terminal-failed (agent-session fail vs a newer permanent pre-start
///    bookkeeping sibling), prefer the row that actually hosted a worker
///    and already has path/cost; pure newest would otherwise hand residual
///    hooks and host reads to the path-less bookkeeping row;
/// 4. newest by `(created_at DESC, id DESC)`.
///
/// Pre-start failure bookkeeping rows (`status='failed'`, never hosted a
/// worker) sort last so a later re-queue's failed sibling cannot capture
/// path/cost writes that belong to the real session row, and so the
/// live-status resolver does not report NULL/wrong-host when only a failed
/// sibling is newer than a path-bearing session.
fn resolve_run_id_for_execution_hooks(conn: &Connection, execution_id: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT id FROM work_runs
         WHERE execution_id = ?1
         ORDER BY
           CASE WHEN finished_at IS NULL THEN 0 ELSE 1 END,
           CASE WHEN status = 'failed' THEN 1 ELSE 0 END,
           CASE WHEN transcript_path IS NOT NULL THEN 0 ELSE 1 END,
           created_at DESC,
           id DESC
         LIMIT 1",
        params![execution_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}
