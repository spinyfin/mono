//! Hub-owned durable transactions. Implementations must offload blocking DB I/O.
//! No network write occurs while a transaction is held. Errors never imply a
//! command was not committed: callers reconcile by its stable command ID.
//! Operations must be cancellation-safe and bounded: dropped futures cannot
//! roll back a committed request or leave a transaction/lock alive indefinitely.

use boss_protocol_spoke::{negotiate::Selection, v1::*};

#[derive(Debug, Clone, thiserror::Error)]
pub enum StoreError {
    #[error("replay_gap")]
    ReplayGap,
    #[error("not permitted")]
    Denied,
    #[error("durable quota exhausted")]
    Full,
    #[error("durable store unavailable")]
    Unavailable,
}

/// Immutable contract and identity attached to every admitted record.
#[derive(Debug, Clone)]
pub struct Session {
    pub host_id: String,
    pub journal_id: String,
    pub generation: u64,
    pub selection: Selection,
}

pub struct Admission {
    pub generation: u64,
    pub received: Cursor,
}

#[tonic::async_trait]
pub trait SpokeObservationSink: Send + Sync {
    /// Atomically validate the persisted journal identity/history, increment a
    /// durable connection generation, and return the committed record receipt.
    /// New journals require explicit authorization; never reset a known cursor.
    /// Failure must not change the generation. Called under the per-host gate.
    async fn open(&self, hello: &Hello, selection: &Selection) -> Result<Admission, StoreError>;

    /// In one transaction check generation AND assignment ownership (including
    /// every inventory member), quota and immutable duplicate bytes; store the
    /// original SpokeFrame bytes with the selected contract; advance receipt.
    /// For a result, atomically verify the outbox command ID/Run and persist its
    /// terminal result in that SAME transaction (visible through `completed`).
    /// Sink and outbox share the hub database: a crash must never advance receipt
    /// past a result that command waiters cannot subsequently query.
    /// Accept only a contiguous record or an identical duplicate. No projection
    /// or external effect belongs here. Return only after durable commit.
    async fn commit(&self, session: &Session, raw_frame: &[u8], record: &Record) -> Result<Cursor, StoreError>;
}

/// An original encoded Command and the outbox sequence it was stored under.
pub struct PendingCommand {
    pub sequence: u64,
    pub bytes: Vec<u8>,
}

pub struct CommandJournal {
    pub journal_id: String,
    pub received: u64,
}

#[tonic::async_trait]
pub trait CommandOutbox: Send + Sync {
    /// Validate journal continuity, monotonic receipt and its upper bound against
    /// persisted commands, then durably record the peer's receipt. Missing or
    /// changed identity on an established journal is ReplayGap, even at zero.
    async fn resume(&self, host: &str, receipt: Option<&Cursor>) -> Result<CommandJournal, StoreError>;

    /// Persist an already-authorized request/assignment and allocate sequence.
    /// Input sequence is zero. Reusing an ID with identical input returns the
    /// original command; changed input is denied. Enforce durable count/byte
    /// quotas, including unresolved receipted commands. Never delete on timeout.
    async fn append(&self, host: &str, command: Command) -> Result<Command, StoreError>;

    /// Return at most one original encoded Command after `after`, in sequence
    /// order, skipping sequences settled by `reject`. Retain these bytes
    /// unchanged on retry and across software upgrades. Delivered sequences may
    /// therefore be non-contiguous; they are always strictly increasing.
    async fn next(&self, host: &str, after: u64) -> Result<Option<PendingCommand>, StoreError>;

    /// Atomically record a terminal `REJECTED` result for a command that cannot
    /// be replayed on the current connection (for example, its feature pins no
    /// longer match the negotiated contract) and skip its sequence in `next`.
    /// The result must be visible through `completed`, so a retry with the same
    /// ID returns it. `command_id` is empty when the stored bytes cannot be
    /// decoded; the sequence is then skipped without recording a result.
    /// Idempotent: rejecting a settled command returns its existing result.
    async fn reject(
        &self,
        host: &str,
        sequence: u64,
        command_id: &str,
        reason: &str,
    ) -> Result<CommandResult, StoreError>;

    /// Durably advance receipt, validating identity, monotonicity and bounds.
    /// Receipt alone is never command completion.
    async fn receipt(&self, host: &str, receipt: &Cursor) -> Result<(), StoreError>;

    /// The immutable request, used to reject unknown or cross-run results before
    /// admitting their record. Must retain unresolved command tombstones.
    async fn command(&self, host: &str, command_id: &str) -> Result<Option<Command>, StoreError>;

    /// Return a previously committed result for retry/query by the same ID.
    async fn completed(&self, host: &str, command_id: &str) -> Result<Option<CommandResult>, StoreError>;
}
