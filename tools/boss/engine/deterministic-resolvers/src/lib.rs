//! Deterministic resolver framework — rung 0 of the merge-conflict
//! escalation ladder described in
//! `docs/designs/merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`
//! (§"Rung 0 — Deterministic resolvers").
//!
//! The engine core stays agnostic to any particular repo's tooling: all
//! format knowledge lives behind the [`DeterministicResolver`] trait and
//! its [`ResolverRegistry`], which is the extension seam the design calls
//! out: built-ins today, plus [`RecipeResolver`] — the declarative
//! recipe mechanism (design §"Extension mechanism — declarative
//! resolution recipes", tracked as T13) — for user-configured formulas
//! that don't warrant a compiled-in resolver. See [`recipe_config`] for
//! the recipe format and its boss-side config file. This crate is
//! standalone and unit-tested against fixture conflicts; it is not yet
//! wired into `conflict_watch` (a later task).

mod command;
mod lockfile;
mod recipe_config;
mod registry;
mod resolvers;

use std::path::Path;

use async_trait::async_trait;
pub use boss_conflict_diagnosis::ConflictedFile;

pub use recipe_config::{ConflictRecipe, ConflictRecipesStore};
pub use registry::{DeclinedFile, RegistryResolution, ResolvedFile, ResolverRegistry};
pub use resolvers::{
    BazelModuleLockResolver, CargoLockResolver, InvalidRecipe, RecipeResolver, RegistryAppendUnionResolver,
};

/// Coarse classification of a conflicted file's resolution strategy, kept
/// for telemetry attribution (`conflict_resolutions.conflict_class`,
/// wired up by a later task).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictClass {
    CargoLock,
    BazelModuleLock,
    RegistryAppendUnion,
    /// Resolved by a [`RecipeResolver`] — a user-configured declarative
    /// recipe rather than a compiled-in resolver. Coarse on purpose;
    /// [`ResolvedFile::summary`] carries the specific recipe name.
    Recipe,
}

impl ConflictClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConflictClass::CargoLock => "cargo_lock",
            ConflictClass::BazelModuleLock => "bazel_module_lock",
            ConflictClass::RegistryAppendUnion => "registry_append_union",
            ConflictClass::Recipe => "recipe",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_class_as_str_maps_each_variant_to_its_telemetry_label() {
        // These strings are persisted as telemetry
        // (`conflict_resolutions.conflict_class`); pin the exact wire
        // values so a rename can't silently change the recorded label.
        assert_eq!(ConflictClass::CargoLock.as_str(), "cargo_lock");
        assert_eq!(ConflictClass::BazelModuleLock.as_str(), "bazel_module_lock");
        assert_eq!(ConflictClass::RegistryAppendUnion.as_str(), "registry_append_union");
    }
}
