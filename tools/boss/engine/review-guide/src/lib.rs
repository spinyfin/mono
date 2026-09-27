//! Prompt, source-manifest rendering, and output validation for PR review
//! guides (`ExecutionKind::PrReviewGuide`).
//!
//! This crate owns the exact versioned prompt template, how a captured
//! [`boss_pr_review_sources::SourcePacket`] is rendered into a small
//! comparison manifest, and how the driver's raw Markdown response is
//! validated before it may become a readable guide version. The driver reads
//! full file content, diffs, and related code itself from its own pinned
//! read-only cube workspace (checked out at the comparison head, with
//! immutable git access to the merge base) — this crate never inlines file
//! contents, so prompt size does not scale with the size of the change. It
//! has no database or engine dependency: `engine/core` owns durable attempts,
//! versions, and publication fencing, while this crate is pure, testable
//! transformation logic reused by both. See
//! `tools/boss/docs/designs/automatic-pr-review-guides.md`.

use std::fmt;

#[cfg(test)]
use boss_pr_review_sources::validate_pinned_reference;
use boss_pr_review_sources::{SourcePacket, SourceSide};

/// The prompt contract version this crate implements. A change to
/// [`PROMPT_TEMPLATE`] (or its substitution behavior) must land as a new
/// version constant and prompt id — the desired-comparison key an attempt
/// binds to includes the prompt version, so a prompt change never silently
/// reinterprets an already-captured comparison's existing readable version.
pub const PROMPT_VERSION: &str = "review-guide-v6";

/// The exact production prompt template, byte-identical to the fenced block
/// in `automatic-pr-review-guides.md`'s "Prompt contract" section. Only the
/// five `{{PLACEHOLDER}}` tokens are substituted by [`render_prompt`]; the
/// source context is supplied separately (see [`render_source_context`]).
///
/// Do not hand-edit this string without also updating
/// [`PROMPT_TEMPLATE_SHA256`] and bumping [`PROMPT_VERSION`] — the
/// `prompt_template_hash_is_pinned` test fails loudly on any byte drift so
/// a prompt change is always a visible, deliberate, versioned decision.
pub const PROMPT_TEMPLATE: &str = "I want you to provide me a guided summary of the changes in {{PR_URL}}. The summary should break down as:

1. a general overview of the problem being solved.
2. a general overview of the core fix / implementation.
3. a runthrough of major changes to logic and architecture in the change, with a primary focus on the core change that fixed the problem / implemented the solution.
4. a summary of what tests were added, and what test infrastructure was modified to support it.

This is meant to function as a human guide to code review, so it should reference and include code snippets, but not giant diffs.

Make the core fix concrete with one worked example. Give the input and relevant state, trace the decisive old and new behavior, and show the observable result. Choose an example supported by the implementation or tests; label invented inputs as illustrative. Include a contrasting boundary or failure case only when it helps explain the changed contract. For changes without a runtime behavior, use an equivalent concrete before/after scenario. Ground every step in source you have actually read.

Review context:
- Repository: {{REPOSITORY}}
- PR title: {{PR_TITLE}}
- Merge-base revision: {{BASE_SHA}}
- Head revision: {{HEAD_SHA}}
- You have a read-only checkout of the repository at the head revision in your working directory, plus immutable git access to the merge-base revision. Read changed files, related callers, helpers, types, and tests directly from the checkout. Read a file's merge-base (\"before\") content with `git show {{BASE_SHA}}:<path>`, and its diff with `git diff --no-ext-diff --no-textconv {{BASE_SHA}} {{HEAD_SHA}} -- <path>`. Boss also supplies the PR description, the changed-file manifest, and validated GitHub link targets below. Do not invent content you have not read.

Ground the guide in those revisions. Explain relevant callers, helpers, types, and tests only to the extent you can verify them by reading the checkout. When missing context limits an explanation, state the limitation. Treat the PR description and code comments as statements to verify against the implementation. Distinguish enforced behavior from conventions, prompt instructions, and assumptions. Do not turn a conditional or local check into a broader guarantee.

