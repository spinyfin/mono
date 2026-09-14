//! Prompt, source-context rendering, and output validation for PR review
//! guides (`ExecutionKind::PrReviewGuide`).
//!
//! This crate owns the exact versioned prompt template, how a captured
//! [`boss_pr_review_sources::SourcePacket`] is rendered into the driver's
//! read-only source context, and how the driver's raw Markdown response is
//! validated before it may become a readable guide version. It has no
//! database or engine dependency: `engine/core` owns durable attempts,
//! versions, and publication fencing, while this crate is pure, testable
//! transformation logic reused by both. See
//! `tools/boss/docs/designs/automatic-pr-review-guides.md`.

use std::fmt;

use boss_pr_review_sources::{SourcePacket, SourceSide, validate_pinned_reference};

/// The prompt contract version this crate implements. A change to
/// [`PROMPT_TEMPLATE_V1`] (or its substitution behavior) must land as a new
/// version constant and prompt id — the desired-comparison key an attempt
/// binds to includes the prompt version, so a prompt change never silently
/// reinterprets an already-captured comparison's existing readable version.
pub const PROMPT_VERSION: &str = "review-guide-v1";

/// The exact production prompt template, byte-identical to the fenced block
/// in `automatic-pr-review-guides.md`'s "Prompt contract" section. Only the
/// five `{{PLACEHOLDER}}` tokens are substituted by [`render_prompt`]; the
/// packet/broker context is supplied separately (see [`render_source_context`]).
///
/// Do not hand-edit this string without also updating
/// [`PROMPT_TEMPLATE_V1_SHA256`] and bumping [`PROMPT_VERSION`] — the
/// `prompt_template_v1_hash_is_pinned` test fails loudly on any byte drift so
/// a prompt change is always a visible, deliberate, versioned decision.
pub const PROMPT_TEMPLATE_V1: &str = "I want you to provide me a guided summary of the changes in {{PR_URL}}. The summary should break down as:

1. a general overview of the problem being solved.
2. a general overview of the core fix / implementation.
3. a runthrough of major changes to logic and architecture in the change, with a primary focus on the core change that fixed the problem / implemented the solution.
4. a summary of what tests were added, and what test infrastructure was modified to support it.

This is meant to function as a human guide to code review, so it should reference and include code snippets, but not giant diffs.

Make the core fix concrete with one worked example. Give the input and relevant state, trace the decisive old and new behavior, and show the observable result. Choose an example supported by the implementation or tests; label invented inputs as illustrative. Include a contrasting boundary or failure case only when it helps explain the changed contract. For changes without a runtime behavior, use an equivalent concrete before/after scenario. Check every step against the actual code.

Review context:
- Repository: {{REPOSITORY}}
- PR title: {{PR_TITLE}}
- Base revision: {{BASE_SHA}}
- Head revision: {{HEAD_SHA}}
- The accompanying source context and available read tools provide the PR description, diff, before/after files, related source and tests, and validated GitHub link targets.

Ground the guide in those revisions. Inspect relevant callers, helpers, types, and tests when they determine what the change actually does. Treat the PR description and code comments as statements to verify against the implementation. Distinguish enforced behavior from conventions, prompt instructions, and assumptions. Do not turn a conditional or local check into a broader guarantee.

Organize the walkthrough in a useful reading order through the core implementation. Explain why the important pieces fit together, not just which files changed. Prioritize details that help a reviewer understand or verify the fix. Use short faithful excerpts; clearly label condensed pseudocode. Avoid repetitive summaries and incidental cleanup unless it matters to the solution.

Link the core fix, worked example, and important test changes to the supplied GitHub diff locations. Use revision-pinned source links for relevant unchanged context or lines outside the displayed diff. Reuse supplied URLs or validated reference mappings and check that each link targets the code being discussed. Do not invent diff anchors or imply that a mutable PR URL identifies an immutable revision. Where an exact diff link is unavailable, use the corresponding pinned source link.

In the tests section, distinguish added, modified, and removed tests. Name the important scenarios and assertions; identify relevant fixture, helper, and build/configuration changes. Include counts only when verified and useful. Distinguish author-reported validation from checks you actually performed and from conclusions drawn by reading the tests. Do not claim to have run tests when you have not. State important coverage limits without producing an exhaustive speculative bug hunt.

