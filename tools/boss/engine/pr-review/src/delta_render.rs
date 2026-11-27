//! Delta re-review brief renderer for single-reviewer repeat pre-merge rounds.
//!
//! Used for review rounds after the first when a PR runs a single Light-tier
//! reviewer with no consolidating supervisor. The brief carries:
//!
//! 1. The prior round's findings, asking the reviewer to mark each resolved or
//!    unresolved and carry unresolved items forward in `findings`.
//! 2. The delta diff since the last reviewed commit (embedded when within the
//!    line cap, otherwise an explicit compare/`jj diff` command).
//! 3. The PR's full changed-file list.
//! 4. Delta-only scoping (replacing the broad whole-PR re-review wording) paired
//!    with one explicit whole-PR duplication check over the changed-file list.
//!
//! This module takes only plain review inputs and never sees an experiment key.

use crate::brief::ReviewBriefPacket;
use crate::render::{ReviewerReportDestination, render_merged_parent_deletion_block};
use crate::types::{PrReviewContext, ReviewFindingCategory, ReviewFindingSeverity, ReviewScope};

/// One finding from the immediately preceding pre-merge review round.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct PriorReviewFinding {
    /// Stable finding id from the prior verdict (e.g. `C1`), when available.
    pub id: Option<String>,
    pub severity: ReviewFindingSeverity,
    pub category: ReviewFindingCategory,
    pub file: String,
    pub location: Option<String>,
    pub title: String,
    pub detail: String,
}

/// Summary and findings carried forward from the prior pre-merge review round.
#[derive(Debug, Clone, PartialEq, Eq, Default, bon::Builder)]
#[builder(on(String, into))]
pub struct PriorReviewRound {
    /// Head SHA the prior round reviewed, when recorded.
    pub head_sha: Option<String>,
    /// Prior round's overall assessment, when recorded.
    pub summary: Option<String>,
    /// Structured findings from the prior round's `SupervisorVerdict`.
    #[builder(default)]
    pub findings: Vec<PriorReviewFinding>,
    /// Rendered revision instructions from a legacy single-reviewer pass when
    /// no structured verdict payload exists on record.
    pub legacy_revision_instructions: Option<String>,
}

/// Inputs specific to a delta re-review round, paired with [`PrReviewContext`].
#[derive(Debug, Clone, Default, bon::Builder)]
#[builder(on(String, into))]
pub struct DeltaRereviewContext {
    /// Findings from the prior pre-merge review round, if available in storage.
    pub prior_round: Option<PriorReviewRound>,
    /// Unified diff from `last_reviewed_sha` to `target_sha` when pre-fetched
    /// and within the embedding line cap.
    pub delta_diff_content: Option<String>,
    /// Fallback `last_reviewed_sha` from the PR's review-cycle state, used when
    /// GitHub metadata (`PrReviewContext`) could not be pre-fetched and no
    /// prior verdict row recorded a head SHA.
    pub last_reviewed_sha: Option<String>,
    /// The last reviewed commit is no longer a clean ancestor of the target
    /// (rebase, amend, force-push, or a base-branch merge in between), so a
    /// compare against it is not a trustworthy delta. `delta_diff_content`
    /// then holds the full PR diff, and the brief says so.
    #[builder(default)]
    pub history_rewritten: bool,
}

