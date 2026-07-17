//! Prompt and rubric rendering for the reviewer worker: the CLAUDE.md, the
//! initial task prompt, the scope-specific rubric, and the revision
//! instructions rendered from a [`ReviewResult`].

use super::types::*;

/// Render qualifying `ReviewResult` findings as human-readable revision
/// instructions (design §4 of P992, task 8).
///
/// Groups all findings by severity (critical first, low last) and formats
/// each with its title, file, location, and concrete detail. The rendering
/// is the `revision_instructions` passed to the revising worker — it must
/// be specific enough to act on without guessing.
pub fn render_revision_instructions(result: &ReviewResult) -> String {
    let severity_rank = |s: &ReviewFindingSeverity| match s {
        ReviewFindingSeverity::Critical => 0u8,
        ReviewFindingSeverity::High => 1,
        ReviewFindingSeverity::Medium => 2,
        ReviewFindingSeverity::Low => 3,
    };

    let mut findings = result.findings.clone();
    findings.sort_by_key(|f| severity_rank(&f.severity));

    let mut out = format!(
        "Automated PR review found {} finding(s) requiring attention.\n\
         Address ALL findings before finalising this revision.\n\
         \n\
         ## HARD RULE: no punting — do the actual work\n\
         \n\
         Each finding below requires a real code change that resolves it.\n\
         The following are FORBIDDEN — they do NOT count as addressing a finding:\n\
         \n\
         - Filing a follow-up task, chore, or issue in lieu of fixing the finding.\n\
         - Leaving a `TODO`, `FIXME`, `Phase N`, \"wire this later\", or similar\n\
           deferral comment and calling the finding done.\n\
         - Acknowledging the finding in a comment or PR description without making\n\
           the substantive code change it requires.\n\
         - Marking the revision complete while a finding's core work is unaddressed.\n\
         \n\
         For example: a \"write-only field\" finding requires wiring the READ/consumer\n\
         side so the field is actually used — not a TODO comment promising to do so\n\
         later, and not a follow-up task filed for someone else.\n\
         \n\
         **If — and only if — a finding genuinely cannot or should not be fixed within\n\
         this revision's scope**, you must EXPLICITLY SURFACE that in your response:\n\
         state which finding you are not fixing and exactly why (technical blocker,\n\
         out-of-scope dependency, requires operator decision, etc.). Do NOT silently\n\
         file a follow-up or leave a TODO. The default is to do the work.\n\
         \n",
        findings.len()
    );

    for f in &findings {
        let loc = f.location.as_deref().map(|l| format!(" ({l})")).unwrap_or_default();
        let category = match f.category {
            ReviewFindingCategory::Correctness => "correctness",
            ReviewFindingCategory::Regression => "regression",
            ReviewFindingCategory::Architecture => "architecture",
            ReviewFindingCategory::Readability => "readability",
            ReviewFindingCategory::Tests => "tests",
            ReviewFindingCategory::EdgeCase => "edgecase",
            ReviewFindingCategory::Duplication => "duplication",
            ReviewFindingCategory::DeferredScope => "deferred_scope",
            ReviewFindingCategory::AgentIsms => "agent_isms",
        };
        out.push_str(&format!(
            "### [{severity}] {title}\n\
             **File:** `{file}`{loc}  \n\
             **Category:** {category}  \n\
             **Confidence:** {confidence}\n\n\
             {detail}\n\n",
            severity = f.severity.as_str(),
            title = f.title,
            file = f.file,
            category = category,
            confidence = match f.confidence {
                ReviewFindingConfidence::High => "high",
                ReviewFindingConfidence::Medium => "medium",
                ReviewFindingConfidence::Low => "low",
            },
            detail = f.detail,
        ));
    }

    out.push_str(&format!("**Review summary:** {}\n", result.summary));
    out
}

/// Render the CLAUDE.md for a reviewer worker (design §9 of P992).
///
/// Reviewer workers operate **read-only**: they read PR diffs and workspace
/// files but must not write, push, or post to GitHub. This CLAUDE.md
/// prominently states that mandate and omits PR-creation / VCS-push
/// guidance entirely (those actions are also blocked by the reviewer denylist,
/// so this is the belt that accompanies that suspenders layer).
///
/// The workspace is already checked out to the PR head SHA, so the reviewer
/// can read files directly without `gh pr diff` for every lookup.
pub fn render_reviewer_claude_md(lease_id: &str, workspace_path: &str) -> String {
    format!(
        "# Boss reviewer rules\n\
         \n\
         You are running inside a Boss-managed **reviewer** session. The engine\n\
         spawned you in a leased cube workspace checked out to the PR head.\n\
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
         - Running `cube pr create`/`cube pr update` or any Boss PR helper.\n\
         \n\
         **The one permitted write** is your `ReviewResult` JSON, which you\n\
         write with the `Write` tool to the engine-owned artifact path given in\n\
         your task prompt (also exported as `$BOSS_STRUCTURED_OUTPUT`). That\n\
         path is OUTSIDE every worker workspace, so it is not part of the PR or\n\
         repo — writing it does not violate the read-only mandate. Do not write\n\
         anywhere else.\n\
         \n\
         Anything you would \"fix\", describe as a finding in the\n\
         `ReviewResult` JSON instead. Your feedback stays inside Boss —\n\
         **it is never posted to GitHub**.\n\
         \n\
         Allowed read-only tools: `grep`, `find`, `cat`, `head`, `tail`,\n\
         `Read`, `jj log`, `jj show`, `jj diff`, `gh pr view`, `gh pr diff`,\n\
         `gh pr list`, and similar read-only operations.\n\
         \n\
         ## `gh` requires `--repo` in this workspace\n\
         \n\
         `gh` cannot auto-detect the repo in a jj workspace (there is no `.git`\n\
         directory at the root — only `.jj/`). Your initial task prompt states\n\
         the concrete repo slug. Pass `--repo <owner/repo>` on every `gh`\n\
         command: `gh pr view`, `gh pr diff`, `gh pr checks`, `gh api`, etc.\n\
         \n\
         ## Your workspace\n\
         \n\
         - Workspace path: `{workspace_path}`\n\
         - Cube lease id: `{lease}`\n\
         \n\
         The workspace is already checked out to the PR head. You can read\n\
         changed files and surrounding context directly — use `Read`, `cat`,\n\
         `grep`, etc. on files in `{workspace_path}`. No need to use\n\
         `git show <sha>:<path>` or fetch files via `gh`.\n\
         \n\
         Lease held for the lifetime of this run. Do not lease, release,\n\
         or mutate cube state.\n\
         \n\
         ## VCS (read-only)\n\
         \n\
         Use `jj` for read-only navigation. Do not push or modify history.\n\
         \n\
         - `jj log`, `jj show`, `jj diff` — browse history and diffs.\n\
         - `gh pr diff <url>` — fetch the PR diff (useful for the annotated diff view).\n\
         - `gh pr view <url>` — read the PR description.\n\
         \n\
         {boundaries_and_coordinator}",
        lease = lease_id,
        workspace_path = workspace_path,
        boundaries_and_coordinator = crate::prompt_fragments::boundaries_and_coordinator_fragment(),
    )
}

