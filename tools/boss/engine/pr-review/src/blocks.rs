//! Engine-flagged prompt blocks embedded verbatim in the reviewer prompt.
//!
//! Both blocks are pure `&[String] -> String` renderers over the *already
//! scanned* hit lines the engine carries on [`crate::PrReviewContext`]. The
//! deterministic scans that produce those lines live in the engine
//! (`supersession_scan`, `boss_construct_scan`); only the reviewer-facing
//! rendering lives here, next to the prompt it is interpolated into.

/// Render an authoritative reviewer-prompt block for a set of deterministic
/// supersession flag lines (from the engine's `hit_lines` helper). Returns an empty string when
/// there are no flags, so the caller can unconditionally interpolate it.
///
/// The block is phrased so the reviewer must *verify a design-doc citation*
/// for each flagged claim — the reviewer's job is verification, the engine's
/// job (this scan) is to guarantee the claim is not silently overlooked.
pub fn render_supersession_flag_block(flag_lines: &[String]) -> String {
    if flag_lines.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("## Supersession-claim citation check (engine-flagged) — CRITICAL\n\n");
    out.push_str(
        "The engine's deterministic scan found **supersession / obsolescence \
         language** in this PR's body, commits, or comments. A claim that one \
         surface \"supersedes\", \"obsoletes\", \"replaces\", is \"now-dead\", \
         or is \"orphaned\" is a **design decision** the worker is not \
         authorised to make unilaterally — in incident-002 exactly such a \
         narrative laundered the deletion of a merged feature that the design \
         doc specified as a sibling surface.\n\n\
         For EACH flagged phrase below you MUST verify:\n\n\
         1. Does the PR cite a specific **design doc + section** that authorises \
         the supersession? (A clean build, \"it looks dead\", or \"this same PR \
         removed its only caller\" are NOT authorisation.)\n\
         2. Read the cited section. Does it **actually support** the claim (say \
         the surface is replaced), or does it contradict it (specify the \
         surface as still-required / a sibling)?\n\n\
         If a flagged claim has no design-doc citation, or the citation does not \
         support it, raise a `regression` finding: the removal it justifies is \
         presumptively wrong and must be restored or escalated. Do not accept \
         the narrative at face value.\n\n\
         Flagged phrases:\n\n",
    );
    for line in flag_lines {
        out.push_str(&format!("- {line}\n"));
    }
    out.push('\n');
    out
}

/// Render an authoritative reviewer-prompt block for a set of deterministic
/// Boss-construct-id sweep lines (from the engine's `hit_lines` helper). Returns an empty
/// string when there are no hits, so the caller can unconditionally
/// interpolate it.
///
/// The block converts each hit into a forced disposition: the reviewer must
/// either raise it as an `agent_isms` finding or explicitly state why it is
/// not one. It does not decide the disposition itself — like
/// [`render_supersession_flag_block`], it guarantees the candidate is not silently
/// overlooked.
pub fn render_boss_construct_sweep_block(flag_lines: &[String]) -> String {
    if flag_lines.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("## Boss work-item id sweep (engine-computed) — CRITICAL\n\n");
    out.push_str(
        "The engine's deterministic scan found bare Boss work-item id tokens \
         (`T<n>`/`P<n>`) in this PR's added diff lines and/or its title or \
         description. In a Boss-managed repo every such bare token IS a Boss \
         work-item id — there is no other tracker using this format. \
         Pre-existing `T<n>`/`P<n>` references already in the repo are \
         earlier leaks and never legitimize new ones, and an id appearing in \
         this prompt's own Task description is evidence FOR a violation if \
         the worker echoed it into the PR, not evidence the id is ordinary \
         project vocabulary — see the agent-isms rubric's Boss-construct \
         sub-rule above.\n\n\
         For EACH candidate below you MUST either raise it as an \
         `agent_isms` finding, or explicitly state in your review why it is \
         not one. Silently skipping a candidate is not an acceptable \
         disposition.\n\n\
         Candidates:\n\n",
    );
    for line in flag_lines {
        out.push_str(&format!(
            "- candidate Boss-construct reference {line} — you must either flag it as a finding or state why it is not one.\n"
        ));
    }
    out.push('\n');
    out
}

/// Render the shared "Required output — CRITICAL" verdict-submission
/// contract block embedded in both the supervisor's and the post-merge
/// reviewer's initial prompt: the write-then-submit steps and the exact
/// `boss propose review-verdict` invocation. Both roles submit the same
/// [`crate::supervisor_types::SupervisorVerdict`] shape through the same
/// command, so this text — including a future flag rename or a new
/// required field in the write-then-submit contract — must never drift
/// between the two prompts. The caller appends its own `Schema:` section
/// immediately after, since the JSON schema itself differs per role.
pub fn render_verdict_submission_block(batch_id: &str, body_path: &str) -> String {
    format!(
        "## Required output — CRITICAL\n\
         \n\
         You must submit exactly one structured verdict while this session is alive.\n\
         \n\
         1. Write the JSON object below to this exact engine-owned body file:\n\
         \n\
         `{body_path}`\n\
         \n\
         2. Submit it immediately with:\n\
         \n\
         ```sh\n\
         boss propose review-verdict --batch-id {batch_id} --verdict-file \"{body_path}\"\n\
         ```\n\
         \n\
         The command validates the verdict immediately. If it rejects the file, correct the\n\
         reported field errors and submit again before ending your turn. Do not put the JSON\n\
         in your final response: transcript recovery is intentionally unavailable for batch\n\
         reviews. The one body-file write and this local `boss propose` call are permitted;\n\
         do not edit repository files or publish anything.\n",
        body_path = body_path,
        batch_id = batch_id,
    )
}
