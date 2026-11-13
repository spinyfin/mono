//! Projection of durable spoke observations onto work state.
//!
//! The stream manager's sink makes a record *received*: its original bytes and
//! a contiguous cursor commit before the hub acknowledges anything, and nothing
//! about work status happens there. This module is the second half. A
//! [`Projector`] reads the stored frames of each host in order, reduces each
//! to observations ([`reduce`]), hands them to the engine's existing
//! consumers through an [`ObservationApplier`], and only then moves a separate
//! application cursor. Receipt and application therefore fail independently: a
//! crash between them replays locally and never asks the spoke to resend.
//!
//! # What makes an effect happen once
//!
//! Three layers, each with its own key:
//!
//! 1. **Ingress** already stores a frame once per `(host, journal, sequence)`
//!    and a duplicate delivery changes nothing.
//! 2. **The cursor** moves one record at a time, in order, and only in the same
//!    transaction that records the durable effects the record owes
//!    (`WorkDb::advance_spoke_projection`). A projector that crashes before
//!    that commit replays the record; one that crashes after never sees it
//!    again.
//! 3. **Effects** carry a `(host, kind, key)` identity. Two records that
//!    describe the same proposal yield one decision, and a record is not
//!    re-applied once its decision exists.
//!
//! What the applier does for the part of a record that must run before the
//! cursor moves is idempotent by the engine's own keys (a proposal by its
//! idempotency key, a finalize by its status guards), so the one window a
//! crash can open, after an application and before the cursor commit, repeats
//! work that is already a no-op. A transient failure of that commit retries
//! only the commit; it does not re-run observations. Hook events are the
//! exception that is at-least-once rather than idempotent: the live-state
//! reducer and the completion paths already tolerate a repeated event, as
//! they do a shim retry.
//!
//! # Ordering
//!
//! A host's records are applied strictly in order, so a worker's `done`
//! proposal is applied before the `Stop` that follows it, exactly as the
//! synchronous local call does. A record that cannot be applied yet (the
//! database is busy) holds that host's cursor and is retried with backoff; it
//! is never skipped. Work that waits on something other than the database, such
//! as the spoke being reachable to receive a decision, is a durable *effect*
//! that does not hold the cursor.
//!
//! # What this does not decide
//!
//! The projection never decides what absence or a pane exit means for work
//! status. It records the observation durably and reflects it where the engine
//! already reflects such evidence; the hub's existing policy decides the rest.

pub mod reduce;

use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use boss_backoff::exponential_delay;
use boss_protocol::ProposalKind;
use boss_protocol_spoke::{
    limits::MAX_PAYLOAD_BYTES,
    v1::{Command, CommandResult, ProposalDecision, ResultCode, command},
};
use boss_spoke_custody::{Applied as Custodied, Evidence, Source};
pub use boss_spoke_custody::{Custody, Mark, TranscriptSource};
pub use boss_spoke_stream::CommandOutcome;
use boss_spoke_stream::{StreamManager, store::CommandAdmission};
use boss_work_db::{
    NewSpokeEffect, SpokeAdvance, SpokeEffect, SpokeEffectOutcome, SpokeIngressRow, SpokePaneObservation,
    SpokeProjectionFault, SpokeProjectionPosition, WorkDb,
};
use prost::Message;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tokio::task::{Id, JoinSet};

use crate::HubStream;

pub use reduce::{
    ArtifactObservation, EventObservation, GapObservation, Life, Observation, Origin, PaneObservation,
    ProposalObservation, ProposalRequest, Reduction, Skip, TranscriptObservation,
};

impl HubStream {
    /// A projector that also holds each run's transcript and artifact bytes in
    /// `custody`, so the engine's readers find them by run identity. Lives here
    /// beside the projector it builds; the stream's wake and manager are private
    /// to the crate.
    pub fn projector_with_custody(
        &self,
        db: WorkDb,
        applier: Arc<dyn ObservationApplier>,
        config: ProjectorConfig,
        custody: Arc<Custody>,
    ) -> Projector {
        Projector::with_custody(
            db,
            applier,
            Arc::new(self.manager.clone()),
            Arc::clone(&self.projection_wake),
            config,
            custody,
        )
    }
}

/// Effect kind: the decision for a queued proposal, owed to the spoke.
pub const PROPOSAL_DECISION: &str = "proposal_decision";
/// Effect kind: a frame that could not be read. Stored as a failed effect and
/// never retried; the cursor moves on.
pub const QUARANTINE: &str = "quarantine";
/// Effect kind: a transcript or artifact the hub stopped holding because its
/// own copy could not be trusted. Stored as a failed effect, so it counts
/// against the host on engine health; the resource's ledger row says which.
pub const CUSTODY_HALT: &str = "custody_halt";
/// Effect kind: a completion boundary went ahead without transcript bytes the
/// spoke had said it needed, because they did not arrive in time.
pub const EVIDENCE_UNAVAILABLE: &str = "transcript_evidence_unavailable";
/// Effect kind: a Stop or proposal parked so the host cursor can move while
/// its transcript or artifact bytes materialize independently.
pub const PENDING_OBSERVATION: &str = "pending_observation";

/// The longest `reason` a decision carries.
const MAX_REASON_CHARS: usize = 1024;

/// How an application attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// Applied, or deliberately dropped with the reason logged.
    Done,
    /// Could not be applied now; the same record is tried again, in order.
    Retry(String),
}

/// How far delivery of a proposal decision has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    /// The spoke acknowledged the decision.
    Delivered,
    /// Delivery ended without an acknowledgement and is never retried.
    GaveUp,
    /// Delivery failed for now and will be retried.
    Pending,
}

/// A decision effect's delivery state, with the identity needed to find the
/// proposal it answers after a restart.
#[derive(Debug, Clone, Copy)]
pub struct DecisionSettled<'a> {
    pub effect_key: &'a str,
    pub execution_id: &'a str,
    pub proposal_id: &'a str,
    pub settlement: Settlement,
    /// Wire result code of the stored decision (`ResultCode::Ok` accepted it).
    pub code: i32,
}

/// What the hub decided about a queued proposal.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// Whether the hub accepted the proposal. A rejection is the hub's answer,
    /// not a failure to answer.
    pub accepted: bool,
    pub reason: String,
    /// What the worker's CLI shows; bounded to the wire payload limit.
    pub result: Value,
}

impl Decision {
    pub fn refused(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            accepted: false,
            result: json!({ "refused": reason }),
            reason,
        }
    }
}

/// How a proposal application ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Proposed {
    Decided(Decision),
    Retry(String),
}

/// The engine's existing consumers, behind the projection.
///
/// Every method is called with one observation of a run whose assignment is
/// the execution's current one; the projector has already dropped what a newer
/// attempt superseded.
#[async_trait]
pub trait ObservationApplier: Send + Sync {
    /// Fan one hook event out to live state, completion, probes and the other
    /// worker-progress consumers, as a hook arriving on the local events socket
    /// would be.
    async fn event(&self, observation: &EventObservation) -> Applied;

