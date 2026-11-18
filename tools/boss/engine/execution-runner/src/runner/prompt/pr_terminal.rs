//! Terminal PR-creation sequence rendered into worker prompts.

/// Directive that warns workers PR creation is terminal: no followup turn is
/// possible after it. Workers must finish all work — including consuming
/// any in-flight reviews they started — BEFORE opening the PR. Incident: a
/// worker opened a PR, then tried to wait for background review subagents
/// and address their findings as followup commits. The engine terminated
/// the worker before the review was consumed. This universal guidance
/// applies to every execution kind and prevents that pattern.
///
/// States the worker's terminal sequence exactly once, in the order it must
/// happen, rather than as several directives that can read as contradicting
/// each other (a prior version literally did: "opening the PR is the LAST
/// thing you do", followed later by "you will not get another turn after
/// `gh pr create` *and* the `boss propose pr-created` call" — two different
/// claims about what the last step is). It also does not claim the engine
/// reaps the worker "immediately after the PR is created": the PR-created
/// hook only stages and arms the URL (`pr_url_capture.rs`); finalization —
/// and the termination that comes with it — happens at the worker's next
/// Stop boundary, not synchronously inside the `gh pr create` call.
///
/// `run_done_proposals_seam_enabled` mirrors `run_done_proposals_seam`: when
/// on, the sequence gains a `{boss} propose done` declaration step after
/// the push and any PR-created declaration, because accepting it finalizes
/// the run synchronously.
///
/// `pr_created_proposals_seam_enabled` mirrors `pr_created_proposals_seam`
/// (design implementation task 12): when on, the sequence gains a `{boss}
/// propose pr-created --url ...` declaration step after the push, since
/// finalization now reads that proposal row first. With both flags `false` this
/// reproduces the pre-migration two-step sequence (just push, then stop).
pub(super) fn pr_terminal_directive(
    run_done_proposals_seam_enabled: bool,
    pr_created_proposals_seam_enabled: bool,
) -> String {
    let boss = boss_engine_worker_bin::WORKER_BOSS_INVOCATION;
    let cube = boss_engine_worker_bin::WORKER_CUBE_INVOCATION;
    let push_step = format!("open or update the PR with `{cube} pr create` / `{cube} pr update`");
    let mut steps = Vec::new();
    steps.push(push_step);
    if pr_created_proposals_seam_enabled {
        steps.push(format!("declare `{boss} propose pr-created --url ...`"));
    }
    if run_done_proposals_seam_enabled {
        steps.push(format!("declare `{boss} propose done` (see below)"));
    }
    let sequence = steps
        .iter()
        .enumerate()
        .map(|(i, step)| format!("{}. {step}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let terminal_action_sentence = if run_done_proposals_seam_enabled {
        format!(
            "You will NOT get another turn after `{boss} propose done` — that declaration is the \
             terminal action, not an afterthought."
        )
    } else if pr_created_proposals_seam_enabled {
        format!(
            "You will NOT get another turn after `gh pr create` / `{cube} pr create` (or `{cube} \
             pr update` for an existing PR) and the `{boss} propose pr-created --url ...` call \
             that declares it — that declaration IS the terminal action, not an afterthought."
        )
    } else {
        format!(
            "You will NOT get another turn after `gh pr create` / `{cube} pr create` (or `{cube} \
             pr update` for an existing PR) — that is the terminal action, not an afterthought."
        )
    };

    let mut out = String::new();
    out.push_str("\n## Important: PR creation is your terminal act\n\n");
    out.push_str(
        "Everything else in this run happens BEFORE the sequence below. Do this, in order, as \
         your last actions:\n\n",
    );
    out.push_str(&sequence);
    out.push_str("\n\n");
    out.push_str(&format!(
        "{terminal_action_sentence} Do not plan followup commits, do not defer work to \
         \"after the PR\", do not open the PR until every gate, build, test run, review and \
         sub-agent run you started has returned its real result — the PR comes after those \
         results, never in place of them.\n\n"
    ));
    out.push_str("Therefore: finish everything — including consuming any review/self-review findings you started — BEFORE you open the PR. If a background review is still running and you care about its results, wait for it and address all findings FIRST, then open the PR. If you don't intend to wait, don't start the review.\n");
    out
}