Organize the walkthrough in a useful reading order through the core implementation. Explain why the important pieces fit together, not just which files changed. Prioritize details that help a reviewer understand or verify the fix. Use short faithful excerpts; clearly label condensed pseudocode. Avoid repetitive summaries and incidental cleanup unless it matters to the solution.

Link the core fix, worked example, and important test changes to the supplied GitHub diff locations. Use revision-pinned source links for relevant unchanged context or lines outside the displayed diff. Reuse supplied URLs or validated reference mappings and check that each link targets the code being discussed. Do not invent diff anchors or imply that a mutable PR URL identifies an immutable revision. Where an exact diff link is unavailable, use the corresponding pinned source link.

In the tests section, distinguish added, modified, and removed tests. Name the important scenarios and assertions; identify relevant fixture, helper, and build/configuration changes. Include counts only when supported by what you read and useful. Distinguish author-reported validation from conclusions supported by the test source you read. Do not claim to have executed tests or performed independent validation. State important coverage limits without producing an exhaustive speculative bug hunt.

Use the complete revised PR comparison if this is a regenerated guide. Do not describe only the latest incremental commit. Existing comments may provide context, but the explanation must match the actual current source revisions.

Submit the finished Markdown guide using `\"$BOSS_BIN\" propose review-guide --body '<finished Markdown guide>'`. This is the only permitted tool command. Pass the entire Markdown as a literal single-quoted shell argument (escape any apostrophe with the standard shell quote sequence); do not write a file, pipe input, use command substitution, or run any other command. The command is bound to your execution automatically. A final assistant message does not submit a guide. If submission fails, correct the reported error and retry the same command before ending. The guide must have a descriptive title and the four requested main sections. Put the worked example within the implementation walkthrough. Keep the guide as concise as the explanation permits while preserving useful reasoning and evidence. Do not include a chat preamble, model details, internal execution details, a merge recommendation, or an unsupported declaration that the PR is safe to merge. If essential context is absent from what you can read, state the specific limitation rather than inventing behavior.";

/// SHA-256 of [`PROMPT_TEMPLATE`] (UTF-8, excluding any fence/terminal
/// newline) — matches the value recorded in the design doc, computed
/// independently from the doc's own fenced block as a second source of
/// truth. See `prompt_template_hash_is_pinned`.
pub const PROMPT_TEMPLATE_SHA256: &str = "243b4702e77fb27fafce3eb6bcdde779c27d05b70d65e4c73a5451148ea01514";

/// The metadata substituted into [`PROMPT_TEMPLATE`] for one comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptMetadata<'a> {
    pub pr_url: &'a str,
    pub repository: &'a str,
    pub pr_title: &'a str,
    pub base_sha: &'a str,
    pub head_sha: &'a str,
}

/// Substitute only the five metadata placeholders into [`PROMPT_TEMPLATE`].
/// Source context is not part of this string — callers append
/// [`render_source_context`] separately, keeping "the guide task" and "the
/// source material" visibly distinct in the rendered prompt.
pub fn render_prompt(metadata: &PromptMetadata<'_>) -> String {
    PROMPT_TEMPLATE
        .replace("{{PR_URL}}", metadata.pr_url)
        .replace("{{REPOSITORY}}", metadata.repository)
        .replace("{{PR_TITLE}}", metadata.pr_title)
        .replace("{{BASE_SHA}}", metadata.base_sha)
        .replace("{{HEAD_SHA}}", metadata.head_sha)
}

