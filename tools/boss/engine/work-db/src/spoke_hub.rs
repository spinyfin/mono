//! Hub-side durable state for spoke hosts: who owns an execution, and the
//! per-host command outbox.
//!
//! Two invariants live here because only a database transaction can keep them:
//!
//! * **One execution owner.** `(execution_id, run_id, assignment_generation,
//!   host_id, spawn_token)` is committed by [`WorkDb::reserve_spoke_assignment`]
//!   before any command naming it can be stored, and an unresolved assignment is
//!   never handed to a second host. Only an explicit
//!   [`WorkDb::resolve_spoke_assignment`] frees it.
//! * **Durable before sent.** A command is a row in `host_commands` before it can
//!   leave the hub, keyed by its stable command id. A timeout never deletes it,
//!   and a result is stored by [`WorkDb::settle_host_command`] independently of
//!   the receipt that preceded it.
//!
//! Payloads are opaque bytes here; the transport crate owns their encoding, and
//! the ingress sink that stores spoke observations joins the same database.
use crate::*;

pub(crate) const MIGRATION_ID: &str = "20261106_1500_spoke_hub_assignments_and_outbox";
pub(crate) const RUN_INDEX_MIGRATION_ID: &str = "20261107_1600_host_commands_run_idx";
pub(crate) const LEGACY_DESTINATION_MIGRATION_ID: &str = "20261108_2100_legacy_destination_claims";

/// Most unsettled commands and encoded bytes one host may hold.
pub const MAX_UNSETTLED_HOST_COMMANDS: i64 = 1024;
pub const MAX_UNSETTLED_HOST_COMMAND_BYTES: i64 = 64 * 1024 * 1024;
/// How long a settled command is kept for an at-least-once retry of its id.
pub const SETTLED_HOST_COMMAND_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// Newest settled commands per host that retention never removes.
pub const MIN_RETAINED_SETTLED_HOST_COMMANDS: u32 = 256;

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS spoke_assignments (
            execution_id          TEXT NOT NULL,
            assignment_generation INTEGER NOT NULL,
            run_id                TEXT NOT NULL UNIQUE,
            spawn_token           TEXT NOT NULL UNIQUE,
            host_id               TEXT NOT NULL,
            repo_id               TEXT,
            workspace_id          TEXT,
            lease_id              TEXT,
            display_path          TEXT,
            pane_pid              INTEGER,
            in_doubt              INTEGER NOT NULL DEFAULT 0,
            created_at_ms         INTEGER NOT NULL,
            resolved_at_ms        INTEGER,
            resolution            TEXT,
            PRIMARY KEY (execution_id, assignment_generation)
         );
         CREATE UNIQUE INDEX IF NOT EXISTS spoke_assignments_open_idx
            ON spoke_assignments(execution_id) WHERE resolved_at_ms IS NULL;
         CREATE INDEX IF NOT EXISTS spoke_assignments_lease_idx
            ON spoke_assignments(host_id, lease_id) WHERE lease_id IS NOT NULL;
         CREATE TABLE IF NOT EXISTS host_transports (
            host_id       TEXT PRIMARY KEY,
            transport     TEXT NOT NULL,
            updated_at_ms INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS host_command_journals (
            host_id    TEXT PRIMARY KEY,
            journal_id TEXT NOT NULL,
            received   INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS host_commands (
            host_id          TEXT NOT NULL,
            command_id       TEXT NOT NULL,
            sequence         INTEGER,
            run_execution_id TEXT,
            run_generation   INTEGER,
            run_id           TEXT,
            spawn_token      TEXT,
            request          BLOB NOT NULL,
            wire             BLOB NOT NULL,
            result           BLOB,
            created_at_ms    INTEGER NOT NULL,
            settled_at_ms    INTEGER,
            PRIMARY KEY (host_id, command_id)
         );
         CREATE UNIQUE INDEX IF NOT EXISTS host_commands_sequence_idx
            ON host_commands(host_id, sequence) WHERE sequence IS NOT NULL;",
    )?;
    Ok(())
}

pub(crate) fn migrate_host_commands_run_idx(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS host_commands_run_idx
         ON host_commands(run_execution_id, run_generation)
         WHERE run_execution_id IS NOT NULL",
    )?;
    Ok(())
}

pub(crate) fn migrate_legacy_destinations(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS legacy_destinations (
            execution_id  TEXT PRIMARY KEY,
            host_id       TEXT NOT NULL,
            transport     TEXT NOT NULL,
            claimed_at_ms INTEGER NOT NULL
         );",
    )?;
    Ok(())
}

/// The durable ownership of one execution attempt by one spoke host.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct SpokeAssignment {
    pub execution_id: String,
    pub assignment_generation: u64,
    pub run_id: String,
    pub spawn_token: String,
    pub host_id: String,
    pub repo_id: Option<String>,
    pub workspace_id: Option<String>,
    pub lease_id: Option<String>,
    /// The workspace path on the host. Display only; the hub never opens it.
    pub display_path: Option<String>,
    pub pane_pid: Option<i64>,
    pub resolved: bool,
}

/// The run identity a command or result is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpokeRunKey {
    pub execution_id: String,
    pub assignment_generation: u64,
    pub run_id: String,
    pub spawn_token: String,
}

/// Re-encodes a stored command's wire bytes under a new sequence number.
pub type Resequence<'a> = &'a dyn Fn(&[u8], u64) -> Result<Vec<u8>>;

/// Which transport new work for a host uses. A host with no row is `LegacySsh`.
/// A run keeps the transport it started on: an execution with a spoke
/// assignment is a spoke run whatever this says now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostTransport {
    LegacySsh,
    /// Enrolled, readiness not proven: new work still goes over SSH. An
    /// assignment that already has spoke evidence stays addressable.
    SpokePending,
    SpokeReady,
}

/// Whether a run-bound command may start new work, or only address an already
/// assigned attempt (including a resolved newest generation, for recovery).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunAdmission {
    /// Lease, spawn, or any command that starts work. The assignment must be
    /// the newest generation and still unresolved.
    Start,
    /// Recovery operations stay addressable after the assignment resolves.
    Recovery,
}

impl HostTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LegacySsh => "legacy_ssh",
            Self::SpokePending => "spoke_pending",
            Self::SpokeReady => "spoke_ready",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "legacy_ssh" => Self::LegacySsh,
            "spoke_pending" => Self::SpokePending,
            "spoke_ready" => Self::SpokeReady,
            _ => return None,
        })
    }
}

/// A refusal to hand an unresolved assignment to a second host.
#[derive(Debug, thiserror::Error)]
#[error(
    "execution {execution_id} is still assigned to host {held_by} (generation {generation}); \
     an unresolved assignment is never handed to another host"
)]
pub struct SpokeHeldByOtherHost {
    pub execution_id: String,
    pub held_by: String,
    pub generation: u64,
}

/// A refusal to hand an execution to a host while a legacy (non-spoke)
/// destination holds it: another host claimed it for SSH or local work, or one
/// of its leases lives there. Those transports have no fence of their own, so the
/// claim stays until [`WorkDb::release_legacy_destination`] or a lease says
/// otherwise.
#[derive(Debug, thiserror::Error)]
#[error(
    "execution {execution_id} is claimed by legacy destination {held_by}; \
     it is never handed to host {requested} while that claim is live"
)]
pub struct LegacyDestinationHeld {
    pub execution_id: String,
    pub held_by: String,
    pub requested: String,
}

/// A refusal to move a host back to SSH while spoke runs are still assigned to
/// it. Those runs are controlled over the spoke stream; flipping the transport
/// under them would strand work the hub can no longer address.
#[derive(Debug, thiserror::Error)]
#[error(
    "host {host_id} still has an unresolved spoke assignment for execution {execution_id}; \
     resolve it before moving the host back to SSH"
)]
pub struct SpokeHostInUse {
    pub host_id: String,
    pub execution_id: String,
}

/// Why the outbox refused a request. Carried inside the returned error so the
/// transport adapter can map it without parsing text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HostOutboxFault {
    #[error("replay_gap")]
    ReplayGap,
    #[error("not permitted")]
    Denied,
    #[error("durable quota exhausted")]
    Full,
}

/// More than one open assignment matched a pid or workspace lookup. Those
/// keys never identify a worker on their own, so the caller must treat
/// liveness as unknown.
#[derive(Debug, thiserror::Error)]
#[error(
    "host {host_id} has more than one open assignment matching {kind} {value}; \
     pids and pane numbers alone never identify a worker"
)]
pub struct AmbiguousSpokeAssignment {
    pub host_id: String,
    pub kind: &'static str,
    pub value: String,
}

fn fault(fault: HostOutboxFault) -> anyhow::Error {
    fault.into()
}

/// A stored command as it travels: its wire sequence and original bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredHostCommand {
    pub sequence: u64,
    pub wire: Vec<u8>,
}

const ASSIGNMENT_COLUMNS: &str = "execution_id, assignment_generation, run_id, spawn_token, host_id, repo_id,
    workspace_id, lease_id, display_path, pane_pid, resolved_at_ms";

fn map_assignment(row: &Row<'_>) -> rusqlite::Result<SpokeAssignment> {
    Ok(SpokeAssignment {
        execution_id: row.get(0)?,
        assignment_generation: row.get::<_, i64>(1)? as u64,
        run_id: row.get(2)?,
        spawn_token: row.get(3)?,
        host_id: row.get(4)?,
        repo_id: row.get(5)?,
        workspace_id: row.get(6)?,
        lease_id: row.get(7)?,
        display_path: row.get(8)?,
        pane_pid: row.get(9)?,
        resolved: row.get::<_, Option<i64>>(10)?.is_some(),
    })
}

/// Whether anything may already have run for this assignment: it holds a lease,
/// a command is in doubt, or any command names the run. A hub rejection does
/// not prove that an earlier connection never delivered the command.
fn assignment_may_have_started(conn: &Connection, execution_id: &str, generation: i64) -> Result<bool> {
    Ok(assignment_committed_to_spoke(conn, execution_id, generation)?
        || conn.query_row(
            "SELECT COALESCE((SELECT in_doubt <> 0 FROM spoke_assignments
                               WHERE execution_id = ?1 AND assignment_generation = ?2), 0)",
            params![execution_id, generation],
            |row| row.get(0),
        )?)
}

