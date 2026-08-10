//! Engine-owned lifecycle for the durable coordinator tmux session.
//!
//! The coordinator is deliberately not a `work_runs` row: it has no slot,
//! workspace, or execution. Its durable pointer is instead the metadata
//! singleton managed by [`crate::work::WorkDb`]. The write ordering remains
//! identical to a worker session: commit `intended`, create with the token in
//! the atomic tmux environment, mirror/confirm, then mark `created`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use boss_tmux::{DisplayField, NewSession, Tmux};

use crate::engine_control::generate_token;
use crate::spawn_flow::TMUX_SESSION_SCHEMA;
use crate::work::{CoordinatorTmuxRecord, WorkDb};

pub const COORDINATOR_SESSION_NAME: &str = "boss-coordinator";
const SPAWN_TOKEN_ENV: &str = "BOSS_SPAWN_TOKEN";
const SESSION_SCHEMA_ENV: &str = "BOSS_SESSION_SCHEMA";
const SPAWN_TOKEN_OPTION: &str = "@boss_spawn_token";

/// The result of reconciling the singleton with the requested model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CoordinatorState {
    /// A live session is ready for the app to attach.
    Running(CoordinatorTmuxRecord),
    /// A live session has a different model. It remains attached until a
    /// human confirms recreation because that action discards conversation.
    ModelChangeRequiresConfirmation(CoordinatorTmuxRecord),
}

/// Create or recover the coordinator for an app that has just registered.
///
/// A model mismatch intentionally returns a non-destructive state instead of
/// killing a live conversation. Call [`recreate_after_confirmation`] only
/// after the UI has made that loss explicit to the operator.
pub(crate) async fn ensure_for_attach(
    work_db: &WorkDb,
    tmux: &Tmux,
    requested_model: &str,
) -> Result<CoordinatorState> {
    match work_db.coordinator_tmux_record()? {
        None => start_new(work_db, tmux, requested_model)
            .await
            .map(CoordinatorState::Running),
        Some(record) => reconcile_existing(work_db, tmux, requested_model, record).await,
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
) -> Result<Option<CoordinatorTmuxRecord>> {
    let Some(record) = work_db.coordinator_tmux_record()? else {
        return Ok(None);
    };
    if !session_exists(tmux, &record.session_name).await? {
        return start_new(work_db, tmux, requested_model).await.map(Some);
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
    start_new(work_db, tmux, requested_model).await.map(Some)
}

/// Recreate a model-mismatched coordinator after an explicit UI confirmation.
/// The expected token prevents a delayed confirmation from killing a newer
/// session created by a concurrent restart recovery.
pub(crate) async fn recreate_after_confirmation(
    work_db: &WorkDb,
    tmux: &Tmux,
    requested_model: &str,
    expected_spawn_token: &str,
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
    start_new(work_db, tmux, requested_model).await
}

async fn reconcile_existing(
    work_db: &WorkDb,
    tmux: &Tmux,
    requested_model: &str,
    mut record: CoordinatorTmuxRecord,
) -> Result<CoordinatorState> {
    if !session_exists(tmux, &record.session_name).await? {
        // Covers both crash windows in which metadata was committed but
        // `new-session` never happened, and normal session loss. No live
        // conversation remains, so recreation is non-destructive.
        return start_new(work_db, tmux, requested_model)
            .await
            .map(CoordinatorState::Running);
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
                return start_new(work_db, tmux, requested_model)
                    .await
                    .map(CoordinatorState::Running);
            }
            // `model` was added with this durable singleton. A record written
            // by an early build has no model; preserve its live conversation
            // rather than guessing that recreation is safe.
            if !record.model.is_empty() && record.model != requested_model {
                return Ok(CoordinatorState::ModelChangeRequiresConfirmation(record));
            }
            if record.spawn_state == "intended" {
                confirm_existing_intent(work_db, tmux, &record).await?;
                record.spawn_state = "created".to_owned();
            }
            Ok(CoordinatorState::Running(record))
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

async fn start_new(work_db: &WorkDb, tmux: &Tmux, model: &str) -> Result<CoordinatorTmuxRecord> {
    let model = model.trim();
    if model.is_empty() {
        bail!("coordinator model may not be empty");
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
    let working_directory = coordinator_working_directory()?;
    let quoted_model = boss_ssh_transport::shell_quote(model);
    let command = format!(
        "[ -n \"$BOSS_BIN_DIR\" ] && export PATH=\"$BOSS_BIN_DIR:$PATH\"; unset ANTHROPIC_API_KEY; exec claude --model {quoted_model} --permission-mode auto"
    );
    tmux.new_session(&NewSession {
        name: COORDINATOR_SESSION_NAME.to_owned(),
        environment,
        working_directory,
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

fn coordinator_working_directory() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is required for the coordinator session"))?;
    let path = PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("Boss")
        .join("boss-session");
    if !path.is_dir() {
        bail!("coordinator session directory is not prepared: {}", path.display());
    }
    Ok(path)
}
