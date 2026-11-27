//! Terminal run-done declaration directive (`run_done_proposals_seam`) — both
//! the control text (byte-pinned by existing prompt tests) and the treated
//! kind-gated variant for `slim_worker_brief`.

use crate::prompt_fragments::{
    execution_kind_has_bound_pr, execution_kind_has_build_gate, execution_kind_supports_file_count_protocol,
    execution_kind_supports_multiple_choice_questions, execution_kind_supports_no_op_outcome,
    execution_kind_supports_prerequisite_questions,
};
use boss_protocol::ExecutionKind;

#[derive(bon::Builder)]
pub(super) struct RunDoneParams<'a> {
    pub seam_enabled: bool,
    pub worker_signal_seam_enabled: bool,
    pub conflict_resolution: bool,
    pub standard_worker: bool,
    pub execution_kind: &'a ExecutionKind,
    pub existing_pr_url: bool,
    pub bazel_workspace: Option<bool>,
    pub slim_worker_brief: bool,
}

/// Dispatch between the control [`run_done_directive`] and the treated
/// kind-gated variant when `slim_worker_brief` is enabled for a Standard
/// worker.
pub(super) fn render(params: RunDoneParams<'_>) -> String {
    if !params.seam_enabled {
        return String::new();
    }
    if params.slim_worker_brief && params.standard_worker {
        run_done_directive_slim(&params)
    } else {
        run_done_directive(
            params.seam_enabled,
            params.worker_signal_seam_enabled,
            params.conflict_resolution,
            params.standard_worker,
            params.bazel_workspace,
        )
    }
}

