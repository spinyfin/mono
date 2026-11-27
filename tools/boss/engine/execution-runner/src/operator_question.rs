//! Worker-facing writing contract for the existing question/explanation fields.

use crate::prompt_fragments::{
    execution_kind_supports_file_count_protocol, execution_kind_supports_multiple_choice_questions,
    execution_kind_supports_prerequisite_questions,
};

pub(crate) const GUIDANCE: &str = r#"
## Write questions for someone arriving cold

Assume the repo owner has not seen the PR, review, code, or your run. Use everyday words: no internal symbols, types, file names, API names, review jargon, or links in the question or Yes/No meanings. Keep technical evidence reachable in a separate Details section, not mixed into the decision.

Use `--question` for one specific Yes/No decision in plain language. Write the labelled explanation to a file and pass `--explanation-file <path>` on a single line (or `--explanation-file -` / `--explanation -` to read stdin). You may also pass `--explanation` with one quoted argument per line. A single multi-line `--explanation` argument is accepted when the command can contain newlines; some PreToolUse guards reject any command string that contains a newline, even inside quotes. In the explanation, require these labels on their own lines, in this order (actual newlines, no Markdown heading decoration):

Context:
One or two plain-language sentences: what work you are doing and what problem forces this decision.
Yes:
What Yes does: the path the engine or the next worker will take, the user-visible result, and the rough size of the work.
No:
What No does: a different path the engine or the next worker will take, the user-visible result, and the rough size of that work. "No = remain blocked" is not a real choice. If you cannot name a second path, do not ask; proceed, or report the blocker without a question.
Recommendation:
I recommend Yes (or No) because <one-line reason>.
Details:
Optional technical evidence, symbols, file names, links, and review references. Omit this label if no detail is needed. Never truncate evidence just to make the question shorter.

The card presents Context, Question, Yes, No, Recommendation, then expandable Details. The same format applies to prerequisite questions: use a plain-language prerequisite name, explain the work and the dependency in Context, and explain that Yes creates/links the prerequisite and waits, while No records the declined path and redispatches. `--file-count` records the projected upper bound. Checkleft does not enforce the approved count: before pushing, compare the non-deleted file count from `jj diff --stat` against the PR base with the approved bound, and ask again if it is over. Use a free-form bypass reason without a numeric prefix, citing the recorded answer and date. The question must state that projected count, the limit, and the proposed split's PR boundaries. Never apply that exception without the recorded authorization. The prerequisite description can retain the technical instructions for the next worker.

A question about a change that would exceed the file-count limit (`--file-count`) is not a Yes/No: its two answers are different actions, so the card shows buttons named **Split** and **Bypass limit**, and the engine words the question and owns those labels. Replace the `Yes:` and `No:` sections with `Split:` and `Bypass limit:`, in the order Context, Split, Bypass limit, Recommendation, Details, with the same content rules (`--explanation` is refused if it still uses `Yes:` / `No:`). Split creates the first PR as a prerequisite and waits for it; Bypass limit authorizes a file-count exception for this PR only and the next worker finishes in one PR. Stop adding scope and explain what lands in this PR versus the prerequisite or follow-up along capability boundaries. Recommend Split by default; recommend Bypass limit only for the unsplittable cases named in the target repo's `CHECKS.yaml` (in spinyfin/mono, coordinated renames and generated trees). Revisions re-run `checkleft run` before every push and report a proposed split if the PR would exceed the limit or an approved bound; an earlier approval covers only the file count it was given. Never write "Yes" or "No" for these options, and phrase the recommendation as "I recommend Split because <one-line reason>" unless an unsplittable case justifies Bypass limit.

Before submitting, read only Context, Question, the two options (Yes and No, or Split and Bypass limit), and Recommendation. Can someone who has never seen the work decide what each button will do? If not, rewrite those parts yourself. This writing contract does not change when a question is warranted or grant permission to bypass checks.
"#;

