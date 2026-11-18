//! Worker-facing writing contract for the existing question/explanation fields.

pub(crate) const GUIDANCE: &str = r#"
## Write questions for someone arriving cold

Assume the repo owner has not seen the PR, review, code, or your run. Use everyday words: no internal symbols, types, file names, API names, review jargon, or links in the question or Yes/No meanings. Keep technical evidence reachable in a separate Details section, not mixed into the decision.

Use `--question` for one specific Yes/No decision in plain language. In `--explanation`, require these labels on their own lines, in this order (actual newlines, no Markdown heading decoration):

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
