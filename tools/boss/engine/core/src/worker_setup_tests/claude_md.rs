use super::super::*;
use super::helpers::*;

#[test]
fn claude_md_mentions_workspace_and_lease() {
    let input = sample_input();
    let rendered = claude_md_for(&input);
    assert!(rendered.contains(input.workspace_path.to_str().unwrap()));
    assert!(rendered.contains(&input.lease_id));
    assert!(rendered.contains("`jj`"));
    assert!(rendered.contains("PR"));
}

#[test]
fn claude_md_explains_origin_is_the_real_github_upstream() {
    // Workers must be told that in a (shared-store) cube workspace `origin`
    // IS the real GitHub upstream, that `jj git push` reaches GitHub, and
    // that a push can be confirmed against GitHub's head sha.
    let input = sample_input();
    let rendered = claude_md_for(&input);
    assert!(rendered.contains("real GitHub upstream"));
    assert!(rendered.contains("SHARES one"));
    assert!(rendered.contains(".head.sha") || rendered.contains(".commit.sha"));
}

#[test]
fn claude_md_forbids_editor_fallthrough_for_commit_messages() {
    let input = sample_input();
    let rendered = claude_md_for(&input);
    // The rule must explicitly call out `-m` and the editor
    // fallthrough so a worker that grepped only for "commit" still
    // hits the guidance.
    assert!(rendered.contains("-m"));
    assert!(rendered.contains("$EDITOR"));
    assert!(rendered.contains("jj describe"));
    assert!(rendered.contains("git commit"));
}

#[test]
fn triage_kind_renders_triage_claude_md_without_pr_mandate() {
    let mut input = sample_input();
    input.worker_kind = WorkerKind::Triage;
    let rendered = claude_md_for(&input);
    // Routed to the triage CLAUDE.md: restates the marker contract.
    assert!(
        rendered.contains("automation: task") && rendered.contains("automation: skip"),
        "triage worker CLAUDE.md must restate the decision-marker contract",
    );
    // Must NOT carry the Standard implementation PR-delivery mandate — that
    // conflict is what leaves triage runs ending without a decision marker.
    assert!(
        !rendered.contains("Pull requests are the deliverable"),
        "triage CLAUDE.md must not include the standard PR-required reminder",
    );
    assert!(
        !rendered.contains("PR creation is your terminal act"),
        "triage CLAUDE.md must not state PR creation is the terminal act",
    );
    // Lease id surfaced, workspace path not hardcoded.
    assert!(rendered.contains(&input.lease_id));
    assert!(
        !rendered.contains(input.workspace_path.to_str().unwrap()),
        "triage CLAUDE.md must not hardcode the workspace path",
    );
}

#[test]
fn claude_md_warns_against_touching_boss_state_dir() {
    // A worker that misses the harness-level deny rule (e.g. a
    // future claude-code release changes the rule format) needs
    // a soft soft-rule in the CLAUDE.md system prompt to know
    // it's off-limits. Belt-and-suspenders.
    let input = sample_input();
    let rendered = claude_md_for(&input);
    assert!(
        rendered.contains("Library/Application Support/Boss"),
        "CLAUDE.md must call out the Boss state dir explicitly",
    );
    assert!(
        rendered.contains("bossctl"),
        "CLAUDE.md must explicitly identify bossctl as coordinator-only",
    );
}

/// Workers must be told jj is the VCS, not git. A worker that reaches for raw
/// `git` in a cube workspace hits a bare-repo surprise (there is no `.git/` at
/// the workspace root), so the jj-first mandate is load-bearing rather than
/// stylistic.
#[test]
fn claude_md_mandates_jj_first_vcs() {
    let input = sample_input();
    let rendered = claude_md_for(&input);
    assert!(
        rendered.contains("Use `jj` for all VCS"),
        "CLAUDE.md must state the jj-first mandate; got:\n{rendered}",
    );
    assert!(
        rendered.contains("Do not invoke `git` directly"),
        "CLAUDE.md must steer workers off raw git; got:\n{rendered}",
    );
}

