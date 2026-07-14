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

        for file in files {
            match self.resolvers.iter().find(|resolver| resolver.applies_to(file)) {
                Some(resolver) => match resolver.resolve(workspace_path, file).await {
                    ResolveOutcome::Resolved { summary } => resolved.push(ResolvedFile {
                        path: file.path.clone(),
                        class: resolver.class(),
                        summary,
                    }),
                    ResolveOutcome::Declined { reason } => declined.push(DeclinedFile {
                        path: file.path.clone(),
                        reason,
                    }),
                },
                None => declined.push(DeclinedFile {
                    path: file.path.clone(),
                    reason: "no registered resolver applies to this file".to_owned(),
                }),
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
    use super::*;
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
