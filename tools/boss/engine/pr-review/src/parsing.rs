//! Result parsing, classification, and the engine severity gate:
//! [`classify_changed_files`], [`extract_review_result`],
//! [`extract_review_result_verbose`], and [`passes_severity_gate`].

use boss_engine_structured_output::fallback::{FallbackCandidate, json_object_candidates};
use boss_protocol::{
    ReviewClassification, ReviewComplexityFlag, ReviewLanguageBucket, ReviewMetadataField, ReviewProfile,
};

use crate::types::*;

/// PR metadata used for review-profile selection.
///
/// The inputs deliberately contain only immutable GitHub diff metadata: the
/// classifier has no task-effort, driver, or database dependency. A missing
/// field is retained in the resulting [`ReviewClassification`] and selects
/// the conservative Standard profile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrReviewMetadata {
    pub additions: Option<i64>,
    pub changed_files: Option<Vec<String>>,
    pub deletions: Option<i64>,
}

/// Classify immutable PR metadata into a review profile and audit snapshot.
///
/// The thresholds and lexical path rules implement the initial policy in
/// `docs/designs/multi-agent-code-review.md`. This function is pure so the
/// batch creator can compute the profile once, persist its complete input,
/// and reuse that immutable choice for every member.
pub fn classify_pr_review_metadata(metadata: &PrReviewMetadata) -> ReviewClassification {
    let mut metadata_missing = Vec::new();
    if metadata.additions.is_none() {
        metadata_missing.push(ReviewMetadataField::Additions);
    }
    if metadata.deletions.is_none() {
        metadata_missing.push(ReviewMetadataField::Deletions);
    }
    if metadata.changed_files.is_none() {
        metadata_missing.push(ReviewMetadataField::ChangedFiles);
    }

    let changed_files = metadata.changed_files.clone().unwrap_or_default();
    let subsystem_buckets = sorted_unique(changed_files.iter().map(|path| subsystem_bucket(path)));
    let production_languages = sorted_unique(changed_files.iter().filter_map(|path| production_language(path)));
    let complexity_flags = complexity_flags(&changed_files);
    let has_production_code = !production_languages.is_empty();
    let docs_or_test_only = !changed_files.is_empty() && changed_files.iter().all(|path| is_docs_or_test_file(path));

    let profile = if !metadata_missing.is_empty() {
        ReviewProfile::Standard
    } else {
        let changed_lines = metadata.additions.unwrap_or_default() + metadata.deletions.unwrap_or_default();
        let changed_file_count = changed_files.len();
        let light_line_limit = if docs_or_test_only { 400 } else { 200 };
        let light_file_limit = if docs_or_test_only { 10 } else { 5 };
        let is_light = changed_lines <= light_line_limit
            && changed_file_count <= light_file_limit
            && subsystem_buckets.len() <= 1
            && production_languages.len() <= 1
            && complexity_flags.is_empty();
        let is_deep = changed_lines > 1_000
            || changed_file_count > 25
            || subsystem_buckets.len() >= 4
            || production_languages.len() >= 3
            || complexity_flags.len() >= 2;

        if is_light {
            ReviewProfile::Light
        } else if is_deep {
            ReviewProfile::Deep
        } else {
            ReviewProfile::Standard
        }
    };

    ReviewClassification {
        changed_files,
        complexity_flags,
        has_production_code,
        metadata_missing,
        production_languages,
        profile,
        subsystem_buckets,
        additions: metadata.additions,
        deletions: metadata.deletions,
    }
}

fn sorted_unique<T: Ord>(values: impl IntoIterator<Item = T>) -> Vec<T> {
    values
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn subsystem_bucket(path: &str) -> String {
    let components = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    let directories = components.get(..components.len().saturating_sub(1)).unwrap_or_default();
    if directories.is_empty() {
        return "root".to_owned();
    }

    let bucket_len = if directories
        .first()
        .is_some_and(|component| component.eq_ignore_ascii_case("tools"))
    {
        3
    } else {
        2
    };
    directories[..directories.len().min(bucket_len)].join("/")
}

fn production_language(path: &str) -> Option<ReviewLanguageBucket> {
    if is_docs_or_test_file(path) {
        return None;
    }
    let extension = path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())?;
    Some(match extension.as_str() {
        "rs" => ReviewLanguageBucket::Rust,
        "swift" => ReviewLanguageBucket::Swift,
        "bzl" | "bazel" => ReviewLanguageBucket::Starlark,
        "sh" | "bash" | "zsh" | "fish" => ReviewLanguageBucket::Shell,
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "html" | "css" | "scss" | "vue" | "svelte" => {
            ReviewLanguageBucket::Web
        }
        "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "go" | "java" | "kt" | "kts" | "py" | "rb" | "php" | "lua"
        | "ex" | "exs" | "hs" | "scala" | "sql" => ReviewLanguageBucket::Other,
        _ => return None,
    })
}

fn is_docs_or_test_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let components = lower.split('/').collect::<Vec<_>>();
    let file_name = components.last().copied().unwrap_or_default();
    lower.ends_with(".md")
        || lower.ends_with(".mdx")
        || lower.ends_with(".rst")
        || lower.ends_with(".txt")
        || lower.ends_with(".snap")
        || components.iter().any(|component| {
            matches!(
                *component,
                "doc" | "docs" | "fixture" | "fixtures" | "snapshot" | "snapshots" | "test" | "testdata" | "tests"
            )
        })
        || file_name.starts_with("readme")
        || file_name.starts_with("changelog")
        || file_name.ends_with("_test.rs")
        || file_name.ends_with("_tests.rs")
}

