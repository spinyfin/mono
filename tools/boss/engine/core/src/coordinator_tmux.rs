//! Engine-owned lifecycle for the durable coordinator tmux session.
//!
//! The coordinator is deliberately not a `work_runs` row: it has no slot,
//! workspace, or execution. Its durable pointer is instead the metadata
//! singleton managed by [`crate::work::WorkDb`]. The write ordering remains
//! identical to a worker session: commit `intended`, create with the token in
//! the atomic tmux environment, mirror/confirm, then mark `created`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use boss_protocol::CoordinatorRecreateReason;
use boss_tmux::{DisplayField, NewSession, Tmux};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::audit;
use crate::engine_control::generate_token;
use crate::spawn_flow::TMUX_SESSION_SCHEMA;
use crate::work::{CoordinatorTmuxRecord, WorkDb};

pub const COORDINATOR_SESSION_NAME: &str = "boss-coordinator";
const SPAWN_TOKEN_ENV: &str = "BOSS_SPAWN_TOKEN";
const SESSION_SCHEMA_ENV: &str = "BOSS_SESSION_SCHEMA";
const SPAWN_TOKEN_OPTION: &str = "@boss_spawn_token";

/// Filename of the coordinator's rendered instructions, written into
/// `working_directory` by the app on every launch (see `bossSystemPrompt`
/// in `BossPaneModel.swift`). The re-read nudge hashes this file's
/// *content* — never its mtime or size, since the app unconditionally
/// rewrites it on every launch regardless of whether the content changed.
const RENDERED_PROMPT_FILENAME: &str = "CLAUDE.md";

/// Metadata key for the content hash of the rendered prompt as of the last
/// successfully sent re-read nudge (or the baseline seeded at session
/// creation / first adoption with no recorded hash). Absent means this
/// coordinator predates the feature or seeding never persisted — an
/// adoption with no recorded baseline seeds the current hash and returns
/// without sending, because there is no evidence the prompt changed.
const PROMPT_NUDGE_HASH_KEY: &str = "coordinator.prompt_nudge_hash";

/// Dependency-injected probe for the installed `claude` binary's version,
/// mirroring the `Tmux` injection this module already uses elsewhere.
/// Threading this in (rather than `start_new` shelling out directly) keeps
/// every lifecycle function in this module hermetic under test — see
/// [`RealClaudeVersionProbe`] for the production implementation and
/// [`coordinator_update_available`] for why the *comparison* is a separate,
/// fully pure function.
#[async_trait::async_trait]
pub(crate) trait ClaudeVersionProbe: Send + Sync {
    async fn probe(&self) -> Option<String>;
}

/// Production probe: shells out to `claude --version` with a bounded
/// timeout. See [`probe_installed_claude_version`].
pub(crate) struct RealClaudeVersionProbe;

#[async_trait::async_trait]
impl ClaudeVersionProbe for RealClaudeVersionProbe {
    async fn probe(&self) -> Option<String> {
        probe_installed_claude_version().await
    }
}

/// Create or recover the coordinator for an app that has just registered.
///
/// A model mismatch leaves the live conversation intact; the app compares the
/// returned model with its requested model before asking for replacement.
/// `working_directory` is the prepared Boss-session directory; callers
/// resolve it once via [`coordinator_working_directory`]. `version_probe` is
/// only ever consulted when this call actually creates a new session (see
/// [`start_new`]).
pub(crate) async fn ensure_for_attach(
    work_db: &WorkDb,
    tmux: &Tmux,
    create_tmux: &Tmux,
    requested_model: &str,
    working_directory: &Path,
    version_probe: &dyn ClaudeVersionProbe,
) -> Result<CoordinatorTmuxRecord> {
    match work_db.coordinator_tmux_record()? {
        None => start_new(work_db, create_tmux, requested_model, working_directory, version_probe).await,
        Some(record) => {
            reconcile_existing(
                work_db,
                tmux,
                create_tmux,
                requested_model,
                record,
                working_directory,
                version_probe,
            )
            .await
        }
    }
}

/// Restart a previously-created coordinator whose tmux session or child has
/// disappeared. This never creates the singleton from scratch: first
/// creation remains tied to app registration, after the app has prepared the
/// coordinator's isolated session directory.
///
/// Returns the replacement record only when the viewer must reattach. A
/// healthy session and a live model mismatch are deliberately left alone.
pub(crate) async fn restart_if_dead(
    work_db: &WorkDb,
    tmux: &Tmux,
    create_tmux: &Tmux,
    requested_model: &str,
    working_directory: &Path,
    version_probe: &dyn ClaudeVersionProbe,
) -> Result<Option<CoordinatorTmuxRecord>> {
    let Some(record) = work_db.coordinator_tmux_record()? else {
        return Ok(None);
    };
    if !session_exists(tmux, &record.session_name).await? {
        return start_new(work_db, create_tmux, requested_model, working_directory, version_probe)
            .await
            .map(Some);
    }
    let live_token = tmux.show_environment(&record.session_name, SPAWN_TOKEN_ENV).await?;
    match live_token {
        Some(token) if token == record.spawn_token => {
            crate::tmux_session_options::apply(tmux, &record.session_name)
                .await
                .context("applying Boss coordinator tmux session options")?;
        }
        Some(_) => bail!("coordinator tmux token does not match the metadata singleton"),
        None => bail!("coordinator tmux session exists without the metadata singleton token"),
    }
    if tmux
        .display_message(&record.session_name, DisplayField::PaneDead)
        .await?
        .trim()
        != "1"
    {
        if record.spawn_state == "intended" {
            confirm_existing_intent(work_db, tmux, &record).await?;
        }
        return Ok(None);
    }
    tmux.kill_session_verified(&record.session_name, &record.spawn_token)
        .await
        .context("removing dead coordinator tmux session before restart")?;
    start_new(work_db, create_tmux, requested_model, working_directory, version_probe)
        .await
        .map(Some)
}

/// Consecutive-failure tracker for the coordinator tmux supervisor.
///
/// Counts consecutive failed supervisor passes. A failed pass
/// (`restart_if_dead` returning `Err`, or a session directory that cannot
/// be resolved) increments; a healthy or recovered pass resets the count.
/// Reaching `LIMIT` raises the operator-facing restart-failure ceiling
/// attention once per streak.
///
/// The count is consecutive, not windowed: the supervisor already backs off
/// a full minute between hard failures, so a wall-clock window of that same
/// minute would reset the counter on every retry and the ceiling would still
/// never fire.
///
/// Separately tracks consecutive *successful* restarts (`Ok(Some(record))`
/// from `restart_if_dead`): a coordinator whose `claude` child exits
/// immediately (bad model, expired auth, `claude` missing from `PATH`) kills
/// and recreates the tmux session on every pass, which is a technically
/// successful restart and never touches the failure counter above. Left
/// unchecked that churns forever with no backoff and no operator signal, so
/// `record_restart` gives that path its own escalating backoff and its own
/// ceiling attention, reset only by a genuinely idle pass (`Ok(None)`).
pub(crate) struct CoordinatorRestartFailures {
    consecutive_failures: u32,
    ceiling_notified: bool,
    restart_churn: u32,
    restart_churn_notified: bool,
}

/// What the supervisor should do after recording a failed pass.
pub(crate) struct RestartFailureDecision {
    pub delay: Duration,
    /// Raise the operator attention; true only the first time we reach the ceiling.
    pub notify: bool,
}

/// What the supervisor should do after recording a successful restart.
pub(crate) struct RestartChurnDecision {
    pub delay: Duration,
    /// Raise the operator attention; true only the first time we reach the ceiling.
    pub notify: bool,
}

impl CoordinatorRestartFailures {
    pub(crate) const LIMIT: u32 = 5;
    pub(crate) const BACKOFF_CAP: Duration = Duration::from_secs(60);

    pub(crate) fn new() -> Self {
        Self {
            consecutive_failures: 0,
            ceiling_notified: false,
            restart_churn: 0,
            restart_churn_notified: false,
        }
    }

    pub(crate) fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    pub(crate) fn restart_churn(&self) -> u32 {
        self.restart_churn
    }