/// Validation-failure / gate-exception paragraph for [`run_done_directive`].
/// Built from named pieces chosen by the resolved build system so a Bazel
/// rewording cannot silently leave Bazel commands in a non-Bazel prompt,
/// and an unresolved probe is not collapsed to non-Bazel.
fn run_done_gate_exception(conflict_resolution: bool, bazel_workspace: Option<bool>) -> &'static str {
    match (conflict_resolution, bazel_workspace) {
        (true, Some(true)) => {
            "The merge-correctness pre-push gate is not covered by any unattributable-failure exception. \
             If `bazel build` fails or times out, do NOT push: the merged code MUST COMPILE. \
             A still-red compile is a reason to stop without delivery, not a reason to push, even if the \
             failure looks pre-existing or environmental. Only the full test suite is deferred to CI; \
             a successful `bazel build` and a passing run of the tests affected by the conflicted files are preconditions for this push. Bazel \
             remains the source of truth: do not bypass or weaken checks, and still stop for required \
             approval to relax a check, missing credentials or authorization, genuine instruction \
             conflicts or contradictory requirements."
        }
        (true, Some(false)) => {
            "The merge-correctness pre-push gate is not covered by any unattributable-failure exception. \
             If the repo's documented compile, type-check or test command fails or times out, do NOT push: \
             the merged code MUST compile, type-check, and pass those tests. \
             A still-red command is a reason to stop without delivery, not a reason to push, even if the \
             failure looks pre-existing or environmental. A successful run of those documented commands is \
             a precondition for this push. The repo's own checks remain the source of truth: do not \
             bypass or weaken checks, and still stop for required approval to relax a check, missing \
             credentials or authorization, genuine instruction conflicts or contradictory requirements."
        }
        (true, None) => {
            "The merge-correctness pre-push gate is not covered by any unattributable-failure exception. \
             If the discovered compile, type-check or test command fails or times out, do NOT push: \
             discover the build system on this execution host first, then apply the matching merge-correctness policy. \
             A still-red command is a reason to stop without delivery, not a reason to push, even if the \
             failure looks pre-existing or environmental. A successful run of the matching commands is \
             a precondition for this push. Those checks remain the source of truth: do not \
             bypass or weaken checks, and still stop for required approval to relax a check, missing \
             credentials or authorization, genuine instruction conflicts or contradictory requirements."
        }
        (false, Some(true)) => {
            "For validation failures, this is a narrow exception to the earlier gate/stop rules, including \
             the pre-push Bazel gate: a failing gate warrants stopping without delivery only when it is \
             reproducible AND attributable to your change — fix it; if attribution remains unresolved, \
             the failure still stands. A Bazel test TIMEOUT result, a full-suite failure that passes in \
             isolation, or a failure in untouched code is not by itself a reason to stop or to refuse the push: retry the \
             specific Bazel target (two or three attempts maximum), and if it keeps failing, check whether \
             it reproduces on an unmodified base revision. Separately, a shell-timeout of `bazel` or `checkleft run` is \
             not a reason to rerun the same command under the same cap — keep polling a still-running \
             command instead, and do not wrap those commands in `timeout`. You MAY push despite a still-red target, and \
             declare `delivered`, only with evidence that the failure is pre-existing or environmental (a \
             base reproduction, a passing rerun/isolation run, or a concrete code-path argument showing why \
             your change cannot cause it), recording the target, failure, attempts and evidence in the \
             PR body; merely calling it flaky is not evidence. Bazel remains the source of truth: do not \
             bypass or weaken checks, and still stop for required approval to relax a check, missing \
             credentials or authorization, genuine instruction conflicts or contradictory requirements."
        }
        (false, Some(false)) => {
            "For validation failures, this is a narrow exception to the earlier gate/stop rules, including \
             the pre-push build gate: a failing gate warrants stopping without delivery only when it is \
             reproducible AND attributable to your change — fix it; if attribution remains unresolved, \
             the failure still stands. A test TIMEOUT result, a full-suite failure that passes in \
             isolation, or a failure in untouched code is not by itself a reason to stop or to refuse the push: retry the \
             specific failing command (two or three attempts maximum), and if it keeps failing, check whether \
             it reproduces on an unmodified base revision. Separately, a shell-timeout of a build, test or `checkleft run` command is \
             not a reason to rerun the same command under the same cap — keep polling a still-running \
             command instead, and do not wrap those commands in `timeout`. You MAY push despite a still-red target, and \
             declare `delivered`, only with evidence that the failure is pre-existing or environmental (a \
             base reproduction, a passing rerun/isolation run, or a concrete code-path argument showing why \
             your change cannot cause it), recording the target, failure, attempts and evidence in the \
             PR body; merely calling it flaky is not evidence. The repo's own checks remain the source of truth: do not \
             bypass or weaken checks, and still stop for required approval to relax a check, missing \
             credentials or authorization, genuine instruction conflicts or contradictory requirements."
        }
        (false, None) => {
            "For validation failures, this is a narrow exception to the earlier gate/stop rules, including \
             the pre-push build gate: a failing gate warrants stopping without delivery only when it is \
             reproducible AND attributable to your change — fix it; if attribution remains unresolved, \
             the failure still stands. A test TIMEOUT result, a full-suite failure that passes in \
             isolation, or a failure in untouched code is not by itself a reason to stop or to refuse the push: retry the \
             specific failing command (two or three attempts maximum), and if it keeps failing, check whether \
             it reproduces on an unmodified base revision. Separately, a shell-timeout of a build, test or `checkleft run` command is \
             not a reason to rerun the same command under the same cap — keep polling a still-running \
             command instead, and do not wrap those commands in `timeout`. You MAY push despite a still-red target, and \
             declare `delivered`, only with evidence that the failure is pre-existing or environmental (a \
             base reproduction, a passing rerun/isolation run, or a concrete code-path argument showing why \
             your change cannot cause it), recording the target, failure, attempts and evidence in the \
             PR body; merely calling it flaky is not evidence. The checks you discovered on this execution host remain the source of truth: do not \
             bypass or weaken checks, and still stop for required approval to relax a check, missing \
             credentials or authorization, genuine instruction conflicts or contradictory requirements."
        }
    }
}