/// Whether the spoke has real evidence of this assignment: it holds a lease or a
/// command names the run. Unlike [`assignment_may_have_started`] this ignores the
/// in-doubt flag, which a caller can set without any command existing.
fn assignment_committed_to_spoke(conn: &Connection, execution_id: &str, generation: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT
            COALESCE((SELECT lease_id IS NOT NULL FROM spoke_assignments
                       WHERE execution_id = ?1 AND assignment_generation = ?2), 0)
            OR EXISTS(SELECT 1 FROM host_commands
                       WHERE run_execution_id = ?1 AND run_generation = ?2)",
        params![execution_id, generation],
        |row| row.get(0),
    )?)
}

/// The host a legacy (non-spoke) destination holds `execution_id` on: the host
/// of a cube lease no spoke assignment recorded, else the host that claimed it
/// through [`WorkDb::claim_legacy_destination`].
fn legacy_binding(conn: &Connection, execution_id: &str) -> Result<Option<String>> {
    let leased: Option<String> = conn
        .query_row(
            "SELECT COALESCE(e.host_id, 'local') FROM work_executions e
              WHERE e.id = ?1 AND e.cube_lease_id IS NOT NULL
                AND NOT EXISTS(SELECT 1 FROM spoke_assignments a WHERE a.lease_id = e.cube_lease_id)",
            [execution_id],
            |row| row.get(0),
        )
        .optional()?;
    if leased.is_some() {
        return Ok(leased);
    }
    Ok(conn
        .query_row(
            // A claim binds while its destination can be leasing or running: a
            // live execution has a run on the claimed host, or the claim is recent and
            // the host still has the transport it was made under. A claim whose
            // destination never leased (an unreachable SSH host) expires, and
            // one made while the host was `spoke_pending` stops binding once the
            // host's transport moves on.
            "SELECT d.host_id FROM legacy_destinations d
              WHERE d.execution_id = ?1
                AND (EXISTS(SELECT 1 FROM work_runs r
                             JOIN work_executions e ON e.id = r.execution_id
                             WHERE r.execution_id = d.execution_id AND e.status IN ('running', 'waiting_human')
                               AND COALESCE(r.host_id, 'local') = d.host_id)
                     OR (d.claimed_at_ms > ?2
                         AND d.transport = COALESCE((SELECT transport FROM host_transports WHERE host_id = d.host_id),
                                                    'legacy_ssh')))",
            params![execution_id, now_ms() - LEGACY_CLAIM_GRACE_MS],
            |row| row.get(0),
        )
        .optional()?)
}

/// How long a legacy claim with no lease and no run binds the execution: long
/// enough for the destination to lease its workspace, short enough that a
/// destination that failed before leasing does not pin the execution.
const LEGACY_CLAIM_GRACE_MS: i64 = 10 * 60 * 1000;

