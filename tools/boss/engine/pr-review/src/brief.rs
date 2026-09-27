//! Work-item brief + design-doc section packet for PR review.
//!
//! This module is the typed review-input packet and the pure helpers that
//! locate a design-doc section (planner breakdown entries, then ATX
//! headings). The engine assembles a packet at spawn time (live GitHub
//! fetch of the design doc; no mirrored copy) and the prompt renderers
//! embed it.

/// One required review input the engine could not resolve for a work-item PR.
///
/// [`Self::Brief`] is a real brief gap the reviewer must raise a blocking
/// `deferred_scope`/`high` finding for. [`Self::DesignSection`] is an
/// engine-side fetch/resolution failure the PR worker cannot fix by changing
/// the PR; the prompt tells the reviewer about it and an operator attention
/// item is filed, but it must not force a revision loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnresolvedReviewInput {
    /// The work item has no usable (human-authored) description.
    Brief { reason: String },
    /// The work item belongs to a project with a design-doc pointer, but the
    /// section (or the doc itself) could not be fetched or resolved.
    DesignSection { reason: String },
}

impl UnresolvedReviewInput {
    fn prompt_line(&self) -> String {
        match self {
            Self::Brief { reason } => format!("- **Work-item brief:** {reason}"),
            Self::DesignSection { reason } => format!("- **Design doc section:** {reason}"),
        }
    }
}

/// The design-doc excerpt a reviewer should check the PR against.
///
/// Located by matching the work-item name against planner breakdown entries
/// (ATX `###`, numbered list items, or `- **Name.**` bullets) first, then
/// against markdown headings (case-insensitive, punctuation-stripped). When
/// neither matches, [`Self::heading`] is `None` and [`Self::body`] is the
/// whole document — that fallback is success, not an unresolved input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesignDocSection {
    /// Repo-relative path of the design doc (e.g. `tools/boss/docs/designs/foo.md`).
    pub path: String,
    /// Matching heading text, or `None` when the whole doc is the fallback.
    pub heading: Option<String>,
    /// Section body (heading line through the next same-or-higher heading),
    /// or the full document when no heading matched.
    pub body: String,
}

impl DesignDocSection {
    pub fn is_whole_doc_fallback(&self) -> bool {
        self.heading.is_none()
    }
}

/// Engine-assembled review inputs for a work-item PR.
///
/// For an ordinary (non-revision) item, [`Self::work_item_brief`] is the
/// item's description. For a revision, [`Self::revision_ask`] is the
/// revision's description and [`Self::work_item_brief`] is the chain-root
/// brief — the reviewer checks both.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewBriefPacket {
    pub task_name: String,
    pub work_item_brief: Option<String>,
    pub revision_ask: Option<String>,
    pub design_section: Option<DesignDocSection>,
    /// Engine-recorded `[deferred-scope]` declarations (proposal summaries
    /// and audit-line markers) for this work item / chain. A deliverable
    /// covered by one of these is **declared deferred**, not missing.
    #[builder(default)]
    pub deferred_scope_declarations: Vec<String>,
    #[builder(default)]
    pub unresolved: Vec<UnresolvedReviewInput>,
}

impl ReviewBriefPacket {
    /// Build a packet from a task name + description. An empty description
    /// is recorded as an unresolved brief so the conformance check still
    /// fires. Test-only: production packets are assembled by the engine.
    #[cfg(test)]
    pub fn from_task(name: impl Into<String>, description: impl Into<String>) -> Self {
        let task_name = name.into();
        let description = description.into();
        if description.trim().is_empty() {
            Self {
                task_name,
                work_item_brief: None,
                revision_ask: None,
                design_section: None,
                deferred_scope_declarations: Vec::new(),
                unresolved: vec![UnresolvedReviewInput::Brief {
                    reason: "work item description is empty".to_owned(),
                }],
            }
        } else {
            Self {
                task_name,
                work_item_brief: Some(description),
                revision_ask: None,
                design_section: None,
                deferred_scope_declarations: Vec::new(),
                unresolved: Vec::new(),
            }
        }
    }
}

/// One planner-parsed breakdown entry (`###` heading, numbered list item, or
/// `- **Name.**` bullet) offered to [`locate_design_section_with_breakdown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakdownSection<'a> {
    pub title: &'a str,
    pub body: &'a str,
}