Use the complete revised PR comparison if this is a regenerated guide. Do not describe only the latest incremental commit. Existing comments may provide context, but the explanation must match the actual current source revisions.

Return only the finished Markdown guide, with a descriptive title and the four requested main sections. Put the worked example within the implementation walkthrough. Keep the guide as concise as the explanation permits while preserving useful reasoning and evidence. Do not include a chat preamble, model details, internal tool logs, a merge recommendation, or an unsupported declaration that the PR is safe to merge. If essential context cannot be obtained, state the specific limitation rather than inventing behavior.";

/// SHA-256 of [`PROMPT_TEMPLATE_V1`] (UTF-8, excluding any fence/terminal
/// newline) — matches the value recorded in the design doc, computed
/// independently from the doc's own fenced block as a second source of
/// truth. See `prompt_template_v1_hash_is_pinned`.
pub const PROMPT_TEMPLATE_V1_SHA256: &str = "77d3ff117a07898771b4802b7d1f0c195b4fb4112cf1f58d6fb57fdb6639c543";

/// The metadata substituted into [`PROMPT_TEMPLATE_V1`] for one comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptMetadata<'a> {
    pub pr_url: &'a str,
    pub repository: &'a str,
    pub pr_title: &'a str,
    pub base_sha: &'a str,
    pub head_sha: &'a str,
}

/// Substitute only the five metadata placeholders into [`PROMPT_TEMPLATE_V1`].
/// Packet/broker context is not part of this string — callers append
/// [`render_source_context`] separately, keeping "the guide task" and "the
/// source material" visibly distinct in the rendered prompt.
pub fn render_prompt(metadata: &PromptMetadata<'_>) -> String {
    PROMPT_TEMPLATE_V1
        .replace("{{PR_URL}}", metadata.pr_url)
        .replace("{{REPOSITORY}}", metadata.repository)
        .replace("{{PR_TITLE}}", metadata.pr_title)
        .replace("{{BASE_SHA}}", metadata.base_sha)
        .replace("{{HEAD_SHA}}", metadata.head_sha)
}

/// Render every changed file's pinned before/after content and diff hunk
/// from the immutable packet into read-only Markdown context.
///
/// This is the review-guide worker's entire "source access": the worker has
/// no leased checkout, no shell, and no interactive read tool (its guard
/// blocks every `PreToolUse` call — see
/// `boss_engine_driver::codex::review_guide_guard`), so every pinned line it
/// can possibly cite must already be present here. The rendering is
/// revision-aware by construction: it reads only the packet's already-pinned
/// `before`/`after` content, never a live checkout or a moving branch.
pub fn render_source_context(packet: &SourcePacket) -> String {
    let mut out = String::new();
    out.push_str("## Source context\n\n");
    out.push_str(&format!(
        "Comparison: `{}` base `{}` (merge base `{}`) to head `{}`.\n\n",
        packet.canonical_pr_url, packet.observed_base_sha, packet.merge_base_sha, packet.head_sha
    ));
    if let Some(body) = &packet.body
        && !body.is_empty()
    {
        out.push_str("### PR description\n\n");
        out.push_str(body);
        out.push_str("\n\n");
    }
    for file in &packet.files {
        out.push_str(&format!("### `{}` ({:?})\n\n", file.path, file.change_kind));
        if let Some(previous) = &file.previous_path {
            out.push_str(&format!("Renamed from `{previous}`.\n\n"));
        }
        if let Some(patch) = &file.patch {
            out.push_str("Diff hunk:\n\n```diff\n");
            out.push_str(patch);
            out.push_str("\n```\n\n");
        }
        render_pinned_side(&mut out, "Before (merge base)", file.before.as_ref());
        render_pinned_side(&mut out, "After (head)", file.after.as_ref());
    }
    if !packet.omissions.is_empty() {
        out.push_str("### Collection omissions\n\n");
        out.push_str(
            "The following source material could not be captured. Do not invent content for these; state the \
             limitation instead.\n\n",
        );
        for omission in &packet.omissions {
            let path = omission.path.as_deref().unwrap_or("(unknown path)");
            let side = omission
                .side
                .map(|side| format!("{side:?}"))
                .unwrap_or_else(|| "both".to_owned());
            out.push_str(&format!("- `{path}` ({side}): {}\n", omission.reason));
        }
        out.push('\n');
    }
    out
}

