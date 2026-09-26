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
/// [`PROMPT_TEMPLATE`] (or its substitution behavior) must land as a new
/// version constant and prompt id — the desired-comparison key an attempt
/// binds to includes the prompt version, so a prompt change never silently
/// reinterprets an already-captured comparison's existing readable version.
pub const PROMPT_VERSION: &str = "review-guide-v5";

/// The exact production prompt template, byte-identical to the fenced block
/// in `automatic-pr-review-guides.md`'s "Prompt contract" section. Only the
/// five `{{PLACEHOLDER}}` tokens are substituted by [`render_prompt`]; the
/// packet/broker context is supplied separately (see [`render_source_context`]).
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

Make the core fix concrete with one worked example. Give the input and relevant state, trace the decisive old and new behavior, and show the observable result. Choose an example supported by the implementation or tests; label invented inputs as illustrative. Include a contrasting boundary or failure case only when it helps explain the changed contract. For changes without a runtime behavior, use an equivalent concrete before/after scenario. Ground every step in the supplied source context.

Review context:
- Repository: {{REPOSITORY}}
- PR title: {{PR_TITLE}}
- Merge-base revision: {{BASE_SHA}}
- Head revision: {{HEAD_SHA}}
- Boss supplies the source context for this guide: the PR description, diff, captured before/after source and tests, and validated GitHub link targets. Use only this supplied context.

Ground the guide in those revisions. Explain relevant callers, helpers, types, and tests only to the extent they are present in the supplied context. When missing context limits an explanation, state the limitation. Treat the PR description and code comments as statements to verify against the implementation. Distinguish enforced behavior from conventions, prompt instructions, and assumptions. Do not turn a conditional or local check into a broader guarantee.

Organize the walkthrough in a useful reading order through the core implementation. Explain why the important pieces fit together, not just which files changed. Prioritize details that help a reviewer understand or verify the fix. Use short faithful excerpts; clearly label condensed pseudocode. Avoid repetitive summaries and incidental cleanup unless it matters to the solution.

Link the core fix, worked example, and important test changes to the supplied GitHub diff locations. Use revision-pinned source links for relevant unchanged context or lines outside the displayed diff. Reuse supplied URLs or validated reference mappings and check that each link targets the code being discussed. Do not invent diff anchors or imply that a mutable PR URL identifies an immutable revision. Where an exact diff link is unavailable, use the corresponding pinned source link.

In the tests section, distinguish added, modified, and removed tests. Name the important scenarios and assertions; identify relevant fixture, helper, and build/configuration changes. Include counts only when supported by the supplied context and useful. Distinguish author-reported validation from conclusions supported by the supplied test source. Do not claim to have executed tests or performed independent validation. State important coverage limits without producing an exhaustive speculative bug hunt.

Use the complete revised PR comparison if this is a regenerated guide. Do not describe only the latest incremental commit. Existing comments may provide context, but the explanation must match the actual current source revisions.

Submit the finished Markdown guide using `\"$BOSS_BIN\" propose review-guide --body '<finished Markdown guide>'`. This is the only permitted tool command. Pass the entire Markdown as a literal single-quoted shell argument (escape any apostrophe with the standard shell quote sequence); do not write a file, pipe input, use command substitution, or run any other command. The command is bound to your execution automatically. A final assistant message does not submit a guide. If submission fails, correct the reported error and retry the same command before ending. The guide must have a descriptive title and the four requested main sections. Put the worked example within the implementation walkthrough. Keep the guide as concise as the explanation permits while preserving useful reasoning and evidence. Do not include a chat preamble, model details, internal execution details, a merge recommendation, or an unsupported declaration that the PR is safe to merge. If essential context is absent from the supplied material, state the specific limitation rather than inventing behavior.";

/// SHA-256 of [`PROMPT_TEMPLATE`] (UTF-8, excluding any fence/terminal
/// newline) — matches the value recorded in the design doc, computed
/// independently from the doc's own fenced block as a second source of
/// truth. See `prompt_template_hash_is_pinned`.
pub const PROMPT_TEMPLATE_SHA256: &str = "cc4bddde22f1162c21484ff22aca69810215066a0eb0f27a281b25411cfa9911";

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
/// Packet/broker context is not part of this string — callers append
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

