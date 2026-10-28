//! Task context rendered into the review-guide prompt after the source
//! manifest: what the engine knows about the change beyond the PR itself.
//!
//! The pinned template asks every guide to open with a plain-language
//! `## Orientation` section. That section needs facts the PR description
//! alone does not carry — the original task, the revisions filed against the
//! PR and why, and the project the change belongs to. The engine gathers them
//! into a [`GuideContext`] and this module renders them. Like the rest of the
//! crate this is pure: no database access, so the engine owns the fail-loudly
//! behavior when a lookup fails and this crate only formats what it is given.

use std::fmt::Write as _;

/// Longest task description rendered verbatim. Descriptions longer than this
/// are cut with an explicit marker so the prompt stays bounded.
const MAX_DESCRIPTION_CHARS: usize = 8_000;

/// Why a revision was filed against the PR. The orientation for a revision
/// guide names this so the reader knows what kind of follow-up they are
/// looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevisionOrigin {
    /// Addresses findings from an automated or human code review.
    ReviewFindings,
    /// Repairs a failing CI check.
    CiFix,
    /// Resolves a merge conflict with the base branch.
    MergeConflict,
    /// Requested directly by a person.
    HumanRequested,
}

impl RevisionOrigin {
    fn label(self) -> &'static str {
        match self {
            Self::ReviewFindings => "review-findings revision",
            Self::CiFix => "CI-fix revision",
            Self::MergeConflict => "merge-conflict-resolution revision",
            Self::HumanRequested => "human-requested revision",
        }
    }
}

/// The task that opened the PR (the chain root).
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct RootTask {
    pub name: String,
    /// Plain-words kind of work, e.g. "chore" or "project task".
    pub kind: String,
    pub description: Option<String>,
}

/// A revision pushed onto the root's PR.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct Revision {
    pub origin: RevisionOrigin,
    pub name: String,
    pub description: Option<String>,
    /// Lifecycle status, e.g. "in_review" or "done".
    pub status: String,
    /// Whether this revision produced the head commit this guide is pinned to.
    #[builder(default)]
    pub produced_pinned_head: bool,
}

/// How another piece of project work relates to the change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// The change depends on this work landing first.
    BuildsOn,
    /// This work depends on the change landing first.
    Enables,
    /// No declared dependency either way.
    Related,
}

impl Relation {
    fn label(self) -> &'static str {
        match self {
            Self::BuildsOn => "builds on (earlier piece)",
            Self::Enables => "enabled by this change (later piece)",
            Self::Related => "related piece, no declared dependency",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct RelatedWork {
    pub name: String,
    pub relation: Relation,
    pub status: String,
}

/// The project the change is one step of.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct Project {
    pub name: String,
    pub goal: String,
    pub description: String,
    /// Where the project's design document lives, as a sentence the model can
    /// act on (repo path plus link), or why the pointer cannot be resolved.
    pub design_doc: Option<String>,
    #[builder(default)]
    pub related_work: Vec<RelatedWork>,
}

/// Everything the orientation section is built from.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct GuideContext {
    pub root: RootTask,
    /// Oldest first. Empty for a primary change with no revisions.
    #[builder(default)]
    pub revisions: Vec<Revision>,
    pub project: Option<Project>,
}

/// Render the `### Task context` block appended to the prompt.
pub fn render_guide_context(context: &GuideContext) -> String {
    let mut out = String::new();
    out.push_str("### Task context\n\n");
    out.push_str(
        "Inputs for the `## Orientation` section, taken from the engine's records of the work. Treat them as claims to \
         check against the source, as with the PR description. Describe things in plain words and do not repeat \
         internal work-item identifiers.\n\n",
    );

    out.push_str("#### Original change\n\n");
    let _ = writeln!(out, "- Kind of work: {}", context.root.kind);
    let _ = writeln!(out, "- Title: {}", one_line(&context.root.name));
    out.push_str("- Task description:\n\n");
    out.push_str(&quote_description(context.root.description.as_deref()));
    out.push('\n');

    out.push_str("#### Revisions filed against this PR\n\n");
    if context.revisions.is_empty() {
        out.push_str(
            "None. This guide covers a primary change; the Orientation describes the change itself and needs no \
             revision structure.\n\n",
        );
    } else {
        let _ = writeln!(
            out,
            "{} revision(s), oldest first. This guide covers the whole PR, including the original change, so the \
             Orientation must summarize the original change first and then say what these revisions change and why. \
             Check each against the pinned diff; a revision that is not yet reflected in the head revision is not \
             part of this change.\n",
            context.revisions.len()
        );
        for (index, revision) in context.revisions.iter().enumerate() {
            let latest = if revision.produced_pinned_head {
                "; produced the head revision this guide is pinned to"
            } else {
                ""
            };
            let _ = writeln!(
                out,
                "{}. {} (status: {}{latest}) — {}\n",
                index + 1,
                revision.origin.label(),
                revision.status,
                one_line(&revision.name)
            );
            out.push_str(&quote_description(revision.description.as_deref()));
            out.push('\n');
        }
    }

    out.push_str("#### Containing project\n\n");
    match &context.project {
        None => out.push_str("None. This change is not part of a larger project; do not describe one.\n\n"),
        Some(project) => {
            let _ = writeln!(out, "- Project: {}", one_line(&project.name));
            let _ = writeln!(out, "- Goal: {}", one_line(&project.goal));
            out.push_str("- Summary:\n\n");
            out.push_str(&quote_description(Some(project.description.as_str())));
            out.push('\n');
            match &project.design_doc {
                Some(reference) => {
                    let _ = writeln!(out, "- Design document: {reference}");
                }
                None => out.push_str("- Design document: none recorded for this project.\n"),
            }
            if project.related_work.is_empty() {
                out.push_str("- Other project work: none recorded.\n");
            } else {
                out.push_str("- Other project work (status in parentheses):\n");
                for work in &project.related_work {
                    let _ = writeln!(
                        out,
                        "  - {} ({}) — {}",
                        one_line(&work.name),
                        work.status,
                        work.relation.label()
                    );
                }
            }
            out.push('\n');
        }
    }
    out
}