/// Locate the design-doc section that a work item derives from.
///
/// Match strategy (first hit wins, highest score):
/// 1. A markdown heading whose normalized text equals the work-item name.
/// 2. A heading whose normalized text contains the name, or whose name
///    contains the heading (heading must be at least three words, so a
///    generic `## Goals` does not swallow every task).
///
/// Normalization lowercases and strips punctuation. The extracted body runs
/// from the matching heading through the next heading of the same or higher
/// level. No match → the whole document, with `heading: None`.
///
/// Callers that have planner breakdown entries should use
/// [`locate_design_section_with_breakdown`] so numbered/bullet entries match
/// before this heading scan falls back to the whole doc.
pub fn locate_design_section(doc: &str, task_name: &str, path: &str) -> DesignDocSection {
    locate_design_section_with_breakdown(doc, task_name, path, &[])
}

/// Same as [`locate_design_section`], but planner breakdown entries are
/// scored first with the same normalization. A matching entry's title+body
/// is returned; the heading scan runs only when no entry matches.
pub fn locate_design_section_with_breakdown(
    doc: &str,
    task_name: &str,
    path: &str,
    breakdown: &[BreakdownSection<'_>],
) -> DesignDocSection {
    let needle = normalize_text(task_name);
    if needle.is_empty() {
        return whole_doc(doc, path);
    }
    if let Some(section) = best_breakdown_match(&needle, path, breakdown) {
        return section;
    }
    let headings = parse_headings(doc);
    let mut best: Option<(u8, usize)> = None;
    for (i, heading) in headings.iter().enumerate() {
        let score = title_match_score(&heading.title, &needle);
        if score > 0 && best.is_none_or(|(s, _)| score > s) {
            best = Some((score, i));
        }
    }
    match best {
        Some((_, i)) => {
            let heading = &headings[i];
            let end = headings[i + 1..]
                .iter()
                .find(|next| next.level <= heading.level)
                .map(|next| next.start)
                .unwrap_or(doc.len());
            DesignDocSection {
                path: path.to_owned(),
                heading: Some(heading.title.clone()),
                body: doc[heading.start..end].trim().to_owned(),
            }
        }
        None => whole_doc(doc, path),
    }
}

fn best_breakdown_match(needle: &str, path: &str, breakdown: &[BreakdownSection<'_>]) -> Option<DesignDocSection> {
    let mut best: Option<(u8, usize)> = None;
    for (i, entry) in breakdown.iter().enumerate() {
        let score = title_match_score(entry.title, needle);
        if score > 0 && best.is_none_or(|(s, _)| score > s) {
            best = Some((score, i));
        }
    }
    best.map(|(_, i)| {
        let entry = &breakdown[i];
        let mut body = format!("### {}\n", entry.title);
        if !entry.body.trim().is_empty() {
            body.push('\n');
            body.push_str(entry.body.trim());
        }
        DesignDocSection {
            path: path.to_owned(),
            heading: Some(entry.title.to_owned()),
            body,
        }
    })
}

fn title_match_score(title: &str, needle: &str) -> u8 {
    let title = normalize_text(title);
    if title.is_empty() {
        0
    } else if title == needle {
        3
    } else if title.contains(needle) {
        2
    } else if needle.contains(&title) && title.split_whitespace().count() >= 3 {
        1
    } else {
        0
    }
}

fn whole_doc(doc: &str, path: &str) -> DesignDocSection {
    DesignDocSection {
        path: path.to_owned(),
        heading: None,
        body: doc.to_owned(),
    }
}

// Heading tokenization (fence-aware, CommonMark ATX rules) is shared with
// `boss-pr-template`'s required-heading extraction and the planner's `###`
// breakdown parser — see `boss_pr_template::parse_all_headings` — so a fix
// to fence or indentation handling never has to be made in two parsers that
// can drift.
use boss_pr_template::{HeadingToken, parse_all_headings};

fn parse_headings(doc: &str) -> Vec<HeadingToken> {
    parse_all_headings(doc)
}

fn normalize_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Prompt block embedding the packet: task, briefs, design section, declared
/// deferrals, and a loud unresolved-input finding requirement when needed.
pub fn render_brief_packet_block(packet: &ReviewBriefPacket) -> String {
    let mut out = format!("**Task:** {}\n\n", packet.task_name);

    match packet.work_item_brief.as_deref() {
        Some(brief) if packet.revision_ask.is_some() => {
            out.push_str("**Chain-root brief** (the originating work item this revision is for):\n");
            out.push_str(brief);
            out.push_str("\n\n");
        }
        Some(brief) => {
            out.push_str("**Task description:**\n");
            out.push_str(brief);
            out.push_str("\n\n");
        }
        None => {
            out.push_str("**Task description:** *(unresolved — see Unresolved review inputs)*\n\n");
        }
    }

    if let Some(ask) = packet.revision_ask.as_deref() {
        out.push_str("**Revision ask** (this pass's revision instructions; check these AND the chain-root brief):\n");
        out.push_str(ask);
        out.push_str("\n\n");
    }

    match packet.design_section.as_ref() {
        Some(section) if section.is_whole_doc_fallback() => {
            out.push_str(&format!(
                "**Design doc** (`{}`; no heading matched the work-item name, so the whole doc is included):\n\n",
                section.path
            ));
            out.push_str(&section.body);
            out.push_str("\n\n");
        }
        Some(section) => {
            let heading = section.heading.as_deref().unwrap_or("");
            out.push_str(&format!(
                "**Design doc section** (`{}`, heading `{heading}`):\n\n",
                section.path
            ));
            out.push_str(&section.body);
            out.push_str("\n\n");
        }
        None if packet
            .unresolved
            .iter()
            .any(|u| matches!(u, UnresolvedReviewInput::DesignSection { .. })) =>
        {
            out.push_str("**Design doc section:** *(unresolved — see Engine-side design-doc input)*\n\n");
        }
        None => {
            out.push_str(
                "**Design doc section:** none — this work item has no project design-doc pointer, so \
                 there is no design section to check.\n\n",
            );
        }
    }

    if packet.deferred_scope_declarations.is_empty() {
        out.push_str("**Declared deferred scope:** none recorded by the engine.\n\n");
    } else {
        out.push_str(
            "**Declared deferred scope** (engine-recorded `[deferred-scope]` proposals / audit lines; \
             these are the only deferrals that count — PR-body prose alone does not):\n",
        );
        for line in &packet.deferred_scope_declarations {
            out.push_str(&format!("- {line}\n"));
        }
        out.push('\n');
    }

    let brief_gaps: Vec<_> = packet
        .unresolved
        .iter()
        .filter(|item| matches!(item, UnresolvedReviewInput::Brief { .. }))
        .collect();
    let design_gaps: Vec<_> = packet
        .unresolved
        .iter()
        .filter(|item| matches!(item, UnresolvedReviewInput::DesignSection { .. }))
        .collect();

    if !brief_gaps.is_empty() {
        out.push_str("## Unresolved review inputs — CRITICAL\n\n");
        out.push_str(
            "The engine could not resolve the following required inputs for this work-item PR. \
             This is a **blocking finding**, same class as a correctness bug: raise a \
             `deferred_scope` finding (`severity: high`) for EACH item below. Do **not** skip \
             the brief-conformance check because an input is missing — that silent skip is \
             how a substituted deliverable ships.\n\n",
        );
        for item in brief_gaps {
            out.push_str(&item.prompt_line());
            out.push('\n');
        }
        out.push('\n');
    }

    if !design_gaps.is_empty() {
        out.push_str("## Engine-side design-doc input — operator attention filed\n\n");
        out.push_str(
            "The engine could not fetch or resolve the design-doc section for this work item. \
             An operator attention item has been filed so the broken pointer, missing file, or \
             auth/rate-limit problem can be fixed. Do **not** raise a `deferred_scope` finding \
             for this gap — a revision cannot fix an engine-side fetch. Continue the \
             brief-conformance check against the work-item brief (and revision ask, if any).\n\n",
        );
        for item in design_gaps {
            out.push_str(&item.prompt_line());
            out.push('\n');
        }
        out.push('\n');
    }

    out
}

/// First-class brief-conformance rubric. Rendered for every review scope
/// (code and docs-only): a docs PR can still omit a brief-named deliverable.
pub fn render_brief_conformance_rubric() -> String {
    "## Brief conformance — CRITICAL (must not skip)\n\
     \n\
     This check is first-class and blocking, the same class as a correctness \
     bug. It is how we catch a PR that silently substitutes a required \
     deliverable (the review-guide \"revision-aware broker\" incident: the \
     brief required a broker; the PR inlined every source file into the \
     prompt instead; six reviews missed it; the feature shipped broken).\n\
     \n\
     Inputs are engine-supplied in **PR under review** above. Do not skip \
     this check if an input is missing — raise a finding for the gap \
     (see **Unresolved review inputs**).\n\
     \n\
     Procedure:\n\
     \n\
     1. **Enumerate** each deliverable the brief names: required behaviour, \
        tests, named components, explicit \"must\" items. If a **Revision \
        ask** is present, enumerate deliverables from BOTH the revision ask \
        AND the chain-root brief.\n\
     2. For each deliverable, classify it as exactly one of:\n\
        - **delivered** — cite the file/hunk/function in the **diff** (not \
          the PR body) that implements it.\n\
        - **declared deferred** — an engine-recorded `[deferred-scope]` \
          proposal or audit line in **Declared deferred scope** covers it, \
          or the PR body explicitly states the deferral AND a matching \
          engine-recorded declaration exists. PR-body claims alone do not \
          count — **except** the same manual/interactive-verification \
          carve-out as the Deferred-scope hygiene check below: a deliverable \
          that is itself manual, interactive, or display-requiring \
          verification a headless worker cannot perform (live GUI runs, \
          screenshot-based checks, physical-device tests) needs no \
          engine-recorded marker; a plain prose note is enough. That carve-out \
          is narrow — infeasibility for a headless agent, not the word \
          \"testing\" in general — and applies only to the deliverable being \
          deferred, never to the deliverable being silently dropped.\n\
        - **missing / silently substituted** — the diff does not deliver it, \
          and no declared deferral covers it. Replacing the asked-for \
          approach with a different one without saying so (e.g. inlining \
          sources instead of a broker) is a silent substitution.\n\
     3. Every missing / silently substituted deliverable is a blocking \
        finding: `category: \"deferred_scope\"`, `severity: \"high\"`. It \
        flows into the existing findings → revision path. Do not mark it \
        advisory or low-confidence to avoid a revision.\n\
     4. Do **not** trust the PR body as evidence of delivery. Read the diff.\n\
     \n"
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passes_severity_gate;
    use crate::types::{
        ReviewFinding, ReviewFindingCategory, ReviewFindingConfidence, ReviewFindingSeverity, ReviewResult,
    };

    #[test]
    fn packet_from_task_embeds_brief_and_marks_empty_as_unresolved() {
        let packet = ReviewBriefPacket::from_task("Add broker", "Build a revision-aware broker.");
        assert_eq!(
            packet.work_item_brief.as_deref(),
            Some("Build a revision-aware broker.")
        );
        assert!(packet.unresolved.is_empty());

        let empty = ReviewBriefPacket::from_task("Add broker", "   ");
        assert!(empty.work_item_brief.is_none());
        assert!(matches!(
            empty.unresolved.as_slice(),
            [UnresolvedReviewInput::Brief { .. }]
        ));
    }

    #[test]
    fn review_input_packet_includes_brief_and_design_section() {
        let packet = ReviewBriefPacket {
            task_name: "Run durable Astra-high guide jobs".to_owned(),
            work_item_brief: Some(
                "Implement a revision-aware broker that fetches callers and tests on demand.".to_owned(),
            ),
            revision_ask: None,
            design_section: Some(DesignDocSection {
                path: "tools/boss/docs/designs/automatic-pr-review-guides.md".to_owned(),
                heading: Some("Run durable Astra-high guide jobs".to_owned()),
                body: "### Run durable Astra-high guide jobs\n\nA revision-aware broker \
                       fetches callers and tests rather than inlining every source file."
                    .to_owned(),
            }),
            deferred_scope_declarations: Vec::new(),
            unresolved: Vec::new(),
        };
        let block = render_brief_packet_block(&packet);
        assert!(
            block.contains("revision-aware broker that fetches callers and tests"),
            "packet must embed the work-item brief:\n{block}"
        );
        assert!(
            block.contains("tools/boss/docs/designs/automatic-pr-review-guides.md"),
            "packet must name the design doc path:\n{block}"
        );
        assert!(
            block.contains("fetches callers and tests rather than inlining"),
            "packet must embed the design section body:\n{block}"
        );
        assert!(block.contains("**Design doc section**"));
        assert!(!block.contains("Unresolved review inputs"));
    }

    #[test]
    fn revision_packet_includes_revision_ask_and_chain_root_brief() {
        let packet = ReviewBriefPacket {
            task_name: "Address review findings".to_owned(),
            work_item_brief: Some("Implement a revision-aware broker.".to_owned()),
            revision_ask: Some("Restore the broker the first pass inlined away.".to_owned()),
            design_section: None,
            deferred_scope_declarations: Vec::new(),
            unresolved: Vec::new(),
        };
        let block = render_brief_packet_block(&packet);
        assert!(block.contains("Chain-root brief"));
        assert!(block.contains("Implement a revision-aware broker."));
        assert!(block.contains("Revision ask"));
        assert!(block.contains("Restore the broker"));
    }

    #[test]
    fn unresolved_inputs_render_as_required_blocking_finding() {
        let packet = ReviewBriefPacket {
            task_name: "Add broker".to_owned(),
            work_item_brief: None,
            revision_ask: None,
            design_section: None,
            deferred_scope_declarations: Vec::new(),
            unresolved: vec![
                UnresolvedReviewInput::Brief {
                    reason: "work item description is empty".to_owned(),
                },
                UnresolvedReviewInput::DesignSection {
                    reason: "fetch of tools/boss/docs/designs/foo.md failed: 404".to_owned(),
                },
            ],
        };
        let block = render_brief_packet_block(&packet);
        assert!(block.contains("Unresolved review inputs — CRITICAL"));
        assert!(block.contains("blocking finding"));
        assert!(block.contains("Work-item brief"));
        assert!(block.contains("Do **not** skip"));
        assert!(block.contains("Engine-side design-doc input — operator attention filed"));
        assert!(block.contains("Design doc section"));
        assert!(
            block.contains("Do **not** raise a `deferred_scope` finding"),
            "engine-side design-doc gaps must not force a revision: {block}"
        );
    }

    #[test]
    fn locate_design_section_extracts_matching_heading() {
        let doc = "# Design\n\nIntro.\n\n## Goals\n\nShip it.\n\n\
                   ## Run durable Astra-high guide jobs\n\n\
                   A revision-aware broker fetches callers.\n\n\
                   ## Follow-up chores\n\nLater.\n";
        let section = locate_design_section(doc, "Run durable Astra-high guide jobs", "docs/designs/guides.md");
        assert_eq!(section.heading.as_deref(), Some("Run durable Astra-high guide jobs"));
        assert!(section.body.contains("revision-aware broker"));
        assert!(!section.body.contains("Follow-up chores"));
        assert!(!section.body.contains("## Goals"));
        assert_eq!(section.path, "docs/designs/guides.md");
    }

    /// A `#`-prefixed line inside a fenced code block (a Rust `#[derive]`
    /// attribute, or a shell `#` comment) must never be mistaken for a
    /// markdown heading — the shared `boss_pr_template::parse_all_headings`
    /// tokenizer tracks fences, so the matched section runs all the way to
    /// the next REAL heading instead of being cut short at the fake one.
    #[test]
    fn locate_design_section_ignores_pseudo_headings_inside_fences() {
        let doc = "# Design\n\n\
                   ## Run durable Astra-high guide jobs\n\n\
                   A revision-aware broker fetches callers.\n\n\
                   ```rust\n\
                   #[derive(Debug)]\n\
                   pub struct Broker;\n\
                   ```\n\n\
                   More prose after the fence.\n\n\
                   ```sh\n\
                   # shell comment, not a heading\n\
                   echo hi\n\
                   ```\n\n\
                   Still inside the same section.\n\n\
                   ## Follow-up chores\n\nLater.\n";
        let section = locate_design_section(doc, "Run durable Astra-high guide jobs", "docs/designs/guides.md");
        assert_eq!(section.heading.as_deref(), Some("Run durable Astra-high guide jobs"));
        assert!(section.body.contains("#[derive(Debug)]"));
        assert!(section.body.contains("# shell comment, not a heading"));
        assert!(section.body.contains("Still inside the same section."));
        assert!(
            !section.body.contains("Follow-up chores"),
            "the real next heading must still end the section: {}",
            section.body
        );
    }

    #[test]
    fn locate_design_section_falls_back_to_whole_doc_when_unmatched() {
        let doc = "# Design\n\n## Goals\n\nShip it.\n";
        let section = locate_design_section(doc, "Run durable Astra-high guide jobs", "docs/designs/guides.md");
        assert!(section.is_whole_doc_fallback());
        assert_eq!(section.body, doc);
    }

    #[test]
    fn deferred_scope_high_finding_passes_the_severity_gate() {
        let result = ReviewResult {
            pr_url: "https://github.com/org/repo/pull/2969".to_owned(),
            head_sha: "abc".to_owned(),
            summary: "missing brief deliverable".to_owned(),
            revision_warranted: true,
            findings: vec![
                ReviewFinding::builder()
                    .severity(ReviewFindingSeverity::High)
                    .category(ReviewFindingCategory::DeferredScope)
                    .file("PR diff")
                    .title("Missing brief deliverable: revision-aware broker")
                    .detail("The work-item brief names a broker the diff does not implement.")
                    .confidence(ReviewFindingConfidence::High)
                    .build(),
            ],
            regression_check: crate::types::RegressionCheck {
                performed: true,
                suspected_deletions: Vec::new(),
            },
        };
        assert!(
            passes_severity_gate(&result),
            "deferred_scope/high is the revision-forcing class the brief-conformance rubric uses"
        );
    }

    #[test]
    fn locate_design_section_prefers_numbered_breakdown_entry_over_whole_doc() {
        let doc = "# Design\n\n## Proposed implementation task breakdown\n\n\
                   1. Protocol types. Add the contract.\n\
                   2. Engine handler. Depends on 1.\n";
        let entries = [
            BreakdownSection {
                title: "Protocol types. Add the contract.",
                body: "",
            },
            BreakdownSection {
                title: "Engine handler. Depends on 1.",
                body: "",
            },
        ];
        let section = locate_design_section_with_breakdown(doc, "Protocol types", "docs/designs/guides.md", &entries);
        assert_eq!(section.heading.as_deref(), Some("Protocol types. Add the contract."));
        assert!(section.body.contains("Protocol types"));
        assert!(!section.is_whole_doc_fallback());
    }

    #[test]
    fn locate_design_section_prefers_bullet_breakdown_entry_over_whole_doc() {
        let doc = "# Design\n\n## Implementation plan\n\n\
                   - **6f-4: protocol additions.** Adds RegisterAppSession.\n\
                   - **6f-5: engine-side dispatch.** ServerState tracks sessions.\n";
        let entries = [
            BreakdownSection {
                title: "6f-4: protocol additions",
                body: "Adds RegisterAppSession.",
            },
            BreakdownSection {
                title: "6f-5: engine-side dispatch",
                body: "ServerState tracks sessions.",
            },
        ];
        let section =
            locate_design_section_with_breakdown(doc, "6f-4: protocol additions", "docs/designs/rpc.md", &entries);
        assert_eq!(section.heading.as_deref(), Some("6f-4: protocol additions"));
        assert!(section.body.contains("Adds RegisterAppSession"));
        assert!(!section.body.contains("engine-side dispatch"));
    }

    #[test]
    fn brief_conformance_rubric_is_blocking_and_forbids_trusting_the_pr_body() {
        let rubric = render_brief_conformance_rubric();
        assert!(rubric.contains("Brief conformance — CRITICAL"));
        assert!(rubric.contains("must not skip"));
        assert!(rubric.contains("silently substituted"));
        assert!(rubric.contains("Do **not** trust the PR body"));
        assert!(rubric.contains("category: \"deferred_scope\""));
        assert!(rubric.contains("revision-aware broker"));
    }

    #[test]
    fn brief_conformance_rubric_carries_the_manual_verification_carve_out() {
        let rubric = render_brief_conformance_rubric();
        assert!(
            rubric.contains("manual/interactive-verification"),
            "rubric must reference the manual/interactive-verification carve-out so it does not \
             contradict the Deferred-scope hygiene rubric's exception: {rubric}"
        );
        assert!(rubric.contains("headless worker cannot perform"));
    }
}
