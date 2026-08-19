//! Engine-owned lifecycle for the durable coordinator tmux session.
//!
//! The coordinator is deliberately not a `work_runs` row: it has no slot,
//! workspace, or execution. Its durable pointer is instead the metadata
//! singleton managed by [`crate::work::WorkDb`]. The write ordering remains
//! identical to a worker session: commit `intended`, create with the token in
//! the atomic tmux environment, mirror/confirm, then mark `created`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
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

/// Create or recover the coordinator for an app that has just registered.
///
/// A model mismatch leaves the live conversation intact; the app compares the
/// returned model with its requested model before asking for replacement.
/// `working_directory` is the prepared Boss-session directory; callers
/// resolve it once via [`coordinator_working_directory`].
pub(crate) async fn ensure_for_attach(
    work_db: &WorkDb,
    tmux: &Tmux,
    requested_model: &str,
    working_directory: &Path,
) -> Result<CoordinatorTmuxRecord> {
    match work_db.coordinator_tmux_record()? {
        None => start_new(work_db, tmux, requested_model, working_directory).await,
        Some(record) => reconcile_existing(work_db, tmux, requested_model, record, working_directory).await,
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
    requested_model: &str,
    working_directory: &Path,
) -> Result<Option<CoordinatorTmuxRecord>> {
    let Some(record) = work_db.coordinator_tmux_record()? else {
        return Ok(None);
    };
    if !session_exists(tmux, &record.session_name).await? {
        return start_new(work_db, tmux, requested_model, working_directory)
            .await
            .map(Some);
    }
    let live_token = tmux.show_environment(&record.session_name, SPAWN_TOKEN_ENV).await?;
    match live_token {
        Some(token) if token == record.spawn_token => {}
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
    start_new(work_db, tmux, requested_model, working_directory)
        .await
        .map(Some)
}

/// Recreate a model-mismatched coordinator after an explicit UI confirmation.
/// The expected token prevents a delayed confirmation from killing a newer
/// session created by a concurrent restart recovery.
pub(crate) async fn recreate_after_confirmation(
    work_db: &WorkDb,
    tmux: &Tmux,
    requested_model: &str,
    expected_spawn_token: &str,
    working_directory: &Path,
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
    start_new(work_db, tmux, requested_model, working_directory).await
}

async fn reconcile_existing(
    work_db: &WorkDb,
    tmux: &Tmux,
    requested_model: &str,
    mut record: CoordinatorTmuxRecord,
    working_directory: &Path,
) -> Result<CoordinatorTmuxRecord> {
    if !session_exists(tmux, &record.session_name).await? {
        // Covers both crash windows in which metadata was committed but
        // `new-session` never happened, and normal session loss. No live
        // conversation remains, so recreation is non-destructive.
        return start_new(work_db, tmux, requested_model, working_directory).await;
    }
    let live_token = tmux.show_environment(&record.session_name, SPAWN_TOKEN_ENV).await?;
    match live_token {
        Some(token) if token == record.spawn_token => {
            if tmux
                .display_message(&record.session_name, DisplayField::PaneDead)
                .await?
                .trim()
                == "1"
            {
                tmux.kill_session_verified(&record.session_name, &record.spawn_token)
                    .await
                    .context("removing dead coordinator tmux session before restart")?;
                return start_new(work_db, tmux, requested_model, working_directory).await;
            }
            // Live matching-token sessions are left alone (including
            // model mismatches, which the app surfaces for confirmation).
            // An interrupted create still needs its token mirror repaired.
            if record.spawn_state == "intended" {
                confirm_existing_intent(work_db, tmux, &record).await?;
                record.spawn_state = "created".to_owned();
            }
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

async fn session_exists(tmux: &Tmux, name: &str) -> Result<bool> {
    Ok(tmux.list_sessions().await?.iter().any(|session| session.name == name))
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
    work_db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, &spawn_token, model)?;

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
    })
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

    use super::*;

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
        let tmux = Tmux::with_runner("/usr/bin/tmux", server.clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        (WorkDb::open(PathBuf::from(":memory:")).unwrap(), tmux, server, dir)
    }

    fn tmux_for(server: FakeTmux) -> (Tmux, Arc<FakeTmux>) {
        let server = Arc::new(server);
        let tmux = Tmux::with_runner("/usr/bin/tmux", server.clone()).unwrap();
        (tmux, server)
    }

    fn send_keys_calls(calls: &[Vec<String>]) -> Vec<&Vec<String>> {
        calls
            .iter()
            .filter(|call| call.get(2).map(String::as_str) == Some("send-keys") && call.iter().any(|a| a == "-l"))
            .collect()
    }

    #[tokio::test]
    async fn ensure_without_record_writes_intent_before_new_session_and_mirrors_options() {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![], None, "0"));
        let record = ensure_for_attach(&db, &tmux, "opus", dir.path()).await.unwrap();
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
        assert_eq!(calls[1][2], "set-option");
        assert_eq!(calls[1][5], "@boss_spawn_token");
        assert_eq!(calls[2][5], "remain-on-exit");
    }

    #[tokio::test]
    async fn intended_live_session_repairs_its_tmux_mirrors() {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus")
            .unwrap();
        let record = ensure_for_attach(&db, &tmux, "opus", dir.path()).await.unwrap();
        assert_eq!(record.spawn_state, "created");
        let calls = server.calls();
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
    async fn dead_matching_session_is_killed_before_recreation() {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "1"));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus")
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();
        ensure_for_attach(&db, &tmux, "opus", dir.path()).await.unwrap();
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
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus")
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();
        assert!(ensure_for_attach(&db, &tmux, "opus", dir.path()).await.is_err());
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
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus")
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();

        let record = ensure_for_attach(&db, &tmux, "sonnet", dir.path()).await.unwrap();

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
        let err = ensure_for_attach(&db, &tmux, "opus", &missing).await.unwrap_err();
        assert!(err.to_string().contains("not prepared"), "unexpected error: {err:#}");
        assert!(
            server.calls().is_empty(),
            "tmux must not be invoked when the session directory is missing"
        );
    }

    #[tokio::test]
    async fn stale_confirmation_does_not_kill_current_session() {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus")
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();
        assert!(
            recreate_after_confirmation(&db, &tmux, "sonnet", "stale", dir.path())
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

    // --- coordinator prompt re-read nudge ---

    #[tokio::test]
    async fn freshly_created_session_is_not_nudged() {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(RENDERED_PROMPT_FILENAME), "prompt v1").unwrap();

        let (tmux, server) = tmux_for(FakeTmux::new(vec![], None, "0"));
        ensure_for_attach(&db, &tmux, "opus", dir.path()).await.unwrap();

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
        let created = ensure_for_attach(&db, &tmux, "opus", dir.path()).await.unwrap();

        // Simulate a restart that adopts the still-live session: same
        // prompt content, no new-session/kill-session activity this time.
        let (tmux, server) = tmux_for(FakeTmux::new(
            vec![COORDINATOR_SESSION_NAME],
            Some(&created.spawn_token),
            "0",
        ));
        ensure_for_attach(&db, &tmux, "opus", dir.path()).await.unwrap();

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
        db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus")
            .unwrap();
        db.record_coordinator_tmux_session_created("token").unwrap();
        assert_eq!(db.get_metadata(PROMPT_NUDGE_HASH_KEY).unwrap(), None);

        let (tmux, server) = tmux_for(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        ensure_for_attach(&db, &tmux, "opus", dir.path()).await.unwrap();

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
        let created = ensure_for_attach(&db, &tmux, "opus", dir.path()).await.unwrap();

        std::fs::write(dir.path().join(RENDERED_PROMPT_FILENAME), "prompt v2").unwrap();
        let (tmux, server) = tmux_for(FakeTmux::new(
            vec![COORDINATOR_SESSION_NAME],
            Some(&created.spawn_token),
            "0",
        ));
        ensure_for_attach(&db, &tmux, "opus", dir.path()).await.unwrap();

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
        let created = ensure_for_attach(&db, &tmux, "opus", dir.path()).await.unwrap();

        std::fs::write(dir.path().join(RENDERED_PROMPT_FILENAME), "prompt v2").unwrap();
        let (tmux, _server) = tmux_for(FakeTmux::new(
            vec![COORDINATOR_SESSION_NAME],
            Some(&created.spawn_token),
            "0",
        ));
        ensure_for_attach(&db, &tmux, "opus", dir.path()).await.unwrap();

        // A second restart, prompt unchanged since the nudge above.
        let (tmux, server) = tmux_for(FakeTmux::new(
            vec![COORDINATOR_SESSION_NAME],
            Some(&created.spawn_token),
            "0",
        ));
        ensure_for_attach(&db, &tmux, "opus", dir.path()).await.unwrap();

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
        let created = ensure_for_attach(&db, &tmux, "opus", dir.path()).await.unwrap();
        let hash_v1 = hash_rendered_prompt(dir.path()).unwrap();
        assert_eq!(db.get_metadata(PROMPT_NUDGE_HASH_KEY).unwrap(), Some(hash_v1.clone()));

        std::fs::write(dir.path().join(RENDERED_PROMPT_FILENAME), "prompt v2").unwrap();
        let (tmux, server) = tmux_for(FakeTmux::with_send_keys_outcome(
            vec![COORDINATOR_SESSION_NAME],
            Some(&created.spawn_token),
            "0",
            true,
        ));
        ensure_for_attach(&db, &tmux, "opus", dir.path()).await.unwrap();
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
        ensure_for_attach(&db, &tmux, "opus", dir.path()).await.unwrap();
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