/// Slim validation-failure / gate-exception paragraph for treated build-gate
/// runs: keeps every attribution, retry, base-reproduction, and evidence rule
/// without repeating the no-`timeout` or check-bypass clauses that already live
/// in the worker rules.
fn run_done_gate_exception_slim(conflict_resolution: bool, bazel_workspace: Option<bool>) -> &'static str {
    match (conflict_resolution, bazel_workspace) {
        (true, Some(true)) => {
            "The merge-correctness pre-push gate has no unattributable-failure exception. \
             If `bazel build` fails or times out, do NOT push: the merged code MUST COMPILE, \
             even if the failure looks pre-existing or environmental. Only the full test suite \
             is deferred to CI; a successful `bazel build` and a passing run of the tests affected \
             by the conflicted files are preconditions for this push. Still stop for missing \
             credentials or authorization, genuine instruction conflicts, or contradictory requirements."
        }
        (true, Some(false)) => {
            "The merge-correctness pre-push gate has no unattributable-failure exception. \
             If the repo's documented compile, type-check or test command fails or times out, do NOT push: \
             the merged code MUST compile, type-check, and pass those tests, even if the failure looks \
             pre-existing or environmental. Still stop for missing credentials or authorization, \
             genuine instruction conflicts, or contradictory requirements."
        }
        (true, None) => {
            "The merge-correctness pre-push gate has no unattributable-failure exception. \
             Discover the build system on this execution host first, then apply the matching \
             merge-correctness policy: if the compile, type-check or test command fails or times out, \
             do NOT push, even if the failure looks pre-existing or environmental. Still stop for \
             missing credentials or authorization, genuine instruction conflicts, or contradictory requirements."
        }
        (false, Some(true)) => {
            "For validation failures, this is a narrow exception to the earlier gate/stop rules, including \
             the pre-push Bazel gate: a failing gate warrants stopping without delivery only when it is \
             reproducible AND attributable to your change — fix it; if attribution remains unresolved, \
             the failure still stands. A Bazel test TIMEOUT result, a full-suite failure that passes in \
             isolation, or a failure in untouched code is not by itself a reason to stop or to refuse the push: \
             retry the specific Bazel target (two or three attempts maximum), and if it keeps failing, check \
             whether it reproduces on an unmodified base revision. You MAY push despite a still-red target, and \
             declare `delivered`, only with evidence that the failure is pre-existing or environmental (a \
             base reproduction, a passing rerun/isolation run, or a concrete code-path argument showing why \
             your change cannot cause it), recording the target, failure, attempts and evidence in the \
             PR body; merely calling it flaky is not evidence. Bazel remains the source of truth; still stop \
             for missing credentials or authorization, genuine instruction conflicts, or contradictory requirements."
        }
        (false, Some(false)) => {
            "For validation failures, this is a narrow exception to the earlier gate/stop rules, including \
             the pre-push build gate: a failing gate warrants stopping without delivery only when it is \
             reproducible AND attributable to your change — fix it; if attribution remains unresolved, \
             the failure still stands. A test TIMEOUT result, a full-suite failure that passes in \
             isolation, or a failure in untouched code is not by itself a reason to stop or to refuse the push: \
             retry the specific failing command (two or three attempts maximum), and if it keeps failing, check \
             whether it reproduces on an unmodified base revision. You MAY push despite a still-red target, and \
             declare `delivered`, only with evidence that the failure is pre-existing or environmental (a \
             base reproduction, a passing rerun/isolation run, or a concrete code-path argument showing why \
             your change cannot cause it), recording the target, failure, attempts and evidence in the \
             PR body; merely calling it flaky is not evidence. The repo's own checks remain the source of truth; \
             still stop for missing credentials or authorization, genuine instruction conflicts, or contradictory requirements."
        }
        (false, None) => {
            "For validation failures, this is a narrow exception to the earlier gate/stop rules, including \
             the pre-push build gate: a failing gate warrants stopping without delivery only when it is \
             reproducible AND attributable to your change — fix it; if attribution remains unresolved, \
             the failure still stands. A test TIMEOUT result, a full-suite failure that passes in \
             isolation, or a failure in untouched code is not by itself a reason to stop or to refuse the push: \
             retry the specific failing command (two or three attempts maximum), and if it keeps failing, check \
             whether it reproduces on an unmodified base revision. You MAY push despite a still-red target, and \
             declare `delivered`, only with evidence that the failure is pre-existing or environmental (a \
             base reproduction, a passing rerun/isolation run, or a concrete code-path argument showing why \
             your change cannot cause it), recording the target, failure, attempts and evidence in the \
             PR body; merely calling it flaky is not evidence. The checks you discovered on this execution host \
             remain the source of truth; still stop for missing credentials or authorization, genuine \
             instruction conflicts, or contradictory requirements."
        }
    }
}

