//! Result parsing, classification, and the engine severity gate:
//! [`classify_changed_files`], [`extract_review_result`],
//! [`extract_review_result_verbose`], and [`passes_severity_gate`].

use boss_engine_utils::json_extract::extract_balanced_object;

use super::types::*;

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
/// use boss_engine::pr_review::{classify_changed_files, ReviewScope};
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
/// assistant message (design §3 of P992, task 8).
///
/// Tries three strategies in order, returning the first successful parse:
///
/// 1. Fenced ` ```json ` block — the canonical happy-path shape.
/// 2. Plain ` ``` ` block (no language tag).
/// 3. Bare/unfenced JSON — scans for the last balanced `{…}` object in the
///    text and validates it against the `ReviewResult` schema. This handles the
///    observed failure mode where the model emits valid JSON inline after prose
///    without a code fence (e.g. "Key findings below.\n\n{ … }").
///
/// Returns `None` when no parseable `ReviewResult` is found (reviewer may
/// have crashed or emitted malformed output — the caller should fall back to
/// advancing without revision).
///
/// To also receive the serde error from the last failed parse attempt (useful
/// for surfacing in a re-prompt), use [`extract_review_result_verbose`].
pub fn extract_review_result(text: &str) -> Option<ReviewResult> {
    extract_review_result_verbose(text).0
}

/// Like [`extract_review_result`] but also returns the last serde parse error
/// when all strategies fail.
///
/// The error string names the specific field path and type mismatch so the
/// caller can include it verbatim in a reviewer re-prompt, giving the reviewer
/// signal about exactly what is wrong rather than a generic "write valid JSON"
/// message. Returns `(None, None)` when the text contains no JSON-like content
/// at all (the error is only `Some` when a JSON block was present but failed to
/// deserialize as a `ReviewResult`).
pub fn extract_review_result_verbose(text: &str) -> (Option<ReviewResult>, Option<String>) {
    let mut last_error: Option<String> = None;

    // Strategy 1: ```json fenced blocks
    let mut rest = text;
    while let Some(fence_start) = rest.find("```json") {
        let after_fence = &rest[fence_start + 7..];
        let trimmed = after_fence.trim_start_matches('\n');
        if let Some(end) = trimmed.find("```") {
            let json_str = trimmed[..end].trim();
            match ReviewResult::from_json(json_str) {
                Ok(result) => return (Some(result), None),
                Err(e) => last_error = Some(e.to_string()),
            }
        }
        rest = &rest[fence_start + 7..];
    }

    // Strategy 2: plain ``` fenced blocks (no language tag)
    let mut rest = text;
    while let Some(fence_start) = rest.find("```") {
        let after_fence = &rest[fence_start + 3..];
        // Skip if this is actually a ```json or ```jsonc block (already handled)
        let peek = after_fence.trim_start_matches('\n');
        if peek.starts_with("json") {
            rest = &rest[fence_start + 3..];
            continue;
        }
        let trimmed = after_fence.trim_start_matches('\n');
        if let Some(end) = trimmed.find("```") {
            let json_str = trimmed[..end].trim();
            match ReviewResult::from_json(json_str) {
                Ok(result) => return (Some(result), None),
                Err(e) => last_error = Some(e.to_string()),
            }
        }
        rest = &rest[fence_start + 3..];
    }

    // Strategy 3: bare/unfenced JSON — find the last balanced { … } object
    // that validates as a ReviewResult. Scanning from the end handles the
    // common "prose then trailing JSON" shape.
    let bytes = text.as_bytes();
    let mut last_result: Option<ReviewResult> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(json_str) = extract_balanced_object(&text[i..])
        {
            match ReviewResult::from_json(json_str) {
                Ok(result) => {
                    last_result = Some(result);
                }
                Err(e) => {
                    // Only surface errors from blocks that look like ReviewResults
                    // (contain "revision_warranted") to avoid noise from unrelated
                    // JSON objects in the reviewer's prose.
                    if json_str.contains("revision_warranted") {
                        last_error = Some(e.to_string());
                    }
                }
            }
            // Advance past this object to find any later one
            i += json_str.len();
            continue;
        }
        i += 1;
    }
    if last_result.is_some() {
        return (last_result, None);
    }
    (None, last_error)
}

/// Engine severity gate (design §3 of P992, task 8).
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
    use crate::pr_review::*;

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