    /// Submit a queued proposal through the same validators and apply pipeline
    /// a local worker's `propose` uses, including its completion finalization,
    /// and report the decision. Replaying the same proposal must be safe.
    async fn proposal(&self, origin: &Origin, kind: ProposalKind, payload: &Value, idempotency_key: &str) -> Proposed;

    /// Apply a run-scoped write the gateway queued under its own contract,
    /// through the same ownership-checked handlers a local worker's matching
    /// RPC uses. Replaying the same write must be safe.
    async fn run_write(&self, origin: &Origin, contract: &str, payload: &Value, idempotency_key: &str) -> Proposed {
        let _ = (origin, payload, idempotency_key);
        Proposed::Decided(Decision::refused(format!(
            "payload contract `{contract}` is not supported by this hub"
        )))
    }

    /// Called on every sweep of the projector, whether or not a record is
    /// waiting, so state the applier could not rebuild earlier is retried
    /// without needing another observation to arrive first.
    async fn sweep(&self) {}

    /// A run's transcript now has bytes on the hub. The projector has already
    /// written them; this lets the engine point the run at its hub copy, so the
    /// readers that find a transcript by run (live summary, cost, completion)
    /// open that file and never a path on the host.
    async fn transcript_attached(&self, origin: &Origin, source: &TranscriptSource) -> Applied {
        let _ = (origin, source);
        Applied::Done
    }

    /// Reflect a pane the host looked at. The projector has already recorded the
    /// observation durably; this is its effect on live state.
    async fn pane(&self, observation: &PaneObservation) -> Applied;

    /// Where a proposal decision's delivery stands. Called after the cursor
    /// commit with [`Settlement::Delivered`] or [`Settlement::GaveUp`] once the
    /// effect is terminal, and with [`Settlement::Pending`] while delivery is
    /// still being retried, so finalization that does not need the spoke's
    /// acknowledgement is not held back by an unreachable host. Must return
    /// promptly: it runs on the single delivery pass shared by every host.
    async fn decision_settled(&self, decision: &DecisionSettled<'_>) {
        let _ = decision;
    }
}

/// Delivery of a command to a spoke over its live stream.
#[async_trait]
pub trait CommandSender: Send + Sync {
    async fn send(&self, host_id: &str, command: Command, deadline: Duration) -> Result<CommandResult, CommandOutcome>;
}

#[async_trait]
impl CommandSender for StreamManager {
    async fn send(&self, host_id: &str, command: Command, deadline: Duration) -> Result<CommandResult, CommandOutcome> {
        // A decision only addresses an assignment that already exists, so it
        // stays deliverable after that assignment resolves.
        self.command(host_id, CommandAdmission::Recovery, command, deadline)
            .await
    }
}

#[derive(Debug, Clone, bon::Builder)]
pub struct ProjectorConfig {
    /// How often to look for received records even when nothing woke the
    /// projector, so a missed wake cannot strand a host.
    #[builder(default = Duration::from_secs(5))]
    pub sweep_interval: Duration,
    /// How often owed effects are retried.
    #[builder(default = Duration::from_secs(3))]
    pub delivery_interval: Duration,
    /// First wait before a record that could not be applied is tried again.
    #[builder(default = Duration::from_millis(250))]
    pub retry_initial: Duration,
    #[builder(default = Duration::from_secs(30))]
    pub retry_max: Duration,
    /// How long a host whose journal cannot be read stays quiet before the
    /// projector looks again.
    #[builder(default = Duration::from_secs(30))]
    pub halt_hold: Duration,
    /// Frames read from the journal per query.
    #[builder(default = 64)]
    pub batch: u32,
    /// How long one decision delivery waits for the spoke's answer.
    #[builder(default = Duration::from_secs(10))]
    pub delivery_deadline: Duration,
    /// How long a decision command stays valid on the spoke.
    #[builder(default = Duration::from_secs(7 * 24 * 60 * 60))]
    pub decision_ttl: Duration,
    /// Definite refusals by the spoke before a decision is given up on.
    #[builder(default = 3)]
    pub max_rejections: u32,
    /// After this long retrying one record in place, persist a stall so engine
    /// health can report the host as blocked.
    #[builder(default = Duration::from_secs(30))]
    pub retry_stall: Duration,
    /// How long parked completion work waits for transcript or artifact bytes
    /// after its record has already advanced the host cursor. Past this the
    /// observation is applied with unavailable evidence. Independent
    /// materialization of later records is never held for this bound.
    #[builder(default = Duration::from_secs(60))]
    pub watermark_wait: Duration,
    /// How often a waiting boundary looks for newly received bytes.
    #[builder(default = Duration::from_millis(50))]
    pub watermark_poll: Duration,
}

impl Default for ProjectorConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

/// Why one record stopped the projection of its host.
#[derive(Debug)]
enum Interrupted {
    /// Try the same record again after a backoff.
    Retry(String),
    /// The journal cannot be projected as it stands. The host stays halted
    /// (retried after a hold) rather than skipping evidence.
    Halt(String),
}

/// Backoff and stall reporting for a retry loop that holds one host's cursor.
struct RetryHold {
    attempt: u32,
    started: std::time::Instant,
    stall_recorded: bool,
}

impl RetryHold {
    fn new() -> Self {
        Self {
            attempt: 0,
            started: std::time::Instant::now(),
            stall_recorded: false,
        }
    }
}

/// How a wait for transcript bytes ended.
enum Settled {
    Ready(TranscriptSource),
    Unavailable {
        reason: String,
        /// Worth an alert: bytes the spoke sent did not arrive. The spoke
        /// declaring them unavailable is recorded in custody and is not one.
        alert: bool,
    },
    /// Bytes or named outputs are still arriving; the observation is parked.
    Pending {
        have: u64,
        want: u64,
    },
}

enum Currentness {
    Current,
    Superseded(String),
}

enum HostEnd {
    CaughtUp,
    /// The database could not be read just now; look again on the next sweep.
    Unreadable,
    /// The journal cannot be projected as stored; wait before looking again.
    Halted,
}

/// Clears the delivery flag however the pass ends, including by being dropped.
struct Delivering<'a>(&'a AtomicBool);

impl Drop for Delivering<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[derive(bon::Builder)]
struct Inner {
    db: WorkDb,
    applier: Arc<dyn ObservationApplier>,
    sender: Arc<dyn CommandSender>,
    wake: Arc<Notify>,
    config: ProjectorConfig,
    /// Where transcript and artifact bytes are held. Without it, transcript,
    /// artifact and gap records are received but not written to disk.
    custody: Option<Arc<Custody>>,
    /// The position a halt was last reported at, per host, so a persistent
    /// fault is logged once rather than every sweep.
    #[builder(skip)]
    reported: Mutex<HashMap<String, u64>>,
    #[builder(skip)]
    delivering: AtomicBool,
}