fn render_pinned_side(out: &mut String, label: &str, source: Option<&boss_pr_review_sources::PinnedSource>) {
    let Some(source) = source else { return };
    out.push_str(&format!("**{label}** (`{}` @ `{}`)", source.path, source.sha));
    match (&source.content, &source.omission) {
        (Some(content), _) => {
            out.push_str(":\n\n```\n");
            out.push_str(content);
            if !content.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```\n\n");
        }
        (None, Some(reason)) => {
            out.push_str(&format!(" — omitted: {reason}\n\n"));
        }
        (None, None) => out.push_str(" — omitted: no content captured\n\n"),
    }
}

/// A guide that passed structural and reference validation, ready to become
/// a readable version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedGuide {
    pub markdown: String,
}

/// One reason a raw driver response was refused as a readable guide.
/// Deliberately not `std::error::Error`-only text: a caller that must
/// classify a failure (retryable vs. requires a human) matches on this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuideValidationIssue {
    /// No text at all, or only whitespace.
    Empty,
    /// No top-level (`# `) title heading.
    MissingTitle,
    /// Fewer than [`MIN_SECTION_HEADINGS`] second-level (or deeper) headings
    /// after the title — the four requested sections did not materialize as
    /// a structured walkthrough.
    TooFewSections { found: usize },
    /// A `github.com` link the model wrote does not resolve to any pinned
    /// source in the packet — an invented anchor, a stale/foreign SHA, or a
    /// line range outside the captured content.
    InventedReference { href: String },
    /// A `github.com` link uses a navigation shape this contract never hands
    /// out as validated (current-PR-diff `#diff-` fragments, repository
    /// `/compare/` pages) — see the design's "GitHub navigation contract".
    /// Only the pinned-source `blob/{sha}/...#L..` shape is supported today.
    UnsupportedNavigation { href: String },
}

impl fmt::Display for GuideValidationIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "the driver returned no text"),
            Self::MissingTitle => write!(f, "the guide has no top-level `#` title"),
            Self::TooFewSections { found } => {
                write!(
                    f,
                    "the guide has only {found} section heading(s); the four requested sections are missing"
                )
            }
            Self::InventedReference { href } => {
                write!(
                    f,
                    "reference `{href}` does not resolve to any pinned source in the packet"
                )
            }
            Self::UnsupportedNavigation { href } => {
                write!(
                    f,
                    "reference `{href}` uses an unsupported navigation shape (only pinned-source blob links are validated)"
                )
            }
        }
    }
}

/// Minimum count of `##`-or-deeper headings (after the title) a valid guide
/// must have — a structural proxy for "the four requested main sections are
/// present", not a semantic check. The prompt asks for four; the worked
/// example nests inside the implementation walkthrough rather than adding a
/// fifth, so four is both the floor and the expected count.
pub const MIN_SECTION_HEADINGS: usize = 4;

/// Validate a driver's raw Markdown response against the packet it was
/// generated from.
///
/// This is mechanical validation only — required sections present, a title,
/// and no invented/unsupported GitHub reference. It cannot establish that
/// the explanation is *correct*: "Packet availability does not prove the
/// model inspected or correctly understood every relevant line" (design,
/// "Source acquisition at pinned revisions"). Human sampling covers that.
pub fn validate_guide_output(raw: &str, packet: &SourcePacket) -> Result<ValidatedGuide, Vec<GuideValidationIssue>> {
    let mut issues = Vec::new();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(vec![GuideValidationIssue::Empty]);
    }

    let headings = markdown_headings(trimmed);
    if !headings.iter().any(|(level, _)| *level == 1) {
        issues.push(GuideValidationIssue::MissingTitle);
    }
    let section_count = headings.iter().filter(|(level, _)| *level >= 2).count();
    if section_count < MIN_SECTION_HEADINGS {
        issues.push(GuideValidationIssue::TooFewSections { found: section_count });
    }

    for href in github_links(trimmed) {
        if let Err(issue) = validate_reference(&href, packet) {
            issues.push(issue);
        }
    }

    if issues.is_empty() {
        Ok(ValidatedGuide {
            markdown: trimmed.to_owned(),
        })
    } else {
        Err(issues)
    }
}

