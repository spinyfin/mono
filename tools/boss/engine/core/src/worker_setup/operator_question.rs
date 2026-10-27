//! Worker-facing writing contract for the existing question/explanation fields.

pub(crate) const GUIDANCE: &str = r#"
### Write questions for someone arriving cold

Assume the repo owner has not seen the PR, review, code, or your run. Use everyday words: no internal symbols, types, file names, API names, review jargon, or links in the question or Yes/No meanings. Keep technical evidence reachable in a separate Details section, not mixed into the decision.

Use `--question` for one specific Yes/No decision in plain language. In `--explanation`, require these labels on their own lines, in this order (actual newlines, no Markdown heading decoration):

Context:
One or two plain-language sentences: what work you are doing and what problem forces this decision.
Yes:
What Yes authorizes, the user-visible result, and the rough size of the work.
No:
What declining means for the user and the rough size of the alternative. Be honest: No leaves the work blocked; it does not automatically start that alternative.
Recommendation:
I recommend Yes (or No) because <one-line reason>.
Details:
Optional technical evidence, symbols, file names, links, and review references. Omit this label if no detail is needed. Never truncate evidence just to make the question shorter.

The card presents Context, Question, Yes, No, Recommendation, then expandable Details. The same format applies to prerequisite questions: use a plain-language prerequisite name, explain the work and the dependency in Context, and explain that Yes creates/links the prerequisite and waits, while No leaves the work blocked. The prerequisite description can retain the technical instructions for the next worker.

Before submitting, read only Context, Question, Yes, No, and Recommendation. Can someone who has never seen the work decide what each button will do? If not, rewrite those parts yourself. This writing contract does not change when a question is warranted or grant permission to bypass checks.
"#;