fn complexity_flags(paths: &[String]) -> Vec<ReviewComplexityFlag> {
    let path_text = paths.iter().map(|path| path.to_ascii_lowercase()).collect::<Vec<_>>();
    let has = |needles: &[&str]| {
        path_text
            .iter()
            .any(|path| needles.iter().any(|needle| path.contains(needle)))
    };
    let has_build_file = path_text.iter().any(|path| {
        matches!(
            path.rsplit('/').next().unwrap_or_default(),
            "build" | "build.bazel" | "module.bazel" | "workspace" | "workspace.bazel" | "cargo.toml" | "cargo.lock"
        )
    });

    let mut flags = Vec::new();
    if has(&["/migrations/", "migration", "/schema/", "schema_init"]) {
        flags.push(ReviewComplexityFlag::DatabaseSchemaMigration);
    }
    if has(&["auth", "permission", "sandbox"]) {
        flags.push(ReviewComplexityFlag::AuthPermissionsSandbox);
    }
    if has(&["scheduler", "concurrency", "lifecycle", "process"]) {
        flags.push(ReviewComplexityFlag::SchedulerConcurrencyLifecycle);
    }
    if has_build_file || has(&["/build/", "release", "dependenc"]) {
        flags.push(ReviewComplexityFlag::BuildReleaseDependency);
    }
    flags
}

/// Classify a list of changed file paths as docs-only or code.
///
/// Returns [`ReviewScope::DocsOnly`] if every path in `files` is a
/// documentation file (`.md`, `.mdx`, `.rst`, `.txt`, or any path that
/// lives under a `docs/` directory at any depth). Returns
/// [`ReviewScope::Code`] if any path is a source, build, or config file,
/// or if `files` is empty (an empty diff defaults to the code rubric).
///
/// # Examples
///
/// ```
/// use boss_pr_review::{classify_changed_files, ReviewScope};
///
/// assert_eq!(
///     classify_changed_files(&["docs/design.md", "README.md"]),
///     ReviewScope::DocsOnly,
/// );
/// assert_eq!(
///     classify_changed_files(&["src/lib.rs", "docs/design.md"]),
///     ReviewScope::Code,
/// );
/// assert_eq!(classify_changed_files(&[]), ReviewScope::Code);
/// ```
pub fn classify_changed_files(files: &[&str]) -> ReviewScope {
    if files.is_empty() {
        return ReviewScope::Code;
    }
    if files.iter().all(|f| is_docs_file(f)) {
        ReviewScope::DocsOnly
    } else {
        ReviewScope::Code
    }
}

fn is_docs_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".md")
        || lower.ends_with(".mdx")
        || lower.ends_with(".rst")
        || lower.ends_with(".txt")
        || lower.starts_with("docs/")
        || lower.contains("/docs/")
}

/// Extract and parse the first `ReviewResult` from a reviewer's final
/// assistant message.
///
/// Convenience wrapper over [`review_result_from_candidates`] fed by the
/// shared JSON-block scanner. The engine itself goes through the *driver's*
/// structured-output fallback producer (which uses the same scanner) so the
/// transcript conventions stay with the driver; this entry point remains for
/// callers holding raw reviewer text.
///
/// Returns `None` when no parseable `ReviewResult` is found (reviewer may
/// have crashed or emitted malformed output — the caller should fall back to
/// advancing without revision).
///
/// To also receive the serde error from a failed parse attempt (useful for
/// surfacing in a re-prompt), use [`extract_review_result_verbose`].
pub fn extract_review_result(text: &str) -> Option<ReviewResult> {
    extract_review_result_verbose(text).0
}

/// Like [`extract_review_result`] but also returns the serde parse error from
/// the most-preferred candidate that looked like a `ReviewResult` and failed.
///
/// The error string names the specific field path and type mismatch so the
/// caller can include it verbatim in a reviewer re-prompt, giving the reviewer
/// signal about exactly what is wrong rather than a generic "write valid JSON"
/// message. Returns `(None, None)` when the text contains no JSON-like content
/// at all.
pub fn extract_review_result_verbose(text: &str) -> (Option<ReviewResult>, Option<String>) {
    review_result_from_candidates(&json_object_candidates(text))
}

/// Validate structured-output fallback candidates against the `ReviewResult`
/// schema, keeping the first that parses.
///
/// `candidates` come from a driver's
/// `AgentDriver::structured_output_fallback` for
/// `StructuredOutputKind::ReviewResult` — the *fallback* channel, used when
/// the reviewer's file artifact is absent. They are ordered most-preferred
/// first (fenced blocks the reviewer plainly meant as output, then bare
/// objects found in prose, latest first).
///
/// The returned error prefers the first failing candidate that carries the
/// `revision_warranted` field — the strongest signal that it *was* the
/// reviewer's `ReviewResult` rather than an unrelated JSON object quoted in
/// prose — and only falls back to the first failing explicitly-fenced
/// candidate when no `revision_warranted`-bearing candidate failed. This
/// keeps a plain non-JSON fenced block (which `json_object_candidates` also
/// marks explicit) from shadowing the real `ReviewResult` parse error that
/// should reach the reviewer re-prompt.
pub fn review_result_from_candidates(candidates: &[FallbackCandidate]) -> (Option<ReviewResult>, Option<String>) {
    let mut named_error: Option<String> = None;
    let mut explicit_error: Option<String> = None;
    for candidate in candidates {
        match ReviewResult::from_json(&candidate.payload) {
            Ok(result) => return (Some(result), None),
            Err(err) => {
                if named_error.is_none() && candidate.payload.contains("revision_warranted") {
                    named_error = Some(err.to_string());
                }
                if explicit_error.is_none() && candidate.explicit {
                    explicit_error = Some(err.to_string());
                }
            }
        }
    }
    (None, named_error.or(explicit_error))
}