/// `(level, text)` for every Markdown ATX heading (`# ` through `###### `)
/// at the start of a line.
fn markdown_headings(text: &str) -> Vec<(u8, &str)> {
    text.lines()
        .filter_map(|line| {
            let trimmed_start = line.trim_start();
            let hashes = trimmed_start.chars().take_while(|c| *c == '#').count();
            if hashes == 0 || hashes > 6 {
                return None;
            }
            let rest = &trimmed_start[hashes..];
            if !rest.starts_with(' ') && !rest.is_empty() {
                return None;
            }
            Some((hashes as u8, rest.trim()))
        })
        .collect()
}

/// Every `https://github.com/...` href inside a Markdown link or bare
/// autolink in `text`.
fn github_links(text: &str) -> Vec<String> {
    let link_re = regex::Regex::new(r"\]\((https://github\.com/[^\s)]+)\)").expect("static regex");
    let bare_re = regex::Regex::new(r"(?:^|[\s(])(https://github\.com/\S+)").expect("static regex");
    let mut hrefs: Vec<String> = link_re
        .captures_iter(text)
        .map(|caps| caps[1].trim_end_matches(['.', ',', ')']).to_owned())
        .collect();
    for caps in bare_re.captures_iter(text) {
        let href = caps[1].trim_end_matches(['.', ',', ')']).to_owned();
        if !hrefs.contains(&href) {
            hrefs.push(href);
        }
    }
    hrefs
}

/// Classify and validate one extracted `github.com` href against the packet.
fn validate_reference(href: &str, packet: &SourcePacket) -> Result<(), GuideValidationIssue> {
    if let Some(parsed) = parse_pinned_blob_href(href) {
        let (owner_repo, sha, path, start, end) = parsed;
        for side in [SourceSide::Before, SourceSide::After] {
            if let Ok(reference) = validate_pinned_reference(packet, side, &path, start, end)
                && reference.href == href
                && reference_repository_matches(packet, side, &owner_repo, &sha)
            {
                return Ok(());
            }
        }
        return Err(GuideValidationIssue::InventedReference { href: href.to_owned() });
    }
    if href.contains("/files#diff-") || href.contains("/compare/") {
        return Err(GuideValidationIssue::UnsupportedNavigation { href: href.to_owned() });
    }
    // A bare PR/issue/repo link with no line-anchored content claim (e.g. the
    // canonical `{{PR_URL}}` itself, restated in prose) makes no pinned-source
    // claim this validator can check, so it is not treated as invented.
    Ok(())
}

fn reference_repository_matches(packet: &SourcePacket, side: SourceSide, owner_repo: &str, sha: &str) -> bool {
    match side {
        SourceSide::Before => owner_repo == packet.base_repository && sha == packet.merge_base_sha,
        SourceSide::After => owner_repo == packet.head_repository && sha == packet.head_sha,
    }
}

/// Parse a pinned-source blob URL of the shape
/// `https://github.com/{owner}/{repo}/blob/{40-hex-sha}/{path}#L{n}` or
/// `#L{start}-L{end}`, matching exactly what
/// `boss_pr_review_sources::validate_pinned_reference` emits. Any other
/// shape (no fragment, non-hex/short SHA, missing path) returns `None` and
/// is handled by the caller as an unsupported/unverifiable reference.
fn parse_pinned_blob_href(href: &str) -> Option<(String, String, String, u32, u32)> {
    let re =
        regex::Regex::new(r"^https://github\.com/([^/]+/[^/]+)/blob/([0-9a-fA-F]{40})/([^#]+)#L(\d+)(?:-L(\d+))?$")
            .expect("static regex");
    let caps = re.captures(href)?;
    let owner_repo = caps[1].to_owned();
    let sha = caps[2].to_owned();
    let path = percent_decode(&caps[3]);
    let start: u32 = caps[4].parse().ok()?;
    let end: u32 = caps.get(5).map(|m| m.as_str().parse().ok()).unwrap_or(Some(start))?;
    Some((owner_repo, sha, path, start, end))
}