/// Treated, kind-gated question-writing contract and command templates for
/// `slim_worker_brief`. Holds the single long form of every question type the
/// execution kind can use so the brief's done-declaration section only carries
/// concise pointers.
pub(crate) fn render_slim_guidance(execution_kind: &str, boss: &str, worker_signal_seam_enabled: bool) -> String {
    let supports_multiple_choice = execution_kind_supports_multiple_choice_questions(execution_kind);
    let supports_prerequisites = execution_kind_supports_prerequisite_questions(execution_kind);
    let supports_file_count = execution_kind_supports_file_count_protocol(execution_kind);

    let question_companion = if worker_signal_seam_enabled {
        format!(
            " A declaration that carries a question is its own blocker record: do NOT also file \
             `{boss} propose blocked`, because that raises a separate attention item the answer never clears."
        )
    } else {
        String::new()
    };

    let mut out = format!(
        "\n## Write questions for someone arriving cold\n\
         \n\
         Assume the repo owner has not seen the PR, review, code, or your run. Use everyday words: \
         no internal symbols, types, file names, API names, review jargon, or links in the question or \
         option meanings. Keep technical evidence reachable in a separate `Details:` section, not mixed \
         into the decision.\n\
         \n\
         Before asking, check the task description for recorded approval of the requested action or an \
         `## Recorded answer` section. If it already grants the authorization or makes the decision \
         needed, proceed and cite it in the PR body. Do not ask for decisions you are allowed to make, \
         or use questions to hand back work that is merely hard. The answer authorizes exactly the \
         question; it does not disable or bypass any check beyond what that question asked. One \
         question per run: ask the most blocking decision and disclose any further decisions in the \
         explanation.{question_companion}\n\
         \n\
         ### Yes/No decision\n\
         \n\
         Use `--question` for one specific Yes/No decision in plain language (\"Can I proceed?\" is not \
         specific enough; put the justification in the explanation file, not the question). Write the \
         labelled explanation to a file and pass `--explanation-file <path>` on a single line \
         (`--explanation-file -` or `--explanation -` reads stdin; you may also pass `--explanation` \
         with one quoted argument per line, and some PreToolUse guards reject commands containing \
         newlines even inside quotes):\n\
         \n\
         ```\n\
         {boss} propose done --outcome blocked --summary \"<blocker>\" --question \"Approve switching the retry to exponential backoff?\" --answer-type yes-no --explanation-file /tmp/explanation.txt\n\
         ```\n\
         \n\
         In the explanation, require these labels on their own lines, in this order (actual newlines, \
         no Markdown heading decoration):\n\
         \n\
         Context:\n\
         One or two plain-language sentences: what work you are doing and what problem forces this decision.\n\
         Yes:\n\
         What Yes does: the path the engine or the next worker will take, the user-visible result, and the rough size of the work.\n\
         No:\n\
         What No does: a different path the engine or the next worker will take, the user-visible result, and the rough size of that work. \"No = remain blocked\" is not a real choice. If you cannot name a second path, do not ask; proceed, or report the blocker without a question.\n\
         Recommendation:\n\
         I recommend Yes (or No) because <one-line reason>.\n\
         Details:\n\
         Optional technical evidence, symbols, file names, links, and review references. Omit this label if no detail is needed. Never truncate evidence just to make the question shorter.\n\
         \n\
         The card presents Context, Question, Yes, No, Recommendation, then expandable Details. The run \
         waits durably for the answer: Yes records the approval and redispatches; No records the \
         declined path (granting no authorization) and redispatches so the next worker takes it.\n"
    );

    if supports_multiple_choice {
        out.push_str(&format!(
            "\n### Choosing among several options\n\
             \n\
             When the decision is a choice among several options rather than approve/decline, use \
             `--answer-type multiple-choice` with two to six short, distinct, single-line labels via \
             `--choices` (the card shows one button per label). Use only `Context:` (say what each \
             option leads to), `Recommendation:` and optional `Details:` — no `Yes:` or `No:` sections \
             (rejected for this type):\n\
             \n\
             ```\n\
             {boss} propose done --outcome blocked --summary \"<blocker>\" --question \"Which rollout should the migration use?\" --answer-type multiple-choice --choices \"Gradual\" \"All at once\" --explanation-file /tmp/explanation.txt\n\
             ```\n\
             \n\
             The chosen label is recorded verbatim and the task restarts in the preserved workspace \
             with an `## Recorded answer` section naming the question and the chosen option. Act on \
             that choice; it grants no authorization beyond the decision itself.\n"
        ));
    }

    if supports_prerequisites {
        out.push_str(&format!(
            "\n### Waiting on another PR (prerequisite tasks)\n\
             \n\
             When your task cannot finish until another PR lands first, propose a prerequisite task \
             instead of a plain blocker, plain Yes/No, or `{boss} propose followup-task` (which \
             remains only the non-blocking path for nice-to-have work later). Use the same \
             `Context` / `Yes` / `No` / `Recommendation` / `Details` format with a plain-language \
             prerequisite name: explain the work and dependency in `Context`, and explain that Yes \
             creates or links the prerequisite and waits while No records the declined path (granting \
             no authorization) and redispatches. Do not pass `--question` — the engine words it for \
             the repo owner.\n\
             \n\
             - When no existing open task owns the work, create one (the prerequisite description can \
               retain technical instructions and must stand alone for a different worker picking it up \
               cold — name the files, failure and evidence, and what done looks like; never refer to \
               \"this task\", \"above\", or your own session; if it names a PR already owned by another \
               open task, the engine stores a link question instead):\n\
             \n\
             ```\n\
             {boss} propose done --outcome blocked --summary \"<blocker>\" --answer-type create-prerequisite-task --prerequisite-name \"<short task name>\" --prerequisite-brief \"<self-contained description>\" --explanation-file /tmp/explanation.txt\n\
             ```\n\
             \n\
             - When an existing open task already owns the work (for example one owning an open PR), \
               link it with `--prerequisite-pr` (URL, `owner/repo#n`, or PR number) and/or \
               `--prerequisite-id` (canonical or friendly task id; never matched by name):\n\
             \n\
             ```\n\
             {boss} propose done --outcome blocked --summary \"<blocker>\" --answer-type link-prerequisite-task --prerequisite-pr \"<github-pr>\" --explanation-file /tmp/explanation.txt\n\
             ```\n"
        ));
    }

    if supports_file_count {
        out.push_str(&format!(
            "\n### Exceeding the `change/file-count` limit (Split vs Bypass limit)\n\
             \n\
             Estimate the file count early from the target repo's documented per-change costs (not the \
             number of deliverables); check the footprint once the shape is clear and again before \
             final validation. When the change would exceed the `change/file-count` limit, stop adding \
             scope and report a proposed split through the proposal below (do not file a plain \
             blocker, plain Yes/No, or `followup-task`):\n\
             \n\
             ```\n\
             {boss} propose done --outcome blocked --summary \"<blocker>\" --answer-type create-prerequisite-task --prerequisite-name \"<first PR title>\" --prerequisite-brief \"<self-contained description of that first PR>\" --file-count <n> --file-count-limit <limit> --explanation-file /tmp/explanation.txt\n\
             ```\n\
             \n\
             This is not a Yes/No question: the card shows buttons named **Split** and **Bypass \
             limit**, and the engine words the question and owns those labels. Replace `Yes:` and \
             `No:` with `Split:` and `Bypass limit:` in the order `Context`, `Split`, `Bypass limit`, \
             `Recommendation`, `Details` (`--explanation` is refused if it still uses `Yes:` / `No:`). \
             Never write \"Yes\" or \"No\" for these options. State the projected upper-bound file \
             count (`--file-count`), the limit, and what lands in this PR versus the prerequisite or \
             follow-up along capability boundaries. Recommend **Split** by default (`I recommend Split \
             because <one-line reason>`); recommend **Bypass limit** only for the unsplittable cases \
             named in the target repo's `CHECKS.yaml` (in spinyfin/mono, coordinated renames and \
             generated trees).\n\
             \n\
             **Split** creates the first coherent capability as a prerequisite PR and waits for it; \
             **Bypass limit** authorizes a file-count exception for this PR only and redispatches so \
             the next worker finishes in one PR by putting \
             `BYPASS_CHANGE_FILE_COUNT=Split-vs-bypass question answered Bypass limit by the repo owner on <date>; one-PR exception for this PR only` \
             (a free-form bypass reason without a numeric prefix, citing the recorded answer and date) \
             in the commit description. Never apply that bypass without recorded authorization; it \
             does not relax any other check. Checkleft does not enforce the approved count: before \
             pushing, compare the non-deleted file count from `jj diff --stat` against the PR base \
             with the approved bound and ask again if it is over.\n"
        ));
        if execution_kind == "revision_implementation" {
            out.push_str(
                "\nRevisions re-run `checkleft run` before every push. If addressing review would push \
                 the PR past the limit or a previously approved bound, stop adding scope, finish the \
                 in-scope work that fits, and report the proposed split through the same proposal, \
                 recommending Split; never silently grow the PR. An earlier approval covers only the \
                 file count it was given.\n",
            );
        }
    }

    let options_label = if supports_file_count {
        "the two options (Yes and No, or Split and Bypass limit)"
    } else {
        "the options"
    };
    out.push_str(&format!(
        "\nBefore submitting, read only Context, Question, {options_label}, and Recommendation. Can \
         someone who has never seen the work decide what each button will do? If not, rewrite those \
         parts yourself. This writing contract does not change when a question is warranted or grant \
         permission to bypass checks.\n"
    ));
    out
}
