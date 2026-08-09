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

/// Design-discipline guidance distilled into lessons general enough to apply
/// to any project. Shared by both design-family directives: an
/// undocumented decision, an invariant pitched at the wrong level, or an
/// unfalsifiable rejection costs the same whether the doc is being
/// authored fresh or reconciled against what shipped.
pub(super) fn design_discipline_block() -> String {
    let mut out = String::new();
    out.push_str("- **Design discipline:**\n");
    out.push_str("  - **Name the contested property up front.** If this project's central bet is a property a reviewer could reasonably disagree with, put that property in the title or opening sentence — not bury it as an implementation detail nobody is ever asked about.\n");
    out.push_str("  - **Silence is not neutral.** If you can't find a recorded reason for an existing decision, don't assume a reason once existed and was lost — the stronger and more common reading is that the decision was never made at all. Treat that as a finding, not something to paper over.\n");
    out.push_str("  - **State invariants at the level of the property that is load-bearing, not the container that usually carries it.** If an implementation could satisfy the letter of a constraint while breaking everything downstream of it, the constraint is written at the wrong level. When you record two things as equivalent, name the dimension the equivalence holds on — an equivalence noted on one dimension gets read as holding on all of them.\n");
    out.push_str("  - **Every rejection must be checkable, and must survive contact with existing practice.** A pejorative label is not an argument. If you reject an approach that this project (or a comparable one) already relies on, name that precedent and say why the reasoning doesn't apply to it — a rejection that would also disqualify an established approach isn't a rejection. Also check that each requirement used to reject an alternative is a real requirement, not an artifact of the option you'd already picked.\n");
    out.push_str("  - **Decide whether a study is choosing between options or validating one, and say so before you run it.** A study that can only return \"the chosen approach works\" or \"it's broken\" will never surface that a different approach is better, however carefully it's run. Evidence gathered to verify a claim can't later stand in for a comparison it was never asked to make, and a hedge like \"fine, if that option were chosen\" is marking an unmade decision — escalate the decision instead of filing the hedge as a result.\n");
    out.push_str("  - **A gate at the end of a phase has no authority over the phase it belongs to** — it can only block what comes after. And a hand-built reproduction of a real system is structurally unable to find integration bugs, because it's built from the same beliefs that produced the code; only the genuine end-to-end path finds those.\n");
    out.push_str("  - **Don't let documents or tests harden drift into fact.** When a premise changes, make the change and everything downstream of it visible in the same diff — quietly folding an amendment in before anyone sees the contradiction just hides it. A test that pins a since-superseded requirement turns drift into a defended invariant, so sweep the tests that pin a premise in the same change that changes the premise. Separate a doc's durable reasoning from its perishable status/gaps/tasks so the latter can be refreshed without disturbing the former.\n");
    out
}

