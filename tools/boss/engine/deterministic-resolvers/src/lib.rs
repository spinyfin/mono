//! Deterministic resolver framework — rung 0 of the merge-conflict
//! escalation ladder described in
//! `docs/designs/merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`
//! (§"Rung 0 — Deterministic resolvers").
//!
//! The engine core stays agnostic to any particular repo's tooling: all
//! format knowledge lives behind the [`DeterministicResolver`] trait and
//! its [`ResolverRegistry`], which is the extension seam the design calls
//! out (built-ins today; declarative recipes and user-authored resolvers
//! later). This crate is standalone and unit-tested against fixture
//! conflicts; it is not yet wired into `conflict_watch` (a later task).

mod command;
mod lockfile;
mod registry;
mod resolvers;

use std::path::Path;

use async_trait::async_trait;
pub use boss_conflict_diagnosis::ConflictedFile;

pub use registry::{DeclinedFile, RegistryResolution, ResolvedFile, ResolverRegistry};
pub use resolvers::{BazelModuleLockResolver, CargoLockResolver, RegistryAppendUnionResolver};

/// Coarse classification of a conflicted file's resolution strategy, kept
/// for telemetry attribution (`conflict_resolutions.conflict_class`,
/// wired up by a later task).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictClass {
    CargoLock,
    BazelModuleLock,
    RegistryAppendUnion,
}

impl ConflictClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConflictClass::CargoLock => "cargo_lock",
            ConflictClass::BazelModuleLock => "bazel_module_lock",
            ConflictClass::RegistryAppendUnion => "registry_append_union",
        }
    }
}

/// One formulaic-conflict resolution strategy, dispatched by
/// [`DeterministicResolver::applies_to`] against a [`ConflictedFile`].
///
/// `applies_to` should be a cheap, side-effect-free predicate over the
/// file's path/shape; `resolve` may shell out (e.g. to regenerate a
/// lockfile) and is the only method allowed to touch the workspace.
#[async_trait]
pub trait DeterministicResolver: Send + Sync {
    /// Class used for telemetry attribution.
    fn class(&self) -> ConflictClass;

    /// Whether this resolver knows how to handle `file`. The registry
    /// tries resolvers in registration order and dispatches to the first
    /// match.
    fn applies_to(&self, file: &ConflictedFile) -> bool;

    /// Attempt the resolution. `workspace_path` is the root of the leased
    /// workspace; `file.path` is relative to it.
    async fn resolve(&self, workspace_path: &Path, file: &ConflictedFile) -> ResolveOutcome;
}

/// Outcome of a single resolver's attempt at a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// The file is resolved; its content on disk represents both sides'
    /// changes.
    Resolved { summary: String },
    /// This resolver could not handle this instance. The caller must
    /// climb to the next rung rather than treat this as a partial
    /// success — see [`RegistryResolution`].
    Declined { reason: String },
}
