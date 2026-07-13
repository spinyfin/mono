//! Validation layer for [`PlannerOutput`] proposals.
//!
//! Sits between the Planner (LLM inference) and the Materializer (DB writes).
//! Every check here is **no-op-safe**: nothing is read from or written to the
//! database. On any rejection the caller maps the variant to its
//! `PLANNER_OUTCOME_*` constant and raises an attention item; no tasks are
//! created.
//!
//! See design §"Validation of the structured proposal":
//! `tools/boss/docs/designs/auto-populate-project-tasks-on-design-pr-merge.md`

use std::collections::{HashMap, HashSet};

use boss_protocol::{Confidence, PlannerOutput, ProposedEdge, ProposedTask};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The typed result of validating a [`PlannerOutput`] proposal.
///
/// All rejection variants are no-op-safe: nothing has been written before or
/// during validation. The Populator maps each variant to its
/// `PLANNER_OUTCOME_*` constant and raises an attention item.
///
/// Only `Valid` should proceed to the Materializer.
#[derive(Debug, PartialEq, Eq)]
pub enum ValidationResult {
    /// `breakdown_found == false` — the design doc had no task-breakdown
    /// section. Clean no-op; maps to `PLANNER_OUTCOME_NO_BREAKDOWN`.
    NoBreakdown,

    /// `breakdown_found == true` but `tasks` is empty — the planner found a
    /// section but extracted nothing meaningful from it. No-op + attention item.
    EmptyBreakdown,

    /// `tasks.len() > max_tasks`. Silent truncation is forbidden per the
    /// design; the whole proposal is rejected.
    /// Maps to `PLANNER_OUTCOME_REJECTED_TOO_MANY`.
    RejectedTooMany { count: usize, max: usize },

    /// A handle appears more than once in the `tasks` list.
    /// Maps to `PLANNER_OUTCOME_REJECTED_CYCLE` (re-uses the "bad graph" bucket).
    RejectedDuplicateHandle { handle: String },

    /// An edge references a handle not present in the `tasks` list.
    /// Maps to `PLANNER_OUTCOME_REJECTED_CYCLE` (re-uses the "bad graph" bucket).
    RejectedUnknownHandle { handle: String },

    /// The proposed edge set forms a dependency cycle.
    /// Maps to `PLANNER_OUTCOME_REJECTED_CYCLE`.
    /// `cycle` is a representative cycle path expressed as handle names;
    /// the last element is the back-edge target that also appears earlier
    /// in the list, making the loop explicit.
    RejectedCycle { cycle: Vec<String> },

    /// All checks passed; the proposal is ready for the Materializer.
    Valid {
        /// `true` when the planner returned `Confidence::Low`. The proposal
        /// is still materialised (staged), but the attention item should be
        /// escalated in prominence so the operator scrutinises the plan
        /// before releasing.
        low_confidence: bool,
    },
}

// ---------------------------------------------------------------------------
// Validation entry point
// ---------------------------------------------------------------------------

