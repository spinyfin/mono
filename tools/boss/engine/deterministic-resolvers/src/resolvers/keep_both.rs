//! The keep-both resolvers: when two branches each inserted something new
//! at the same spot, keep both insertions and reformat.
//!
//! [`KeepBothUnionResolver`] is the built-in. It applies to every Rust
//! source file and keeps only whole items and imports, whose order does
//! not change meaning. [`KeepBothRuleResolver`] is its declarative
//! counterpart for paths an operator names in the boss-side recipes file
//! (see [`crate::recipe_config`]); there it keeps any self-contained
//! lines, which is right for a list the operator knows is append-only.
//!
//! Both leave the verdict on the merged code to the checks every pushed
//! resolution already passes: the push gate, the PR's CI and the
//! both-parents deletion tripwire. Neither ever writes one side's
//! version, and neither keeps both when the two sides added the same
//! thing.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use globset::GlobMatcher;

use crate::command::{CommandRunner, RealCommandRunner};
use crate::keep_both::{KeepBothScope, keep_both_insertions};
use crate::recipe_config::KeepBothRule;
use crate::resolvers::InvalidRecipe;
use crate::{ConflictClass, ConflictedFile, DeterministicResolver, ResolveOutcome};

/// Keeps both sides of same-spot insertions of whole items and imports in
/// any Rust source file, then runs `rustfmt` on the result.
///
/// Declines, leaving the file exactly as it was, unless every conflict in
/// the file qualifies; see [`crate::keep_both`] for the rules. A result
/// `rustfmt` rejects is also a decline: it means the two insertions did
/// not combine into valid Rust.
pub struct KeepBothUnionResolver {
    runner: Arc<dyn CommandRunner>,
}

