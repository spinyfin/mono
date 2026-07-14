//! Declarative resolution recipes — the rung-0 extension mechanism
//! described in the design's "Extension mechanism — declarative
//! resolution recipes" section
//! (`docs/designs/merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`,
//! tracked as T13).
//!
//! A recipe is a data-driven `DeterministicResolver`: a file-glob
//! paired with a resolution formula ("discard the conflicted file, run
//! `<command>`, verify with `<command>`" — the same shape as the
//! built-in lockfile resolvers, generalised). [`RecipeResolver`]
//! (`resolvers::recipe`) is the one generic resolver that interprets
//! every [`ConflictRecipe`]; this module owns the recipe data type and
//! its boss-side config file.
//!
//! ## Trust boundary — boss-side config only
//!
//! Per the design's explicit recommendation, this module loads recipes
//! from a **boss-side** TOML file (analogous to
//! `boss-feature-flags`'s `feature-flags.toml`), never from a file
//! inside the target repo. Boss-side config means a PR to the target
//! repo cannot alter what the engine will execute on the next
//! conflict — an in-repo recipe file is attacker-adjacent input in a
//! world of agent-authored PRs. In-repo recipe files remain a
//! follow-up that must answer the trust question explicitly (e.g.
//! "only honor recipes as they exist on `main`, never from the PR
//! branch") before they're honored.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// One declarative resolution recipe: a file-glob paired with the
/// discard/run/verify formula. Deliberately minimal — the formula is
/// always "discard the conflicted file, run `resolve_command`, then
/// verify" (either via `verify_command` if set, or by re-checking the
/// target file exists, mirroring the built-in lockfile resolvers).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ConflictRecipe {
    /// Stable identifier, used in telemetry summaries and decline
    /// reasons so operators can tell which recipe fired.
    pub name: String,
    /// `globset`-syntax pattern matched against the conflicted file's
    /// path (relative to the workspace root), e.g. `"**/Cargo.lock"`.
    /// The registry tries resolvers — recipes included — in
    /// registration order, so when two recipes could both match a
    /// path, the one listed first in the config file wins.
    pub glob: String,
    /// Program + args run (with cwd = the conflicted file's parent
    /// directory, unless `workdir` overrides it) after the conflicted
    /// file is discarded, e.g. `["cargo", "generate-lockfile"]`. Must
    /// be non-empty.
    pub resolve_command: Vec<String>,
    /// Optional program + args that must exit 0 to accept the
    /// resolution, run (same cwd as `resolve_command`) after it
    /// succeeds. `None` falls back to verifying the target file exists
    /// again, same as the built-in lockfile resolvers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_command: Option<Vec<String>>,
    /// Optional cwd override, relative to the workspace root, for both
    /// `resolve_command` and `verify_command`. Defaults to the
    /// conflicted file's parent directory, which is non-overridable
    /// without this field — set it for recipes whose command must run
    /// at the workspace root or another fixed location (e.g. a
    /// top-level `make regen-schema`, or `bazel mod deps` for a nested
    /// `MODULE.bazel`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
}

/// On-disk file shape: a flat list of recipes under a `[[recipe]]`
/// TOML array-of-tables, e.g.:
///
/// ```toml
/// [[recipe]]
/// name = "cargo_lock"
/// glob = "**/Cargo.lock"
/// resolve_command = ["cargo", "generate-lockfile"]
/// ```
#[derive(Debug, Deserialize)]
struct FileShape {
    #[serde(default)]
    recipe: Vec<ConflictRecipe>,
}

/// Boss-side config store for declarative recipes. Read-only by
/// design: recipes are operator-authored (hand-edited TOML), not
/// mutated through an RPC surface the way feature flags are.
pub struct ConflictRecipesStore {
    path: PathBuf,
}

impl ConflictRecipesStore {
    /// Build a store rooted at the given file path. The file does not
    /// have to exist yet — [`Self::load`] tolerates a missing file and
    /// returns an empty recipe list (rung 0 simply has no recipes
    /// configured).
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Convenience: derive the default file path from the Boss state
    /// root (i.e. the directory holding `state.db`), mirroring
    /// `boss_feature_flags::FeatureFlagsStore::default_path`.
    pub fn default_path(state_root: &Path) -> PathBuf {
        state_root.join("conflict-recipes.toml")
    }

    /// Path the store reads from. Test-only callers can inspect it.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Parse the config file into recipes. A missing file yields an
    /// empty `Vec` (not configured). A malformed file, or a recipe
    /// with an empty `name`/`glob`/`resolve_command`, returns `Err` —
    /// callers should log and decline to register any recipes rather
    /// than partially trusting a broken config.
    pub fn load(&self) -> Result<Vec<ConflictRecipe>> {
        let contents = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("read conflict recipes file: {}", self.path.display()));
            }
        };
        let parsed: FileShape = toml::from_str(&contents)
            .with_context(|| format!("parse conflict recipes file: {}", self.path.display()))?;

        for recipe in &parsed.recipe {
            validate(recipe).with_context(|| format!("invalid recipe {:?} in {}", recipe.name, self.path.display()))?;
        }

        Ok(parsed.recipe)
    }
}