/// Applies each host's stored observations, in order, and delivers the effects
/// they owe. Cheap to clone.
#[derive(Clone)]
pub struct Projector {
    inner: Arc<Inner>,
}

impl Projector {
    /// `wake` is notified whenever the sink commits a record; see
    /// [`crate::HubStream::projector`].
    pub fn new(
        db: WorkDb,
        applier: Arc<dyn ObservationApplier>,
        sender: Arc<dyn CommandSender>,
        wake: Arc<Notify>,
        config: ProjectorConfig,
    ) -> Self {
        Self::build(db, applier, sender, wake, config, None)
    }

    /// A projector that also holds each run's transcript and artifact bytes in
    /// `custody`.
    pub fn with_custody(
        db: WorkDb,
        applier: Arc<dyn ObservationApplier>,
        sender: Arc<dyn CommandSender>,
        wake: Arc<Notify>,
        config: ProjectorConfig,
        custody: Arc<Custody>,
    ) -> Self {
        Self::build(db, applier, sender, wake, config, Some(custody))
    }

    fn build(
        db: WorkDb,
        applier: Arc<dyn ObservationApplier>,
        sender: Arc<dyn CommandSender>,
        wake: Arc<Notify>,
        config: ProjectorConfig,
        custody: Option<Arc<Custody>>,
    ) -> Self {
        Self {
            inner: Arc::new(
                Inner::builder()
                    .db(db)
                    .applier(applier)
                    .sender(sender)
                    .wake(wake)
                    .config(config)
                    .maybe_custody(custody)
                    .build(),
            ),
        }
    }

    /// Project and deliver until the returned future is dropped. Dropping it
    /// stops every host's task: that is the crash a restart recovers from.
    pub async fn run(self) {
        let inner = self.inner;
        // Host tasks are tracked by id so a task that ends abnormally still
        // frees its host for the next pass.
        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut running: HashMap<Id, String> = HashMap::new();
        let mut sweep = tokio::time::interval(inner.config.sweep_interval);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut delivery = tokio::time::interval(inner.config.delivery_interval);
        delivery.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = inner.wake.notified() => {}
                _ = sweep.tick() => {
                    inner.applier.sweep().await;
                    inner.settle_parked(false).await;
                }
                _ = delivery.tick() => {
                    // A pass already under way makes this one return at once.
                    let inner = Arc::clone(&inner);
                    tasks.spawn(async move { inner.deliver_pending().await });
                }
                Some(finished) = tasks.join_next_with_id(), if !tasks.is_empty() => {
                    let id = match finished {
                        Ok((id, ())) => id,
                        Err(error) => {
                            tracing::error!(%error, "a spoke projection task ended abnormally");
                            error.id()
                        }
                    };
                    running.remove(&id);
                }
            }
            let behind = match inner.hosts_behind().await {
                Ok(behind) => behind,
                Err(reason) => {
                    tracing::warn!(%reason, "cannot list spoke hosts awaiting projection");
                    continue;
                }
            };
            for position in behind {
                if running.values().any(|host| host == &position.host_id) {
                    continue;
                }
                let host = position.host_id;
                let task_inner = Arc::clone(&inner);
                let task_host = host.clone();
                let hold = inner.config.halt_hold;
                let handle = tasks.spawn(async move {
                    if matches!(task_inner.drain_host(&task_host).await, HostEnd::Halted) {
                        // The host stays claimed while it waits, so a fault in
                        // its journal is looked at again after the hold, not
                        // on every sweep.
                        tokio::time::sleep(hold).await;
                    }
                });
                running.insert(handle.id(), host);
            }
        }
    }

    /// Project every host's received records and deliver the owed effects once,
    /// then return. The same work [`Self::run`] does continuously; tests and
    /// startup use it to reach a known point.
    pub async fn drain(&self) {
        match self.inner.hosts_behind().await {
            Ok(behind) => {
                for position in behind {
                    self.inner.drain_host(&position.host_id).await;
                }
            }
            Err(reason) => tracing::warn!(%reason, "cannot list spoke hosts awaiting projection"),
        }
        self.inner.settle_parked(true).await;
        self.inner.deliver_pending().await;
    }
}

