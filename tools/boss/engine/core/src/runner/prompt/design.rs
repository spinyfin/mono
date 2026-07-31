//! Design-family directive builders: the `kind = 'design'` and
//! `kind = 'design_postmortem'` prompt fragments, and the shared building
//! blocks they compose (markdown structure guidance, the canonical design
//! doc path line, and the attentions question-manifest instruction). Split
//! out of `runner/prompt.rs` to keep that file under the repo's `file/size`
//! check — see the module's call sites in `compose_execution_prompt`.

use crate::work::Project;

/// Markdown structure guidance shared by design and investigation docs.
/// Generated docs frequently render their intro as a single wall-of-text
/// paragraph: workers write metadata and framing as consecutive
/// single-newline lines, and a single newline is a soft wrap in Markdown —
/// it folds into one paragraph on render even though the source looks fine
/// line-by-line in an editor. Stated explicitly here because it looks
/// correct in the editor, so workers never self-correct without being told.
pub(super) fn doc_structure_conventions_block() -> String {
    let mut out = String::new();
    out.push_str("- **Markdown structure — avoid wall-of-text rendering.** A single newline is a soft wrap in Markdown: consecutive non-blank lines collapse into one paragraph when rendered, even though they look like separate lines in an editor.\n");
    out.push_str("  - put metadata (Date, Task/provenance, related work items) in a bullet list or table immediately after the H1 — never as consecutive prose lines.\n");
    out.push_str("  - put a blank line between every logical block (metadata, framing, method, each finding, the verdict). Single newlines between blocks will smoosh them together.\n");
    out.push_str("  - give the verdict/TL;DR its own short section or paragraph — never embed it mid-paragraph.\n");
    out.push_str("  - keep the first paragraph after the title to 2-3 sentences; move framing and method detail into later sections.\n");
    out
}