/// Compose the initial prompt for a single-reviewer delta re-review round.
///
/// The reviewer operates read-only and submits a single `SupervisorVerdict`
/// JSON directly through `boss propose review-verdict` (`phase: "pre_merge"`,
/// `sources: ["claude"]`, `contradictions: []`, no `rejected` entries).
pub fn render_delta_rereviewer_initial_prompt(
    brief: &ReviewBriefPacket,
    destination: &ReviewerReportDestination,
    scope: ReviewScope,
    ctx: Option<&PrReviewContext>,
    delta: &DeltaRereviewContext,
    repo_slug: &str,
) -> String {
    let brief_block = crate::brief::render_brief_packet_block(brief);
    let target_sha = destination.target_sha.as_str();
    let pr_url = destination.pr_url.as_str();
    let pr_ref = ctx
        .map(|c| c.pr_number.to_string())
        .or_else(|| pr_url.rsplit('/').next().filter(|s| !s.is_empty()).map(str::to_owned))
        .unwrap_or_else(|| pr_url.to_owned());

    let last_reviewed_sha = ctx
        .and_then(|c| c.last_reviewed_sha.as_deref())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            delta
                .prior_round
                .as_ref()
                .and_then(|r| r.head_sha.as_deref())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| delta.last_reviewed_sha.as_deref().filter(|s| !s.is_empty()));

    let pr_metadata_block = render_pr_metadata_block(ctx, target_sha, last_reviewed_sha);
    let prior_findings_block = render_prior_findings_block(delta.prior_round.as_ref());
    let (delta_diff_step, embedded_delta_diff_section) = render_delta_diff_sections(
        delta.delta_diff_content.as_deref(),
        delta.history_rewritten,
        last_reviewed_sha,
        target_sha,
        &pr_ref,
        repo_slug,
    );
    let merged_parent_deletion_block = ctx
        .map(|c| render_merged_parent_deletion_block(&c.merged_parent_deletions))
        .unwrap_or_default();
    let supersession_flag_block = ctx
        .map(|c| crate::blocks::render_supersession_flag_block(&c.supersession_flags))
        .unwrap_or_default();
    let boss_construct_sweep_block = ctx
        .map(|c| crate::blocks::render_boss_construct_sweep_block(&c.boss_construct_refs))
        .unwrap_or_default();
    let rubric = render_delta_rubric_section(&scope);
    let verdict_submission_block =
        crate::blocks::render_verdict_submission_block(&destination.batch_id, &destination.body_path);

    let last_sha_label = last_reviewed_sha
        .map(|sha| format!("`{sha}`"))
        .unwrap_or_else(|| "the last reviewed commit".to_owned());

    format!(
        "# Delta PR re-review\n\
         \n\
         You are the sole reviewer for a repeat pre-merge review round. Earlier round(s) \
         already reviewed this PR up to {last_sha_label}. Your job is to verify the prior \
         round's findings, review **only the delta** since {last_sha_label}, run one \
         explicit whole-PR duplication check over the changed-file list, and submit a \
         single structured `SupervisorVerdict` JSON directly (there is no second reviewer \
         and no separate supervisor).\n\
         \n\
         You MUST NOT change the PR in any way: no commits, no pushes, no `gh` writes, no \
         edits to repo files or branches, no comments on GitHub. You operate **read-only** \
         on the PR. The ONE write you make is your verdict JSON body file and the single \
         local `\"$BOSS_BIN\" propose review-verdict` call below.\n\
         \n\
         ## `gh` requires `--repo` in this workspace\n\
         \n\
         This repo is `{repo_slug}`. `gh` cannot auto-detect the repo in a jj workspace \
         (there is no `.git` directory at the root — only `.jj/`). Pass `--repo {repo_slug}` \
         on every `gh` command (`gh pr view`, `gh pr diff`, `gh api`, etc.).\n\
         \n\
         ## PR under review\n\
         \n\
         {brief_block}\
         **PR:** {pr_url}\n\
         {pr_metadata_block}\n\
         {prior_findings_block}\
         ## Delta-only review scope\n\
         \n\
         1. **Verify every prior finding first.** For each item in **Prior round findings** \
            above, inspect the delta and current workspace to determine whether it is \
            **resolved** or **unresolved**. In your verdict's `summary`, state the \
            disposition of each prior finding explicitly (for example: `C1: resolved; \
            C2: unresolved — ...`). Any prior finding that is still unresolved MUST be \
            included in your `findings` list so it remains actionable.\n\
         2. **Review only the delta since {last_sha_label}.** Do not re-review unchanged \
            parts of the PR that earlier rounds already accepted unless the new commits \
            modify or break them. Apply the rubric below to the new/changed hunks and the \
            revision ask.\n\
         3. **Whole-PR duplication check — REQUIRED (one explicit step).** Before \
            concluding the PR is clean, scan the full **Changed files** list above for \
            whole-PR duplication defects that a delta-only diff cannot see — specifically, \
            whether a refactor, move, or crate/module extraction left duplicate copies of \
            the same module, type, helper, or file tree in the PR (for example, an old \
            copy left behind alongside a newly extracted crate or module). Check any \
            suspicious file pairs in the workspace and raise a `category: \"duplication\"` \
            finding for any confirmed duplicate. Keep all other scrutiny scoped strictly \
            to the delta and the prior findings.\n\
         \n\
         {merged_parent_deletion_block}\
         {supersession_flag_block}\
         {boss_construct_sweep_block}\
         ## Review steps\n\
         \n\
         1. Your workspace is already checked out to `{target_sha}` — read changed files \
            directly with `Read`, `cat`, `grep`, etc.\n\
         {delta_diff_step}\
         3. Check each prior finding against the delta and workspace and record whether \
            each one is resolved or unresolved.\n\
         4. Run the single **Whole-PR duplication check** over the full **Changed files** \
            list above.\n\
         5. Submit your `SupervisorVerdict` JSON using **Required output** below.\n\
         \n\
         {embedded_delta_diff_section}\
         {rubric}\n\
         ## `revision_warranted`\n\
         \n\
         Set to `true` when there is at least one finding at `critical`/`high` severity, \
         or any finding with `category: \"regression\"`, `\"duplication\"`, \
         `\"deferred_scope\"`, or `\"agent_isms\"`, regardless of severity. Set to `false` \
         when every prior finding is resolved and the delta is clean (or has only \
         `medium`/`low` non-gating observations).\n\
         \n\
         {verdict_submission_block}\
         \n\
         This is static analysis only. Do not run builds, tests, formatters, generators, or\n\
         executable code.\n\
         \n\
         Schema:\n\
         \n\
         ```jsonc\n\
         {{\n\
           \"batch_id\": \"{batch_id}\",\n\
           \"pr_url\": \"{pr_url}\",\n\
           \"target_sha\": \"{target_sha}\",\n\
           \"phase\": \"pre_merge\",\n\
           \"summary\": \"<prior finding dispositions (resolved/unresolved) + concise delta assessment>\",\n\
           \"revision_warranted\": false,\n\
           \"findings\": [\n\
             {{\n\
               \"id\": \"C1\",\n\
               \"severity\": \"critical | high | medium | low\",\n\
               \"category\": \"correctness | regression | architecture | readability | tests | edgecase | duplication | deferred_scope | agent_isms\",\n\
               \"confidence\": \"high | medium | low\",\n\
               \"file\": \"path/to/file.rs\",\n\
               \"location\": \"fn foo, ~L42\",\n\
               \"title\": \"<short scannable title>\",\n\
               \"detail\": \"<concrete description + what to change>\",\n\
               \"sources\": [\"claude\"]\n\
             }}\n\
           ],\n\
           \"contradictions\": []\n\
         }}\n\
         ```\n\
         \n\
         `findings` may be empty when every prior finding is resolved and the delta is \
         clean. `contradictions` is always `[]` here — there is no second reviewer to \
         disagree with. Give every finding a unique `id` (`C1`, `C2`, ...); `sources` on \
         every finding must be exactly `[\"claude\"]`; omit `location` when it does not \
         apply. There are no separate leaf reports in a delta re-review, so omit `rejected`.\n",
        last_sha_label = last_sha_label,
        repo_slug = repo_slug,
        brief_block = brief_block,
        pr_url = pr_url,
        pr_metadata_block = pr_metadata_block,
        prior_findings_block = prior_findings_block,
        merged_parent_deletion_block = merged_parent_deletion_block,
        supersession_flag_block = supersession_flag_block,
        boss_construct_sweep_block = boss_construct_sweep_block,
        target_sha = target_sha,
        delta_diff_step = delta_diff_step,
        embedded_delta_diff_section = embedded_delta_diff_section,
        rubric = rubric,
        verdict_submission_block = verdict_submission_block,
        batch_id = destination.batch_id,
    )
}