/// The sibling-workspace boundary is a containment property, not prose polish:
/// every worker shares `~/Documents/dev/workspaces/` with other live workers,
/// and an edit that strays outside this workspace corrupts someone else's
/// in-flight task. The deny rules fence this at the permission layer; this
/// assertion pins the matching instruction the worker actually reads.
#[test]
fn claude_md_forbids_touching_sibling_workspaces() {
    let input = sample_input();
    let rendered = claude_md_for(&input);
    assert!(
        rendered.contains("Do not modify files outside this workspace"),
        "CLAUDE.md must fence workers into their own workspace; got:\n{rendered}",
    );
    assert!(
        rendered.contains("Sibling workspaces"),
        "CLAUDE.md must call out sibling workspaces as off-limits; got:\n{rendered}",
    );
}

#[test]
fn claude_md_warns_against_force_tracking_dot_claude() {
    let input = sample_input();
    let rendered = claude_md_for(&input);
    // The CLAUDE.md must remind workers not to override the
    // engine's gitignore — otherwise a worker that runs into a
    // status surprise might `jj file track` the engine plumbing
    // back into its PR, undoing the fix.
    assert!(rendered.contains(".claude/"));
    assert!(rendered.contains("force") || rendered.contains("track"));
}

/// Authoring-side guardrail against reimplementing existing infrastructure.
/// A prior incident saw a sixth hand-rolled Anthropic Messages API client
/// land unflagged. Standard workers must be told to search for an existing
/// implementation before building a cross-cutting capability, and that a
/// genuinely-necessary duplication needs an explicit justification recorded
/// in the PR description.
#[test]
fn claude_md_has_reuse_before_you_build_guardrail() {
    let input = sample_input();
    let rendered = claude_md_for(&input);
    assert!(
        rendered.contains("Reuse before you build"),
        "expected a 'Reuse before you build' section"
    );
    assert!(
        rendered.contains("search the repo for an existing"),
        "expected guidance to search the repo before implementing cross-cutting capabilities"
    );
    assert!(
        rendered.contains("API/HTTP client"),
        "expected the guardrail to name API/HTTP clients as an example cross-cutting capability"
    );
    assert!(
        rendered.contains("say so explicitly") && rendered.contains("PR description with the reason"),
        "expected the justified-exception escape hatch to require an explicit PR description note"
    );
}

/// Reviewer and triage workers never author new code, so the authoring-side
/// reuse guardrail (which references "PR description" justification for
/// implementers) must be scoped to standard workers only.
#[test]
fn reviewer_claude_md_omits_reuse_before_you_build_guardrail() {
    let rendered = crate::pr_review::render_reviewer_claude_md(
        "lease-1",
        "/tmp/ws",
        crate::prompt_fragments::boundaries_and_coordinator_fragment(),
    );
    assert!(
        !rendered.contains("Reuse before you build"),
        "reviewer CLAUDE.md must not include the authoring-side guardrail"
    );
}

#[test]
fn claude_md_pr_section_is_front_and_centre() {
    // The PR rule moved out from after Boundaries and now sits
    // immediately after the intro. If a future edit buries it
    // again, this test will fail and the writer can move it back.
    let input = sample_input();
    let rendered = claude_md_for(&input);
    let pr_offset = rendered
        .find("Pull requests are the deliverable")
        .expect("expected the strengthened PR heading to be present");
    let workspace_offset = rendered
        .find("## Your workspace")
        .expect("expected the workspace heading to be present");
    assert!(
        pr_offset < workspace_offset,
        "PR section must come before `## Your workspace`",
    );
    // Resuming-work guidance must mention how to detect an
    // existing PR rather than just letting the worker open a duplicate.
    assert!(rendered.contains("gh pr list --head"));
    assert!(rendered.contains("not complete until a PR exists"));
    assert!(rendered.contains("PR URL on its own line"));
    // Empty-diff guard: the worker must verify the diff is non-empty
    // before pushing so the engine's empty-diff probe is never needed.
    assert!(
        rendered.contains("jj diff -r @"),
        "CLAUDE.md must remind workers to verify the diff before pushing",
    );
}

