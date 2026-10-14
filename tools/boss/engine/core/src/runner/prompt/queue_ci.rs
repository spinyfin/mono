//! Queue failures need local verification of the combined tree, even when the
//! branch itself is green. Shared by revision and legacy remediation prompts.

use super::*;

pub(super) fn fragment(attempt: &CiRemediation) -> String {
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
    if let Ok(checks) = serde_json::from_str::<Vec<serde_json::Value>>(&attempt.failed_checks)
        && let Some(base) = checks.iter().find_map(|c| c["queue_base_sha"].as_str())
    {
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
3. On that main-rebased tree, use `bazel test` for EVERY failed test target, and `bazel build` for every failed build target. A fast whole-job failure requires the job's FULL build/test invocation and target set from its script, not one isolated test. Include all shards; do not use test filters, suppress tests, or bypass caches. Fix failures and repeat the affected verification. Record each command, output and real exit code. A passing rerun on the old PR branch does not count.
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
    "source_url": "<exact captured failing target_url, including job fragment>",
    "command": "bazel test //path:each_failed_target",
    "exit_code": 0,
    "output": "<Bazel result summary>",
    "coverage": "<all failed targets from this job, or its full invocation for a whole-job failure>"
  }]
}
```
For a failing lint job, record `checkleft run` and its output as that job's verification command. Include every failing check name and job URL and every required build/test invocation. Only successful verification of this tree permits a terminal declaration. The engine rejects missing, incomplete, wrong-queue or failing evidence every time; repeating the declaration does not waive the gate. Do not classify from green PR-head checks, hand back coordinator resubmission, or use branch-CI noop/retrigger shortcuts for this queue failure. `engine ci classify` also requires `--queue-ci-evidence-file` before accepting a queue verdict.

If verification passed but a genuine external blocker prevents recovery, both `"$BOSS_BIN" engine ci mark-failed --attempt-id <attempt> --reason <reason> --queue-ci-evidence-file /tmp/queue-ci-evidence.json` and `"$BOSS_BIN" propose done --outcome blocked --summary <reason> --queue-ci-evidence-file /tmp/queue-ci-evidence.json` require that evidence. State the main SHA, targets and results in the summary too. An unsatisfied gate must stay loud; do not claim completion.
"#);
    if attempt.failure_kind.as_deref() == Some("merge_queue_rebounce") {
        out.push_str(r#"
### Re-enqueue the verified GitHub head

If you pushed a changed head and its checks are still pending, arm normal queue entry with `gh pr merge <PR URL> --auto --squash --match-head-commit <verified_head_sha>` (use the repository's configured merge method). A successful request lets GitHub enqueue once checks pass; declare delivered with the verification evidence and stop without waiting for CI. An actual queue-entry receipt is required when delivering an unchanged head, whose already-green checks cannot establish any progress by themselves.

GitHub's `enqueuePullRequest` mutation accepts the existing head SHA; a new SHA is not required. Read the PR node ID with `gh pr view <PR URL> --json id,headRefOid`, check that headRefOid equals verified_head_sha, and enqueue once using the tested SHA as the concurrency guard:
```sh
gh api graphql -f query='mutation($id: ID!, $head: GitObjectID!) { enqueuePullRequest(input: {pullRequestId: $id, expectedHeadOid: $head}) { mergeQueueEntry { id } } }' -f id='<PR node ID>' -f head='<verified_head_sha>'
```
If already queued, read its existing entry ID with `gh api graphql -f query='query($id: ID!) { node(id: $id) { ... on PullRequest { headRefOid mergeQueueEntry { id } } } }' -f id='<PR node ID>'`. Put the returned entry ID in `queue_entry_id` in the evidence file. A rejected mutation is not a receipt: resolve its error, do not loop enqueue until green. This requests a normal queue build with all checks still required. Do not poll CI to completion.
"#);
    } else {
        out.push_str("\n### Re-enter the Trunk queue\n\nDeclare the verified revision delivered. Boss's existing poller resubmits once the revision is in review and head CI is green; it does not require a changed head SHA. Do not submit a competing queue request.\n");
    }
    out.push_str("\nFinish with `\"$BOSS_BIN\" propose done --outcome delivered --summary <main-SHA-and-target-results> --queue-ci-evidence-file /tmp/queue-ci-evidence.json`. Verified requeue is a delivery even when no code changed. This queue-specific completion path replaces any generic requirement to produce a diff or new head SHA.\n");
    out.push_str(check_bypass_prohibition_text());
    out
}
