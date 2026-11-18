use std::path::Path;

use async_trait::async_trait;

use crate::command::RealCommandRunner;
use crate::lockfile::regenerate_lockfile;
use crate::{ConflictClass, ConflictedFile, DeterministicResolver, ResolveOutcome};

/// Other supported package-manager lockfiles use the same base-and-regenerate contract.
pub(crate) struct GeneratedLockResolver {
    pub base_revision: Option<String>,
}

fn recipe(path: &str) -> Option<(&'static str, &'static str, &'static [&'static str])> {
    match Path::new(path).file_name()?.to_str()? {
        "package-lock.json" => Some((
            "package.json",
            "npm",
            &["install", "--package-lock-only", "--ignore-scripts"],
        )),
        "yarn.lock" => Some(("package.json", "yarn", &["install", "--mode=update-lockfile"])),
        "pnpm-lock.yaml" => Some((
            "package.json",
            "pnpm",
            &["install", "--lockfile-only", "--ignore-scripts"],
        )),
        "uv.lock" => Some(("pyproject.toml", "uv", &["lock"])),
        "poetry.lock" => Some(("pyproject.toml", "poetry", &["lock"])),
        _ => None,
    }
}

pub fn is_generated_lockfile(path: &str) -> bool {
    recipe(path).is_some()
        || matches!(
            Path::new(path).file_name().and_then(|name| name.to_str()),
            Some("Cargo.lock" | "MODULE.bazel.lock")
        )
}

#[async_trait]
impl DeterministicResolver for GeneratedLockResolver {
    fn class(&self) -> ConflictClass {
        ConflictClass::GeneratedLock
    }

    fn applies_to(&self, file: &ConflictedFile) -> bool {
        recipe(&file.path).is_some()
    }

    async fn resolve(&self, workspace_path: &Path, file: &ConflictedFile) -> ResolveOutcome {
        let Some((manifest, program, args)) = recipe(&file.path) else {
            return ResolveOutcome::Declined {
                reason: "no package-manager lockfile recipe".into(),
            };
        };
        regenerate_lockfile(
            &RealCommandRunner,
            workspace_path,
            file,
            manifest,
            program,
            args,
            self.base_revision.as_deref(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_lockfiles_have_owner_commands_and_nested_paths_are_recognized() {
        for (path, tool) in [
            ("package-lock.json", "npm"),
            ("yarn.lock", "yarn"),
            ("pnpm-lock.yaml", "pnpm"),
            ("uv.lock", "uv"),
            ("poetry.lock", "poetry"),
        ] {
            let path = format!("nested/{path}");
            assert!(is_generated_lockfile(&path));
            assert_eq!(recipe(&path).unwrap().1, tool);
        }
        assert!(is_generated_lockfile("Cargo.lock"));
        assert!(is_generated_lockfile("MODULE.bazel.lock"));
        assert!(!is_generated_lockfile("src/lib.rs"));
    }
}