/// Render `products.design_guidance` (per-product, design-directive-only
/// prompt text — see [`crate::work::Product::design_guidance`]), wrapped in
/// visible `[product-design-guidance]…[/product-design-guidance]` markers so
/// a human reading a transcript can see exactly what the engine injected —
/// mirrors the `[product-preamble]` convention `worker_spawn.rs` uses for
/// `dispatch_preamble`. `None` / empty → no block, today's behaviour. Shared
/// by [`compose_design_directive`] and [`compose_design_postmortem_directive`]
/// so the guidance text is rendered from exactly one place.
fn design_guidance_block(design_guidance: Option<&str>) -> String {
    let Some(guidance) = design_guidance.filter(|s| !s.is_empty()) else {
        return String::new();
    };
    format!("[product-design-guidance]\n{guidance}\n[/product-design-guidance]\n\n")
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
/// a concrete penalty for under-splitting (an oversize entry the *planner*
/// would re-prompt to force-split — a behaviour since retired because it
/// re-expanded design-reviewed entries) and no cost at all for over-splitting,
/// so a worker unsure about a boundary was correctly reading "split" as the
/// safe move. Counting entries in the merged design docs under `docs/designs/`
/// showed where that lands: 31, 24, 24, 20, 14, 12, 9, 9, 8, 5, 4, 3 — a median
/// near 10 but four of twelve at 20 or more, a tail as heavy as the
/// materialised `project_task` counts, so the over-production originates in
/// the breakdown step rather than in later accretion. The large ones blow up
/// because the rules compound: a design spanning several subsystems, each
/// with a multi-phase shape and a validation sweep, hits several split rules
/// at once and multiplies instead of adding. Hence the calibration added
/// below — a named cost for over-splitting, a named ban on inventing entries
/// to pad a band, anchor bands that include a one-entry outcome, an explicit
/// "the rules add, they do not multiply", and a required `Breakdown size:`
/// line that makes the author defend N from the change (not from a band).
/// The planner now trusts the breakdown one-for-one (entry count in, row
/// count out); design-time honesty is the only control. No cap: a design that
/// genuinely needs twenty entries may still propose twenty.
pub(super) fn compose_design_directive(parent_project: Option<&Project>, design_guidance: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str(&design_guidance_block(design_guidance));
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
    out.push_str(&design_discipline_block());
    out.push_str("- the **Proposed implementation task breakdown** section must:\n");
    out.push_str("  - use exactly that heading (`## Proposed implementation task breakdown`) so a downstream parser can locate it reliably.\n");
    out.push_str("  - **each entry must open with `### <task name>`** — a `###` ATX heading, one per entry, in dependency order. This is the entry boundary a downstream parser keys off of; bullets, bold-prefixed numbers, or plain numbered lines are not recognised as entries and silently degrade the handoff to free-form prose. Put the task name text directly after `### ` (no bold wrapping needed — the heading itself is the boundary).\n");
    out.push_str("  - list PR-sized tasks in dependency order, where each entry contains:\n");
    out.push_str("    - a short **task name** (one line).\n");
    out.push_str("    - a one-paragraph **scope** description.\n");
    out.push_str("    - an **effort hint**: one of `trivial | small | medium | large`.\n");
    out.push_str("    - **explicit dependencies** — which other entries in this list gate this one (use the task names; \"none\" if it can start immediately).\n");
    out.push_str("    - a **scope tag** — exactly one of `Scope: in-scope` or `Scope: deferred (future / not a v1 blocker)`. Use this exact `Scope:` line (own line, this literal wording) on every entry — downstream scheduling keys off it verbatim, so free prose like \"this is a stretch goal\" instead of the tag will not be recognised. Tag an entry `deferred` when it is explicitly out of scope for v1, a stretch goal, or something you are deliberately not proposing for immediate implementation; follow the tag with a short inline reason (e.g. `Scope: deferred (future / not a v1 blocker) — needs the batch API landing in phase 2`).\n");
    out.push_str("  - **size each entry to one reviewable PR by one worker in one session.** This is the granularity scheduling materialises into tasks **one-for-one** — the planner trusts this breakdown and does not re-expand entries, so pre-split only where the work genuinely needs cutting:\n");
    out.push_str("    - keep each entry single-subsystem and single-PR when the seams are real review boundaries. Scope that spans several subsystems (for example, a client, service, shared data model, and user interface) is several entries with dependency edges when each subsystem is its own reviewable unit — but a thin change across layers that belongs in one PR is one entry, not one per layer.\n");
    out.push_str("    - multi-phase scope (\"parse (i)… and (ii)… and emit… and validate…\") is several entries when each phase is a separate reviewable PR — list each phase separately with explicit dependencies; leave adjacent phases that would ship together as one entry.\n");
    out.push_str("    - sweeps and validation campaigns (\"validate/sweep/migrate all N X\", an all-lists reconciliation, a corpus-wide fixture sweep) are separate dependent entries when they are independently reviewable work, listed after the implementation they validate — do not invent a sweep entry solely because the implementer mentions tests.\n");
    out.push_str("    - unknown-format discovery (study / dump / reverse-engineer / reconcile-against-source) is its own investigation entry when discovery is a real gate before implementation.\n");
    out.push_str("    - a multi-clause Scope paragraph, multi-layer naming, or length alone is not automatic grounds to split — if the work is one reviewable PR, keep one entry.\n");
    out.push_str("  - **size the breakdown as a whole to the size of the problem.** The rules above tell you where to cut; this tells you how many cuts the problem is worth. Both directions of miscalibration — and inventing work that is not in the problem — cost something real:\n");
    out.push_str("    - **too few entries** — an oversize entry becomes one large task on the board (the planner will not re-split it for you). Prefer honest pre-splitting at design time when the seams are real.\n");
    out.push_str("    - **too many entries** — each entry becomes its own worker session, PR, and review cycle. Entries carved below one reviewable unit serialise changes that belonged in a single PR, put sibling workers into merge conflicts over the same files, and bury the real critical path in bookkeeping. Splitting a one-seam change into eight entries is exactly as wrong as folding four subsystems into one; when you are unsure about a boundary, neither side is automatically safe.\n");
    out.push_str("    - **fabricated entries** — never invent tasks the requested change does not need in order to reach a band, look thorough, or pad a breakdown. Speculative, future, or \"nice to have\" work that the change does not require must not be added — neither as `Scope: in-scope` entries nor as `Scope: deferred` ones. A deferred entry for work nobody asked for is scope creep with a later timestamp: it still materialises as a row somebody has to triage. Derive every entry from the design's actual seams and callers; if the doc's non-goals or chosen approach already put a topic out of scope, do not reintroduce it as a deferred entry.\n");
    out.push_str("    - **one entry is a valid and expected outcome.** When the change is genuinely one reviewable PR by one worker in one session — a single seam and its only thin callers, a flag-plus-confirmation, a bounded fix with no intermediate contract worth reviewing alone — a single-entry breakdown is the correct answer, not a sign the design under-analysed the problem. Do not split solely so the count lands inside a multi-entry band.\n");
    out.push_str("    - **do not manufacture unreviewable intermediates.** If a seam and its only callers would land as separate entries, and the seam's own PR would ship with no exercised caller, keep them in one entry. A three-link chain where the middle and last links are a handful of lines against a contract nobody has exercised is over-splitting, even when each link looks \"single-subsystem.\" Reaching that conclusion and then splitting anyway is wrong — the conclusion is binding.\n");
    out.push_str("    - **the split rules add, they do not multiply.** A design touching four subsystems, each with a few phases and a validation sweep, must not be expanded to subsystems x phases x sweeps entries. Apply each rule only where the work genuinely needs cutting, and leave the rest whole: if two adjacent phases in the same subsystem would be reviewed as one PR by one worker, they are one entry, not two.\n");
    out.push_str("    - calibrate the total against these anchors. They are reference points for a sanity check, not targets to hit and not a cap — a design that genuinely needs more may propose more, but a count well outside the band for its scope is a signal to re-check the breakdown before shipping it. Never invent or split entries just to land inside a band:\n");
    out.push_str("      - a change that is one reviewable PR (one seam and its thin callers, a single bounded edit): **1 entry** is correct; a change behind one seam in one subsystem that still wants a few natural cuts (e.g. implement + test sweep, or two independent call sites with real review weight): **1-4 entries**.\n");
    out.push_str("      - a feature contained in one subsystem, carrying its own schema/migration or test sweep: **4-8 entries**.\n");
    out.push_str("      - a feature spanning two or three subsystems (e.g. engine + cli + app), or one subsystem plus a discovery step: **8-14 entries**.\n");
    out.push_str("      - a genuinely large build-out — a new integration or subsystem reaching across most of the stack, with its own investigations and acceptance sweeps: **15+ entries**, and the count needs the justification below to earn it.\n");
    out.push_str("    - **state the count and defend it from the change.** Open the section with a single line, before the first entry, of exactly this shape: `Breakdown size: N entries (M in-scope, K deferred) — <one sentence describing what the change actually touches and why that yields N>`. The justification must name the real seams, callers, phases, or sweeps that produce N — not that N sits inside or below a band. Comparing N to an anchor band is optional context after that justification; it must not by itself constitute the defence. If you cannot justify N from the change, the breakdown is miscalibrated — merge entries (or drop fabricated ones) until you can. This line is prose about the section, not an entry: it has no name/scope/effort/dependency shape and is not a task.\n");
    out.push_str("  - note which tasks at the same dependency depth may run in parallel, so the task graph (not just a linear list) is expressible.\n");
    out.push_str("  - when you mark tasks parallel, weigh **file** overlap, not just functional independence: two tasks can be independent in design yet edit the same shared integration surface. If two otherwise-parallel tasks are clearly and substantially likely to co-edit the same files, say so — give them a defined order and note that the later one must forward-port the earlier one's changes preservingly (integrate, never delete). Do not over-serialise: only flag clear, substantial overlap; incidental overlap stays parallel.\n");
    out.push_str("  - when analysis of the requested change surfaces work that is deliberately out of scope for v1, include those items as their own entries (tagged `Scope: deferred (future / not a v1 blocker)`, see above) rather than silently omitting a decision the design already made — silent omissions force the coordinator to guess what was considered and rejected. Do not invent deferred rows for topics the change never needed; do not drop a real deferred decision just because it is deferred — the scope tag is what lets a genuine deferral stay visible without being auto-started.\n");
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
    design_guidance: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(&design_guidance_block(design_guidance));
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
    out.push_str(&design_discipline_block());
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
        "- the project's `design_doc_path` pointer is not yet set. Place the doc at `docs/designs/{slug}.md`; if the repository already has an established design-doc layout, use that layout instead. After you create the file, set the pointer with `boss project set-design-doc --project <id> --path <path>` so the next run resolves it directly.\n",
    ))
}
