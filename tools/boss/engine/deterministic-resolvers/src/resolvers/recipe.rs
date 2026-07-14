//! [`RecipeResolver`] — the one generic, data-driven
//! [`DeterministicResolver`] that interprets every configured
//! [`ConflictRecipe`]. One instance wraps one recipe (mirroring
//! [`crate::CargoLockResolver`]/[`crate::BazelModuleLockResolver`],
//! which are effectively hard-coded recipes); the caller constructs
//! one `RecipeResolver` per recipe loaded from
//! [`crate::ConflictRecipesStore`] and registers each into the
//! [`crate::ResolverRegistry`] via its existing `register` method — no
//! registry changes are needed for recipes to "drop in."

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use globset::GlobMatcher;

use crate::command::{CommandRunner, RealCommandRunner, run_or_decline};
use crate::recipe_config::ConflictRecipe;
use crate::{ConflictClass, ConflictedFile, DeterministicResolver, ResolveOutcome};

/// A [`ConflictRecipe`] that failed to compile into a [`RecipeResolver`]
/// — an invalid glob pattern or an empty command list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidRecipe(pub String);

impl fmt::Display for InvalidRecipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for InvalidRecipe {}

pub struct RecipeResolver {
    // Holds the whole recipe (rather than destructuring it into
    // separate fields) to keep this struct's field count under the
    // repo's giant-structs threshold without reaching for a builder —
    // `ConflictRecipe` is already the natural unit of recipe config.
    recipe: ConflictRecipe,
    matcher: GlobMatcher,
    runner: Arc<dyn CommandRunner>,
}