impl Inner {
    async fn blocking<T, F>(&self, work: F) -> Result<T, Interrupted>
    where
        T: Send + 'static,
        F: FnOnce(&WorkDb) -> anyhow::Result<T> + Send + 'static,
    {
        let db = self.db.clone();
        match tokio::task::spawn_blocking(move || work(&db)).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(match error.downcast_ref::<SpokeProjectionFault>() {
                Some(fault) => Interrupted::Halt(fault.to_string()),
                None => Interrupted::Retry(format!("{error:#}")),
            }),
            Err(error) => Err(Interrupted::Retry(format!("database task failed: {error}"))),
        }
    }

    async fn hosts_behind(&self) -> Result<Vec<SpokeProjectionPosition>, String> {
        self.blocking(|db| db.spoke_hosts_behind())
            .await
            .map_err(|interrupted| match interrupted {
                Interrupted::Retry(reason) | Interrupted::Halt(reason) => reason,
            })
    }

    /// Apply a host's received records until it has none left, or one stops it.
    async fn drain_host(&self, host: &str) -> HostEnd {
        loop {
            let host_id = host.to_owned();
            let position = match self.blocking(move |db| db.spoke_projection_position(&host_id)).await {
                Ok(Some(position)) => position,
                Ok(None) => return HostEnd::CaughtUp,
                Err(Interrupted::Retry(reason) | Interrupted::Halt(reason)) => {
                    tracing::warn!(host, %reason, "cannot read a spoke host's projection cursor");
                    return HostEnd::Unreadable;
                }
            };
            if position.lag() == 0 {
                let host_id = host.to_owned();
                let _ = self.blocking(move |db| db.clear_spoke_projection_halt(&host_id)).await;
                // Bytes this pass just wrote may unblock a parked completion of
                // this host; apply it now rather than on the next sweep.
                self.settle_parked_inner(Some(host), false).await;
                return HostEnd::CaughtUp;
            }
            if position
                .cursor_journal_id
                .as_deref()
                .is_some_and(|cursor| cursor != position.journal_id.as_str())
            {
                self.halt(
                    host,
                    position.applied,
                    "the host's projection cursor belongs to another journal",
                );
                return HostEnd::Halted;
            }
            let (host_id, journal_id, applied, batch) = (
                host.to_owned(),
                position.journal_id.clone(),
                position.applied,
                self.config.batch,
            );
            let rows = match self
                .blocking(move |db| db.spoke_ingress_after(&host_id, &journal_id, applied, batch))
                .await
            {
                Ok(rows) => rows,
                Err(Interrupted::Retry(reason) | Interrupted::Halt(reason)) => {
                    tracing::warn!(host, %reason, "cannot read a spoke host's stored records");
                    return HostEnd::Unreadable;
                }
            };
            let mut next = position.applied;
            if rows.first().is_none_or(|row| row.sequence != next + 1) {
                self.halt(host, next, &format!("record {} is received but not stored", next + 1));
                return HostEnd::Halted;
            }
            for row in rows {
                if row.sequence != next + 1 {
                    self.halt(host, next, &format!("record {} is received but not stored", next + 1));
                    return HostEnd::Halted;
                }
                if let Err(reason) = self.project_record(host, &position.journal_id, &row).await {
                    self.halt(host, next, &reason);
                    return HostEnd::Halted;
                }
                next = row.sequence;
            }
            self.settle_parked(false).await;
            self.reported
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(host);
        }
    }

    fn halt(&self, host: &str, applied: u64, reason: &str) {
        let mut reported = self.reported.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if reported.insert(host.to_owned(), applied) != Some(applied) {
            tracing::error!(
                host,
                applied,
                reason,
                "spoke observation projection halted: received evidence cannot be applied and is not skipped; \
                 the host is retried after a hold"
            );
        }
        let (host_id, halt_reason) = (host.to_owned(), reason.to_owned());
        let db = self.db.clone();
        // Best-effort: a halt that does not persist still holds the host in
        // this process and is retried after the hold.
        tokio::task::spawn_blocking(move || {
            if let Err(error) = db.set_spoke_projection_halt(&host_id, &halt_reason) {
                tracing::warn!(host = %host_id, %error, "cannot persist a spoke projection halt");
            }
        });
    }

    /// Apply one record, retrying transient failures in place. Returns an error
    /// only when the record can never be applied as stored. Retry covers
    /// failures that happen before any observation is applied; once
    /// observations have run, only the cursor commit is retried.
    async fn project_record(&self, host: &str, journal: &str, row: &SpokeIngressRow) -> Result<(), String> {
        let mut hold = RetryHold::new();
        loop {
            match self.apply_record(host, journal, row).await {
                Ok(()) => {
                    self.clear_stall_if_recorded(host, &hold).await;
                    return Ok(());
                }
                Err(Interrupted::Halt(reason)) => return Err(reason),
                Err(Interrupted::Retry(reason)) => {
                    self.hold_retry(host, row.sequence, &reason, &mut hold).await;
                }
            }
        }
    }

    async fn apply_record(&self, host: &str, journal: &str, row: &SpokeIngressRow) -> Result<(), Interrupted> {
        let (host_id, journal_id, sequence) = (host.to_owned(), journal.to_owned(), row.sequence);
        match self
            .blocking(move |db| db.check_spoke_projection_advance(&host_id, &journal_id, sequence))
            .await
        {
            Ok(SpokeAdvance::AlreadyApplied) => return Ok(()),
            Ok(SpokeAdvance::Advanced { .. }) => {}
            Err(interrupted) => return Err(interrupted),
        }
        let mut owed: Vec<Owed> = Vec::new();
        match reduce::reduce(host, journal, row) {
            Reduction::Skip(skip) => {
                tracing::debug!(
                    host,
                    sequence = row.sequence,
                    ?skip,
                    "spoke record carries nothing to project"
                );
            }
            Reduction::Quarantine(reason) => {
                tracing::error!(
                    host,
                    sequence = row.sequence,
                    %reason,
                    "quarantined an unreadable spoke record; the cursor moves past it"
                );
                owed.push(Owed::quarantine(row.sequence, &reason));
            }
            Reduction::Observations(observations) => {
                for observation in observations {
                    self.apply_observation(observation, &mut owed).await?;
                }
            }
        }
        self.commit_cursor(host, journal, row.sequence, owed).await
    }

    /// Advance the cursor after observations have already been applied.
    /// Transient failures retry only this commit, so a hook already fanned
    /// out is not dispatched again.
    async fn commit_cursor(
        &self,
        host: &str,
        journal: &str,
        sequence: u64,
        owed: Vec<Owed>,
    ) -> Result<(), Interrupted> {
        let mut hold = RetryHold::new();
        loop {
            let (host_id, journal_id, owed) = (host.to_owned(), journal.to_owned(), owed.clone());
            match self
                .blocking(move |db| {
                    let effects: Vec<_> = owed.iter().map(Owed::as_new).collect();
                    db.advance_spoke_projection(&host_id, &journal_id, sequence, &effects)
                })
                .await
            {
                Ok(_) => {
                    self.clear_stall_if_recorded(host, &hold).await;
                    return Ok(());
                }
                Err(Interrupted::Halt(reason)) => return Err(Interrupted::Halt(reason)),
                Err(Interrupted::Retry(reason)) => {
                    self.hold_retry(host, sequence, &reason, &mut hold).await;
                }
            }
        }
    }

    async fn hold_retry(&self, host: &str, sequence: u64, reason: &str, hold: &mut RetryHold) {
        hold.attempt += 1;
        if !hold.stall_recorded && hold.started.elapsed() >= self.config.retry_stall {
            let (host_id, stall_reason) = (host.to_owned(), reason.to_owned());
            match self
                .blocking(move |db| db.set_spoke_projection_stall(&host_id, &stall_reason))
                .await
            {
                Ok(_) => hold.stall_recorded = true,
                Err(Interrupted::Retry(error) | Interrupted::Halt(error)) => {
                    tracing::warn!(host, %error, "cannot record a spoke projection stall; it is retried");
                }
            }
        }
        let wait = exponential_delay(self.config.retry_initial, self.config.retry_max, hold.attempt.min(16));
        tracing::warn!(
            host,
            sequence,
            attempt = hold.attempt,
            %reason,
            "a spoke record could not be applied yet; holding this host's cursor"
        );
        tokio::time::sleep(wait).await;
    }

    async fn clear_stall_if_recorded(&self, host: &str, hold: &RetryHold) {
        if !hold.stall_recorded {
            return;
        }
        let host_id = host.to_owned();
        if let Err(Interrupted::Retry(error) | Interrupted::Halt(error)) =
            self.blocking(move |db| db.clear_spoke_projection_stall(&host_id)).await
        {
            tracing::warn!(host, %error, "cannot clear a spoke projection stall");
        }
    }

    async fn apply_observation(&self, observation: Observation, owed: &mut Vec<Owed>) -> Result<(), Interrupted> {
        let currentness = self.currentness(observation.origin()).await?;
        match (observation, currentness) {
            (Observation::Proposal(proposal), currentness) => {
                if matches!(currentness, Currentness::Current) && self.should_park_proposal(&proposal).await? {
                    owed.push(Owed::pending_observation(&proposal.origin, "proposal"));
                    return Ok(());
                }
                self.apply_proposal(proposal, currentness, Some(owed)).await
            }
            (Observation::Event(event), Currentness::Current) => {
                if self.should_park_event(&event).await? {
                    owed.push(Owed::pending_observation(&event.origin, "event"));
                    return Ok(());
                }
                self.dispatch_event(event, Some(owed)).await
            }
            (
                observation @ (Observation::Transcript(_) | Observation::Artifact(_) | Observation::Gap(_)),
                Currentness::Current,
            ) => self.apply_custody(observation, owed).await,
            (observation, Currentness::Superseded(why)) => {
                tracing::debug!(
                    host = %observation.origin().host_id,
                    sequence = observation.origin().sequence,
                    execution = %observation.origin().run.execution_id,
                    %why,
                    "dropped an observation of a superseded assignment"
                );
                Ok(())
            }
            (Observation::Pane(pane), Currentness::Current) => {
                let stored = observation_row(&pane);
                self.blocking(move |db| db.record_spoke_pane_observation(&stored))
                    .await?;
                match self.applier.pane(&pane).await {
                    Applied::Done => Ok(()),
                    Applied::Retry(reason) => Err(Interrupted::Retry(reason)),
                }
            }
        }
    }

    async fn apply_proposal(
        &self,
        proposal: ProposalObservation,
        currentness: Currentness,
        owed: Option<&mut Vec<Owed>>,
    ) -> Result<(), Interrupted> {
        let origin = &proposal.origin;
        // `run_id` is unique per assignment, so one proposal id can never name
        // two attempts' proposals.
        let key = format!("{}:{}", origin.run.run_id, proposal.proposal_id);
        let (host, lookup) = (origin.host_id.clone(), key.clone());
        if self
            .blocking(move |db| db.spoke_effect_exists(&host, PROPOSAL_DECISION, &lookup))
            .await?
        {
            tracing::debug!(
                host = %origin.host_id,
                proposal = %proposal.proposal_id,
                "a decision for this proposal already exists; the repeated record applies nothing"
            );
            return Ok(());
        }
        let mut local = Vec::new();
        let record_local = owed.is_none();
        let dest = owed.unwrap_or(&mut local);
        if let (Currentness::Current, Some(mark)) = (&currentness, &proposal.mark) {
            match self
                .check_evidence(origin, mark, proposal_wants_capture(&proposal))
                .await?
            {
                Settled::Unavailable { reason, alert } => {
                    self.note_unavailable(origin, mark, &reason, alert, dest).await;
                }
                Settled::Pending { have, want } => {
                    self.note_unavailable(
                        origin,
                        mark,
                        &pending_timeout_reason(have, want, self.config.watermark_wait),
                        true,
                        dest,
                    )
                    .await;
                }
                Settled::Ready(_) => {}
            }
        }
        let decision = match (&proposal.request, currentness) {
            (_, Currentness::Superseded(why)) => Decision::refused(format!("the assignment was superseded: {why}")),
            (ProposalRequest::Refused(reason), _) => Decision::refused(reason.clone()),
            (
                ProposalRequest::Standard {
                    kind,
                    payload,
                    idempotency_key,
                },
                Currentness::Current,
            ) => match self.applier.proposal(origin, *kind, payload, idempotency_key).await {
                Proposed::Decided(decision) => decision,
                Proposed::Retry(reason) => return Err(Interrupted::Retry(reason)),
            },
            (
                ProposalRequest::RunWrite {
                    contract,
                    payload,
                    idempotency_key,
                },
                Currentness::Current,
            ) => match self.applier.run_write(origin, contract, payload, idempotency_key).await {
                Proposed::Decided(decision) => decision,
                Proposed::Retry(reason) => return Err(Interrupted::Retry(reason)),
            },
        };
        dest.push(Owed::decision(&proposal, &key, &decision));
        if record_local {
            for item in local {
                self.record_owed(&proposal.origin, item).await?;
            }
        }
        Ok(())
    }

    /// Write one transcript, artifact or gap record to the hub's custody copy.
    /// A disk or database fault holds the cursor and retries; a resource the
    /// hub cannot trust is halted and reported, and the cursor moves on.
    async fn apply_custody(&self, observation: Observation, owed: &mut Vec<Owed>) -> Result<(), Interrupted> {
        let Some(custody) = self.custody.clone() else {
            tracing::debug!(
                sequence = observation.origin().sequence,
                "no custody is configured; the record is received and left alone"
            );
            return Ok(());
        };
        let origin = observation.origin().clone();
        let transcript = matches!(observation, Observation::Transcript(_));
        let handle = Arc::clone(&custody);
        let applied = self
            .blocking(move |_| {
                let origin = observation.origin().clone();
                let source = Source {
                    host_id: &origin.host_id,
                    run: &origin.run,
                    sequence: origin.sequence,
                };
                match &observation {
                    Observation::Transcript(found) => handle.apply_transcript(&source, &found.delta),
                    Observation::Artifact(found) => handle.apply_artifact(&source, &found.chunk),
                    Observation::Gap(found) => handle.apply_gap(&source, &found.gap),
                    _ => Ok(Custodied::Ignored("not a custody record")),
                }
            })
            .await?;
        match applied {
            Custodied::Halted { resource, reason } => {
                tracing::error!(
                    host = %origin.host_id,
                    sequence = origin.sequence,
                    %resource,
                    %reason,
                    "the hub stopped holding a spoke resource: its copy cannot be trusted"
                );
                owed.push(Owed::custody_halt(&origin, &resource, &reason));
            }
            Custodied::Wrote { resource, .. } | Custodied::Replayed { resource } if transcript => {
                self.attach_transcript(&custody, &origin, &resource).await?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Point the run at its hub transcript. Repeated for every written or
    /// replayed chunk, because a crash after a chunk is held and before the run
    /// is pointed at it replays the chunk, not the pointer.
    async fn attach_transcript(
        &self,
        custody: &Arc<Custody>,
        origin: &Origin,
        resource: &str,
    ) -> Result<(), Interrupted> {
        let (handle, run, generation) = (
            Arc::clone(custody),
            origin.run.clone(),
            resource.trim_start_matches("transcript:").to_owned(),
        );
        let source = self
            .blocking(move |_| handle.transcript_source(&run, &generation))
            .await?;
        let Some(source) = source else {
            return Ok(());
        };
        match self.applier.transcript_attached(origin, &source).await {
            Applied::Done => Ok(()),
            Applied::Retry(reason) => Err(Interrupted::Retry(reason)),
        }
    }

    async fn dispatch_event(
        &self,
        mut event: EventObservation,
        owed: Option<&mut Vec<Owed>>,
    ) -> Result<(), Interrupted> {
        let mut local = Vec::new();
        let record_local = owed.is_none();
        let dest = owed.unwrap_or(&mut local);
        if let Some(mark) = event.mark.clone() {
            match self
                .check_evidence(&event.origin, &mark, event_wants_capture(&event))
                .await?
            {
                Settled::Ready(source) => event.transcript = Some(source),
                Settled::Unavailable { reason, alert } => {
                    self.note_unavailable(&event.origin, &mark, &reason, alert, dest).await;
                }
                Settled::Pending { have, want } => {
                    self.note_unavailable(
                        &event.origin,
                        &mark,
                        &pending_timeout_reason(have, want, self.config.watermark_wait),
                        true,
                        dest,
                    )
                    .await;
                }
            }
        }
        match self.applier.event(&event).await {
            Applied::Done => {}
            Applied::Retry(reason) => return Err(Interrupted::Retry(reason)),
        }
        if record_local {
            for item in local {
                self.record_owed(&event.origin, item).await?;
            }
        }
        Ok(())
    }

    async fn should_park_event(&self, event: &EventObservation) -> Result<bool, Interrupted> {
        if self.run_is_parked(&event.origin).await? {
            return Ok(true);
        }
        if !event_wants_capture(event) {
            return Ok(false);
        }
        self.boundary_is_pending(&event.origin, event.mark.as_ref()).await
    }

    async fn should_park_proposal(&self, proposal: &ProposalObservation) -> Result<bool, Interrupted> {
        if self.run_is_parked(&proposal.origin).await? {
            return Ok(true);
        }
        if !proposal_wants_capture(proposal) {
            return Ok(false);
        }
        self.boundary_is_pending(&proposal.origin, proposal.mark.as_ref()).await
    }

    async fn boundary_is_pending(&self, origin: &Origin, mark: Option<&Mark>) -> Result<bool, Interrupted> {
        Ok(matches!(
            self.boundary_settled(origin, mark).await?,
            Settled::Pending { .. }
        ))
    }

    /// Capture-concluded and, when the spoke named a watermark, the transcript
    /// bytes. A Stop with no watermark still waits for named outputs.
    async fn boundary_settled(&self, origin: &Origin, mark: Option<&Mark>) -> Result<Settled, Interrupted> {
        self.check_evidence(origin, mark.unwrap_or(&Mark::default()), true)
            .await
    }

    async fn run_is_parked(&self, origin: &Origin) -> Result<bool, Interrupted> {
        let (host, run) = (origin.host_id.clone(), origin.run.run_id.clone());
        self.blocking(move |db| db.spoke_run_has_pending_observation(&host, &run))
            .await
    }

    /// Whether the bytes `mark` names are held, still arriving, or will not come.
    /// Does not wait and does not hold the host cursor.
    async fn check_evidence(
        &self,
        origin: &Origin,
        mark: &Mark,
        artifacts_settled: bool,
    ) -> Result<Settled, Interrupted> {
        let Some(custody) = self.custody.clone() else {
            return Ok(Settled::Unavailable {
                reason: "no custody is configured on this hub".into(),
                alert: false,
            });
        };
        let (handle, run, wanted) = (Arc::clone(&custody), origin.run.clone(), mark.clone());
        Ok(
            match self
                .blocking(move |_| handle.transcript_evidence(&run, &wanted, artifacts_settled))
                .await?
            {
                Evidence::Ready(source) => Settled::Ready(source),
                Evidence::Unavailable(reason) => Settled::Unavailable { reason, alert: false },
                Evidence::Pending { have, want } => Settled::Pending { have, want },
            },
        )
    }

    /// Apply parked Stop and proposal observations whose bytes have arrived, or
    /// whose wait has run out. Later observations of the same run stay parked
    /// until earlier ones apply, so per-run order is preserved. Other runs are
    /// not in this outbox.
    async fn settle_parked(&self, wait: bool) {
        self.settle_parked_inner(None, wait).await;
    }

    async fn settle_parked_inner(&self, host: Option<&str>, wait: bool) {
        loop {
            let pending = match self.blocking(|db| db.pending_spoke_observations()).await {
                Ok(pending) => pending,
                Err(Interrupted::Retry(reason) | Interrupted::Halt(reason)) => {
                    tracing::warn!(%reason, "cannot list parked spoke observations");
                    return;
                }
            };
            if pending.is_empty() {
                return;
            }
            let mut applied_any = false;
            let mut wait_for: Option<Duration> = None;
            let mut seen: HashSet<String> = HashSet::new();
            for effect in pending {
                if let Some(host) = host
                    && effect.host_id != host
                {
                    continue;
                }
                let Ok(parked) = serde_json::from_slice::<Parked>(&effect.payload) else {
                    tracing::error!(
                        host = %effect.host_id,
                        key = %effect.key,
                        "parked observation payload does not decode; it is dropped"
                    );
                    self.finish_parked(&effect, SpokeEffectOutcome::Failed("payload does not decode".into()))
                        .await;
                    applied_any = true;
                    continue;
                };
                if !seen.insert(parked.run_id.clone()) {
                    continue;
                }
                match self.settle_one(&effect, &parked).await {
                    Ok(ParkedEnd::Applied) => applied_any = true,
                    Ok(ParkedEnd::Waiting(remaining)) => {
                        wait_for = Some(wait_for.map_or(remaining, |current| current.min(remaining)));
                    }
                    Err(Interrupted::Retry(reason) | Interrupted::Halt(reason)) => {
                        tracing::warn!(
                            host = %effect.host_id,
                            key = %effect.key,
                            %reason,
                            "parked observation could not be applied yet"
                        );
                    }
                }
            }
            if applied_any {
                continue;
            }
            let Some(remaining) = wait_for else {
                return;
            };
            if !wait {
                return;
            }
            tokio::time::sleep(remaining.min(self.config.watermark_poll)).await;
        }
    }

    async fn settle_one(&self, effect: &SpokeEffect, parked: &Parked) -> Result<ParkedEnd, Interrupted> {
        let (host, journal, sequence) = (effect.host_id.clone(), effect.journal_id.clone(), effect.sequence);
        let after = sequence.saturating_sub(1);
        let rows = self
            .blocking(move |db| db.spoke_ingress_after(&host, &journal, after, 1))
            .await?;
        let Some(row) = rows.into_iter().find(|row| row.sequence == sequence) else {
            self.finish_parked(
                effect,
                SpokeEffectOutcome::Failed("the parked record is no longer stored".into()),
            )
            .await;
            return Ok(ParkedEnd::Applied);
        };
        let Reduction::Observations(found) = reduce::reduce(&effect.host_id, &effect.journal_id, &row) else {
            self.finish_parked(
                effect,
                SpokeEffectOutcome::Failed("the parked record no longer reduces".into()),
            )
            .await;
            return Ok(ParkedEnd::Applied);
        };
        let Some(observation) = found.into_iter().find(|observation| {
            observation.origin().run.run_id == parked.run_id && observation.origin().sequence == sequence
        }) else {
            self.finish_parked(
                effect,
                SpokeEffectOutcome::Failed("the parked record has no matching observation".into()),
            )
            .await;
            return Ok(ParkedEnd::Applied);
        };
        if let Some(remaining) = self.parked_still_waiting(&observation, parked).await? {
            return Ok(ParkedEnd::Waiting(remaining));
        }
        match observation {
            Observation::Event(event) => self.dispatch_event(event, None).await?,
            Observation::Proposal(proposal) => self.apply_proposal(proposal, Currentness::Current, None).await?,
            _ => {}
        }
        self.finish_parked(effect, SpokeEffectOutcome::Done).await;
        Ok(ParkedEnd::Applied)
    }

    async fn parked_still_waiting(
        &self,
        observation: &Observation,
        parked: &Parked,
    ) -> Result<Option<Duration>, Interrupted> {
        let elapsed = parked_elapsed(parked.parked_at_ms);
        let bound = self.config.watermark_wait;
        let remaining = bound.saturating_sub(elapsed);
        if remaining.is_zero() {
            return Ok(None);
        }
        let pending = match observation {
            Observation::Event(event) if event_wants_capture(event) => {
                self.boundary_is_pending(&event.origin, event.mark.as_ref()).await?
            }
            Observation::Proposal(proposal) if proposal_wants_capture(proposal) => {
                self.boundary_is_pending(&proposal.origin, proposal.mark.as_ref())
                    .await?
            }
            _ => false,
        };
        Ok(pending.then_some(remaining))
    }

    async fn finish_parked(&self, effect: &SpokeEffect, outcome: SpokeEffectOutcome) {
        let (host, kind, key) = (effect.host_id.clone(), effect.kind.clone(), effect.key.clone());
        if let Err(Interrupted::Retry(reason) | Interrupted::Halt(reason)) = self
            .blocking(move |db| db.settle_spoke_effect(&host, &kind, &key, &outcome))
            .await
        {
            tracing::warn!(host = %effect.host_id, %reason, "cannot settle a parked observation");
        }
    }

    async fn record_owed(&self, origin: &Origin, owed: Owed) -> Result<(), Interrupted> {
        let (host, journal, sequence) = (origin.host_id.clone(), origin.journal_id.clone(), origin.sequence);
        self.blocking(move |db| db.record_spoke_effect(&host, &journal, sequence, owed.as_new()))
            .await
    }

    async fn note_unavailable(&self, origin: &Origin, mark: &Mark, reason: &str, alert: bool, owed: &mut Vec<Owed>) {
        tracing::warn!(
            host = %origin.host_id,
            execution = %origin.run.execution_id,
            generation = %mark.generation,
            through = mark.through,
            %reason,
            "a completion boundary goes ahead without the transcript bytes it named"
        );
        if alert {
            owed.push(Owed::evidence_unavailable(origin, mark, reason));
        }
    }

    /// Whether the observation's run is still the execution's current
    /// assignment. An older attempt's evidence must not touch the new one.
    async fn currentness(&self, origin: &Origin) -> Result<Currentness, Interrupted> {
        let execution = origin.run.execution_id.clone();
        let assignment = self
            .blocking(move |db| db.spoke_assignment_for_execution(&execution))
            .await?;
        let run = &origin.run;
        Ok(match assignment {
            Some(a)
                if a.assignment_generation == run.assignment_generation
                    && a.run_id == run.run_id
                    && a.spawn_token == run.spawn_token
                    && a.host_id == origin.host_id =>
            {
                Currentness::Current
            }
            Some(a) if a.assignment_generation > run.assignment_generation => Currentness::Superseded(format!(
                "generation {} replaced generation {}",
                a.assignment_generation, run.assignment_generation
            )),
            Some(_) => Currentness::Superseded("the run does not match the execution's assignment".into()),
            None => Currentness::Superseded("the execution has no spoke assignment".into()),
        })
    }

    /// Deliver the effects still owed, once each. A host that cannot be reached
    /// is skipped for the rest of the pass.
    async fn deliver_pending(&self) {
        if self.delivering.swap(true, Ordering::AcqRel) {
            return;
        }
        let _pass = Delivering(&self.delivering);
        let batch = self.config.batch;
        let pending = self.blocking(move |db| db.pending_spoke_effects(batch)).await;
        let effects = match pending {
            Ok(effects) => effects,
            Err(Interrupted::Retry(reason) | Interrupted::Halt(reason)) => {
                tracing::warn!(%reason, "cannot list owed spoke effects");
                return;
            }
        };
        let mut unreachable: HashSet<String> = HashSet::new();
        for effect in effects {
            if effect.kind == PENDING_OBSERVATION {
                continue;
            }
            if unreachable.contains(&effect.host_id) {
                continue;
            }
            let outcome = self.deliver(&effect).await;
            if let SpokeEffectOutcome::Retry(reason) = &outcome {
                tracing::debug!(host = %effect.host_id, kind = %effect.kind, %reason, "owed spoke effect not delivered yet");
                unreachable.insert(effect.host_id.clone());
                // A host that stays unreachable would otherwise be written to
                // on every pass to say the same thing.
                if effect.last_error.as_deref() == Some(reason.as_str()) {
                    self.settled(&effect, Settlement::Pending).await;
                    continue;
                }
            }
            let settle = outcome.clone();
            let (host, kind, key) = (effect.host_id.clone(), effect.kind.clone(), effect.key.clone());
            if let Err(Interrupted::Retry(reason) | Interrupted::Halt(reason)) = self
                .blocking(move |db| db.settle_spoke_effect(&host, &kind, &key, &settle))
                .await
            {
                tracing::warn!(host = %effect.host_id, %reason, "cannot record a spoke effect's outcome; it will be retried");
            } else {
                let settlement = match &outcome {
                    SpokeEffectOutcome::Done => Settlement::Delivered,
                    SpokeEffectOutcome::Failed(reason) => {
                        tracing::error!(
                            host = %effect.host_id,
                            kind = %effect.kind,
                            key = %effect.key,
                            %reason,
                            "a spoke effect failed and is never retried"
                        );
                        Settlement::GaveUp
                    }
                    SpokeEffectOutcome::Retry(_) | SpokeEffectOutcome::Rejected(_) => Settlement::Pending,
                };
                self.settled(&effect, settlement).await;
            }
        }
    }

    /// Tell the applier where a decision's delivery stands. An effect whose
    /// decision cannot be read back has nothing the applier could finalize.
    async fn settled(&self, effect: &SpokeEffect, settlement: Settlement) {
        if effect.kind != PROPOSAL_DECISION {
            return;
        }
        let Ok(command) = Command::decode(effect.payload.as_slice()) else {
            return;
        };
        let (Some(run), Some(command::Body::ProposalDecision(decision))) =
            (command.run.as_ref(), command.body.as_ref())
        else {
            return;
        };
        self.applier
            .decision_settled(&DecisionSettled {
                effect_key: &effect.key,
                execution_id: &run.execution_id,
                proposal_id: &decision.proposal_id,
                settlement,
                code: decision.code,
            })
            .await;
    }

    async fn deliver(&self, effect: &SpokeEffect) -> SpokeEffectOutcome {
        match effect.kind.as_str() {
            PROPOSAL_DECISION => self.deliver_decision(effect).await,
            other => SpokeEffectOutcome::Failed(format!("no delivery for effect kind `{other}`")),
        }
    }

    /// Send the stored decision. A retry presents the command stored under its
    /// id byte for byte, as the outbox requires; after the spoke refuses a
    /// command the decision goes out under a fresh id, since the spoke applies a
    /// decision by compare-and-swap and the same bytes are always safe to repeat.
    async fn deliver_decision(&self, effect: &SpokeEffect) -> SpokeEffectOutcome {
        let mut command = match Command::decode(effect.payload.as_slice()) {
            Ok(command) => command,
            Err(error) => return SpokeEffectOutcome::Failed(format!("stored decision does not decode: {error}")),
        };
        if effect.rejections > 0 {
            command.command_id = format!("{}-r{}", command.command_id, effect.rejections);
        }
        let (host, id) = (effect.host_id.clone(), command.command_id.clone());
        let stored = match self.blocking(move |db| db.host_command_wire(&host, &id)).await {
            Ok(stored) => stored,
            Err(Interrupted::Retry(reason) | Interrupted::Halt(reason)) => return SpokeEffectOutcome::Retry(reason),
        };
        match stored {
            Some(wire) => match Command::decode(wire.as_slice()) {
                Ok(mut original) => {
                    original.sequence = 0;
                    command = original;
                }
                Err(error) => return SpokeEffectOutcome::Failed(format!("stored command does not decode: {error}")),
            },
            None => command.not_after_unix_ms = unix_ms(SystemTime::now() + self.config.decision_ttl),
        }
        match self
            .sender
            .send(&effect.host_id, command, self.config.delivery_deadline)
            .await
        {
            Ok(result) if result.code == ResultCode::Ok as i32 => SpokeEffectOutcome::Done,
            Ok(result) => {
                let reason = format!("the spoke refused the decision: {}", result.reason);
                if effect.rejections + 1 >= self.config.max_rejections {
                    SpokeEffectOutcome::Failed(reason)
                } else {
                    SpokeEffectOutcome::Rejected(reason)
                }
            }
            Err(CommandOutcome::Invalid | CommandOutcome::ResetReleaseUnsupported) => {
                SpokeEffectOutcome::Failed("the decision command is invalid".into())
            }
            Err(outcome) => SpokeEffectOutcome::Retry(outcome.to_string()),
        }
    }
}

/// An effect a record owes, held until its cursor advances.
#[derive(Clone)]
struct Owed {
    kind: &'static str,
    key: String,
    payload: Vec<u8>,
    failure: Option<String>,
}

impl Owed {
    fn quarantine(sequence: u64, reason: &str) -> Self {
        Self {
            kind: QUARANTINE,
            key: sequence.to_string(),
            payload: reason.as_bytes().to_vec(),
            failure: Some(reason.to_owned()),
        }
    }

    fn custody_halt(origin: &Origin, resource: &str, reason: &str) -> Self {
        Self {
            kind: CUSTODY_HALT,
            key: format!("{}:{resource}", origin.run.run_id),
            payload: reason.as_bytes().to_vec(),
            failure: Some(format!("{resource}: {reason}")),
        }
    }

    fn evidence_unavailable(origin: &Origin, mark: &Mark, reason: &str) -> Self {
        Self {
            kind: EVIDENCE_UNAVAILABLE,
            key: format!("{}:{}:{}", origin.run.run_id, mark.generation, mark.through),
            payload: reason.as_bytes().to_vec(),
            failure: Some(reason.to_owned()),
        }
    }

    fn pending_observation(origin: &Origin, kind: &str) -> Self {
        let payload = serde_json::to_vec(&Parked {
            run_id: origin.run.run_id.clone(),
            kind: kind.to_owned(),
            parked_at_ms: unix_ms(SystemTime::now()),
        })
        .unwrap_or_default();
        Self {
            kind: PENDING_OBSERVATION,
            key: format!("{}:{}", origin.run.run_id, origin.sequence),
            payload,
            failure: None,
        }
    }

    fn decision(proposal: &ProposalObservation, key: &str, decision: &Decision) -> Self {
        let mut result = serde_json::to_vec(&decision.result).unwrap_or_default();
        if result.len() > MAX_PAYLOAD_BYTES / 2 {
            result = serde_json::to_vec(&json!({"truncated": true})).unwrap_or_default();
        }
        let reason: String = decision.reason.chars().take(MAX_REASON_CHARS).collect();
        let digest = Sha256::digest(key.as_bytes());
        let command = Command {
            sequence: 0,
            command_id: format!("proposal-decision-{}", hex(&digest[..12])),
            run: Some(proposal.origin.run.clone()),
            // Filled in when the command is first sent, so its validity starts
            // then rather than when the decision was made.
            not_after_unix_ms: 0,
            body: Some(command::Body::ProposalDecision(ProposalDecision {
                proposal_id: proposal.proposal_id.clone(),
                code: (if decision.accepted {
                    ResultCode::Ok
                } else {
                    ResultCode::Rejected
                }) as i32,
                reason,
                result_json: result,
            })),
        };
        Self {
            kind: PROPOSAL_DECISION,
            key: key.to_owned(),
            payload: command.encode_to_vec(),
            failure: None,
        }
    }

    fn as_new(&self) -> NewSpokeEffect<'_> {
        match &self.failure {
            Some(error) => NewSpokeEffect::failed(self.kind, &self.key, &self.payload, error),
            None => NewSpokeEffect::pending(self.kind, &self.key, &self.payload),
        }
    }
}

fn observation_row(pane: &PaneObservation) -> SpokePaneObservation {
    let state = &pane.pane;
    SpokePaneObservation::builder()
        .execution_id(&pane.origin.run.execution_id)
        .assignment_generation(pane.origin.run.assignment_generation)
        .host_id(&pane.origin.host_id)
        .life(state.life)
        .session(&state.session)
        .pane_id(&state.pane_id)
        .pid(state.pid)
        .maybe_exit_code(state.exit_code)
        .boot_id(&state.boot_id)
        .source_sequence(pane.origin.sequence)
        .observed_at_ms(pane.origin.observed.wall_unix_ms)
        .build()
}

fn unix_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct Parked {
    run_id: String,
    kind: String,
    parked_at_ms: i64,
}

enum ParkedEnd {
    Applied,
    Waiting(Duration),
}

fn event_wants_capture(event: &EventObservation) -> bool {
    matches!(event.event, boss_protocol::WorkerEvent::Stop { .. })
}

fn proposal_wants_capture(proposal: &ProposalObservation) -> bool {
    matches!(
        proposal.request,
        ProposalRequest::Standard {
            kind: ProposalKind::RunDone,
            ..
        }
    )
}

fn parked_elapsed(parked_at_ms: i64) -> Duration {
    Duration::from_millis(unix_ms(SystemTime::now()).saturating_sub(parked_at_ms) as u64)
}

fn pending_timeout_reason(have: u64, want: u64, bound: Duration) -> String {
    if have == 0 && want == 1 {
        format!("named outputs did not arrive within {bound:?}")
    } else {
        format!("only {have} of {want} transcript bytes arrived within {bound:?}")
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
