//! Prompt and rules-file rendering for the post-merge reviewer: the solo,
//! strong-tier reviewer dispatched against a PR's real landed tree after it
//! merges (parent project: "Multi-agent code review" — an automatic
//! post-merge review for large or complex PRs).
//!
//! Unlike a pre-merge leaf (which reports raw findings for a supervisor to
//! consolidate) or the supervisor itself (which consolidates *other*
//! reviewers' reports), a post-merge reviewer both inspects the code and
//! submits the batch's one and only verdict directly — there is no leaf
//! quorum or separate consolidation pass for this topology. It reuses the
//! supervisor's `SupervisorVerdict` output contract (schema and the
//! `boss propose review-verdict` submission command) since its verdict
//! flows through the same unified follow-up-materialisation reconciler
//! ([`boss_engine_core`]'s `review_verdict_apply`, outside this crate) — but
//! its `sources` are always its own driver, never a claim of corroboration
//! from an independently-reported leaf.

use crate::render::{ReviewerReportDestination, render_rubric_section};
use crate::types::ReviewScope;

/// Render the CLAUDE.md for a post-merge reviewer worker.
///
/// Shares the leaf/supervisor read-only posture (design §9): may read the
/// landed tree and PR diff, must never write, push, or post to GitHub. The
/// one permitted write is its verdict, submitted via `boss propose
/// review-verdict` — the same command the supervisor uses.
///
/// See [`crate::render::render_review_worker_claude_md`] for the shared
/// body and the rationale behind its shape.
pub fn render_post_merge_reviewer_claude_md(
    lease_id: &str,
    workspace_path: &str,
    absolute_paths: &str,
    boundaries_and_coordinator: &str,
) -> String {
    crate::render::render_review_worker_claude_md(
        crate::render::ReviewWorkerRoleRules {
            role_heading: "post-merge reviewer",
            intro: "You are running inside a Boss-managed **post-merge reviewer** session. \
                    The engine spawned you in a leased cube workspace after this PR's landed \
                    classification came back large/complex enough to warrant a second, deeper \
                    pass against the real landed tree — independent of whatever the pre-merge \
                    reviewers already found.",
            last_forbidden_bullet: "Running `cube pr create`/`cube pr update` or any Boss PR helper —\n\
               this does NOT include the `boss propose review-verdict` call named\n\
               in your task prompt, which is not a Boss PR helper: it records\n\
               your verdict, it does not touch the PR (which is already merged).",
            permitted_write_section: "**The one permitted write** is the `boss propose review-verdict`\n\
             call your task prompt names. Write your verdict JSON to the body\n\
             file it names, then run exactly that one `boss propose` call to\n\
             submit it. That call is a local call to the engine control socket,\n\
             not a write to the PR or repo, so it does not violate the\n\
             read-only mandate. Do not write anywhere else.\n\
             \n\
             The PR you are reviewing has already merged — there is no PR left to\n\
             comment on or push to, and no revision cycle this pass gates. Your\n\
             verdict becomes a standalone follow-up against the default branch if\n\
             it warrants one; it never blocks or reopens the merged PR itself.\n\
             \n\
             Your feedback stays inside Boss — it is never posted to GitHub.\n\
             \n",
            workspace_extra: "You can read\n\
                 changed files and surrounding context directly — use `Read`, `cat`,\n\
                 `grep`, etc. Confirm you are looking at the actual landed commit\n\
                 (see **Verifying the landed tree** in your task prompt) before\n\
                 forming findings; do not assume the workspace's default checkout\n\
                 is already at that exact commit.\n\
                 \n",
        },
        lease_id,
        workspace_path,
        absolute_paths,
        boundaries_and_coordinator,
    )
}