/// Engine severity gate.
///
/// Returns `true` when `result` qualifies for a revision:
/// - any finding with `severity = Critical` or `High`, **or**
/// - any finding with `category = Regression` (regardless of severity), **or**
/// - any finding with `category = Duplication` (regardless of severity) —
///   confirmed infrastructure reimplementation is a revision-required finding,
///   not advisory (operator directive: reuse/duplication findings get the
///   exact same forcing treatment as regressions, not a parallel escalation
///   path), **or**
/// - any finding with `category = DeferredScope` (regardless of severity) —
///   undeclared/misdeclared deferred scope or a malformed `[deferred-scope]`
///   marker is a process gap the engine cannot otherwise catch, so it gets
///   the same forcing treatment as regression/duplication, **or**
/// - any finding with `category = AgentIsms` (regardless of severity) — a
///   code comment or PR title/description that names a Boss work
///   item/phase/brief/effort-level, or calls the directing human "the
///   operator", reads as agent scaffolding left behind, so it gets the same
///   forcing treatment as regression/duplication/deferred-scope. (Historical
///   narration is flagged only in code comments — PR descriptions are exempt
///   from that specific sub-check, since narrating what changed and why is
///   their normal job.)
///
/// `revision_warranted = false` in the `ReviewResult` does not suppress the
/// gate — the engine's own threshold governs.
pub fn passes_severity_gate(result: &ReviewResult) -> bool {
    result.findings.iter().any(|f| {
        matches!(
            f.severity,
            ReviewFindingSeverity::Critical | ReviewFindingSeverity::High
        ) || matches!(
            f.category,
            ReviewFindingCategory::Regression
                | ReviewFindingCategory::Duplication
                | ReviewFindingCategory::DeferredScope
                | ReviewFindingCategory::AgentIsms
        )
    })
}

#[cfg(test)]
mod tests {
    use crate::*;
    use boss_protocol::{ReviewComplexityFlag, ReviewLanguageBucket, ReviewMetadataField, ReviewProfile};

    fn metadata(additions: Option<i64>, deletions: Option<i64>, files: &[&str]) -> PrReviewMetadata {
        PrReviewMetadata {
            additions,
            changed_files: Some(files.iter().map(|file| (*file).to_owned()).collect()),
            deletions,
        }
    }

    #[test]
    fn review_profile_classifies_a_small_simple_code_pr_as_light() {
        let classification = classify_pr_review_metadata(&metadata(
            Some(100),
            Some(100),
            &[
                "tools/boss/engine/pr-review/src/types.rs",
                "tools/boss/engine/pr-review/src/parsing.rs",
                "tools/boss/engine/pr-review/src/render.rs",
                "tools/boss/engine/pr-review/src/lib.rs",
                "tools/boss/engine/pr-review/src/blocks.rs",
            ],
        ));

        assert_eq!(classification.profile, ReviewProfile::Light);
        assert_eq!(classification.subsystem_buckets, vec!["tools/boss/engine"]);
        assert_eq!(classification.production_languages, vec![ReviewLanguageBucket::Rust]);
        assert!(classification.has_production_code);
    }

    #[test]
    fn review_profile_relaxes_light_limits_for_docs_and_tests() {
        let classification = classify_pr_review_metadata(&metadata(
            Some(250),
            Some(150),
            &[
                "docs/guide-1.md",
                "docs/guide-2.md",
                "docs/guide-3.md",
                "docs/guide-4.md",
                "docs/guide-5.md",
                "docs/guide-6.md",
                "docs/guide-7.md",
                "docs/guide-8.md",
                "docs/guide-9.md",
                "docs/guide-10.md",
            ],
        ));

        assert_eq!(classification.profile, ReviewProfile::Light);
        assert!(!classification.has_production_code);
        assert!(classification.production_languages.is_empty());
    }

    #[test]
    fn review_profile_classifies_intermediate_or_single_flag_prs_as_standard() {
        let size_classification = classify_pr_review_metadata(&metadata(Some(201), Some(0), &["src/lib.rs"]));
        assert_eq!(size_classification.profile, ReviewProfile::Standard);

        let flag_classification = classify_pr_review_metadata(&metadata(
            Some(10),
            Some(0),
            &["tools/boss/engine/core/src/worker_sandbox_audit.rs"],
        ));
        assert_eq!(flag_classification.profile, ReviewProfile::Standard);
        assert_eq!(
            flag_classification.complexity_flags,
            vec![ReviewComplexityFlag::AuthPermissionsSandbox]
        );
    }