fn now_ms() -> i64 {
    boss_engine_utils::epoch_time::now_epoch_ms() as i64
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Reserve `execution_id` for `host_id`, or return the reservation it already
    /// holds there. Atomic: the check and the insert share one transaction, so
    /// two dispatchers cannot both win. Fails with [`SpokeHeldByOtherHost`] while
    /// an unresolved assignment names a different host.
    pub fn reserve_spoke_assignment(&self, execution_id: &str, host_id: &str) -> Result<SpokeAssignment> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // A legacy destination already holds the execution (a claim, or a lease
        // there); its transport has no fence, so a spoke assignment would run it
        // a second time.
        if let Some(held_by) = legacy_binding(&tx, execution_id)? {
            return Err(LegacyDestinationHeld {
                execution_id: execution_id.to_owned(),
                held_by,
                requested: host_id.to_owned(),
            }
            .into());
        }
        // Nothing binds the execution to a legacy destination any more, so a
        // leftover expired claim must not revive if that host's transport
        // changes back.
        tx.execute(
            "DELETE FROM legacy_destinations WHERE execution_id = ?1",
            [execution_id],
        )?;
        let open = tx
            .query_row(
                &format!("SELECT {ASSIGNMENT_COLUMNS} FROM spoke_assignments WHERE execution_id = ?1 AND resolved_at_ms IS NULL"),
                [execution_id],
                map_assignment,
            )
            .optional()?;
        if let Some(open) = open {
            if open.host_id == host_id {
                return Ok(open);
            }
            // A reservation that never leased, has no command in doubt and has
            // never had a command stored for it cannot have started anything, so
            // it does not pin the execution to that host.
            if assignment_may_have_started(&tx, execution_id, open.assignment_generation as i64)? {
                return Err(SpokeHeldByOtherHost {
                    execution_id: execution_id.to_owned(),
                    held_by: open.host_id,
                    generation: open.assignment_generation,
                }
                .into());
            }
            tx.execute(
                "UPDATE spoke_assignments SET resolved_at_ms = ?3, resolution = 'never_started'
                 WHERE execution_id = ?1 AND assignment_generation = ?2",
                params![execution_id, open.assignment_generation as i64, now_ms()],
            )?;
        }
        let generation: i64 = tx.query_row(
            "SELECT COALESCE(MAX(assignment_generation), 0) + 1 FROM spoke_assignments WHERE execution_id = ?1",
            [execution_id],
            |row| row.get(0),
        )?;
        let run_id = format!("{execution_id}-g{generation}");
        let spawn_token = crate::next_id("spawn");
        tx.execute(
            "INSERT INTO spoke_assignments
                (execution_id, assignment_generation, run_id, spawn_token, host_id, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![execution_id, generation, run_id, spawn_token, host_id, now_ms()],
        )?;
        tx.commit()?;
        Ok(SpokeAssignment::builder()
            .execution_id(execution_id)
            .assignment_generation(generation as u64)
            .run_id(run_id)
            .spawn_token(spawn_token)
            .host_id(host_id)
            .resolved(false)
            .build())
    }

    /// Make `host_id` the destination of `execution_id` for a transport that
    /// reserves no spoke assignment (local, SSH, a host whose spoke is pending),
    /// and record that claim, all in one transaction.
    ///
    /// * A legacy claim or lease on another host refuses the move
    ///   ([`LegacyDestinationHeld`]); neither transport can be fenced from here.
    /// * An unresolved spoke assignment that nothing can have started on is
    ///   retired as `never_started`, whichever host holds it, so a bound adapter
    ///   for it is fenced out of command admission from now on. One that holds a
    ///   lease or has a command stored is the spoke's run: on `host_id` it is
    ///   returned and keeps being used, on another host the move is refused
    ///   ([`SpokeHeldByOtherHost`]). An in-doubt flag alone is genuinely
    ///   uncertain ownership and is refused the same way.
    ///
    /// Returns `None` once the claim is recorded, or the spoke assignment the
    /// execution keeps on `host_id`.
    pub fn claim_legacy_destination(
        &self,
        execution_id: &str,
        host_id: &str,
        transport: HostTransport,
    ) -> Result<Option<SpokeAssignment>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(held_by) = legacy_binding(&tx, execution_id)?
            && held_by != host_id
        {
            return Err(LegacyDestinationHeld {
                execution_id: execution_id.to_owned(),
                held_by,
                requested: host_id.to_owned(),
            }
            .into());
        }
        let open = tx
            .query_row(
                &format!("SELECT {ASSIGNMENT_COLUMNS} FROM spoke_assignments WHERE execution_id = ?1 AND resolved_at_ms IS NULL"),
                [execution_id],
                map_assignment,
            )
            .optional()?;
        if let Some(open) = open {
            let generation = open.assignment_generation as i64;
            if open.host_id == host_id && assignment_committed_to_spoke(&tx, execution_id, generation)? {
                return Ok(Some(open));
            }
            if assignment_may_have_started(&tx, execution_id, generation)? {
                return Err(SpokeHeldByOtherHost {
                    execution_id: execution_id.to_owned(),
                    held_by: open.host_id,
                    generation: open.assignment_generation,
                }
                .into());
            }
            tx.execute(
                "UPDATE spoke_assignments SET resolved_at_ms = ?3, resolution = 'never_started'
                 WHERE execution_id = ?1 AND assignment_generation = ?2",
                params![execution_id, generation, now_ms()],
            )?;
        }
        // A claim that still binds is on this host (checked above) and is kept
        // as it is; an expired or superseded one is replaced.
        if legacy_binding(&tx, execution_id)?.is_none() {
            tx.execute(
                "INSERT INTO legacy_destinations(execution_id, host_id, transport, claimed_at_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(execution_id) DO UPDATE SET
                    host_id = excluded.host_id,
                    transport = excluded.transport,
                    claimed_at_ms = excluded.claimed_at_ms",
                params![execution_id, host_id, transport.as_str(), now_ms()],
            )?;
        }
        tx.commit()?;
        Ok(None)
    }

    /// Drop the legacy claim `host_id` holds on `execution_id`, for a handoff
    /// that was abandoned before anything ran there. A lease the execution holds
    /// on that host keeps it bound. Returns whether a claim was removed.
    pub fn release_legacy_destination(&self, execution_id: &str, host_id: &str) -> Result<bool> {
        Ok(self.connect()?.execute(
            "DELETE FROM legacy_destinations WHERE execution_id = ?1 AND host_id = ?2",
            params![execution_id, host_id],
        )? == 1)
    }

    /// The host a legacy destination holds `execution_id` on, by claim or lease.
    pub fn legacy_destination_host(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        legacy_binding(&conn, execution_id)
    }

    /// Whether `host_id` has ever held a spoke assignment, resolved or not.
    pub fn host_has_spoke_history(&self, host_id: &str) -> Result<bool> {
        Ok(self.connect()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM spoke_assignments WHERE host_id = ?1)",
            [host_id],
            |row| row.get(0),
        )?)
    }

    /// Whether an execution on `host_id` holds a cube lease that no spoke
    /// assignment recorded: a run still using the legacy transport.
    pub fn host_has_legacy_lease(&self, host_id: &str) -> Result<bool> {
        Ok(self.connect()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_executions e
                            WHERE e.host_id = ?1 AND e.cube_lease_id IS NOT NULL
                              AND NOT EXISTS(SELECT 1 FROM spoke_assignments a
                                              WHERE a.lease_id = e.cube_lease_id))",
            [host_id],
            |row| row.get(0),
        )?)
    }

    /// Whether the transcript at `path` is the legacy transport's to read on
    /// `host_id`: the recorded transcript of a run on this host that no spoke
    /// assignment owned. A host that ever ran spoke work answers every other path
    /// as unavailable, because a spoke run's transcript lives on the host and is
    /// materialized on the hub by run identity, whatever layout the driver uses.
    /// A host with no such history has nothing but legacy runs to read.
    pub fn transcript_is_legacy(&self, host_id: &str, path: &str) -> Result<bool> {
        Ok(self.connect()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_runs r
                            WHERE r.host_id = ?1 AND r.transcript_path = ?2
                              AND NOT EXISTS(SELECT 1 FROM spoke_assignments a
                                              WHERE a.execution_id = r.execution_id
                                                AND COALESCE(a.resolution, '') <> 'never_started'))
                 OR NOT EXISTS(SELECT 1 FROM spoke_assignments
                                WHERE host_id = ?1 AND COALESCE(resolution, '') <> 'never_started')",
            params![host_id, path],
            |row| row.get(0),
        )?)
    }

    /// Whether any unresolved spoke assignment is held on `host_id`: its workspaces
    /// exist only on the host's spoke, whatever transport new work uses.
    pub fn host_has_open_spoke_assignment(&self, host_id: &str) -> Result<bool> {
        Ok(self.connect()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM spoke_assignments WHERE host_id = ?1 AND resolved_at_ms IS NULL)",
            [host_id],
            |row| row.get(0),
        )?)
    }

    /// The execution's open assignment, else its latest resolved one that held
    /// or could have held a run (not one retired as `never_started`). Recovery
    /// operations stay addressable after the lease is released.
    pub fn spoke_assignment_for_execution(&self, execution_id: &str) -> Result<Option<SpokeAssignment>> {
        Ok(self
            .connect()?
            .query_row(
                &format!(
                    "SELECT {ASSIGNMENT_COLUMNS} FROM spoke_assignments
                     WHERE execution_id = ?1 AND COALESCE(resolution, '') <> 'never_started'
                     ORDER BY resolved_at_ms IS NULL DESC, assignment_generation DESC LIMIT 1"
                ),
                [execution_id],
                map_assignment,
            )
            .optional()?)
    }

    /// The assignment that holds `lease_id` on `host_id`. Lease ids are not reused.
    pub fn spoke_assignment_for_lease(&self, host_id: &str, lease_id: &str) -> Result<Option<SpokeAssignment>> {
        Ok(self
            .connect()?
            .query_row(
                &format!("SELECT {ASSIGNMENT_COLUMNS} FROM spoke_assignments WHERE host_id = ?1 AND lease_id = ?2"),
                params![host_id, lease_id],
                map_assignment,
            )
            .optional()?)
    }

    /// The open assignment using `workspace_id` on `host_id`. Workspaces are
    /// leased repeatedly, so only an unresolved assignment can own one. Two
    /// open matches is unknown liveness, not an arbitrary winner.
    pub fn open_spoke_assignment_for_workspace(
        &self,
        host_id: &str,
        workspace_id: &str,
    ) -> Result<Option<SpokeAssignment>> {
        let conn = self.connect()?;
        unique_open_assignment(
            &conn,
            &format!(
                "SELECT {ASSIGNMENT_COLUMNS} FROM spoke_assignments
                 WHERE host_id = ?1 AND workspace_id = ?2 AND resolved_at_ms IS NULL LIMIT 2"
            ),
            params![host_id, workspace_id],
            host_id,
            "workspace_id",
            workspace_id,
        )
    }

    /// The open assignment whose worker pane had `pid` when it launched. Two
    /// open matches is unknown liveness: a recycled pid must not observe the
    /// wrong run.
    pub fn open_spoke_assignment_for_pane_pid(&self, host_id: &str, pid: i64) -> Result<Option<SpokeAssignment>> {
        let conn = self.connect()?;
        unique_open_assignment(
            &conn,
            &format!(
                "SELECT {ASSIGNMENT_COLUMNS} FROM spoke_assignments
                 WHERE host_id = ?1 AND pane_pid = ?2 AND resolved_at_ms IS NULL LIMIT 2"
            ),
            params![host_id, pid],
            host_id,
            "pane_pid",
            &pid.to_string(),
        )
    }

    /// Record the repository an assignment is leasing in, before the lease
    /// command leaves. Written once; a different repository is refused.
    pub fn bind_spoke_repo(&self, execution_id: &str, generation: u64, repo_id: &str) -> Result<()> {
        let changed = self.connect()?.execute(
            "UPDATE spoke_assignments SET repo_id = ?3
             WHERE execution_id = ?1 AND assignment_generation = ?2 AND COALESCE(repo_id, ?3) = ?3",
            params![execution_id, generation as i64, repo_id],
        )?;
        anyhow::ensure!(
            changed == 1,
            "assignment {execution_id}#{generation} already leases in another repository"
        );
        Ok(())
    }

    /// Record the lease the host granted this assignment.
    pub fn bind_spoke_lease(
        &self,
        execution_id: &str,
        generation: u64,
        workspace_id: &str,
        lease_id: &str,
        display_path: &str,
    ) -> Result<()> {
        let changed = self.connect()?.execute(
            "UPDATE spoke_assignments SET workspace_id = ?3, lease_id = ?4, display_path = ?5
             WHERE execution_id = ?1 AND assignment_generation = ?2
               AND (lease_id IS NULL OR lease_id = ?4)",
            params![execution_id, generation as i64, workspace_id, lease_id, display_path],
        )?;
        anyhow::ensure!(
            changed == 1,
            "assignment {execution_id}#{generation} already holds a different lease"
        );
        Ok(())
    }

    pub fn record_spoke_pane_pid(&self, execution_id: &str, generation: u64, pid: i64) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let host_id: Option<String> = tx
            .query_row(
                "SELECT host_id FROM spoke_assignments WHERE execution_id = ?1 AND assignment_generation = ?2",
                params![execution_id, generation as i64],
                |row| row.get(0),
            )
            .optional()?;
        let Some(host_id) = host_id else {
            return Ok(());
        };
        // A reused pid proves the previous process is gone, so any other open
        // assignment on this host holding it no longer identifies a live pane.
        tx.execute(
            "UPDATE spoke_assignments SET pane_pid = NULL
             WHERE host_id = ?1 AND pane_pid = ?2 AND resolved_at_ms IS NULL
               AND NOT (execution_id = ?3 AND assignment_generation = ?4)",
            params![host_id, pid, execution_id, generation as i64],
        )?;
        tx.execute(
            "UPDATE spoke_assignments SET pane_pid = ?3 WHERE execution_id = ?1 AND assignment_generation = ?2",
            params![execution_id, generation as i64, pid],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Mark whether a command that may have started something is in flight or
    /// unresolved. Set before the command is sent; cleared only when the host
    /// answers that it did nothing.
    pub fn set_spoke_in_doubt(&self, execution_id: &str, generation: u64, in_doubt: bool) -> Result<()> {
        self.connect()?.execute(
            "UPDATE spoke_assignments SET in_doubt = ?3 WHERE execution_id = ?1 AND assignment_generation = ?2",
            params![execution_id, generation as i64, in_doubt],
        )?;
        Ok(())
    }

    pub fn host_transport(&self, host_id: &str) -> Result<HostTransport> {
        let stored: Option<String> = self
            .connect()?
            .query_row(
                "SELECT transport FROM host_transports WHERE host_id = ?1",
                [host_id],
                |row| row.get(0),
            )
            .optional()?;
        match stored {
            None => Ok(HostTransport::LegacySsh),
            Some(text) => HostTransport::parse(&text).with_context(|| format!("unknown host transport {text:?}")),
        }
    }

    /// Set the transport new work uses. Moving a host back to
    /// [`HostTransport::LegacySsh`] is refused while an assignment that may have
    /// started work is unresolved on it, atomically with the write: such a run
    /// keeps its spoke and must be resolved first.
    pub fn set_host_transport(&self, host_id: &str, transport: HostTransport) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if transport == HostTransport::LegacySsh {
            let open: Vec<(String, i64)> = {
                let mut statement = tx.prepare(
                    "SELECT execution_id, assignment_generation FROM spoke_assignments
                     WHERE host_id = ?1 AND resolved_at_ms IS NULL ORDER BY created_at_ms",
                )?;
                statement
                    .query_map([host_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?
            };
            for (execution_id, generation) in open {
                if assignment_may_have_started(&tx, &execution_id, generation)? {
                    return Err(SpokeHostInUse {
                        host_id: host_id.to_owned(),
                        execution_id,
                    }
                    .into());
                }
            }
        }
        tx.execute(
            "INSERT INTO host_transports(host_id, transport, updated_at_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(host_id) DO UPDATE SET transport = excluded.transport, updated_at_ms = excluded.updated_at_ms",
            params![host_id, transport.as_str(), now_ms()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Free an assignment. Only definitive evidence may call this: the host
    /// reported the work never started, or its lease was released. Returns
    /// whether it was open.
    pub fn resolve_spoke_assignment(&self, execution_id: &str, generation: u64, resolution: &str) -> Result<bool> {
        Ok(self.connect()?.execute(
            "UPDATE spoke_assignments SET resolved_at_ms = ?3, resolution = ?4
             WHERE execution_id = ?1 AND assignment_generation = ?2 AND resolved_at_ms IS NULL",
            params![execution_id, generation as i64, now_ms(), resolution],
        )? == 1)
    }

    /// Validate the command journal against the peer's receipt and return
    /// `(journal_id, durable receipt)`. A first contact creates the journal; a
    /// peer that remembers a journal the hub does not hold, or a receipt that
    /// moves backwards or past the stored commands, is a replay gap.
    pub fn resume_host_command_journal(&self, host_id: &str, receipt: Option<(&str, u64)>) -> Result<(String, u64)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let journal: Option<(String, i64)> = tx
            .query_row(
                "SELECT journal_id, received FROM host_command_journals WHERE host_id = ?1",
                [host_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let result = match (journal, receipt) {
            (None, Some((id, _))) if !id.is_empty() => return Err(fault(HostOutboxFault::ReplayGap)),
            (None, _) => {
                let id = crate::next_id("hostcmd");
                tx.execute(
                    "INSERT INTO host_command_journals(host_id, journal_id, received) VALUES (?1, ?2, 0)",
                    params![host_id, id],
                )?;
                (id, 0)
            }
            // A peer that never bound the journal can only have received nothing.
            (Some((id, 0)), None) => (id, 0),
            (Some(_), None) => return Err(fault(HostOutboxFault::ReplayGap)),
            (Some((id, received)), Some((peer_id, through))) => {
                let highest = highest_sequence(&tx, host_id)?;
                if peer_id != id || (through as i64) < received || through > highest {
                    return Err(fault(HostOutboxFault::ReplayGap));
                }
                tx.execute(
                    "UPDATE host_command_journals SET received = ?2 WHERE host_id = ?1",
                    params![host_id, through as i64],
                )?;
                (id, through)
            }
        };
        tx.commit()?;
        Ok(result)
    }

    /// Advance the durable command receipt. Never moves backwards or past a
    /// stored command; receipt is not completion.
    pub fn record_host_command_receipt(&self, host_id: &str, journal_id: &str, through: u64) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<(String, i64)> = tx
            .query_row(
                "SELECT journal_id, received FROM host_command_journals WHERE host_id = ?1",
                [host_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match current {
            Some((id, received))
                if id == journal_id && through as i64 >= received && through <= highest_sequence(&tx, host_id)? =>
            {
                tx.execute(
                    "UPDATE host_command_journals SET received = ?2 WHERE host_id = ?1",
                    params![host_id, through as i64],
                )?;
                tx.commit()?;
                Ok(())
            }
            _ => Err(fault(HostOutboxFault::ReplayGap)),
        }
    }

    /// Store a command and allocate its wire sequence, or return the command
    /// already stored under `command_id`. `request` is the encoding of the
    /// command without a sequence: the same id with different request bytes is
    /// refused. A command naming a run must name an assignment this host holds.
    /// `encode` renders the wire bytes for the allocated sequence.
    pub fn append_host_command(
        &self,
        host_id: &str,
        command_id: &str,
        run: Option<&SpokeRunKey>,
        admission: RunAdmission,
        request: &[u8],
        encode: &dyn Fn(u64) -> Vec<u8>,
    ) -> Result<StoredHostCommand> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(Option<i64>, Vec<u8>, Vec<u8>)> = tx
            .query_row(
                "SELECT sequence, request, wire FROM host_commands WHERE host_id = ?1 AND command_id = ?2",
                params![host_id, command_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((sequence, stored, wire)) = existing {
            if stored != request {
                return Err(fault(HostOutboxFault::Denied));
            }
            return Ok(StoredHostCommand {
                sequence: sequence.unwrap_or(0) as u64,
                wire,
            });
        }
        if let Some(run) = run {
            // Only the execution's newest generation on this host is admitted: a
            // superseded attempt's key is stale, whatever else it still matches.
            // Start-class commands also require the assignment still be open, so
            // a fenced or released newest generation cannot spawn or lease.
            // Recovery keeps a resolved newest generation addressable.
            let start = matches!(admission, RunAdmission::Start);
            let owned: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM spoke_assignments
                  WHERE execution_id = ?1 AND assignment_generation = ?2 AND run_id = ?3
                    AND spawn_token = ?4 AND host_id = ?5
                    AND assignment_generation = (SELECT MAX(assignment_generation) FROM spoke_assignments
                                                  WHERE execution_id = ?1)
                    AND (?6 = 0 OR resolved_at_ms IS NULL))",
                params![
                    run.execution_id,
                    run.assignment_generation as i64,
                    run.run_id,
                    run.spawn_token,
                    host_id,
                    start as i64
                ],
                |row| row.get(0),
            )?;
            let needs_spoke_evidence: bool = tx.query_row(
                "SELECT COALESCE((SELECT transport FROM host_transports WHERE host_id = ?1), 'legacy_ssh') <> 'spoke_ready'",
                [host_id],
                |row| row.get(0),
            )?;
            if !owned
                // The legacy fence guards new work only: preserved work and
                // bookmarks on a resolved spoke generation stay recoverable
                // through the stream after a legacy handoff.
                || (start && legacy_binding(&tx, &run.execution_id)?.is_some())
                || (needs_spoke_evidence
                    && !assignment_committed_to_spoke(&tx, &run.execution_id, run.assignment_generation as i64)?)
            {
                return Err(fault(HostOutboxFault::Denied));
            }
        }
        let (unsettled, bytes): (i64, i64) = tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(wire)), 0) FROM host_commands
             WHERE host_id = ?1 AND result IS NULL",
            [host_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let sequence = highest_sequence(&tx, host_id)? + 1;
        let wire = encode(sequence);
        if unsettled >= MAX_UNSETTLED_HOST_COMMANDS || bytes + wire.len() as i64 > MAX_UNSETTLED_HOST_COMMAND_BYTES {
            return Err(fault(HostOutboxFault::Full));
        }
        tx.execute(
            "INSERT INTO host_commands
                (host_id, command_id, sequence, run_execution_id, run_generation, run_id, spawn_token,
                 request, wire, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                host_id,
                command_id,
                sequence as i64,
                run.map(|r| r.execution_id.as_str()),
                run.map(|r| r.assignment_generation as i64),
                run.map(|r| r.run_id.as_str()),
                run.map(|r| r.spawn_token.as_str()),
                request,
                wire,
                now_ms()
            ],
        )?;
        tx.commit()?;
        Ok(StoredHostCommand { sequence, wire })
    }

    /// The stored command with wire sequence `after + 1`, if any.
    pub fn host_command_after(&self, host_id: &str, after: u64) -> Result<Option<StoredHostCommand>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT sequence, wire FROM host_commands
                 WHERE host_id = ?1 AND sequence > ?2 ORDER BY sequence LIMIT 1",
                params![host_id, after as i64],
                |row| {
                    Ok(StoredHostCommand {
                        sequence: row.get::<_, i64>(0)? as u64,
                        wire: row.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// Settle an unreceipted command with a terminal result and close the hole
    /// it leaves: every later command moves down one wire sequence, re-encoded
    /// by `reseq`. Returns the command id and the stored result. A sequence the
    /// peer acknowledged, or that names no live command, is refused. A missing
    /// receipt does not prove it was never delivered on an earlier stream.
    pub fn reject_host_command(
        &self,
        host_id: &str,
        sequence: u64,
        result_for: &dyn Fn(&str) -> Vec<u8>,
        reseq: Resequence<'_>,
    ) -> Result<(String, Vec<u8>)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let received: i64 = tx
            .query_row(
                "SELECT received FROM host_command_journals WHERE host_id = ?1",
                [host_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let command_id: Option<String> = tx
            .query_row(
                "SELECT command_id FROM host_commands
                 WHERE host_id = ?1 AND sequence = ?2 AND result IS NULL",
                params![host_id, sequence as i64],
                |row| row.get(0),
            )
            .optional()?;
        let Some(command_id) = command_id.filter(|_| sequence as i64 > received) else {
            return Err(fault(HostOutboxFault::Denied));
        };
        let result = result_for(&command_id);
        tx.execute(
            "UPDATE host_commands SET sequence = NULL, result = ?3, settled_at_ms = ?4
             WHERE host_id = ?1 AND command_id = ?2",
            params![host_id, command_id, result, now_ms()],
        )?;
        let later: Vec<(String, i64, Vec<u8>)> = {
            let mut statement = tx.prepare(
                "SELECT command_id, sequence, wire FROM host_commands
                 WHERE host_id = ?1 AND sequence > ?2 ORDER BY sequence",
            )?;
            statement
                .query_map(params![host_id, sequence as i64], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?
                .collect::<rusqlite::Result<_>>()?
        };
        for (id, old, wire) in later {
            tx.execute(
                "UPDATE host_commands SET sequence = ?3, wire = ?4 WHERE host_id = ?1 AND command_id = ?2",
                params![host_id, id, old - 1, reseq(&wire, (old - 1) as u64)?],
            )?;
        }
        tx.commit()?;
        Ok((command_id, result))
    }

    /// Remove settled commands that nothing can still need, in one transaction,
    /// and return how many went. A command is kept when it:
    ///
    /// * has no result yet, which includes every unresolved lease or spawn;
    /// * has a wire sequence the peer has not durably received, or the highest
    ///   sequence stored (allocation is max(stored sequence, durable receipt)
    ///   and never reuses a sequence);
    /// * belongs to an assignment that is still unresolved, whose commands are
    ///   the evidence it may have started;
    /// * settled at or after `settled_before_ms`, or is among the `keep_recent`
    ///   newest settled commands, so a retry of a recent id still finds its result.
    ///
    /// Once a command is removed its id is unknown: a late retry of that id is a
    /// new command, which is why the window is long ([`SETTLED_HOST_COMMAND_RETENTION_MS`]).
    pub fn prune_settled_host_commands(
        &self,
        host_id: &str,
        settled_before_ms: i64,
        keep_recent: u32,
    ) -> Result<usize> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let received: i64 = tx
            .query_row(
                "SELECT received FROM host_command_journals WHERE host_id = ?1",
                [host_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let highest = highest_sequence(&tx, host_id)? as i64;
        let removed = tx.execute(
            "DELETE FROM host_commands
             WHERE host_id = ?1 AND result IS NOT NULL AND settled_at_ms < ?2
               AND (sequence IS NULL OR (sequence <= ?3 AND sequence < ?4))
               AND NOT (run_execution_id IS NOT NULL AND EXISTS(
                    SELECT 1 FROM spoke_assignments a
                     WHERE a.execution_id = host_commands.run_execution_id
                       AND a.assignment_generation = host_commands.run_generation
                       AND a.resolved_at_ms IS NULL))
               AND command_id NOT IN (
                    SELECT command_id FROM host_commands
                     WHERE host_id = ?1 AND result IS NOT NULL
                     ORDER BY settled_at_ms DESC, created_at_ms DESC, command_id DESC LIMIT ?5)",
            params![host_id, settled_before_ms, received, highest, keep_recent as i64],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    /// The stored wire bytes of a command, whether or not it has settled.
    pub fn host_command_wire(&self, host_id: &str, command_id: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT wire FROM host_commands WHERE host_id = ?1 AND command_id = ?2",
                params![host_id, command_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// The terminal result stored for a command, for a retry of the same id.
    pub fn host_command_result(&self, host_id: &str, command_id: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT result FROM host_commands WHERE host_id = ?1 AND command_id = ?2",
                params![host_id, command_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Store a command's terminal result. The command must exist and `run` must
    /// match the run it was issued for, so a result cannot complete another
    /// run's command. Repeating identical bytes is a no-op; different bytes
    /// are refused. The ingress transaction calls this next to its receipt.
    pub fn settle_host_command(
        &self,
        host_id: &str,
        command_id: &str,
        run: Option<&SpokeRunKey>,
        result: &[u8],
    ) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        settle_host_command_in_tx(&tx, host_id, command_id, run, result)?;
        tx.commit()?;
        Ok(())
    }
}

/// Persist a terminal result inside an already-open transaction. Ingress
/// commits the observation receipt in the same transaction so a crash cannot
/// acknowledge a result the outbox cannot later query.
pub(crate) fn settle_host_command_in_tx(
    tx: &rusqlite::Transaction<'_>,
    host_id: &str,
    command_id: &str,
    run: Option<&SpokeRunKey>,
    result: &[u8],
) -> Result<()> {
    type Stored = (
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<Vec<u8>>,
    );
    let stored: Option<Stored> = tx
        .query_row(
            "SELECT run_execution_id, run_generation, run_id, spawn_token, result
             FROM host_commands WHERE host_id = ?1 AND command_id = ?2",
            params![host_id, command_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()?;
    let Some((execution_id, generation, run_id, token, settled)) = stored else {
        return Err(fault(HostOutboxFault::Denied));
    };
    let stored_run = match (execution_id, generation, run_id, token) {
        (Some(execution_id), Some(generation), Some(run_id), Some(spawn_token)) => Some(SpokeRunKey {
            execution_id,
            assignment_generation: generation as u64,
            run_id,
            spawn_token,
        }),
        _ => None,
    };
    if stored_run.as_ref() != run {
        return Err(fault(HostOutboxFault::Denied));
    }
    match settled {
        Some(previous) if previous == result => return Ok(()),
        Some(_) => return Err(fault(HostOutboxFault::Denied)),
        None => {}
    }
    tx.execute(
        "UPDATE host_commands SET result = ?3, settled_at_ms = ?4 WHERE host_id = ?1 AND command_id = ?2",
        params![host_id, command_id, result, now_ms()],
    )?;
    Ok(())
}

fn unique_open_assignment(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    host_id: &str,
    kind: &'static str,
    value: &str,
) -> Result<Option<SpokeAssignment>> {
    let mut statement = conn.prepare(sql)?;
    let mut rows = statement.query_map(params, map_assignment)?;
    let first = rows.next().transpose()?;
    if first.is_some() && rows.next().transpose()?.is_some() {
        return Err(AmbiguousSpokeAssignment {
            host_id: host_id.to_owned(),
            kind,
            value: value.to_owned(),
        }
        .into());
    }
    Ok(first)
}

fn highest_sequence(conn: &Connection, host_id: &str) -> Result<u64> {
    Ok(conn.query_row(
        "SELECT MAX(
            COALESCE((SELECT MAX(sequence) FROM host_commands WHERE host_id = ?1), 0),
            COALESCE((SELECT received FROM host_command_journals WHERE host_id = ?1), 0)
         )",
        [host_id],
        |row| row.get::<_, i64>(0),
    )? as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).expect("open in-memory work db")
    }

    /// A db whose test hosts route through the spoke; a host with no transport
    /// row is legacy and does not admit run-bound commands.
    fn spoke_db() -> WorkDb {
        let db = db();
        for host in ["host-a", "host-b", "h"] {
            db.set_host_transport(host, HostTransport::SpokeReady).unwrap();
        }
        db
    }

    fn key(assignment: &SpokeAssignment) -> SpokeRunKey {
        SpokeRunKey {
            execution_id: assignment.execution_id.clone(),
            assignment_generation: assignment.assignment_generation,
            run_id: assignment.run_id.clone(),
            spawn_token: assignment.spawn_token.clone(),
        }
    }

    fn append(db: &WorkDb, host: &str, id: &str, run: Option<&SpokeRunKey>, body: &[u8]) -> Result<StoredHostCommand> {
        append_with(db, host, id, run, RunAdmission::Start, body)
    }

    fn append_with(
        db: &WorkDb,
        host: &str,
        id: &str,
        run: Option<&SpokeRunKey>,
        admission: RunAdmission,
        body: &[u8],
    ) -> Result<StoredHostCommand> {
        db.append_host_command(host, id, run, admission, body, &|sequence| {
            let mut wire = sequence.to_be_bytes().to_vec();
            wire.extend_from_slice(body);
            wire
        })
    }

    fn fault_of(error: &anyhow::Error) -> Option<HostOutboxFault> {
        error.downcast_ref::<HostOutboxFault>().copied()
    }

    fn legacy_execution(db: &WorkDb, host: &str, execution_id: &str, lease: &str) {
        db.connect()
            .unwrap()
            .execute(
                "INSERT INTO work_executions
                    (id, work_item_id, kind, status, repo_remote_url, created_at, host_id, cube_lease_id)
                 VALUES (?1, 'chore_1', 'task_implementation', 'running', 'https://example.test/r', '1', ?2, ?3)",
                (execution_id, host, lease),
            )
            .unwrap();
    }

    fn held_by(error: &anyhow::Error) -> Option<String> {
        error
            .downcast_ref::<SpokeHeldByOtherHost>()
            .map(|held| held.held_by.clone())
    }

    fn legacy_held_by(error: &anyhow::Error) -> Option<String> {
        error
            .downcast_ref::<LegacyDestinationHeld>()
            .map(|held| held.held_by.clone())
    }

    #[test]
    fn claiming_a_legacy_destination_retires_another_hosts_idle_reservation() {
        let db = spoke_db();
        let reserved = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        assert_eq!(
            db.claim_legacy_destination("exec_1", "ssh-host", HostTransport::LegacySsh)
                .unwrap(),
            None
        );
        // Retired as never started, it is no longer what the execution routes to.
        assert_eq!(db.spoke_assignment_for_execution("exec_1").unwrap(), None);
        assert!(!db.host_has_open_spoke_assignment("host-a").unwrap());
        // The retired reservation is fenced out of admission.
        let stale = append(&db, "host-a", "lease-1", Some(&key(&reserved)), b"lease");
        assert_eq!(fault_of(&stale.unwrap_err()), Some(HostOutboxFault::Denied));
        // The claim is the new owner: a spoke reservation on any host, the old
        // one included, is refused while it stands, and so is another claim.
        assert_eq!(
            db.legacy_destination_host("exec_1").unwrap().as_deref(),
            Some("ssh-host")
        );
        for host in ["host-a", "host-b"] {
            let refused = db.reserve_spoke_assignment("exec_1", host).unwrap_err();
            assert_eq!(legacy_held_by(&refused).as_deref(), Some("ssh-host"), "{host}");
        }
        let refused = db
            .claim_legacy_destination("exec_1", "other-ssh", HostTransport::LegacySsh)
            .unwrap_err();
        assert_eq!(legacy_held_by(&refused).as_deref(), Some("ssh-host"));
        // The same destination asking again is idempotent.
        assert_eq!(
            db.claim_legacy_destination("exec_1", "ssh-host", HostTransport::LegacySsh)
                .unwrap(),
            None
        );
        // Abandoning the handoff releases the execution to a spoke again.
        assert!(db.release_legacy_destination("exec_1", "ssh-host").unwrap());
        assert!(!db.release_legacy_destination("exec_1", "ssh-host").unwrap());
        assert_eq!(
            db.reserve_spoke_assignment("exec_1", "host-b")
                .unwrap()
                .assignment_generation,
            2
        );
    }

    #[test]
    fn a_started_reservation_refuses_the_move_and_an_unknown_execution_is_claimed() {
        let db = spoke_db();
        db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        db.bind_spoke_lease("exec_1", 1, "ws-a", "lease-a", "/a").unwrap();
        let refused = db
            .claim_legacy_destination("exec_1", "ssh-host", HostTransport::LegacySsh)
            .unwrap_err();
        assert_eq!(held_by(&refused).as_deref(), Some("host-a"));
        assert_eq!(db.legacy_destination_host("exec_1").unwrap(), None);
        // The holder itself keeps its spoke run.
        let kept = db
            .claim_legacy_destination("exec_1", "host-a", HostTransport::LegacySsh)
            .unwrap()
            .expect("a committed spoke run stays on its host");
        assert_eq!(kept.assignment_generation, 1);
        assert_eq!(db.legacy_destination_host("exec_1").unwrap(), None);
        // A flag with nothing stored is genuinely uncertain: refused.
        db.reserve_spoke_assignment("exec_2", "host-a").unwrap();
        db.set_spoke_in_doubt("exec_2", 1, true).unwrap();
        let refused = db
            .claim_legacy_destination("exec_2", "host-a", HostTransport::LegacySsh)
            .unwrap_err();
        assert_eq!(held_by(&refused).as_deref(), Some("host-a"));
        // An execution nobody holds is simply claimed.
        assert_eq!(
            db.claim_legacy_destination("never_seen", "ssh-host", HostTransport::LegacySsh)
                .unwrap(),
            None
        );
        assert_eq!(
            db.legacy_destination_host("never_seen").unwrap().as_deref(),
            Some("ssh-host")
        );
    }

    #[test]
    fn an_unused_reservation_does_not_survive_its_host_leaving_the_spoke() {
        for transport in [HostTransport::LegacySsh, HostTransport::SpokePending] {
            let db = spoke_db();
            db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
            // Nothing leased and nothing stored, so the host may leave the spoke.
            db.set_host_transport("host-a", transport).unwrap();
            assert_eq!(
                db.claim_legacy_destination("exec_1", "host-a", transport).unwrap(),
                None,
                "{transport:?}"
            );
            assert_eq!(db.spoke_assignment_for_execution("exec_1").unwrap(), None);
            assert_eq!(db.legacy_destination_host("exec_1").unwrap().as_deref(), Some("host-a"));
            // A started one is kept, and the commands it admitted stay addressable.
            db.reserve_spoke_assignment("exec_2", "host-b").unwrap();
            let run = key(&db.spoke_assignment_for_execution("exec_2").unwrap().unwrap());
            append(&db, "host-b", "lease-2", Some(&run), b"lease").unwrap();
            if transport == HostTransport::LegacySsh {
                db.set_host_transport("host-b", transport).unwrap_err();
            }
        }
    }

    #[test]
    fn a_lease_held_on_another_host_refuses_every_other_destination() {
        let db = spoke_db();
        legacy_execution(&db, "ssh-host", "exec_1", "ssh-lease");
        assert_eq!(
            db.legacy_destination_host("exec_1").unwrap().as_deref(),
            Some("ssh-host")
        );
        let refused = db.reserve_spoke_assignment("exec_1", "host-a").unwrap_err();
        assert_eq!(legacy_held_by(&refused).as_deref(), Some("ssh-host"));
        let refused = db
            .claim_legacy_destination("exec_1", "other-ssh", HostTransport::LegacySsh)
            .unwrap_err();
        assert_eq!(legacy_held_by(&refused).as_deref(), Some("ssh-host"));
        // The lease's own host keeps it; a claim is not what binds it.
        assert_eq!(
            db.claim_legacy_destination("exec_1", "ssh-host", HostTransport::LegacySsh)
                .unwrap(),
            None
        );
        assert!(!db.release_legacy_destination("exec_1", "other-ssh").unwrap());
    }

    #[test]
    fn transcripts_are_legacy_only_when_a_legacy_run_on_the_host_recorded_them() {
        let db = spoke_db();
        let run = |id: &str, execution: &str, host: &str, path: &str| {
            db.connect()
                .unwrap()
                .execute(
                    "INSERT INTO work_runs (id, execution_id, agent_id, status, transcript_path, created_at, host_id)
                     VALUES (?1, ?2, 'agent', 'running', ?3, '1', ?4)",
                    (id, execution, path, host),
                )
                .unwrap();
        };
        legacy_execution(&db, "host-a", "exec_legacy", "ssh-lease");
        run(
            "run_legacy",
            "exec_legacy",
            "host-a",
            "/home/u/.claude/projects/-ws-a-1/legacy.jsonl",
        );
        // A host with no spoke history reads everything over the legacy transport.
        assert!(db.transcript_is_legacy("host-a", "/anywhere/else.jsonl").unwrap());

        db.reserve_spoke_assignment("exec_spoke", "host-a").unwrap();
        db.bind_spoke_lease("exec_spoke", 1, "ws", "spoke-lease", "/ws/a-1")
            .unwrap();
        legacy_execution(&db, "host-a", "exec_spoke", "spoke-lease");
        for path in [
            // Per-run and durable layouts and an encoding that rewrites '_' name
            // no workspace path, and a prefix of a legacy workspace is not it.
            "/state/executions/run-1/transcripts/codex/sessions/rollout.jsonl",
            "/state/grok/runs/run-1/home/sessions/s.jsonl",
            "/home/u/.claude/projects/-ws-a_1/s.jsonl",
            "/home/u/.claude/projects/-ws-a-10/s.jsonl",
            "/ws/a-10/transcript.jsonl",
        ] {
            assert!(!db.transcript_is_legacy("host-a", path).unwrap(), "{path}");
        }
        // The genuine legacy transcript is still the legacy transport's.
        assert!(
            db.transcript_is_legacy("host-a", "/home/u/.claude/projects/-ws-a-1/legacy.jsonl")
                .unwrap()
        );
        // A recorded transcript of a run that did have a spoke assignment is not.
        run("run_spoke", "exec_spoke", "host-a", "/recorded/spoke.jsonl");
        assert!(!db.transcript_is_legacy("host-a", "/recorded/spoke.jsonl").unwrap());
        // A reservation retired unused leaves a legacy run's transcript legacy.
        db.reserve_spoke_assignment("exec_moved", "host-b").unwrap();
        db.claim_legacy_destination("exec_moved", "host-c", HostTransport::LegacySsh)
            .unwrap();
        assert!(db.transcript_is_legacy("host-b", "/idle/host.jsonl").unwrap());
    }

    fn backdate_claim(db: &WorkDb, execution_id: &str) {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE legacy_destinations SET claimed_at_ms = 0 WHERE execution_id = ?1",
                [execution_id],
            )
            .unwrap();
    }

    #[test]
    fn recovery_for_a_resolved_generation_is_admitted_after_a_legacy_handoff() {
        let db = spoke_db();
        let assignment = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        let run = key(&assignment);
        append(&db, "host-a", "lease-1", Some(&run), b"lease").unwrap();
        db.resolve_spoke_assignment("exec_1", 1, "released").unwrap();
        // A legacy destination on another host claims the execution.
        assert_eq!(
            db.claim_legacy_destination("exec_1", "ssh-host", HostTransport::LegacySsh)
                .unwrap(),
            None
        );
        let check = append_with(&db, "host-a", "check-1", Some(&run), RunAdmission::Recovery, b"check");
        assert!(check.is_ok(), "{check:?}");
        let spawn = append(&db, "host-a", "lease-2", Some(&run), b"lease");
        assert_eq!(fault_of(&spawn.unwrap_err()), Some(HostOutboxFault::Denied));
    }

    #[test]
    fn a_claim_whose_destination_never_leased_is_replaceable() {
        let db = spoke_db();
        db.claim_legacy_destination("exec_1", "ssh-a", HostTransport::LegacySsh)
            .unwrap();
        // Recent, so the destination may still be leasing.
        let refused = db
            .claim_legacy_destination("exec_1", "ssh-b", HostTransport::LegacySsh)
            .unwrap_err();
        assert_eq!(legacy_held_by(&refused).as_deref(), Some("ssh-a"));
        backdate_claim(&db, "exec_1");
        assert_eq!(db.legacy_destination_host("exec_1").unwrap(), None);
        assert_eq!(
            db.claim_legacy_destination("exec_1", "ssh-b", HostTransport::LegacySsh)
                .unwrap(),
            None
        );
        assert_eq!(db.legacy_destination_host("exec_1").unwrap().as_deref(), Some("ssh-b"));
    }

    #[test]
    fn an_old_claim_with_a_run_on_its_host_keeps_binding() {
        let db = spoke_db();
        db.claim_legacy_destination("exec_1", "ssh-a", HostTransport::LegacySsh)
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at, host_id)
             VALUES ('exec_1', 'chore_1', 'task_implementation', 'running', 'https://example.test/r', '1', 'ssh-a')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, host_id)
             VALUES ('run_1', 'exec_1', 'agent', 'running', '1', 'ssh-a')",
            [],
        )
        .unwrap();
        drop(conn);
        backdate_claim(&db, "exec_1");
        assert_eq!(db.legacy_destination_host("exec_1").unwrap().as_deref(), Some("ssh-a"));
        let refused = db.reserve_spoke_assignment("exec_1", "host-a").unwrap_err();
        assert_eq!(legacy_held_by(&refused).as_deref(), Some("ssh-a"));
        // The dispatch action finishing does not imply the worker has ended.
        let conn = db.connect().unwrap();
        conn.execute("UPDATE work_runs SET status = 'completed' WHERE id = 'run_1'", [])
            .unwrap();
        drop(conn);
        assert_eq!(db.legacy_destination_host("exec_1").unwrap().as_deref(), Some("ssh-a"));
        // A terminal execution with no lease must not stay bound by its history.
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = 'exec_1'",
            [],
        )
        .unwrap();
        drop(conn);
        assert!(db.legacy_destination_host("exec_1").unwrap().is_none());
        db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
    }

    #[test]
    fn a_claim_made_while_the_host_was_pending_yields_to_the_ready_spoke() {
        let db = spoke_db();
        db.set_host_transport("host-p", HostTransport::SpokePending).unwrap();
        db.claim_legacy_destination("exec_1", "host-p", HostTransport::SpokePending)
            .unwrap();
        assert_eq!(db.legacy_destination_host("exec_1").unwrap().as_deref(), Some("host-p"));
        db.set_host_transport("host-p", HostTransport::SpokeReady).unwrap();
        assert_eq!(db.legacy_destination_host("exec_1").unwrap(), None);
        let assignment = db.reserve_spoke_assignment("exec_1", "host-p").unwrap();
        assert_eq!(assignment.host_id, "host-p");
    }

    #[test]
    fn releasing_the_lease_ends_the_legacy_claim() {
        let db = spoke_db();
        db.claim_legacy_destination("exec_1", "ssh-host", HostTransport::LegacySsh)
            .unwrap();
        legacy_execution(&db, "ssh-host", "exec_1", "ssh-lease");
        assert!(db.claim_execution_workspace_release("exec_1", "ssh-lease").unwrap());
        assert_eq!(db.legacy_destination_host("exec_1").unwrap(), None);
    }

    #[test]
    fn new_work_is_admitted_only_while_the_assignment_is_open() {
        let db = spoke_db();
        let assignment = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        let run = key(&assignment);
        let admit = |id: &str, start: bool| {
            db.append_host_command(
                "host-a",
                id,
                Some(&run),
                if start {
                    RunAdmission::Start
                } else {
                    RunAdmission::Recovery
                },
                id.as_bytes(),
                &|n| n.to_be_bytes().to_vec(),
            )
        };
        admit("lease-1", true).unwrap();
        db.resolve_spoke_assignment("exec_1", 1, "released").unwrap();
        assert_eq!(
            fault_of(&admit("lease-2", true).unwrap_err()),
            Some(HostOutboxFault::Denied)
        );
        // Recovery and teardown stay addressable on the resolved generation, and
        // a command already stored is returned to its retry.
        admit("recovery-1", false).unwrap();
        admit("lease-1", true).unwrap();
    }

    #[test]
    fn an_unresolved_assignment_is_never_handed_to_a_second_host() {
        let db = db();
        let first = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        assert_eq!(db.reserve_spoke_assignment("exec_1", "host-a").unwrap(), first);
        // Holding a lease is what pins the execution to its host.
        db.bind_spoke_lease("exec_1", 1, "ws-a", "lease-a", "/a").unwrap();
        let refused = db.reserve_spoke_assignment("exec_1", "host-b").unwrap_err();
        assert_eq!(
            refused.downcast_ref::<SpokeHeldByOtherHost>().unwrap().held_by,
            "host-a"
        );
        assert!(db.resolve_spoke_assignment("exec_1", 1, "released").unwrap());
        let next = db.reserve_spoke_assignment("exec_1", "host-b").unwrap();
        assert_eq!(next.assignment_generation, 2);
        assert_ne!(next.spawn_token, first.spawn_token);
        assert_ne!(next.run_id, first.run_id);
        // The resolved attempt stays addressable by its lease for late recovery.
        assert_eq!(
            db.spoke_assignment_for_lease("host-a", "lease-a")
                .unwrap()
                .unwrap()
                .execution_id,
            "exec_1"
        );
        db.bind_spoke_lease("exec_1", 2, "ws", "lease", "/w").unwrap();
        assert_eq!(
            db.spoke_assignment_for_lease("host-b", "lease")
                .unwrap()
                .unwrap()
                .execution_id,
            "exec_1"
        );
    }

    #[test]
    fn a_reservation_that_never_started_anything_does_not_pin_the_execution() {
        let db = db();
        db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        db.set_spoke_in_doubt("exec_1", 1, true).unwrap();
        assert!(db.reserve_spoke_assignment("exec_1", "host-b").is_err());
        db.set_spoke_in_doubt("exec_1", 1, false).unwrap();
        let moved = db.reserve_spoke_assignment("exec_1", "host-b").unwrap();
        assert_eq!((moved.host_id.as_str(), moved.assignment_generation), ("host-b", 2));
        assert!(db.spoke_assignment_for_execution("exec_1").unwrap().unwrap().host_id == "host-b");
        // A lease pins it regardless of the doubt flag.
        db.bind_spoke_lease("exec_1", 2, "ws", "lease", "/w").unwrap();
        assert!(db.reserve_spoke_assignment("exec_1", "host-a").is_err());
        assert_eq!(db.host_transport("host-a").unwrap(), HostTransport::LegacySsh);
        db.set_host_transport("host-a", HostTransport::SpokeReady).unwrap();
        assert_eq!(db.host_transport("host-a").unwrap(), HostTransport::SpokeReady);
    }

    #[test]
    fn commands_need_an_owned_run_and_keep_their_identity() {
        let db = spoke_db();
        let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        let run = key(&owned);
        let stored = append(&db, "host-a", "c1", Some(&run), b"one").unwrap();
        assert_eq!(stored.sequence, 1);
        assert_eq!(append(&db, "host-a", "c1", Some(&run), b"one").unwrap(), stored);
        let changed = append(&db, "host-a", "c1", Some(&run), b"other").unwrap_err();
        assert_eq!(fault_of(&changed), Some(HostOutboxFault::Denied));
        // Another host cannot carry this run, and an unknown run cannot be sent at all.
        let foreign = append(&db, "host-b", "c2", Some(&run), b"two").unwrap_err();
        assert_eq!(fault_of(&foreign), Some(HostOutboxFault::Denied));
        let mut unknown = run.clone();
        unknown.spawn_token = "forged".into();
        let forged = append(&db, "host-a", "c3", Some(&unknown), b"three").unwrap_err();
        assert_eq!(fault_of(&forged), Some(HostOutboxFault::Denied));
    }

    #[test]
    fn results_settle_once_and_only_for_the_issuing_run() {
        let db = spoke_db();
        let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        let run = key(&owned);
        append(&db, "host-a", "c1", Some(&run), b"one").unwrap();
        let mut other = run.clone();
        other.assignment_generation = 9;
        assert_eq!(
            fault_of(&db.settle_host_command("host-a", "c1", Some(&other), b"r").unwrap_err()),
            Some(HostOutboxFault::Denied)
        );
        assert_eq!(db.host_command_result("host-a", "c1").unwrap(), None);
        db.settle_host_command("host-a", "c1", Some(&run), b"r").unwrap();
        db.settle_host_command("host-a", "c1", Some(&run), b"r").unwrap();
        assert_eq!(
            fault_of(
                &db.settle_host_command("host-a", "c1", Some(&run), b"different")
                    .unwrap_err()
            ),
            Some(HostOutboxFault::Denied)
        );
        assert_eq!(
            db.host_command_result("host-a", "c1").unwrap().as_deref(),
            Some(&b"r"[..])
        );
    }

    #[test]
    fn rejecting_an_undelivered_command_closes_the_sequence_hole() {
        let db = db();
        for (id, body) in [("a", b"1"), ("b", b"2"), ("c", b"3")] {
            append(&db, "h", id, None, body).unwrap();
        }
        let (journal, _) = db.resume_host_command_journal("h", None).unwrap();
        db.record_host_command_receipt("h", &journal, 1).unwrap();
        let result = |id: &str| id.as_bytes().to_vec();
        let reseq = |wire: &[u8], sequence: u64| {
            let mut out = sequence.to_be_bytes().to_vec();
            out.extend_from_slice(&wire[8..]);
            Ok(out)
        };
        // Sequence 1 may have reached the peer, so it cannot be rejected.
        assert_eq!(
            fault_of(&db.reject_host_command("h", 1, &result, &reseq).unwrap_err()),
            Some(HostOutboxFault::Denied)
        );
        let (id, stored) = db.reject_host_command("h", 2, &result, &reseq).unwrap();
        assert_eq!((id.as_str(), stored.as_slice()), ("b", &b"b"[..]));
        let next = db.host_command_after("h", 1).unwrap().unwrap();
        assert_eq!(next.sequence, 2);
        assert_eq!(&next.wire[8..], b"3");
        assert_eq!(db.host_command_result("h", "b").unwrap().as_deref(), Some(&b"b"[..]));
        // Receipt cannot name a command that does not exist.
        assert_eq!(
            fault_of(&db.record_host_command_receipt("h", &journal, 3).unwrap_err()),
            Some(HostOutboxFault::ReplayGap)
        );
    }

    #[test]
    fn a_peer_that_remembers_a_journal_the_hub_lost_is_a_replay_gap() {
        let db = db();
        let (journal, received) = db.resume_host_command_journal("h", None).unwrap();
        assert_eq!(received, 0);
        assert_eq!(
            db.resume_host_command_journal("h", Some((&journal, 0))).unwrap().0,
            journal
        );
        assert_eq!(
            fault_of(&db.resume_host_command_journal("h", Some(("other", 0))).unwrap_err()),
            Some(HostOutboxFault::ReplayGap)
        );
        assert_eq!(
            fault_of(
                &db.resume_host_command_journal("new-host", Some(("remembered", 0)))
                    .unwrap_err()
            ),
            Some(HostOutboxFault::ReplayGap)
        );
    }

    fn lease(db: &WorkDb, exec: &str, generation: u64, lease: &str) {
        db.bind_spoke_lease(exec, generation, "ws", lease, "/w").unwrap();
    }

    #[test]
    fn a_superseded_attempt_is_not_admitted_but_the_latest_stays_addressable() {
        let db = spoke_db();
        let first = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        let stale = key(&first);
        append(&db, "host-a", "c1", Some(&stale), b"lease").unwrap();
        db.settle_host_command("host-a", "c1", Some(&stale), b"r").unwrap();
        lease(&db, "exec_1", 1, "lease-a");
        // The newest generation stays admissible after it resolves, for recovery.
        assert!(db.resolve_spoke_assignment("exec_1", 1, "released").unwrap());
        append_with(&db, "host-a", "c2", Some(&stale), RunAdmission::Recovery, b"restore").unwrap();
        let second = db.reserve_spoke_assignment("exec_1", "host-b").unwrap();
        // Once a newer attempt exists the old key is stale, even on its own host.
        let refused = append(&db, "host-a", "c3", Some(&stale), b"late").unwrap_err();
        assert_eq!(fault_of(&refused), Some(HostOutboxFault::Denied));
        append(&db, "host-b", "c4", Some(&key(&second)), b"go").unwrap();
    }

    #[test]
    fn a_reservation_with_a_command_in_flight_still_pins_the_execution() {
        let db = spoke_db();
        let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        let run = key(&owned);
        // The lease command is stored but its doubt flag was never set.
        append(&db, "host-a", "lease", Some(&run), b"lease").unwrap();
        let held = db.reserve_spoke_assignment("exec_1", "host-b").unwrap_err();
        assert!(held.downcast_ref::<SpokeHeldByOtherHost>().is_some());
        // Settled by the host: it may have run, so it still pins the execution.
        db.settle_host_command("host-a", "lease", Some(&run), b"ok").unwrap();
        assert!(db.reserve_spoke_assignment("exec_1", "host-b").is_err());
    }

    #[test]
    fn quota_counts_encoded_wire_bytes() {
        let db = db();
        let error = db
            .append_host_command("h", "large", None, RunAdmission::Start, b"small", &|_| {
                vec![0; MAX_UNSETTLED_HOST_COMMAND_BYTES as usize + 1]
            })
            .unwrap_err();
        assert_eq!(fault_of(&error), Some(HostOutboxFault::Full));
        assert!(db.host_command_after("h", 0).unwrap().is_none());
    }

    #[test]
    fn a_rejected_command_that_was_offered_still_pins_the_execution() {
        let db = spoke_db();
        let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        append(&db, "host-a", "lease", Some(&key(&owned)), b"lease").unwrap();
        let result = |id: &str| id.as_bytes().to_vec();
        let reseq = |wire: &[u8], _: u64| Ok(wire.to_vec());
        assert!(db.host_command_after("host-a", 0).unwrap().is_some());
        db.reject_host_command("host-a", 1, &result, &reseq).unwrap();
        assert!(db.reserve_spoke_assignment("exec_1", "host-b").is_err());
    }

    #[test]
    fn in_doubt_alone_does_not_admit_a_command_on_a_legacy_host() {
        let db = db();
        db.set_host_transport("host-a", HostTransport::SpokeReady).unwrap();
        let idle = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        db.set_host_transport("host-a", HostTransport::LegacySsh).unwrap();
        db.set_spoke_in_doubt("exec_1", 1, true).unwrap();
        assert_eq!(
            fault_of(&append(&db, "host-a", "lease", Some(&key(&idle)), b"lease").unwrap_err()),
            Some(HostOutboxFault::Denied)
        );
        // A host with no transport row is legacy too.
        let other = db.reserve_spoke_assignment("exec_2", "host-b").unwrap();
        assert_eq!(
            fault_of(&append(&db, "host-b", "lease", Some(&key(&other)), b"lease").unwrap_err()),
            Some(HostOutboxFault::Denied)
        );
    }

    #[test]
    fn a_host_with_running_spoke_work_cannot_be_moved_back_to_ssh() {
        let db = db();
        db.set_host_transport("host-a", HostTransport::SpokeReady).unwrap();
        // A reservation that started nothing does not block the downgrade.
        let idle = db.reserve_spoke_assignment("exec_idle", "host-a").unwrap();
        db.set_host_transport("host-a", HostTransport::SpokePending).unwrap();
        db.set_host_transport("host-a", HostTransport::LegacySsh).unwrap();
        assert_eq!(
            fault_of(&append(&db, "host-a", "late-lease", Some(&key(&idle)), b"lease").unwrap_err()),
            Some(HostOutboxFault::Denied)
        );
        db.set_host_transport("host-a", HostTransport::SpokeReady).unwrap();
        db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        lease(&db, "exec_1", 1, "lease-a");
        let refused = db.set_host_transport("host-a", HostTransport::LegacySsh).unwrap_err();
        assert_eq!(refused.downcast_ref::<SpokeHostInUse>().unwrap().execution_id, "exec_1");
        assert_eq!(db.host_transport("host-a").unwrap(), HostTransport::SpokeReady);
        // Pending does not flip the host back to SSH, so a running spoke assignment may stay.
        db.set_host_transport("host-a", HostTransport::SpokePending).unwrap();
        // Another host is unaffected.
        db.set_host_transport("host-b", HostTransport::LegacySsh).unwrap();
        db.resolve_spoke_assignment("exec_1", 1, "released").unwrap();
        db.resolve_spoke_assignment("exec_idle", 1, "released").unwrap();
        db.set_host_transport("host-a", HostTransport::LegacySsh).unwrap();
    }

    fn ids(db: &WorkDb, host: &str, all: &[&str]) -> Vec<String> {
        all.iter()
            .filter(|id| db.host_command_wire(host, id).unwrap().is_some())
            .map(|id| (*id).to_owned())
            .collect()
    }

    #[test]
    fn retention_keeps_what_receipts_and_open_assignments_still_need() {
        let db = spoke_db();
        let owned = db.reserve_spoke_assignment("exec_1", "h").unwrap();
        let run = key(&owned);
        let names = ["a", "b", "c", "d", "unsettled", "run"];
        append(&db, "h", "a", None, b"1").unwrap();
        append(&db, "h", "b", None, b"2").unwrap();
        append(&db, "h", "c", None, b"3").unwrap();
        append(&db, "h", "d", None, b"4").unwrap();
        append(&db, "h", "unsettled", None, b"5").unwrap();
        append(&db, "h", "run", Some(&run), b"6").unwrap();
        for id in ["a", "b", "c", "d"] {
            db.settle_host_command("h", id, None, b"r").unwrap();
        }
        db.settle_host_command("h", "run", Some(&run), b"r").unwrap();
        let (journal, _) = db.resume_host_command_journal("h", None).unwrap();
        // The peer has durably received a, b and c only.
        db.record_host_command_receipt("h", &journal, 3).unwrap();
        let far_future = i64::MAX;
        let removed = db.prune_settled_host_commands("h", far_future, 0).unwrap();
        // d is past the receipt, unsettled is unfinished, run belongs to an open assignment.
        assert_eq!(removed, 3);
        assert_eq!(ids(&db, "h", &names), ["d", "unsettled", "run"]);
        // Sequences are never reused: the stored tail still allocates after it.
        assert_eq!(append(&db, "h", "next", None, b"7").unwrap().sequence, 7);
        // Once the assignment resolves and the peer catches up, the rest can go,
        // except the highest sequence and any unsettled command.
        db.resolve_spoke_assignment("exec_1", 1, "released").unwrap();
        db.record_host_command_receipt("h", &journal, 7).unwrap();
        db.settle_host_command("h", "next", None, b"r").unwrap();
        assert_eq!(db.prune_settled_host_commands("h", far_future, 0).unwrap(), 2);
        assert_eq!(ids(&db, "h", &["d", "unsettled", "run", "next"]), ["unsettled", "next"]);
    }

    #[test]
    fn retention_respects_age_and_the_recent_floor() {
        let db = db();
        for id in ["a", "b", "c"] {
            append(&db, "h", id, None, id.as_bytes()).unwrap();
            db.settle_host_command("h", id, None, b"r").unwrap();
        }
        let (journal, _) = db.resume_host_command_journal("h", None).unwrap();
        db.record_host_command_receipt("h", &journal, 3).unwrap();
        // Nothing is old enough.
        assert_eq!(db.prune_settled_host_commands("h", 0, 0).unwrap(), 0);
        // The recent floor keeps the newest settled commands; the top sequence stays regardless.
        assert_eq!(db.prune_settled_host_commands("h", i64::MAX, 3).unwrap(), 0);
        assert_eq!(db.prune_settled_host_commands("h", i64::MAX, 0).unwrap(), 2);
        assert_eq!(ids(&db, "h", &["a", "b", "c"]), ["c"]);
        // Rejected commands carry no sequence and follow the same rules.
        append(&db, "other", "x", None, b"x").unwrap();
        let result = |id: &str| id.as_bytes().to_vec();
        db.reject_host_command("other", 1, &result, &|wire: &[u8], _| Ok(wire.to_vec()))
            .unwrap();
        assert_eq!(db.prune_settled_host_commands("other", i64::MAX, 0).unwrap(), 1);
        assert_eq!(db.host_command_result("other", "x").unwrap(), None);
    }

    #[test]
    fn allocation_never_falls_below_the_durable_receipt_after_prune_and_reject() {
        let db = db();
        for (id, body) in [("a", b"1"), ("b", b"2"), ("c", b"3")] {
            append(&db, "h", id, None, body).unwrap();
        }
        db.settle_host_command("h", "a", None, b"r").unwrap();
        db.settle_host_command("h", "b", None, b"r").unwrap();
        let (journal, _) = db.resume_host_command_journal("h", None).unwrap();
        db.record_host_command_receipt("h", &journal, 2).unwrap();
        assert_eq!(db.prune_settled_host_commands("h", i64::MAX, 0).unwrap(), 2);
        let result = |id: &str| id.as_bytes().to_vec();
        db.reject_host_command("h", 3, &result, &|wire: &[u8], _| Ok(wire.to_vec()))
            .unwrap();
        let next = append(&db, "h", "d", None, b"4").unwrap();
        assert_eq!(next.sequence, 3);
        let after = db.host_command_after("h", 2).unwrap().unwrap();
        assert_eq!(after.sequence, 3);
        assert_eq!(&after.wire[8..], b"4");
        db.record_host_command_receipt("h", &journal, 2).unwrap();
        assert_eq!(db.resume_host_command_journal("h", Some((&journal, 2))).unwrap().1, 2);
    }

    #[test]
    fn a_resolved_newest_generation_admits_recovery_but_not_start() {
        let db = spoke_db();
        let first = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        let run = key(&first);
        lease(&db, "exec_1", 1, "lease-a");
        assert!(db.resolve_spoke_assignment("exec_1", 1, "operator_fenced").unwrap());
        assert_eq!(
            fault_of(&append(&db, "host-a", "spawn", Some(&run), b"spawn").unwrap_err()),
            Some(HostOutboxFault::Denied)
        );
        append_with(&db, "host-a", "restore", Some(&run), RunAdmission::Recovery, b"restore").unwrap();
    }

    #[test]
    fn pid_and_workspace_lookups_fail_closed_when_two_open_assignments_match() {
        let db = db();
        db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        db.reserve_spoke_assignment("exec_2", "host-a").unwrap();
        db.bind_spoke_lease("exec_1", 1, "ws-shared", "lease-a", "/a").unwrap();
        db.bind_spoke_lease("exec_2", 1, "ws-shared", "lease-b", "/b").unwrap();
        let workspace = db
            .open_spoke_assignment_for_workspace("host-a", "ws-shared")
            .unwrap_err();
        assert_eq!(
            workspace.downcast_ref::<AmbiguousSpokeAssignment>().unwrap().kind,
            "workspace_id"
        );
        db.connect()
            .unwrap()
            .execute(
                "UPDATE spoke_assignments SET pane_pid = 42
                 WHERE host_id = 'host-a' AND execution_id IN ('exec_1', 'exec_2')",
                [],
            )
            .unwrap();
        let pid = db.open_spoke_assignment_for_pane_pid("host-a", 42).unwrap_err();
        assert_eq!(pid.downcast_ref::<AmbiguousSpokeAssignment>().unwrap().kind, "pane_pid");
    }

    #[test]
    fn recording_a_pane_pid_clears_the_same_pid_on_other_open_assignments() {
        let db = db();
        db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        db.reserve_spoke_assignment("exec_2", "host-a").unwrap();
        db.record_spoke_pane_pid("exec_1", 1, 42).unwrap();
        db.record_spoke_pane_pid("exec_2", 1, 42).unwrap();
        let found = db.open_spoke_assignment_for_pane_pid("host-a", 42).unwrap().unwrap();
        assert_eq!(found.execution_id, "exec_2");
        assert_eq!(
            db.spoke_assignment_for_execution("exec_1").unwrap().unwrap().pane_pid,
            None
        );
    }

    #[test]
    fn an_idle_reservation_cannot_start_after_the_host_drops_to_pending() {
        let db = db();
        db.set_host_transport("host-a", HostTransport::SpokeReady).unwrap();
        let idle = db.reserve_spoke_assignment("exec_idle", "host-a").unwrap();
        db.set_host_transport("host-a", HostTransport::SpokePending).unwrap();
        assert_eq!(
            fault_of(&append(&db, "host-a", "lease", Some(&key(&idle)), b"lease").unwrap_err()),
            Some(HostOutboxFault::Denied)
        );
        db.set_host_transport("host-a", HostTransport::SpokeReady).unwrap();
        let started = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        lease(&db, "exec_1", 1, "lease-a");
        append(&db, "host-a", "lease", Some(&key(&started)), b"lease").unwrap();
        db.set_host_transport("host-a", HostTransport::SpokePending).unwrap();
        append(&db, "host-a", "spawn", Some(&key(&started)), b"spawn").unwrap();
    }
}