fn render_pr_metadata_block(
    ctx: Option<&PrReviewContext>,
    target_sha: &str,
    last_reviewed_sha: Option<&str>,
) -> String {
    let base_line = ctx
        .map(|c| format!("**Base commit:** `{}`  \n", c.base_sha))
        .unwrap_or_default();
    let last_line = match last_reviewed_sha {
        Some(sha) => format!("**Last reviewed commit:** `{sha}`  \n"),
        None => "**Last reviewed commit:** *(unavailable)*  \n".to_owned(),
    };
    let head_line = format!("**Head commit:** `{target_sha}`  \n");
    let files_block = match ctx {
        Some(c) if !c.changed_files.is_empty() => {
            let files = c
                .changed_files
                .iter()
                .map(|f| format!("- `{f}`"))
                .collect::<Vec<_>>()
                .join("\n");
            format!("**Changed files ({}):**\n{files}\n", c.changed_files.len())
        }
        _ => "**Changed files:** *(unavailable — inspect with `gh pr view --json files`)*\n".to_owned(),
    };
    format!("{base_line}{last_line}{head_line}{files_block}")
}

fn render_prior_findings_block(prior_round: Option<&PriorReviewRound>) -> String {
    let mut out = String::from("## Prior round findings\n\n");
    let Some(round) = prior_round else {
        out.push_str(
            "No prior-round findings are recorded in storage for this PR. Review the delta \
             since the last reviewed commit and the revision ask above.\n\n",
        );
        return out;
    };
    if let Some(sha) = round.head_sha.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("**Prior reviewed commit:** `{sha}`\n\n"));
    }
    if let Some(summary) = round.summary.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push_str(&format!("**Prior review summary:** {}\n\n", summary.trim()));
    }
    if !round.findings.is_empty() {
        out.push_str(
            "Mark each finding below **resolved** or **unresolved** in your verdict summary, \
             and include every still-unresolved finding in `findings`:\n\n",
        );
        for (index, finding) in round.findings.iter().enumerate() {
            let default_id = format!("F{}", index + 1);
            let id = finding.id.as_deref().unwrap_or(&default_id);
            let loc = finding
                .location
                .as_deref()
                .map(|l| format!(" ({l})"))
                .unwrap_or_default();
            out.push_str(&format!(
                "### `{id}` [{severity}] {title}\n\
                 **File:** `{file}`{loc}  \n\
                 **Category:** {category}\n\n\
                 {detail}\n\n",
                severity = finding.severity.as_str(),
                title = finding.title,
                file = finding.file,
                category = finding.category.as_str(),
                detail = finding.detail,
            ));
        }
    } else if let Some(instructions) = round
        .legacy_revision_instructions
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        out.push_str(
            "The prior round produced the revision instructions below. Mark each item \
             **resolved** or **unresolved** in your verdict summary, and carry every \
             still-unresolved item forward in `findings`:\n\n",
        );
        out.push_str(instructions.trim());
        out.push_str("\n\n");
    } else {
        out.push_str(
            "The prior review round recorded no findings (clean pass). Focus on the delta \
             pushed since then and the single whole-PR duplication check.\n\n",
        );
    }
    out
}