/// Total byte budget for [`render_source_context`]'s rendered output (the
/// whole embedded diff/before/after/omissions block, not the whole prompt).
///
/// Derivation: the review-guide worker runs on `gpt-6-astra` (see
/// `resolve_review_guide_spawn_config` in `engine/core`'s `worker_spawn`),
/// whose driver reports a 258,400-token context window. Reserving ~40,000
/// tokens for the fixed prompt template, PR title/body, and the model's own
/// output leaves ~218,400 tokens for source context. Diff- and code-heavy
/// text tokenizes less efficiently than English prose, so this uses a
/// conservative 3 bytes/token (rather than the ~4 typical for prose),
/// giving a derived budget of 218,400 * 3 = 655,200 bytes. Rounded down to a
/// clean, clearly-conservative constant.
pub const MAX_TOTAL_SOURCE_CONTEXT_BYTES: usize = 600_000;

/// [`render_source_context`] refused to render: even the mandatory part of
/// the context (the comparison header, PR description, every changed file's
/// diff hunk, and a named omission note for every before/after side that
/// might not fit as full content — never omitted, since a reviewer without
/// diffs has nothing to ground a guide in, and a dropped side with no
/// record would hide that a gap exists) already exceeds the total budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceContextBudgetExceeded {
    pub required_bytes: usize,
    pub budget_bytes: usize,
}

impl fmt::Display for SourceContextBudgetExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "review-guide source context's mandatory diffs and omission notes need {} bytes, exceeding the {}-byte total budget",
            self.required_bytes, self.budget_bytes
        )
    }
}

impl std::error::Error for SourceContextBudgetExceeded {}

/// Render every changed file's diff hunk, and as much of its pinned
/// before/after content as fits [`MAX_TOTAL_SOURCE_CONTEXT_BYTES`], into
/// read-only Markdown context.
///
/// The inline packet remains available alongside the pinned read-only workspace.
/// The rendering is
/// revision-aware by construction: it reads only the packet's already-pinned
/// `before`/`after` content, never a live checkout or a moving branch.
///
/// Every changed file's diff hunk is always included in full — it is never
/// cut to make room, and content is never truncated mid-file. Full
/// before/after file content is included only while budget remains; a side
/// that does not fit is instead listed as a named omission in the same
/// "Collection omissions" section used for capture-time gaps, so the guide
/// and its reader both know what was not seen. Those fallback omission
/// lines are reserved up front alongside the diffs: a gap is never dropped
/// with no record. If the mandatory diffs plus those reserved notes exceed
/// the budget, this returns [`SourceContextBudgetExceeded`] instead of
/// silently sending a truncated prompt.
pub fn render_source_context(packet: &SourcePacket) -> Result<String, SourceContextBudgetExceeded> {
    render_source_context_with_budget(packet, MAX_TOTAL_SOURCE_CONTEXT_BYTES)
}