/// Directive block for the synthetic `kind = 'design'` task that the
/// engine auto-creates with every project. Without this block the
/// `project_design` worker only sees the generic "draft or update a
/// repo-backed design artifact" line and frequently starts
/// implementing — observed against worker O'Brien
/// (exec_18aebf0caa1187e8_b). State up front that the deliverable is
/// a markdown design doc (not code), name the canonical path, and
/// list the section shape the reader expects so the worker doesn't
/// invent its own.
///
/// The breakdown block's split rules used to be one-directional: they named
/// a concrete penalty for under-splitting (an oversize entry the scheduler
/// rejects and re-plans) and no cost at all for over-splitting, so a worker
/// unsure about a boundary was correctly reading "split" as the safe move.
/// Counting entries in the merged design docs under `docs/designs/` showed
/// where that lands: 31, 24, 24, 20, 14, 12, 9, 9, 8, 5, 4, 3 — a median
/// near 10 but four of twelve at 20 or more, a tail as heavy as the
/// materialised `project_task` counts, so the over-production originates in
/// the breakdown step rather than in later accretion. The large ones blow up
/// because the rules compound: a design spanning several subsystems, each
/// with a multi-phase shape and a validation sweep, hits several split rules
/// at once and multiplies instead of adding. Hence the calibration added
/// below — a named cost for over-splitting, anchor bands relating entry
/// count to scope, an explicit "the rules add, they do not multiply", and a
/// required `Breakdown size:` line that makes the author defend N. No cap:
/// a design that genuinely needs twenty entries may still propose twenty.
pub(super) fn compose_design_directive(parent_project: Option<&Project>) -> String {
    let mut out = String::new();
    out.push_str("Expected outcome for this run:\n");
    out.push_str("- the deliverable is a **design document**, not an implementation. Do not edit code, do not start prototyping, do not open partial implementation PRs.\n");
    out.push_str("- the PR for this run contains **only the design doc** (one new or updated markdown file). If you find yourself touching `.rs`, `.ts`, `.swift`, build files, or anything else, stop — you are out of scope.\n");
    if let Some(path_line) = canonical_design_doc_path_line(parent_project) {
        out.push_str(&path_line);
    }
    out.push_str(&doc_structure_conventions_block());
    out.push_str("- the design must cover, at minimum, these sections (use these as headings unless the parent project's description specifies a different shape):\n");
    out.push_str("  - **Goals** — what this project is trying to achieve, pulled from the parent project's goal/description above.\n");
    out.push_str("  - **Non-goals** — what is explicitly out of scope so reviewers don't have to guess.\n");
    out.push_str("  - **Alternatives considered** — at least two distinct approaches and why they were not chosen.\n");
    out.push_str("  - **Chosen approach** — the design itself, with enough detail that a follow-up implementation task can be filed against it.\n");
    out.push_str("  - **Risks / open questions** — anything the author wants a human reviewer to land on before implementation starts.\n");
    out.push_str("  - **Proposed implementation task breakdown** — this section is **required** and must be the final section of the doc. It is the machine-findable handoff to scheduling (see below).\n");
    out.push_str("- the **Proposed implementation task breakdown** section must:\n");
    out.push_str("  - use exactly that heading (`## Proposed implementation task breakdown`) so a downstream parser can locate it reliably.\n");
    out.push_str("  - list PR-sized tasks in dependency order, where each entry contains:\n");
    out.push_str("    - a short **task name** (one line).\n");
    out.push_str("    - a one-paragraph **scope** description.\n");
    out.push_str("    - an **effort hint**: one of `trivial | small | medium | large`.\n");
    out.push_str("    - **explicit dependencies** — which other entries in this list gate this one (use the task names; \"none\" if it can start immediately).\n");
    out.push_str("    - a **scope tag** — exactly one of `Scope: in-scope` or `Scope: deferred (future / not a v1 blocker)`. Use this exact `Scope:` line (own line, this literal wording) on every entry — downstream scheduling keys off it verbatim, so free prose like \"this is a stretch goal\" instead of the tag will not be recognised. Tag an entry `deferred` when it is explicitly out of scope for v1, a stretch goal, or something you are deliberately not proposing for immediate implementation; follow the tag with a short inline reason (e.g. `Scope: deferred (future / not a v1 blocker) — needs the batch API landing in phase 2`).\n");
    out.push_str("  - **size each entry to one reviewable PR by one worker in one session.** This is the granularity scheduling materialises into tasks, so pre-split the work here — an oversize entry forces the scheduler to reject and re-plan it:\n");
    out.push_str("    - keep each entry single-subsystem and single-PR. Scope that spans several subsystems (engine + cli + protocol + app + …) is several entries with dependency edges, not one.\n");
    out.push_str("    - multi-phase scope (\"parse (i)… and (ii)… and emit… and validate…\") is several entries — list each phase separately with explicit dependencies, never one entry that does it all.\n");
    out.push_str("    - sweeps and validation campaigns (\"validate/sweep/migrate all N X\", an all-lists reconciliation, a corpus-wide fixture sweep) are separate dependent entries, listed after the implementation they validate — do not fold them into the implementer.\n");
    out.push_str("    - unknown-format discovery (study / dump / reverse-engineer / reconcile-against-source) is its own investigation entry, sequenced before the implementation that consumes its findings.\n");
    out.push_str("    - if an entry needs a paragraph to describe, it is probably several tasks — split it.\n");
    out.push_str("  - **size the breakdown as a whole to the size of the problem.** The rules above tell you where to cut; this tells you how many cuts the problem is worth. Both directions of miscalibration cost something real:\n");
    out.push_str("    - **too few entries** — an oversize entry forces the scheduler to reject and re-plan it (the failure the split rules above exist to prevent).\n");
    out.push_str("    - **too many entries** — each entry becomes its own worker session, PR, and review cycle. Entries carved below one reviewable unit serialise changes that belonged in a single PR, put sibling workers into merge conflicts over the same files, and bury the real critical path in bookkeeping. Splitting a one-seam change into eight entries is exactly as wrong as folding four subsystems into one; when you are unsure about a boundary, neither side is automatically safe.\n");
    out.push_str("    - **the split rules add, they do not multiply.** A design touching four subsystems, each with a few phases and a validation sweep, must not be expanded to subsystems x phases x sweeps entries. Apply each rule only where the work genuinely needs cutting, and leave the rest whole: if two adjacent phases in the same subsystem would be reviewed as one PR by one worker, they are one entry, not two.\n");
    out.push_str("    - calibrate the total against these anchors. They are reference points for a sanity check, not targets to hit and not a cap — a design that genuinely needs more may propose more, but a count well outside the band for its scope is a signal to re-check the breakdown before shipping it:\n");
    out.push_str("      - a change behind one seam in one subsystem (a new flag, one new endpoint, a bounded refactor): **2-4 entries**.\n");
    out.push_str("      - a feature contained in one subsystem, carrying its own schema/migration or test sweep: **4-8 entries**.\n");
    out.push_str("      - a feature spanning two or three subsystems (e.g. engine + cli + app), or one subsystem plus a discovery step: **8-14 entries**.\n");
    out.push_str("      - a genuinely large build-out — a new integration or subsystem reaching across most of the stack, with its own investigations and acceptance sweeps: **15+ entries**, and the count needs the justification below to earn it.\n");
    out.push_str("    - **state the count and defend it.** Open the section with a single line, before the first entry, of exactly this shape: `Breakdown size: N entries (M in-scope, K deferred) — <one sentence on why this problem needs N entries rather than fewer>`. Name the anchor band you are calibrating against, and if you land above it, say what makes this design bigger than that band. If you cannot justify N, the breakdown is miscalibrated — merge entries until you can. This line is prose about the section, not an entry: it has no name/scope/effort/dependency shape and is not a task.\n");
    out.push_str("  - note which tasks at the same dependency depth may run in parallel, so the task graph (not just a linear list) is expressible.\n");
    out.push_str("  - when you mark tasks parallel, weigh **file** overlap, not just functional independence: two tasks can be independent in design yet edit the same file (e.g. a compact-view task and a detail-view task that both edit the same component/container). If two otherwise-parallel tasks are clearly and substantially likely to co-edit the same files, say so — give them a defined order and note that the later one must forward-port the earlier one's changes preservingly (integrate, never delete). Do not over-serialise: only flag clear, substantial overlap; incidental overlap stays parallel.\n");
    out.push_str("  - include items that are deferred or explicitly out of scope as their own entries (tagged `Scope: deferred (future / not a v1 blocker)`, see above) rather than silently omitting them — silent omissions force the coordinator to guess what was considered and rejected. Do not drop the entry just because it is deferred; the scope tag is what lets it stay visible without being auto-started.\n");
    out.push_str("  - This section is what the design doc's auto-populate step will consume to materialise dependent tasks with edges, so completeness matters.\n");
    out.push_str(&design_questions_manifest_block());
    out.push_str("- when the doc is ready for review, push it and open a PR (see the acceptance criterion below). Do not start implementation tasks — those come from follow-up work items the human files after the design is approved.\n");
    out
}

