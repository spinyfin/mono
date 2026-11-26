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
         - `revise --brief '<text>'` — create a revision on this item's open PR. Counts against the create budget. One unfinished progression revision per PR; delivered revisions in review do not count. Refused if the PR is closed.\n\
         - `precursor --name … --brief '<text>'` — create a chore in the same product and make this item depend on it. Canonical use: a red required check that is unrelated to this PR or flaky. Counts against the create budget. At most one open progression-created precursor per item.\n\
         - `depend --on <id>` / `undepend --on <id>` — add or remove a dependency edge in the neighbourhood. `undepend` only when the prerequisite is terminal, archived, or a hidden revision with no live execution and a green parent PR.\n\
         - `redispatch [--brief-addendum '<text>']` — explicit start. Clears `churn_guard`, `deliberate_park` (and its attention), and non-operator-No `worker_failed`. Refused with a live execution, `awaiting_operator_answer`, `question_dismissed`, or operator-No `worker_failed`.\n\
         - `clear-signal --reason <r>` — clear a `blocked_reason` whose probe is already clear (for example `merge_conflict` while GitHub reports MERGEABLE). Refused when the probe disagrees.\n\
         - `archive-revision <id> --reason …` — archive a hidden revision of this chain with no live or pending execution when it has delivered (in review or its latest implementation completed), or its parent PR is green. Never the item itself.\n\
         - `unblock` — move the item `blocked → todo`, or `in_review` when it already owns a bound PR, after its block reason is cleared.\n\
         - `followup-task --name … --description '<text>' --rationale '<why>'` — existing non-blocking follow-up.\n\
         - `done --outcome blocked --question …` — ask the owner. Required `--criterion`. A bare `blocked` without a question is refused. Ends the run as `asked`. Bind the board change an answer means with `--on-answer '<answer>=<change>'` (see \"When you may ask the owner\"): the engine makes it when the owner picks that answer.\n\
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
     The owner decides; the engine acts. A question is never a request for the owner to change the board: do not ask them to delete, archive, start, move or un-block a row, to remove a dependency, or to run a command, and do not word an option as the owner promising to do it (\"I will delete the row\"). A question worded that way is refused. Say what each answer does, and bind the board change it means to that answer. The engine makes the change the moment the owner picks it, or refuses the answer and tells the owner why:\n\
     \n\
     - `--on-answer 'yes=archive-revision <id>'` closes a revision of this item's PR chain and releases whatever waits on it. Unlike your own `archive-revision` it needs no proof the revision is dead: the owner's answer is the decision.\n\
     - `--on-answer 'no=start <id>'` starts a row in your neighbourhood, as an explicit start by the owner.\n\
     - `--on-answer '<option label>=undepend <dependent-id> <prerequisite-id>'` drops a dependency edge between two rows in your neighbourhood.\n\
     - The option labels `Park this item` and `Archive this item` need no binding: the engine carries them out.\n\
     \n\
     Use `yes` or `no` for a yes-no question and the option label, verbatim, for a multiple-choice one; repeat `--on-answer` to bind more than one answer. A question under criterion 1 or 2 asks for an action, so at least one of its answers must be one the engine carries out (a bound change, one of those two labels, or a create-prerequisite or link-prerequisite question). Without one it is refused: no answer could change anything, and the next run would be refused exactly as you were. Changing priority, merging a pull request, stopping a live worker, and answering someone else's question are not `--on-answer` changes, so do not ask the owner to do them: end the run and say what is needed in `--summary`. Criteria 3 and 4 are decisions: the answer reaches the next run, which acts on it with the verbs above.\n\
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

    /// The brief is where the agent learns that an answer is carried out by
    /// the engine. Without it the agent asks the owner to make the change.
    #[test]
    fn question_criteria_teach_binding_a_change_instead_of_asking_the_owner_to_make_it() {
        let criteria = render_question_criteria();
        for taught in [
            "The owner decides; the engine acts.",
            "--on-answer 'yes=archive-revision <id>'",
            "--on-answer 'no=start <id>'",
            "undepend <dependent-id> <prerequisite-id>",
            "`Park this item` and `Archive this item`",
            "A question under criterion 1 or 2 asks for an action",
            "create-prerequisite or link-prerequisite question",
            "Changing priority, merging a pull request, stopping a live worker",
        ] {
            assert!(criteria.contains(taught), "the criteria must teach: {taught}");
        }
        let done = render_verb_catalogue(&budget());
        let line = done
            .lines()
            .find(|line| line.contains("`done --outcome blocked"))
            .expect("catalogue teaches asking");
        assert!(line.contains("--on-answer"), "{line}");
    }

    #[test]
    fn redispatch_line_refuses_a_dismissed_question() {
        let catalogue = render_verb_catalogue(&budget());
        let line = catalogue
            .lines()
            .find(|line| line.contains("`redispatch"))
            .expect("catalogue teaches redispatch");
        assert!(
            line.contains("`question_dismissed`"),
            "redispatch must name the dismissed-question hold: {line}"
        );
    }
}
