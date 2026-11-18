//! CI failure classification shared by all standard workers.

pub(crate) const GUIDANCE: &str = "\n## CI failure classification\n\n\
     Classify a CI failure by cause, then act — do not park, and do not \
     leave \"someone else will handle it\" as an available excuse:\n\
     - Caused by your change: fix it. No judgement call, not grounds to \
     block, no handoff.\n\
     - Unrelated to your change AND trivial: fix it inline — that is what \
     a human would do.\n\
     - Unrelated AND not trivial: do not absorb it into this PR. Do not \
     silently expand this PR into repairing someone else's breakage — \
     that makes the diff unreviewable and couples two unrelated changes. \
     This is the case where stopping to escalate can be legitimate. Flag \
     it, stating whether the remedy is a separate fix or reverting the \
     offending commit, and why. \"CI is red\" is not an escalation; name \
     the specific problem and the proposed remedy.\n\
     \n\
     Whichever branch you take, the PR description must say which \
     category you judged the failure into and on what basis. A silent \
     inline fix of an unrelated failure is nearly as bad as ignoring it, \
     because nobody can review a decision they cannot see.\n\
     \n\
     Trivial means the correct fix is mechanical and obviously correct — \
     the canonical case is `main` adding a required `is_review_supervisor` \
     field to `StartWorkerInput` after a test was written, breaking clippy \
     on an unrelated PR's merge-queue synthetic commit. One line, \
     mechanical. A failing test whose correct behaviour you would have to \
     reason about, or a failure whose cause you cannot identify, is not \
     trivial, regardless of how few lines the fix looks.\n\
     \n\
     The engine already mints CI-fix revisions. That path is the fallback \
     for failures that genuinely need a separate change (the \
     unrelated-and-not-trivial case). It is not a reason to punt a \
     failure you could fix in this run.\n\
     \n\
";
