//! Verb catalogue and the 2.5 question criteria, in the brief's own words.

use crate::packet::VerbBudget;

/// Remaining budget lines the catalogue section prints.
pub fn render_budget(budget: &VerbBudget) -> String {
    let remaining = budget.create_max.saturating_sub(budget.create_used);
    format!(
        "Remaining create budget: {remaining} of {} revisions-plus-precursors this window ({}s).\n\
         Remaining redispatch allowance: {}. At most one progression \
         redispatch per item per stuck kind until a non-progression work run completes.",
        budget.create_max, budget.create_window_secs, budget.redispatch_remaining
    )
}

/// The 2.4 verb table, as the agent reads it.
pub fn render_verb_catalogue(budget: &VerbBudget) -> String {
    format!(
        "## Verb catalogue\n\
         \n\
         You mutate only through `boss propose`. Direct taxonomy requests are refused.\n\
         \n\
         - `revise --brief '<text>'` — create a revision on this item's open PR. Counts against the create budget. One open progression revision per PR. Refused if the PR is closed.\n\
         - `precursor --name … --brief '<text>'` — create a chore in the same product and make this item depend on it. Canonical use: a red required check that is unrelated to this PR or flaky. Counts against the create budget. At most one open progression-created precursor per item.\n\
         - `depend --on <id>` / `undepend --on <id>` — add or remove a dependency edge in the neighbourhood. `undepend` only when the prerequisite is terminal, archived, or a hidden revision with no live execution and a green parent PR.\n\
         - `redispatch [--brief-addendum '<text>']` — explicit start. Clears `churn_guard`, `deliberate_park` (and its attention), and non-operator-No `worker_failed`. Refused with a live execution, `awaiting_operator_answer`, or operator-No `worker_failed`.\n\
         - `clear-signal --reason <r>` — clear a `blocked_reason` whose probe is already clear (for example `merge_conflict` while GitHub reports MERGEABLE). Refused when the probe disagrees.\n\
         - `archive-revision <id> --reason …` — archive a hidden revision of this chain that has no live execution and whose parent PR is green. Never the item itself.\n\
         - `unblock` — move the item `blocked → todo`, or `in_review` when it already owns a bound PR, after its block reason is cleared.\n\
         - `followup-task --name … --description '<text>' --rationale '<why>'` — existing non-blocking follow-up.\n\
         - `done --outcome blocked --question …` — ask the owner. Required `--criterion`. A bare `blocked` without a question is refused. Ends the run as `asked`.\n\
         - `done --outcome delivered|no-changes-needed --summary …` — end the run as `progressed` or `no_action`. There is no PR to resolve. A run with no declaration fails as `no disposition`.\n\
         \n\
         Human-only (refused): park/unpark; archive or delete anything other than a chain revision; priority; dispatch pause/resume; merging; stopping or reaping workers; releasing leases; answering operator questions; editing other items' docs; changing effort, model, driver or reasoning; anything outside the neighbourhood.\n\
         \n\
         {}\n",
        render_budget(budget)
    )
}

/// Design 2.5 "When a question is permitted", in the brief's own words.
pub fn render_question_criteria() -> &'static str {
    "## When you may ask the owner\n\
     \n\
     Human escalations are rare and reserved for a progression agent that is truly blocked. You may ask (`done --outcome blocked --question`) only when one of these holds:\n\
     \n\
     1. The action the item needs is on the human-only list above (park, archive the item itself, change priority, merge, reap a worker, answer an open question).\n\
     2. Every direct verb that applies to the item's stuck kind has been tried or is refused (fingerprint, redispatch bound, budget), which the progression-history section shows.\n\
     3. A worker's declared blocker, or the item's own brief and design doc, names a decision only the owner can make, and you cannot resolve it from the recorded state, the docs, or the PR.\n\
     4. The evidence contradicts itself in a way that changes product behaviour (two dependents want incompatible things; a precursor's purpose no longer matches the item), so any direct action would be a guess.\n\
     \n\
     Do not ask to confirm an action you may take directly. Do not ask \"should I keep trying\" (the engine files that question after two attempts). Do not restate a worker's free-text blocker as a question without first classifying it. `no-changes-needed` is refused for a declared-blocker item: either redispatch with the evidence the blocker is stale, or ask a specific question.\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> VerbBudget {
        VerbBudget {
            create_used: 0,
            create_max: 2,
            create_window_secs: 3600,
            redispatch_remaining: 1,
        }
    }

    #[test]
    fn followup_task_line_names_the_cli_flags() {
        let catalogue = render_verb_catalogue(&budget());
        let line = catalogue
            .lines()
            .find(|line| line.contains("`followup-task"))
            .expect("catalogue teaches followup-task");
        assert!(line.contains("--description"), "{line}");
        assert!(line.contains("--rationale"), "{line}");
        assert!(
            !line.contains("--brief"),
            "followup-task takes --description, not --brief: {line}"
        );
    }
}
