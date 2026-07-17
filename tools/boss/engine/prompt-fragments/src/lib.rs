//! Shared text fragments used verbatim across multiple agent-prompt
//! renderers.
//!
//! Several agent prompts (the `boss_engine_pr_review` reviewer prompt and
//! `boss_engine`'s `automation_triage`) end with an identical
//! `## Boundaries` + `## Coordinator` block. Keeping one copy here prevents
//! the renderers from drifting apart and makes an intentional wording change
//! a one-line edit. This crate sits below both consumers so neither has to
//! depend on the other.
//!
//! Note that not every prompt shares this fragment: renderers with
//! *intentionally* different boundaries wording keep their own copy —
//! `boss_engine`'s `answer_agent` widens the first rule to "inside or outside"
//! (a strictly read-only agent) and omits the coordinator-probe section, and
//! its `worker_setup` names a concrete sibling-workspace path and adds a
//! work-taxonomy sentence. Do not fold those onto this fragment without
//! matching each site's semantics.

/// The `## Boundaries` + `## Coordinator` block shared verbatim by the
/// `pr_review` and `automation_triage` agent prompts.
///
/// The string starts at the `## Boundaries` heading and ends with a trailing
/// newline after the coordinator-probe sentence, so a caller embeds it right
/// after the blank line that precedes the boundaries section.
pub fn boundaries_and_coordinator_fragment() -> &'static str {
    "## Boundaries\n\
     \n\
     - Do not modify files outside your workspace. Other workspaces\n\
     belong to other workers.\n\
     - Do not modify cube's database, lease state, or workspace registry.\n\
     - `~/Library/Application Support/Boss/` is coordinator/engine-only.\n\
     Never read, write, or touch it.\n\
     `bossctl` is coordinator-only.\n\
     \n\
     ## Coordinator\n\
     \n\
     The coordinator may probe this session between turns. Treat probes\n\
     as questions from a human reviewer — short, specific answers.\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragment_has_both_headings_and_bossctl_note() {
        let frag = boundaries_and_coordinator_fragment();
        assert!(frag.starts_with("## Boundaries\n"));
        assert!(frag.contains("## Coordinator\n"));
        assert!(frag.contains("`bossctl` is coordinator-only."));
        assert!(frag.ends_with("short, specific answers.\n"));
    }
}