/// Validate a [`PlannerOutput`] proposal before handing it to the Materializer.
///
/// Checks run in order, short-circuiting on the first failure:
///
/// 1. `breakdown_found == false` → [`ValidationResult::NoBreakdown`]
/// 2. `tasks.is_empty()` with `breakdown_found == true` → [`ValidationResult::EmptyBreakdown`]
/// 3. `tasks.len() > max_tasks` → [`ValidationResult::RejectedTooMany`]
/// 4. Duplicate handle in `tasks` → [`ValidationResult::RejectedDuplicateHandle`]
/// 5. Edge references unknown handle → [`ValidationResult::RejectedUnknownHandle`]
/// 6. Edge set contains a cycle → [`ValidationResult::RejectedCycle`]
/// 7. Otherwise → [`ValidationResult::Valid`] (`low_confidence` set when
///    `confidence == Confidence::Low`)
///
/// This function performs no I/O and has no side effects.
pub fn validate(output: &PlannerOutput, max_tasks: usize) -> ValidationResult {
    // 1. No breakdown section in the doc at all.
    if !output.breakdown_found {
        return ValidationResult::NoBreakdown;
    }

    // 2. Breakdown section present but nothing extracted.
    if output.tasks.is_empty() {
        return ValidationResult::EmptyBreakdown;
    }

    // 3. Proposal exceeds the task cap — reject whole, never truncate.
    if output.tasks.len() > max_tasks {
        return ValidationResult::RejectedTooMany {
            count: output.tasks.len(),
            max: max_tasks,
        };
    }

    // 4. Handle uniqueness — every handle must appear exactly once.
    let mut known: HashSet<&str> = HashSet::with_capacity(output.tasks.len());
    for task in &output.tasks {
        if !known.insert(task.handle.as_str()) {
            return ValidationResult::RejectedDuplicateHandle {
                handle: task.handle.clone(),
            };
        }
    }

    // 5. Handle integrity — every edge endpoint must name a known handle.
    for edge in &output.edges {
        if !known.contains(edge.dependent.as_str()) {
            return ValidationResult::RejectedUnknownHandle {
                handle: edge.dependent.clone(),
            };
        }
        if !known.contains(edge.prerequisite.as_str()) {
            return ValidationResult::RejectedUnknownHandle {
                handle: edge.prerequisite.clone(),
            };
        }
    }

    // 6. Acyclicity — the edge set must form a DAG.
    if let Some(cycle) = detect_cycle(&known, &output.edges) {
        return ValidationResult::RejectedCycle { cycle };
    }

    // 7. All checks passed.
    ValidationResult::Valid {
        low_confidence: output.confidence == Confidence::Low,
    }
}

// ---------------------------------------------------------------------------
// Cycle detection (in-memory, handle graph)
// ---------------------------------------------------------------------------