/// Reviewer note that applies to every delta brief: upstream changes that
/// arrived through the base branch were not written by this PR.
const BASE_BRANCH_SCOPE_NOTE: &str = "Changes that came in from the base branch (a rebase onto, or merge of, the \
     base) are out of scope: do not raise findings against upstream code this PR did not write.";

fn render_delta_diff_sections(
    diff: Option<&str>,
    history_rewritten: bool,
    last_reviewed_sha: Option<&str>,
    target_sha: &str,
    pr_ref: &str,
    repo_slug: &str,
) -> (String, String) {
    if history_rewritten {
        let rewritten_note = format!(
            "The PR's history was rewritten since the last review (rebase, amend, force-push, or a \
             base-branch merge), so a delta against the last reviewed commit would be misleading. \
             Review the **full PR diff** instead and verify the prior findings against it. \
             {BASE_BRANCH_SCOPE_NOTE}"
        );
        if let Some(diff) = diff {
            let step = "2. The PR history was rewritten, so the **full PR diff** is pre-embedded in \
                        **Full PR diff** below — no need to fetch it separately.\n"
                .to_owned();
            let section =
                format!("## Full PR diff (history rewritten)\n\n{rewritten_note}\n\n```diff\n{diff}\n```\n\n");
            return (step, section);
        }
        let step = format!(
            "2. The PR history was rewritten since the last review, so do NOT diff against the last \
             reviewed commit. Fetch the full PR diff (`gh pr diff {pr_ref} --repo {repo_slug}`) and review \
             it, verifying the prior findings against it. {BASE_BRANCH_SCOPE_NOTE}\n"
        );
        return (step, String::new());
    }

    if let Some(diff) = diff {
        let step = "2. The delta diff since the last reviewed commit is pre-embedded in \
                    **Delta diff** below — no need to fetch it separately.\n"
            .to_owned();
        let section = format!(
            "## Delta diff\n\
             \n\
             Pre-fetched diff since the last reviewed commit. Review only these changes \
             (plus the prior findings and the one whole-PR duplication check). \
             {BASE_BRANCH_SCOPE_NOTE}\n\
             \n\
             ```diff\n\
             {diff}\n\
             ```\n\
             \n"
        );
        return (step, section);
    }

    let step = match last_reviewed_sha {
        Some(last_sha) => format!(
            "2. Fetch the delta diff since `{last_sha}`: \
             `gh api repos/{repo_slug}/compare/{last_sha}...{target_sha} -H \"Accept: application/vnd.github.diff\"`. \
             Check first that `{last_sha}` is still an ancestor of `{target_sha}` (the compare `status` is \
             `ahead`); if the PR was rebased, amended or had its base merged in, review the full PR diff \
             (`gh pr diff {pr_ref} --repo {repo_slug}`) instead. {BASE_BRANCH_SCOPE_NOTE}\n"
        ),
        None => format!(
            "2. Fetch the PR diff (`gh pr diff {pr_ref} --repo {repo_slug}`) and inspect recent commits \
             with `jj log -R <workspace>`.\n"
        ),
    };
    (step, String::new())
}