/// Minimal percent-decoder for the subset [`encode_path`]-style encoders
/// produce (uppercase-hex `%XX` escapes over UTF-8 bytes). Invalid escapes
/// are passed through literally rather than erroring — this only needs to
/// invert our own encoder, not accept arbitrary attacker input safely.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_pr_review_sources::{ChangeKind, PinnedSource, SourceFile};
    use sha2::{Digest, Sha256};

    fn packet() -> SourcePacket {
        SourcePacket {
            schema_version: 2,
            canonical_pr_url: "https://github.com/acme/widget/pull/4".to_owned(),
            pr_number: 4,
            title: "Fix retry backoff".to_owned(),
            body: Some("Fixes a bug in retry handling.".to_owned()),
            base_repository: "acme/widget".to_owned(),
            head_repository: "acme/widget".to_owned(),
            observed_base_sha: "a".repeat(40),
            merge_base_sha: "b".repeat(40),
            head_sha: "c".repeat(40),
            files: vec![SourceFile {
                path: "src/retry.rs".to_owned(),
                previous_path: None,
                change_kind: ChangeKind::Modified,
                additions: 3,
                deletions: 1,
                patch: Some("@@ -1,3 +1,3 @@\n-old\n+new".to_owned()),
                before: Some(
                    PinnedSource::builder()
                        .repository("acme/widget")
                        .sha("b".repeat(40))
                        .path("src/retry.rs")
                        .content("fn retry() {\n    old();\n}\n")
                        .content_hash(sha256_hex("fn retry() {\n    old();\n}\n"))
                        .byte_count(27u64)
                        .build(),
                ),
                after: Some(
                    PinnedSource::builder()
                        .repository("acme/widget")
                        .sha("c".repeat(40))
                        .path("src/retry.rs")
                        .content("fn retry() {\n    new();\n}\n")
                        .content_hash(sha256_hex("fn retry() {\n    new();\n}\n"))
                        .byte_count(27u64)
                        .build(),
                ),
            }],
            omissions: Vec::new(),
        }
    }

    fn sha256_hex(text: &str) -> String {
        Sha256::digest(text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn valid_guide_body(extra_link: &str) -> String {
        format!(
            "# Fix retry backoff\n\n\
             ## Problem\n\nRetries never stopped.\n\n\
             ## Core fix\n\nGuard the retry count. {extra_link}\n\n\
             ## Logic and architecture walkthrough\n\nSee above.\n\n\
             ## Tests\n\nAdded a regression test.\n"
        )
    }

    #[test]
    fn prompt_template_v1_hash_is_pinned() {
        let digest: String = Sha256::digest(PROMPT_TEMPLATE_V1.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            digest, PROMPT_TEMPLATE_V1_SHA256,
            "PROMPT_TEMPLATE_V1 changed without updating PROMPT_TEMPLATE_V1_SHA256 / PROMPT_VERSION",
        );
    }

    #[test]
    fn render_prompt_substitutes_only_the_five_placeholders() {
        let rendered = render_prompt(&PromptMetadata {
            pr_url: "https://github.com/acme/widget/pull/4",
            repository: "acme/widget",
            pr_title: "Fix retry backoff",
            base_sha: &"a".repeat(40),
            head_sha: &"c".repeat(40),
        });
        assert!(rendered.contains("https://github.com/acme/widget/pull/4"));
        assert!(rendered.contains("Repository: acme/widget"));
        assert!(rendered.contains(&format!("Base revision: {}", "a".repeat(40))));
        assert!(rendered.contains(&format!("Head revision: {}", "c".repeat(40))));
        assert!(!rendered.contains("{{"));
        assert!(rendered.starts_with("I want you to provide me a guided summary"));
        assert!(rendered.ends_with("rather than inventing behavior."));
    }

    #[test]
    fn source_context_embeds_pinned_content_and_diff() {
        let context = render_source_context(&packet());
        assert!(context.contains("src/retry.rs"));
        assert!(context.contains("old();"));
        assert!(context.contains("new();"));
        assert!(context.contains("@@ -1,3 +1,3 @@"));
    }

    #[test]
    fn valid_guide_with_pinned_link_passes() {
        let href = format!(
            "https://github.com/acme/widget/blob/{}/src/retry.rs#L1-L2",
            "c".repeat(40)
        );
        let link = format!("[the fix]({href})");
        let guide = validate_guide_output(&valid_guide_body(&link), &packet()).expect("must validate");
        assert!(guide.markdown.contains("Fix retry backoff"));
    }

    #[test]
    fn empty_output_is_rejected() {
        let issues = validate_guide_output("   \n\n  ", &packet()).unwrap_err();
        assert_eq!(issues, vec![GuideValidationIssue::Empty]);
    }

    #[test]
    fn missing_title_and_sections_are_both_reported() {
        let issues = validate_guide_output("Just some prose with no headings at all.", &packet()).unwrap_err();
        assert!(issues.contains(&GuideValidationIssue::MissingTitle));
        assert!(matches!(issues[1], GuideValidationIssue::TooFewSections { found: 0 }));
    }

    #[test]
    fn invented_blob_reference_is_rejected() {
        let bogus_sha = "f".repeat(40);
        let href = format!("https://github.com/acme/widget/blob/{bogus_sha}/src/retry.rs#L1-L2");
        let link = format!("[the fix]({href})");
        let issues = validate_guide_output(&valid_guide_body(&link), &packet()).unwrap_err();
        assert_eq!(issues, vec![GuideValidationIssue::InventedReference { href }]);
    }

    #[test]
    fn out_of_range_line_reference_is_rejected() {
        let href = format!(
            "https://github.com/acme/widget/blob/{}/src/retry.rs#L1-L99",
            "c".repeat(40)
        );
        let link = format!("[the fix]({href})");
        let issues = validate_guide_output(&valid_guide_body(&link), &packet()).unwrap_err();
        assert_eq!(issues, vec![GuideValidationIssue::InventedReference { href }]);
    }

    #[test]
    fn current_pr_diff_fragment_is_rejected_as_unsupported() {
        let href = "https://github.com/acme/widget/pull/4/files#diff-abc123L10".to_owned();
        let link = format!("[the fix]({href})");
        let issues = validate_guide_output(&valid_guide_body(&link), &packet()).unwrap_err();
        assert_eq!(issues, vec![GuideValidationIssue::UnsupportedNavigation { href }]);
    }

    #[test]
    fn compare_page_fragment_is_rejected_as_unsupported() {
        let href = format!(
            "https://github.com/acme/widget/compare/{}..{}",
            "b".repeat(40),
            "c".repeat(40)
        );
        let link = format!("[the diff]({href})");
        let issues = validate_guide_output(&valid_guide_body(&link), &packet()).unwrap_err();
        assert_eq!(issues, vec![GuideValidationIssue::UnsupportedNavigation { href }]);
    }

    #[test]
    fn bare_canonical_pr_url_is_not_treated_as_invented() {
        let link = format!("See {}", packet().canonical_pr_url);
        validate_guide_output(&valid_guide_body(&link), &packet()).expect("bare PR URL restated in prose is fine");
    }

    #[test]
    fn before_side_link_must_use_merge_base_sha_not_head_sha() {
        // Citing the AFTER content but labelling it with the BEFORE sha is a
        // real mistake this must catch: `validate_pinned_reference` looks up
        // by path only, so this guards the (side, repository, sha) cross-check
        // in `reference_repository_matches`, not just path/line existence.
        let href = format!(
            "https://github.com/acme/widget/blob/{}/src/retry.rs#L1-L2",
            "a".repeat(40)
        );
        let link = format!("[the fix]({href})");
        let issues = validate_guide_output(&valid_guide_body(&link), &packet()).unwrap_err();
        assert_eq!(issues, vec![GuideValidationIssue::InventedReference { href }]);
    }
}
