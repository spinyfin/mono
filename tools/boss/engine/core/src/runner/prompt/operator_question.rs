//! Worker-facing writing contract for the existing question/explanation fields.

pub(super) const GUIDANCE: &str = r#"
### Write operator questions for someone arriving cold

Assume the operator has not seen the PR, review, code, or your run. Use everyday words: no internal symbols, types, file names, API names, review jargon, or links in the question or Yes/No meanings. Keep technical evidence reachable in a separate Details section, not mixed into the decision.

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

The card presents Context, Question, Yes, No, Recommendation, then expandable Details. The same format applies to prerequisite questions: use a plain-language prerequisite name, explain the work and the dependency in Context, and explain that Yes creates/links the prerequisite and waits, while No leaves the work blocked. The prerequisite brief can retain the technical instructions for the next worker.

Worked example — bad (assumes the reader knows the code and review):

QUESTION: Approve treating failed attempted NWConnection writes as uncertain, retaining for Retry only actions never passed to send?

WHY THE WORKER IS ASKING: The review explicitly requires failed-write reclamation into the retry buffer. Apple documents contentProcessed as network-stack consumption rather than remote acknowledgment (https://developer.apple.com/videos/play/wwdc2018/715/); its error supplies no delivered-byte count. EngineClient.swift currently reclaims any completion error, while engine/core/src/app.rs dispatches each received request without a deduplication gate. Yes authorizes replacing that reclaim requirement with per-request uncertainty and adapting its tests, then completing the remaining review findings. No requires a different delivery guarantee, such as engine-side deduplication, before retrying attempted writes.

Worked example — plain rewrite:

--question: Only resend actions that definitely never left the app?
--explanation:
Context:
The app sometimes loses your actions (edits, board moves) when its connection to the engine drops, and the review asked me to automatically resend any message whose send failed. The catch: the app cannot tell whether a failed send already arrived, so resending could make an action happen twice (for example, a task could start twice).
Yes:
Only auto-resend messages that definitely never left the app; show the others as "may not have gone through" for you to check. Smaller change; avoids double actions from resending.
No:
Keep the request to resend everything that failed, which first needs the engine to detect and drop duplicate messages. Bigger change; the work stays blocked until that approach is authorized.
Recommendation:
I recommend Yes because a double action is worse than asking you to check.
Details:
Apple documents contentProcessed as network-stack consumption rather than remote acknowledgment (https://developer.apple.com/videos/play/wwdc2018/715/); errors supply no delivered-byte count. EngineClient.swift currently reclaims any completion error, while engine/core/src/app.rs dispatches requests without deduplication. Yes changes that review requirement and its tests to retain only unsent requests for retry.

Before submitting, read only Context, Question, Yes, No, and Recommendation. Can someone who has never seen the work decide what each button will do? If not, rewrite those parts yourself. This writing contract does not change when a question is warranted or grant permission to bypass checks.
"#;