    #[test]
    fn review_profile_classifies_large_or_complex_prs_as_deep() {
        let size_classification = classify_pr_review_metadata(&metadata(Some(1_001), Some(0), &["src/lib.rs"]));
        assert_eq!(size_classification.profile, ReviewProfile::Deep);

        let complexity_classification = classify_pr_review_metadata(&metadata(
            Some(10),
            Some(0),
            &[
                "tools/boss/engine/core/src/work/migrations_b.rs",
                "tools/boss/engine/core/src/worker_sandbox_audit.rs",
            ],
        ));
        assert_eq!(complexity_classification.profile, ReviewProfile::Deep);
        assert_eq!(complexity_classification.complexity_flags.len(), 2);
    }

    #[test]
    fn review_profile_uses_standard_and_records_incomplete_metadata() {
        let classification = classify_pr_review_metadata(&PrReviewMetadata {
            additions: Some(1),
            changed_files: None,
            deletions: None,
        });

        assert_eq!(classification.profile, ReviewProfile::Standard);
        assert_eq!(
            classification.metadata_missing,
            vec![ReviewMetadataField::Deletions, ReviewMetadataField::ChangedFiles]
        );
    }

    #[test]
    fn classify_empty_files_returns_code() {
        assert_eq!(classify_changed_files(&[]), ReviewScope::Code);
    }

    #[test]
    fn classify_all_md_files_returns_docs_only() {
        assert_eq!(
            classify_changed_files(&["README.md", "docs/design.md", "CHANGELOG.md"]),
            ReviewScope::DocsOnly,
        );
    }

    #[test]
    fn classify_mixed_returns_code() {
        assert_eq!(classify_changed_files(&["README.md", "src/lib.rs"]), ReviewScope::Code,);
    }

    #[test]
    fn classify_mdx_and_rst_count_as_docs() {
        assert_eq!(
            classify_changed_files(&["docs/guide.mdx", "notes.rst"]),
            ReviewScope::DocsOnly,
        );
    }

    #[test]
    fn classify_docs_dir_prefix_counts_as_docs() {
        assert_eq!(
            classify_changed_files(&["docs/architecture/overview.txt"]),
            ReviewScope::DocsOnly,
        );
    }

    #[test]
    fn classify_docs_subdir_in_path_counts_as_docs() {
        assert_eq!(
            classify_changed_files(&["tools/boss/docs/designs/foo.md"]),
            ReviewScope::DocsOnly,
        );
    }

    #[test]
    fn classify_rs_file_alone_returns_code() {
        assert_eq!(
            classify_changed_files(&["tools/boss/engine/src/lib.rs"]),
            ReviewScope::Code,
        );
    }

    #[test]
    fn classify_build_file_with_docs_returns_code() {
        assert_eq!(
            classify_changed_files(&["docs/guide.md", "BUILD.bazel"]),
            ReviewScope::Code,
        );
    }

    fn make_review_result_json(revision_warranted: bool, findings: serde_json::Value) -> String {
        serde_json::json!({
            "pr_url": "https://github.com/org/repo/pull/1",
            "head_sha": "abc",
            "summary": "summary text",
            "revision_warranted": revision_warranted,
            "findings": findings,
            "regression_check": { "performed": true, "suspected_deletions": [] }
        })
        .to_string()
    }

    #[test]
    fn extract_review_result_parses_fenced_json_block() {
        let json = make_review_result_json(false, serde_json::json!([]));
        let text = format!("Here is my review:\n\n```json\n{json}\n```\n\nDone.");
        let result = extract_review_result(&text).expect("should parse");
        assert_eq!(result.pr_url, "https://github.com/org/repo/pull/1");
        assert!(!result.revision_warranted);
    }

    #[test]
    fn extract_review_result_returns_none_for_plain_text() {
        let text = "No structured output here, just prose.";
        assert!(extract_review_result(text).is_none());
    }

    #[test]
    fn extract_review_result_returns_none_for_malformed_json() {
        let text = "```json\n{ not valid json }\n```";
        assert!(extract_review_result(text).is_none());
    }

    #[test]
    fn extract_review_result_finds_block_after_prose() {
        let json = make_review_result_json(true, serde_json::json!([]));
        let text = format!("I reviewed the PR.\n\nSome analysis here.\n\n```json\n{json}\n```");
        let result = extract_review_result(&text).expect("should parse");
        assert!(result.revision_warranted);
    }

    #[test]
    fn extract_review_result_parses_plain_fenced_block() {
        let json = make_review_result_json(false, serde_json::json!([]));
        let text = format!("Here is the result:\n\n```\n{json}\n```\n");
        let result = extract_review_result(&text).expect("should parse plain fence");
        assert_eq!(result.pr_url, "https://github.com/org/repo/pull/1");
    }

    #[test]
    fn extract_review_result_parses_bare_json_after_prose() {
        // Regression fixture for T1304 / PR #1320 shape:
        // "## Review summary … Key findings below.\n\n{ … }"
        let json = make_review_result_json(
            true,
            serde_json::json!([
                {
                    "severity": "high",
                    "category": "correctness",
                    "file": "src/lib.rs",
                    "title": "missing null check",
                    "detail": "foo can be null here",
                    "confidence": "high"
                }
            ]),
        );
        let text = format!(
            "## Review summary\n\nI reviewed the PR carefully.\n\
             \nKey findings below.\n\n{json}"
        );
        let result = extract_review_result(&text).expect("should parse bare JSON after prose");
        assert!(result.revision_warranted);
        assert_eq!(result.findings.len(), 1);
    }

