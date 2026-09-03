//! Prompt and rules-file rendering for the supervisor worker: the
//! consolidating role that reads every reported leaf review for one batch and
//! produces a single [`crate::supervisor_types::SupervisorVerdict`].

use crate::render::ReviewerReportDestination;
use crate::supervisor_types::SupervisorSourceRole;
use crate::types::ReviewerReport;

/// One leaf's accepted report, paired with which leaf produced it. The
/// supervisor prompt embeds these directly so it never has to re-fetch them
/// itself.
#[derive(Debug, Clone)]
pub struct SupervisorReportInput {
    pub role: SupervisorSourceRole,
    pub report: ReviewerReport,
}

/// Render the CLAUDE.md for a supervisor worker.
///
/// A supervisor shares the leaf reviewer's read-only posture (design §9):
/// it may read the PR diff and workspace files — needed to independently
/// adjudicate a contradiction between leaves — but must never write, push,
/// or post to GitHub. The one permitted write is its consolidated verdict,
/// submitted via `boss propose review-verdict`.
pub fn render_supervisor_claude_md(
    lease_id: &str,
    workspace_path: &str,
    absolute_paths: &str,
    boundaries_and_coordinator: &str,
) -> String {
    format!(
        "# Boss supervisor rules\n\
         \n\
         You are running inside a Boss-managed **review supervisor** session. \
         The engine spawned you in a leased cube workspace checked out to the \
         PR head, after at least two of three independent reviewers reported \
         on this PR.\n\
         \n\
         ## Read-only mandate (HARD CONSTRAINT)\n\
         \n\
         **You MUST NOT change the PR or its branch in any way.**\n\
         \n\
         Forbidden actions (tool calls for these are denied):\n\
         \n\
         - Editing any file, or writing any file inside this workspace or any\n\
           sibling worker workspace (`Edit`, `Write` under the workspaces root).\n\
         - Committing or pushing (`jj git push`, `git push`).\n\
         - Opening, merging, closing, editing, or commenting on a PR\n\
           (`gh pr create/merge/close/edit/comment/review`).\n\
         - Interacting with GitHub issues in any write capacity.\n\
         - Running `cube pr create`/`cube pr update` or any Boss PR helper —\n\
           this does NOT include the `boss propose review-verdict` call named\n\
           in your task prompt, which is not a Boss PR helper: it records\n\
           your consolidated verdict, it does not touch the PR.\n\
         \n\
         **The one permitted write** is the `boss propose review-verdict`\n\
         call your task prompt names. Write your verdict JSON to the body\n\
         file it names, then run exactly that one `boss propose` call to\n\
         submit it. That call is a local call to the engine control socket,\n\
         not a write to the PR or repo, so it does not violate the\n\
         read-only mandate. Do not write anywhere else.\n\
         \n\
         You MAY read the checked-out workspace to independently verify a\n\
         claim two leaves disagree about (e.g. whether a check was actually\n\
         removed, or moved) — that is exactly what the read-only mandate\n\
         still permits: reading, never changing.\n\
         \n\
         ## `gh` requires `--repo` in this workspace\n\
         \n\
         `gh` cannot auto-detect the repo in a jj workspace (there is no\n\
         `.git` directory at the root — only `.jj/`). Your initial task\n\
         prompt states the concrete repo slug. Pass `--repo <owner/repo>`\n\
         on every `gh` command.\n\
         \n\
         ## Your workspace\n\
         \n\
         - Workspace path: `{workspace_path}`\n\
         - Cube lease id: `{lease}`\n\
         \n\
         The workspace is already checked out to the PR head. Lease held for\n\
         the lifetime of this run. Do not lease, release, or mutate cube\n\
         state.\n\
         \n\
         {absolute_paths}\
         \n\
         ## VCS (read-only)\n\
         \n\
         Use `jj` for read-only navigation. Do not push or modify history.\n\
         Your session's current working directory may not be inside the\n\
         checkout, so bare `jj log`/`jj show`/`jj diff` can fail with no `.jj`\n\
         found — always pass `-R {workspace_path}` explicitly:\n\
         \n\
         - `jj log -R {workspace_path}`, `jj show -R {workspace_path}`,\n\
           `jj diff -R {workspace_path}` — browse history and diffs.\n\
         - `gh pr diff <url>` — fetch the PR diff.\n\
         - `gh pr view <url>` — read the PR description.\n\
         \n\
         {boundaries_and_coordinator}",
        lease = lease_id,
        workspace_path = workspace_path,
        absolute_paths = absolute_paths,
        boundaries_and_coordinator = boundaries_and_coordinator,
    )
}