    /// Record a genuinely healthy pass: no restart was needed. Resets both
    /// the failure counter and the restart-churn counter.
    pub(crate) fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.ceiling_notified = false;
        self.restart_churn = 0;
        self.restart_churn_notified = false;
    }

    /// Record a failed pass (`Err`, or session directory not ready). The
    /// retry cadence stays flat at `BACKOFF_CAP` once the ceiling is
    /// reached — the supervisor keeps retrying, it does not pause.
    pub(crate) fn record_failure(&mut self) -> RestartFailureDecision {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures < Self::LIMIT {
            return RestartFailureDecision {
                delay: Self::BACKOFF_CAP,
                notify: false,
            };
        }
        let notify = !self.ceiling_notified;
        self.ceiling_notified = true;
        RestartFailureDecision {
            delay: Self::BACKOFF_CAP,
            notify,
        }
    }

    /// Record a successful kill-and-recreate (`Ok(Some(record))`). This is
    /// not an `Err`, so it resets the hard-failure counter, but it escalates
    /// its own backoff (2s, 4s, 8s, 16s, 32s, capped at `BACKOFF_CAP`) so a
    /// crash-looping `claude` child cannot respawn every 2s forever, and
    /// raises its own ceiling attention once per streak.
    pub(crate) fn record_restart(&mut self) -> RestartChurnDecision {
        self.consecutive_failures = 0;
        self.ceiling_notified = false;
        self.restart_churn = self.restart_churn.saturating_add(1);
        let delay = Self::churn_backoff(self.restart_churn);
        if self.restart_churn < Self::LIMIT {
            return RestartChurnDecision { delay, notify: false };
        }
        let notify = !self.restart_churn_notified;
        self.restart_churn_notified = true;
        RestartChurnDecision { delay, notify }
    }

    fn churn_backoff(count: u32) -> Duration {
        let secs = 2u64.saturating_pow(count.min(6));
        Duration::from_secs(secs).min(Self::BACKOFF_CAP)
    }
}

/// Recreate the coordinator after an explicit UI confirmation — either a
/// model-mismatch replacement or an operator-initiated reset. The expected
/// token prevents a delayed confirmation from killing a newer session
/// created by a concurrent restart recovery. `reason` is recorded to the
/// audit log so a manual reset is distinguishable there both from the
/// automatic model-mismatch path and from a crash/session-loss restart
/// (`restart_if_dead`/`reconcile_existing`), neither of which audits this
/// event at all.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn recreate_after_confirmation(
    work_db: &WorkDb,
    tmux: &Tmux,
    create_tmux: &Tmux,
    requested_model: &str,
    expected_spawn_token: &str,
    working_directory: &Path,
    reason: CoordinatorRecreateReason,
    version_probe: &dyn ClaudeVersionProbe,
) -> Result<CoordinatorTmuxRecord> {
    let result = recreate_after_confirmation_inner(
        work_db,
        tmux,
        create_tmux,
        requested_model,
        expected_spawn_token,
        working_directory,
        version_probe,
    )
    .await;
    match &result {
        Ok(record) => audit::record_event(
            "coordinator_recreate",
            &json!({
                "outcome": "success",
                "reason": reason,
                "old_spawn_token": expected_spawn_token,
                "new_spawn_token": record.spawn_token,
            }),
        ),
        Err(error) => audit::record_event(
            "coordinator_recreate",
            &json!({
                "outcome": "failed",
                "reason": reason,
                "old_spawn_token": expected_spawn_token,
                "error": format!("{error:#}"),
            }),
        ),
    }
    result
}

async fn recreate_after_confirmation_inner(
    work_db: &WorkDb,
    tmux: &Tmux,
    create_tmux: &Tmux,
    requested_model: &str,
    expected_spawn_token: &str,
    working_directory: &Path,
    version_probe: &dyn ClaudeVersionProbe,
) -> Result<CoordinatorTmuxRecord> {
    let record = work_db
        .coordinator_tmux_record()?
        .ok_or_else(|| anyhow!("no coordinator tmux record exists"))?;
    if record.spawn_token != expected_spawn_token {
        bail!("coordinator changed before confirmation; refresh and confirm the current session instead");
    }
    if session_exists(tmux, &record.session_name).await? {
        match tmux.show_environment(&record.session_name, SPAWN_TOKEN_ENV).await? {
            Some(token) if token == record.spawn_token => {
                tmux.kill_session_verified(&record.session_name, &record.spawn_token)
                    .await
                    .context("destroying the confirmed coordinator session")?;
            }
            Some(_) => bail!("coordinator tmux token does not match the metadata singleton"),
            None => bail!("coordinator tmux session exists without the metadata singleton token"),
        }
    }
    start_new(work_db, create_tmux, requested_model, working_directory, version_probe).await
}

async fn reconcile_existing(
    work_db: &WorkDb,
    tmux: &Tmux,
    create_tmux: &Tmux,
    requested_model: &str,
    mut record: CoordinatorTmuxRecord,
    working_directory: &Path,
    version_probe: &dyn ClaudeVersionProbe,
) -> Result<CoordinatorTmuxRecord> {
    if !session_exists(tmux, &record.session_name).await? {
        // Covers both crash windows in which metadata was committed but
        // `new-session` never happened, and normal session loss. No live
        // conversation remains, so recreation is non-destructive.
        return start_new(work_db, create_tmux, requested_model, working_directory, version_probe).await;
    }
    let live_token = tmux.show_environment(&record.session_name, SPAWN_TOKEN_ENV).await?;
    match live_token {
        Some(token) if token == record.spawn_token => {
            crate::tmux_session_options::apply(tmux, &record.session_name)
                .await
                .context("applying Boss coordinator tmux session options")?;
            if tmux
                .display_message(&record.session_name, DisplayField::PaneDead)
                .await?
                .trim()
                == "1"
            {
                tmux.kill_session_verified(&record.session_name, &record.spawn_token)
                    .await
                    .context("removing dead coordinator tmux session before restart")?;
                return start_new(work_db, create_tmux, requested_model, working_directory, version_probe).await;
            }
            // Live matching-token sessions are left alone (including
            // model mismatches, which the app surfaces for confirmation).
            // An interrupted create still needs its token mirror repaired.
            if record.spawn_state == "intended" {
                confirm_existing_intent(work_db, tmux, &record).await?;
                record.spawn_state = "created".to_owned();
            }
            seed_claude_version_baseline_if_missing(work_db, &mut record, version_probe).await;
            // This branch is reached only when the engine is *adopting* a
            // session that already existed (outlived a prior process) —
            // as opposed to one just created by `start_new` above/below,
            // which already has the current prompt by construction.
            maybe_nudge_prompt_change(work_db, tmux, &record, working_directory).await;
            Ok(record)
        }
        Some(_) => bail!("coordinator tmux token does not match the metadata singleton"),
        None => bail!("coordinator tmux session exists without the metadata singleton token"),
    }
}

/// An adopted session that predates version tracking has no trustworthy
/// launch-version evidence. Seed it from a single current probe so this
/// upgrade cycle fails closed; later upgrades compare against that honest
/// baseline. This mirrors `seed_prompt_nudge_baseline`: best effort probe,
/// durable seed, and never overwrite an existing baseline. A failed probe
/// yields no information at all, so nothing is written and the next
/// adoption retries — writing an empty value here would be indistinguishable
/// from a real baseline and would permanently block future seeding attempts
/// (see `coordinator_tmux_claude_version_is_missing`'s doc comment: an empty
/// value is intentionally *not* treated as missing).
async fn seed_claude_version_baseline_if_missing(
    work_db: &WorkDb,
    record: &mut CoordinatorTmuxRecord,
    version_probe: &dyn ClaudeVersionProbe,
) {
    let is_missing = match work_db.coordinator_tmux_claude_version_is_missing() {
        Ok(is_missing) => is_missing,
        Err(error) => {
            tracing::warn!(
                error = %format!("{error:#}"),
                "coordinator claude version baseline: failed to check whether a baseline is missing"
            );
            return;
        }
    };
    if !is_missing {
        return;
    }
    let Some(current_version) = version_probe.probe().await else {
        return;
    };
    match work_db.seed_coordinator_tmux_claude_version_if_absent(Some(&current_version)) {
        Ok(seeded) => {
            if seeded {
                record.launched_claude_version = Some(current_version);
            }
        }
        Err(error) => {
            tracing::warn!(
                error = %format!("{error:#}"),
                "coordinator claude version baseline: failed to seed baseline for adopted session"
            );
        }
    }
}