/// Control terminal run-done declaration directive — byte-identical to the
/// pre-experiment text.
fn run_done_directive(
    seam_enabled: bool,
    worker_signal_seam_enabled: bool,
    conflict_resolution: bool,
    standard_worker: bool,
    bazel_workspace: Option<bool>,
) -> String {
    if !seam_enabled {
        return String::new();
    }
    let boss = boss_engine_worker_bin::WORKER_BOSS_INVOCATION;
    let (blocked_file, blocked_while_continuing) = if worker_signal_seam_enabled {
        let question_carve_out = if standard_worker {
            ", unless the declaration carries a question as described below"
        } else {
            ""
        };
        (
            format!(
                "File `{boss} propose blocked --reason \"...\"` alongside it (before this call){question_carve_out}"
            ),
            format!("`{boss} propose blocked --reason \"...\"` alone records"),
        )
    } else {
        (
            "Put the blocker reason in this call's `--summary` (a `[blocked] reason=\"...\"` marker \
             emitted immediately before this call is never parsed — the marker is Stop-boundary-only)"
                .to_string(),
            "a `[blocked] reason=\"...\"` marker is parsed only at the Stop boundary (when the turn \
             ends without a terminal `propose done`) and records"
                .to_string(),
        )
    };
    let gate_exception = run_done_gate_exception(conflict_resolution, bazel_workspace);
    let operator_question_block = if standard_worker {
        let question_companion = if worker_signal_seam_enabled {
            format!(
                " A declaration that carries a question is its own blocker record: do NOT also file \
                 `{boss} propose blocked`, because that raises a separate attention item the answer never clears."
            )
        } else {
            String::new()
        };
        format!(
            "When delivery requires a human decision, attach one specific Yes/No question to that blocked declaration. Write the labelled explanation to a file first, then pass `--explanation-file <path>` on a single line (`--explanation-file -` or `--explanation -` reads stdin). Inline `--explanation` is accepted when the command can contain newlines; some PreToolUse guards reject any command string that contains a newline, even inside quotes.\n\n\
     ```\n\
     {boss} propose done --outcome blocked --summary \"<blocker>\" --question \"Approve switching the retry to exponential backoff?\" --answer-type yes-no --explanation-file /tmp/explanation.txt\n\
     ```\n\n\
     Say in plain words what is blocked and what each answer will do; put check names and numbers in Details. \"Can I proceed?\" is not specific enough. Put the justification in the explanation file, not the question.{question_companion} One question per run: ask the most blocking decision and disclose any further decisions in the explanation. The run waits durably for the answer. Yes records the approval and redispatches. No records the declined path (granting no authorization) and redispatches so the next worker takes it. Both Yes and No must name an actionable path the engine or the next worker will take; \"No = remain blocked\" is not a real choice. If there is only one viable path, do not ask; proceed, or report the blocker without a question.\n\n\
     Follow the question-writing contract in the generated worker rules.\n\n\
     **When the decision is a choice among several options rather than approve/decline, use `--answer-type multiple-choice`.** Pass two to six short, distinct, single-line labels with `--choices`; the card shows one button per label. Use `--question` for the decision itself and an explanation with only `Context:` (say what each option leads to), `Recommendation:` and optional `Details:` — no `Yes:` or `No:` sections, which are rejected for this type. Example:\n\n\
     ```\n\
     {boss} propose done --outcome blocked --summary \"<blocker>\" --question \"Which rollout should the migration use?\" --answer-type multiple-choice --choices \"Gradual\" \"All at once\" --explanation-file /tmp/explanation.txt\n\
     ```\n\n\
     The same one-question-per-run rule applies. The chosen label is recorded verbatim and the task restarts in the preserved workspace; your next run's task description ends with a `## Recorded answer` section naming the question and the chosen option. Act on that choice; it grants no authorization beyond the decision itself.\n\n\
     **When your task cannot finish until another PR lands first, ask to create a prerequisite task instead of a plain Yes/No.** Example: a CI-fix revision finds that a pre-existing test fixture races retention cleanup; that needs its own fix on main before the PR's CI can go green, and it is out of scope for you. Do not file a plain blocker, a plain yes/no question or `followup-task` for that case. Propose that work as its own task:\n\n\
     ```\n\
     {boss} propose done --outcome blocked --summary \"<blocker>\" --answer-type create-prerequisite-task --prerequisite-name \"<short task name>\" --prerequisite-brief \"<self-contained description>\" --explanation-file /tmp/explanation.txt\n\
     ```\n\n\
     The description must stand alone: the prerequisite becomes a separate task that a different worker picks up cold, seeing only that description. Name the files, the failure and its evidence, and what done looks like; never refer to \"this task\", \"above\", or your own session. Do not pass `--question` — the engine words it for the repo owner (\"Create prerequisite task '<name>'? This task will wait for it.\"). The repo owner still answers Yes or No. Yes creates the task in this product (or, if an equivalent open task already exists, links to it instead), makes your task depend on it, appends a dated note to your task description, and redispatches your task once the prerequisite is done. No records the declined path (granting no authorization) and redispatches so the next worker takes it. `{boss} propose followup-task` remains only the non-blocking path for nice-to-have work later. The one-question-per-run rule applies here too.\n\n\
     **When that work already belongs to an existing open task** (for example one that owns an open PR), ask to wait for that task instead of creating a duplicate. Pass `--answer-type link-prerequisite-task` with `--prerequisite-pr` (a GitHub URL, `owner/repo#n`, or PR number) and/or `--prerequisite-id` (the canonical or friendly task id). The engine resolves the task from that id or PR; it never matches by name. Do not pass `--question` — the engine words it after it resolves the task (\"Wait for '<name>'? This task will wait for it.\"). Yes adds a dependency on that task and parks this one until it is done. No records the declined path and redispatches. Keep `--answer-type create-prerequisite-task` only when no existing open task owns the work. If a create request names a PR that already belongs to another open task, the engine stores a link question instead.\n\n\
     ```\n\
     {boss} propose done --outcome blocked --summary \"<blocker>\" --answer-type link-prerequisite-task --prerequisite-pr \"<github-pr>\" --explanation-file /tmp/explanation.txt\n\
     ```\n\n\
     **When the change would exceed the `change/file-count` limit, stop adding scope and report a proposed split through the proposal below.** Estimate the file count early from the target repo's documented per-change costs, not the number of deliverables; check the footprint once the shape is clear and again before final validation. Describe what lands in this PR and what becomes a prerequisite or follow-up along capability boundaries. Recommend **Split** by default; recommend **Bypass limit** only for the unsplittable cases named in the target repo's `CHECKS.yaml` (in spinyfin/mono, coordinated renames and generated trees). Use the existing proposal: ask the split-vs-bypass question — both answers act. State the projected upper-bound file count of the single-PR change, the limit, and the proposed split's PR boundaries. `--file-count` records the projected upper bound. Checkleft does not enforce the approved count: before pushing, compare the non-deleted file count from `jj diff --stat` against the PR base with the approved bound, and ask again if it is over. Use a free-form bypass reason without a numeric prefix, citing the recorded answer and date. This is not a Yes/No question: the repo owner sees two buttons named for the actions, Split and Bypass limit, and `--explanation` uses `Split:` and `Bypass limit:` sections instead of `Yes:` and `No:`. Split creates the first coherent capability as a prerequisite (a standalone description of that first PR, split along a capability boundary); this task delivers the remainder once that PR has merged. Bypass limit authorizes a file-count exception for this PR only, records that approval, and redispatches so the next worker finishes in one PR by putting `BYPASS_CHANGE_FILE_COUNT=Split-vs-bypass question answered Bypass limit by the repo owner on <date>; one-PR exception for this PR only` in the commit description. Never apply that bypass without the recorded authorization. It does not relax any other check. Do not file a plain blocker, a plain yes/no question or `followup-task` for an over-limit change. **Revisions re-check the footprint:** re-run `checkleft run` before every push. If addressing review would push the PR past the limit or a previously approved bound, stop adding scope, finish the in-scope work that fits, and report the proposed split through the same proposal, recommending Split; never silently grow the PR. An earlier approval covers only the file count it was given.\n\n\
     ```\n\
     {boss} propose done --outcome blocked --summary \"<blocker>\" --answer-type create-prerequisite-task --prerequisite-name \"<first PR title>\" --prerequisite-brief \"<self-contained description of that first PR>\" --file-count <n> --file-count-limit <limit> --explanation-file /tmp/explanation.txt\n\
     ```\n\n\
     Before asking, check the task description for recorded approval of the requested action or an `## Recorded answer` section. If it already grants the authorization or makes the decision needed, proceed and cite it in the PR body. Do not ask for decisions you are allowed to make, or use questions to hand back work that is merely hard. The answer authorizes exactly the question; it does not disable or bypass any check beyond what that question asked.\n\n"
        )
    } else {
        String::new()
    };
    let superseded_ask_hint = if standard_worker {
        " To request that approval first, declare `--outcome blocked --answer-type yes-no --superseded-by <PR>` with `--question` and `--explanation-file`."
    } else {
        ""
    };
    format!(
        "\n## Declaring your run finished\n\n\
     When your run is over, say so — as the ABSOLUTE LAST thing you do:\n\n\
     ```\n\
     {boss} propose done --outcome <delivered|no-changes-needed|blocked> --summary \"<one line>\"\n\
     ```\n\n\
     **An accepted `delivered` or `blocked` declaration ends your run** — the engine tears your pane \
     down, with no turn after it, not even to see the response. Finish every other thing first, \
     including opening or updating the PR; only declare once nothing is left to do.\n\n\
     Pick the outcome that is true:\n\n\
     - `delivered` — the deliverable exists (you opened or pushed to the PR, wrote the review, \
     posted the reply). If you closed the bound PR as superseded with approval, also pass \
     `--superseded-by <PR>` (URL, `owner/repo#N`, `#N`, or PR number) so the engine records \
     a non-merged completion instead of treating the closed PR as a failure.{superseded_ask_hint}\n\
     - `no-changes-needed` — you verified there was nothing to produce. End your turn after submitting; \
     the engine checks command evidence at that boundary. If refused, verify again and redeclare.\n\
     - `blocked` — a genuine external blocker or mandated approval stop prevents delivery. This fails \
     the execution, releases its resources, and records the explanation on the task; it does not \
     park a live worker or automatically retry the same attempt. Fix recoverable failures first. \
     Never relax a repository check without approval. Include the exact failed command, missing \
     credential, or decision needed in the summary. {blocked_file} so the blocker itself is \
     recorded, not just the fact that you stopped.\n\n\
     {operator_question_block}\
     {gate_exception}\n\n\
     To flag a concern while continuing, {blocked_while_continuing} the blocker and pauses the \
     nudge loop; it does NOT end the run. Only `{boss} propose done --outcome blocked` ends the \
     run, releases the slot and lease, and parks the row for a human.\n\n\
     If you simply stop without declaring, you are not left alone: the engine holds the run open \
     while it can see you working, then asks you once whether you are finished, then fails the attempt \
     with a visible diagnostic. That is worse for you and for the human than \
     one command.\n"
    )
}