fn render_delta_rubric_section(scope: &ReviewScope) -> String {
    match scope {
        ReviewScope::Code => "## Delta review rubric — code PR\n\
             \n\
             Apply a **high bar** on the delta and prior findings: push back only on real \
             problems, not stylistic preferences. Every finding must name a file and state \
             **concretely what to change**.\n\
             \n\
             Check the delta for:\n\
             \n\
             - **Unresolved prior findings** — prior findings that were skipped, only \
               partially fixed, or deferred with a TODO/follow-up instead of fixed.\n\
             - **Correctness and edge cases** (`correctness`, `edgecase`) — bugs, broken \
               invariants, mishandled errors, or new races introduced by the delta.\n\
             - **Regressions and deletions** (`regression`) — functionality or merged-parent \
               surfaces dropped by the delta without a verified design-doc citation.\n\
             - **Whole-PR duplication** (`duplication`) — run the single explicit check over \
               the **Changed files** list for left-behind duplicate copies/modules, plus any \
               duplicated infrastructure introduced in the delta.\n\
             - **Deferred-scope hygiene** (`deferred_scope`) — revision-ask items silently \
               dropped or deferred only in prose without an engine-recorded `[deferred-scope]` \
               declaration (manual/interactive verification a headless worker cannot perform \
               is exempt with a plain prose note).\n\
             - **Agent-isms** (`agent_isms`) — bare Boss work-item ids (`T<n>`/`P<n>`), \
               \"the operator\"/actor references, or lineage-narrating code comments added by \
               the delta.\n\
             - **Tests** (`tests`) — missing or weakened tests for behaviour changed by the \
               delta.\n"
            .to_owned(),
        ReviewScope::DocsOnly => "## Delta review rubric — docs-only PR\n\
             \n\
             This PR contains only documentation files. Apply the **light rubric** to the \
             delta and prior findings only — skip code-review concerns.\n\
             \n\
             - Confirm each prior finding is resolved or carry it forward.\n\
             - Check the delta for structure, completeness, internal consistency, and \
               required design-doc sections where applicable.\n\
             - Run the single **Whole-PR duplication check** over the **Changed files** list \
               to confirm no duplicate document copy was left behind.\n"
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::render_reviewer_initial_prompt;

    fn sample_destination() -> ReviewerReportDestination {
        ReviewerReportDestination::builder()
            .batch_id("rvb_delta_1")
            .pr_url("https://github.com/org/repo/pull/42")
            .target_sha("head-sha-2")
            .phase(boss_protocol::ReviewBatchPhase::PreMerge)
            .body_path("/tmp/boss-worker-output/exec_delta_1.review-result.json")
            .build()
    }

    fn sample_ctx() -> PrReviewContext {
        PrReviewContext::builder()
            .pr_number(42)
            .base_sha("base-sha-0")
            .head_sha("head-sha-2")
            .changed_files(vec![
                "tools/boss/engine/pr-review/src/delta_render.rs".to_owned(),
                "tools/boss/engine/completion/src/pr_transition.rs".to_owned(),
            ])
            .last_reviewed_sha("head-sha-1")
            .build()
    }

    fn sample_prior_round() -> PriorReviewRound {
        PriorReviewRound::builder()
            .head_sha("head-sha-1")
            .summary("Round 1 found a missing stale-head check.")
            .findings(vec![
                PriorReviewFinding::builder()
                    .id("C1")
                    .severity(ReviewFindingSeverity::High)
                    .category(ReviewFindingCategory::Correctness)
                    .file("tools/boss/engine/completion/src/pr_transition.rs")
                    .location("finalize_pr_transition, ~L435")
                    .title("Missing stale-head check on repeat review")
                    .detail("Verify the head SHA before minting a revision.")
                    .build(),
            ])
            .build()
    }

    #[test]
    fn delta_prompt_carries_prior_findings_delta_diff_changed_files_and_duplication_check() {
        let brief = ReviewBriefPacket::from_task("Address review findings", "Fix C1 from round 1.");
        let delta = DeltaRereviewContext::builder()
            .prior_round(sample_prior_round())
            .delta_diff_content("diff --git a/src/a.rs b/src/a.rs\n+check_stale_head();\n")
            .build();
        let prompt = render_delta_rereviewer_initial_prompt(
            &brief,
            &sample_destination(),
            ReviewScope::Code,
            Some(&sample_ctx()),
            &delta,
            "org/repo",
        );

        // Prior findings & resolution requirement.
        assert!(prompt.contains("## Prior round findings"), "{prompt}");
        assert!(prompt.contains("`C1` [high] Missing stale-head check on repeat review"));
        assert!(prompt.contains("Verify the head SHA before minting a revision."));
        assert!(prompt.contains("Mark each finding below **resolved** or **unresolved**"));

        // Delta diff embedded & delta-only framing replacing whole-PR wording.
        assert!(prompt.contains("## Delta diff"), "{prompt}");
        assert!(prompt.contains("+check_stale_head();"));
        assert!(prompt.contains("review **only the delta** since `head-sha-1`"));
        assert!(
            !prompt.contains("Do not scope your review to a diff-only view"),
            "delta brief must replace the broad non-diff-only wording"
        );

        // Changed-file list and explicit whole-PR duplication check.
        assert!(prompt.contains("**Changed files (2):**"));
        assert!(prompt.contains("- `tools/boss/engine/pr-review/src/delta_render.rs`"));
        assert!(prompt.contains("- `tools/boss/engine/completion/src/pr_transition.rs`"));
        assert!(prompt.contains("Whole-PR duplication check — REQUIRED"));
        assert!(prompt.contains("left duplicate copies"));

        // Solo pre-merge verdict submission contract.
        assert!(prompt.contains("\"$BOSS_BIN\" propose review-verdict --batch-id rvb_delta_1"));
        assert!(prompt.contains("\"phase\": \"pre_merge\""));
        assert!(prompt.contains("\"sources\": [\"claude\"]"));
        assert!(prompt.contains("\"contradictions\": []"));
        assert!(prompt.contains("omit `rejected`"));

        // Prompt must never reveal experiment machinery.
        let lower = prompt.to_ascii_lowercase();
        for forbidden in ["experiment", "treatment", "control arm", "delta_rereview", "a/b"] {
            assert!(
                !lower.contains(forbidden),
                "delta brief must not leak experiment term `{forbidden}`"
            );
        }
    }

    #[test]
    fn delta_prompt_falls_back_to_compare_command_when_delta_diff_not_embedded() {
        let brief = ReviewBriefPacket::from_task("Address review findings", "Fix C1.");
        let delta = DeltaRereviewContext::builder()
            .prior_round(sample_prior_round())
            .build();
        let prompt = render_delta_rereviewer_initial_prompt(
            &brief,
            &sample_destination(),
            ReviewScope::Code,
            Some(&sample_ctx()),
            &delta,
            "org/repo",
        );

        assert!(!prompt.contains("## Delta diff"));
        assert!(
            prompt.contains(
                "gh api repos/org/repo/compare/head-sha-1...head-sha-2 -H \"Accept: application/vnd.github.diff\""
            ),
            "expected compare command fallback:\n{prompt}"
        );
        assert!(prompt.contains("is still an ancestor of `head-sha-2`"), "{prompt}");
    }

    #[test]
    fn delta_prompt_handles_clean_legacy_and_missing_prior_rounds() {
        let brief = ReviewBriefPacket::from_task("Fix CI", "Make tests green.");
        let dest = sample_destination();
        let ctx = sample_ctx();

        let clean = DeltaRereviewContext::builder()
            .prior_round(PriorReviewRound::builder().head_sha("head-sha-1").build())
            .build();
        let clean_prompt =
            render_delta_rereviewer_initial_prompt(&brief, &dest, ReviewScope::Code, Some(&ctx), &clean, "org/repo");
        assert!(clean_prompt.contains("recorded no findings (clean pass)"));

        let legacy = DeltaRereviewContext::builder()
            .prior_round(
                PriorReviewRound::builder()
                    .head_sha("head-sha-1")
                    .legacy_revision_instructions("### [high] Old bug\nFix foo.")
                    .build(),
            )
            .build();
        let legacy_prompt =
            render_delta_rereviewer_initial_prompt(&brief, &dest, ReviewScope::Code, Some(&ctx), &legacy, "org/repo");
        assert!(legacy_prompt.contains("### [high] Old bug"));

        let missing = DeltaRereviewContext::default();
        let missing_prompt = render_delta_rereviewer_initial_prompt(
            &brief,
            &dest,
            ReviewScope::DocsOnly,
            Some(&ctx),
            &missing,
            "org/repo",
        );
        assert!(missing_prompt.contains("No prior-round findings are recorded"));
        assert!(missing_prompt.contains("## Delta review rubric — docs-only PR"));
    }

    #[test]
    fn control_revision_prompt_keeps_original_non_diff_only_wording() {
        let brief = ReviewBriefPacket::from_task("Address review findings", "Fix C1.");
        let control = render_reviewer_initial_prompt(
            &brief,
            "https://github.com/org/repo/pull/42",
            "/tmp/out.json",
            ReviewScope::Code,
            Some(&sample_ctx()),
            "org/repo",
        );
        assert!(
            control.contains("Do not scope your review to a diff-only view"),
            "control/first-round prompt must remain unchanged"
        );
    }

    fn render_with(delta: &DeltaRereviewContext) -> String {
        render_delta_rereviewer_initial_prompt(
            &ReviewBriefPacket::from_task("Address review findings", "Fix C1 from round 1."),
            &sample_destination(),
            ReviewScope::Code,
            Some(&sample_ctx()),
            delta,
            "org/repo",
        )
    }

    #[test]
    fn rebased_or_amended_history_reviews_the_full_pr_diff_instead_of_a_delta() {
        let prompt = render_with(
            &DeltaRereviewContext::builder()
                .prior_round(sample_prior_round())
                .delta_diff_content("diff --git a/src/a.rs b/src/a.rs\n+whole_pr_change();\n")
                .history_rewritten(true)
                .build(),
        );
        assert!(prompt.contains("## Full PR diff (history rewritten)"), "{prompt}");
        assert!(prompt.contains("+whole_pr_change();"));
        assert!(!prompt.contains("## Delta diff"));
        assert!(prompt.contains("out of scope: do not raise findings against upstream code"));
    }

    #[test]
    fn rewritten_history_without_an_embedded_diff_never_points_at_a_compare_range() {
        let prompt = render_with(
            &DeltaRereviewContext::builder()
                .prior_round(sample_prior_round())
                .history_rewritten(true)
                .build(),
        );
        assert!(prompt.contains("gh pr diff 42 --repo org/repo"), "{prompt}");
        assert!(!prompt.contains("compare/head-sha-1...head-sha-2"), "{prompt}");
        assert!(prompt.contains("do NOT diff against the last reviewed commit"));
    }

    #[test]
    fn a_linear_delta_tells_the_reviewer_base_branch_changes_are_out_of_scope() {
        let prompt = render_with(
            &DeltaRereviewContext::builder()
                .prior_round(sample_prior_round())
                .delta_diff_content("diff --git a/src/a.rs b/src/a.rs\n+x();\n")
                .build(),
        );
        assert!(prompt.contains("## Delta diff"));
        assert!(prompt.contains("came in from the base branch"));
    }
}