impl RecipeResolver {
    /// Compile a [`ConflictRecipe`] into a resolver. Fails if the glob
    /// doesn't parse or `resolve_command`/`verify_command` is empty —
    /// [`crate::ConflictRecipesStore::load`] already validates this,
    /// so a caller that loaded recipes through the store won't hit
    /// this in practice, but any other caller building a
    /// `ConflictRecipe` by hand still gets a clear error instead of a
    /// panic.
    pub fn from_recipe(recipe: ConflictRecipe) -> Result<Self, InvalidRecipe> {
        if recipe.resolve_command.is_empty() {
            return Err(InvalidRecipe(format!(
                "recipe {:?}: resolve_command must not be empty",
                recipe.name
            )));
        }
        if recipe.verify_command.as_ref().is_some_and(Vec::is_empty) {
            return Err(InvalidRecipe(format!(
                "recipe {:?}: verify_command must not be empty when set",
                recipe.name
            )));
        }
        let matcher = globset::Glob::new(&recipe.glob)
            .map_err(|e| InvalidRecipe(format!("recipe {:?}: invalid glob {:?}: {e}", recipe.name, recipe.glob)))?
            .compile_matcher();
        Ok(Self {
            recipe,
            matcher,
            runner: Arc::new(RealCommandRunner),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_runner(recipe: ConflictRecipe, runner: Arc<dyn CommandRunner>) -> Self {
        let mut resolver = Self::from_recipe(recipe).expect("test recipe must be valid");
        resolver.runner = runner;
        resolver
    }
}

#[async_trait]
impl DeterministicResolver for RecipeResolver {
    fn class(&self) -> ConflictClass {
        ConflictClass::Recipe
    }

    fn applies_to(&self, file: &ConflictedFile) -> bool {
        self.matcher.is_match(&file.path)
    }

    async fn resolve(&self, workspace_path: &Path, file: &ConflictedFile) -> ResolveOutcome {
        resolve_recipe(
            self.runner.as_ref(),
            workspace_path,
            file,
            &self.recipe.name,
            &self.recipe.resolve_command,
            self.recipe.verify_command.as_deref(),
            self.recipe.workdir.as_deref(),
        )
        .await
    }
}

/// The generic recipe formula: discard the conflicted file, run
/// `resolve_command`, then verify — either by running `verify_command`
/// (must exit 0) or, when none is configured, by re-checking the
/// target file exists. Both commands run with cwd = the conflicted
/// file's parent directory, unless `workdir` overrides it with a path
/// relative to the workspace root (needed for recipes whose command
/// must run at the workspace root, e.g. a top-level `make
/// regen-schema`).
async fn resolve_recipe(
    runner: &dyn CommandRunner,
    workspace_path: &Path,
    file: &ConflictedFile,
    recipe_name: &str,
    resolve_command: &[String],
    verify_command: Option<&[String]>,
    workdir: Option<&str>,
) -> ResolveOutcome {
    let target_path = workspace_path.join(&file.path);
    let dir = match workdir {
        Some(workdir) => workspace_path.join(workdir),
        None => match target_path.parent() {
            Some(dir) => dir.to_path_buf(),
            None => {
                return ResolveOutcome::Declined {
                    reason: format!("recipe {recipe_name:?}: {} has no parent directory", file.path),
                };
            }
        },
    };
    let dir = dir.as_path();

    // Discard the conflicted content so the resolve command writes
    // fresh output instead of tripping over merge markers — same
    // rationale as the built-in lockfile resolvers.
    if let Err(e) = std::fs::remove_file(&target_path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return ResolveOutcome::Declined {
            reason: format!("recipe {recipe_name:?}: failed to remove conflicted {}: {e}", file.path),
        };
    }

    if let Err(outcome) = run_command(runner, dir, recipe_name, "resolve_command", resolve_command).await {
        return outcome;
    }

    if let Some(verify) = verify_command {
        if let Err(outcome) = run_command(runner, dir, recipe_name, "verify_command", verify).await {
            return outcome;
        }
    } else if !target_path.is_file() {
        return ResolveOutcome::Declined {
            reason: format!(
                "recipe {recipe_name:?}: resolve_command succeeded but did not regenerate {}",
                file.path
            ),
        };
    }

    ResolveOutcome::Resolved {
        summary: format!(
            "recipe {recipe_name:?} resolved {} via `{}`",
            file.path,
            resolve_command.join(" ")
        ),
    }
}

/// Runs one recipe command (`resolve_command` or `verify_command`) via
/// the shared [`run_or_decline`] core. `field_name` is only used to make
/// the decline reason legible.
async fn run_command(
    runner: &dyn CommandRunner,
    dir: &Path,
    recipe_name: &str,
    field_name: &str,
    command: &[String],
) -> Result<(), ResolveOutcome> {
    let (program, args) = command
        .split_first()
        .expect("recipe commands are validated non-empty at construction");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    run_or_decline(
        runner,
        dir,
        program,
        &args,
        &format!("recipe {recipe_name:?} ({field_name}): "),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{CommandOutput, FakeCommandRunner};

    fn success_output() -> CommandOutput {
        CommandOutput {
            success: true,
            code: Some(0),
            stderr: String::new(),
        }
    }

    fn failure_output(stderr: &str) -> CommandOutput {
        CommandOutput {
            success: false,
            code: Some(1),
            stderr: stderr.to_owned(),
        }
    }

    fn file(path: &str) -> ConflictedFile {
        ConflictedFile {
            path: path.to_owned(),
            marker_count: Some(1),
            shape: "content".to_owned(),
        }
    }

    fn recipe(name: &str, glob: &str, resolve_command: &[&str]) -> ConflictRecipe {
        ConflictRecipe {
            name: name.to_owned(),
            glob: glob.to_owned(),
            resolve_command: resolve_command.iter().map(|s| s.to_string()).collect(),
            verify_command: None,
            workdir: None,
        }
    }

    #[test]
    fn from_recipe_rejects_empty_resolve_command() {
        let bad = recipe("x", "*.lock", &[]);
        assert!(RecipeResolver::from_recipe(bad).is_err());
    }

    #[test]
    fn from_recipe_rejects_invalid_glob() {
        let bad = recipe("x", "[", &["true"]);
        assert!(RecipeResolver::from_recipe(bad).is_err());
    }

    #[test]
    fn from_recipe_rejects_empty_verify_command() {
        let mut bad = recipe("x", "*.lock", &["true"]);
        bad.verify_command = Some(Vec::new());
        assert!(RecipeResolver::from_recipe(bad).is_err());
    }

    #[test]
    fn applies_to_matches_configured_glob() {
        let r = recipe("schema", "**/schema.generated.json", &["true"]);
        let resolver = RecipeResolver::from_recipe(r).unwrap();
        assert!(resolver.applies_to(&file("api/schema.generated.json")));
        assert!(!resolver.applies_to(&file("api/schema.json")));
    }

    #[tokio::test]
    async fn resolve_runs_command_and_accepts_when_target_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("schema.generated.json"), "<<<<<<< ours\n").unwrap();

        let runner = Arc::new(FakeCommandRunner::success_writing_file("schema.generated.json", "{}\n"));
        let r = recipe("schema", "**/schema.generated.json", &["make", "regen-schema"]);
        let resolver = RecipeResolver::with_runner(r, runner.clone());

        let outcome = resolver.resolve(dir.path(), &file("schema.generated.json")).await;
        assert!(
            matches!(outcome, ResolveOutcome::Resolved { .. }),
            "outcome: {outcome:?}"
        );

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "make");
        assert_eq!(calls[0].1, vec!["regen-schema".to_owned()]);
    }

    #[tokio::test]
    async fn resolve_declines_when_command_succeeds_but_target_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("schema.generated.json"), "<<<<<<< ours\n").unwrap();

        let runner = Arc::new(FakeCommandRunner::success());
        let r = recipe("schema", "**/schema.generated.json", &["make", "regen-schema"]);
        let resolver = RecipeResolver::with_runner(r, runner);