async fn session_exists(tmux: &Tmux, name: &str) -> Result<bool> {
    Ok(tmux.list_sessions().await?.iter().any(|session| session.name == name))
}

/// Resolve which tmux server currently hosts the coordinator session:
/// `socket_tmux` (the durable `-S` socket) unless a live
/// [`COORDINATOR_SESSION_NAME`] session survives on the pre-move `-L boss`
/// server, in which case `legacy_tmux`.
///
/// Every coordinator lifecycle entry point (`ensure_for_attach`,
/// `restart_if_dead`, `recreate_after_confirmation`) takes a single `&Tmux`
/// and, before this routing existed, every caller always passed the socket
/// handle. That meant `session_exists(tmux, ...)` reported "absent" for a
/// coordinator the boot-time drain had adopted on the legacy server (it
/// lives on a different server entirely), so the supervisor's restart loop
/// would spawn a brand-new coordinator on the socket on top of the still-live
/// legacy one — two agents driving the same `state.db`. Calling this once,
/// before invoking any lifecycle function, and passing its result through
/// for that call routes every check (`session_exists`, `show_environment`,
/// `kill_session_verified`, `new_session`) at the server that actually holds
/// the session, so the legacy coordinator is recovered/replaced in place
/// rather than shadowed by a second one.
///
/// `legacy_tmux` is `None` when the caller could not build a legacy handle
/// (e.g. no resolved tmux executable yet) — in that case this always
/// returns `socket_tmux`, matching the engine's pre-migration behavior.
pub(crate) async fn resolve_active_handle<'a>(socket_tmux: &'a Tmux, legacy_tmux: Option<&'a Tmux>) -> &'a Tmux {
    let Some(legacy) = legacy_tmux else {
        return socket_tmux;
    };
    match session_exists(legacy, COORDINATOR_SESSION_NAME).await {
        Ok(true) => {
            tracing::warn!(
                "coordinator tmux: a live coordinator session survives on the pre-move -L boss server; \
                 routing this lifecycle call through it instead of the durable socket so the engine does \
                 not spawn a second live coordinator on top of it",
            );
            legacy
        }
        Ok(false) => socket_tmux,
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "coordinator tmux: failed to probe the legacy -L boss server for a surviving coordinator \
                 session (best-effort); assuming it is not there",
            );
            socket_tmux
        }
    }
}

/// Read the detached coordinator pane's real pid for the engine's trust
/// root. A Ghostty surface attached to tmux is only a client and must not be
/// recorded here.
pub(crate) async fn pane_pid(tmux: &Tmux, record: &CoordinatorTmuxRecord) -> Result<libc::pid_t> {
    let raw = tmux
        .display_message(&record.session_name, DisplayField::PanePid)
        .await?;
    let pid = raw
        .parse::<libc::pid_t>()
        .with_context(|| format!("parsing coordinator pane pid {raw:?}"))?;
    if pid <= 0 {
        bail!("coordinator pane pid is not positive: {pid}");
    }
    Ok(pid)
}

async fn confirm_existing_intent(work_db: &WorkDb, tmux: &Tmux, record: &CoordinatorTmuxRecord) -> Result<()> {
    tmux.set_option(&record.session_name, SPAWN_TOKEN_OPTION, &record.spawn_token)
        .await
        .context("repairing coordinator token mirror after interrupted creation")?;
    tmux.set_option(&record.session_name, "remain-on-exit", "on")
        .await
        .context("repairing coordinator remain-on-exit option")?;
    if !work_db.record_coordinator_tmux_session_created(&record.spawn_token)? {
        bail!("coordinator intent changed while repairing its creation record");
    }
    Ok(())
}

