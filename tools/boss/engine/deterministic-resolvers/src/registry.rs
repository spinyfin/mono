use std::path::Path;

use crate::resolvers::{BazelModuleLockResolver, CargoLockResolver, RegistryAppendUnionResolver};
use crate::{ConflictClass, ConflictedFile, DeterministicResolver, ResolveOutcome};

/// A file some resolver successfully resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFile {
    pub path: String,
    pub class: ConflictClass,
    pub summary: String,
}

/// A file no resolver could handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclinedFile {
    pub path: String,
    pub reason: String,
}

/// Outcome of running a whole conflicted-file set through the registry.
///
/// Per the rung-0 design: rung 0 only auto-retires when *every* file
/// resolves. A single decline means the caller must discard any partial
/// resolution and climb to the next rung — `resolved` on the `Declined`
/// variant is informational (useful for telemetry on how close rung 0
/// got), not a partial success to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryResolution {
    AllResolved(Vec<ResolvedFile>),
    Declined {
        resolved: Vec<ResolvedFile>,
        declined: Vec<DeclinedFile>,
    },
}

/// Registry of [`DeterministicResolver`]s, dispatched by
/// [`DeterministicResolver::applies_to`]. This is the extension seam: use
/// [`ResolverRegistry::with_builtins`] for the shipped lockfile
/// resolvers, or [`ResolverRegistry::empty`] plus
/// [`ResolverRegistry::register`] to build a custom set.
pub struct ResolverRegistry {
    resolvers: Vec<Box<dyn DeterministicResolver>>,
}

impl ResolverRegistry {
    pub fn empty() -> Self {
        Self { resolvers: Vec::new() }
    }

    pub fn with_builtins() -> Self {
        let mut registry = Self::empty();
        registry.register(Box::new(CargoLockResolver::new()));
        registry.register(Box::new(BazelModuleLockResolver::new()));
        registry.register(Box::new(RegistryAppendUnionResolver::new()));
        registry
    }

    pub fn register(&mut self, resolver: Box<dyn DeterministicResolver>) {
        self.resolvers.push(resolver);
    }

    /// Attempt every conflicted file against the registry. Returns
    /// `AllResolved` only if every file matched some resolver and that
    /// resolver resolved it; the first matching resolver in registration
    /// order is used per file.
    pub async fn resolve_all(&self, workspace_path: &Path, files: &[ConflictedFile]) -> RegistryResolution {
        let mut resolved = Vec::new();
        let mut declined = Vec::new();

        // One INFO trace line per resolver invocation (conflict class,
        // resolver id, outcome) so rung-0 activity is answerable from the
        // engine trace — the "runs silently" logging gap this crate had
        // before (mono#1398/#1764 diagnosis).
        for file in files {
            match self.resolvers.iter().find(|resolver| resolver.applies_to(file)) {
                Some(resolver) => {
                    let class = resolver.class();
                    match resolver.resolve(workspace_path, file).await {
                        ResolveOutcome::Resolved { summary } => {
                            tracing::info!(
                                path = %file.path,
                                conflict_class = class.as_str(),
                                outcome = "resolved",
                                summary = %summary,
                                "deterministic_resolvers: resolver resolved conflicted file",
                            );
                            resolved.push(ResolvedFile {
                                path: file.path.clone(),
                                class,
                                summary,
                            });
                        }
                        ResolveOutcome::Declined { reason } => {
                            tracing::info!(
                                path = %file.path,
                                conflict_class = class.as_str(),
                                outcome = "declined",
                                reason = %reason,
                                "deterministic_resolvers: resolver declined conflicted file",
                            );
                            declined.push(DeclinedFile {
                                path: file.path.clone(),
                                reason,
                            });
                        }
                    }
                }
                None => {
                    let reason = "no registered resolver applies to this file".to_owned();
                    tracing::info!(
                        path = %file.path,
                        outcome = "declined",
                        reason = %reason,
                        "deterministic_resolvers: no resolver applies to conflicted file",
                    );
                    declined.push(DeclinedFile {
                        path: file.path.clone(),
                        reason,
                    });
                }
            }
        }

        if declined.is_empty() {
            RegistryResolution::AllResolved(resolved)
        } else {
            RegistryResolution::Declined { resolved, declined }
        }
    }
}