impl KeepBothUnionResolver {
    pub fn new() -> Self {
        Self {
            runner: Arc::new(RealCommandRunner),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_runner(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }
}

impl Default for KeepBothUnionResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DeterministicResolver for KeepBothUnionResolver {
    fn class(&self) -> ConflictClass {
        ConflictClass::KeepBothUnion
    }

    fn applies_to(&self, file: &ConflictedFile) -> bool {
        is_rust_source(&file.path)
    }

    async fn resolve(&self, workspace_path: &Path, file: &ConflictedFile) -> ResolveOutcome {
        apply_keep_both(
            self.runner.as_ref(),
            workspace_path,
            file,
            KeepBothScope::ItemsAndImports,
            &Formatter::Rustfmt,
        )
        .await
    }
}

/// A [`KeepBothRule`] compiled into a resolver: keep-both for the paths
/// the rule's glob names, including insertions that are not items.
pub struct KeepBothRuleResolver {
    rule: KeepBothRule,
    matcher: GlobMatcher,
    runner: Arc<dyn CommandRunner>,
}

impl KeepBothRuleResolver {
    /// Fails if the glob does not parse or `format_command` is set but
    /// empty. [`crate::ConflictRecipesStore`] validates both on load.
    pub fn from_rule(rule: KeepBothRule) -> Result<Self, InvalidRecipe> {
        if rule.format_command.as_ref().is_some_and(Vec::is_empty) {
            return Err(InvalidRecipe(format!(
                "keep_both rule {:?}: format_command must not be empty when set",
                rule.name
            )));
        }
        let matcher = globset::Glob::new(&rule.glob)
            .map_err(|e| {
                InvalidRecipe(format!(
                    "keep_both rule {:?}: invalid glob {:?}: {e}",
                    rule.name, rule.glob
                ))
            })?
            .compile_matcher();
        Ok(Self {
            rule,
            matcher,
            runner: Arc::new(RealCommandRunner),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_runner(rule: KeepBothRule, runner: Arc<dyn CommandRunner>) -> Self {
        let mut resolver = Self::from_rule(rule).expect("test rule must be valid");
        resolver.runner = runner;
        resolver
    }
}

#[async_trait]
impl DeterministicResolver for KeepBothRuleResolver {
    fn class(&self) -> ConflictClass {
        ConflictClass::Recipe
    }

    fn applies_to(&self, file: &ConflictedFile) -> bool {
        self.matcher.is_match(&file.path)
    }

    async fn resolve(&self, workspace_path: &Path, file: &ConflictedFile) -> ResolveOutcome {
        let formatter = match &self.rule.format_command {
            Some(command) => Formatter::Command(command.clone()),
            None if is_rust_source(&file.path) => Formatter::Rustfmt,
            None => Formatter::None,
        };
        let outcome = apply_keep_both(
            self.runner.as_ref(),
            workspace_path,
            file,
            KeepBothScope::WholeLines,
            &formatter,
        )
        .await;
        let rule = &self.rule.name;
        match outcome {
            ResolveOutcome::Resolved { summary } => ResolveOutcome::Resolved {
                summary: format!("keep_both rule {rule:?}: {summary}"),
            },
            ResolveOutcome::Declined { reason } => ResolveOutcome::Declined {
                reason: format!("keep_both rule {rule:?}: {reason}"),
            },
            ResolveOutcome::Failed { reason } => ResolveOutcome::Failed {
                reason: format!("keep_both rule {rule:?}: {reason}"),
            },
        }
    }
}

fn is_rust_source(path: &str) -> bool {
    Path::new(path).extension().is_some_and(|extension| extension == "rs")
}

/// How a resolved file is put back into its formatted shape.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Formatter {
    /// Leave the file as resolved.
    None,
    /// `rustfmt`, invoked as [`rustfmt_command`] describes.
    Rustfmt,
    /// An operator-supplied program and arguments, run at the workspace
    /// root. `{file}` in an argument is replaced by the file's path.
    Command(Vec<String>),
}

impl Formatter {
    fn command(&self, workspace_path: &Path, file: &str) -> Option<Vec<String>> {
        match self {
            Formatter::None => None,
            Formatter::Rustfmt => Some(rustfmt_command(workspace_path, file)),
            Formatter::Command(command) => Some(command.iter().map(|arg| arg.replace("{file}", file)).collect()),
        }
    }
}

/// The `rustfmt` invocation, to be run at the workspace root, that formats
/// `file` (a path relative to that root) the way its repository does.
///
/// It formats only that file, not the modules the file declares. When the
/// repository carries a rustfmt configuration at or above the file, that
/// configuration is named explicitly, so a user-level rustfmt config on
/// the host cannot change the result. `--config-path` is an error when
/// there is no configuration to find, so a repository without one is
/// formatted with the owning crate's edition instead, as `cargo fmt` does.
pub fn rustfmt_command(workspace_path: &Path, file: &str) -> Vec<String> {
    let mut command = vec!["rustfmt".to_owned(), "--config=skip_children=true".to_owned()];
    if let Some(config) = nearest_rustfmt_config(workspace_path, file) {
        command.push(format!("--config-path={}", config.display()));
    } else if let Some(edition) = crate_edition(workspace_path, file) {
        command.push(format!("--edition={edition}"));
    }
    command.push(file.to_owned());
    command
}

/// The directories from the one holding `file` up to the workspace root.
fn enclosing_directories<'a>(workspace_path: &'a Path, file: &'a str) -> impl Iterator<Item = std::path::PathBuf> + 'a {
    Path::new(file)
        .ancestors()
        .skip(1)
        .map(move |directory| workspace_path.join(directory))
}

fn nearest_rustfmt_config(workspace_path: &Path, file: &str) -> Option<std::path::PathBuf> {
    enclosing_directories(workspace_path, file)
        .flat_map(|directory| [directory.join("rustfmt.toml"), directory.join(".rustfmt.toml")])
        .find(|candidate| candidate.is_file())
}

/// The edition of the crate that owns `file`, read from the nearest
/// `Cargo.toml`, following `edition.workspace = true` to the workspace
/// manifest above it.
fn crate_edition(workspace_path: &Path, file: &str) -> Option<String> {
    let mut inherited = false;
    for directory in enclosing_directories(workspace_path, file) {
        let Ok(text) = std::fs::read_to_string(directory.join("Cargo.toml")) else {
            continue;
        };
        let Ok(manifest) = text.parse::<toml::Table>() else {
            continue;
        };
        if !inherited {
            let Some(package) = manifest.get("package") else {
                // A virtual manifest: no crate at or below it owns the file.
                return None;
            };
            match package.get("edition") {
                Some(toml::Value::String(edition)) => return Some(edition.clone()),
                Some(toml::Value::Table(edition)) if edition.get("workspace") == Some(&toml::Value::Boolean(true)) => {
                    inherited = true;
                }
                _ => return None,
            }
        }
        let workspace_edition = manifest
            .get("workspace")
            .and_then(|workspace| workspace.get("package"))
            .and_then(|package| package.get("edition"))
            .and_then(toml::Value::as_str);
        if let Some(edition) = workspace_edition {
            return Some(edition.to_owned());
        }
    }
    None
}

/// Reads the conflicted file, keeps both sides' insertions, writes the
/// result and formats it. On any refusal the file is left, or put back,
/// exactly as it was found.
async fn apply_keep_both(
    runner: &dyn CommandRunner,
    workspace_path: &Path,
    file: &ConflictedFile,
    scope: KeepBothScope,
    formatter: &Formatter,
) -> ResolveOutcome {
    let full_path = workspace_path.join(&file.path);
    let conflicted = match std::fs::read_to_string(&full_path) {
        Ok(content) => content,
        Err(e) => {
            return ResolveOutcome::Declined {
                reason: format!("failed to read {}: {e}", file.path),
            };
        }
    };
    let resolution = match keep_both_insertions(&file.path, &conflicted, scope) {
        Ok(resolution) => resolution,
        Err(reason) => return ResolveOutcome::Declined { reason },
    };
    if let Err(e) = std::fs::write(&full_path, &resolution.content) {
        return ResolveOutcome::Declined {
            reason: format!("failed to write resolved {}: {e}", file.path),
        };
    }

    if let Some(command) = formatter.command(workspace_path, &file.path)
        && let Err(outcome) = format_resolved(runner, workspace_path, &file.path, &command).await
    {
        // The next rung must see the conflict as jj wrote it, not a
        // half-finished resolution.
        return match std::fs::write(&full_path, &conflicted) {
            Ok(()) => outcome,
            Err(e) => ResolveOutcome::Failed {
                reason: format!("could not restore conflicted {} after a failed format: {e}", file.path),
            },
        };
    }

    let mut kinds: Vec<&str> = resolution.insertions.iter().map(|kind| kind.as_str()).collect();
    kinds.sort_unstable();
    kinds.dedup();
    ResolveOutcome::Resolved {
        summary: format!(
            "kept both sides' insertions in {} ({} conflict(s): {})",
            file.path,
            resolution.insertions.len(),
            kinds.join(", ")
        ),
    }
}

/// Runs the formatter. A formatter that cannot be started is an
/// operational failure; one that rejects the file means keeping both did
/// not produce well-formed source, which is a decline.
async fn format_resolved(
    runner: &dyn CommandRunner,
    workspace_path: &Path,
    file: &str,
    command: &[String],
) -> Result<(), ResolveOutcome> {
    let (program, args) = command.split_first().expect("format commands are validated non-empty");
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let output = runner
        .run(Path::new(program), &args, Some(workspace_path))
        .await
        .map_err(|e| ResolveOutcome::Failed {
            reason: format!("failed to spawn `{program}` to format {file}: {e}"),
        })?;
    if output.success {
        return Ok(());
    }
    let detail = output
        .stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("(no stderr)");
    Err(ResolveOutcome::Declined {
        reason: format!("keeping both insertions in {file} did not produce a file `{program}` accepts: {detail}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{CommandOutput, FakeCommandRunner};

    const CONFLICTED: &str = "mod a;\n<<<<<<< Conflict 1 of 1\n%%%%%%% Changes from base to side #1\n+mod c;\n+++++++ Contents of side #2\nmod b;\n>>>>>>> Conflict 1 of 1 ends\n";

    fn file(path: &str) -> ConflictedFile {
        ConflictedFile {
            path: path.to_owned(),
            marker_count: Some(1),
            shape: "content".to_owned(),
        }
    }

    fn rule(glob: &str, format_command: Option<&[&str]>) -> KeepBothRule {
        KeepBothRule {
            name: "append_only_list".to_owned(),
            glob: glob.to_owned(),
            format_command: format_command.map(|command| command.iter().map(|s| s.to_string()).collect()),
        }
    }

    fn rejecting_formatter(stderr: &str) -> Arc<FakeCommandRunner> {
        Arc::new(FakeCommandRunner::sequence(vec![Ok(CommandOutput {
            success: false,
            code: Some(1),
            stdout: String::new(),
            stderr: stderr.to_owned(),
        })]))
    }

    #[test]
    fn the_builtin_applies_to_every_rust_file_and_nothing_else() {
        let resolver = KeepBothUnionResolver::new();
        for path in ["mod.rs", "src/lib.rs", "tools/a/src/work/schema_init.rs", "tests/it.rs"] {
            assert!(resolver.applies_to(&file(path)), "{path}");
        }
        for path in ["Cargo.lock", "BUILD.bazel", "App/View.swift", "notes/rs", "README.md"] {
            assert!(!resolver.applies_to(&file(path)), "{path}");
        }
    }

    #[tokio::test]
    async fn keeps_both_insertions_then_formats_only_that_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), CONFLICTED).unwrap();
        // rustfmt sorts adjacent `mod` lines; stand in for that rewrite.
        let runner = Arc::new(
            FakeCommandRunner::success_writing_file("src/lib.rs", "mod a;\nmod b;\nmod c;\n")
                .capture_file("src/lib.rs"),
        );
        let resolver = KeepBothUnionResolver::with_runner(runner.clone());

        let outcome = resolver.resolve(dir.path(), &file("src/lib.rs")).await;

        match outcome {
            ResolveOutcome::Resolved { summary } => {
                assert_eq!(
                    summary,
                    "kept both sides' insertions in src/lib.rs (1 conflict(s): imports)"
                );
            }
            other => panic!("expected Resolved, got {other:?}"),
        }
        // The formatter was handed the union, side #1's insertion first.
        assert_eq!(
            runner.captured_file.lock().unwrap().as_deref(),
            Some("mod a;\nmod c;\nmod b;\n")
        );
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "rustfmt");
        assert_eq!(calls[0].1, rustfmt_command(dir.path(), "src/lib.rs")[1..]);
        assert_eq!(calls[0].2, dir.path());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
            "mod a;\nmod b;\nmod c;\n"
        );
    }

    #[test]
    fn rustfmt_is_pointed_at_the_nearest_configuration_in_the_repository() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("tools/a/src")).unwrap();
        std::fs::write(root.join("rustfmt.toml"), "edition = \"2024\"\n").unwrap();
        // An edition in a manifest is not consulted when a config exists.
        std::fs::write(root.join("tools/a/Cargo.toml"), "[package]\nedition = \"2021\"\n").unwrap();

        assert_eq!(
            rustfmt_command(root, "tools/a/src/lib.rs"),
            [
                "rustfmt".to_owned(),
                "--config=skip_children=true".to_owned(),
                format!("--config-path={}", root.join("rustfmt.toml").display()),
                "tools/a/src/lib.rs".to_owned(),
            ]
        );

        // A configuration nearer the file wins over the root one.
        std::fs::write(root.join("tools/a/.rustfmt.toml"), "max_width = 80\n").unwrap();
        assert_eq!(
            rustfmt_command(root, "tools/a/src/lib.rs")[2],
            format!("--config-path={}", root.join("tools/a/.rustfmt.toml").display())
        );
        // A file at the root still finds the root configuration.
        assert_eq!(
            rustfmt_command(root, "build.rs")[2],
            format!("--config-path={}", root.join("rustfmt.toml").display())
        );
    }