fn validate(recipe: &ConflictRecipe) -> Result<()> {
    if recipe.name.trim().is_empty() {
        anyhow::bail!("recipe name must not be empty");
    }
    if recipe.glob.trim().is_empty() {
        anyhow::bail!("recipe glob must not be empty");
    }
    if recipe.resolve_command.is_empty() {
        anyhow::bail!("recipe resolve_command must not be empty");
    }
    if let Some(verify) = &recipe.verify_command
        && verify.is_empty()
    {
        anyhow::bail!("recipe verify_command must not be empty when set");
    }
    globset::Glob::new(&recipe.glob).with_context(|| format!("invalid glob pattern {:?}", recipe.glob))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_returns_empty_list() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ConflictRecipesStore::new(tmp.path().join("conflict-recipes.toml"));
        assert_eq!(store.load().unwrap(), Vec::new());
    }

    #[test]
    fn default_path_joins_state_root() {
        let root = Path::new("/var/boss/state");
        assert_eq!(
            ConflictRecipesStore::default_path(root),
            PathBuf::from("/var/boss/state/conflict-recipes.toml")
        );
    }

    #[test]
    fn parses_recipes_with_and_without_verify_command() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("conflict-recipes.toml");
        std::fs::write(
            &path,
            r#"
[[recipe]]
name = "cargo_lock"
glob = "**/Cargo.lock"
resolve_command = ["cargo", "generate-lockfile"]

[[recipe]]
name = "generated_schema"
glob = "**/schema.generated.json"
resolve_command = ["make", "regen-schema"]
verify_command = ["make", "validate-schema"]
"#,
        )
        .unwrap();

        let store = ConflictRecipesStore::new(path);
        let recipes = store.load().unwrap();
        assert_eq!(recipes.len(), 2);
        assert_eq!(recipes[0].name, "cargo_lock");
        assert_eq!(recipes[0].verify_command, None);
        assert_eq!(
            recipes[1].verify_command,
            Some(vec!["make".to_owned(), "validate-schema".to_owned()])
        );
        assert_eq!(recipes[0].workdir, None);
    }

    #[test]
    fn parses_recipe_with_workdir_override() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("conflict-recipes.toml");
        std::fs::write(
            &path,
            r#"
[[recipe]]
name = "generated_schema"
glob = "**/schema.generated.json"
resolve_command = ["make", "regen-schema"]
workdir = "."
"#,
        )
        .unwrap();

        let store = ConflictRecipesStore::new(path);
        let recipes = store.load().unwrap();
        assert_eq!(recipes.len(), 1);
        assert_eq!(recipes[0].workdir.as_deref(), Some("."));
    }

    #[test]
    fn malformed_toml_errs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("conflict-recipes.toml");
        std::fs::write(&path, "this is = not = valid toml = at = all").unwrap();
        let store = ConflictRecipesStore::new(path);
        assert!(store.load().is_err());
    }

    #[test]
    fn empty_name_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("conflict-recipes.toml");
        std::fs::write(
            &path,
            "[[recipe]]\nname = \"\"\nglob = \"*.lock\"\nresolve_command = [\"true\"]\n",
        )
        .unwrap();
        let store = ConflictRecipesStore::new(path);
        let err = store.load().unwrap_err();
        assert!(format!("{err:#}").contains("name"), "error was: {err:#}");
    }

    #[test]
    fn empty_glob_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("conflict-recipes.toml");
        std::fs::write(
            &path,
            "[[recipe]]\nname = \"x\"\nglob = \"\"\nresolve_command = [\"true\"]\n",
        )
        .unwrap();
        let store = ConflictRecipesStore::new(path);
        assert!(store.load().is_err());
    }

    #[test]
    fn empty_resolve_command_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("conflict-recipes.toml");
        std::fs::write(
            &path,
            "[[recipe]]\nname = \"x\"\nglob = \"*.lock\"\nresolve_command = []\n",
        )
        .unwrap();
        let store = ConflictRecipesStore::new(path);
        assert!(store.load().is_err());
    }

    #[test]
    fn empty_verify_command_is_rejected_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("conflict-recipes.toml");
        std::fs::write(
            &path,
            "[[recipe]]\nname = \"x\"\nglob = \"*.lock\"\nresolve_command = [\"true\"]\nverify_command = []\n",
        )
        .unwrap();
        let store = ConflictRecipesStore::new(path);
        assert!(store.load().is_err());
    }

    #[test]
    fn invalid_glob_syntax_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("conflict-recipes.toml");
        std::fs::write(
            &path,
            "[[recipe]]\nname = \"x\"\nglob = \"[\"\nresolve_command = [\"true\"]\n",
        )
        .unwrap();
        let store = ConflictRecipesStore::new(path);
        assert!(store.load().is_err());
    }
}