/// Treated, kind-gated `## Declaring your run finished` section for
/// `slim_worker_brief`: lists only the outcomes, flags, question types, and
/// build-gate exception applicable to `params.execution_kind`, pointing to the
/// long-form question contract and file-count protocol in the worker rules.
fn run_done_directive_slim(params: &RunDoneParams<'_>) -> String {
    let boss = boss_engine_worker_bin::WORKER_BOSS_INVOCATION;
    let kind_str = params.execution_kind.as_str();
    let has_bound_pr = execution_kind_has_bound_pr(kind_str, params.existing_pr_url);
    let supports_no_op = execution_kind_supports_no_op_outcome(kind_str, has_bound_pr);
    let supports_multiple_choice = execution_kind_supports_multiple_choice_questions(kind_str);
    let supports_prerequisites = execution_kind_supports_prerequisite_questions(kind_str);
    let supports_file_count = execution_kind_supports_file_count_protocol(kind_str);
    let has_build_gate = execution_kind_has_build_gate(kind_str);

    let outcomes_syntax = if supports_no_op {
        "<delivered|no-changes-needed|blocked>"
    } else {
        "<delivered|blocked>"
    };
    let (blocked_file, blocked_while_continuing) = if params.worker_signal_seam_enabled {
        (
            format!(
                "File `{boss} propose blocked --reason \"...\"` alongside it (before this call), unless the declaration carries a question"
            ),
            format!("`{boss} propose blocked --reason \"...\"` alone records"),
        )
    } else {
        (
            "Put the blocker reason in this call's `--summary` (a `[blocked] reason=\"...\"` marker \
             emitted immediately before this call is never parsed — the marker is Stop-boundary-only)"
                .to_string(),
            "a `[blocked] reason=\"...\"` marker is parsed only at the Stop boundary (when the turn \
             ends without a terminal `propose done`) and records"
                .to_string(),
        )
    };
    let delivered_example = match params.execution_kind {
        ExecutionKind::CiRemediation => "you completed the CI remediation",
        ExecutionKind::ProductDesign => "you completed the assigned work",
        _ => "you opened or pushed to the PR",
    };
    let superseded_clause = if has_bound_pr {
        " If you closed the bound PR as superseded with approval, also pass `--superseded-by <PR>` \
         (URL, `owner/repo#N`, `#N`, or PR number) so the engine records a non-merged completion \
         instead of treating the closed PR as a failure. To request that approval first, declare \
         `--outcome blocked --answer-type yes-no --superseded-by <PR>` with `--question` and \
         `--explanation-file`."
    } else {
        ""
    };
    let no_op_bullet = if supports_no_op {
        "- `no-changes-needed` — you verified there was nothing to produce (follow \"Empty diff and no-changes-needed\" in the worker rules).\n"
    } else {
        ""
    };

    let mut question_types = vec!["Yes/No (`--answer-type yes-no`)"];
    if supports_multiple_choice {
        question_types.push("multiple-choice (`--answer-type multiple-choice`)");
    }
    if supports_prerequisites {
        question_types.push("prerequisite tasks (`create-prerequisite-task` / `link-prerequisite-task`)");
    }
    let question_pointer = format!(
        "When delivery requires a human decision ({}), attach one question to that blocked declaration \
         following the \"Write questions for someone arriving cold\" section in the worker rules (check \
         for an existing `## Recorded answer` before asking).\n\n",
        question_types.join(", "),
    );
    let file_count_pointer = if supports_file_count {
        "If the change would exceed the `change/file-count` limit (or an earlier approved bound), stop \
         adding scope and follow the Split-vs-Bypass-limit protocol in the worker rules.\n\n"
    } else {
        ""
    };
    let gate_exception = if has_build_gate {
        format!(
            "{}\n\n",
            run_done_gate_exception_slim(params.conflict_resolution, params.bazel_workspace)
        )
    } else {
        String::new()
    };

    format!(
        "\n## Declaring your run finished\n\n\
         When your run is over, say so — as the ABSOLUTE LAST thing you do:\n\n\
         ```\n\
         {boss} propose done --outcome {outcomes_syntax} --summary \"<one line>\"\n\
         ```\n\n\
         **An accepted `delivered` or `blocked` declaration ends your run** — the engine tears your \
         pane down immediately, with no turn after it, not even to see the response. Finish every \
         other thing first, including opening or updating the PR; PR creation/update still gives you a \
         turn to declare. Never declare before pushing.\n\n\
         Pick the outcome that is true:\n\n\
         - `delivered` — the deliverable exists ({delivered_example}).{superseded_clause}\n\
         {no_op_bullet}\
         - `blocked` — a genuine external blocker or mandated approval stop prevents delivery. This \
         fails the execution, releases its resources, and records the explanation on the task; it does \
         not park a live worker or automatically retry the same attempt. Fix recoverable failures \
         first. Include the exact failed command, missing credential, or decision needed in the \
         summary. {blocked_file} so the blocker itself is recorded, not just the fact that you stopped.\n\n\
         {question_pointer}\
         {file_count_pointer}\
         {gate_exception}\
         To flag a concern while continuing, {blocked_while_continuing} the blocker and pauses the \
         nudge loop; it does NOT end the run. Only `{boss} propose done --outcome blocked` ends the \
         run, releases the slot and lease, and parks the row for a human.\n\n\
         If you simply stop without declaring, you are not left alone: the engine holds the run open \
         while it can see you working, then asks you once whether you are finished, then fails the \
         attempt with a visible diagnostic. That is worse for you and for the human than one command.\n"
    )
}