    #[test]
    fn without_a_configuration_rustfmt_gets_the_owning_crates_edition() {
        // `--config-path` fails outright when the path holds no rustfmt
        // configuration, so it must not be passed for such a repository.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("crates/own/src")).unwrap();
        std::fs::create_dir_all(root.join("crates/inherits/src")).unwrap();
        std::fs::create_dir_all(root.join("crates/silent/src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("crates/own/Cargo.toml"),
            "[package]\nname = \"own\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("crates/inherits/Cargo.toml"),
            "[package]\nname = \"inherits\"\nedition.workspace = true\n",
        )
        .unwrap();
        std::fs::write(root.join("crates/silent/Cargo.toml"), "[package]\nname = \"silent\"\n").unwrap();

        let args = |file: &str| rustfmt_command(root, file)[1..].to_vec();
        assert_eq!(
            args("crates/own/src/lib.rs"),
            ["--config=skip_children=true", "--edition=2021", "crates/own/src/lib.rs"]
        );
        assert_eq!(
            args("crates/inherits/src/deep.rs"),
            [
                "--config=skip_children=true",
                "--edition=2024",
                "crates/inherits/src/deep.rs"
            ]
        );
        // No edition declared, and none to be had: rustfmt's own default.
        assert_eq!(
            args("crates/silent/src/lib.rs"),
            ["--config=skip_children=true", "crates/silent/src/lib.rs"]
        );
        assert_eq!(args("loose.rs"), ["--config=skip_children=true", "loose.rs"]);
    }

    #[tokio::test]
    async fn a_refused_file_is_left_untouched_and_never_formatted() {
        let dir = tempfile::tempdir().unwrap();
        let duplicate = CONFLICTED.replace("mod b;", "mod c;");
        std::fs::write(dir.path().join("lib.rs"), &duplicate).unwrap();
        let runner = Arc::new(FakeCommandRunner::sequence(Vec::new()));
        let resolver = KeepBothUnionResolver::with_runner(runner.clone());

        let outcome = resolver.resolve(dir.path(), &file("lib.rs")).await;

        assert_eq!(
            outcome,
            ResolveOutcome::Declined {
                reason: "both sides added the same item `c`".to_owned()
            }
        );
        assert_eq!(std::fs::read_to_string(dir.path().join("lib.rs")).unwrap(), duplicate);
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_result_the_formatter_rejects_is_a_decline_and_the_conflict_is_restored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), CONFLICTED).unwrap();
        let runner = rejecting_formatter("\nerror: expected one of `!` or `::`, found `}`\n --> lib.rs:3:1\n");
        let resolver = KeepBothUnionResolver::with_runner(runner);