        let outcome = resolver.resolve(dir.path(), &file("schema.generated.json")).await;
        match outcome {
            ResolveOutcome::Declined { reason } => assert!(reason.contains("did not regenerate")),
            other => panic!("expected Declined, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_declines_when_resolve_command_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("schema.generated.json"), "<<<<<<< ours\n").unwrap();

        let runner = Arc::new(FakeCommandRunner::failure("regen failed: bad input"));
        let r = recipe("schema", "**/schema.generated.json", &["make", "regen-schema"]);
        let resolver = RecipeResolver::with_runner(r, runner);

        let outcome = resolver.resolve(dir.path(), &file("schema.generated.json")).await;
        match outcome {
            ResolveOutcome::Declined { reason } => assert!(reason.contains("regen failed: bad input")),
            other => panic!("expected Declined, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_runs_verify_command_and_declines_when_it_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("schema.generated.json"), "<<<<<<< ours\n").unwrap();

        let runner = Arc::new(FakeCommandRunner::sequence(vec![
            Ok(success_output()),
            Ok(failure_output("schema invalid")),
        ]));
        let mut r = recipe("schema", "**/schema.generated.json", &["make", "regen-schema"]);
        r.verify_command = Some(vec!["make".to_owned(), "validate-schema".to_owned()]);
        let resolver = RecipeResolver::with_runner(r, runner.clone());

        let outcome = resolver.resolve(dir.path(), &file("schema.generated.json")).await;
        match outcome {
            ResolveOutcome::Declined { reason } => {
                assert!(reason.contains("verify_command"), "reason: {reason}");
                assert!(reason.contains("schema invalid"), "reason: {reason}");
            }
            other => panic!("expected Declined, got {other:?}"),
        }

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["regen-schema".to_owned()]);
        assert_eq!(calls[1].1, vec!["validate-schema".to_owned()]);
    }

    #[tokio::test]
    async fn resolve_accepts_when_verify_command_succeeds_even_without_target_file() {
        // verify_command is authoritative when configured — no
        // fallback "does the file exist" check needed.
        let dir = tempfile::tempdir().unwrap();
        // Note: no file written at all — resolve_command in this test
        // doesn't touch the filesystem (e.g. it might regenerate a
        // sibling file, or the recipe's "target" is really validated
        // by verify_command).
        let runner = Arc::new(FakeCommandRunner::sequence(vec![
            Ok(success_output()),
            Ok(success_output()),
        ]));
        let mut r = recipe("schema", "**/schema.generated.json", &["make", "regen-schema"]);
        r.verify_command = Some(vec!["make".to_owned(), "validate-schema".to_owned()]);
        let resolver = RecipeResolver::with_runner(r, runner);

        let outcome = resolver.resolve(dir.path(), &file("schema.generated.json")).await;
        assert!(
            matches!(outcome, ResolveOutcome::Resolved { .. }),
            "outcome: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn resolve_tolerates_target_already_absent_before_running_command() {
        let dir = tempfile::tempdir().unwrap();
        // No conflicted file on disk at all (already removed upstream).
        let runner = Arc::new(FakeCommandRunner::success_writing_file("schema.generated.json", "{}\n"));
        let r = recipe("schema", "**/schema.generated.json", &["make", "regen-schema"]);
        let resolver = RecipeResolver::with_runner(r, runner);

        let outcome = resolver.resolve(dir.path(), &file("schema.generated.json")).await;
        assert!(matches!(outcome, ResolveOutcome::Resolved { .. }));
    }

    #[tokio::test]
    async fn resolve_declines_when_resolve_command_fails_to_spawn() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("schema.generated.json"), "<<<<<<< ours\n").unwrap();

        let runner = Arc::new(FakeCommandRunner::spawn_error());
        let r = recipe("schema", "**/schema.generated.json", &["make", "regen-schema"]);
        let resolver = RecipeResolver::with_runner(r, runner);

        let outcome = resolver.resolve(dir.path(), &file("schema.generated.json")).await;
        match outcome {
            ResolveOutcome::Declined { reason } => assert!(reason.contains("failed to spawn"), "reason: {reason}"),
            other => panic!("expected Declined, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_runs_command_in_configured_workdir_instead_of_targets_parent() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("api")).unwrap();
        std::fs::write(workspace.path().join("api/schema.generated.json"), "<<<<<<< ours\n").unwrap();

        // Command writes its output relative to `dir` (the cwd it's run
        // with), so if `workdir` isn't honored the file lands in the
        // wrong place and the resolve fails to find it.
        let runner = Arc::new(FakeCommandRunner::success_writing_file(
            "api/schema.generated.json",
            "{}\n",
        ));
        let mut r = recipe("schema", "**/schema.generated.json", &["make", "regen-schema"]);
        r.workdir = Some(".".to_owned());
        let resolver = RecipeResolver::with_runner(r, runner.clone());

        let outcome = resolver
            .resolve(workspace.path(), &file("api/schema.generated.json"))
            .await;
        assert!(
            matches!(outcome, ResolveOutcome::Resolved { .. }),
            "outcome: {outcome:?}"
        );

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls[0].2, workspace.path());
    }
}
