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

use crate::engine_control::generate_token;
use crate::spawn_flow::TMUX_SESSION_SCHEMA;
use crate::work::{CoordinatorTmuxRecord, WorkDb};

pub const COORDINATOR_SESSION_NAME: &str = "boss-coordinator";
const SPAWN_TOKEN_ENV: &str = "BOSS_SPAWN_TOKEN";
const SESSION_SCHEMA_ENV: &str = "BOSS_SESSION_SCHEMA";
const SPAWN_TOKEN_OPTION: &str = "@boss_spawn_token";

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
    tmux.kill_session(&record.session_name)
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
                tmux.kill_session(&record.session_name)
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
                tmux.kill_session(&record.session_name)
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
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeTmux {
        fn new(sessions: Vec<&str>, token: Option<&str>, pane_dead: &str) -> Self {
            Self {
                sessions: sessions.into_iter().map(str::to_owned).collect(),
                token: token.map(str::to_owned),
                pane_dead: pane_dead.to_owned(),
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
}