/// Content hash of the rendered prompt the coordinator actually reads.
/// Deliberately not mtime/size-based — see [`RENDERED_PROMPT_FILENAME`].
///
/// The hash is only meaningful once the app has prepared the session
/// directory for this launch (rewritten `CLAUDE.md` into
/// `working_directory`). Engine attach hashes this file after app
/// registration; coordinator creation already depends on that same
/// ordering.
fn hash_rendered_prompt(working_directory: &Path) -> Result<String> {
    let path = working_directory.join(RENDERED_PROMPT_FILENAME);
    let contents =
        std::fs::read(&path).with_context(|| format!("reading rendered coordinator prompt at {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&contents);
    Ok(format!("{:x}", hasher.finalize()))
}

/// A coordinator session just created by [`start_new`] already has the
/// current prompt, so its nudge baseline is seeded to match — otherwise
/// the very next adoption of this same session would spuriously nudge it
/// to re-read the instructions it just started with. Best-effort: a
/// failure here costs one avoidable nudge later, never a wrong or lost
/// one, so it must never fail session creation.
fn seed_prompt_nudge_baseline(work_db: &WorkDb, working_directory: &Path) {
    match hash_rendered_prompt(working_directory) {
        Ok(hash) => {
            if let Err(error) = work_db.set_metadata(PROMPT_NUDGE_HASH_KEY, &hash) {
                tracing::warn!(
                    error = %format!("{error:#}"),
                    "coordinator prompt nudge: failed to seed baseline hash for freshly created session"
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                error = %format!("{error:#}"),
                "coordinator prompt nudge: failed to hash rendered prompt while seeding baseline"
            );
        }
    }
}

/// Nudge an *adopted* coordinator session to re-read its instructions when
/// the rendered prompt has changed since the last successfully sent
/// nudge. Best-effort and non-fatal by design: this must never fail the
/// attach flow adoption is part of. A send failure is recorded via
/// `tracing` and the engine-audit log, and the stored hash is left
/// unchanged so the next restart's adoption retries rather than silently
/// dropping the change. `send_keys` succeeding only proves tmux accepted
/// the bytes into the pane pty — not that the coordinator treated them as
/// a pending prompt — so audit/log outcomes use `"sent"`, not `"delivered"`.
async fn maybe_nudge_prompt_change(
    work_db: &WorkDb,
    tmux: &Tmux,
    record: &CoordinatorTmuxRecord,
    working_directory: &Path,
) {
    let current_hash = match hash_rendered_prompt(working_directory) {
        Ok(hash) => hash,
        Err(error) => {
            let error = format!("{error:#}");
            tracing::error!(
                error = %error,
                "coordinator prompt nudge: could not hash rendered prompt; skipping"
            );
            audit::record_event(
                "coordinator_prompt_nudge",
                &json!({
                    "outcome": "skipped",
                    "reason": "hash_unavailable",
                    "error": error,
                }),
            );
            return;
        }
    };
    let last_nudged_hash = match work_db.get_metadata(PROMPT_NUDGE_HASH_KEY) {
        Ok(value) => value,
        Err(error) => {
            let error = format!("{error:#}");
            tracing::warn!(
                error = %error,
                "coordinator prompt nudge: could not read last-nudged hash; skipping"
            );
            audit::record_event(
                "coordinator_prompt_nudge",
                &json!({
                    "outcome": "skipped",
                    "reason": "metadata_unavailable",
                    "error": error,
                }),
            );
            return;
        }
    };
    let Some(last_nudged_hash) = last_nudged_hash else {
        // No recorded baseline: a live session from before this feature
        // (or a failed seed) gives no evidence the prompt changed, so
        // establish the current hash and do not send.
        if let Err(error) = work_db.set_metadata(PROMPT_NUDGE_HASH_KEY, &current_hash) {
            tracing::warn!(
                error = %format!("{error:#}"),
                "coordinator prompt nudge: failed to seed baseline hash for session with no recorded baseline"
            );
        }
        return;
    };
    if last_nudged_hash == current_hash {
        return;
    }

    let prompt_path = working_directory.join(RENDERED_PROMPT_FILENAME);
    let message = format!(
        "Your coordinator instructions changed since this session started. Re-read {} now and follow the updated instructions.",
        prompt_path.display()
    );
    match tmux.send_keys(&record.session_name, &message).await {
        Ok(()) => {
            if let Err(error) = work_db.set_metadata(PROMPT_NUDGE_HASH_KEY, &current_hash) {
                // Sent but couldn't persist: leave this to the next
                // restart's comparison rather than risk under-reporting a
                // sent nudge as a failed one.
                let error = format!("{error:#}");
                tracing::error!(
                    error = %error,
                    "coordinator prompt nudge: sent but failed to persist the new hash; will re-nudge next restart"
                );
                audit::record_event(
                    "coordinator_prompt_nudge",
                    &json!({
                        "outcome": "sent_unpersisted",
                        "session_name": record.session_name,
                        "error": error,
                    }),
                );
                return;
            }
            audit::record_event(
                "coordinator_prompt_nudge",
                &json!({"outcome": "sent", "session_name": record.session_name}),
            );
            tracing::info!(
                session_name = %record.session_name,
                "coordinator prompt nudge sent: instructions changed since session start"
            );
        }
        Err(error) => {
            audit::record_event(
                "coordinator_prompt_nudge",
                &json!({
                    "outcome": "failed",
                    "session_name": record.session_name,
                    "error": format!("{error:#}"),
                }),
            );
            tracing::error!(
                error = %format!("{error:#}"),
                session_name = %record.session_name,
                "coordinator prompt nudge: send failed; stored hash left unchanged so the next restart retries"
            );
        }
    }
}

async fn start_new(
    work_db: &WorkDb,
    tmux: &Tmux,
    model: &str,
    working_directory: &Path,
    version_probe: &dyn ClaudeVersionProbe,
) -> Result<CoordinatorTmuxRecord> {
    let model = model.trim();
    if model.is_empty() {
        bail!("coordinator model may not be empty");
    }
    if !working_directory.is_dir() {
        bail!(
            "coordinator session directory is not prepared: {}",
            working_directory.display()
        );
    }
    let spawn_token = generate_token();
    let claude_version = version_probe.probe().await;
    work_db.record_coordinator_tmux_spawn_intent(
        COORDINATOR_SESSION_NAME,
        &spawn_token,
        model,
        claude_version.as_deref(),
    )?;

    let mut environment = BTreeMap::from([
        (SPAWN_TOKEN_ENV.to_owned(), spawn_token.clone()),
        (SESSION_SCHEMA_ENV.to_owned(), TMUX_SESSION_SCHEMA.to_owned()),
    ]);
    if let Ok(bin_dir) = std::env::var("BOSS_BIN_DIR")
        && !bin_dir.is_empty()
    {
        environment.insert("BOSS_BIN_DIR".to_owned(), bin_dir.clone());
        environment.insert("BOSS_BIN".to_owned(), format!("{bin_dir}/boss"));
    }
    let quoted_model = boss_ssh_transport::shell_quote(model);
    let command = format!(
        "{}unset ANTHROPIC_API_KEY; exec claude --model {quoted_model} --permission-mode auto",
        crate::runner::pane_spawn::path_prepend_clause("BOSS_BIN_DIR")
    );
    tmux.new_session(&NewSession {
        name: COORDINATOR_SESSION_NAME.to_owned(),
        environment,
        working_directory: working_directory.to_path_buf(),
        command,
    })
    .await
    .context("creating detached coordinator tmux session")?;
    crate::tmux_session_options::apply(tmux, COORDINATOR_SESSION_NAME)
        .await
        .context("applying Boss coordinator tmux session options")?;
    tmux.set_option(COORDINATOR_SESSION_NAME, SPAWN_TOKEN_OPTION, &spawn_token)
        .await
        .context("mirroring coordinator spawn token in tmux")?;
    tmux.set_option(COORDINATOR_SESSION_NAME, "remain-on-exit", "on")
        .await
        .context("preserving coordinator exit state for engine-side restart")?;
    if !work_db.record_coordinator_tmux_session_created(&spawn_token)? {
        bail!("coordinator session was created but its metadata intent was replaced");
    }
    seed_prompt_nudge_baseline(work_db, working_directory);
    Ok(CoordinatorTmuxRecord {
        session_name: COORDINATOR_SESSION_NAME.to_owned(),
        spawn_token,
        spawn_state: "created".to_owned(),
        model: model.to_owned(),
        launched_claude_version: claude_version,
    })
}

/// Parse `claude --version` stdout, e.g. `"2.1.237 (Claude Code)\n"` — the
/// first whitespace-separated token, when it is a dotted run of digits.
/// Mirrors the soft-parse shape `conformance::version_pin::parse_codex_version`
/// uses for `codex` (which instead takes the *last* token; the two CLIs put
/// the version in different positions).
fn parse_claude_version(stdout: &str) -> Option<String> {
    let line = stdout.lines().next()?.trim();
    let version = line.split_whitespace().next()?;
    let looks_like_semver = !version.is_empty()
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
    looks_like_semver.then(|| version.to_owned())
}

/// Bound on how long the version probe may block a tokio worker thread
/// before giving up. Both call sites (`start_new`, the attach path) are
/// async, so an un-timed `claude` invocation — a hung wrapper script, a
/// stalled filesystem, a node startup that never returns — would otherwise
/// park a worker indefinitely.
const CLAUDE_VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Probe the `claude` binary on `PATH` for its version. Best-effort and
/// silent on any failure (binary absent, non-zero exit, unparseable output,
/// or timeout) — a coordinator session must never fail to launch because
/// this probe did, and a failed probe must read as "can't tell", never "no
/// update". Uses `tokio::process::Command` (not `std::process::Command`)
/// because both call sites run on the async attach/creation path.
async fn probe_installed_claude_version() -> Option<String> {
    let output = tokio::time::timeout(
        CLAUDE_VERSION_PROBE_TIMEOUT,
        tokio::process::Command::new("claude").arg("--version").output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_claude_version(&String::from_utf8_lossy(&output.stdout))
}

/// Compare two `claude --version` strings as dotted numeric tuples (e.g.
/// `"2.1.237"`). `true` only when both parse in that shape and `installed`
/// is strictly greater component-wise — an unparseable, equal, or older
/// version returns `false` rather than guessing which way a mismatched
/// shape should be read.
fn is_strictly_newer(installed: &str, running: &str) -> bool {
    fn parts(v: &str) -> Option<Vec<u64>> {
        v.split('.').map(|p| p.parse::<u64>().ok()).collect()
    }
    match (parts(installed), parts(running)) {
        (Some(installed), Some(running)) => installed > running,
        _ => false,
    }
}

/// The installed `claude` version, but only when it is confidently newer
/// than the one this coordinator session actually launched with. `None`
/// covers "no update available", "can't tell" (no recorded launch version,
/// probe failed, either version unparseable) and a downgrade alike — the
/// caller must never distinguish those and must never render an "up to
/// date" state from this. Deliberately pure — `installed` is resolved by
/// the caller (see `probe_installed_claude_version`/[`ClaudeVersionProbe`]),
/// so this is directly testable without spawning a real process.
pub(crate) fn coordinator_update_available(record: &CoordinatorTmuxRecord, installed: Option<&str>) -> Option<String> {
    let running = record.launched_claude_version.as_deref()?;
    let installed = installed?;
    is_strictly_newer(installed, running).then_some(installed.to_owned())
}

/// Resolve the production coordinator session directory under Application
/// Support. Callers pass the result into lifecycle helpers so tests can
/// inject a prepared temporary directory instead.
pub(crate) fn coordinator_working_directory() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is required for the coordinator session"))?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("Boss")
        .join("boss-session"))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use boss_tmux::{CommandOutput, CommandRunner};
    use serde_json::Value;

    use super::*;

    #[test]
    fn consecutive_failures_trip_the_restart_ceiling() {
        let mut failures = CoordinatorRestartFailures::new();
        for i in 0..CoordinatorRestartFailures::LIMIT - 1 {
            let decision = failures.record_failure();
            assert!(
                !decision.notify,
                "failure {} of {} must not notify",
                i + 1,
                CoordinatorRestartFailures::LIMIT
            );
            assert_eq!(decision.delay, CoordinatorRestartFailures::BACKOFF_CAP);
        }
        let decision = failures.record_failure();
        assert!(
            decision.notify,
            "the {limit}th consecutive failure must trip the ceiling",
            limit = CoordinatorRestartFailures::LIMIT
        );
        assert_eq!(decision.delay, CoordinatorRestartFailures::BACKOFF_CAP);
        assert_eq!(failures.consecutive_failures(), CoordinatorRestartFailures::LIMIT);
    }

    #[test]
    fn successful_restarts_do_not_trip_the_restart_failure_ceiling() {
        let mut failures = CoordinatorRestartFailures::new();
        for _ in 0..CoordinatorRestartFailures::LIMIT + 3 {
            failures.record_success();
            assert_eq!(failures.consecutive_failures(), 0);
        }
    }

    #[test]
    fn a_success_resets_consecutive_failures() {
        let mut failures = CoordinatorRestartFailures::new();
        for _ in 0..CoordinatorRestartFailures::LIMIT - 1 {
            assert!(!failures.record_failure().notify);
        }
        failures.record_success();
        for _ in 0..CoordinatorRestartFailures::LIMIT - 1 {
            assert!(!failures.record_failure().notify);
        }
        let decision = failures.record_failure();
        assert!(decision.notify);
    }

    #[test]
    fn ceiling_keeps_retrying_at_the_flat_backoff_without_renotifying() {
        let mut failures = CoordinatorRestartFailures::new();
        for _ in 0..CoordinatorRestartFailures::LIMIT {
            failures.record_failure();
        }
        let again = failures.record_failure();
        assert_eq!(again.delay, CoordinatorRestartFailures::BACKOFF_CAP);
        assert!(!again.notify, "still at the ceiling: do not raise a second attention");
        failures.record_success();
        assert_eq!(failures.consecutive_failures(), 0);
    }

    #[test]
    fn consecutive_restarts_escalate_and_trip_their_own_ceiling() {
        let mut failures = CoordinatorRestartFailures::new();
        let expected_delays = [2, 4, 8, 16, 32];
        for (i, &expected_secs) in expected_delays.iter().enumerate() {
            let decision = failures.record_restart();
            assert_eq!(decision.delay, Duration::from_secs(expected_secs));
            assert_eq!(
                decision.notify,
                i + 1 >= CoordinatorRestartFailures::LIMIT as usize,
                "restart {} of {}",
                i + 1,
                CoordinatorRestartFailures::LIMIT
            );
        }
        let further = failures.record_restart();
        assert_eq!(further.delay, CoordinatorRestartFailures::BACKOFF_CAP);
        assert!(
            !further.notify,
            "still at the churn ceiling: do not raise a second attention"
        );
        assert_eq!(failures.restart_churn(), 6);
    }

    #[test]
    fn a_healthy_pass_resets_restart_churn_but_a_restart_does_not() {
        let mut failures = CoordinatorRestartFailures::new();
        failures.record_restart();
        failures.record_restart();
        assert_eq!(failures.restart_churn(), 2);
        failures.record_success();
        assert_eq!(failures.restart_churn(), 0);
    }

    // --- claude version probe / comparison ---

    #[test]
    fn parses_claude_version_line() {
        assert_eq!(
            parse_claude_version("2.1.237 (Claude Code)\n"),
            Some("2.1.237".to_owned())
        );
    }

    #[test]
    fn rejects_unparseable_version_output() {
        assert_eq!(parse_claude_version(""), None);
        assert_eq!(parse_claude_version("\n"), None);
        assert_eq!(parse_claude_version("command not found\n"), None);
        assert_eq!(
            parse_claude_version("v2.1.237\n"),
            None,
            "leading 'v' is not a bare digit run"
        );
    }

    #[test]
    fn detects_strictly_newer_installed_version() {
        assert!(is_strictly_newer("2.1.238", "2.1.237"));
        assert!(is_strictly_newer("2.2.0", "2.1.237"));
        assert!(is_strictly_newer("3.0.0", "2.9.999"));
    }

    #[test]
    fn does_not_report_newer_for_equal_older_or_unparseable_versions() {
        assert!(!is_strictly_newer("2.1.237", "2.1.237"), "equal versions are not newer");
        assert!(
            !is_strictly_newer("2.1.0", "2.1.237"),
            "an older installed version is not newer"
        );
        assert!(
            !is_strictly_newer("2.1.237", "not-a-version"),
            "an unparseable running version can't be compared"
        );
        assert!(
            !is_strictly_newer("not-a-version", "2.1.237"),
            "an unparseable installed version can't be compared"
        );
    }

    fn record_with_launched_version(launched: Option<&str>) -> CoordinatorTmuxRecord {
        CoordinatorTmuxRecord {
            session_name: COORDINATOR_SESSION_NAME.to_owned(),
            spawn_token: "token".to_owned(),
            spawn_state: "created".to_owned(),
            model: "opus".to_owned(),
            launched_claude_version: launched.map(str::to_owned),
        }
    }

    #[test]
    fn coordinator_update_available_reports_a_confidently_newer_installed_version() {
        let record = record_with_launched_version(Some("2.1.237"));
        assert_eq!(
            coordinator_update_available(&record, Some("2.1.238")),
            Some("2.1.238".to_owned())
        );
    }

    #[test]
    fn coordinator_update_available_is_none_without_evidence() {
        let up_to_date = record_with_launched_version(Some("2.1.237"));
        assert_eq!(coordinator_update_available(&up_to_date, Some("2.1.237")), None);
        assert_eq!(coordinator_update_available(&up_to_date, None), None, "probe failed");

        let no_recorded_launch = record_with_launched_version(None);
        assert_eq!(
            coordinator_update_available(&no_recorded_launch, Some("2.1.238")),
            None,
            "no recorded launch version means no baseline to compare against"
        );
    }

    /// Test double for [`ClaudeVersionProbe`] that never spawns a real
    /// process, keeping every test in this module hermetic.
    struct NoneProbe;

    #[async_trait::async_trait]
    impl ClaudeVersionProbe for NoneProbe {
        async fn probe(&self) -> Option<String> {
            None
        }
    }

    struct FixedProbe(&'static str);

    #[async_trait::async_trait]
    impl ClaudeVersionProbe for FixedProbe {
        async fn probe(&self) -> Option<String> {
            Some(self.0.to_owned())
        }
    }

    struct FakeTmux {
        sessions: Vec<String>,
        token: Option<String>,
        pane_dead: String,
        send_keys_fails: bool,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeTmux {
        fn new(sessions: Vec<&str>, token: Option<&str>, pane_dead: &str) -> Self {
            Self::with_send_keys_outcome(sessions, token, pane_dead, false)
        }

        fn with_send_keys_outcome(
            sessions: Vec<&str>,
            token: Option<&str>,
            pane_dead: &str,
            send_keys_fails: bool,
        ) -> Self {
            Self {
                sessions: sessions.into_iter().map(str::to_owned).collect(),
                token: token.map(str::to_owned),
                pane_dead: pane_dead.to_owned(),
                send_keys_fails,
                calls: Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl CommandRunner for FakeTmux {
        async fn run(&self, _: &Path, args: &[OsString], cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
            assert!(cwd.is_none());
            let args: Vec<String> = args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
            self.calls.lock().unwrap().push(args.clone());
            let (success, stdout, stderr) = match args.get(2).map(String::as_str) {
                Some("list-sessions") => (
                    true,
                    self.sessions.iter().map(|name| format!("{name}\t\n")).collect(),
                    String::new(),
                ),
                Some("show-environment") => match &self.token {
                    Some(token) => (true, format!("BOSS_SPAWN_TOKEN={token}\n"), String::new()),
                    None => (false, String::new(), "unknown variable".to_owned()),
                },
                Some("display-message") => (true, format!("{}\n", self.pane_dead), String::new()),
                Some("new-session") | Some("set-option") | Some("kill-session") => (true, String::new(), String::new()),
                Some("send-keys") if self.send_keys_fails => (
                    false,
                    String::new(),
                    "tmux server unreachable (fake failure)".to_owned(),
                ),
                Some("send-keys") => (true, String::new(), String::new()),
                other => panic!("unexpected tmux command: {other:?}, args={args:?}"),
            };
            Ok(CommandOutput {
                success,
                code: Some(if success { 0 } else { 1 }),
                stdout,
                stderr,
            })
        }
    }

    fn fixture(server: FakeTmux) -> (WorkDb, Tmux, Arc<FakeTmux>, tempfile::TempDir) {
        let server = Arc::new(server);
        let tmux = Tmux::with_runner_and_socket("/usr/bin/tmux", server.clone(), boss_tmux::TEST_SOCKET_PATH).unwrap();
        let dir = tempfile::tempdir().unwrap();
        (WorkDb::open(PathBuf::from(":memory:")).unwrap(), tmux, server, dir)
    }

    fn tmux_for(server: FakeTmux) -> (Tmux, Arc<FakeTmux>) {
        let server = Arc::new(server);
        let tmux = Tmux::with_runner_and_socket("/usr/bin/tmux", server.clone(), boss_tmux::TEST_SOCKET_PATH).unwrap();
        (tmux, server)
    }

    fn legacy_tmux_for(server: FakeTmux) -> (Tmux, Arc<FakeTmux>) {
        let server = Arc::new(server);
        let tmux = Tmux::for_legacy_label_server_with_runner("/usr/bin/tmux", server.clone()).unwrap();
        (tmux, server)
    }

    fn send_keys_calls(calls: &[Vec<String>]) -> Vec<&Vec<String>> {
        calls
            .iter()
            .filter(|call| call.get(2).map(String::as_str) == Some("send-keys") && call.iter().any(|a| a == "-l"))
            .collect()
    }

    fn is_server_option(call: &[String], option: &str, value: &str) -> bool {
        call.get(2).map(String::as_str) == Some("set-option")
            && call.get(3).map(String::as_str) == Some("-s")
            && call.get(4).map(String::as_str) == Some(option)
            && call.get(5).map(String::as_str) == Some(value)
    }

    /// Boss-owned server options must be present in the recorded argv
    /// by the time the caller returns an attach identity.
    fn assert_extended_keys_applied(calls: &[Vec<String>]) {
        assert!(
            calls
                .iter()
                .any(|call| is_server_option(call, "terminal-features[100]", "xterm*:extkeys")),
            "expected set-option -s terminal-features[100] xterm*:extkeys, got {calls:?}"
        );
        assert!(
            calls.iter().any(|call| is_server_option(call, "extended-keys", "on")),
            "expected set-option -s extended-keys on, got {calls:?}"
        );
        assert!(
            calls.iter().any(|call| is_server_option(call, "focus-events", "on")),
            "expected set-option -s focus-events on, got {calls:?}"
        );
    }

    // --- legacy-server routing (`resolve_active_handle`) ---

    #[tokio::test]
    async fn resolve_active_handle_prefers_socket_when_no_legacy_coordinator_survives() {
        let (socket_tmux, _socket_server) = tmux_for(FakeTmux::new(vec![], None, "0"));
        let (legacy_tmux, _legacy_server) = legacy_tmux_for(FakeTmux::new(vec![], None, "0"));

        let resolved = resolve_active_handle(&socket_tmux, Some(&legacy_tmux)).await;
        assert_eq!(resolved.operator_prefix(), socket_tmux.operator_prefix());
    }

    #[tokio::test]
    async fn resolve_active_handle_routes_to_the_legacy_server_when_a_coordinator_survives_there() {
        let (socket_tmux, socket_server) = tmux_for(FakeTmux::new(vec![], None, "0"));
        let (legacy_tmux, _legacy_server) =
            legacy_tmux_for(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("legacy-token"), "0"));

        let resolved = resolve_active_handle(&socket_tmux, Some(&legacy_tmux)).await;
        assert_eq!(
            resolved.operator_prefix(),
            format!("tmux -L {}", boss_tmux::SERVER_LABEL),
            "a coordinator surviving on -L boss must be routed to instead of the durable socket",
        );
        assert!(
            socket_server.calls().is_empty(),
            "resolving which handle to use must never itself probe the socket server",
        );
    }

    #[tokio::test]
    async fn resolve_active_handle_falls_back_to_socket_with_no_legacy_handle() {
        let (socket_tmux, _socket_server) = tmux_for(FakeTmux::new(vec![], None, "0"));
        let resolved = resolve_active_handle(&socket_tmux, None).await;
        assert_eq!(resolved.operator_prefix(), socket_tmux.operator_prefix());
    }

    /// A live coordinator surviving on the legacy server must be recovered
    /// in place — options reapplied against `-L boss` — never shadowed by a
    /// second coordinator spawned fresh on the durable socket.
    #[tokio::test]
    async fn ensure_for_attach_recovers_a_legacy_coordinator_without_spawning_a_second_one() {
        let (db, socket_tmux, socket_server, dir) = fixture(FakeTmux::new(vec![], None, "0"));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "legacy-token", "opus", None)
            .unwrap();
        db.record_coordinator_tmux_session_created("legacy-token").unwrap();
        let (legacy_tmux, legacy_server) =
            legacy_tmux_for(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("legacy-token"), "0"));

        let active_tmux = resolve_active_handle(&socket_tmux, Some(&legacy_tmux)).await;
        let record = ensure_for_attach(&db, active_tmux, &socket_tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();

        assert_eq!(record.spawn_state, "created");
        assert!(
            socket_server
                .calls()
                .iter()
                .all(|call| call.get(2).map(String::as_str) != Some("new-session")),
            "the durable socket must never see a new-session call while the legacy coordinator is live",
        );
        assert!(
            legacy_server
                .calls()
                .iter()
                .any(|call| call.get(2).map(String::as_str) == Some("set-option")),
            "the legacy session's options must be reapplied in place",
        );
    }

    #[tokio::test]
    async fn ensure_without_record_writes_intent_before_new_session_and_mirrors_options() {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![], None, "0"));
        let record = ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();
        assert_eq!(record.spawn_state, "created");
        let calls = server.calls();
        assert_eq!(calls[0][2], "new-session");
        assert!(
            calls[0]
                .windows(2)
                .any(|pair| pair[0] == "-e" && pair[1].starts_with("BOSS_SPAWN_TOKEN="))
        );
        assert!(
            calls[0]
                .windows(2)
                .any(|pair| pair[0] == "-c" && Path::new(&pair[1]) == dir.path())
        );
        assert!(is_server_option(&calls[1], "terminal-features[100]", "xterm*:extkeys"));
        assert!(is_server_option(&calls[2], "extended-keys", "on"));
        assert!(is_server_option(&calls[3], "focus-events", "on"));
        assert_eq!(calls[4][2], "set-option");
        assert_eq!(calls[4][5], "status");
        assert_eq!(calls[4][6], "off");
        assert_eq!(calls[5][5], "@boss_spawn_token");
        assert_eq!(calls[6][5], "remain-on-exit");
        assert_extended_keys_applied(&calls);
    }

    #[tokio::test]
    async fn new_session_records_the_injected_claude_version_without_a_real_probe() {
        let (db, tmux, _server, dir) = fixture(FakeTmux::new(vec![], None, "0"));
        let record = ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &FixedProbe("2.1.237"))
            .await
            .unwrap();
        assert_eq!(record.launched_claude_version.as_deref(), Some("2.1.237"));
    }

    #[tokio::test]
    async fn adopted_pre_version_tracking_session_seeds_once_without_overwriting() {
        let (db, tmux, _server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();
        db.connect()
            .unwrap()
            .execute(
                "DELETE FROM metadata WHERE key = ?1",
                ["coordinator.tmux_claude_version"],
            )
            .unwrap();

        let adopted = ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &FixedProbe("2.1.238"))
            .await
            .unwrap();
        assert_eq!(adopted.launched_claude_version.as_deref(), Some("2.1.238"));
        assert_eq!(
            db.coordinator_tmux_record()
                .unwrap()
                .unwrap()
                .launched_claude_version
                .as_deref(),
            Some("2.1.238"),
            "adoption persists its current-version baseline"
        );

        let later_adopt = ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &FixedProbe("2.1.239"))
            .await
            .unwrap();
        assert_eq!(
            later_adopt.launched_claude_version.as_deref(),
            Some("2.1.238"),
            "a later adoption must retain the original baseline"
        );
    }

    #[tokio::test]
    async fn adopted_session_with_failed_probe_leaves_baseline_missing_for_retry() {
        let (db, tmux, _server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();
        db.connect()
            .unwrap()
            .execute(
                "DELETE FROM metadata WHERE key = ?1",
                ["coordinator.tmux_claude_version"],
            )
            .unwrap();

        let adopted = ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();
        assert_eq!(
            adopted.launched_claude_version, None,
            "a failed probe must not fabricate an empty baseline"
        );
        assert!(
            db.coordinator_tmux_claude_version_is_missing().unwrap(),
            "the baseline must still read as missing after a failed probe"
        );

        let retried = ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &FixedProbe("2.1.238"))
            .await
            .unwrap();
        assert_eq!(
            retried.launched_claude_version.as_deref(),
            Some("2.1.238"),
            "the next adoption must retry seeding successfully"
        );
    }

    #[tokio::test]
    async fn intended_live_session_repairs_its_tmux_mirrors() {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
            .unwrap();
        let record = ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();
        assert_eq!(record.spawn_state, "created");
        let calls = server.calls();
        assert!(
            calls
                .iter()
                .any(|call| call.get(5).map(String::as_str) == Some("status")
                    && call.get(6).map(String::as_str) == Some("off"))
        );
        assert!(
            calls
                .iter()
                .any(|call| call.get(2).map(String::as_str) == Some("set-option")
                    && call.get(5).map(String::as_str) == Some("@boss_spawn_token"))
        );
        assert!(
            calls
                .iter()
                .any(|call| call.get(5).map(String::as_str) == Some("remain-on-exit"))
        );
    }

    #[tokio::test]
    async fn existing_live_session_reapplies_boss_presentation_options() {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();

        ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();

        let calls = server.calls();
        assert!(calls.iter().any(|call| {
            call.get(2).map(String::as_str) == Some("set-option")
                && call.get(5).map(String::as_str) == Some("status")
                && call.get(6).map(String::as_str) == Some("off")
        }));
        assert_extended_keys_applied(&calls);
    }

    #[tokio::test]
    async fn restart_if_dead_on_a_live_session_reapplies_extended_keys() {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();

        let replacement = restart_if_dead(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();
        assert!(replacement.is_none(), "a live session must not force reattach");
        assert_extended_keys_applied(&server.calls());
    }

    #[tokio::test]
    async fn dead_matching_session_is_killed_before_recreation() {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "1"));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();
        ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();
        let calls = server.calls();
        let kill = calls
            .iter()
            .position(|call| call.get(2).map(String::as_str) == Some("kill-session"))
            .unwrap();
        let new = calls
            .iter()
            .position(|call| call.get(2).map(String::as_str) == Some("new-session"))
            .unwrap();
        assert!(kill < new);
    }

    #[tokio::test]
    async fn mismatched_live_token_errors_without_killing() {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("other"), "0"));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();
        assert!(
            ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
                .await
                .is_err()
        );
        assert!(
            !server
                .calls()
                .iter()
                .any(|call| call.get(2).map(String::as_str) == Some("kill-session"))
        );
    }

    #[tokio::test]
    async fn live_matching_token_session_is_preserved_without_kill() {
        // A healthy matching-token session is never recreated, regardless
        // of the requested model (model replacement is confirmation-gated).
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();

        let record = ensure_for_attach(&db, &tmux, &tmux, "sonnet", dir.path(), &NoneProbe)
            .await
            .unwrap();

        assert_eq!(record.model, "opus");
        assert!(
            !server
                .calls()
                .iter()
                .any(|call| call.get(2).map(String::as_str) == Some("kill-session")
                    || call.get(2).map(String::as_str) == Some("new-session"))
        );
    }

    #[tokio::test]
    async fn unprepared_working_directory_bails_before_new_session() {
        let (db, tmux, server, _dir) = fixture(FakeTmux::new(vec![], None, "0"));
        let missing = PathBuf::from("/tmp/boss-coordinator-session-does-not-exist");
        let err = ensure_for_attach(&db, &tmux, &tmux, "opus", &missing, &NoneProbe)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not prepared"), "unexpected error: {err:#}");
        assert!(
            server.calls().is_empty(),
            "tmux must not be invoked when the session directory is missing"
        );
    }

    #[tokio::test]
    async fn stale_confirmation_does_not_kill_current_session() {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();
        assert!(
            recreate_after_confirmation(
                &db,
                &tmux,
                &tmux,
                "sonnet",
                "stale",
                dir.path(),
                CoordinatorRecreateReason::OperatorReset,
                &NoneProbe,
            )
            .await
            .is_err()
        );
        assert!(
            !server
                .calls()
                .iter()
                .any(|call| call.get(2).map(String::as_str) == Some("kill-session"))
        );
    }

    // --- operator-confirmed reset (`recreate_after_confirmation`) ---

    /// Drop guard that clears `AUDIT_PATH_ENV` even if the test body panics
    /// on an assertion, so a failure here can't leak the env override into
    /// every later test in this binary.
    struct AuditPathEnvGuard;

    impl Drop for AuditPathEnvGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var(audit::AUDIT_PATH_ENV);
            }
        }
    }

    // The default `#[tokio::test]` flavor is single-threaded (`current_thread`),
    // so holding this std `Mutex` across the `recreate_after_confirmation` await
    // below cannot deadlock or block a sibling worker thread here; it is exactly
    // what serializes this test against the other `AUDIT_PATH_ENV`/`AUDIT_PATH`
    // mutators in this binary for the whole operation, not just the `set_var`
    // call, which is the point (see `audit::lock_audit_globals_for_tests`'s docs).
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn operator_reset_kills_old_session_and_creates_a_fresh_one() {
        let _audit_globals = audit::lock_audit_globals_for_tests();

        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();

        let audit_dir = tempfile::tempdir().unwrap();
        let audit_path = audit_dir.path().join("engine-audit.log");
        let audit_already_resolved = audit::path_already_resolved_for_tests();
        let _env_guard = if audit_already_resolved {
            None
        } else {
            unsafe {
                std::env::set_var(audit::AUDIT_PATH_ENV, &audit_path);
            }
            Some(AuditPathEnvGuard)
        };

        let record = recreate_after_confirmation(
            &db,
            &tmux,
            &tmux,
            "opus",
            "token",
            dir.path(),
            CoordinatorRecreateReason::OperatorReset,
            &FixedProbe("2.1.238"),
        )
        .await
        .unwrap();

        assert_ne!(record.spawn_token, "token", "the replacement must mint a fresh token");
        assert_eq!(record.spawn_state, "created");
        assert_eq!(record.launched_claude_version.as_deref(), Some("2.1.238"));

        let calls = server.calls();
        let kill = calls
            .iter()
            .position(|call| {
                call.get(2).map(String::as_str) == Some("kill-session")
                    && call.iter().any(|a| a == COORDINATOR_SESSION_NAME)
            })
            .expect("expected a kill-session for the coordinator");
        let new = calls
            .iter()
            .position(|call| call.get(2).map(String::as_str) == Some("new-session"))
            .expect("expected a new-session for the replacement");
        assert!(
            kill < new,
            "the old session must be killed before the replacement is created"
        );

        if !audit_already_resolved {
            let contents = std::fs::read_to_string(&audit_path).unwrap_or_default();
            let last = contents
                .lines()
                .last()
                .and_then(|line| serde_json::from_str::<Value>(line).ok())
                .expect("expected a coordinator_recreate audit record");
            assert_eq!(last["event"], "coordinator_recreate");
            assert_eq!(last["outcome"], "success");
            assert_eq!(last["reason"], "operator_reset");
        }
    }

    #[tokio::test]
    async fn operator_reset_new_session_carries_current_binary_and_environment() {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();

        let record = recreate_after_confirmation(
            &db,
            &tmux,
            &tmux,
            "sonnet",
            "token",
            dir.path(),
            CoordinatorRecreateReason::OperatorReset,
            &NoneProbe,
        )
        .await
        .unwrap();

        let calls = server.calls();
        let new_session = calls
            .iter()
            .find(|call| call.get(2).map(String::as_str) == Some("new-session"))
            .expect("expected a new-session call");
        assert!(
            new_session
                .windows(2)
                .any(|pair| pair[0] == "-e" && pair[1] == format!("{SPAWN_TOKEN_ENV}={}", record.spawn_token)),
            "the replacement must launch with its own fresh spawn token, got {new_session:?}"
        );
        assert!(
            new_session
                .windows(2)
                .any(|pair| pair[0] == "-e" && pair[1].starts_with(&format!("{SESSION_SCHEMA_ENV}="))),
            "the replacement must mirror the current session schema, got {new_session:?}"
        );
        assert!(
            new_session
                .iter()
                .any(|arg| arg.contains("exec claude --model") && arg.contains("sonnet")),
            "the replacement must launch the current claude binary with the requested model, got {new_session:?}"
        );
    }

    #[tokio::test]
    async fn operator_reset_never_touches_worker_sessions() {
        const WORKER_SESSION: &str = "boss-slot1-exec123";
        let (db, tmux, server, dir) = fixture(FakeTmux::new(
            vec![COORDINATOR_SESSION_NAME, WORKER_SESSION],
            Some("token"),
            "0",
        ));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();

        recreate_after_confirmation(
            &db,
            &tmux,
            &tmux,
            "opus",
            "token",
            dir.path(),
            CoordinatorRecreateReason::OperatorReset,
            &NoneProbe,
        )
        .await
        .unwrap();

        assert!(
            server.calls().iter().all(|call| {
                call.get(2).map(String::as_str) != Some("kill-session")
                    || call.iter().any(|a| a == COORDINATOR_SESSION_NAME)
            }),
            "no kill-session may name anything but the coordinator session"
        );
    }

    #[tokio::test]
    async fn operator_reset_leaves_no_orphaned_session_behind() {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();

        recreate_after_confirmation(
            &db,
            &tmux,
            &tmux,
            "opus",
            "token",
            dir.path(),
            CoordinatorRecreateReason::OperatorReset,
            &NoneProbe,
        )
        .await
        .unwrap();

        let calls = server.calls();
        let kills = calls
            .iter()
            .filter(|call| call.get(2).map(String::as_str) == Some("kill-session"))
            .count();
        let creates = calls
            .iter()
            .filter(|call| call.get(2).map(String::as_str) == Some("new-session"))
            .count();
        assert_eq!(kills, 1, "expected exactly one kill-session, got {kills}");
        assert_eq!(creates, 1, "expected exactly one new-session, got {creates}");
    }

    #[tokio::test]
    async fn operator_reset_surfaces_a_failure_to_create_the_replacement() {
        let (db, tmux, _server, _dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();

        let missing = PathBuf::from("/tmp/boss-coordinator-reset-does-not-exist");
        let err = recreate_after_confirmation(
            &db,
            &tmux,
            &tmux,
            "opus",
            "token",
            &missing,
            CoordinatorRecreateReason::OperatorReset,
            &NoneProbe,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not prepared"), "unexpected error: {err:#}");
    }

    // --- coordinator prompt re-read nudge ---

    #[tokio::test]
    async fn freshly_created_session_is_not_nudged() {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(RENDERED_PROMPT_FILENAME), "prompt v1").unwrap();

        let (tmux, server) = tmux_for(FakeTmux::new(vec![], None, "0"));
        ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();

        assert!(
            send_keys_calls(&server.calls()).is_empty(),
            "creation must never nudge the session it just created"
        );
        assert_eq!(
            db.get_metadata(PROMPT_NUDGE_HASH_KEY).unwrap(),
            Some(hash_rendered_prompt(dir.path()).unwrap()),
            "baseline hash must be seeded at creation so the next adoption doesn't spuriously nudge"
        );
    }

    #[tokio::test]
    async fn unchanged_prompt_across_restart_produces_no_nudge() {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(RENDERED_PROMPT_FILENAME), "prompt v1").unwrap();

        let (tmux, _server) = tmux_for(FakeTmux::new(vec![], None, "0"));
        let created = ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();

        // Simulate a restart that adopts the still-live session: same
        // prompt content, no new-session/kill-session activity this time.
        let (tmux, server) = tmux_for(FakeTmux::new(
            vec![COORDINATOR_SESSION_NAME],
            Some(&created.spawn_token),
            "0",
        ));
        ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();

        assert!(
            send_keys_calls(&server.calls()).is_empty(),
            "an unchanged prompt across a restart must not nudge"
        );
    }

    #[tokio::test]
    async fn absent_baseline_on_adoption_seeds_without_nudging() {
        // Pre-feature live session: metadata singleton exists, prompt
        // hash key does not. Adoption must establish a baseline, not
        // claim the prompt changed.
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(RENDERED_PROMPT_FILENAME), "prompt v1").unwrap();
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();
        assert_eq!(db.get_metadata(PROMPT_NUDGE_HASH_KEY).unwrap(), None);

        let (tmux, server) = tmux_for(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();

        assert!(
            send_keys_calls(&server.calls()).is_empty(),
            "a session with no recorded baseline must not be nudged"
        );
        assert_eq!(
            db.get_metadata(PROMPT_NUDGE_HASH_KEY).unwrap(),
            Some(hash_rendered_prompt(dir.path()).unwrap()),
            "adoption with no recorded baseline must persist the current hash"
        );
    }

    #[tokio::test]
    async fn prompt_change_produces_exactly_one_nudge() {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(RENDERED_PROMPT_FILENAME), "prompt v1").unwrap();

        let (tmux, _server) = tmux_for(FakeTmux::new(vec![], None, "0"));
        let created = ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();

        std::fs::write(dir.path().join(RENDERED_PROMPT_FILENAME), "prompt v2").unwrap();
        let (tmux, server) = tmux_for(FakeTmux::new(
            vec![COORDINATOR_SESSION_NAME],
            Some(&created.spawn_token),
            "0",
        ));
        ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();

        let calls = server.calls();
        let nudges = send_keys_calls(&calls);
        assert_eq!(
            nudges.len(),
            1,
            "a changed prompt must nudge exactly once, got {nudges:?}"
        );
        let message = nudges[0].last().unwrap();
        assert!(
            message.contains(&dir.path().join(RENDERED_PROMPT_FILENAME).display().to_string()),
            "nudge message must name the instructions path, got {message:?}"
        );
        assert_eq!(
            db.get_metadata(PROMPT_NUDGE_HASH_KEY).unwrap(),
            Some(hash_rendered_prompt(dir.path()).unwrap()),
        );
    }

    #[tokio::test]
    async fn second_restart_with_no_further_change_produces_no_additional_nudge() {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(RENDERED_PROMPT_FILENAME), "prompt v1").unwrap();

        let (tmux, _server) = tmux_for(FakeTmux::new(vec![], None, "0"));
        let created = ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();

        std::fs::write(dir.path().join(RENDERED_PROMPT_FILENAME), "prompt v2").unwrap();
        let (tmux, _server) = tmux_for(FakeTmux::new(
            vec![COORDINATOR_SESSION_NAME],
            Some(&created.spawn_token),
            "0",
        ));
        ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();

        // A second restart, prompt unchanged since the nudge above.
        let (tmux, server) = tmux_for(FakeTmux::new(
            vec![COORDINATOR_SESSION_NAME],
            Some(&created.spawn_token),
            "0",
        ));
        ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();

        assert!(
            send_keys_calls(&server.calls()).is_empty(),
            "no further prompt change must not produce an additional nudge"
        );
    }

    #[tokio::test]
    async fn failed_delivery_leaves_hash_unadvanced_for_retry() {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(RENDERED_PROMPT_FILENAME), "prompt v1").unwrap();

        let (tmux, _server) = tmux_for(FakeTmux::new(vec![], None, "0"));
        let created = ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();
        let hash_v1 = hash_rendered_prompt(dir.path()).unwrap();
        assert_eq!(db.get_metadata(PROMPT_NUDGE_HASH_KEY).unwrap(), Some(hash_v1.clone()));

        std::fs::write(dir.path().join(RENDERED_PROMPT_FILENAME), "prompt v2").unwrap();
        let (tmux, server) = tmux_for(FakeTmux::with_send_keys_outcome(
            vec![COORDINATOR_SESSION_NAME],
            Some(&created.spawn_token),
            "0",
            true,
        ));
        ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();
        assert!(
            !send_keys_calls(&server.calls()).is_empty(),
            "delivery must have been attempted"
        );
        assert_eq!(
            db.get_metadata(PROMPT_NUDGE_HASH_KEY).unwrap(),
            Some(hash_v1),
            "a failed delivery must leave the stored hash unadvanced"
        );

        // Next restart retries against the still-changed prompt, and this
        // time delivery succeeds.
        let (tmux, server) = tmux_for(FakeTmux::new(
            vec![COORDINATOR_SESSION_NAME],
            Some(&created.spawn_token),
            "0",
        ));
        ensure_for_attach(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe)
            .await
            .unwrap();
        assert_eq!(
            send_keys_calls(&server.calls()).len(),
            1,
            "the retried nudge must be delivered exactly once"
        );
        assert_eq!(
            db.get_metadata(PROMPT_NUDGE_HASH_KEY).unwrap(),
            Some(hash_rendered_prompt(dir.path()).unwrap()),
            "a successful retry must advance the stored hash"
        );
    }
}
