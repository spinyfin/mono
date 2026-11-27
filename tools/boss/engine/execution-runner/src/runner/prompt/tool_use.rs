//! How a worker is taught to use its tools: declaring a wait on a long-running
//! job, and (for one arm of an experiment) batching independent lookups.

/// Teach the worker the structured wait verb so a legitimate background
/// build/test wait is declared instead of narrated in prose. The produce-a-PR
/// nudge probe repeats the same command; this prompt copy is the earlier
/// teaching so the worker can declare before the first Stop.
pub(super) fn worker_wait_protocol_directive() -> String {
    "\n## If you are waiting on a long-running job\n\n\
     If you are legitimately waiting on a background build, test gate, or similar job, \
     run this command rather than ending the turn in prose:\n\n\
     ```\n\
     \"$BOSS_BIN\" propose wait --reason \"<what you are waiting on>\" --duration <bound>\n\
     ```\n\n\
     `--duration` is required (for example `30m` or `2h`; the engine caps a single wait at 2 hours). \
     Optionally pass `--waiting-on <task-id|pid|file>`. Re-running renews the wait. An accepted \
     wait remains valid until its deadline, including across turns answering status questions \
     or checking the job. Those turns neither cancel nor extend it; renew only if the job \
     needs more time. Each declaration charges its full duration against the cap, including \
     overlapping renewals; choose the expected remaining wait. An unexpired \
     wait holds the produce-a-PR nudge ladder and the nudge circuit breaker; it does not pause \
     the stale-worker reap or other safety checks. After expiry, normal nudging resumes. Total \
     wait-extension per execution is also capped.\n"
        .to_string()
}

/// Worker rule telling a worker to issue independent searches and reads in one
/// turn. It states the habit and the one exception, and names nothing about why
/// it is there: a worker has no use for the experiment it happens to be in.
const BATCHED_SEARCH_DIRECTIVE: &str = "\n**Batch independent tool calls.** When you need several independent searches, listings or file reads, issue them together as parallel tool calls in a single turn rather than one per turn. Locate with a few searches issued together, then read the files they point to in one batch. Sequence calls only when a later call needs the result of an earlier one.\n";

/// Insert [`BATCHED_SEARCH_DIRECTIVE`] directly under the prompt's opening
/// line, where the other rules every worker inherits sit.
pub(super) fn with_batched_search_directive(prompt: String) -> String {
    match prompt.split_once('\n') {
        Some((opening, rest)) => format!("{opening}\n{BATCHED_SEARCH_DIRECTIVE}{rest}"),
        None => format!("{prompt}{BATCHED_SEARCH_DIRECTIVE}"),
    }
}