/// Collapse whitespace so a title or goal cannot introduce structure.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Block-quote a free-form description so its own headings and lists cannot
/// be mistaken for prompt structure, truncating it with a visible marker.
fn quote_description(description: Option<&str>) -> String {
    let text = description.map(str::trim).filter(|text| !text.is_empty());
    let Some(text) = text else {
        return "> (no description recorded)\n".to_owned();
    };
    let mut quoted = String::new();
    let mut shown = 0usize;
    let mut truncated = false;
    for line in text.lines() {
        let remaining = MAX_DESCRIPTION_CHARS - shown;
        if remaining == 0 {
            truncated = true;
            break;
        }
        let line: String = if line.chars().count() > remaining {
            truncated = true;
            line.chars().take(remaining).collect()
        } else {
            line.to_owned()
        };
        shown += line.chars().count() + 1;
        let _ = writeln!(quoted, "> {line}");
        if truncated {
            break;
        }
    }
    if truncated {
        let _ = writeln!(
            quoted,
            "> [description truncated at {MAX_DESCRIPTION_CHARS} characters]"
        );
    }
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> RootTask {
        RootTask::builder()
            .name("Add retry backoff")
            .kind("project task")
            .description("Retries should back off exponentially.\n## Not a heading")
            .build()
    }

    #[test]
    fn primary_change_has_no_revision_structure() {
        let rendered = render_guide_context(&GuideContext::builder().root(root()).build());
        assert!(rendered.contains("- Kind of work: project task"));
        assert!(rendered.contains("- Title: Add retry backoff"));
        assert!(rendered.contains("> Retries should back off exponentially."));
        assert!(rendered.contains("#### Revisions filed against this PR\n\nNone."));
        assert!(rendered.contains("None. This change is not part of a larger project"));
    }

    #[test]
    fn description_cannot_inject_headings() {
        let rendered = render_guide_context(&GuideContext::builder().root(root()).build());
        assert!(rendered.contains("> ## Not a heading"));
        assert!(!rendered.contains("\n## Not a heading"));
    }

    #[test]
    fn long_descriptions_are_truncated_with_a_marker() {
        let long = "x".repeat(MAX_DESCRIPTION_CHARS + 50);
        let quoted = quote_description(Some(&long));
        assert!(quoted.contains("[description truncated at 8000 characters]"));
        assert!(quoted.matches('x').count() == MAX_DESCRIPTION_CHARS);
    }

    #[test]
    fn revisions_render_oldest_first_with_origin_and_head_marker() {
        let context = GuideContext::builder()
            .root(root())
            .revisions(vec![
                Revision::builder()
                    .origin(RevisionOrigin::ReviewFindings)
                    .name("Address review findings")
                    .description("Cap the delay.")
                    .status("done")
                    .build(),
                Revision::builder()
                    .origin(RevisionOrigin::CiFix)
                    .name("Fix clippy")
                    .status("in_review")
                    .produced_pinned_head(true)
                    .build(),
            ])
            .build();
        let rendered = render_guide_context(&context);
        let first = rendered.find("1. review-findings revision (status: done)").unwrap();
        let second = rendered
            .find("2. CI-fix revision (status: in_review; produced the head revision this guide is pinned to)")
            .unwrap();
        assert!(first < second);
        assert!(rendered.contains("2 revision(s), oldest first"));
        assert!(rendered.contains("> (no description recorded)"));
    }

    #[test]
    fn project_context_lists_design_doc_and_related_work() {
        let project = Project::builder()
            .name("Resilient transport")
            .goal("Make the client survive flaky networks")
            .description("Adds retries, backoff, and circuit breaking.")
            .design_doc("`docs/designs/transport.md` on `main`")
            .related_work(vec![
                RelatedWork::builder()
                    .name("Retry policy types")
                    .relation(Relation::BuildsOn)
                    .status("done")
                    .build(),
                RelatedWork::builder()
                    .name("Circuit breaker")
                    .relation(Relation::Enables)
                    .status("todo")
                    .build(),
            ])
            .build();
        let rendered = render_guide_context(&GuideContext::builder().root(root()).project(project).build());
        assert!(rendered.contains("- Project: Resilient transport"));
        assert!(rendered.contains("- Goal: Make the client survive flaky networks"));
        assert!(rendered.contains("> Adds retries, backoff, and circuit breaking."));
        assert!(rendered.contains("- Design document: `docs/designs/transport.md` on `main`"));
        assert!(rendered.contains("Retry policy types (done) — builds on (earlier piece)"));
        assert!(rendered.contains("Circuit breaker (todo) — enabled by this change (later piece)"));
    }
}