/// Returns `Some(cycle_path)` if the directed graph defined by `edges` over
/// `handles` contains at least one cycle; otherwise returns `None`.
///
/// Edge direction: `dependent → prerequisite` (dependent depends on
/// prerequisite). A cycle exists when following this direction from some
/// handle eventually leads back to itself.
///
/// The returned `cycle_path` is a sequence of handle names where each entry
/// is a prerequisite of the next; the last entry matches an earlier entry,
/// making the cycle explicit (e.g. `["A", "B", "C", "A"]`).
///
/// Uses iterative DFS with three-colour marking (white → gray → black) to
/// avoid call-stack depth limits on large proposals.
fn detect_cycle<'a>(handles: &HashSet<&'a str>, edges: &'a [ProposedEdge]) -> Option<Vec<String>> {
    // Adjacency list: dependent → list of prerequisites it depends on.
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::with_capacity(handles.len());
    for &h in handles {
        adj.entry(h).or_default();
    }
    for edge in edges {
        adj.entry(edge.dependent.as_str())
            .or_default()
            .push(edge.prerequisite.as_str());
    }

    // Three-colour DFS state.
    const WHITE: u8 = 0; // not yet visited
    const GRAY: u8 = 1; // on the current DFS path
    const BLACK: u8 = 2; // fully explored, no cycle through here

    let mut color: HashMap<&str, u8> = handles.iter().map(|&h| (h, WHITE)).collect();
    // `path` mirrors the DFS stack: the sequence of handles on the current
    // path from the DFS root to the node being explored.
    let mut path: Vec<String> = Vec::new();

    for &start in handles {
        if color[start] != WHITE {
            continue;
        }

        // Push the start node and begin iterative DFS.
        color.insert(start, GRAY);
        path.push(start.to_owned());
        // Stack frames: (handle, next-neighbor-index-to-examine).
        let mut stack: Vec<(&str, usize)> = vec![(start, 0)];

        while let Some(frame) = stack.last_mut() {
            let node = frame.0;
            let prereqs: &[&str] = adj.get(node).map(|v| v.as_slice()).unwrap_or(&[]);

            if frame.1 < prereqs.len() {
                let next = prereqs[frame.1];
                frame.1 += 1;

                match color.get(next).copied().unwrap_or(WHITE) {
                    GRAY => {
                        // Back edge — cycle detected.
                        // Find where `next` appears in the current path.
                        let pos = path.iter().position(|s| s.as_str() == next).unwrap_or(0);
                        let mut cycle = path[pos..].to_vec();
                        cycle.push(next.to_owned()); // close the loop
                        return Some(cycle);
                    }
                    WHITE => {
                        color.insert(next, GRAY);
                        path.push(next.to_owned());
                        stack.push((next, 0));
                    }
                    _ => {} // BLACK: already fully explored, no cycle through here
                }
            } else {
                // All neighbors explored for `node`; mark done and retreat.
                color.insert(node, BLACK);
                path.pop();
                stack.pop();
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Oversize-task detection (the decomposition gate)
// ---------------------------------------------------------------------------
//
// See the design brief "Systematic oversized-task detection": T298 ("Full
// national rolling-points PDF detail parse") ran well over an hour in a
// single worker session — a multi-table parse across sections/slots plus a
// projected-impact seed path plus an all-lists validation sweep, shipped as
// one task. The planner's own effort classification even called it "a
// project in disguise" and shipped it anyway. Detection must trigger
// **decomposition**, not just a bigger model.
//
// This is a *per-task* sizing check, distinct from the graph-level
// [`validate`] above: it inspects each [`ProposedTask`]'s scope for signals
// that the task packs more than one reviewable-PR-per-session unit of work.
// The Planner ([`crate::planner`]) runs it on its own output and re-prompts
// for a decomposed graph when it trips (bounded, feedback fed back to the
// model), so oversize proposals are split before they are ever staged.
// Like everything else in this module it is a pure, no-I/O function.

/// A substantive scope description (excluding its trailing
/// `[effort-classification]` audit line) longer than this many characters
/// is almost always a project in disguise — the brief's "if a breakdown
/// item needs a paragraph to describe, it is probably several tasks".
pub const OVERSIZE_DESCRIPTION_CHARS: usize = 600;

/// More than this many distinct *deliverable clauses* in one task's scope
/// (enumerated sub-parts like `(i)…(ii)…(iii)`, or distinct
/// deliverable-verbs such as parse/emit/validate/reconcile) means the task
/// spans multiple phases that should be split with dependency edges.
pub const OVERSIZE_DELIVERABLE_CLAUSES: usize = 2;

/// A scope naming at least this many distinct engine subsystems / module
/// surfaces is a multi-subsystem unit that should be decomposed. Two
/// surfaces is merely `medium` per the effort heuristic; three or more is
/// the oversize signal.
pub const OVERSIZE_SUBSYSTEM_COUNT: usize = 3;

/// One reason a [`ProposedTask`] tripped the decomposition gate. Each
/// variant maps to one of the oversize signals the brief enumerates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OversizeSignal {
    /// The scope or its classification reason uses project-in-disguise
    /// language (the effort heuristic's own "a project in disguise").
    ProjectInDisguise,
    /// The substantive scope description exceeds [`OVERSIZE_DESCRIPTION_CHARS`].
    LongDescription,
    /// The scope enumerates more than [`OVERSIZE_DELIVERABLE_CLAUSES`]
    /// distinct deliverable clauses (multi-phase scope).
    MultipleDeliverables,
    /// The scope names at least [`OVERSIZE_SUBSYSTEM_COUNT`] distinct
    /// subsystems / module surfaces.
    MultiSubsystem,
    /// The scope embeds a fan-out ("validate/sweep/migrate all N X") that
    /// should be its own dependent task.
    FanOut,
}

impl OversizeSignal {
    /// Short kebab-case label for logs and audit lines.
    pub fn label(self) -> &'static str {
        match self {
            OversizeSignal::ProjectInDisguise => "project-in-disguise",
            OversizeSignal::LongDescription => "long-description",
            OversizeSignal::MultipleDeliverables => "multiple-deliverables",
            OversizeSignal::MultiSubsystem => "multi-subsystem",
            OversizeSignal::FanOut => "embedded-fan-out",
        }
    }

    /// One-line explanation of what the signal means and how to fix it —
    /// fed back to the model in the decomposition re-prompt.
    pub fn reason(self) -> &'static str {
        match self {
            OversizeSignal::ProjectInDisguise => {
                "its own classification calls it a project in disguise — split it into dependency-ordered tasks"
            }
            OversizeSignal::LongDescription => {
                "the scope needs a paragraph to describe — if a breakdown item needs a paragraph it is probably several tasks"
            }
            OversizeSignal::MultipleDeliverables => {
                "it enumerates multiple deliverable clauses (parse … and emit … and validate …) — emit each phase as its own task with dependency edges"
            }
            OversizeSignal::MultiSubsystem => {
                "it spans several subsystems — keep each task single-subsystem and single-PR"
            }
            OversizeSignal::FanOut => {
                "it embeds a fan-out (validate/sweep/migrate all N X) — emit that sweep as its own dependent task"
            }
        }
    }
}