/// Render the small read-only comparison manifest appended after
/// [`render_prompt`]: the comparison identity, PR description, and a
/// change-kind/line-count entry for every changed file. The driver reads full
/// file content, diffs, and related code itself from its pinned read-only
/// workspace (see [`PROMPT_TEMPLATE`]'s embedded git-read instructions) — this
/// never inlines diff hunks or before/after file content, so its size never
/// scales with the size of the change.
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

    out.push_str("### Changed files\n\n");
    for file in &packet.files {
        out.push_str(&render_file_manifest_line(file));
    }
    out.push('\n');

    if !packet.omissions.is_empty() {
        out.push_str(OMISSIONS_HEADER);
        for omission in &packet.omissions {
            out.push_str(&render_omission_line(omission));
        }
        out.push_str(OMISSIONS_FOOTER);
    }
    out
}

const OMISSIONS_HEADER: &str = "### Collection omissions\n\n\
The following source material could not be captured. Do not invent content for these; state the limitation \
instead.\n\n";
const OMISSIONS_FOOTER: &str = "\n";

fn render_omission_line(omission: &boss_pr_review_sources::SourceOmission) -> String {
    let path = omission.path.as_deref().unwrap_or("(unknown path)");
    let side = omission
        .side
        .map(|side| format!("{side:?}"))
        .unwrap_or_else(|| "both".to_owned());
    format!("- `{path}` ({side}): {}\n", omission.reason)
}