/// Compose the initial prompt for the sole post-merge reviewer of one batch.
///
/// `destination.target_sha` is the merge commit's SHA for a post-merge
/// batch — the exact landed tree this pass must review, which may differ
/// from whatever ref the workspace happens to be checked out to (the
/// batch's `pr_url` branch may already be deleted post-merge). The
/// **Verifying the landed tree** section tells the reviewer to confirm
/// against it explicitly via `jj`/`gh` rather than trusting the ambient
/// checkout.
pub fn render_post_merge_reviewer_initial_prompt(
    task_name: &str,
    task_description: &str,
    destination: &ReviewerReportDestination,
    scope: ReviewScope,
    repo_slug: &str,
) -> String {
    let rubric = render_rubric_section(&scope);
    let merge_sha = destination.target_sha.as_str();
    let pr_ref = destination
        .pr_url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(destination.pr_url.as_str());
    let verdict_submission_block =
        crate::blocks::render_verdict_submission_block(&destination.batch_id, &destination.body_path);

    format!(
        "# Post-merge PR review\n\
         \n\
         You are the sole reviewer for a landed-code post-merge review. This PR already \
         merged; your job is to review its real landed diff against the base it merged \
         into, independent of any pre-merge review this PR already received, and produce \
         a single structured `SupervisorVerdict` JSON — the same consolidated-verdict shape \
         a pre-merge review's supervisor produces, since you are both the reviewer and the \
         verdict-writer here. You MUST NOT change anything: no commits, no pushes, no `gh` \
         writes, no edits to repo files, no comments on GitHub. The PR is already merged, so \
         none of that would even apply to it — you operate strictly read-only.\n\
         \n\
         ## `gh` requires `--repo` in this workspace\n\
         \n\
         This repo is `{repo_slug}`. Pass `--repo {repo_slug}` on every `gh` command: `gh pr \
         view`, `gh pr diff`, `gh api`, etc. — `gh pr diff` and `gh pr view` both still work \
         on a merged PR.\n\
         \n\
         ## PR under review\n\
         \n\
         **Task:** {task_name}\n\
         \n\
         **Task description:**\n\
         {task_description}\n\
         \n\
         **PR:** {pr_url} (already merged)\n\
         **Merge commit:** `{merge_sha}`\n\
         \n\
         ## Verifying the landed tree\n\
         \n\
         Read against the exact merge commit above, not whatever the workspace's ambient \
         checkout happens to be (the PR's own branch may already be deleted post-merge). \
         Your CLAUDE.md names the workspace path to pass as `jj`'s `-R` flag; confirm and \
         read with:\n\
         \n\
         - `jj log -r {merge_sha}` (with `-R <workspace>`) — confirm the commit is present.\n\
         - `jj file show -r {merge_sha} <path>` (with `-R <workspace>`) — read one file's \
           content exactly as it landed at that commit, regardless of what the working copy \
           shows.\n\
         - `gh pr diff {pr_ref} --repo {repo_slug}` — the PR's own diff, for orientation on \
           what changed (still available after merge).\n\
         \n\
         ## Review steps\n\
         \n\
         1. Get the PR description and diff: `gh pr view {pr_ref} --repo {repo_slug}` and \
            `gh pr diff {pr_ref} --repo {repo_slug}`.\n\
         2. Read the changed files at the merge commit (see **Verifying the landed tree** \
            above) and their surrounding context.\n\
         3. Apply the rubric below to what actually landed — not to the PR's stated intent, \
            and not assuming a pre-merge reviewer already caught everything: this pass \
            exists because the PR was large/complex enough to warrant independent scrutiny \
            against the real landed state.\n\
         4. Produce the `SupervisorVerdict` JSON (schema below).\n\
         \n\
         {rubric}\n\
         ## `revision_warranted`\n\
         \n\
         Set to `true` when at least one finding is `critical`/`high` severity, or any \
         finding is `category: \"regression\"`, `\"duplication\"`, `\"deferred_scope\"`, or \
         `\"agent_isms\"`, regardless of severity.\n\
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
           \"target_sha\": \"{merge_sha}\",\n\
           \"phase\": \"post_merge\",\n\
           \"summary\": \"<one-paragraph overall assessment>\",\n\
           \"revision_warranted\": true,\n\
           \"findings\": [\n\
             {{\n\
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
         `findings` may be empty if the landed code is clean. `contradictions` is always \
         `[]` here — there is no second reviewer to disagree with. `sources` on every \
         finding must be exactly `[\"claude\"]`; omit `location` when it does not apply.\n",
        repo_slug = repo_slug,
        task_name = task_name,
        task_description = task_description,
        pr_url = destination.pr_url,
        merge_sha = merge_sha,
        pr_ref = pr_ref,
        rubric = rubric,
        verdict_submission_block = verdict_submission_block,
        batch_id = destination.batch_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn destination() -> ReviewerReportDestination {
        ReviewerReportDestination::builder()
            .batch_id("rvb_1")
            .pr_url("https://github.com/org/repo/pull/7")
            .target_sha("merge-sha-1")
            .phase(boss_protocol::ReviewBatchPhase::PostMerge)
            .body_path("/tmp/boss-worker-output/exec_pm.verdict.json")
            .build()
    }

    #[test]
    fn post_merge_prompt_submits_verdict_and_names_the_merge_commit() {
        let prompt = render_post_merge_reviewer_initial_prompt(
            "Review change",
            "Inspect the implementation.",
            &destination(),
            ReviewScope::Code,
            "org/repo",
        );
        assert!(prompt.contains("boss propose review-verdict --batch-id rvb_1 --verdict-file"));
        assert!(prompt.contains("merge-sha-1"));
        assert!(prompt.contains("\"phase\": \"post_merge\""));
        assert!(prompt.contains("[\"claude\"]"));
        assert!(prompt.contains("jj file show"));
        assert!(prompt.contains("already merged"));
    }

    #[test]
    fn post_merge_claude_md_authorizes_review_verdict_and_forbids_pr_writes() {
        let rendered = render_post_merge_reviewer_claude_md("lease-1", "/tmp/ws", "", "");
        assert!(rendered.contains("boss propose review-verdict"));
        assert!(rendered.contains("MUST NOT change the PR"));
        assert!(rendered.contains("jj log -R /tmp/ws"));
    }
}