/// One oversize task the decomposition gate rejected, with the signals it
/// tripped. `handle`/`name` identify the task so the re-prompt can name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OversizeFinding {
    pub handle: String,
    pub name: String,
    pub signals: Vec<OversizeSignal>,
}

impl OversizeFinding {
    /// Human-readable one-liner naming the task and its tripped signals —
    /// used verbatim in the re-prompt and in logs.
    pub fn describe(&self) -> String {
        let reasons: Vec<&str> = self.signals.iter().map(|s| s.reason()).collect();
        format!("`{}` ({}): {}", self.name, self.handle, reasons.join("; "))
    }
}

/// Distinct engine subsystem / module tokens. Naming three or more of these
/// in one task's scope is the multi-subsystem oversize signal. Matched as
/// whole hyphen-aware words so `cli` does not match `client` and
/// `app-macos` stays one token.
const SUBSYSTEM_TOKENS: &[&str] = &[
    "engine",
    "protocol",
    "cli",
    "cube",
    "bossctl",
    "materializer",
    "planner",
    "populator",
    "kanban",
    "app-macos",
    "macos",
];

/// Deliverable verbs whose co-occurrence signals a multi-phase task. A task
/// doing three or more distinct ones of these is packing several
/// deliverables. Deliberately excludes generic verbs (`add`, `build`,
/// `wire`) that appear in well-sized single-PR tasks.
const DELIVERABLE_VERBS: &[&str] = &[
    "parse",
    "emit",
    "validate",
    "reconcile",
    "map",
    "seed",
    "migrate",
    "sweep",
    "instrument",
    "backfill",
    "ingest",
    "attribute",
    "extract",
];

/// Case-insensitive fan-out markers: an embedded "validate/sweep/migrate all
/// N X" that should be lifted out as its own dependent task.
const FAN_OUT_MARKERS: &[&str] = &[
    "sweep",
    "all lists",
    "all-lists",
    "across all",
    "for each",
    "each of the",
    "for all",
    "all of the",
    "reconcile all",
    "validate all",
    "migrate all",
    "fan-out",
    "fan out",
    "every fixture",
    "all fixtures",
    "-fixture sweep",
];

/// Project-in-disguise phrases, drawn from the effort heuristic's own
/// language (rule 2: "Long scope is almost always a project in disguise").
const PROJECT_IN_DISGUISE_MARKERS: &[&str] = &["project in disguise", "in disguise"];

/// Scan a whole [`PlannerOutput`] and return one [`OversizeFinding`] per task
/// that trips the decomposition gate (empty when every task is well-sized).
///
/// Pure and side-effect-free. The Planner calls this on its own structured
/// output and, when it is non-empty, re-prompts the model to decompose the
/// offending tasks into dependency-ordered, single-subsystem, single-PR
/// units before the proposal is ever staged.
pub fn detect_oversize_tasks(output: &PlannerOutput) -> Vec<OversizeFinding> {
    output
        .tasks
        .iter()
        .enumerate()
        .filter_map(|(i, task)| {
            // The per-task classification line is appended to the
            // description by the prompt contract, but the aligned
            // `effort_audit` entry is the canonical copy; fold both into the
            // haystack so project-in-disguise language is caught wherever it
            // landed.
            let audit = output.effort_audit.get(i).map(String::as_str).unwrap_or("");
            detect_oversize_task(task, audit)
        })
        .collect()
}