#[test]
fn claude_md_has_cube_pr_create_section() {
    let input = sample_input();
    let rendered = claude_md_for(&input);
    assert!(
        rendered.contains("Creating a PR from a jj workspace"),
        "expected a 'Creating a PR from a jj workspace' section",
    );
    assert!(
        rendered.contains("cube pr create"),
        "expected cube pr create to be the canonical PR creation command",
    );
    assert!(
        rendered.contains("cube pr update"),
        "expected cube pr update to be the canonical PR-advancing command",
    );
    assert!(
        !rendered.contains("cube pr ensure"),
        "new CLAUDE.md guidance must not mention the deprecated cube pr ensure",
    );
    assert!(rendered.contains("--branch"), "expected --branch flag guidance",);
    assert!(
        rendered.contains("jj bookmark create"),
        "expected canonical bookmark creation command",
    );
}

#[test]
fn claude_md_explains_no_git_at_workspace_root() {
    // Workers must know why bare `gh` calls fail before reaching for the fix.
    let input = sample_input();
    let rendered = claude_md_for(&input);
    assert!(
        rendered.contains("fatal: not a git repository") || rendered.contains("no `.git/`"),
        "expected an explanation of why bare gh fails in a jj workspace",
    );
}

#[test]
fn claude_md_draft_directive_present_when_enabled() {
    let mut input = sample_input();
    input.draft_pr_mode = true;
    let rendered = claude_md_for(&input);
    assert!(
        rendered.contains("--draft"),
        "CLAUDE.md must include --draft directive when draft_pr_mode is true",
    );
    assert!(
        rendered.contains("cube pr create"),
        "draft directive must reference cube pr create",
    );
}

#[test]
fn claude_md_draft_directive_absent_when_disabled() {
    let input = sample_input(); // draft_pr_mode: false
    let rendered = claude_md_for(&input);
    assert!(
        !rendered.contains("--draft"),
        "CLAUDE.md must NOT include --draft directive when draft_pr_mode is false",
    );
}

#[test]
fn reviewer_claude_md_states_read_only_mandate() {
    let mut input = sample_input();
    input.worker_kind = WorkerKind::Reviewer;
    let rendered = claude_md_for(&input);
    // Must contain the read-only mandate section.
    assert!(
        rendered.contains("Read-only mandate"),
        "reviewer CLAUDE.md must contain read-only mandate section",
    );
    // Must contain both lease id and workspace path — reviewers need both to
    // navigate the workspace that the engine checked out to the PR head.
    assert!(rendered.contains(&input.lease_id));
    assert!(
        rendered.contains(input.workspace_path.to_str().unwrap()),
        "reviewer CLAUDE.md must include workspace path (workspace is checked out to PR head)",
    );
    // Must mention that the workspace is at the PR head.
    assert!(
        rendered.contains("checked out to the PR head"),
        "reviewer CLAUDE.md must mention that the workspace is at the PR head",
    );
    // Must NOT contain the standard PR-required delivery mandate from
    // the implementation worker CLAUDE.md.
    assert!(
        !rendered.contains("Pull requests are the deliverable"),
        "reviewer CLAUDE.md must not include the standard PR-required reminder",
    );
    // Must not instruct the reviewer to create a PR — the tool is listed
    // only as a *forbidden* action, not as a delivery requirement.
    assert!(
        !rendered.contains("A task is not complete until a PR exists"),
        "reviewer CLAUDE.md must not include the implementation PR mandate",
    );
}

#[test]
fn reviewer_claude_md_mentions_allowed_read_only_tools() {
    let mut input = sample_input();
    input.worker_kind = WorkerKind::Reviewer;
    let rendered = claude_md_for(&input);
    assert!(rendered.contains("gh pr diff"), "must mention gh pr diff");
    assert!(rendered.contains("gh pr view"), "must mention gh pr view");
    assert!(rendered.contains("jj log"), "must mention jj log");
}

#[test]
fn standard_claude_md_is_unchanged_by_reviewer_branch() {
    // Verify the reviewer kind change does not affect standard workers.
    let input = sample_input(); // WorkerKind::Standard
    let rendered = claude_md_for(&input);
    assert!(rendered.contains("Pull requests are the deliverable"));
    assert!(rendered.contains("cube pr create"));
    assert!(rendered.contains("real GitHub upstream"));
}