fn render_source_context_with_budget(
    packet: &SourcePacket,
    budget_bytes: usize,
) -> Result<String, SourceContextBudgetExceeded> {
    let mut header = String::new();
    header.push_str("## Source context\n\n");
    header.push_str(&format!(
        "Comparison: `{}` base `{}` (merge base `{}`) to head `{}`.\n\n",
        packet.canonical_pr_url, packet.observed_base_sha, packet.merge_base_sha, packet.head_sha
    ));
    if let Some(body) = &packet.body
        && !body.is_empty()
    {
        header.push_str("### PR description\n\n");
        header.push_str(body);
        header.push_str("\n\n");
    }

    let mandatory_sections: Vec<String> = packet.files.iter().map(render_mandatory_file_section).collect();

    // The "Collection omissions" section is itself part of the rendered
    // budget (see `MAX_TOTAL_SOURCE_CONTEXT_BYTES`'s doc comment): its
    // header/footer, every capture-time omission the packet already carries,
    // and a named fallback line for every pinned before/after side that
    // might not fit as full content, are reserved as mandatory up front
    // alongside the diffs. Remaining budget is then only spent on full
    // before/after blocks; a side that does not fit reuses its already-
    // reserved line rather than competing with later files for leftover
    // bytes. Without that reservation, remaining_bytes can shrink below
    // one omission line and stay there, so every subsequent dropped side
    // would vanish with no record — including from this section.
    let static_omission_lines: Vec<String> = packet.omissions.iter().map(render_omission_line).collect();
    let has_static_omissions = !static_omission_lines.is_empty();

    struct OptionalSide {
        block: String,
        omission: boss_pr_review_sources::SourceOmission,
        omission_line: String,
    }
    let optional_by_file: Vec<Vec<OptionalSide>> = packet
        .files
        .iter()
        .map(|file| {
            [
                ("Before (merge base)", SourceSide::Before, file.before.as_ref()),
                ("After (head)", SourceSide::After, file.after.as_ref()),
            ]
            .into_iter()
            .filter_map(|(label, side, source)| {
                let block = render_pinned_side_block(label, source)?;
                let omission = budget_driven_omission(&file.path, side, budget_bytes, block.len());
                let omission_line = render_omission_line(&omission);
                Some(OptionalSide {
                    block,
                    omission,
                    omission_line,
                })
            })
            .collect()
        })
        .collect();
    let has_optional_content = optional_by_file.iter().any(|sides| !sides.is_empty());
    let needs_omissions_capacity = has_static_omissions || has_optional_content;
    let omissions_overhead = OMISSIONS_HEADER.len() + OMISSIONS_FOOTER.len();
    let potential_omission_bytes: usize = optional_by_file
        .iter()
        .flatten()
        .map(|side| side.omission_line.len())
        .sum();

    let mut required_bytes = header.len() + mandatory_sections.iter().map(String::len).sum::<usize>();
    if needs_omissions_capacity {
        required_bytes += omissions_overhead
            + static_omission_lines.iter().map(String::len).sum::<usize>()
            + potential_omission_bytes;
    }
    if required_bytes > budget_bytes {
        return Err(SourceContextBudgetExceeded {
            required_bytes,
            budget_bytes,
        });
    }

    let mut out = header;
    let mut remaining_bytes = budget_bytes - required_bytes;
    let mut budget_omissions = Vec::new();
    for (section, sides) in mandatory_sections.into_iter().zip(optional_by_file) {
        out.push_str(&section);
        for side in sides {
            if side.block.len() <= remaining_bytes {
                // Include the full block and give back this side's unused
                // omission reservation so later files can still spend it on
                // content. Later sides keep their own reserved lines, so a
                // subsequent gap is still named.
                remaining_bytes -= side.block.len();
                remaining_bytes += side.omission_line.len();
                out.push_str(&side.block);
                continue;
            }
            budget_omissions.push(side.omission);
        }
    }

    if needs_omissions_capacity && (has_static_omissions || !budget_omissions.is_empty()) {
        out.push_str(OMISSIONS_HEADER);
        for line in &static_omission_lines {
            out.push_str(line);
        }
        for omission in &budget_omissions {
            out.push_str(&render_omission_line(omission));
        }
        out.push_str(OMISSIONS_FOOTER);
    }
    Ok(out)
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

fn budget_driven_omission(
    path: &str,
    side: SourceSide,
    budget_bytes: usize,
    block_bytes: usize,
) -> boss_pr_review_sources::SourceOmission {
    boss_pr_review_sources::SourceOmission {
        path: Some(path.to_owned()),
        side: Some(side),
        reason: format!(
            "omitted to stay within the {budget_bytes}-byte total source-context budget \
             ({block_bytes} bytes needed)"
        ),
        terminal: true,
    }
}

/// The part of a file's rendered section that is never subject to the
/// budget: its heading, rename note, and diff hunk.
fn render_mandatory_file_section(file: &boss_pr_review_sources::SourceFile) -> String {
    let mut section = String::new();
    section.push_str(&format!("### `{}` ({:?})\n\n", file.path, file.change_kind));
    if let Some(previous) = &file.previous_path {
        section.push_str(&format!("Renamed from `{previous}`.\n\n"));
    }
    if let Some(patch) = &file.patch {
        section.push_str("Diff hunk:\n\n```diff\n");
        section.push_str(patch);
        section.push_str("\n```\n\n");
    }
    section
}

fn render_pinned_side_block(label: &str, source: Option<&boss_pr_review_sources::PinnedSource>) -> Option<String> {
    let source = source?;
    let mut block = String::new();
    block.push_str(&format!("**{label}** (`{}` @ `{}`)", source.path, source.sha));
    match (&source.content, &source.omission) {
        (Some(content), _) => {
            block.push_str(":\n\n```\n");
            block.push_str(content);
            if !content.ends_with('\n') {
                block.push('\n');
            }
            block.push_str("```\n\n");
        }
        (None, Some(reason)) => {
            block.push_str(&format!(" — omitted: {reason}\n\n"));
        }
        (None, None) => block.push_str(" — omitted: no content captured\n\n"),
    }
    Some(block)
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
/// commits, including files absent from the inline packet.
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
    let link_re = regex::Regex::new(r"(https://github\.com/[^\s<>\)\]]+)").expect("static regex");
    let mut hrefs: Vec<String> = link_re
        .captures_iter(text)
        .map(|caps| caps[1].trim_end_matches(['.', ',', ')']).to_owned())
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
                && start > 0
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
    /// distinct before/after content, so budget apportionment across files
    /// (not just within one file) can be exercised.
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
    fn prompt_uses_only_supplied_context_without_inviting_exploration() {
        assert_eq!(PROMPT_VERSION, "review-guide-v5");
        assert!(PROMPT_TEMPLATE.contains("Use only this supplied context."));
        assert!(PROMPT_TEMPLATE.contains("Do not claim to have executed tests or performed independent validation."));
        // Submission instructions may name a tool; source exploration may not.
        let prompt = PROMPT_TEMPLATE.to_ascii_lowercase();
        for invitation in [
            "read tools",
            "available tools",
            "inspect",
            "read files",
            "open files",
            "checks you actually performed",
            "context cannot be obtained",
        ] {
            assert!(
                !prompt.contains(invitation),
                "prompt invites external action: {invitation}"
            );
        }
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
    fn source_context_embeds_pinned_content_and_diff() {
        let context = render_source_context(&packet()).expect("fits the default budget");
        assert!(context.contains("src/retry.rs"));
        assert!(context.contains("old();"));
        assert!(context.contains("new();"));
        assert!(context.contains("@@ -1,3 +1,3 @@"));
    }

    #[test]
    fn budget_omits_full_content_but_keeps_diffs_and_names_the_omission() {
        let packet = packet();
        let file = &packet.files[0];
        let before_block =
            render_pinned_side_block("Before (merge base)", file.before.as_ref()).expect("fixture has before content");
        let after_block =
            render_pinned_side_block("After (head)", file.after.as_ref()).expect("fixture has after content");

        // Mandatory reservation now includes both sides' named omission
        // notes. Adding one byte less than the smaller content block leaves
        // room for neither full side, so both fall back to those notes.
        let mandatory_bytes = required_bytes_for(&packet);
        let budget = mandatory_bytes + before_block.len().min(after_block.len()) - 1;

        let context =
            render_source_context_with_budget(&packet, budget).expect("mandatory diff alone fits this budget");
        assert!(context.contains("@@ -1,3 +1,3 @@"), "diff hunk must never be omitted");
        assert!(!context.contains("old();"), "before content should not fit the budget");
        assert!(!context.contains("new();"), "after content should not fit the budget");
        assert!(context.contains("### Collection omissions"));
        let omissions = collection_omissions_section(&context);
        assert!(omissions.contains("- `src/retry.rs` (Before):"));
        assert!(omissions.contains("- `src/retry.rs` (After):"));
        assert!(omissions.contains("total source-context budget"));
        assert!(
            context.len() <= budget,
            "rendered context ({}) must stay within budget ({budget})",
            context.len()
        );
    }

    #[test]
    fn diffs_alone_over_budget_fails_loudly() {
        let err = render_source_context_with_budget(&packet(), 10).unwrap_err();
        assert!(err.required_bytes > 10);
        assert_eq!(err.budget_bytes, 10);
        assert!(err.to_string().contains("exceeding the 10-byte total budget"));
        assert!(err.to_string().contains("mandatory diffs and omission notes"));
    }

    #[test]
    fn diffs_plus_reserved_omission_notes_over_budget_fails_loudly() {
        let packet = multi_file_packet(20);
        let required = required_bytes_for(&packet);
        let err = render_source_context_with_budget(&packet, required - 1).unwrap_err();
        assert!(err.required_bytes > required - 1);
        assert_eq!(err.budget_bytes, required - 1);
    }

    fn collection_omissions_section(context: &str) -> &str {
        let idx = context
            .find("### Collection omissions")
            .expect("Collection omissions section must be present");
        &context[idx..]
    }

    /// Smallest budget at which diffs plus reserved omission notes fit.
    /// Omission-line length depends on the digit count of `budget_bytes`, so
    /// a probe at 1 is not the size that will actually be required at that
    /// size; this iterates until the two agree.
    fn required_bytes_for(packet: &SourcePacket) -> usize {
        let mut probe = 1usize;
        for _ in 0..16 {
            match render_source_context_with_budget(packet, probe) {
                Err(err) => {
                    assert!(
                        err.required_bytes > probe,
                        "budget {probe} failed with a non-larger required_bytes {}",
                        err.required_bytes
                    );
                    probe = err.required_bytes;
                }
                Ok(_) => return probe,
            }
        }
        panic!("required_bytes did not converge");
    }

    #[test]
    fn budget_holds_across_many_omitted_files() {
        let packet = multi_file_packet(20);
        let mandatory_bytes = required_bytes_for(&packet);

        let file0 = &packet.files[0];
        let before0 =
            render_pinned_side_block("Before (merge base)", file0.before.as_ref()).expect("fixture has before");
        let after0 = render_pinned_side_block("After (head)", file0.after.as_ref()).expect("fixture has after");

        // Enough for the first file's full before/after content on top of
        // the reserved diffs, omissions header, and a named omission line
        // for every side. Later files may pick up a little extra from the
        // first file's reclaimed reservation; the last file still cannot
        // fit as full content.
        let budget = mandatory_bytes + before0.len() + after0.len();

        let context =
            render_source_context_with_budget(&packet, budget).expect("mandatory diffs alone fit this budget");
        assert!(
            context.len() <= budget,
            "rendered context ({}) must stay within budget ({budget}) even with many omissions",
            context.len()
        );
        assert!(context.contains("### Collection omissions"));
        let content_part = &context[..context.find("### Collection omissions").unwrap()];
        let omissions = collection_omissions_section(&context);
        let mut dropped_blocks = 0usize;
        for i in 0..20 {
            let path = format!("src/file_{i}.rs");
            assert!(
                content_part.contains(&format!("### `{path}`")),
                "every file's diff heading must always be present, including {path}"
            );
            let before_kept = content_part.contains(&format!("old_{i}();"));
            let after_kept = content_part.contains(&format!("new_{i}();"));
            for (kept, side) in [(before_kept, "Before"), (after_kept, "After")] {
                let named = format!("- `{path}` ({side}):");
                if kept {
                    assert!(
                        !omissions.contains(&named),
                        "kept {side} side of {path} must not be listed as omitted"
                    );
                } else {
                    assert!(
                        omissions.contains(&named),
                        "dropped {side} side of {path} must be named in Collection omissions"
                    );
                    dropped_blocks += 1;
                }
            }
        }
        assert!(dropped_blocks > 0, "budget must actually force some omissions");
        assert_eq!(
            omissions.matches("- `src/file_").count(),
            dropped_blocks,
            "every dropped before/after block must appear as exactly one omission line"
        );
        // Order-dependent apportionment: budget is spent on earlier files
        // first, so the very first file should keep its full content while
        // a later one is exhausted into an omission instead.
        assert!(
            context.contains("old_0();") && context.contains("new_0();"),
            "first file should retain full content before budget runs out"
        );
        assert!(
            !content_part.contains("old_19();") && !content_part.contains("new_19();"),
            "last file's full content must be omitted once budget runs out"
        );
    }

    #[test]
    fn budget_equal_to_mandatory_names_every_omitted_side() {
        let packet = multi_file_packet(20);
        let mandatory_bytes = required_bytes_for(&packet);
        let context = render_source_context_with_budget(&packet, mandatory_bytes)
            .expect("diffs plus reserved omission notes must fit the mandatory size");
        assert!(
            context.len() <= mandatory_bytes,
            "rendered context ({}) must stay within the mandatory reservation ({mandatory_bytes})",
            context.len()
        );
        let content_part = &context[..context.find("### Collection omissions").unwrap()];
        let omissions = collection_omissions_section(&context);
        for i in 0..20 {
            assert!(
                !content_part.contains(&format!("old_{i}();")) && !content_part.contains(&format!("new_{i}();")),
                "no full before/after content should fit at the mandatory reservation"
            );
            let path = format!("src/file_{i}.rs");
            assert!(omissions.contains(&format!("- `{path}` (Before):")));
            assert!(omissions.contains(&format!("- `{path}` (After):")));
        }
        assert_eq!(omissions.matches("- `src/file_").count(), 40);
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