/// Render one leaf report as a labeled, embedded JSON block for the
/// supervisor prompt.
fn render_leaf_report_block(input: &SupervisorReportInput) -> String {
    let findings_json = serde_json::to_string_pretty(&input.report.findings).unwrap_or_else(|_| "[]".to_owned());
    format!(
        "### {role} reviewer\n\
         \n\
         **Summary:** {summary}\n\
         \n\
         **Coverage:** inspected {inspected} file(s); omitted {omitted} file(s); \
         limitations: {limitations}\n\
         \n\
         **Findings:**\n\
         \n\
         ```json\n\
         {findings_json}\n\
         ```\n\
         \n",
        role = input.role,
        summary = input.report.summary,
        inspected = input.report.coverage.files_inspected.len(),
        omitted = input.report.coverage.files_omitted.len(),
        limitations = if input.report.coverage.limitations.is_empty() {
            "none reported".to_owned()
        } else {
            input.report.coverage.limitations.join("; ")
        },
        findings_json = findings_json,
    )
}

/// Compose the initial prompt for a supervisor batch member.
///
/// `reports` holds every leaf report the engine accepted for this batch — at
/// least two (a batch is never dispatched to a supervisor with fewer), and
/// possibly all three. A missing third leaf (it exhausted its retry without
/// reporting) is named explicitly so the supervisor does not read silence as
/// "that reviewer found nothing".
pub fn render_supervisor_initial_prompt(
    task_name: &str,
    task_description: &str,
    destination: &ReviewerReportDestination,
    reports: &[SupervisorReportInput],
    repo_slug: &str,
) -> String {
    let reported_roles: Vec<SupervisorSourceRole> = reports.iter().map(|r| r.role).collect();
    let missing_roles: Vec<&'static str> = [
        SupervisorSourceRole::Claude,
        SupervisorSourceRole::Codex,
        SupervisorSourceRole::Grok,
    ]
    .into_iter()
    .filter(|role| !reported_roles.contains(role))
    .map(SupervisorSourceRole::as_str)
    .collect();
    let missing_block = if missing_roles.is_empty() {
        String::new()
    } else {
        format!(
            "**Missing reviewer(s):** {} did not report (exhausted its retry without \
             submitting). Treat this as \"no data from that reviewer\", never as \"that \
             reviewer found nothing\" — do not lower confidence on a finding just because \
             it lacks a source you never actually had.\n\n",
            missing_roles.join(", "),
        )
    };

    let leaf_reports_block = reports
        .iter()
        .map(render_leaf_report_block)
        .collect::<Vec<_>>()
        .join("");

    format!(
        "# PR review consolidation\n\
         \n\
         You are the consolidating supervisor for an automated PR review. \
         {count} independent leaf reviewer(s) have already inspected this PR \
         and reported their raw findings, reproduced below. Your ONLY job is \
         to read them, reconcile them, and produce a single structured \
         `SupervisorVerdict` JSON. You MUST NOT change the PR in any way: no \
         commits, no pushes, no `gh` writes, no edits to repo files or \
         branches, no comments on GitHub.\n\
         \n\
         ## `gh` requires `--repo` in this workspace\n\
         \n\
         This repo is `{repo_slug}`. Pass `--repo {repo_slug}` on every `gh` \
         command.\n\
         \n\
         ## PR under review\n\
         \n\
         **Task:** {task_name}\n\
         \n\
         **Task description:**\n\
         {task_description}\n\
         \n\
         **PR:** {pr_url}\n\
         \n\
         {missing_block}\
         ## Leaf reviewer reports\n\
         \n\
         {leaf_reports_block}\
         ## Your job\n\
         \n\
         1. **Semantic dedup with source attribution.** Two or three leaves \
            often flag the same underlying defect in different words. Merge \
            those into ONE consolidated finding and list every leaf that \
            raised it in `sources`. Do not emit near-duplicate findings as \
            separate entries just because the wording differs — a defect two \
            reviewers independently found is stronger signal than one, and \
            that only shows up if you actually merge them.\n\
         2. **Contradiction handling.** When leaves disagree about the same \
            file or claim (one says a check was removed, another says it \
            moved; one calls something a regression, another disputes it), \
            do not silently pick a side. Record it as a `contradictions` \
            entry naming every position. Where you can settle it — re-read \
            the actual file at the PR head, which your workspace has \
            checked out — do so and record what you found in `resolution`. \
            Where you genuinely cannot settle it, say so and leave \
            `resolved_in_favor_of` unset rather than guessing.\n\
         3. **Independent judgment, not a vote.** A finding raised by only \
            one leaf can still be correct — do not discard it for lacking \
            corroboration. Conversely, corroboration from multiple leaves \
            does not make a weak finding strong; use your own judgment on \
            the merits, the same high bar a single reviewer would apply.\n\
         4. **`revision_warranted`.** Set to `true` when at least one \
            consolidated finding is `critical`/`high` severity, or any \
            finding is `category: \"regression\"`, `\"duplication\"`, \
            `\"deferred_scope\"`, or `\"agent_isms\"`, regardless of \
            severity.\n\
         \n\
         ## Required output — CRITICAL\n\
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
         do not edit repository files or publish anything.\n\
         \n\
         Schema:\n\
         \n\
         ```jsonc\n\
         {{\n\
           \"batch_id\": \"{batch_id}\",\n\
           \"pr_url\": \"{pr_url}\",\n\
           \"target_sha\": \"{target_sha}\",\n\
           \"phase\": \"{phase}\",\n\
           \"summary\": \"<one-paragraph overall assessment, written for a human who will not read the leaf reports>\",\n\
           \"revision_warranted\": true,\n\
           \"findings\": [\n\
             {{\n\
               \"severity\": \"critical | high | medium | low\",\n\
               \"category\": \"correctness | regression | architecture | readability | tests | edgecase | duplication | deferred_scope | agent_isms\",\n\
               \"confidence\": \"high | medium | low\",\n\
               \"file\": \"path/to/file.rs\",\n\
               \"location\": \"fn foo, ~L42\",\n\
               \"title\": \"<short scannable title>\",\n\
               \"detail\": \"<your consolidated description + what to change>\",\n\
               \"sources\": [\"claude\", \"codex\"]\n\
             }}\n\
           ],\n\
           \"contradictions\": [\n\
             {{\n\
               \"file\": \"path/to/file.rs\",\n\
               \"location\": \"fn foo, ~L42\",\n\
               \"description\": \"<what the leaves disagree about>\",\n\
               \"positions\": [\n\
                 {{ \"role\": \"grok\", \"claim\": \"<what this leaf claimed>\" }},\n\
                 {{ \"role\": \"codex\", \"claim\": \"<what this leaf claimed>\" }}\n\
               ],\n\
               \"resolution\": \"<how you resolved it and why>\",\n\
               \"resolved_in_favor_of\": \"codex\"\n\
             }}\n\
           ]\n\
         }}\n\
         ```\n\
         \n\
         `findings` may be empty only if every leaf report was clean. `contradictions` may be\n\
         empty when nothing conflicted. Omit `location` when it does not apply, and omit\n\
         `resolved_in_favor_of` when the disagreement is genuinely unresolved. `role` in\n\
         `sources`/`positions` must be exactly `\"claude\"`, `\"codex\"`, or `\"grok\"` — never\n\
         `\"supervisor\"`.\n",
        count = reports.len(),
        repo_slug = repo_slug,
        task_name = task_name,
        task_description = task_description,
        pr_url = destination.pr_url,
        missing_block = missing_block,
        leaf_reports_block = leaf_reports_block,
        body_path = destination.body_path,
        batch_id = destination.batch_id,
        target_sha = destination.target_sha,
        phase = destination.phase.as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        ReviewCoverage, ReviewFindingCategory, ReviewFindingConfidence, ReviewFindingSeverity, ReviewerReportFinding,
    };

    fn sample_report(role: SupervisorSourceRole) -> SupervisorReportInput {
        SupervisorReportInput {
            role,
            report: ReviewerReport::builder()
                .batch_id("rvb_1")
                .pr_url("https://github.com/org/repo/pull/7")
                .target_sha("head_7")
                .phase(boss_protocol::ReviewBatchPhase::PreMerge)
                .summary(format!("{role} found one issue."))
                .coverage(
                    ReviewCoverage::builder()
                        .files_inspected(vec!["src/lib.rs".to_owned()])
                        .files_omitted(Vec::<String>::new())
                        .limitations(Vec::<String>::new())
                        .build(),
                )
                .findings(vec![
                    ReviewerReportFinding::builder()
                        .severity(ReviewFindingSeverity::High)
                        .category(ReviewFindingCategory::Correctness)
                        .confidence(ReviewFindingConfidence::High)
                        .file("src/lib.rs")
                        .title("Unchecked index")
                        .problem("Index may be out of bounds.")
                        .impact("Panics on empty input.")
                        .suggested_fix("Bounds-check before indexing.")
                        .static_evidence("No guard before the index expression.")
                        .needs_runtime_verification(false)
                        .build(),
                ])
                .build(),
        }
    }

    #[test]
    fn supervisor_prompt_embeds_all_reports_and_submit_command() {
        let destination = ReviewerReportDestination::builder()
            .batch_id("rvb_1")
            .pr_url("https://github.com/org/repo/pull/7")
            .target_sha("head_7")
            .phase(boss_protocol::ReviewBatchPhase::PreMerge)
            .body_path("/tmp/boss-worker-output/exec_sup.verdict.json")
            .build();
        let reports = vec![
            sample_report(SupervisorSourceRole::Claude),
            sample_report(SupervisorSourceRole::Codex),
        ];
        let prompt = render_supervisor_initial_prompt(
            "Review change",
            "Inspect the implementation.",
            &destination,
            &reports,
            "org/repo",
        );
        assert!(prompt.contains("### claude reviewer"));
        assert!(prompt.contains("### codex reviewer"));
        assert!(prompt.contains("boss propose review-verdict --batch-id rvb_1 --verdict-file"));
        assert!(prompt.contains("Semantic dedup with source attribution"));
        assert!(prompt.contains("Contradiction handling"));
        assert!(prompt.contains("grok did not report"));
    }

    #[test]
    fn supervisor_prompt_omits_missing_block_when_all_three_reported() {
        let destination = ReviewerReportDestination::builder()
            .batch_id("rvb_1")
            .pr_url("https://github.com/org/repo/pull/7")
            .target_sha("head_7")
            .phase(boss_protocol::ReviewBatchPhase::PreMerge)
            .body_path("/tmp/boss-worker-output/exec_sup.verdict.json")
            .build();
        let reports = vec![
            sample_report(SupervisorSourceRole::Claude),
            sample_report(SupervisorSourceRole::Codex),
            sample_report(SupervisorSourceRole::Grok),
        ];
        let prompt = render_supervisor_initial_prompt(
            "Review change",
            "Inspect the implementation.",
            &destination,
            &reports,
            "org/repo",
        );
        assert!(!prompt.contains("did not report"));
    }

    #[test]
    fn supervisor_claude_md_authorizes_review_verdict_and_forbids_pr_writes() {
        let rendered = render_supervisor_claude_md("lease-1", "/tmp/ws", "", "");
        assert!(rendered.contains("boss propose review-verdict"));
        assert!(rendered.contains("MUST NOT change the PR"));
        assert!(rendered.contains("jj log -R /tmp/ws"));
    }
}