        let outcome = resolver.resolve(dir.path(), &file("lib.rs")).await;

        match outcome {
            ResolveOutcome::Declined { reason } => assert_eq!(
                reason,
                "keeping both insertions in lib.rs did not produce a file `rustfmt` accepts: \
                 error: expected one of `!` or `::`, found `}`"
            ),
            other => panic!("expected Declined, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("lib.rs")).unwrap(),
            CONFLICTED,
            "the next rung must find the conflict as jj wrote it"
        );
    }

    #[tokio::test]
    async fn a_formatter_that_cannot_start_is_a_failure_and_the_conflict_is_restored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), CONFLICTED).unwrap();
        let resolver = KeepBothUnionResolver::with_runner(Arc::new(FakeCommandRunner::spawn_error()));

        let outcome = resolver.resolve(dir.path(), &file("lib.rs")).await;

        match outcome {
            ResolveOutcome::Failed { reason } => {
                assert!(
                    reason.contains("failed to spawn `rustfmt` to format lib.rs"),
                    "{reason}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(dir.path().join("lib.rs")).unwrap(), CONFLICTED);
    }

    #[tokio::test]
    async fn declines_when_the_file_cannot_be_read() {
        let dir = tempfile::tempdir().unwrap();
        let resolver = KeepBothUnionResolver::with_runner(Arc::new(FakeCommandRunner::success()));
        let outcome = resolver.resolve(dir.path(), &file("missing.rs")).await;
        assert!(matches!(outcome, ResolveOutcome::Declined { .. }), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_rule_keeps_statement_insertions_the_builtin_refuses() {
        let conflicted = "fn all() {\n    one(&db);\n<<<<<<< Conflict 1 of 1\n%%%%%%% Changes from base to side #1\n+    from_main(&db);\n+++++++ Contents of side #2\n    from_pr(&db);\n>>>>>>> Conflict 1 of 1 ends\n}\n";
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("db")).unwrap();
        std::fs::write(dir.path().join("db/migrations.rs"), conflicted).unwrap();

        let builtin = KeepBothUnionResolver::with_runner(Arc::new(FakeCommandRunner::sequence(Vec::new())));
        let refused = builtin.resolve(dir.path(), &file("db/migrations.rs")).await;
        assert!(matches!(refused, ResolveOutcome::Declined { .. }), "{refused:?}");

        let runner = Arc::new(FakeCommandRunner::success());
        let by_rule = KeepBothRuleResolver::with_runner(rule("**/migrations.rs", None), runner.clone());
        assert!(by_rule.applies_to(&file("db/migrations.rs")));
        assert!(!by_rule.applies_to(&file("db/other.rs")));
        assert_eq!(by_rule.class(), ConflictClass::Recipe);

        let outcome = by_rule.resolve(dir.path(), &file("db/migrations.rs")).await;

        match outcome {
            ResolveOutcome::Resolved { summary } => assert_eq!(
                summary,
                "keep_both rule \"append_only_list\": kept both sides' insertions in db/migrations.rs (1 conflict(s): lines)"
            ),
            other => panic!("expected Resolved, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("db/migrations.rs")).unwrap(),
            "fn all() {\n    one(&db);\n    from_main(&db);\n    from_pr(&db);\n}\n"
        );
        // A Rust path with no `format_command` is formatted with rustfmt.
        assert_eq!(runner.calls.lock().unwrap()[0].0, "rustfmt");
    }

    #[tokio::test]
    async fn a_rule_runs_its_own_format_command_on_the_resolved_file() {
        let conflicted = "deps = [\n<<<<<<< Conflict 1 of 1\n%%%%%%% Changes from base to side #1\n+    \"//main\",\n+++++++ Contents of side #2\n    \"//pr\",\n>>>>>>> Conflict 1 of 1 ends\n]\n";
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("pkg")).unwrap();
        std::fs::write(dir.path().join("pkg/BUILD.bazel"), conflicted).unwrap();
        let runner = Arc::new(FakeCommandRunner::success());
        let resolver = KeepBothRuleResolver::with_runner(
            rule("**/BUILD.bazel", Some(&["buildifier", "-lint=off", "{file}"])),
            runner.clone(),
        );

        let outcome = resolver.resolve(dir.path(), &file("pkg/BUILD.bazel")).await;

        assert!(matches!(outcome, ResolveOutcome::Resolved { .. }), "{outcome:?}");
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls[0].0, "buildifier");
        assert_eq!(calls[0].1, ["-lint=off", "pkg/BUILD.bazel"]);
        assert_eq!(calls[0].2, dir.path());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("pkg/BUILD.bazel")).unwrap(),
            "deps = [\n    \"//main\",\n    \"//pr\",\n]\n"
        );
    }

    #[tokio::test]
    async fn a_rule_without_a_formatter_leaves_a_non_rust_file_as_resolved() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("members.txt"),
            "<<<<<<< Conflict 1 of 1\n%%%%%%% Changes from base to side #1\n+main-entry\n+++++++ Contents of side #2\npr-entry\n>>>>>>> Conflict 1 of 1 ends\n",
        )
        .unwrap();
        let runner = Arc::new(FakeCommandRunner::sequence(Vec::new()));
        let resolver = KeepBothRuleResolver::with_runner(rule("members.txt", None), runner.clone());

        let outcome = resolver.resolve(dir.path(), &file("members.txt")).await;

        assert!(matches!(outcome, ResolveOutcome::Resolved { .. }), "{outcome:?}");
        assert!(runner.calls.lock().unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("members.txt")).unwrap(),
            "main-entry\npr-entry\n"
        );
    }

    #[tokio::test]
    async fn a_rule_names_itself_when_it_declines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("list.rs"), "no conflict here\n").unwrap();
        let resolver = KeepBothRuleResolver::with_runner(rule("list.rs", None), Arc::new(FakeCommandRunner::success()));
        let outcome = resolver.resolve(dir.path(), &file("list.rs")).await;
        assert_eq!(
            outcome,
            ResolveOutcome::Declined {
                reason: "keep_both rule \"append_only_list\": no conflict markers found".to_owned()
            }
        );
    }

    #[test]
    fn from_rule_rejects_an_invalid_glob_and_an_empty_format_command() {
        assert!(KeepBothRuleResolver::from_rule(rule("[", None)).is_err());
        assert!(KeepBothRuleResolver::from_rule(rule("*.rs", Some(&[]))).is_err());
        assert!(KeepBothRuleResolver::from_rule(rule("*.rs", None)).is_ok());
    }
}