/// Directive block for a `kind = 'design_postmortem'` task, auto-scheduled
/// by `project_postmortem_sweep` once a project's implementation work
/// drains to zero. Deliberately the mirror image of
/// [`compose_design_directive`]: that one says "author a new doc"; this one
/// says "reconcile the existing doc against what actually shipped." The
/// task's own `description` (rendered above this block via
/// `work_item_details`) already carries the remit brief — the project's
/// design-doc path/branch and the enumerated merged PRs to review — so this
/// block only needs to state the doc-only scope constraint and the update
/// method.
pub(super) fn compose_design_postmortem_directive(
    parent_project: Option<&Project>,
    structured_output_path: &str,
) -> String {
    let mut out = String::new();
    out.push_str("Expected outcome for this run:\n");
    out.push_str(
        "- the deliverable is an **update to the project's existing design document**, not a new document and not an implementation. Do not edit code, do not start prototyping, do not open partial implementation PRs.\n",
    );
    out.push_str(
        "- the PR for this run contains **only the design doc update** (edits to the existing markdown file). If you find yourself touching `.rs`, `.ts`, `.swift`, build files, or anything else, stop — you are out of scope.\n",
    );
    if let Some(path_line) = canonical_design_doc_path_line(parent_project) {
        out.push_str(&path_line);
    }
    out.push_str(&doc_structure_conventions_block());
    out.push_str("- review each merged PR listed in the details above (`gh pr view`/`gh pr diff`) alongside the current doc, and update the doc to reflect **as-built reality**:\n");
    out.push_str("  - decisions that diverged from what the doc originally said, and why (as best you can tell from the PR/commit history).\n");
    out.push_str("  - scope that was added or dropped relative to the doc's plan.\n");
    out.push_str("  - contracts, interfaces, or data models that evolved during implementation.\n");
    out.push_str(
        "- edit the doc in place — do not append a separate \"postmortem\" or \"changelog\" section unless the doc already uses that structure. The goal is a design doc that reads as if it were written *after* the work, not a diff log bolted onto the original.\n",
    );
    out.push_str(
        "- if the merged PRs matched the doc's plan closely, the update may be small (e.g. a note confirming what shipped matches the design) — that's fine, but still open a PR with that update rather than stopping with no PR at all.\n",
    );
    out.push_str("- open a PR with the update regardless of which repo the doc lands in — the PR is the review window, same as any design change.\n");
    out.push_str(&super::postmortem_followups_emission_block(structured_output_path));
    out
}