/// Detect the oversize signals for a single proposed task. `audit` is the
/// task's aligned `[effort-classification]` line (may be empty).
fn detect_oversize_task(task: &ProposedTask, audit: &str) -> Option<OversizeFinding> {
    let scope = scope_without_audit_line(&task.description);
    let haystack = format!("{scope}\n{audit}").to_lowercase();

    let mut signals = Vec::new();

    if PROJECT_IN_DISGUISE_MARKERS.iter().any(|m| haystack.contains(m)) {
        signals.push(OversizeSignal::ProjectInDisguise);
    }
    if scope.chars().count() > OVERSIZE_DESCRIPTION_CHARS {
        signals.push(OversizeSignal::LongDescription);
    }
    if deliverable_clause_count(&scope) > OVERSIZE_DELIVERABLE_CLAUSES {
        signals.push(OversizeSignal::MultipleDeliverables);
    }
    if distinct_subsystem_count(&haystack) >= OVERSIZE_SUBSYSTEM_COUNT {
        signals.push(OversizeSignal::MultiSubsystem);
    }
    if FAN_OUT_MARKERS.iter().any(|m| haystack.contains(m)) {
        signals.push(OversizeSignal::FanOut);
    }

    if signals.is_empty() {
        return None;
    }
    Some(OversizeFinding {
        handle: task.handle.clone(),
        name: task.name.clone(),
        signals,
    })
}

/// Strip a trailing `[effort-classification] …` audit line (and any blank
/// separator before it) from a task description so the length / clause
/// checks measure the substantive scope, not the boilerplate audit line the
/// prompt contract appends.
fn scope_without_audit_line(description: &str) -> String {
    match description.find("[effort-classification]") {
        Some(idx) => description[..idx].trim_end().to_owned(),
        None => description.trim_end().to_owned(),
    }
}

/// Count the distinct deliverable clauses a scope describes. Takes the
/// larger of two independent estimates: the count of enumerated sub-parts
/// (`(i)`, `(ii)`, …) and the number of distinct [`DELIVERABLE_VERBS`] used.
fn deliverable_clause_count(scope: &str) -> usize {
    let lower = scope.to_lowercase();

    let enumerators = ["(i)", "(ii)", "(iii)", "(iv)", "(v)", "(vi)"]
        .iter()
        .filter(|e| lower.contains(**e))
        .count();

    let words: HashSet<&str> = word_tokens(&lower).collect();
    let distinct_verbs = DELIVERABLE_VERBS.iter().filter(|v| words.contains(**v)).count();

    enumerators.max(distinct_verbs)
}

/// Count the distinct [`SUBSYSTEM_TOKENS`] named in `haystack` (already
/// lowercased). Whole-word, hyphen-aware matching.
fn distinct_subsystem_count(haystack: &str) -> usize {
    let words: HashSet<&str> = word_tokens(haystack).collect();
    SUBSYSTEM_TOKENS.iter().filter(|t| words.contains(**t)).count()
}