    #[test]
    fn extract_review_result_parses_trailing_bare_json() {
        let json = make_review_result_json(false, serde_json::json!([]));
        let text = format!("Some prose up front.\n\n{json}");
        let result = extract_review_result(&text).expect("should parse trailing bare JSON");
        assert!(!result.revision_warranted);
    }

    #[test]
    fn extract_review_result_prefers_last_valid_result_in_bare_scan() {
        // If there are multiple JSON-like objects, the last valid ReviewResult wins.
        let json1 = make_review_result_json(false, serde_json::json!([]));
        let json2 = make_review_result_json(true, serde_json::json!([]));
        let text = format!("First: {json1}\n\nSecond: {json2}");
        let result = extract_review_result(&text).expect("should parse");
        assert!(result.revision_warranted, "should use the last valid result");
    }

    #[test]
    fn extract_review_result_ignores_non_review_result_json_objects() {
        let text = r#"Some context {"key": "value", "unrelated": true} then prose."#;
        assert!(extract_review_result(text).is_none());
    }

    /// Regression test for T1359 — the EXACT quark ReviewResult JSON that was
    /// silently dropped by `extract_review_result` in boss-v1.0.88 (exec
    /// `exec_18b5da2a31922490_161`). The JSON is fed BARE (no ` ```json ` fence)
    /// exactly as the reviewer emitted it. If this test fails the T1359 failure
    /// mode is still live; if it passes the parser handles this specific input.
    ///
    /// Key diagnostic targets:
    /// (a) `extract_balanced_object` mis-bounding on `\"${NOTES_FILE}\"` in
    ///     finding[2].detail (escaped quotes + `${...}` inside a string literal).
    /// (b) `ReviewResult` serde rejecting a present field such as `regression_check`.
    #[test]
    fn extract_review_result_t1359_exact_quark_json_bare_unfenced() {
        // This is quark's verbatim ReviewResult from exec exec_18b5da2a31922490_161.
        // The text is the DECODED reviewer message (what read_final_triage_message
        // returns after parsing the JSONL transcript). The JSON is bare — no fence.
        let bare_json = r#"{
"pr_url": "https://github.com/spinyfin/mono/pull/1361",
"head_sha": "0caebb932d3cd7af212cb7bf31592e8801bcf365",
"summary": "The PR correctly swaps GitHub's --generate-notes for bin/changelog in both the boss and checkleft release steps, reusing the existing per-product LAST_TAG/NEW_TAG (no global-latest-tag regression), routing through repobin dispatch (changelog is registered in REPOBIN.toml; --no-defaults only skips writing repobin.yaml, symlinks for configured tools including changelog are still created), and the --project/--from/--to/--repo/--enrich flags all match tools/changelog's CLI. Two substantive issues remain. (1) The changelog extracts commits with a LOCAL `git log <from>..<to>` (tools/changelog/src/extract.rs get_commits), which succeeds-but-truncates on a shallow Buildkite checkout; the repo is only unshallowed on the non-manual change-detection path, so manual (ui/api) releases — and the LAST_SHA-unresolved cron edge — can silently produce an incomplete/empty release body. (2) In boss-release.sh the new `bazel build`/changelog block is placed AFTER `trap - ERR` is removed and is not covered by the EXIT trap (which only removes WORK_DIR), so a failure there leaks the already-pushed boss-v* tag with no release and no cleanup, wedging subsequent releases on a duplicate-tag push; checkleft handles the equivalent window correctly via its cleanup() EXIT trap (TAG_PUSHED guard). No unrelated features were dropped.",
"revision_warranted": true,
"findings": [
{
"severity": "high",
"category": "correctness",
"file": ".buildkite/steps/checkleft-release.sh",
"location": "phase_prepare, ~L336-351 (and boss-release.sh ~L297-311)",
"title": "changelog reads local git history that isn't unshallowed on manual releases → silently truncated notes",
"detail": "The changelog tool builds the body from a local `git log <LAST_TAG>..<NEW_TAG>` (tools/changelog/src/extract.rs get_commits, line ~147). The old `gh release create --generate-notes` computed notes server-side from GitHub, so a shallow Buildkite checkout was fine; the new approach needs the full local commit range. The repo is only unshallowed inside the change-detection path (checkleft should_skip L246-248, boss L96-98), which is SKIPPED for manual (ui/api) triggers (checkleft is_manual returns early at L232-235; boss skips at L85-86) and is also skipped on the cron edge where LAST_SHA fails to resolve (boss L92-93 'proceeds'). git log on a shallow clone returns success with a truncated/empty set rather than failing, so the release body is silently wrong — directly violating the acceptance criterion that the body contain ALL product-owned commits in the range. Fix: before invoking changelog (when LAST_TAG is non-empty), ensure full history, e.g. `if git rev-parse --is-shallow-repository | grep -q true; then git fetch --unshallow origin || true; fi`, in BOTH scripts, so every trigger path (manual included) renders the complete range.",
"confidence": "medium"
},
{
"severity": "medium",
"category": "correctness",
"file": ".buildkite/steps/boss-release.sh",
"location": "~L280-318 (after `trap - ERR`)",
"title": "boss: fallible `bazel build`/changelog runs after tag-cleanup trap is disarmed → leaked tag wedges future releases",
"detail": "The new notes-generation block (L297-311), which includes `bazel build //tools/repobin:repobin` and the changelog dispatch (itself another bazel build), runs AFTER `trap - ERR` is cleared at L280. The only remaining trap is the EXIT handler set at L288, which removes WORK_DIR but does NOT delete the pushed tag. So if repobin/changelog build or `bin/changelog` fails under `set -e`, the script aborts with boss-v1.0.N already pushed (L199) and no release created or cleaned up. Because the next run computes the version from `gh release list` (L167, releases not tags), it recomputes the same N and `git push origin refs/tags/boss-v1.0.N` (L199) then fails on the pre-existing remote tag — permanently blocking boss releases until someone manually deletes the orphan tag. checkleft avoids this: its cleanup() EXIT trap deletes the leaked tag while TAG_PUSHED=1 (reset to 0 only after the release is created, L361). Fix: in boss, either generate the notes before pushing the tag / before `trap - ERR` (the changelog only needs the LOCAL tag created at L198, so it can run earlier under ERR-trap protection), or extend the EXIT trap to delete the pushed tag if the release was never created.",
"confidence": "medium"
},
{
"severity": "low",
"category": "edgecase",
"file": ".buildkite/steps/boss-release.sh",
"location": "~L298-318 (and checkleft ~L338-357)",
"title": "Notes temp file leaks when `gh release create` fails",
"detail": "`rm -f \"${NOTES_FILE}\"` (boss L318) / `rm -f \"${notes_file}\"` (checkleft L357) runs only after a successful `gh release create`; under `set -e` a failed release create skips the rm, leaving /tmp/*-release-notes-*.md behind. Minor, but easily made robust by registering the temp file in the existing EXIT trap (boss already has one for WORK_DIR; checkleft's cleanup() could `rm -f` it) instead of an inline rm.",
"confidence": "high"
}
],
"regression_check": {
"performed": true,
"suspected_deletions": []
}
}"#;