impl Default for ResolverRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::command::FakeCommandRunner;
    use crate::resolvers::RecipeResolver;
    use async_trait::async_trait;

    struct AlwaysResolves {
        class: ConflictClass,
    }

    #[async_trait]
    impl DeterministicResolver for AlwaysResolves {
        fn class(&self) -> ConflictClass {
            self.class
        }
        fn applies_to(&self, _file: &ConflictedFile) -> bool {
            true
        }
        async fn resolve(&self, _workspace_path: &Path, _file: &ConflictedFile) -> ResolveOutcome {
            ResolveOutcome::Resolved {
                summary: "ok".to_owned(),
            }
        }
    }

    struct AlwaysDeclines;

    #[async_trait]
    impl DeterministicResolver for AlwaysDeclines {
        fn class(&self) -> ConflictClass {
            ConflictClass::CargoLock
        }
        fn applies_to(&self, _file: &ConflictedFile) -> bool {
            true
        }
        async fn resolve(&self, _workspace_path: &Path, _file: &ConflictedFile) -> ResolveOutcome {
            ResolveOutcome::Declined {
                reason: "nope".to_owned(),
            }
        }
    }

    /// Only claims one specific path, so a test can mix it with another
    /// resolver to produce a batch where some files resolve and one
    /// doesn't.
    struct ResolvesOnly {
        path: &'static str,
        class: ConflictClass,
    }

    #[async_trait]
    impl DeterministicResolver for ResolvesOnly {
        fn class(&self) -> ConflictClass {
            self.class
        }
        fn applies_to(&self, file: &ConflictedFile) -> bool {
            file.path == self.path
        }
        async fn resolve(&self, _workspace_path: &Path, _file: &ConflictedFile) -> ResolveOutcome {
            ResolveOutcome::Resolved {
                summary: "ok".to_owned(),
            }
        }
    }

    struct DeclinesOnly {
        path: &'static str,
    }

    #[async_trait]
    impl DeterministicResolver for DeclinesOnly {
        fn class(&self) -> ConflictClass {
            ConflictClass::CargoLock
        }
        fn applies_to(&self, file: &ConflictedFile) -> bool {
            file.path == self.path
        }
        async fn resolve(&self, _workspace_path: &Path, _file: &ConflictedFile) -> ResolveOutcome {
            ResolveOutcome::Declined {
                reason: "nope".to_owned(),
            }
        }
    }

    fn file(path: &str) -> ConflictedFile {
        ConflictedFile {
            path: path.to_owned(),
            marker_count: None,
            shape: "content".to_owned(),
        }
    }

    #[tokio::test]
    async fn all_resolved_when_every_file_matches() {
        let mut registry = ResolverRegistry::empty();
        registry.register(Box::new(AlwaysResolves {
            class: ConflictClass::CargoLock,
        }));
        let result = registry.resolve_all(Path::new("/tmp"), &[file("Cargo.lock")]).await;
        match result {
            RegistryResolution::AllResolved(files) => assert_eq!(files.len(), 1),
            other => panic!("expected AllResolved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn declined_when_no_resolver_applies() {
        let registry = ResolverRegistry::empty();
        let result = registry.resolve_all(Path::new("/tmp"), &[file("weird.bin")]).await;
        match result {
            RegistryResolution::Declined { declined, resolved } => {
                assert_eq!(declined.len(), 1);
                assert!(resolved.is_empty());
            }
            other => panic!("expected Declined, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn one_declined_file_declines_the_whole_batch_even_with_partial_success() {
        let mut registry = ResolverRegistry::empty();
        registry.register(Box::new(ResolvesOnly {
            path: "Cargo.lock",
            class: ConflictClass::CargoLock,
        }));
        registry.register(Box::new(DeclinesOnly {
            path: "MODULE.bazel.lock",
        }));

        let result = registry
            .resolve_all(Path::new("/tmp"), &[file("Cargo.lock"), file("MODULE.bazel.lock")])
            .await;

        match result {
            RegistryResolution::Declined { resolved, declined } => {
                assert_eq!(resolved.len(), 1);
                assert_eq!(resolved[0].path, "Cargo.lock");
                assert_eq!(declined.len(), 1);
                assert_eq!(declined[0].path, "MODULE.bazel.lock");
            }
            other => panic!("expected Declined even though one file resolved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn declined_batch_reports_resolver_reason() {
        let mut registry = ResolverRegistry::empty();
        registry.register(Box::new(AlwaysDeclines));

        let result = registry.resolve_all(Path::new("/tmp"), &[file("Cargo.lock")]).await;

        match result {
            RegistryResolution::Declined { resolved, declined } => {
                assert!(resolved.is_empty());
                assert_eq!(declined.len(), 1);
                assert_eq!(declined[0].reason, "nope");
            }
            other => panic!("expected Declined, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recipe_resolver_drops_into_the_registry_alongside_builtins() {
        // Proves the design's "drop in without rework" claim: a
        // declarative recipe registers through the same `register`
        // method as any compiled-in resolver — no registry API change
        // was needed to support recipes.
        let recipe = crate::ConflictRecipe {
            name: "generated_schema".to_owned(),
            glob: "**/schema.generated.json".to_owned(),
            resolve_command: vec!["make".to_owned(), "regen-schema".to_owned()],
            verify_command: None,
            workdir: None,
        };
        let runner = Arc::new(FakeCommandRunner::success_writing_file("schema.generated.json", "{}\n"));
        let recipe_resolver = RecipeResolver::with_runner(recipe, runner);

        let mut registry = ResolverRegistry::with_builtins();
        registry.register(Box::new(recipe_resolver));

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("schema.generated.json"), "<<<<<<< ours\n").unwrap();

        let result = registry.resolve_all(dir.path(), &[file("schema.generated.json")]).await;

        match result {
            RegistryResolution::AllResolved(resolved) => {
                assert_eq!(resolved.len(), 1);
                assert_eq!(resolved[0].class, ConflictClass::Recipe);
            }
            other => panic!("expected AllResolved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn first_registered_matching_resolver_wins_tie_break() {
        // Both resolvers apply to every file; registration order must
        // decide which one resolves it. First registered → CargoLock.
        let mut registry = ResolverRegistry::empty();
        registry.register(Box::new(AlwaysResolves {
            class: ConflictClass::CargoLock,
        }));
        registry.register(Box::new(AlwaysResolves {
            class: ConflictClass::BazelModuleLock,
        }));

        let result = registry.resolve_all(Path::new("/tmp"), &[file("Cargo.lock")]).await;

        match result {
            RegistryResolution::AllResolved(files) => {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].class, ConflictClass::CargoLock);
            }
            other => panic!("expected AllResolved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tie_break_follows_registration_order_not_resolver_identity() {
        // Symmetric to the previous test with the order swapped: the same
        // two resolvers, registered in the opposite order, now yield the
        // other class. This proves registration order — not which resolver
        // instance it is — decides the winner.
        let mut registry = ResolverRegistry::empty();
        registry.register(Box::new(AlwaysResolves {
            class: ConflictClass::BazelModuleLock,
        }));
        registry.register(Box::new(AlwaysResolves {
            class: ConflictClass::CargoLock,
        }));

        let result = registry.resolve_all(Path::new("/tmp"), &[file("Cargo.lock")]).await;

        match result {
            RegistryResolution::AllResolved(files) => {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].class, ConflictClass::BazelModuleLock);
            }
            other => panic!("expected AllResolved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn first_match_wins_even_when_it_declines() {
        // The first matching resolver declines; a later resolver would
        // have resolved the same file. Per the `.find()` semantics the
        // registry does NOT fall through — it records the first resolver's
        // decline rather than the second resolver's success.
        let mut registry = ResolverRegistry::empty();
        registry.register(Box::new(AlwaysDeclines));
        registry.register(Box::new(AlwaysResolves {
            class: ConflictClass::CargoLock,
        }));

        let result = registry.resolve_all(Path::new("/tmp"), &[file("Cargo.lock")]).await;

        match result {
            RegistryResolution::Declined { resolved, declined } => {
                assert!(resolved.is_empty(), "later resolver must not resolve the file");
                assert_eq!(declined.len(), 1);
                assert_eq!(declined[0].reason, "nope");
            }
            other => panic!("expected Declined from the first matching resolver, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn with_builtins_registers_cargo_and_bazel_lock_resolvers() {
        let registry = ResolverRegistry::with_builtins();
        assert!(registry.resolvers.iter().any(|r| r.applies_to(&file("Cargo.lock"))));
        assert!(
            registry
                .resolvers
                .iter()
                .any(|r| r.applies_to(&file("MODULE.bazel.lock")))
        );
        assert!(registry.resolvers.iter().any(|r| r.applies_to(&file("mod.rs"))));
        assert!(!registry.resolvers.iter().any(|r| r.applies_to(&file("random.txt"))));
    }
}
