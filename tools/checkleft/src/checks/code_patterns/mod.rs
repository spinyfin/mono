use std::path::Path;
use std::sync::{Arc, LazyLock};

use anyhow::Result;
use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::check::{Check, ConfiguredCheck, count_applicable, run_per_text_file};
use crate::input::{ChangeSet, SourceTree};
use crate::output::CheckResult;

mod config;
mod java;

use config::{PatternLanguage, parse_config};
use java::analyze_java_file;

#[derive(Debug, Default)]
pub struct CodePatternsCheck;

#[async_trait]
impl Check for CodePatternsCheck {
    fn id(&self) -> &str {
        "code-patterns"
    }

    fn description(&self) -> &str {
        "flags configured language-aware code patterns in changed files"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config)?))
    }
}

#[async_trait]
impl ConfiguredCheck for config::CompiledCodePatternsConfig {
    fn applicable_file_count(&self, changeset: &ChangeSet) -> usize {
        count_applicable(changeset, |path| matches_language_path(path, self.language))
    }

    async fn run_with_progress(
        &self,
        changeset: &ChangeSet,
        tree: &dyn SourceTree,
        on_file_processed: Arc<dyn Fn(usize) + Send + Sync>,
    ) -> Result<CheckResult> {
        let findings = run_per_text_file(
            changeset,
            tree,
            |path| matches_language_path(path, self.language),
            &*on_file_processed,
            |changed_file, contents, findings| {
                findings.extend(analyze_java_file(&changed_file.path, contents, &self.rules));
            },
        );

        Ok(CheckResult {
            check_id: "code-patterns".to_owned(),
            findings,
        })
    }
}

/// Explicit definition-scope include globs for a `lang` value.
///
/// `lang` selects the parser; these globs are the check's definition-level
/// default path scope (what the framework would answer for "is this file a
/// target" when the check entry carries no further `include`). They must stay
/// in lockstep with [`matches_language_path`] — that function is implemented
/// by matching against this list, so a drift is a compile-time structural
/// impossibility rather than a silent coverage change.
fn language_include_globs(language: PatternLanguage) -> &'static [&'static str] {
    match language {
        // Matches `path.extension() == Some("java")` for ordinary repo-relative
        // paths: `**` covers zero or more directories, `*.java` requires a final
        // path component whose extension is exactly `java` (case-sensitive).
        PatternLanguage::Java => &["**/*.java"],
    }
}

fn language_globset(language: PatternLanguage) -> &'static GlobSet {
    match language {
        PatternLanguage::Java => {
            static JAVA: LazyLock<GlobSet> = LazyLock::new(|| compile_language_globset(PatternLanguage::Java));
            &JAVA
        }
    }
}

fn compile_language_globset(language: PatternLanguage) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for pattern in language_include_globs(language) {
        builder.add(
            Glob::new(pattern)
                .unwrap_or_else(|err| panic!("code-patterns language include glob `{pattern}` must be valid: {err}")),
        );
    }
    builder
        .build()
        .unwrap_or_else(|err| panic!("code-patterns language include globs must compile: {err}"))
}

/// Definition-scope path gate for `lang`. Implemented via
/// [`language_include_globs`] so the explicit default scope and the runtime
/// predicate cannot diverge.
fn matches_language_path(path: &Path, language: PatternLanguage) -> bool {
    language_globset(language).is_match(path)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod language_scope_tests {
    use std::path::Path;

    use super::config::PatternLanguage;
    use super::{language_include_globs, matches_language_path};

    /// The pre-migration gate was a bare extension check. Prove the explicit
    /// default globs reproduce it on representative paths so a silent coverage
    /// change cannot land with this rewrite.
    ///
    /// Paths whose final component is a leading-dot name with a single period
    /// (e.g. `.java`) are excluded: `Path::extension` returns `None` for those
    /// while `**/*.java` matches them. No real Java source tree uses that shape
    /// as a compilation unit.
    fn legacy_extension_gate(path: &Path, language: PatternLanguage) -> bool {
        match language {
            PatternLanguage::Java => {
                matches!(path.extension().and_then(|ext| ext.to_str()), Some("java"))
            }
        }
    }

    #[test]
    fn java_definition_globs_are_exactly_star_star_slash_star_dot_java() {
        assert_eq!(language_include_globs(PatternLanguage::Java), &["**/*.java"]);
    }

    #[test]
    fn java_glob_gate_matches_legacy_extension_gate() {
        let samples = [
            "Foo.java",
            "src/main/java/demo/Foo.java",
            "a/b/c/Bar.java",
            "README.md",
            "src/Foo.rs",
            "src/Foo.JAVA",
            "src/Foo.java.bak",
            "src/Foo.jav",
            "java",
            "src/foo.Java",
            "pkg/Outer$Inner.java",
            "src/main/java/com/example/App.java",
        ];
        for sample in samples {
            let path = Path::new(sample);
            let legacy = legacy_extension_gate(path, PatternLanguage::Java);
            let explicit = matches_language_path(path, PatternLanguage::Java);
            assert_eq!(
                explicit, legacy,
                "path `{sample}`: explicit glob gate ({explicit}) must equal legacy extension gate ({legacy})"
            );
        }
    }
}
