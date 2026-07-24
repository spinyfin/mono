//! Boothby's guarded action executor — the runtime half of Boss's
//! autonomous groundskeeper.
//!
//! Boothby wakes on a timer, reasons about the taxonomy, and mutates it
//! without a human in the loop. Everything in this module exists to bound
//! what that can cost when its judgement is wrong.
//!
//! * [`catalogue`] — the fixed set of verbs Boothby has, each with its
//!   autonomy, blast-radius budget, and reversibility class. Closed by
//!   construction: a slug not in the table is refused.
//! * [`guards`] — the rails that stop it fighting a live worker, a recent
//!   human decision, or a held cube lease, plus the two-pass gate that makes
//!   an irreversible verb prove itself across two passes.
//! * [`executor`] — the choke point every mutation goes through, which
//!   applies all of the above and guarantees the journal.
//!
//! The journal's *write* half lives in [`crate::work::boothby`] instead,
//! next to the mutation layer, because a pre-image is only trustworthy if it
//! is captured in the same transaction as the write it describes.
//!
//! Design: `tools/boss/docs/designs/boothby.md`.

pub mod catalogue;
pub mod executor;
pub mod guards;

pub use catalogue::{Autonomy, CapGroup, JournalMode, Reversibility, VerbSpec};
pub use executor::{BoothbyExecutor, BoothbyMode, BoothbyPolicy, VerbHandler, action_fingerprint};
pub use guards::{Confirmation, GuardVerdict, TwoPassGate};