        let result =
            extract_review_result(bare_json).expect("T1359 exact quark JSON (bare, unfenced) must parse successfully");
        assert!(result.revision_warranted, "revision_warranted must be true");
        assert!(
            result
                .findings
                .iter()
                .any(|f| matches!(f.severity, ReviewFindingSeverity::High)),
            "high-severity finding must be present",
        );
        assert!(
            result
                .findings
                .iter()
                .any(|f| matches!(f.category, ReviewFindingCategory::Correctness)),
            "correctness finding must be present",
        );
    }

    /// Regression test for T1359: bare JSON with RICH text in summary/detail
    /// fields — bash code, escaped quotes, `${VAR}` syntax, and backtick fences
    /// embedded in the finding's `detail`. This mimics the quark reviewer output
    /// that defeated the original bare-JSON scanner.
    ///
    /// The scanner must correctly skip braces inside JSON string literals
    /// even when the strings contain `${...}`, `\"`, and backtick code blocks.
    #[test]
    fn extract_review_result_bare_json_rich_text_with_embedded_code_and_braces() {
        // Construct a JSON that closely resembles what quark emitted for T1359.
        // The `detail` field contains bash code with `${TAG}` syntax (braces inside
        // a JSON string literal) and escaped quotes — the suspected failure vector.
        let json = serde_json::json!({
            "pr_url": "https://github.com/brianduff/mono/pull/1361",
            "head_sha": "deadbeef",
            "summary": "Found a correctness bug in tools/boss/release/boss-release.sh. The script creates a git tag before pushing it (~L42), but if `git push --tags` fails the orphan tag persists locally. On the next release attempt the script would fail with \"tag already exists\".",
            "revision_warranted": true,
            "findings": [
                {
                    "severity": "high",
                    "category": "correctness",
                    "file": "tools/boss/release/boss-release.sh",
                    "location": "~L42-52",
                    "title": "Orphan tag leak when git push fails",
                    "detail": "The script creates the tag before verifying the push:\n\n```bash\ngit tag -a \"${TAG}\" -m \"Release ${TAG}\"\ngit push --tags\n```\n\nIf `git push --tags` fails (auth error, network timeout) the local tag persists. The next run hits \"fatal: tag '${TAG}' already exists\". Fix: tag AFTER push, or clean up on failure with `git push --tags || git tag -d \"${TAG}\"`.",
                    "confidence": "high"
                },
                {
                    "severity": "medium",
                    "category": "correctness",
                    "file": "tools/boss/release/boss-release.sh",
                    "title": "Missing set -euo pipefail",
                    "detail": "No `set -euo pipefail` at the top; a failed intermediate command silently continues. Add it as the first non-comment line.",
                    "confidence": "medium"
                }
            ],
            "regression_check": {
                "performed": true,
                "suspected_deletions": []
            }
        })
        .to_string();

        // Emit the JSON BARE — no code fence anywhere in the message (T1359 shape).
        let text = format!(
            "I reviewed PR #1361. Key findings:\n\n\
             The main issue is an orphan-tag leak in the release script. \
             Full structured result:\n\n{json}"
        );
        let result =
            extract_review_result(&text).expect("rich bare-JSON ReviewResult must be extracted (T1359 regression)");
        assert!(result.revision_warranted, "revision_warranted must be true");
        assert_eq!(result.findings.len(), 2, "must recover both findings");
        assert_eq!(
            result.findings[0].severity,
            ReviewFindingSeverity::High,
            "first finding must be high severity",
        );
        assert_eq!(result.findings[0].category, ReviewFindingCategory::Correctness,);
    }

    /// Regression fixture for T1359: when the bare JSON is preceded by prose
    /// that contains `${VARIABLE}` syntax (which contains `{` and `}` characters),
    /// the scanner must NOT be confused by those brace pairs and must still find
    /// the actual ReviewResult that follows.
    #[test]
    fn extract_review_result_bare_json_with_braces_in_preceding_prose() {
        let json = serde_json::json!({
            "pr_url": "https://github.com/org/repo/pull/99",
            "head_sha": "deadbeef",
            "summary": "Found issue with variable substitution.",
            "revision_warranted": true,
            "findings": [
                {
                    "severity": "high",
                    "category": "correctness",
                    "file": "script.sh",
                    "title": "Orphan tag leak",
                    "detail": "The call `git tag -a \"${TAG}\"` runs before the push check.",
                    "confidence": "high"
                }
            ],
            "regression_check": {
                "performed": true,
                "suspected_deletions": []
            }
        })
        .to_string();

        // Prose BEFORE the JSON contains ${TAG} and ${RELEASE} — braces that
        // must not confuse the balanced-brace scanner.
        let text = format!(
            "The release script sets TAG=${{TAG}} and runs `git push ${{RELEASE}}`.\n\n\
             If the push fails, the local tag at ${{TAG}} persists.\n\n{json}"
        );
        let result =
            extract_review_result(&text).expect("ReviewResult must be found even when preceding prose has bare braces");
        assert!(result.revision_warranted);
        assert_eq!(result.findings.len(), 1);
    }

    /// `extract_review_result_verbose` must return the serde error text when a
    /// fenced JSON block is present but fails to deserialize as `ReviewResult`.
    /// The error text is used in the reviewer re-prompt so the reviewer can
    /// correct the specific malformation rather than blindly rewriting.
    #[test]
    fn extract_review_result_verbose_returns_error_on_malformed_fenced_json() {
        // findings is a string instead of an array — valid JSON but wrong type.
        let text = concat!(
            "Here is my review:\n\n```json\n",
            "{\"pr_url\":\"https://github.com/org/repo/pull/1\",",
            "\"head_sha\":\"abc\",\"summary\":\"s\",\"revision_warranted\":true,",
            "\"findings\":\"not-an-array\",",
            "\"regression_check\":{\"performed\":true,\"suspected_deletions\":[]}}\n",
            "```\n"
        );
        let (result, err) = extract_review_result_verbose(text);
        assert!(result.is_none(), "malformed JSON must not produce a result");
        let err_text = err.expect("error text must be returned for a malformed fenced block");
        assert!(!err_text.is_empty(), "error text must not be empty; got: {err_text}",);
    }

    /// Regression: a plain (non-JSON) fenced block earlier in the message
    /// must not shadow the real `ReviewResult` parse error from a later
    /// malformed bare `ReviewResult` — the reviewer re-prompt needs the
    /// specific field error, not a generic "expected value" from the junk
    /// fenced prose.
    #[test]
    fn extract_review_result_verbose_prefers_named_error_over_unrelated_fenced_block() {
        let text = concat!(
            "```\n",
            "not json\n",
            "```\n\n",
            "{\"pr_url\":\"https://github.com/org/repo/pull/1\",",
            "\"head_sha\":\"abc\",\"summary\":\"s\",\"revision_warranted\":true,",
            "\"findings\":\"not-an-array\",",
            "\"regression_check\":{\"performed\":true,\"suspected_deletions\":[]}}\n",
        );
        let (result, err) = extract_review_result_verbose(text);
        assert!(result.is_none(), "malformed JSON must not produce a result");
        let err_text = err.expect("error text must be returned");
        assert!(
            err_text.contains("not-an-array"),
            "error must name the malformed ReviewResult field's bad value, not a generic \
             \"expected value\" error from the junk fenced block; got: {err_text}",
        );
    }

    fn make_finding(severity: ReviewFindingSeverity, category: ReviewFindingCategory) -> ReviewFinding {
        ReviewFinding::builder()
            .severity(severity)
            .category(category)
            .file("src/lib.rs")
            .title("test finding")
            .detail("something concrete")
            .confidence(ReviewFindingConfidence::High)
            .build()
    }

    #[test]
    fn severity_gate_passes_on_critical() {
        let result = ReviewResult {
            pr_url: String::new(),
            head_sha: String::new(),
            summary: String::new(),
            revision_warranted: false,
            findings: vec![make_finding(
                ReviewFindingSeverity::Critical,
                ReviewFindingCategory::Correctness,
            )],
            regression_check: RegressionCheck {
                performed: true,
                suspected_deletions: vec![],
            },
        };
        assert!(passes_severity_gate(&result));
    }

    #[test]
    fn severity_gate_passes_on_high() {
        let result = ReviewResult {
            pr_url: String::new(),
            head_sha: String::new(),
            summary: String::new(),
            revision_warranted: false,
            findings: vec![make_finding(
                ReviewFindingSeverity::High,
                ReviewFindingCategory::Architecture,
            )],
            regression_check: RegressionCheck {
                performed: true,
                suspected_deletions: vec![],
            },
        };
        assert!(passes_severity_gate(&result));
    }

    #[test]
    fn severity_gate_passes_on_regression_regardless_of_severity() {
        let result = ReviewResult {
            pr_url: String::new(),
            head_sha: String::new(),
            summary: String::new(),
            revision_warranted: false,
            findings: vec![make_finding(
                ReviewFindingSeverity::Low,
                ReviewFindingCategory::Regression,
            )],
            regression_check: RegressionCheck {
                performed: true,
                suspected_deletions: vec![],
            },
        };
        assert!(passes_severity_gate(&result));
    }

    /// Confirmed infrastructure-duplication findings must force a revision
    /// exactly like regression findings, regardless of assigned severity
    /// (operator directive: "revision required", not advisory).
    #[test]
    fn severity_gate_passes_on_duplication_regardless_of_severity() {
        let result = ReviewResult {
            pr_url: String::new(),
            head_sha: String::new(),
            summary: String::new(),
            revision_warranted: false,
            findings: vec![make_finding(
                ReviewFindingSeverity::Low,
                ReviewFindingCategory::Duplication,
            )],
            regression_check: RegressionCheck {
                performed: true,
                suspected_deletions: vec![],
            },
        };
        assert!(
            passes_severity_gate(&result),
            "a duplication finding must force a revision even at low severity"
        );
    }

    /// Undeclared/misdeclared deferred-scope findings must force a revision
    /// exactly like regression/duplication findings, regardless of assigned
    /// severity (operator directive, 2026-07-14: undeclared deferral is a
    /// process gap, not a style nit).
    #[test]
    fn severity_gate_passes_on_deferred_scope_regardless_of_severity() {
        let result = ReviewResult {
            pr_url: String::new(),
            head_sha: String::new(),
            summary: String::new(),
            revision_warranted: false,
            findings: vec![make_finding(
                ReviewFindingSeverity::Low,
                ReviewFindingCategory::DeferredScope,
            )],
            regression_check: RegressionCheck {
                performed: true,
                suspected_deletions: vec![],
            },
        };
        assert!(
            passes_severity_gate(&result),
            "a deferred-scope finding must force a revision even at low severity"
        );
    }

    /// Agent-isms in code comments must force a revision exactly like
    /// regression/duplication/deferred-scope findings, regardless of
    /// assigned severity — agent-authored scaffolding left in comments is a
    /// process gap, not a style nit.
    #[test]
    fn severity_gate_passes_on_agent_isms_regardless_of_severity() {
        let result = ReviewResult {
            pr_url: String::new(),
            head_sha: String::new(),
            summary: String::new(),
            revision_warranted: false,
            findings: vec![make_finding(
                ReviewFindingSeverity::Low,
                ReviewFindingCategory::AgentIsms,
            )],
            regression_check: RegressionCheck {
                performed: true,
                suspected_deletions: vec![],
            },
        };
        assert!(
            passes_severity_gate(&result),
            "an agent-isms finding must force a revision even at low severity"
        );
    }

    #[test]
    fn severity_gate_blocked_on_medium_non_regression() {
        let result = ReviewResult {
            pr_url: String::new(),
            head_sha: String::new(),
            summary: String::new(),
            revision_warranted: true, // reviewer says warranted but engine gate disagrees
            findings: vec![make_finding(
                ReviewFindingSeverity::Medium,
                ReviewFindingCategory::Readability,
            )],
            regression_check: RegressionCheck {
                performed: true,
                suspected_deletions: vec![],
            },
        };
        assert!(!passes_severity_gate(&result));
    }

    #[test]
    fn severity_gate_blocked_on_empty_findings() {
        let result = ReviewResult {
            pr_url: String::new(),
            head_sha: String::new(),
            summary: String::new(),
            revision_warranted: false,
            findings: vec![],
            regression_check: RegressionCheck {
                performed: true,
                suspected_deletions: vec![],
            },
        };
        assert!(!passes_severity_gate(&result));
    }

    /// Companion to [`duplication_finding_forces_revision_pr_1690_fixture`]:
    /// an innocent PR with no duplicated infrastructure (only a low-severity
    /// readability nit) must NOT trip the severity gate.
    #[test]
    fn innocent_pr_without_duplication_does_not_force_revision() {
        let result = ReviewResult::from_json(
            &serde_json::json!({
                "pr_url": "https://github.com/spinyfin/mono/pull/1691",
                "head_sha": "cafef00d",
                "summary": "Small, clean change reusing the existing planner.rs Anthropic \
                    client for a new prompt variant. No duplicated infrastructure found.",
                "revision_warranted": false,
                "findings": [
                    {
                        "severity": "low",
                        "category": "readability",
                        "file": "tools/boss/engine/core/src/planner.rs",
                        "title": "Minor naming nit",
                        "detail": "Consider renaming `tmp` to `draft_prompt` for clarity.",
                        "confidence": "low"
                    }
                ],
                "regression_check": {"performed": true, "suspected_deletions": []}
            })
            .to_string(),
        )
        .expect("fixture ReviewResult must parse");

        assert!(
            !passes_severity_gate(&result),
            "an innocent PR with no duplication/regression/critical findings must not force a revision"
        );
    }
}