/// Split `text` into lowercase-friendly word tokens: maximal runs of
/// ASCII-alphanumeric characters plus `-` (so `app-macos` stays one token).
/// Everything else (whitespace, punctuation) is a separator.
fn word_tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|w| !w.is_empty())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use boss_protocol::{EffortLevel, ProposedTask, TaskKind};

    use super::*;

    // ---- helpers -----------------------------------------------------------

    fn task(handle: &str) -> ProposedTask {
        ProposedTask {
            handle: handle.to_owned(),
            name: format!("Task {handle}"),
            description: "desc".to_owned(),
            kind: TaskKind::ProjectTask,
            effort: EffortLevel::Small,
            ordinal: 0,
        }
    }

    fn edge(dep: &str, pre: &str) -> ProposedEdge {
        ProposedEdge {
            dependent: dep.to_owned(),
            prerequisite: pre.to_owned(),
        }
    }

    fn output_with(
        tasks: Vec<ProposedTask>,
        edges: Vec<ProposedEdge>,
        confidence: Confidence,
        breakdown_found: bool,
    ) -> PlannerOutput {
        PlannerOutput {
            tasks,
            edges,
            confidence,
            breakdown_found,
            notes: String::new(),
            effort_audit: vec![],
        }
    }

    // ---- NoBreakdown -------------------------------------------------------

    #[test]
    fn no_breakdown_when_flag_is_false() {
        let out = output_with(vec![task("t1")], vec![], Confidence::High, false);
        assert_eq!(validate(&out, 30), ValidationResult::NoBreakdown);
    }

    // ---- EmptyBreakdown ----------------------------------------------------

    #[test]
    fn empty_breakdown_when_tasks_empty_and_breakdown_found() {
        let out = output_with(vec![], vec![], Confidence::High, true);
        assert_eq!(validate(&out, 30), ValidationResult::EmptyBreakdown);
    }

    // ---- RejectedTooMany ---------------------------------------------------

    #[test]
    fn rejected_too_many_when_over_cap() {
        let tasks: Vec<_> = (0..5).map(|i| task(&format!("t{i}"))).collect();
        let out = output_with(tasks, vec![], Confidence::High, true);
        assert_eq!(
            validate(&out, 4),
            ValidationResult::RejectedTooMany { count: 5, max: 4 }
        );
    }

    #[test]
    fn not_rejected_when_exactly_at_cap() {
        let tasks: Vec<_> = (0..5).map(|i| task(&format!("t{i}"))).collect();
        let out = output_with(tasks, vec![], Confidence::High, true);
        // Exactly at cap (5 tasks, max_tasks = 5) is valid.
        assert!(matches!(validate(&out, 5), ValidationResult::Valid { .. }));
    }

    // ---- RejectedDuplicateHandle -------------------------------------------

    #[test]
    fn rejected_on_duplicate_handle() {
        let out = output_with(vec![task("alpha"), task("alpha")], vec![], Confidence::High, true);
        assert_eq!(
            validate(&out, 30),
            ValidationResult::RejectedDuplicateHandle {
                handle: "alpha".to_owned()
            }
        );
    }

    // ---- RejectedUnknownHandle ---------------------------------------------

    #[test]
    fn rejected_when_dependent_handle_unknown() {
        let out = output_with(
            vec![task("schema"), task("engine")],
            vec![edge("ghost", "schema")],
            Confidence::High,
            true,
        );
        assert_eq!(
            validate(&out, 30),
            ValidationResult::RejectedUnknownHandle {
                handle: "ghost".to_owned()
            }
        );
    }

    #[test]
    fn rejected_when_prerequisite_handle_unknown() {
        let out = output_with(
            vec![task("schema"), task("engine")],
            vec![edge("engine", "ghost")],
            Confidence::High,
            true,
        );
        assert_eq!(
            validate(&out, 30),
            ValidationResult::RejectedUnknownHandle {
                handle: "ghost".to_owned()
            }
        );
    }

    // ---- RejectedCycle -----------------------------------------------------

    #[test]
    fn rejected_cycle_simple_two_node() {
        // A depends on B, B depends on A.
        let out = output_with(
            vec![task("a"), task("b")],
            vec![edge("a", "b"), edge("b", "a")],
            Confidence::High,
            true,
        );
        assert!(matches!(validate(&out, 30), ValidationResult::RejectedCycle { .. }));
    }

    #[test]
    fn rejected_cycle_three_node_ring() {
        // A → B → C → A (each depends on the next).
        let out = output_with(
            vec![task("a"), task("b"), task("c")],
            vec![edge("a", "b"), edge("b", "c"), edge("c", "a")],
            Confidence::High,
            true,
        );
        let result = validate(&out, 30);
        match result {
            ValidationResult::RejectedCycle { cycle } => {
                // Cycle must be non-empty and the last element must repeat an
                // earlier one (closing the loop).
                assert!(cycle.len() >= 2);
                let last = cycle.last().unwrap();
                assert!(cycle[..cycle.len() - 1].contains(last));
            }
            other => panic!("expected RejectedCycle, got {other:?}"),
        }
    }

    #[test]
    fn rejected_cycle_self_loop() {
        // A depends on itself.
        let out = output_with(vec![task("a")], vec![edge("a", "a")], Confidence::High, true);
        assert!(matches!(validate(&out, 30), ValidationResult::RejectedCycle { .. }));
    }

    // ---- Valid -------------------------------------------------------------

    #[test]
    fn valid_dag_single_task_no_edges() {
        let out = output_with(vec![task("schema")], vec![], Confidence::High, true);
        assert_eq!(validate(&out, 30), ValidationResult::Valid { low_confidence: false });
    }

    #[test]
    fn valid_dag_linear_chain() {
        // schema → engine → integration (schema is prerequisite of engine,
        // engine is prerequisite of integration).
        let out = output_with(
            vec![task("schema"), task("engine"), task("integration")],
            vec![edge("engine", "schema"), edge("integration", "engine")],
            Confidence::Medium,
            true,
        );
        assert_eq!(validate(&out, 30), ValidationResult::Valid { low_confidence: false });
    }

    #[test]
    fn valid_dag_fan_out() {
        // schema is prerequisite for both engine and cli; engine and cli
        // are independent (no edge between them).
        let out = output_with(
            vec![task("schema"), task("engine"), task("cli")],
            vec![edge("engine", "schema"), edge("cli", "schema")],
            Confidence::High,
            true,
        );
        assert_eq!(validate(&out, 30), ValidationResult::Valid { low_confidence: false });
    }

    // ---- Low confidence ----------------------------------------------------

    #[test]
    fn valid_with_low_confidence_flag_set() {
        let out = output_with(vec![task("t1")], vec![], Confidence::Low, true);
        assert_eq!(validate(&out, 30), ValidationResult::Valid { low_confidence: true });
    }

    #[test]
    fn valid_medium_confidence_not_flagged() {
        let out = output_with(vec![task("t1")], vec![], Confidence::Medium, true);
        assert_eq!(validate(&out, 30), ValidationResult::Valid { low_confidence: false });
    }

    // ---- Ordering: breakdown_found takes priority over empty tasks ---------

    #[test]
    fn no_breakdown_takes_priority_over_empty_tasks() {
        // Even if tasks is empty, breakdown_found = false wins.
        let out = output_with(vec![], vec![], Confidence::High, false);
        assert_eq!(validate(&out, 30), ValidationResult::NoBreakdown);
    }

    // ---- Ordering: cap check before handle checks --------------------------

    #[test]
    fn too_many_takes_priority_over_duplicate_handles() {
        // 3 tasks (max_tasks = 2), two of which share a handle.
        let tasks = vec![task("a"), task("b"), task("a")];
        let out = output_with(tasks, vec![], Confidence::High, true);
        // Should hit RejectedTooMany before RejectedDuplicateHandle.
        assert_eq!(
            validate(&out, 2),
            ValidationResult::RejectedTooMany { count: 3, max: 2 }
        );
    }

    // ---- Decomposition gate: detect_oversize_tasks -------------------------

    /// Build a proposed task with an explicit description, so sizing tests
    /// can craft the exact scope prose they need. Effort/kind are irrelevant
    /// to the gate, which reads only the scope.
    fn sized_task(handle: &str, description: &str) -> ProposedTask {
        ProposedTask {
            handle: handle.to_owned(),
            name: format!("Task {handle}"),
            description: description.to_owned(),
            kind: TaskKind::ProjectTask,
            effort: EffortLevel::Small,
            ordinal: 0,
        }
    }

    fn output_of(tasks: Vec<ProposedTask>) -> PlannerOutput {
        output_with(tasks, vec![], Confidence::High, true)
    }

    #[test]
    fn well_sized_tasks_produce_no_findings() {
        // A schema task and a two-subsystem handler task — both the healthy
        // single-PR shape. Neither trips any signal (two subsystems is
        // `medium`, not oversize; short scope; one deliverable).
        let out = output_of(vec![
            sized_task("schema", "Add the contract types to boss-protocol."),
            sized_task("handler", "Wire the engine RPC handler against the protocol types."),
        ]);
        assert!(
            detect_oversize_tasks(&out).is_empty(),
            "well-sized tasks must not trip the decomposition gate",
        );
    }

    #[test]
    fn project_in_disguise_language_trips() {
        // The T298 tell: the effort classification's own reason. Here it
        // rides in the aligned effort_audit entry, not the description, to
        // prove both are scanned.
        let mut out = output_of(vec![sized_task("t298", "Parse the detail tables.")]);
        out.effort_audit = vec![
            "[effort-classification] level=`large` matched-rule=`rule 2` reasons=\"a project in disguise\"".to_owned(),
        ];
        let findings = detect_oversize_tasks(&out);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].signals.contains(&OversizeSignal::ProjectInDisguise));
    }

    #[test]
    fn long_paragraph_scope_trips_long_description() {
        // A paragraph of scope (> OVERSIZE_DESCRIPTION_CHARS) is a project
        // in disguise per the brief.
        let long = "word ".repeat(OVERSIZE_DESCRIPTION_CHARS); // ~5x the threshold
        let out = output_of(vec![sized_task("big", &long)]);
        let findings = detect_oversize_tasks(&out);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].signals.contains(&OversizeSignal::LongDescription));
    }

    #[test]
    fn multi_deliverable_clauses_trip() {
        // Parse … and emit … and validate … and reconcile … — four distinct
        // deliverable verbs, a multi-phase task.
        let out = output_of(vec![sized_task(
            "multi",
            "Parse the tables, emit the slot mapping, validate the fixtures, and reconcile against source.",
        )]);
        let findings = detect_oversize_tasks(&out);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].signals.contains(&OversizeSignal::MultipleDeliverables));
    }

    #[test]
    fn enumerated_subparts_trip_multi_deliverable() {
        // Roman-enumerated sub-parts (i)…(ii)…(iii) count as clauses even
        // without the deliverable verbs.
        let out = output_of(vec![sized_task(
            "phased",
            "Do the work in phases: (i) the reader, (ii) the writer, and (iii) the checker.",
        )]);
        let findings = detect_oversize_tasks(&out);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].signals.contains(&OversizeSignal::MultipleDeliverables));
    }

    #[test]
    fn three_subsystems_trip_multi_subsystem() {
        // engine + cli + protocol = three surfaces → oversize. (Two would be
        // merely `medium` and must NOT trip — covered below.)
        let out = output_of(vec![sized_task(
            "wide",
            "Thread the flag through the engine, the cli, and the protocol crate.",
        )]);
        let findings = detect_oversize_tasks(&out);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].signals.contains(&OversizeSignal::MultiSubsystem));
    }

    #[test]
    fn two_subsystems_do_not_trip_multi_subsystem() {
        // engine + protocol is the common `medium` shape — never oversize on
        // subsystem count alone.
        let out = output_of(vec![sized_task(
            "medium",
            "Add the engine handler for the new protocol message.",
        )]);
        let findings = detect_oversize_tasks(&out);
        assert!(
            findings.is_empty(),
            "two subsystems is medium, not oversize: {findings:?}",
        );
    }

    #[test]
    fn subsystem_tokens_match_whole_words_only() {
        // `cli` must not match `client`; `map` (a deliverable verb) must not
        // match `mapper`. A scope full of near-miss words stays well-sized.
        let out = output_of(vec![sized_task(
            "nearmiss",
            "The client calls the mapper to render the applied template.",
        )]);
        assert!(
            detect_oversize_tasks(&out).is_empty(),
            "near-miss substrings must not trip whole-word signals",
        );
    }

    #[test]
    fn fan_out_marker_trips() {
        // The all-lists sweep — embedded fan-out that should be its own task.
        let out = output_of(vec![sized_task(
            "sweep",
            "Reconcile the parser against the all-lists corpus and run the fixture sweep.",
        )]);
        let findings = detect_oversize_tasks(&out);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].signals.contains(&OversizeSignal::FanOut));
    }

    #[test]
    fn only_the_oversize_task_is_flagged_in_a_mixed_proposal() {
        // A well-sized schema task alongside a T298-shaped monolith: exactly
        // one finding, naming the offending handle.
        let out = output_of(vec![
            sized_task("schema", "Add the contract types."),
            sized_task(
                "t298",
                "Parse the multi-table PDF across sections and slots, emit the event-type slot mapping, \
                 seed the projected_impact path, and validate every fixture in the all-lists sweep.",
            ),
        ]);
        let findings = detect_oversize_tasks(&out);
        assert_eq!(findings.len(), 1, "only the oversize task should be flagged");
        assert_eq!(findings[0].handle, "t298");
        // Its description names the offending signals for the re-prompt.
        let described = findings[0].describe();
        assert!(
            described.contains("t298"),
            "describe() must name the handle: {described}"
        );
    }

    #[test]
    fn audit_line_is_excluded_from_the_length_check() {
        // A short scope whose only length comes from the appended
        // [effort-classification] line must NOT trip LongDescription — the
        // audit boilerplate is stripped before measuring.
        let padding = "x".repeat(OVERSIZE_DESCRIPTION_CHARS);
        let desc = format!(
            "Add the schema types.\n\n[effort-classification] level=`small` matched-rule=`rule 5` reasons=\"{padding}\""
        );
        let out = output_of(vec![sized_task("schema", &desc)]);
        let findings = detect_oversize_tasks(&out);
        assert!(
            findings
                .iter()
                .all(|f| !f.signals.contains(&OversizeSignal::LongDescription)),
            "the [effort-classification] line must be excluded from the scope-length check: {findings:?}",
        );
    }
}