/// One line naming a changed file's path, change kind, added/removed line
/// counts, and (for a rename) its previous path. Never includes diff hunks or
/// before/after content — the driver reads those itself via `git show`/`git
/// diff` in its pinned workspace.
fn render_file_manifest_line(file: &boss_pr_review_sources::SourceFile) -> String {
    let mut line = format!(
        "- `{}` ({:?}, +{}/-{})",
        file.path, file.change_kind, file.additions, file.deletions
    );
    if let Some(previous) = &file.previous_path {
        line.push_str(&format!(", renamed from `{previous}`"));
    }
    line.push('\n');
    line
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
    /// source at the recorded head or merge-base revision — an invented anchor,
    /// a stale/foreign SHA, or a line range outside the source content.
    InventedReference { href: String },
    /// A `github.com` link uses a navigation shape this contract never hands
    /// out as validated (current-PR-diff `#diff-` fragments, repository
    /// `/compare/` pages) — see the design's "GitHub navigation contract".
    /// Only pinned-source `blob/{sha}/...` links with optional line anchors
    /// are supported.
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
                    "reference `{href}` does not resolve to source at the recorded head or merge-base revision"
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
#[cfg(test)]
fn validate_guide_output(raw: &str, packet: &SourcePacket) -> Result<ValidatedGuide, Vec<GuideValidationIssue>> {
    validate_guide_output_with_resolver(raw, packet, |sha, path, start, end| {
        [SourceSide::Before, SourceSide::After].into_iter().any(|side| {
            let expected = match side {
                SourceSide::Before => &packet.merge_base_sha,
                SourceSide::After => &packet.head_sha,
            };
            sha == expected && validate_pinned_reference(packet, side, path, start, end).is_ok()
        })
    })
}

/// Engine callers resolve links from local git objects at the pinned comparison
/// commits, including files absent from the inline packet. A (0, 0) range
/// requests validation of the whole file, including an empty file.
pub fn validate_guide_output_with_resolver(
    raw: &str,
    packet: &SourcePacket,
    resolve: impl Fn(&str, &str, u32, u32) -> bool,
) -> Result<ValidatedGuide, Vec<GuideValidationIssue>> {
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
        if let Err(issue) = validate_reference(&href, packet, &resolve) {
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
    let link_re = regex::Regex::new(r#"(https://github\.com/[^\s<>\)\]`*"']+)"#).expect("static regex");
    let mut hrefs: Vec<String> = link_re
        .captures_iter(text)
        .map(|caps| caps[1].trim_end_matches(['.', ',', ')', ';', ':', '!', '?']).to_owned())
        .collect();
    hrefs.sort();
    hrefs.dedup();
    hrefs
}

/// Classify and validate one extracted `github.com` href against the packet.
fn validate_reference(
    href: &str,
    packet: &SourcePacket,
    resolve: &impl Fn(&str, &str, u32, u32) -> bool,
) -> Result<(), GuideValidationIssue> {
    if let Some(parsed) = parse_pinned_blob_href(href) {
        let (owner_repo, sha, path, start, end) = parsed;
        for side in [SourceSide::Before, SourceSide::After] {
            if reference_repository_matches(packet, side, &owner_repo, &sha)
                && (start > 0 || (start == 0 && end == 0))
                && end >= start
                && resolve(&sha, &path, start, end)
            {
                return Ok(());
            }
        }
        return Err(GuideValidationIssue::InventedReference { href: href.to_owned() });
    }
    if href.contains("/files#diff-") || href.contains("/compare/") {
        return Err(GuideValidationIssue::UnsupportedNavigation { href: href.to_owned() });
    }
    if href.contains("/blob/") {
        return Err(GuideValidationIssue::InventedReference { href: href.to_owned() });
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
/// `#L{start}-L{end}`, matching what
/// `boss_pr_review_sources::validate_pinned_reference` emits. An omitted
/// fragment denotes the whole file and resolves with the (0, 0) range. Any other
/// shape (non-hex/short SHA, missing path) returns `None` and
/// is handled by the caller as an unsupported/unverifiable reference.
fn parse_pinned_blob_href(href: &str) -> Option<(String, String, String, u32, u32)> {
    let re = regex::Regex::new(
        r"^https://github\.com/([^/]+/[^/]+)/blob/([0-9a-fA-F]{40})/([^#]+)(?:#L(\d+)(?:-L(\d+))?)?$",
    )
    .expect("static regex");
    let caps = re.captures(href)?;
    let owner_repo = caps[1].to_owned();
    let sha = caps[2].to_owned();
    let path = percent_decode(&caps[3]);
    let start: u32 = caps.get(4).map(|m| m.as_str().parse().ok()).unwrap_or(Some(0))?;
    if caps.get(4).is_some() && start == 0 {
        return None;
    }
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
            probe_base_sha: None,
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

    /// A packet with `file_count` files, each with its own diff hunk and
    /// distinct before/after content, so manifest rendering across many
    /// files can be exercised.
    fn multi_file_packet(file_count: usize) -> SourcePacket {
        let mut base = packet();
        base.files = (0..file_count)
            .map(|i| {
                let before_content = format!("fn f_{i}() {{\n    old_{i}();\n}}\n");
                let after_content = format!("fn f_{i}() {{\n    new_{i}();\n}}\n");
                SourceFile {
                    path: format!("src/file_{i}.rs"),
                    previous_path: None,
                    change_kind: ChangeKind::Modified,
                    additions: 1,
                    deletions: 1,
                    patch: Some(format!("@@ -1,1 +1,1 @@\n-old_{i}\n+new_{i}")),
                    before: Some(
                        PinnedSource::builder()
                            .repository("acme/widget")
                            .sha("b".repeat(40))
                            .path(format!("src/file_{i}.rs"))
                            .content(before_content.clone())
                            .content_hash(sha256_hex(&before_content))
                            .byte_count(before_content.len() as u64)
                            .build(),
                    ),
                    after: Some(
                        PinnedSource::builder()
                            .repository("acme/widget")
                            .sha("c".repeat(40))
                            .path(format!("src/file_{i}.rs"))
                            .content(after_content.clone())
                            .content_hash(sha256_hex(&after_content))
                            .byte_count(after_content.len() as u64)
                            .build(),
                    ),
                }
            })
            .collect();
        base
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
    fn prompt_template_hash_is_pinned() {
        let digest: String = Sha256::digest(PROMPT_TEMPLATE.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            digest, PROMPT_TEMPLATE_SHA256,
            "PROMPT_TEMPLATE changed without updating PROMPT_TEMPLATE_SHA256 / PROMPT_VERSION",
        );
    }

    #[test]
    fn prompt_directs_the_model_to_its_pinned_workspace() {
        assert_eq!(PROMPT_VERSION, "review-guide-v6");
        assert!(PROMPT_TEMPLATE.contains("read-only checkout of the repository"));
        assert!(PROMPT_TEMPLATE.contains("git show {{BASE_SHA}}:<path>"));
        assert!(PROMPT_TEMPLATE.contains("git diff --no-ext-diff --no-textconv {{BASE_SHA}} {{HEAD_SHA}} -- <path>"));
        assert!(PROMPT_TEMPLATE.contains("Do not claim to have executed tests or performed independent validation."));
        // The old "supplied context only" framing predates the pinned
        // read-only workspace and must not resurface.
        assert!(!PROMPT_TEMPLATE.contains("Use only this supplied context."));
        assert!(
            !PROMPT_TEMPLATE
                .to_ascii_lowercase()
                .contains("checks you actually performed")
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
        assert!(rendered.contains(&format!("Merge-base revision: {}", "a".repeat(40))));
        assert!(rendered.contains(&format!("Head revision: {}", "c".repeat(40))));
        assert!(!rendered.contains("{{"));
        assert!(rendered.starts_with("I want you to provide me a guided summary"));
        assert!(rendered.ends_with("rather than inventing behavior."));
    }

    #[test]
    fn source_context_lists_changed_files_without_inlining_content() {
        let context = render_source_context(&packet());
        assert!(context.contains("src/retry.rs"));
        assert!(context.contains("+3/-1"));
        assert!(!context.contains("old();"), "before content must not be inlined");
        assert!(!context.contains("new();"), "after content must not be inlined");
        assert!(!context.contains("@@ -1,3 +1,3 @@"), "diff hunk must not be inlined");
    }

    #[test]
    fn source_context_manifest_lists_every_changed_file_without_content_and_stays_small() {
        let packet = multi_file_packet(20);
        let context = render_source_context(&packet);
        for i in 0..20 {
            let path = format!("src/file_{i}.rs");
            assert!(context.contains(&format!("`{path}`")), "manifest must list {path}");
            assert!(
                !context.contains(&format!("old_{i}();")),
                "before content for {path} must not be inlined"
            );
            assert!(
                !context.contains(&format!("new_{i}();")),
                "after content for {path} must not be inlined"
            );
        }
        // A manifest-only listing for 20 small files must stay a few hundred
        // bytes, nowhere near the multi-hundred-kilobyte prompts the removed
        // byte-budget machinery existed to cap.
        assert!(
            context.len() < 4_000,
            "manifest-only context for 20 files must stay small, was {} bytes",
            context.len()
        );
    }

    #[test]
    fn source_context_reports_capture_time_omissions() {
        let mut packet = packet();
        packet.omissions.push(boss_pr_review_sources::SourceOmission {
            path: Some("vendor/blob.bin".to_owned()),
            side: None,
            reason: "binary content not captured".to_owned(),
            terminal: true,
        });
        let context = render_source_context(&packet);
        assert!(context.contains("### Collection omissions"));
        assert!(context.contains("vendor/blob.bin"));
        assert!(context.contains("binary content not captured"));
    }

    #[test]
    fn source_context_omits_collection_omissions_section_when_none_recorded() {
        let context = render_source_context(&packet());
        assert!(!context.contains("### Collection omissions"));
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
    fn formatted_links_and_whole_file_permalinks_pass() {
        let href = format!("https://github.com/acme/widget/blob/{}/src/retry.rs", "c".repeat(40));
        for link in [
            format!("`{href}#L1`"),
            format!("**{href}#L1**"),
            format!("'{href}#L1';"),
            format!("{href}!"),
        ] {
            validate_guide_output_with_resolver(&valid_guide_body(&link), &packet(), |sha, path, start, end| {
                sha == "c".repeat(40) && path == "src/retry.rs" && ((start, end) == (1, 1) || (start, end) == (0, 0))
            })
            .unwrap();
        }
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
