//! CI-remediation prompt fragments: the `ci_remediation` execution kind's
//! templated retrigger-only prompt, the CI-fix revision fragment
//! (`created_via = "ci-fix:<crm_id>"`), and the shared rendering helpers
//! (failed-check markdown, ready-to-run `bk` log commands, conflict-
//! diagnosis markdown). Split out of `runner/prompt.rs` to keep that file
//! under the repo's `file/size` check — see the module's call sites in
//! `compose_execution_prompt` and `compose_revision_directive`.

use std::path::Path;

use crate::ci_log_reader::{parse_buildkite_build_id, parse_buildkite_pipeline_slug};
use crate::conflict_diagnosis::ConflictDiagnosis;
use crate::runner::work_item::work_item_name;
use crate::work::{CiRemediation, WorkExecution, WorkItem};

use super::check_bypass_prohibition_text;

fn compose_queue_ci_fragment(attempt: &CiRemediation) -> String {
    let mut out = format!(
        "\n## Merge-queue CI verification\n\nFailure source: the failing merge-queue build, its jobs and logs below. The PR head's checks are not evidence.\n\nPR: {}\nBranch: `{}`\nAttempt: `{}`\nQueue commit: `{}`\n\n### Failing required checks\n\n",
        attempt.pr_url,
        attempt.head_branch,
        attempt.id,
        attempt
            .before_commit_sha
            .as_deref()
            .unwrap_or("resolve from the failing construction build"),
    );
    out.push_str(&render_failed_checks_markdown(&attempt.failed_checks).unwrap_or_else(||
        "No checks captured. Resolve the failing build from the queue commit's commit statuses and check-runs; never substitute `gh pr checks`. If the build cannot be obtained, verification remains unsatisfied and the run must remain visibly failed.\n".into()));
    if let Some(commands) = render_bk_log_commands(&attempt.failed_checks) {
        out.push_str(&commands);
    }
    if let Some(base) = &attempt.queue_base_sha {
        out.push_str(&format!(
            "\nQueue base commit (first parent of the failing construction): `{base}`\n"
        ));
    }
    if let Some(log) = &attempt.log_excerpt {
        out.push_str(&format!("\nQueue job log excerpt:\n```\n{log}\n```\n"));
    }
    out.push_str(r#"
Before any verdict (including flaky, infra, unfixable or cannot complete):

1. Read the failing queue build's jobs and logs. Record its queue commit and queue base commit. Fetch the queue commit's parents with `gh api repos/<owner>/<repo>/commits/<queue-sha> --jq '.parents[].sha'` and use the build's checkout/merge metadata to identify the base (a batch can contain sibling PRs). This is the historical queue base, distinct from today's main. Fetch statuses at that queue SHA if the build was not captured: `gh api repos/<owner>/<repo>/commits/<queue-sha>/status`; also inspect `/check-runs`.
2. Fetch `origin`, then rebase this PR's changes onto current `main@origin` using `"$CUBE_BIN" workspace rebase`. Resolve conflicts by integrating both sides. Record the full `main@origin` SHA and the final tested head SHA. Preserve and advance the execution recovery bookmarks after rebasing. Do this even if main is already an ancestor: verify and record that fact.
3. On that main-rebased tree, use `bazel test` for EVERY failed test target, and `bazel build` for every failed build target. A fast whole-job failure requires the job's FULL build/test invocation and target set from its script, not one isolated test. A job that does not use Bazel (e.g. an `npm ci && npm run build && npm run test` frontend job) is verified by running that job's own npm/pnpm/yarn invocation in full. Include all shards; do not use test filters, suppress tests, or bypass caches. Fix failures and repeat the affected verification. Record each command, output and real exit code. A passing rerun on the old PR branch does not count.
4. Run `checkleft run`. Push substantive changes or a changed rebase through `"$CUBE_BIN" pr update` to the existing PR. If the verified head is unchanged, do not make an empty or cosmetic commit: requeue that same head as described below.

Write a JSON evidence file, for example `/tmp/queue-ci-evidence.json`:
```json
{
  "main_sha": "<full current origin/main SHA used for rebase>",
  "verified_head_sha": "<full final tested PR head SHA>",
  "queue_sha": "<full failing queue commit SHA>",
  "queue_base_sha": "<full base SHA of that failing queue build>",
  "runs": [{
    "check_name": "<exact captured failing check name>",
    "source_url": "<captured target_url including job fragment, or resolved HTTPS build URL if absent>",
    "command": "bazel test //path:each_failed_target",
    "exit_code": 0,
    "output": "<Bazel result summary>",
    "coverage": "<all failed targets from this job, or its full invocation for a whole-job failure>"
  }]
}
```
For a failing lint job (the captured check name's last `/` segment is the Buildkite job label), record `checkleft run` and its output as that job's verification command. Include every failing check name and job URL and every required build/test invocation. Only successful verification of this tree permits a terminal declaration. The engine rejects missing, incomplete, wrong-queue or failing evidence every time; repeating the declaration does not waive the gate. Do not classify from green PR-head checks, hand resubmission back to someone else, or use branch-CI noop/retrigger shortcuts for this queue failure. `engine ci classify` requires `--queue-ci-evidence-file` for every class except `tractable`; `tractable` is metadata and can be recorded before verification.

If verification passed but a genuine external blocker prevents recovery, both `"$BOSS_BIN" engine ci mark-failed --attempt-id <attempt> --reason <reason> --queue-ci-evidence-file /tmp/queue-ci-evidence.json` and `"$BOSS_BIN" propose done --outcome blocked --summary <reason> --queue-ci-evidence-file /tmp/queue-ci-evidence.json` require that evidence. State the main SHA, targets and results in the summary too. An unsatisfied gate must stay loud; do not claim completion.
"#);
    if attempt.failure_kind.as_deref() == Some("merge_queue_rebounce") {
        out.push_str(r#"
### Re-enqueue the verified GitHub head

If you pushed a changed head and its checks are still pending, arm normal queue entry with `gh pr merge <PR URL> --auto --squash --match-head-commit <verified_head_sha>` (use the repository's configured merge method). A successful request lets GitHub enqueue once checks pass; declare delivered with the verification evidence and stop without waiting for CI. An actual queue-entry receipt is required when delivering an unchanged head, whose already-green checks cannot establish any progress by themselves.

GitHub's `enqueuePullRequest` mutation accepts the existing head SHA; a new SHA is not required. Read the PR node ID with `gh pr view <PR URL> --json id,headRefOid`, check that headRefOid equals verified_head_sha, and enqueue once using the tested SHA as the concurrency guard:
```sh
gh api graphql -f query='mutation($id: ID!, $head: GitObjectID!) { enqueuePullRequest(input: {pullRequestId: $id, expectedHeadOid: $head}) { mergeQueueEntry { id enqueuedAt } } }' -f id='<PR node ID>' -f head='<verified_head_sha>'
```
If already queued, read its existing entry ID with `gh api graphql -f query='query($id: ID!) { node(id: $id) { ... on PullRequest { headRefOid mergeQueueEntry { id enqueuedAt } } } }' -f id='<PR node ID>'`. Put the returned entry ID in `queue_entry_id` and its `enqueuedAt` in `queue_entry_enqueued_at` in the evidence file; the engine checks the receipt against GitHub after you declare and flags a mismatch. A rejected mutation is not a receipt: resolve its error, do not loop enqueue until green. This requests a normal queue build with all checks still required. Do not poll CI to completion.
"#);
    } else {
        out.push_str("\n### Re-enter the Trunk queue\n\nDeclare the verified revision delivered. The engine's existing poller resubmits once the revision is in review and head CI is green; it does not require a changed head SHA. Do not submit a competing queue request.\n");
    }
    out.push_str("\nFinish with `\"$BOSS_BIN\" propose done --outcome delivered --summary <main-SHA-and-target-results> --queue-ci-evidence-file /tmp/queue-ci-evidence.json`. Verified requeue is a delivery even when no code changed. This queue-specific completion path replaces any generic requirement to produce a diff or new head SHA.\n");
    out.push_str(check_bypass_prohibition_text());
    out
}

/// Signal-specific fragment appended to `compose_revision_directive` when the
/// revision was created with `created_via = "ci-fix:<crm_id>"`.
///
/// Provides the CI remediation context (failing checks, log excerpt, playbook)
/// that the worker needs to fix the failing CI — identical in content to the
/// bespoke `compose_ci_remediation_prompt` except that the branch/push spine
/// is already covered by the shared revision directive.
pub(super) fn compose_ci_remediation_fragment(attempt: &CiRemediation) -> String {
    if crate::work::queue_ci_evidence::queue_ci_verification_required(attempt) {
        return compose_queue_ci_fragment(attempt);
    }
    let is_rebounce = attempt.failure_kind.as_deref() == Some("merge_queue_rebounce");
    let is_trunk_eviction = attempt.failure_kind.as_deref() == Some("trunk_queue_eviction");

    let mut out = String::new();
    out.push_str("\n---\n\n");

    if is_rebounce {
        out.push_str(&format!(
            "## CI remediation context: PR #{pr_num} ({kind}) — merge-queue FAILED_CHECKS\n\n",
            pr_num = attempt.pr_number,
            kind = attempt.attempt_kind,
        ));
        out.push_str(
            "> **Important**: this is a **merge-queue rebounce**, not a per-PR CI failure.\n\
             > - The PR's own required checks are **green** on its head SHA.\n\
             > - **`gh pr checks` will show green** — this is expected and does NOT mean CI passed.\n\
             >   Do NOT run `gh pr checks` and conclude there is nothing to fix. The actual failing\n\
             >   build is on the **synthetic merge commit** on a `gh-readonly-queue/*` branch,\n\
             >   listed under \"Failing required checks\" below with its build URL and job id.\n\
             > - Root cause: something landed on `main` between this PR's CI run and its queue turn\n\
             >   that is semantically incompatible. After fixing, **re-enqueue** the PR.\n\n",
        );
    } else if is_trunk_eviction {
        out.push_str(&format!(
            "## CI remediation context: PR #{pr_num} ({kind}) — Trunk merge-queue eviction\n\n",
            pr_num = attempt.pr_number,
            kind = attempt.attempt_kind,
        ));
        out.push_str(
            "> **Important**: this PR was **evicted from the Trunk merge queue**, not a per-PR CI failure.\n\
             > - The PR's own required checks are **green** on its head SHA.\n\
             > - **`gh pr checks` will show green** — this is expected and does NOT mean CI passed.\n\
             >   Do NOT run `gh pr checks` and conclude there is nothing to fix. The actual failing\n\
             >   build ran on Trunk's ephemeral `trunk-merge/pr-<N>/<uuid>` construction branch,\n\
             >   listed under \"Failing required checks\" below with its build URL and job id.\n\
             > - Root cause: something landed on the target branch between this PR's CI run and its\n\
             >   queue turn that is semantically incompatible. After fixing, just push and stop —\n\
             >   Boss auto-resubmits the PR to the Trunk queue on the next poller pass, once this\n\
             >   revision comes to rest and the PR's head CI reads green. Do NOT ask a human to\n\
             >   re-run the merge, do NOT comment `/trunk merge` yourself, and do NOT run\n\
             >   `gh pr merge` — that bypasses the queue and races the automatic resubmit.\n\
             > - If a failing construction-branch build is listed below, **push a commit** that \
             >   addresses it — the engine refuses every \"nothing to push\" terminal for a \
             >   queue-side failure because the PR's head CI is green already and so proves nothing \
             >   about that build. If no failing build is listed, do **not** invent one: follow the \
             >   STOP section and `\"$BOSS_BIN\" engine ci mark-failed` rather than force-pushing.\n\n",
        );
    } else {
        out.push_str(&format!(
            "## CI remediation context: PR #{pr_num} ({kind}) — required checks failing\n\n",
            pr_num = attempt.pr_number,
            kind = attempt.attempt_kind,
        ));
    }

    if !attempt.head_branch.is_empty() {
        out.push_str(&format!("**Branch**: `{}`\n", attempt.head_branch));
    }
    out.push_str(&format!(
        "**Head sha at trigger**: `{}`\n",
        crate::work::merge_queue_rebounce_pr_head(&attempt.head_sha_at_trigger),
    ));
    out.push_str(&format!("**Attempt id**: `{}`\n\n", attempt.id));

    out.push_str("### Failing required checks\n\n");
    let captured_checks = render_failed_checks_markdown(&attempt.failed_checks);
    // A Trunk eviction with nothing captured is the shape that destroyed
    // flunge#1137: the engine could not name a failing build, the prompt
    // asserted one existed anyway, and the bail-out below was gated off.
    // Those attempts are now minted (a confirmed queue rejection is
    // authoritative even with no construction build); this STOP section
    // is what keeps the worker from inventing a fix. Merge-queue rebounce
    // *may* land with empty checks when evidence enrichment misses; that
    // path returns early into the queue CI fragment, never the Trunk bail-out.
    let trunk_eviction_without_evidence = is_trunk_eviction && captured_checks.is_none();
    match captured_checks {
        Some(md) => out.push_str(&md),
        None => {
            if is_trunk_eviction {
                out.push_str(&format!(
                    "_The engine did not capture the failing checks for this Trunk queue eviction. \
                     Do NOT use `gh pr checks` — it shows the PR-head checks, which are green. \
                     Instead, discover the failing build directly on Buildkite (org-wide, since more \
                     than one pipeline may build the episode branch): \
                     `bk api \"/builds?branch=trunk-merge/pr-{pr_num}/<episode-uuid>\"` if the episode \
                     uuid is known, otherwise `bk api \"/builds?state[]=failed&state[]=failing&per_page=100\"` \
                     filtered client-side to a branch starting with `trunk-merge/pr-{pr_num}/` (do NOT \
                     match `trunk-temp/*` — that is a different, non-gating branch). **A single page — \
                     especially an empty or truncated one — is not proof no such build exists.** Page \
                     with `&page=N` until a page comes back with fewer than `per_page` results, or state \
                     explicitly how many pages you searched, before concluding there is none._\n",
                    pr_num = attempt.pr_number,
                ));
            } else {
                out.push_str(
                    "_The engine did not record a parseable `failed_checks` blob for this attempt. \
                     Read `gh pr checks` to enumerate the failing required checks before deciding the fix._\n",
                );
            }
        }
    }
    out.push('\n');

    if let Some(bk_cmds) = render_bk_log_commands(&attempt.failed_checks) {
        out.push_str(&bk_cmds);
    }

    // The bail-out for "the engine cannot name a failing build". It is NOT
    // `mark-noop`: `handle_mark_ci_remediation_noop` rejects every queue-side
    // attempt outright, before it probes, because head-branch CI is green by
    // construction for these and so cannot validate the claim. Pointing a
    // worker at a verb the engine is guaranteed to refuse would be worse than
    // saying nothing. `mark-failed` is the terminal the engine does accept —
    // it is what the rejection message itself recommends.
    if trunk_eviction_without_evidence {
        out.push_str("### If there is no failing build to find (STOP — do not invent one)\n\n");
        out.push_str(&format!(
            "The engine could not identify a failing build for this eviction, and its own \
             classification could not confirm the cause either. Trunk reports the same `failed` state \
             whether a construction build went red, the PR genuinely conflicts with the target branch, \
             or it merely collided with a sibling PR in the same batch — so it is entirely possible \
             **nothing is broken on this PR**, but the engine does not know that, and neither do you \
             yet. Determine the cause yourself, in this order, before deciding what (if anything) to \
             do:\n\n\
             1. **Trunk's newest bot comment** — `gh api repos/<owner>/<repo>/issues/{pr_num}/comments` \
             (paginate if needed) and read the newest `trunk-io[bot]` entry. Its prose names the cause \
             directly: \"...because there was a merge conflict\" is a real conflict against the target \
             branch; \"...because it conflicted with #N\" is a sibling-in-queue collision, not a defect \
             on this PR.\n\
             2. **GitHub's live mergeability** — `gh pr view {pr_num} --json mergeable,mergeStateStatus`, \
             read fresh, not from memory or an earlier command's output. `CONFLICTING` confirms a real \
             conflict. `UNKNOWN` means GitHub is still recomputing — re-run rather than treat it as an \
             answer either way.\n\
             3. **`jj status`** after `jj workspace update-stale` — names conflicted files offline, if \
             your own workspace copy has any.\n\
             4. **The Buildkite search above** — exhaustive (every page, or an explicit \"searched N \
             pages\" claim), not a single possibly-truncated page.\n\n\
             If every one of these says the PR is clean — no conflict, no sibling-in-queue collision, \
             and no failing `trunk-merge/pr-{pr_num}/*` build after an exhaustive search — that is your \
             answer: Trunk never got as far as testing, or the eviction has already cleared. Record it \
             and stop. The engine accepts a give-up without evidence only with `--reason` one of \
             `no-failing-build-found`, `merge-conflict` or `sibling-collision`:\n\n\
             ```\n\
             \"$BOSS_BIN\" engine ci mark-failed --attempt-id {attempt} --reason no-failing-build-found\n\
             ```\n\n\
             **If the search DOES find a failing `trunk-merge/pr-{pr_num}/*` build**, this is a real \
             merge-queue CI failure: rebase onto current `main@origin`, run every failed target with \
             Bazel (the full job invocation for whole-job failures), and give up or deliver only \
             with `--queue-ci-evidence-file` (main_sha, verified_head_sha, queue_sha, queue_base_sha \
             and successful runs). Without that file the engine rejects the give-up.\n\n\
             **Do NOT** rebase, reset, force-push, or \"resolve\" anything unless one of the checks \
             above found something concrete on **this PR** for you to act on. A revision whose head \
             ends up with an empty diff has destroyed the PR's contents, not fixed them. If your work \
             would produce a zero-diff commit, stop and mark the attempt failed instead.\n\n",
            pr_num = attempt.pr_number,
            attempt = attempt.id,
        ));
    }

    if !is_trunk_eviction {
        out.push_str("### If CI is already green (nothing to fix)\n\n");
        out.push_str(&format!(
            "Before assuming there is work to do, check the **current** state of the PR's required \
             checks (`gh pr checks {pr}` / `gh pr view {pr}`). If they are **already passing** — the \
             failure cleared on its own (a flaky check settled, `main` moved, or a stale failure was \
             re-detected) — you do NOT have to invent a fix. Declare it; the engine VALIDATES your \
             claim against live CI before retiring the attempt:\n\n\
             ```\n\
             \"$BOSS_BIN\" engine ci mark-noop --attempt-id {attempt} --observed-sha <current-head-sha> --reason already-green\n\
             ```\n\n\
             The engine independently re-probes live CI for the PR's current head SHA. If every \
             required check is verified green, the attempt is retired and the parent unblocks — you are \
             **done, stop**. If CI is still red or pending, the command **fails** (non-zero exit) with \
             the live status and the attempt stays open: the failure is real, so continue below.\n\n",
            pr = attempt.pr_url,
            attempt = attempt.id,
        ));
    }

    if attempt.attempt_kind == "retrigger" {
        out.push_str("### Action: retrigger the failing build\n\n");
        out.push_str(
            "The engine has pre-classified this failure as infra (every failing check has \
             `conclusion ∈ {STARTUP_FAILURE, CANCELLED}`). No log read or code change is needed.\n\n",
        );
        out.push_str(
            "1. Re-run the failing build via the per-provider CLI (`bk build retry <build-id>` \
             for Buildkite or `gh run rerun <run-id> --failed` for GitHub Actions). The failing \
             check's `target_url` above carries the right id.\n\
             2. Call `\"$BOSS_BIN\" engine ci mark-retriggered --attempt-id <attempt-id> --new-id <new-build-or-run-id>` \
             so the engine records the new run id and stays out of the budget path. Do NOT call \
             `mark-failed` or push code.\n\
             3. Stop. The merge-poller will observe the re-run's outcome on the next sweep.\n\n",
        );
    } else if !trunk_eviction_without_evidence {
        out.push_str("### Action: rebase first, then fix\n\n");
        out.push_str(
            "Many CI failures on long-running PRs are caused by `main` moving. The cheapest \
             experiment is rebasing onto `main` HEAD before changing any code — if CI goes \
             green after the rebase, no fix-attempt slot is consumed.\n\n",
        );
        out.push_str("**Step 1 — Rebase onto base HEAD and force-push** (replaces step 3 above).\n\n");
        out.push_str(&format!(
            "```\n\
             jj edit {branch}\n\
             jj rebase -d main -b {branch}\n\
             # then push via step 5 of the revision directive\n\
             ```\n\n",
            branch = if attempt.head_branch.is_empty() {
                "<branch>"
            } else {
                attempt.head_branch.as_str()
            },
        ));
        out.push_str(
            "**If the rebase produces conflicts on a stacked branch:** jj records conflicts \
             IN each commit independently — `jj git push` refuses to push ANY commit that \
             still contains a conflict, including ancestors. Resolving only the tip does NOT \
             clear ancestor conflicts. List conflicted commits and resolve from the base upward:\n\
             ```\n\
             # list conflicted commits\n\
             jj log -r '::<branch>' -T 'change_id ++ \" \" ++ description.first_line() ++ \" conflicts=\" ++ conflict ++ \"\\n\"'\n\
             # edit the lowest conflicted commit, fix it, repeat upward\n\
             jj edit <lowest-conflicted-change-id>\n\
             # verify: output must contain no 'true'\n\
             jj log -r '::<branch>' -T 'conflict ++ \"\\n\"'\n\
             ```\n\
             Always pass `-m \"…\"` to `jj describe`/`jj squash`/`jj commit`/`jj new` — \
             `EDITOR=false` in this environment; any command that opens an editor hard-fails.\n\n",
        );
        out.push_str(
            "Wait for the re-run's required checks to settle (`gh pr checks --watch`). Then:\n\n\
             - **If post-rebase CI is green**, call \
             `\"$BOSS_BIN\" engine ci mark-succeeded-via-rebase --attempt-id <attempt-id>`. The engine \
             independently re-probes live CI for the PR's current head SHA before honoring this — \
             calling it early or on a red head gets a rejection (non-zero exit), not a recorded \
             success, so actually wait for `--watch` to finish. On a verified-green response, stop; \
             the engine flips the attempt to `succeeded`, sets `consumes_budget = 0`, and decrements \
             `tasks.ci_attempts_used` so this attempt does not count against the PR's budget.\n\
             - **If post-rebase CI is still red**, continue to Step 2. The budget slot is now \
             consumed; this is the fix attempt the engine pre-classified.\n\n",
        );

        out.push_str("**Step 2 — Read the log, classify, fix, push.**\n\n");
        out.push_str("Engine-collected log excerpt (failing job tail):\n\n");
        match attempt.log_excerpt.as_deref().map(str::trim) {
            Some(tail) if !tail.is_empty() => {
                out.push_str("```\n");
                out.push_str(tail);
                out.push_str("\n```\n\n");
            }
            _ => {
                out.push_str(
                    "_The engine's pre-spawn log fetch did not produce an excerpt for this attempt. \
                     Use the ready-to-run commands above (`bk job log --pipeline …`) or \
                     `gh run view --log-failed --job <job-id>` (job id from the failing check URL)._\n\n",
                );
            }
        }
        out.push_str(
            "1. Classify the failure with `\"$BOSS_BIN\" engine ci classify --attempt-id <attempt-id> --class <tractable|flaky_or_infra|unfixable>`.\n   \
                - `tractable` → there's a clear code change that resolves it. Make it. Push.\n   ",
        );
        out.push_str(
            "- `flaky_or_infra` → the failure is environmental. Pivot to the retrigger playbook \
            (re-run the failing build via the provider CLI and call `mark-retriggered`).\n   ",
        );
        out.push_str(
            "- `unfixable` → the failure is real and out of scope. Call \
                `\"$BOSS_BIN\" engine ci mark-failed --attempt-id <attempt-id> --reason <reason>` \
                and stop. Do NOT push.\n",
        );
        out.push_str("2. No `test_command` context is available here; rely on CI to verify the push.\n");
        out.push_str(&format!(
            "3. Push your fix via step 5 of the revision directive (push to the parent branch \
                `{branch}`). The merge-poller will observe the new head sha and re-evaluate CI on \
                the next sweep — when green it flips the attempt to `succeeded` and unblocks the parent.\n\n",
            branch = if attempt.head_branch.is_empty() {
                "<branch>"
            } else {
                attempt.head_branch.as_str()
            },
        ));
    }

    out.push_str("### Stop conditions\n\n");
    out.push_str(
        "- **You are not adding scope.** The only allowed change is one that makes the failing \
         required checks pass (rebase, infra retrigger, or a focused fix).\n\
         - **Do not close the PR yourself.** Closing is the human's call.\n\
         - **Always pass `-m \"…\"` to `jj describe` / `jj squash`.** The worker \
         environment has no usable `$EDITOR`.\n\n",
    );
    out.push_str(check_bypass_prohibition_text());
    out.push('\n');
    out
}

/// Templated prompt for the `ci_remediation` execution kind, retrigger path
/// only. `fix`-kind CI attempts now dispatch through the revision substrate
/// (`revision_implementation`); only `retrigger` (design Q6: no commit,
/// not revision-shaped) still uses this bespoke execution kind.
pub(super) fn compose_ci_remediation_prompt(
    execution: &WorkExecution,
    work_item: &WorkItem,
    workspace_path: &Path,
    cube_change_id: Option<&str>,
    attempt: &CiRemediation,
    _test_command: Option<&str>,
) -> String {
    let mut prompt = String::new();

    prompt.push_str(&format!(
        "## CI remediation: PR #{pr_num} ({kind}) — required checks failing\n\n",
        pr_num = attempt.pr_number,
        kind = attempt.attempt_kind,
    ));

    prompt.push_str(&format!("**PR**: {}\n", attempt.pr_url));
    if !attempt.head_branch.is_empty() {
        prompt.push_str(&format!("**Branch**: `{}`\n", attempt.head_branch));
    }
    prompt.push_str(&format!(
        "**Head sha at trigger**: `{}`\n",
        crate::work::merge_queue_rebounce_pr_head(&attempt.head_sha_at_trigger),
    ));
    prompt.push_str(&format!("**Workspace**: `{}`\n", workspace_path.display()));
    prompt.push_str(&format!("**Attempt id**: `{}`\n", attempt.id));
    prompt.push_str(&format!("**Execution id**: `{}`\n", execution.id));
    if let Some(change) = cube_change_id {
        prompt.push_str(&format!("**Local change**: `{change}`\n"));
    }
    prompt.push_str(&format!("**Work item**: `{}`\n\n", work_item_name(work_item),));

    if crate::work::queue_ci_evidence::queue_ci_verification_required(attempt) {
        prompt.push_str(&compose_queue_ci_fragment(attempt));
        return prompt;
    }

    // Failing-check list — same JSON the engine seeded on the row at
    // detection time. Rendered as a bulleted summary; the worker has the
    // raw `failed_checks` field if it wants to read further.
    prompt.push_str("### Failing required checks\n\n");
    match render_failed_checks_markdown(&attempt.failed_checks) {
        Some(md) => prompt.push_str(&md),
        None => prompt.push_str(
            "_The engine did not record a parseable `failed_checks` blob for this attempt. \
             Read `gh pr checks` to enumerate the failing required checks before deciding the fix._\n",
        ),
    }
    prompt.push('\n');

    // If the failure already cleared, the worker can declare a
    // validated noop rather than retriggering a build that is no longer
    // red. The engine re-probes live CI before honoring it.
    //
    if attempt.failure_kind.as_deref() != Some("trunk_queue_eviction") {
        prompt.push_str("### If CI is already green (nothing to fix)\n\n");
        prompt.push_str(&format!(
            "Check the **current** required checks first (`gh pr checks {pr}`). If they are already \
             passing, declare it instead of retriggering — the engine validates the claim against live \
             CI before retiring the attempt:\n\n\
             ```\n\
             \"$BOSS_BIN\" engine ci mark-noop --attempt-id {attempt} --observed-sha <current-head-sha> --reason already-green\n\
             ```\n\n\
             Verified green → attempt retired, parent unblocked, you are done. Still red/pending → the \
             command fails (non-zero) and the attempt stays open; fall through to the retrigger playbook.\n\n",
            pr = attempt.pr_url,
            attempt = attempt.id,
        ));
    }

    // §Q4 retrigger playbook: every failure is unambiguous infra,
    // no log read needed, no code change.
    prompt.push_str("### Action: retrigger the failing build\n\n");
    prompt.push_str(
        "The engine has pre-classified this failure as infra (every failing check has \
         `conclusion ∈ {STARTUP_FAILURE, CANCELLED}`). No log read or code change is needed.\n\n",
    );
    prompt.push_str(
        "1. Re-run the failing build via the per-provider CLI (`bk build retry <build-id>` \
         for Buildkite or `gh run rerun <run-id> --failed` for GitHub Actions). The failing \
         check's `target_url` above carries the right id.\n\
         2. Call `\"$BOSS_BIN\" engine ci mark-retriggered --attempt-id <attempt-id> --new-id <new-build-or-run-id>` \
         so the engine records the new run id and stays out of the budget path. Do NOT call \
         `mark-failed` or push code.\n\
         3. Stop. The merge-poller will observe the re-run's outcome on the next sweep.\n\n",
    );

    prompt.push_str("### Stop conditions\n\n");
    prompt.push_str(
        "- **You are not adding scope.** The only allowed change is one that makes the failing \
         required checks pass (infra retrigger only — no code changes).\n\
         - **Do not close the PR yourself.** Closing is the human's call.\n\n",
    );
    prompt.push_str("Respond with concise markdown using exactly these sections:\n");
    prompt.push_str("## Summary\n## Validation\n## Open Questions\n");
    prompt
}

/// Build a block of ready-to-run `bk` CLI commands for every Buildkite
/// entry in the `failed_checks` JSON. Returns `None` when the JSON
/// contains no Buildkite entries or the target URLs lack enough
/// information to construct pre-filled commands.
///
/// Emits two commands per failing Buildkite job:
///   `bk build view <N> --pipeline <slug>`  — enumerate all jobs in the build
///   `bk job log --pipeline <slug> --build-number <N> <job-uuid>`
fn render_bk_log_commands(failed_checks_json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Entry {
        target_url: String,
        provider: String,
        #[serde(default)]
        provider_job_id: Option<String>,
    }
    let entries: Vec<Entry> = serde_json::from_str(failed_checks_json).ok()?;

    let mut commands = String::new();
    for e in &entries {
        if e.provider != "buildkite" {
            continue;
        }
        let Some(pipeline) = parse_buildkite_pipeline_slug(&e.target_url) else {
            continue;
        };
        let Some(build_num) = parse_buildkite_build_id(&e.target_url) else {
            continue;
        };
        commands.push_str(&format!("bk build view {build_num} --pipeline {pipeline}\n",));
        match e.provider_job_id.as_deref() {
            Some(job_id) => {
                commands.push_str(&format!(
                    "bk job log --pipeline {pipeline} --build-number {build_num} {job_id}\n",
                ));
            }
            None => {
                commands.push_str(&format!(
                    "# (replace <job-uuid> with an id from `bk build view` above)\n\
                     bk job log --pipeline {pipeline} --build-number {build_num} <job-uuid>\n",
                ));
            }
        }
    }

    if commands.is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str("### Ready-to-run Buildkite log commands\n\n");
    out.push_str(
        "`bk` is the Buildkite CLI. These commands are pre-filled with the \
         pipeline, build number, and job id — no argument guessing required:\n\n",
    );
    out.push_str("```\n");
    out.push_str(&commands);
    out.push_str("```\n\n");
    Some(out)
}

/// Render the `failed_checks` JSON blob (one entry per failing required
/// check at trigger time) as a small bulleted list for the worker
/// prompt. Returns `None` when the blob is missing or malformed — the
/// caller falls back to a generic instruction.
fn render_failed_checks_markdown(failed_checks_json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
        conclusion: String,
        target_url: String,
        provider: String,
        #[serde(default)]
        provider_job_id: Option<String>,
        #[serde(default)]
        verification: Vec<crate::queue_ci_contract::RequiredRun>,
    }
    let entries: Vec<Entry> = serde_json::from_str(failed_checks_json).ok()?;
    if entries.is_empty() {
        return None;
    }
    let mut out = String::new();
    for e in &entries {
        out.push_str(&format!(
            "- `{name}` — {conclusion} ({provider}): {url}",
            name = e.name,
            conclusion = e.conclusion,
            provider = e.provider,
            url = e.target_url,
        ));
        if let Some(job_id) = e.provider_job_id.as_deref() {
            out.push_str(&format!(" (job `{job_id}`)"));
        }
        out.push('\n');
        for run in &e.verification {
            out.push_str(&format!(
                "  Required {:?} verification ({:?}): `{}`\n",
                run.kind, run.tool, run.command
            ));
        }
    }
    Some(out)
}

pub(super) fn render_diagnosis_markdown(diagnosis: &ConflictDiagnosis) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Schema v{}. Base sha `{}`, dependent head sha `{}`.\n\n",
        diagnosis.schema_version, diagnosis.base_sha, diagnosis.head_sha,
    ));
    if let Some(err) = diagnosis.error.as_deref() {
        out.push_str(&format!(
            "_Engine-side probe failed: {err}. The list below may be incomplete; trust\n\
             `jj st` after the rebase as the source of truth._\n\n",
        ));
    }
    if diagnosis.files.is_empty() {
        if diagnosis.error.is_none() {
            out.push_str(
                "_No conflicted files reported by the engine's pre-spawn probe. The\n\
                 conflict may have been transient; run `jj rebase` and trust `jj st`._\n",
            );
        }
        return out;
    }
    out.push_str(&format!("Conflicted files ({}):\n\n", diagnosis.files.len()));
    for file in &diagnosis.files {
        out.push_str(&format!("- `{}` — {}", file.path, file.shape));
        if let Some(count) = file.marker_count {
            out.push_str(&format!(" ({count} marker block(s))"));
        }
        out.push('\n');
    }
    out
}