/// Attentions question-manifest emission instruction (design:
/// `tools/boss/docs/designs/attentions.md`, "Creation pipeline"). Appended
/// to the `project_design` directive: a design worker that has genuine open
/// questions for the human emits a sibling `<slug>.attentions.json` manifest
/// next to the doc. The engine's `DesignDetector` parses it off the PR
/// branch and upserts an inline question group the human answers in the doc
/// viewer, batched into a single revision.
fn design_questions_manifest_block() -> String {
    let mut out = String::new();
    out.push_str("- OPTIONAL — open questions for the human: if, while writing the doc, you have specific decisions you want a human to make (yes/no calls, multiple-choice forks, or free-text prompts), emit a **questions manifest** as a sibling file next to the design doc — the same path with the `.md` extension replaced by `.attentions.json` (e.g. `…/designs/<slug>.attentions.json`).\n");
    out.push_str("  - The file is a JSON array. Each entry is an object:\n");
    out.push_str("    - `question_type` (required): one of `yes_no` | `multiple_choice` | `prompt` (free text).\n");
    out.push_str("    - `prompt` (required): the question shown to the human.\n");
    out.push_str("    - `choices` (required only for `multiple_choice`): a JSON array of option strings.\n");
    out.push_str("    - `anchor` (optional but encouraged): the heading slug the question is about, so it renders next to the relevant section.\n");
    out.push_str("  - Example: `[{\"question_type\":\"yes_no\",\"prompt\":\"Gate extraction behind a flag?\",\"anchor\":\"rollout\"},{\"question_type\":\"multiple_choice\",\"prompt\":\"One table or two?\",\"choices\":[\"one\",\"two\"],\"anchor\":\"data-model\"}]`\n");
    out.push_str("  - Only emit this when you genuinely need the human to decide something; omit the file entirely otherwise. Do NOT restate the doc's \"Risks / open questions\" prose here — the manifest is just the machine-actionable subset you want answered. The engine batches all entries into one group, so answering them yields a single doc revision.\n");
    out
}

/// If the parent project has an explicit `design_doc_path` pointer
/// (set via `boss project design-doc`), emit that as the canonical
/// path. Otherwise fall back to the `<repo>/docs/designs/<slug>.md`
/// convention, anchored on the project's slug so two design tasks
/// don't collide. Returns `None` only when we have no project at
/// all — in practice the dispatcher always has one for
/// `kind = 'design'` rows, but the runner stays defensive.
fn canonical_design_doc_path_line(parent_project: Option<&Project>) -> Option<String> {
    let project = parent_project?;
    if let Some(path) = project
        .design_doc_path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        return Some(format!(
            "- the canonical path for this design doc is `{path}` (set on the project's `design_doc_path` pointer). Write the doc there.\n",
        ));
    }
    let slug = if project.slug.trim().is_empty() {
        "design"
    } else {
        project.slug.trim()
    };
    Some(format!(
        "- the project's `design_doc_path` pointer is not yet set. Place the doc at `docs/designs/{slug}.md` (the repo's convention; adjust to the product's docs layout if the repo already has one — e.g. `tools/boss/docs/designs/{slug}.md` for the Boss product). After you create the file, set the pointer with `boss project set-design-doc --project <id> --path <path>` so the next run resolves it directly.\n",
    ))
}