/// Compose the initial-prompt for a `pr_review` execution (design §2, §12).
///
/// `task_name` and `task_description` are the producing task's title and
/// description — they tell the reviewer what the PR was *supposed* to do,
/// which is the baseline for the regression/deletion check.
///
/// `pr_url` is the PR to review.
///
/// `scope` controls which rubric is rendered:
/// - [`ReviewScope::Code`] — the full code rubric (correctness, regressions,
///   architecture, tests, edge cases), plus a fallback note to switch to the
///   docs rubric if all changed files turn out to be documentation files.
/// - [`ReviewScope::DocsOnly`] — only the light docs rubric (structure,
///   completeness, required-sections). The code rubric is omitted entirely.
///
/// `ctx` is the pre-fetched PR metadata. When `Some`, the prompt embeds the
/// base/head SHAs and changed-file list as orientation context. When `None`
/// (fetch failed), the prompt falls back to URL-only framing. In both cases
/// the reviewer workspace is at the PR head and can read files directly.
///
/// When the engine cannot pre-classify the PR (no file list available),
/// pass [`ReviewScope::Code`]; the self-detection fallback in the code rubric
/// section covers that case.
///
/// `output_path` is the absolute, engine-owned artifact path the reviewer must
/// write its `ReviewResult` JSON to (see [`crate::structured_output`]). It is
/// the primary output channel; the prompt also asks for a fenced-JSON copy in
/// the final message as a transitional fallback.
pub fn render_reviewer_initial_prompt(
    task_name: &str,
    task_description: &str,
    pr_url: &str,
    output_path: &str,
    scope: ReviewScope,
    ctx: Option<&PrReviewContext>,
    repo_slug: &str,
) -> String {
    let rubric = render_rubric_section(&scope);

    // Extended PR metadata block — only present when we have pre-fetched context.
    let pr_metadata_block = match ctx {
        Some(ctx) => {
            let files = if ctx.changed_files.is_empty() {
                "*(unavailable)*".to_owned()
            } else {
                ctx.changed_files
                    .iter()
                    .map(|f| format!("- `{f}`"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            format!(
                "**Base commit:** `{base}`  \n\
                 **Head commit:** `{head}`  \n\
                 **Changed files ({n}):**\n\
                 {files}\n",
                base = ctx.base_sha,
                head = ctx.head_sha,
                n = ctx.changed_files.len(),
                files = files,
            )
        }
        None => String::new(),
    };

    // Short reference for `gh` commands — PR number is cleaner than the full
    // URL and avoids `gh` having to parse it, but fall back to URL when the
    // number is unavailable.
    let pr_ref = ctx
        .map(|c| c.pr_number.to_string())
        .unwrap_or_else(|| pr_url.to_owned());

    // Head SHA for the ReviewResult schema example — use the known SHA when
    // available so the reviewer can copy it directly rather than fetching it.
    let schema_head_sha = ctx
        .map(|c| c.head_sha.as_str())
        .unwrap_or("<sha from gh pr view --json headRefOid>");

    // When the diff is small enough, embed it directly so the reviewer
    // doesn't need a `gh pr diff` tool call.
    let embedded_diff = ctx.and_then(|c| c.diff_content.as_deref());
    let diff_step = if embedded_diff.is_some() {
        "2. The PR diff is pre-embedded in the **Embedded diff** section \
         below — no need to fetch it separately.\n"
            .to_owned()
    } else {
        format!("2. Get the diff for the annotated view: `gh pr diff {pr_ref} --repo {repo_slug}`\n")
    };
    let embedded_diff_section = match embedded_diff {
        Some(diff) => format!(
            "## Embedded diff\n\
             \n\
             Pre-fetched at review time. Read it carefully before forming \
             findings.\n\
             \n\
             ```diff\n\
             {diff}\n\
             ```\n\
             \n"
        ),
        None => String::new(),
    };

    // 2026-07-01 revision-review experiment: when this pass was triggered by
    // a revision's push (not the PR's first review), tell the reviewer what
    // was already reviewed so it can prioritise the delta — while explicitly
    // overriding diff-only scoping for whole-PR-state defects a delta view
    // would miss (the motivating incident: a crate extraction left two
    // complete copies of the same module in the PR).
    let revision_context_block = match ctx.and_then(|c| c.last_reviewed_sha.as_deref()) {
        Some(last_sha) if !last_sha.is_empty() => format!(
            "## Reviewing a revision\n\
             \n\
             This PR was already reviewed up to commit `{last_sha}`. Since then, a \
             revision (CI-fix, conflict-resolution, operator-filed, or a prior \
             automated-reviewer revision) pushed new commits. **Prioritise the \
             delta since `{last_sha}`** — content already reviewed and accepted \
             does not need re-litigating unless the new commits touch it.\n\
             \n\
             **Do not scope your review to a diff-only view, however.** Some \
             defects are only visible from the PR's whole current state, not \
             from the new commits' diff alone. Concretely: a revision might \
             extract a module into its own crate but leave the original copy in \
             place, so the PR now ships two complete copies of the same code \
             (e.g. one under `blob/`, one in the new crate) — a delta-only \
             reviewer sees only \"new crate added\" and \"nothing removed\" as \
             two unremarkable hunks and misses the duplication entirely. Before \
             concluding the PR is clean, cross-check the full changed-file list \
             and the current workspace contents (not just the new commits) for \
             this class of whole-PR-state defect.\n\
             \n"
        ),
        _ => String::new(),
    };

    // incident-002 P2: rationale-independent both-parents deletion tripwire.
    // When the engine deterministically found merged-parent surfaces this
    // resolution removed, the reviewer receives them as authoritative,
    // must-address context — independent of any "supersedes" narrative.
    let merged_parent_deletion_block = ctx
        .map(|c| render_merged_parent_deletion_block(&c.merged_parent_deletions))
        .unwrap_or_default();

    // incident-002 P3: deterministic supersession-language scan of the PR
    // narrative. When present, the reviewer must verify a design-doc citation
    // for each flagged claim.
    let supersession_flag_block = ctx
        .map(|c| crate::supersession_scan::render_supersession_flag_block(&c.supersession_flags))
        .unwrap_or_default();

    format!(
        "# PR review\n\
         \n\
         You are an independent PR reviewer. Your ONLY job is to review the \
         PR and produce a single structured `ReviewResult` JSON. You MUST NOT \
         change the PR in any way: no commits, no pushes, no `gh` writes, no \
         edits to repo files or branches, no comments on GitHub. You operate \
         **read-only** on the PR. The ONE write you make is your `ReviewResult` \
         JSON, to the engine-owned artifact path below (outside the repo). \
         Anything you would \"fix\" you instead describe as a \
         finding. Posting to GitHub is prohibited — your feedback stays inside \
         Boss as an internal revision.\n\
         \n\
         ## `gh` requires `--repo` in this workspace\n\
         \n\
         This repo is `{repo_slug}`. `gh` cannot auto-detect the repo in a jj \
         workspace (there is no `.git` directory at the root — only `.jj/`). \
         Pass `--repo {repo_slug}` on every `gh` command: `gh pr view`, \
         `gh pr diff`, `gh pr checks`, `gh api`, etc.\n\
         \n\
         ## PR under review\n\
         \n\
         **Task:** {task_name}\n\
         \n\
         **Task description:**\n\
         {task_description}\n\
         \n\
         **PR:** {pr_url}\n\
         {pr_metadata_block}\n\
         {revision_context_block}\
         {merged_parent_deletion_block}\
         {supersession_flag_block}\
         ## Review steps\n\
         \n\
         1. Your workspace is already checked out to the PR head — read \
            changed files directly with `Read`, `cat`, `grep`, etc.\n\
         {diff_step}\
         3. Get the PR description: `gh pr view {pr_ref} --repo {repo_slug}`\n\
         4. Read changed files and surrounding context using `Read`, `cat`, \
            `grep`, `jj show`, etc. — no writes to repo files.\n\
         5. Produce the `ReviewResult` JSON (schema below) and deliver it as \
            described in **Required output** — write it to the artifact file \
            with the `Write` tool, and also include it as a fenced \
            ` ```json ` block at the end of your final message.\n\
         \n\
         {embedded_diff_section}\
         {rubric}\n\
         ## Speed/comprehensiveness balance\n\
         \n\
         Prefer **fast, high-signal feedback** over exhaustive analysis. \
         Every PR may now pass through up to ~3 produce→review→revise cycles, \
         so do NOT excessively lengthen turnaround. Spend your scrutiny budget \
         on correctness and regressions first. If in doubt about a \
         non-critical suggestion, you MAY offer it WITHOUT deep analysis \
         and mark it `low` severity and `low` confidence — the revising \
         worker decides whether to apply it.\n\
         \n\
         ## Required output — CRITICAL\n\
         \n\
         **Primary (required):** write your single `ReviewResult` JSON object \
         to this exact file using the `Write` tool — nothing else, just the \
         JSON:\n\
         \n\
         `{output_path}`\n\
         \n\
         This path is also exported as `$BOSS_STRUCTURED_OUTPUT`. It lives \
         outside every repo/workspace, so writing it is the one write you are \
         permitted (it does not touch the PR). The engine reads and \
         schema-validates this file; if it is missing or invalid the engine \
         will ask you to write it again.\n\
         \n\
         **Also (fallback):** end your final message with the same \
         `ReviewResult` JSON in a fenced ` ```json ` block as the LAST content \
         in your message — nothing follows the closing ` ``` `. This is a \
         backstop the engine uses only if the file is unreadable. No other \
         terminal action is permitted.\n\
         \n\
         Schema:\n\
         \n\
         ```jsonc\n\
         {{\n\
           \"pr_url\": \"{pr_url}\",\n\
           \"head_sha\": \"{schema_head_sha}\",\n\
           \"summary\": \"<one-paragraph overall assessment>\",\n\
           \"revision_warranted\": true,\n\
           \"findings\": [\n\
             {{\n\
               \"severity\": \"critical | high | medium | low\",\n\
               \"category\": \"correctness | regression | architecture | readability | tests | edgecase | duplication | deferred_scope | agent_isms\",\n\
               \"file\": \"path/to/file.rs\",\n\
               \"location\": \"fn foo, ~L42\",\n\
               \"title\": \"<short scannable title>\",\n\
               \"detail\": \"<concrete description + what to change>\",\n\
               \"confidence\": \"high | medium | low\"\n\
             }}\n\
           ],\n\
           \"regression_check\": {{\n\
             \"performed\": true,\n\
             \"suspected_deletions\": []\n\
           }}\n\
         }}\n\
         ```\n\
         \n\
         Rules:\n\
         \n\
         - `revision_warranted`: set to `true` when there is at least one \
           finding at `critical`/`high` severity, or any finding with \
           `category: \"regression\"`, `category: \"duplication\"`, \
           `category: \"deferred_scope\"`, or `category: \"agent_isms\"`, \
           regardless of severity. Set to \
           `false` for findings that are purely `medium`/`low` \
           correctness/style (the engine applies its own gate on top).\n\
         - `regression_check.performed` MUST be `true` — you cannot skip the \
           deletion check. Always set `suspected_deletions: []`; regression \
           findings go in `findings` with `category: \"regression\"` and the \
           engine derives this list automatically — do NOT populate it.\n\
         - `findings` may be empty if the PR is clean. `revision_warranted` \
           must then be `false`.\n\
         - `location` is optional (omit the key if the finding applies to the \
           whole file).\n\
         - Do NOT post this JSON to GitHub or as a PR comment. It stays \
           inside Boss.\n",
        task_name = task_name,
        task_description = task_description,
        pr_url = pr_url,
        output_path = output_path,
        pr_metadata_block = pr_metadata_block,
        revision_context_block = revision_context_block,
        merged_parent_deletion_block = merged_parent_deletion_block,
        supersession_flag_block = supersession_flag_block,
        pr_ref = pr_ref,
        diff_step = diff_step,
        embedded_diff_section = embedded_diff_section,
        schema_head_sha = schema_head_sha,
        rubric = rubric,
        repo_slug = repo_slug,
    )
}

/// Render the authoritative merged-parent deletion-tripwire block (incident-002
/// P2). `deletions` are engine-computed, rename/move-aware descriptions of
/// surfaces a merged parent added and this resolution removed. Returns an empty
/// string when there are none, so the caller can unconditionally interpolate it.
///
/// This is deliberately **rationale-independent**: it is anchored on the fact
/// that a merged parent lost functionality, not on the worker's stated purpose.
/// The engine additionally halts auto-progression on a non-empty set (see
/// `finalize_pr_review_pass`); this block ensures the reviewer also treats each
/// entry as a gating regression rather than accepting a "supersedes" narrative.
fn render_merged_parent_deletion_block(deletions: &[String]) -> String {
    if deletions.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("## Merged-parent deletion tripwire (engine-computed) — CRITICAL\n\n");
    out.push_str(
        "This PR resolves a merge / forward-port. The engine diffed the \
         resolution against **both merge parents** (not just `main`) and found \
         that it **removes surfaces a MERGED parent added** — functionality that \
         already landed and that this resolution drops. This is the exact \
         incident-002 failure class: a forward-port deleting a just-merged \
         feature and calling it \"superseded\".\n\n\
         This finding is **rationale-independent** — it is anchored on the fact \
         that a merged parent lost functionality, NOT on the worker's stated \
         purpose. A \"supersedes\" / \"obsoletes\" narrative does NOT clear it.\n\n\
         For EACH removed surface below, raise a `regression` finding UNLESS the \
         PR cites a specific design doc + section that authorises the removal AND \
         that section actually supports it. Absent such a citation the surface \
         must be restored (integrate both parents) or the resolution escalated to \
         an operator — it must not ship silently.\n\n\
         Removed merged-parent surfaces (engine-computed, rename/move-aware):\n\n",
    );
    for d in deletions {
        out.push_str(&format!("- {d}\n"));
    }
    out.push('\n');
    out
}

fn render_rubric_section(scope: &ReviewScope) -> String {
    match scope {
        ReviewScope::Code => "## Review rubric — code PR\n\
             \n\
             Apply a **high bar**: push back only on real problems, not \
             stylistic preferences. Every finding must name a file (and \
             function/line/hunk where possible) and state **concretely what to \
             change**. Vague findings are not acceptable.\n\
             \n\
             Review for:\n\
             \n\
             - **Critical correctness** — logic errors, broken invariants, \
               mishandled errors, race conditions. (`category: \"correctness\"`)\n\
             - **Inadvertent regressions** *(first-class, explicit check)* — \
               flag anything dropped that is unrelated to the PR's stated \
               purpose. For a conflict-resolution / forward-port PR, do NOT diff \
               against `main` alone — diff against **both merge parents** (the \
               PR's prior head AND the moved base). Any file or exported surface \
               that a MERGED parent added and this resolution removes is a \
               regression **regardless of any \"supersedes\" narrative** — anchor \
               on the fact of the deletion, not the worker's stated purpose. \
               This is the T793 / incident-002 check: a live feature silently \
               removed during a forward-port must be caught. (`category: \
               \"regression\"`)\n\
             - **Supersession / obsolescence claims** *(first-class, explicit \
               check)* — if the PR body, a commit message, or a comment claims \
               one surface \"supersedes\", \"obsoletes\", \"replaces\", is \
               \"now-dead\", or is \"orphaned\" (about code a parent added), that \
               is a DESIGN decision the worker is not authorised to make \
               unilaterally. Require a specific **design-doc citation** (path + \
               section) and VERIFY it: read the cited section and confirm it \
               actually says the surface is replaced. If there is no citation, or \
               the cited section contradicts the claim (e.g. specifies the two \
               surfaces as complementary siblings), raise a `regression` finding \
               — the removal the claim justifies is presumptively wrong. A \
               component is only \"orphaned\" if something OTHER than this same \
               PR orphaned it. (`category: \"regression\"`)\n\
             - **Architectural issues** — wrong layer, missed reuse, abstraction \
               that fights the codebase's conventions. (`category: \
               \"architecture\"`)\n\
             - **Reuse/duplication** *(first-class, explicit check)* — for \
               every substantial new construct the PR introduces (an \
               HTTP/API client, external-service wiring such as endpoints, \
               auth headers, or version constants, a serialization helper, \
               retry/backoff logic, or a utility module), search the WHOLE \
               repo — not just the changed files — for an existing \
               equivalent before accepting the new code as necessary: grep \
               for distinctive strings (the exact endpoint URL, header name, \
               version constant, or a crate-typical helper/module name). If \
               an equivalent already exists, raise a finding naming the \
               existing file/module and recommending reuse or extraction of \
               a shared module — do not let a plausible-looking \
               reimplementation pass just because it works in isolation. \
               Example: a new `api.anthropic.com` HTTP client in a repo that \
               already has five hand-rolled Anthropic clients is a \
               duplication finding regardless of how well the new one is \
               written. A justified exception (duplication that really is \
               necessary) must be explicitly explained in the PR \
               description; its absence there is itself grounds for the \
               finding. (`category: \"duplication\"`) Any confirmed \
               duplication finding forces a revision regardless of the \
               severity you assign it — the same treatment as a regression \
               finding, not an advisory suggestion.\n\
             - **Deferred-scope hygiene** *(first-class, explicit check)* — \
               compare the PR's delivered changes against the owning work \
               item's brief (given above in **Task description**) and check \
               for three distinct failure modes:\n\
               (1) **Undeclared deferral** — the brief asked for scope that \
               the diff does not deliver, and no `[deferred-scope] \
               summary=\"...\" reason=\"...\"` marker (recorded by the \
               engine from the worker's final response) covers the gap. The \
               worker narrowed scope without declaring it anywhere the \
               engine can see.\n\
               (2) **Misdeclared deferral** — the PR body, summary, or a \
               commit message contains prose deferral language (a \
               \"## Deferred\" section, \"left for a followup\", \"out of \
               scope for this PR\", \"will address later\", etc.) that has \
               no matching `[deferred-scope]` marker backing it. Prose alone \
               creates no engine-tracked record, so the deferred work is \
               silently lost. Live example: PR spinyfin/mono#1968 declared \
               two deferred items only in a prose section — nothing was \
               recorded, and the coordinator had to hand-file T2576 after \
               the fact to recover the lost scope.\n\
               (3) **Malformed markers** — a `[deferred-scope]` marker is \
               present but missing `summary=`/`reason=` or improperly \
               quoted. Flag it so the worker fixes the marker's grammar \
               while the PR is still open; the engine records malformed \
               markers with a parse warning, but a clean marker is the \
               contract.\n\
               **Exception — manual/interactive verification a headless \
               worker cannot perform**: none of the three failure modes \
               above apply when the deferred item is manual, interactive, \
               or display-requiring verification — live GUI runs, \
               \"spawn real workers and watch the app\", screenshot-based \
               checks, physical-device tests, or anything else that needs an \
               interactive session with a display. A worker running headless \
               has no way to do this, so do NOT raise a `deferred_scope` \
               finding demanding a `[deferred-scope]` marker for it, and do \
               NOT demand the worker actually perform the manual \
               verification. A prose \"## Deferred\" / \"Validation\" note \
               describing exactly this kind of deferral is acceptable and \
               expected — that is the normal, correct way to surface it. \
               This carve-out is narrow: it keys on infeasibility for a \
               headless agent, not on the word \"testing\" in general. \
               Deferring work the agent COULD do headlessly — code changes, \
               follow-up fixes, or unit/integration tests runnable without a \
               display — still requires the `[deferred-scope]` marker exactly \
               as in (1)/(2) above; this exception does not weaken the \
               spinyfin/mono#1968 lesson that prose deferrals of trackable \
               work get silently lost; it only narrows the marker \
               requirement for work no marker could make trackable anyway \
               (an agent still can't run it after filing a followup). If \
               it's cheap to do so, you may still mention in the overall \
               `summary` field that live verification remains outstanding, so \
               the operator sees it — but do not add it to `findings` and do \
               not let it affect `revision_warranted`.\n\
               (`category: \"deferred_scope\"`) Any confirmed finding in \
               this dimension forces a revision regardless of the severity \
               you assign it — the same treatment as a regression or \
               duplication finding: an undeclared or unrecorded deferral is \
               a process gap, not a style nit.\n\
             - **Agent-isms in code comments and PR descriptions** *(first-class, \
               explicit check)* — read every new or changed comment in the diff, \
               plus the PR title and description (fetch with `gh pr view` — \
               review step 3), and flag any that only makes sense to the agent \
               that wrote it, not to a human reading it cold. **The Task / Task \
               description block elsewhere in this prompt is engine-authored \
               context about this review run, not the PR's own title or \
               description — never flag it under this check.**\n\
               (1) **Historical narration** — *code comments only.* A comment \
               that describes how the code came to be instead of what it \
               currently does (e.g. \"we used to blah blah, but that was \
               removed because foo foo\"). Comments must represent the *state* \
               of the code, not its lineage, unless that history is strongly \
               meaningful to future maintainers (a genuine gotcha, a workaround \
               for a specific external bug, a non-obvious invariant). When you \
               flag one, quote the offending comment and propose replacement \
               wording that states the current behaviour/reason instead. \
               **This does NOT apply to the PR description** — descriptions \
               exist to narrate what changed and why (\"previously X, this PR \
               makes it Y\"), so historical/narrative context there is expected \
               and must never be flagged. Do not overgeneralize the code-comment \
               rule to descriptions.\n\
               (2) **Boss-construct references** — *code comments and PR \
               title/description alike.* Neither may name a Boss work item id, \
               phase, chore, brief, or effort level. Example violation in a \
               comment: \"This implements T234 phase 7.\" Example violation in a \
               PR title/description: \"Implements T234 phase 7 per the brief.\" \
               PR descriptions are read on GitHub, where those identifiers mean \
               nothing to a human reader. Quote the offending text and propose \
               wording that describes what the code does instead of where the \
               instruction to write it came from.\n\
               (3) **\"The operator\" / actor references** — *code comments and \
               PR title/description alike.* Neither may refer to the human \
               directing Boss as \"the operator\", nor to actors in general \
               (\"the operator requested\", \"per the operator's review\"). \
               Prefer \"We want to avoid showing a card here because...\" over \
               \"The operator requested that no card is shown here.\" Quote the \
               offending text and propose wording that states the reason \
               directly, without naming who asked for it.\n\
               (`category: \"agent_isms\"`) Any confirmed finding in this \
               dimension — whether in a code comment or in the PR \
               title/description — forces a revision regardless of the severity you \
               assign it — the same treatment as regression, duplication, \
               and deferred-scope findings: agent-authored scaffolding left \
               behind, in code or in the PR's own description, is a process \
               gap, not a style nit. For a finding about the PR title/description \
               itself (not a code comment), set `file` to the literal `PR \
               description` and omit `location`.\n\
             - **Code quality/readability** — fails to match surrounding style, \
               naming issues, dead/confusing code. (`category: \"readability\"`)\n\
             - **Lint/warning suppressions** — scrutinize every new \
               `#[allow(...)]`, `#[expect(...)]`, lint-disable comment, type/lint \
               ignore, or unused-variable underscoring (`_foo`) of something \
               that should instead be removed or fixed. Ask whether a more \
               correct remedy exists instead of suppressing the check: e.g. \
               `#[allow(dead_code)]` on a helper only used by tests usually \
               means the helper should be compiled under `#[cfg(test)]` (or \
               live in a test-only module) rather than silenced; an unused \
               import usually means dead code to delete, not hide. A \
               suppression is acceptable only when it is genuinely the right \
               tool (e.g. a false positive, or a documented, narrowly-scoped \
               exception) — this is a scrutiny instruction, not a blanket ban. \
               When a more correct remedy exists, raise a finding naming it. \
               (`category: \"readability\"`)\n\
             - **Test coverage gaps** — untested new behaviour, missing \
               edge-case tests. (`category: \"tests\"`)\n\
             - **Edge cases/gotchas** — boundary conditions, nullability, \
               concurrency, failure modes. (`category: \"edgecase\"`)\n\
             \n\
             **Docs-only fallback:** if you determine that every changed file \
             is a documentation file (`.md`, `.mdx`, `.rst`, design docs, \
             READMEs) with no source-code changes, switch to the light docs \
             rubric below and skip the code rubric above.\n\
             \n\
             - Structure and completeness of the document.\n\
             - Internal consistency (no contradictions within the doc).\n\
             - Required-sections check for design docs (problem/goals/approach \
               headings present).\n\
             \n"
        .to_owned(),
        ReviewScope::DocsOnly => "## Review rubric — docs-only PR\n\
             \n\
             This PR contains only documentation files. Apply the **light \
             rubric** — skip code-review concerns (correctness, regressions, \
             architecture, tests) entirely.\n\
             \n\
             Review for:\n\
             \n\
             - **Structure and completeness** — is the document well-organised \
               and does it cover what it claims to cover?\n\
             - **Internal consistency** — no contradictions within the doc; \
               claims made in one section are consistent with claims in another.\n\
             - **Required-sections check** — for design docs, verify that the \
               expected headings are present: Problem / Goal, Goals, Chosen \
               approach (or equivalent). Flag any that are absent or \
               substantively empty.\n\
             \n\
             Do NOT apply the code rubric (correctness, regressions, \
             architecture, tests, edge cases) to this docs-only PR.\n\
             \n"
        .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use crate::pr_review::*;

    #[test]
    fn reviewer_initial_prompt_contains_rubric_and_pr_url() {
        let prompt = render_reviewer_initial_prompt(
            "Fix the auth bug",
            "Auth middleware drops sessions on timeout.",
            "https://github.com/org/repo/pull/99",
            "/tmp/bwo/exec.json",
            ReviewScope::Code,
            None,
            "org/repo",
        );
        assert!(prompt.contains("https://github.com/org/repo/pull/99"));
        assert!(prompt.contains("Fix the auth bug"));
        assert!(prompt.contains("Auth middleware drops sessions on timeout."));
        assert!(prompt.contains("regression"));
        assert!(prompt.contains("correctness"));
        assert!(prompt.contains("revision_warranted"));
        assert!(prompt.contains("regression_check"));
        assert!(prompt.contains("read-only"));
        assert!(prompt.contains("gh pr diff"));
        assert!(prompt.contains("--repo org/repo"), "prompt must include --repo flag");
        assert!(prompt.contains("org/repo"), "prompt must state the repo slug");
        // incident-002 P2: the regression rubric must instruct diffing against
        // BOTH merge parents for forward-ports and anchoring on the fact of the
        // deletion rather than the worker's narrative.
        assert!(
            prompt.contains("both merge parents"),
            "regression rubric must require diffing against both merge parents",
        );
        // incident-002 P3: the reviewer must verify a design-doc citation for
        // supersession claims.
        assert!(
            prompt.contains("Supersession / obsolescence claims"),
            "rubric must carry the supersession-citation check",
        );
        assert!(
            prompt.contains("design-doc citation"),
            "supersession check must demand a design-doc citation",
        );
    }

    #[test]
    fn reviewer_prompt_embeds_engine_flagged_blocks_from_context() {
        let ctx = PrReviewContext {
            pr_number: 753,
            base_sha: "base".to_owned(),
            head_sha: "head".to_owned(),
            changed_files: vec!["components/PlanEventCard.tsx".to_owned()],
            diff_content: None,
            last_reviewed_sha: None,
            supersession_flags: vec!["**supersedes** — \"...supersedes t16's static badge...\"".to_owned()],
            merged_parent_deletions: vec![
                "`components/RecommendationBadge.tsx` — added by a merged parent, removed by this resolution"
                    .to_owned(),
            ],
        };
        let prompt = render_reviewer_initial_prompt(
            "Forward-port drill-down modal",
            "Resolve merge conflicts against main.",
            "https://github.com/org/repo/pull/753",
            "/tmp/bwo/exec.json",
            ReviewScope::Code,
            Some(&ctx),
            "org/repo",
        );
        // P2 authoritative deletion-tripwire block.
        assert!(prompt.contains("Merged-parent deletion tripwire (engine-computed)"));
        assert!(prompt.contains("RecommendationBadge.tsx"));
        assert!(prompt.contains("rationale-independent"));
        // P3 authoritative supersession block.
        assert!(prompt.contains("Supersession-claim citation check (engine-flagged)"));
        assert!(prompt.contains("supersedes"));
    }

    #[test]
    fn reviewer_prompt_omits_engine_blocks_when_context_clean() {
        let ctx = PrReviewContext {
            pr_number: 1,
            base_sha: "b".to_owned(),
            head_sha: "h".to_owned(),
            changed_files: vec!["src/lib.rs".to_owned()],
            diff_content: None,
            last_reviewed_sha: None,
            supersession_flags: vec![],
            merged_parent_deletions: vec![],
        };
        let prompt = render_reviewer_initial_prompt(
            "Add a feature",
            "Implement it.",
            "https://github.com/org/repo/pull/1",
            "/tmp/bwo/exec.json",
            ReviewScope::Code,
            Some(&ctx),
            "org/repo",
        );
        assert!(!prompt.contains("Merged-parent deletion tripwire"));
        assert!(!prompt.contains("Supersession-claim citation check (engine-flagged)"));
    }

    #[test]
    fn reviewer_initial_prompt_states_workspace_at_pr_head() {
        let prompt = render_reviewer_initial_prompt(
            "Fix the auth bug",
            "Auth middleware drops sessions on timeout.",
            "https://github.com/org/repo/pull/99",
            "/tmp/bwo/exec.json",
            ReviewScope::Code,
            None,
            "org/repo",
        );
        assert!(
            prompt.contains("already checked out to the PR head"),
            "prompt must state workspace is at PR head"
        );
        assert!(
            !prompt.contains("DO THIS FIRST"),
            "prompt must not have defensive 'do this first' emphasis"
        );
        assert!(
            !prompt.contains("git show"),
            "prompt must not instruct anchoring reads via git show"
        );
    }

    #[test]
    fn reviewer_initial_prompt_with_context_embeds_metadata() {
        let ctx = PrReviewContext {
            pr_number: 99,
            base_sha: "base000".to_owned(),
            head_sha: "head999".to_owned(),
            changed_files: vec!["src/main.rs".to_owned(), "tests/test.rs".to_owned()],
            diff_content: None,
            last_reviewed_sha: None,
            supersession_flags: Vec::new(),
            merged_parent_deletions: Vec::new(),
        };
        let prompt = render_reviewer_initial_prompt(
            "Fix the auth bug",
            "Auth middleware drops sessions on timeout.",
            "https://github.com/org/repo/pull/99",
            "/tmp/bwo/exec.json",
            ReviewScope::Code,
            Some(&ctx),
            "org/repo",
        );
        assert!(prompt.contains("base000"), "prompt must include base SHA");
        assert!(prompt.contains("head999"), "prompt must include head SHA");
        assert!(prompt.contains("src/main.rs"), "prompt must list changed files");
        assert!(prompt.contains("tests/test.rs"), "prompt must list changed files");
        assert!(
            prompt.contains("gh pr diff 99 --repo org/repo"),
            "prompt must use PR number and --repo in diff command"
        );
        assert!(
            prompt.contains("already checked out to the PR head"),
            "prompt must state workspace is at PR head"
        );
        assert!(
            !prompt.contains("git show head999:"),
            "prompt must not instruct reads via git show"
        );
        assert!(
            !prompt.contains("NOT at the PR head"),
            "prompt must not warn that working tree is stale"
        );
    }

    #[test]
    fn reviewer_initial_prompt_with_embedded_diff_omits_gh_pr_diff_step() {
        let ctx = PrReviewContext {
            pr_number: 42,
            base_sha: "base111".to_owned(),
            head_sha: "head222".to_owned(),
            changed_files: vec!["src/lib.rs".to_owned()],
            diff_content: Some("diff --git a/src/lib.rs b/src/lib.rs\n+fn new() {}".to_owned()),
            last_reviewed_sha: None,
            supersession_flags: Vec::new(),
            merged_parent_deletions: Vec::new(),
        };
        let prompt = render_reviewer_initial_prompt(
            "Add a feature",
            "Implement the new feature.",
            "https://github.com/org/repo/pull/42",
            "/tmp/bwo/exec.json",
            ReviewScope::Code,
            Some(&ctx),
            "org/repo",
        );
        assert!(
            !prompt.contains("gh pr diff 42"),
            "prompt must NOT instruct reviewer to call gh pr diff when diff is embedded"
        );
        assert!(
            prompt.contains("Embedded diff"),
            "prompt must include the embedded diff section"
        );
        assert!(
            prompt.contains("diff --git a/src/lib.rs"),
            "prompt must contain the actual diff content"
        );
        assert!(
            prompt.contains("no need to fetch it separately"),
            "prompt must tell reviewer not to fetch the diff"
        );
    }

    #[test]
    fn reviewer_initial_prompt_without_diff_content_uses_gh_pr_diff_step() {
        let ctx = PrReviewContext {
            pr_number: 42,
            base_sha: "base111".to_owned(),
            head_sha: "head222".to_owned(),
            changed_files: vec!["src/lib.rs".to_owned()],
            diff_content: None,
            last_reviewed_sha: None,
            supersession_flags: Vec::new(),
            merged_parent_deletions: Vec::new(),
        };
        let prompt = render_reviewer_initial_prompt(
            "Add a feature",
            "Implement the new feature.",
            "https://github.com/org/repo/pull/42",
            "/tmp/bwo/exec.json",
            ReviewScope::Code,
            Some(&ctx),
            "org/repo",
        );
        assert!(
            prompt.contains("gh pr diff 42 --repo org/repo"),
            "prompt must instruct reviewer to call gh pr diff with --repo when no diff is embedded"
        );
        assert!(
            !prompt.contains("Embedded diff"),
            "prompt must NOT include the embedded diff section when diff_content is None"
        );
    }

    #[test]
    fn code_scope_prompt_contains_code_rubric_and_docs_fallback() {
        let prompt = render_reviewer_initial_prompt(
            "Fix the auth bug",
            "Auth middleware drops sessions on timeout.",
            "https://github.com/org/repo/pull/99",
            "/tmp/bwo/exec.json",
            ReviewScope::Code,
            None,
            "org/repo",
        );
        assert!(prompt.contains("code PR"));
        assert!(prompt.contains("Docs-only fallback"));
        assert!(prompt.contains("light docs rubric"));
        // Code rubric categories must be present.
        assert!(prompt.contains("correctness"));
        assert!(prompt.contains("regressions"));
        assert!(prompt.contains("architecture"));
        assert!(prompt.contains("tests"));
    }

    /// The reuse/duplication rubric dimension (P1690 incident: a sixth
    /// hand-rolled Anthropic Messages API client landed unflagged) must be
    /// present with the search-then-compare behaviour and the
    /// revision-forcing note — a vague one-liner would not satisfy the
    /// acceptance criterion.
    #[test]
    fn code_scope_prompt_contains_reuse_duplication_dimension() {
        let prompt = render_reviewer_initial_prompt(
            "Add a feature",
            "Implement the new feature.",
            "https://github.com/org/repo/pull/1690",
            "/tmp/bwo/exec.json",
            ReviewScope::Code,
            None,
            "org/repo",
        );
        assert!(
            prompt.contains("Reuse/duplication"),
            "code rubric must contain an explicit reuse/duplication dimension"
        );
        assert!(
            prompt.contains("search the WHOLE repo"),
            "rubric must instruct searching the whole repo, not just changed files"
        );
        assert!(
            prompt.contains("grep for distinctive strings"),
            "rubric must instruct grepping for distinctive strings (endpoint/header/const)"
        );
        assert!(
            prompt.contains("api.anthropic.com"),
            "rubric must include the concrete acceptance-example anchor"
        );
        assert!(
            prompt.contains("\"duplication\""),
            "rubric must name the duplication category"
        );
        assert!(
            prompt.contains("forces a revision regardless of the severity"),
            "rubric must state duplication findings are revision-required, not advisory"
        );
        let revision_warranted_offset = prompt
            .find("`revision_warranted`: set to `true`")
            .expect("prompt must state the revision_warranted rule");
        let rule_text = &prompt[revision_warranted_offset..revision_warranted_offset + 400];
        assert!(
            rule_text.contains("category: \"regression\"") && rule_text.contains("category: \"duplication\""),
            "revision_warranted rule must cover both regression and duplication categories: {rule_text}"
        );
    }

    /// The deferred-scope hygiene rubric dimension (operator directive,
    /// 2026-07-14: reviewers must push back on undeclared/misdeclared
    /// deferred scope) must be present with all three detection modes —
    /// undeclared deferral, prose-only misdeclared deferral, and malformed
    /// markers — plus the revision-forcing note and the concrete
    /// spinyfin/mono#1968 acceptance example.
    ///
    /// This test doubles as the "run a review against a fixture PR body
    /// containing a prose ## Deferred section with no markers" acceptance
    /// check from the brief: the fixture is the task description below (a
    /// stand-in for a PR body/summary carrying prose-only deferral
    /// language), and the assertion confirms the rendered reviewer prompt
    /// instructs the reviewer to raise exactly this as a finding.
    #[test]
    fn code_scope_prompt_contains_deferred_scope_dimension() {
        let fixture_task_description = "Implement the widget importer.\n\n\
            ## Deferred\n\
            - CSV import support (left for a followup)\n\
            - Retry on transient network errors (out of scope for this PR)\n";
        let prompt = render_reviewer_initial_prompt(
            "Add widget importer",
            fixture_task_description,
            "https://github.com/spinyfin/mono/pull/1968",
            "/tmp/bwo/exec.json",
            ReviewScope::Code,
            None,
            "spinyfin/mono",
        );
        assert!(
            prompt.contains("Deferred-scope hygiene"),
            "code rubric must contain an explicit deferred-scope hygiene dimension"
        );
        assert!(
            prompt.contains("Undeclared deferral"),
            "rubric must cover undeclared deferral (brief scope missing, no marker)"
        );
        assert!(
            prompt.contains("Misdeclared deferral"),
            "rubric must cover misdeclared deferral (prose deferral with no matching marker)"
        );
        assert!(
            prompt.contains("## Deferred"),
            "rubric must call out prose \"## Deferred\" sections as a misdeclaration signal"
        );
        assert!(
            prompt.contains("Malformed markers"),
            "rubric must cover malformed [deferred-scope] markers"
        );
        assert!(
            prompt.contains("spinyfin/mono#1968"),
            "rubric must cite the live spinyfin/mono#1968 undeclared-deferral incident"
        );
        assert!(
            prompt.contains("\"deferred_scope\""),
            "rubric must name the deferred_scope category"
        );
        assert!(
            prompt.contains("forces a revision regardless of the severity"),
            "rubric must state deferred-scope findings are revision-required, not advisory"
        );
        // The fixture's prose-only "## Deferred" section (no [deferred-scope]
        // marker) is embedded in the task description the reviewer receives,
        // so a reviewer following the rubric above would raise exactly the
        // misdeclared-deferral finding this dimension exists to catch.
        assert!(
            prompt.contains(fixture_task_description),
            "the fixture PR body/task description must be embedded in the prompt for the reviewer to inspect"
        );
    }

    /// The agent-isms rubric dimension (operator directive: code comments
    /// and PR descriptions/titles that only make sense to the agent that
    /// wrote them — historical narration (code comments only), Boss
    /// work-item/phase/brief/effort-level references, or "the operator"/
    /// actor phrasing — are revision-required findings, not advisory nits)
    /// must be present with all three detection modes plus the
    /// revision-forcing note and the operator's own examples, and must
    /// state that historical/narrative context is expected and exempt in
    /// PR descriptions even though it is flagged in code comments.
    #[test]
    fn code_scope_prompt_contains_agent_isms_dimension() {
        let prompt = render_reviewer_initial_prompt(
            "Add a feature",
            "Implement the new feature.",
            "https://github.com/org/repo/pull/2100",
            "/tmp/bwo/exec.json",
            ReviewScope::Code,
            None,
            "org/repo",
        );
        assert!(
            prompt.contains("Agent-isms in code comments and PR descriptions"),
            "code rubric must contain an explicit agent-isms dimension covering both comments and PR descriptions"
        );
        assert!(
            prompt.contains("Historical narration"),
            "rubric must cover historical-narration comments (lineage instead of current state)"
        );
        assert!(
            prompt.contains("we used to blah blah, but that was removed because foo"),
            "rubric must include the operator's historical-narration example"
        );
        assert!(
            prompt.contains("code comments only"),
            "rubric must scope historical-narration to code comments, not PR descriptions"
        );
        assert!(
            prompt.contains("This does NOT apply to the PR description")
                && prompt.contains("historical/narrative context there is expected"),
            "rubric must explicitly exempt historical/narrative context in PR descriptions"
        );
        assert!(
            prompt.contains("Boss-construct references"),
            "rubric must cover comments naming Boss work items/phases/briefs"
        );
        assert!(
            prompt.contains("This implements T234 phase 7"),
            "rubric must include the operator's Boss-construct example"
        );
        assert!(
            prompt.contains("Implements T234 phase 7 per the brief"),
            "rubric must include a Boss-construct example specific to a PR title/description"
        );
        assert!(
            prompt.contains("\"the operator\""),
            "rubric must cover comments referring to the directing human as \"the operator\""
        );
        assert!(
            prompt.contains("the operator requested\", \"per the operator's review"),
            "rubric must cover PR-description actor framing like \"the operator requested\"/\"per the operator's review\""
        );
        assert!(
            prompt.contains("We want to avoid showing a card here because")
                && prompt.contains("The operator requested that no card is shown here"),
            "rubric must include the operator's before/after example for actor references"
        );
        assert!(
            prompt.contains("code comments and PR title/description alike"),
            "rubric must state Boss-construct and actor-reference checks apply to both comments and PR descriptions"
        );
        assert!(
            prompt.contains("quote the offending comment and propose replacement"),
            "rubric must instruct the reviewer to quote the comment and propose replacement wording"
        );
        assert!(
            prompt.contains("\"agent_isms\""),
            "rubric must name the agent_isms category"
        );
        assert!(
            prompt.contains("forces a revision regardless of the severity"),
            "rubric must state agent-isms findings are revision-required, not advisory"
        );
        let revision_warranted_offset = prompt
            .find("`revision_warranted`: set to `true`")
            .expect("prompt must state the revision_warranted rule");
        let rule_text = &prompt[revision_warranted_offset..revision_warranted_offset + 400];
        assert!(
            rule_text.contains("category: \"agent_isms\""),
            "revision_warranted rule must cover the agent_isms category: {rule_text}"
        );
    }

    /// Operator directive 2026-07-15: manual/interactive verification a
    /// headless worker cannot perform (live GUI runs, "spawn real workers
    /// and watch the app", screenshot-based checks) must be carved out of
    /// the deferred-scope hygiene dimension — a reviewer must NOT demand a
    /// `[deferred-scope]` marker, and must NOT demand the verification be
    /// performed, for that category of deferral. This is the acceptance
    /// fixture from spinyfin/mono PR #1994: a prose "## Deferred" /
    /// "Validation" section deferring "live end-to-end GUI verification ...
    /// not runnable in this headless worker" must not, per the rubric, be
    /// flagged the way PR #1994's review flagged it.
    #[test]
    fn code_scope_prompt_exempts_infeasible_manual_verification_from_deferred_scope() {
        let fixture_task_description = "Fix the 9th-dispatch spillover bug in the Lower Decks tab.\n\n\
            ## Deferred\n\
            Live end-to-end GUI verification of the 9th-dispatch spillover fix — \
            not runnable in this headless worker; needs an interactive macOS \
            session with a display.\n";
        let prompt = render_reviewer_initial_prompt(
            "Fix 9th-dispatch spillover",
            fixture_task_description,
            "https://github.com/spinyfin/mono/pull/1994",
            "/tmp/bwo/exec.json",
            ReviewScope::Code,
            None,
            "spinyfin/mono",
        );
        assert!(
            prompt.contains("manual/interactive verification a"),
            "rubric must carve out an exception for manual/interactive verification a headless worker cannot perform"
        );
        assert!(
            prompt.contains("do NOT raise a `deferred_scope` finding"),
            "rubric must explicitly instruct the reviewer not to raise a deferred_scope finding for this category"
        );
        assert!(
            prompt.contains("live GUI runs"),
            "exception must name live GUI runs as an example of infeasible manual verification"
        );
        assert!(
            prompt.contains("spawn real workers and watch the app"),
            "exception must name the 'spawn real workers and watch the app' phrasing from the triggering finding"
        );
        assert!(
            prompt.contains("screenshot-based"),
            "exception must name screenshot-based checks as an example"
        );
        assert!(
            prompt.contains("acceptable and expected"),
            "rubric must state a prose Deferred/Validation note for this category is acceptable and expected"
        );
        assert!(
            prompt.contains("narrow") && prompt.contains("spinyfin/mono#1968"),
            "exception must stay narrow and reaffirm it does not weaken the spinyfin/mono#1968 lesson"
        );
        assert!(
            prompt.contains(fixture_task_description),
            "the fixture PR body/task description must be embedded in the prompt for the reviewer to inspect"
        );
    }

    /// When `last_reviewed_sha` is set (a revision pushed after a prior
    /// review pass), the prompt must tell the reviewer to prioritise the
    /// delta AND explicitly override diff-only scoping for whole-PR-state
    /// defects — the motivating T192/rec_engine duplication incident.
    #[test]
    fn revision_triggered_prompt_includes_delta_and_whole_pr_guidance() {
        let ctx = PrReviewContext {
            pr_number: 737,
            base_sha: "base000".to_owned(),
            head_sha: "head999".to_owned(),
            changed_files: vec!["crates/rec_engine/src/lib.rs".to_owned()],
            diff_content: None,
            last_reviewed_sha: Some("sha_reviewed_at_1843".to_owned()),
            supersession_flags: Vec::new(),
            merged_parent_deletions: Vec::new(),
        };
        let prompt = render_reviewer_initial_prompt(
            "Extract rec_engine into its own crate",
            "Follow-up revision after the first review pass.",
            "https://github.com/org/repo/pull/737",
            "/tmp/bwo/exec.json",
            ReviewScope::Code,
            Some(&ctx),
            "org/repo",
        );
        assert!(
            prompt.contains("Reviewing a revision"),
            "prompt must call out that this is a revision-triggered re-review"
        );
        assert!(
            prompt.contains("sha_reviewed_at_1843"),
            "prompt must embed the previously-reviewed head SHA"
        );
        assert!(
            prompt.contains("Prioritise the delta"),
            "prompt must instruct the reviewer to prioritise the delta since the last review"
        );
        assert!(
            prompt.contains("Do not scope your review to a diff-only view"),
            "prompt must explicitly override diff-only scoping for whole-PR-state defects"
        );
        assert!(
            prompt.contains("two complete copies"),
            "prompt must state the whole-PR duplication consideration (motivating incident)"
        );
    }

    /// A first review (no prior pass) must NOT show the revision-context
    /// section — it would be misleading noise on a PR that has never been
    /// reviewed before.
    #[test]
    fn first_review_prompt_omits_revision_context_section() {
        let ctx = PrReviewContext {
            pr_number: 42,
            base_sha: "base000".to_owned(),
            head_sha: "head111".to_owned(),
            changed_files: vec!["src/lib.rs".to_owned()],
            diff_content: None,
            last_reviewed_sha: None,
            supersession_flags: Vec::new(),
            merged_parent_deletions: Vec::new(),
        };
        let prompt = render_reviewer_initial_prompt(
            "Add a feature",
            "Implement the new feature.",
            "https://github.com/org/repo/pull/42",
            "/tmp/bwo/exec.json",
            ReviewScope::Code,
            Some(&ctx),
            "org/repo",
        );
        assert!(
            !prompt.contains("Reviewing a revision"),
            "first review must not carry revision-context framing"
        );
    }

    #[test]
    fn docs_only_scope_prompt_contains_docs_rubric_and_no_code_rubric() {
        let prompt = render_reviewer_initial_prompt(
            "Add design doc",
            "Write a design doc for feature X.",
            "https://github.com/org/repo/pull/100",
            "/tmp/bwo/exec.json",
            ReviewScope::DocsOnly,
            None,
            "org/repo",
        );
        assert!(prompt.contains("docs-only PR"));
        assert!(prompt.contains("light rubric"));
        assert!(prompt.contains("Internal consistency"));
        assert!(prompt.contains("Required-sections check"));
        // Code rubric must NOT appear in docs-only prompt.
        assert!(!prompt.contains("code PR"));
        assert!(!prompt.contains("T793"));
    }

    #[test]
    fn reviewer_initial_prompt_states_speed_balance() {
        let prompt = render_reviewer_initial_prompt(
            "T",
            "D",
            "https://github.com/pr/1",
            "/tmp/bwo/exec.json",
            ReviewScope::Code,
            None,
            "org/repo",
        );
        assert!(prompt.contains("fast, high-signal"));
        assert!(prompt.contains("scrutiny budget"));
    }

    #[test]
    fn reviewer_initial_prompt_directs_write_to_artifact_path() {
        let prompt = render_reviewer_initial_prompt(
            "T",
            "D",
            "https://github.com/pr/1",
            "/var/tmp/boss-worker-output/exec_abc_1.json",
            ReviewScope::Code,
            None,
            "org/repo",
        );
        // The artifact path is embedded verbatim as the primary output target.
        assert!(
            prompt.contains("/var/tmp/boss-worker-output/exec_abc_1.json"),
            "prompt must embed the literal artifact path"
        );
        assert!(
            prompt.contains("$BOSS_STRUCTURED_OUTPUT"),
            "prompt must reference the env var fallback"
        );
        assert!(
            prompt.contains("`Write` tool"),
            "prompt must instruct using the Write tool"
        );
        // Fenced JSON is kept as a transitional fallback, not the primary.
        assert!(
            prompt.contains("fallback"),
            "prompt must describe the fenced-JSON fallback"
        );
    }

    #[test]
    fn render_revision_instructions_contains_title_and_detail() {
        let result = ReviewResult {
            pr_url: "https://github.com/org/repo/pull/5".to_owned(),
            head_sha: String::new(),
            summary: "One bug found.".to_owned(),
            revision_warranted: true,
            findings: vec![
                ReviewFinding::builder()
                    .severity(ReviewFindingSeverity::High)
                    .category(ReviewFindingCategory::Correctness)
                    .file("src/main.rs")
                    .location("fn handle, ~L42")
                    .title("Null pointer dereference")
                    .detail("The handle function dereferences without a null check; add a guard.")
                    .confidence(ReviewFindingConfidence::High)
                    .build(),
            ],
            regression_check: RegressionCheck {
                performed: true,
                suspected_deletions: vec![],
            },
        };
        let instructions = render_revision_instructions(&result);
        assert!(instructions.contains("Null pointer dereference"));
        assert!(instructions.contains("src/main.rs"));
        assert!(instructions.contains("fn handle, ~L42"));
        assert!(instructions.contains("add a guard"));
        assert!(instructions.contains("One bug found."));
        assert!(instructions.contains("high")); // severity
    }

    #[test]
    fn render_revision_instructions_sorts_by_severity() {
        let result = ReviewResult {
            pr_url: String::new(),
            head_sha: String::new(),
            summary: String::new(),
            revision_warranted: true,
            findings: vec![
                ReviewFinding::builder()
                    .severity(ReviewFindingSeverity::Low)
                    .category(ReviewFindingCategory::Readability)
                    .file("a.rs")
                    .title("Low finding")
                    .detail("minor")
                    .confidence(ReviewFindingConfidence::Low)
                    .build(),
                ReviewFinding::builder()
                    .severity(ReviewFindingSeverity::Critical)
                    .category(ReviewFindingCategory::Correctness)
                    .file("b.rs")
                    .title("Critical finding")
                    .detail("urgent")
                    .confidence(ReviewFindingConfidence::High)
                    .build(),
            ],
            regression_check: RegressionCheck {
                performed: true,
                suspected_deletions: vec![],
            },
        };
        let instructions = render_revision_instructions(&result);
        let critical_pos = instructions.find("Critical finding").expect("present");
        let low_pos = instructions.find("Low finding").expect("present");
        assert!(critical_pos < low_pos, "critical must appear before low");
    }

    #[test]
    fn render_revision_instructions_forbids_punt_patterns() {
        let result = ReviewResult {
            pr_url: "https://github.com/org/repo/pull/5".to_owned(),
            head_sha: String::new(),
            summary: "A finding.".to_owned(),
            revision_warranted: true,
            findings: vec![
                ReviewFinding::builder()
                    .severity(ReviewFindingSeverity::Medium)
                    .category(ReviewFindingCategory::Correctness)
                    .file("src/lib.rs")
                    .title("Write-only field")
                    .detail("The field is written but never read; wire the read path.")
                    .confidence(ReviewFindingConfidence::High)
                    .build(),
            ],
            regression_check: RegressionCheck {
                performed: true,
                suspected_deletions: vec![],
            },
        };
        let instructions = render_revision_instructions(&result);
        // Anti-punt mandates must be present.
        assert!(
            instructions.contains("no punting"),
            "instructions must forbid punting: {instructions}"
        );
        assert!(
            instructions.contains("follow-up task"),
            "instructions must forbid filing a follow-up task: {instructions}"
        );
        assert!(
            instructions.contains("TODO"),
            "instructions must forbid TODO deferral: {instructions}"
        );
        assert!(
            instructions.contains("Phase N"),
            "instructions must forbid Phase N deferral: {instructions}"
        );
        assert!(
            instructions.contains("EXPLICITLY SURFACE"),
            "instructions must require explicit surfacing when a finding cannot be fixed: {instructions}"
        );
    }

    /// Acceptance fixture (chore scope, incident PR #1690): a reviewer that
    /// finds a SIXTH hand-rolled Anthropic Messages API client — reusing an
    /// existing client would have avoided it — must produce a finding whose
    /// category ("duplication") forces a revision via `passes_severity_gate`,
    /// and the rendered revision instructions must surface it with the
    /// "duplication" category label and the existing modules to reuse.
    #[test]
    fn duplication_finding_forces_revision_pr_1690_fixture() {
        let result = ReviewResult::from_json(
            &serde_json::json!({
                "pr_url": "https://github.com/spinyfin/mono/pull/1690",
                "head_sha": "deadbeef1690",
                "summary": "The PR adds a new Anthropic Messages API client for the widget \
                    detector, but the repo already has five other hand-rolled clients \
                    (planner.rs, magic_wand.rs, attentions_detector.rs, live_status.rs, \
                    pane_summary.rs) that construct the same api.anthropic.com endpoint, \
                    x-api-key header, and anthropic-version constant.",
                "revision_warranted": true,
                "findings": [
                    {
                        "severity": "medium",
                        "category": "duplication",
                        "file": "tools/boss/engine/core/src/widget_detector.rs",
                        "location": "fn call_anthropic, ~L40",
                        "title": "Sixth hand-rolled Anthropic Messages API client",
                        "detail": "This constructs a new reqwest client against \
                            https://api.anthropic.com/v1/messages with its own x-api-key \
                            and anthropic-version wiring. The repo already has five \
                            equivalents: planner.rs, magic_wand.rs, attentions_detector.rs, \
                            live_status.rs, and pane_summary.rs. Reuse one of those or \
                            extract a shared anthropic_client module instead of adding a sixth.",
                        "confidence": "high"
                    }
                ],
                "regression_check": {"performed": true, "suspected_deletions": []}
            })
            .to_string(),
        )
        .expect("fixture ReviewResult must parse");

        assert!(
            passes_severity_gate(&result),
            "a confirmed duplication finding must force a revision (revision-required, not advisory)"
        );

        let instructions = render_revision_instructions(&result);
        assert!(
            instructions.contains("duplication"),
            "instructions must label the duplication category"
        );
        assert!(
            instructions.contains("Sixth hand-rolled Anthropic Messages API client"),
            "instructions must include the finding title"
        );
        assert!(
            instructions.contains("planner.rs") && instructions.contains("magic_wand.rs"),
            "instructions must name the existing modules to reuse"
        );
    }
}
