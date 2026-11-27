# Reducing Boss token consumption: where the tokens go and what to change first

- **Date:** 2026-11-19
- **Window measured:** 2026-11-11 20:00 to 2026-11-18 20:00 local, 7 days
- **Inputs:** the token audit for that window (engine `work_runs` token columns, cross-checked against Claude transcripts and Codex rollouts; Grok estimated), source on `main` at `c88d1058`, and vendor documentation fetched on 2026-11-18 and 2026-11-19
- **Scope:** analysis and recommendations only. Nothing was changed, and nothing was run against live Boss state. Follow-up changes are listed at the end.
- **Related:** [mono#3746](https://github.com/spinyfin/mono/pull/3746) (cheaper model policy for progression runs, open)

Boss used about 9.7 billion tokens in the measured week. About 70% went to runs the engine starts on its own (reviews, revisions, guides, follow-ups), and almost every token was context being re-read on a later model call. This document ranks changes against those measurements, shows the arithmetic, and says what has to be instrumented before the less certain changes are tried.

## Verdict

- **The review pipeline is the first place to cut.** Post-merge review and its follow-ups, pre-merge review batches and the revisions they create, and review guides together cost about 5.5B of the 9.7B. Three policy changes there are estimated to save 1.5–2.6B a week (15–27%).
- **Two launch-configuration changes are close to free.** Claude workers load the personal Claude Code environment of whoever runs Boss and never compact. Trimming the starting context and setting an auto-compact window are estimated at 0.35–0.8B and about 0.4B a week.
- **Three things must be measured before tuning further.** Grok usage (about 20% of the total is an estimate), the unit the provider quotas actually count (cache reads are 96% of Claude tokens but about a third of the list price), and how many review findings lead to a real fix.
- **All recommendations together** are estimated at 2.5–3.5B a week, a quarter to a third of the measured week. The individual estimates overlap and must not be added.

## How to read the numbers

- **Tokens** means fresh input + cache writes + cache reads + output, summed over every API call. This is the audit's definition. M is millions, B is billions.
- **Measured** numbers come from the audit. Numbers marked _est._ are either the audit's estimates (all Grok figures, polling, carried tool results) or derived in this document, with the arithmetic shown.
- **Percentages** are of the ≈9.7B total unless a different base is named.
- **Savings** are per week at the measured week's volume and mix.
- **Paths** are relative to `tools/boss/`.

## Where the tokens went

### By driver and model

| Slice                                 | Tokens         | Share |
| ------------------------------------- | -------------- | ----- |
| grok / grok-4.6                       | ≈1,908M _est._ | 19.7% |
| codex / gpt-6-astra                   | 1,746M         | 18.0% |
| claude / opus (workers, all review)   | 1,691M         | 17.5% |
| claude / sonnet                       | 1,451M         | 15.0% |
| claude / fable                        | 1,219M         | 12.6% |
| codex / gpt-6.1-sol                   | 1,218M         | 12.6% |
| coordinator (opus session, subagents) | 400M           | 4.1%  |
| driverless                            | ≈40M           | 0.4%  |
| **Total**                             | **≈9.7B**      |       |

### By pipeline stage

Grok's estimated input is added to the stage the audit attributed it to. The 341M of Grok input that the audit did not attribute to a stage is placed with filed work.

| Stage                                                                  | Measured | Grok _est._ | Total  | Share |
| ---------------------------------------------------------------------- | -------- | ----------- | ------ | ----- |
| Human- and planner-filed work (tasks, chores, investigations, designs) | 2,204M   | 341M        | 2,545M | 26.3% |
| Revisions that address review findings                                 | 1,387M   | 867M        | 2,254M | 23.3% |
| Pre-merge review batches                                               | 1,328M   | –           | 1,328M | 13.7% |
| Post-merge review                                                      | 942M     | –           | 942M   | 9.7%  |
| Review guides                                                          | 476M     | –           | 476M   | 4.9%  |
| Follow-ups created by post-merge review                                | 202M     | 262M        | 464M   | 4.8%  |
| Coordinator                                                            | 400M     | –           | 400M   | 4.1%  |
| Merge-conflict revisions                                               | 274M     | 93M         | 367M   | 3.8%  |
| Unstick revisions                                                      | 182M     | 167M        | 349M   | 3.6%  |
| CI-fix revisions                                                       | 138M     | 178M        | 316M   | 3.3%  |
| Progression sweeps and the chores they spawn                           | 221M     | –           | 221M   | 2.3%  |

Everything except the first and seventh rows is engine-minted: ≈6.7B, 69% of the total.

### Per pull request

324 PRs had executions in the window, so an average PR cost ≈30M tokens (9.7B / 324). Dividing each stage by 324 gives the shape of that cost (_est._):

| Part of an average PR                  | Tokens |
| -------------------------------------- | ------ |
| The run that wrote the change          | ≈7.9M  |
| Revisions that address review findings | ≈7.0M  |
| Post-merge review and its follow-ups   | ≈4.3M  |
| Pre-merge review batches               | ≈4.1M  |
| CI-fix, conflict and unstick revisions | ≈3.2M  |
| Review guides                          | ≈1.5M  |
| Coordinator share                      | ≈1.2M  |
| Progression                            | ≈0.7M  |

The run that writes the change is about a quarter of what the PR costs. The other three quarters is the pipeline around it.

### What drives the total

Tokens are close to `runs × model calls per run × context per call`, because every call re-sends the whole conversation.

- **Calls.** Claude workers made 30.9k tool calls and Codex 31.1k. Using tool calls as a stand-in for API calls, the average context per call was ≈141k tokens for Claude (4,361M / 30.9k) and ≈95k for Codex (2,964M / 31.1k). _est._; the `rounds` column in `work_runs` holds the real call count.
- **Starting context.** A Claude worker starts at 59–72k tokens and a Codex worker at 28–36k. Re-reading just that starting context on every call is ≈1.9B for Claude (62k × 30.9k, 44% of Claude tokens) and ≈1.0B for Codex (31k × 31.1k, 33%). _est._
- **Rule of thumb.** Each 1,000 tokens in every worker's starting context costs ≈31M tokens a week per driver.
- **Reads.** Shell reads and searches are 55% of Claude tool calls (16,984) and 37% of Codex tool calls (11,492). Each one is a model call that re-reads the context so far.

### Token counts are not cost

Cache reads dominate the token count but are the cheapest token class. The table prices the Claude worker totals at Anthropic's list prices (Opus 5.5 $4 input / $20 output / $0.20 cache read per million; Sonnet 5.5 $2 / $10 / $0.10; Fable 5.2 $8 / $40 / $0.20; cache writes at 1.25× input for the 5-minute lifetime and 2× for the 1-hour lifetime). _est._; it assumes the `opus`, `sonnet` and `fable` aliases resolved to those models all week.

| Claude token class | Share of tokens | Share of list price, 5-minute writes | Share of list price, 1-hour writes |
| ------------------ | --------------- | ------------------------------------ | ---------------------------------- |
| Cache reads        | 96.0% (4,187M)  | 36%                                  | 30%                                |
| Cache writes       | 3.4% (147M)     | 36%                                  | 47%                                |
| Output             | 0.6% (26.6M)    | 28%                                  | 23%                                |

Claude Code uses the 1-hour lifetime for the main conversation on a subscription, so the right-hand column is the likely one. On that basis the week's Claude worker usage is ≈$2,355 at list price: Opus ≈$1,180 (50%), Fable ≈$735 (31%), Sonnet ≈$440 (19%).

A subscription quota is not billed at list price, and how it weighs cache reads is not published. Anthropic's documentation says only that a long session "still draws usage for the whole conversation" on each request. Until that weighting is measured (see [Instrumentation gaps](#instrumentation-gaps)), each recommendation below says whether it looks stronger or weaker when priced this way. Three consequences hold either way:

- **Starting a run is expensive.** Every run writes its starting context to the cache once. About 58M of the 147M tokens Claude wrote to cache (39%) were the first call of a run, and ≈44M of those came from review runs (1,368 Claude review runs × 32k). At list price, writing the starting prompt of review runs is ≈12% of all Claude worker spend. All three figures are _est._
- **Polling and long contexts are cheaper than their token counts suggest**, because they are cache reads.
- **Output is 0.6% of tokens but about a quarter of list price.** Output is governed mostly by the effort level.

### The mix has already changed

The dispatch split at the end of the window was Grok 55%, Claude 45%, Codex 0%. Codex ran 2.12B of non-review work during the window (astra 901M across 105 runs, sol 1,218M across 209). That work now goes to Grok and Claude. Codex keeps only the roles pinned to it: reviewers (369M) and review guides (476M).

Grok's estimated input per run (1,908M / 277 ≈ 6.9M) is higher than a Sonnet non-review run (1,238M / 273 ≈ 4.5M), even though Grok starts from a 13.7k context, and 81 of 264 Grok runs compacted. The driver now taking most new work is the one Boss cannot measure.

## What the code does today

These are the behaviours on `main` that explain the measurements. They are referred to below as C1–C12.

- **C1. Claude workers inherit a personal Claude Code environment.** The launch line is `claude --model <m> [--effort <e>] --disallowedTools=AskUserQuestion`, a permission flag, and `--settings <file>` (`engine/driver/src/claude.rs:808-845`). The settings file holds only permissions and hooks. Nothing limits skills, plugins, MCP servers, user-level `CLAUDE.md` or built-in tools. Codex and Grok workers get a per-run home instead (`engine/driver/src/codex.rs:1989`, `engine/driver/src/grok/environment.rs:115`). A Claude worker session inspected for this investigation was offered 32 skills (charts, slide decks, spreadsheets, browser automation and similar), tools with no use in a worker (`Artifact`, `Workflow`, `ScheduleWakeup`, `SendFeedback`, `ReportFindings`), and connectors from a personal account (documents, mail, calendar, drive).
- **C2. No auto-compact window is set for any driver.** Claude Code compacts models with a 1M window "at about 967K tokens by default", which is why 0 of 1,700 Claude runs compacted.
- **C3. Every finished run that pushes triggers a full review batch.** That includes CI-fix, conflict and review-finding revisions (`engine/completion/src/pr_transition.rs:279-282`, `BOSS_ENABLE_REVISION_TRIGGERED_REVIEWS`, default on). A pure rebase with an unchanged diff is skipped. The minimum changed-line threshold exists but is 0 (`BOSS_MIN_REVIEW_CHANGED_LINES`). The cap is 3 rounds per PR (`BOSS_MAX_REVIEW_CYCLES`).
- **C4. A repeat review is a whole-PR review by both reviewers and the supervisor.** The prompt says to "prioritise the delta" and then "do not scope your review to a diff-only view" (`engine/pr-review/src/render.rs:520-547`).
- **C5. A revision is minted when any finding is critical or high, or when any finding at any severity is in one of four categories** (regression, duplication, deferred scope, agent-isms) (`engine/pr-review/src/parsing.rs:355-368`). Once the gate passes, every finding goes into the revision, including low ones.
- **C6. The "Deep" review profile is easy to reach.** A PR is Deep above 1,000 changed lines or 25 files, with 4 or more subsystem buckets, 3 or more languages, or 2 or more complexity flags (`engine/pr-review/src/parsing.rs:31-80`). The flags are matched on words in the paths of changed non-test, non-doc files (`parsing.rs:162`): any `BUILD.bazel`, `Cargo.toml` or `Cargo.lock` raises one, and a path containing `schema`, `migration`, `scheduler`, `process`, `lifecycle`, `permission`, `sandbox` or `auth` raises another. Deep selects Opus for the Claude reviewer and makes the PR eligible for post-merge review (`engine/merge-poller/src/post_merge_review.rs:39-58`). In the window, 397 of 622 Claude reviewer runs (64%) ran on Opus.
- **C7. The post-merge reviewer runs at effort `large`, which maps to Claude `xhigh`** (`engine/work-db/src/review_batches.rs:489-517`, `engine/driver/src/claude.rs:39-47`; the code comment says `high`). Its prompt carries no diff. Findings that pass the same gate as C5 become a follow-up task with a ready execution, with no human approval (`engine/work-db/src/review_verdict_apply.rs:144-149`). Those follow-up PRs are reviewed like any other, including post-merge review if they classify as Deep. No guard stops that recursion.
- **C8. Review guides are pinned to Codex astra at `high` effort** (`engine/execution-runner/src/runner/worker_spawn.rs:473-503`). Nothing in the review pipeline reads a guide. Its only consumer is a person looking at the PR in the app.
- **C9. Workers are told to poll.** The Bazel gate says to run long commands in the foreground and "keep polling the running command" (`engine/execution-runner/src/runner/prompt.rs:641-646`, `:709`). The Codex preamble shows a `write_stdin` loop with a 300-second yield (`engine/driver/src/codex.rs:420-445`).
- **C10. Grok usage is discarded, prices are stale and quota history is not kept.** The Grok transcript parser recognises `turn_completed` but keeps only `stop_reason` (`engine/driver/src/grok/transcript.rs:205-207`), and `engine/core/src/run_cost.rs` has no Grok branch. The price table has Opus at $15 / $75 and no entry for Fable, GPT or Grok (`engine/core/src/cost_pricing.rs:31-59`). Provider quota snapshots are held in memory for 15 minutes and dispatch never reads them (`engine/driver-quota/src/cache.rs`).
- **C11. The coordinator is rotated only by events.** It launches as `claude --model opus --permission-mode auto` with `crossSessionInbound` set to `accept` (`engine/core/src/coordinator_tmux/inbox.rs:89-100`). A new session starts only when the process exits or is updated, the tmux session is lost, the restart supervisor recreates it, or it is reset by hand (`docs/coordinator-session-handoff.md`). There is no size threshold.
- **C12. Investigation reasoning selects Fable, and several engine paths set or inherit it.** Any row with investigation reasoning, or of design kind, gets the driver's design/investigation model, which is `fable` for Claude (`engine/effort/src/lib.rs:451-454`, `:603-607`, `engine/driver/src/claude.rs:160`). A comment in the same file still says Fable is reachable only through an explicit per-row override. Revisions inherit the chain root's reasoning, and a CI-fix revision with no failure evidence is pinned to investigation (`engine/merge-poller/src/ci_watch.rs:957-971`). Effort `large` maps to `xhigh`.

## Recommendations

Ranked by estimated weekly saving at the measured mix. "Weighted" says how the saving looks at list price instead of raw tokens.

| #   | Change                                                                 | Saving _est._                       | Share | Weighted | Effort | Quality risk  |
| --- | ---------------------------------------------------------------------- | ----------------------------------- | ----- | -------- | ------ | ------------- |
| R1  | Gate and slim post-merge review                                        | 0.7–1.2B                            | 7–12% | same     | medium | medium        |
| R2  | Make repeat reviews a delta check; raise the bar for minting revisions | 0.5–1.0B                            | 5–10% | same     | medium | medium        |
| R3  | Trim every worker's starting context                                   | 0.35–0.8B                           | 4–8%  | same     | small  | low           |
| R4  | Replace polling with blocking waits                                    | 0.5–0.6B at the measured mix        | 5–6%  | weaker   | small  | low           |
| R5  | Set an auto-compact window; alarm on runaway runs                      | ≈0.4–0.5B                           | 4–5%  | weaker   | small  | low to medium |
| R6  | Generate a review guide once per PR, on a cheaper path                 | 0.27–0.43B                          | 3–4%  | same     | small  | low           |
| R7  | Reading discipline: ranged reads, batched search, exploration subagent | 0.2B firm, 0.3–0.6B experimental    | 2–8%  | stronger | medium | medium        |
| R8  | Coordinator: lower compact window, rotate on size                      | 0.13–0.19B                          | 1–2%  | weaker   | small  | low to medium |
| R9  | Model and effort selection                                             | 0.1–0.3B, plus relief on Opus/Fable | 1–3%  | stronger | small  | medium        |
| R10 | Failed, orphaned and overlapping runs                                  | ≤0.15B                              | ≤1.5% | same     | medium | none          |

### R1. Gate and slim post-merge review

**Evidence.** The post-merge reviewer used 942M over 147 runs: 56% of all Opus worker tokens, at a median of 5.58M per run against 1.27M for a pre-merge Opus review. Its follow-ups cost another 202M measured plus ≈262M Grok _est._ There were about 120 follow-up executions for 144 post-merge reviews (84 measured, ≈38 Grok at 6.9M each), close to one per review. 502 review executions are attached to follow-up items. The chain is 1.4B directly (15%), more with the reviews of its follow-ups. C6 and C7 explain the size: 64% of Claude reviewer runs were at the Deep tier, the run is at the highest effort Boss uses for review, it gets no diff, and its follow-ups can be reviewed post-merge again.

**Change.**

1. Exclude follow-ups created by a post-merge review from post-merge review.
2. Run the post-merge reviewer at `medium` effort (Claude `high`, the same as pre-merge reviewers) and embed the merged diff, with the same line limit and lockfile exclusion as pre-merge prompts.
3. Make eligibility depend on size (over 1,000 lines or 25 files), or on two flags plus a size floor, instead of on path words alone.
4. Require critical or high severity before a post-merge finding becomes a task. A post-merge finding costs a whole new PR, not a revision.

**Estimated saving.** Step 2: if a run falls to twice a pre-merge Deep review (2.5M), 942M × (1 − 2.5/5.58) ≈ 520M. Step 3: if eligibility halves, half of the remaining 422M and half of the 464M of follow-ups ≈ 440M. Step 1: 6.4M per avoided review (942M / 147); the count needs one query. Step 4 is not estimated. Total 0.7–1.2B.

**Effort.** Steps 1, 2 and 4 are small. Step 3 changes the classifier and its tests.

**Risk and quality.** Post-merge review is the only stage with a single reviewer, and it exists to catch what pre-merge review missed. Fewer runs means fewer second looks at mid-sized PRs. The near one-to-one ratio of reviews to follow-ups suggests either that pre-merge review leaks a lot or that a reviewer at maximum effort always finds something. Published studies of LLM review point to the second (see the survey), but the data to decide is in Boss's own tables and has not been pulled.

**Validate.** First pull yield from the existing tables: per post-merge review, the findings that passed the gate, by severity and category, and whether the follow-up merged. Then split eligible PRs by hash for a week and compare follow-ups filed, later fixes to the same files, and tokens per merged PR.

### R2. Make repeat reviews a delta check and raise the bar for minting revisions

**Evidence.** There were 604 pre-merge batches on 290 PRs, so 314 batches (52%) were repeat reviews, at a median 2.1M per batch (≈660M). 118 PRs (41%) had three or more batches, which is the cap (C3). Revisions that address findings are the largest line in the audit once Grok is included: 368 measured executions for 1,387M (mean 3.8M) plus ≈867M Grok _est._, 2.25B in all (23%). C4 and C5 explain both numbers: a repeat review repeats the full batch, and a single low-severity finding in one of four categories starts another revision and another batch, about 5.9M per round (3.8M + 2.1M).

**Change.**

1. From the second round on, run one reviewer at the Light tier with no supervisor. Give it the prior findings, the diff since the last reviewed commit and the changed-file list. Ask it to mark each prior finding resolved or not and to review only the delta. Keep the whole-PR duplication check that motivated C4 as one explicit step over the changed-file list.
2. Set `BOSS_MIN_REVIEW_CHANGED_LINES` above 0 so that small CI-fix and conflict pushes do not trigger a review.
3. Require at least medium severity for the four always-on categories to mint a revision. Post lower findings as PR comments.
4. Once yield per round is known, consider a cap of 2 rounds.

**Estimated saving.** Step 1: 314 × (2.1M − 0.6M) ≈ 470M, using the Sonnet reviewer median of 0.61M. Step 3: each 10% of the ≈494 finding revisions (368 measured plus ≈126 Grok _est._) that would no longer be minted saves 49 × 5.9M ≈ 290M. The fraction is not measured. Total 0.5–1.0B.

**Effort.** Step 1 is a new prompt and a batch shape with one member. Steps 2 and 3 are a setting and a predicate.

**Risk and quality.** Fix commits lose the second model's corroboration. The reason for the current wording was a real incident (a module left duplicated after an extraction), so step 1 keeps that check explicitly. Evidence on the other side, both as summarised in the survey: Anthropic's own managed review focuses later runs on "what changed since the last review", and one 2026 study reports LLM review accuracy falling from F1 0.657 on diffs under 10 lines to 0.043 over 150 lines, which favours small delta reviews.

**Validate.** Split PRs by hash. Compare batches per PR, finding revisions per PR, defects found later (post-merge findings while R1 is unchanged, reverts, follow-ups) and tokens per merged PR. Before step 3, count how many past revisions were minted only by low-severity findings in the four categories.

### R3. Trim every worker's starting context

**Evidence.** Re-reading the starting context is ≈2.9B a week across Claude and Codex (see "What drives the total"). Measured components of a Claude worker's start: Boss worker rules 23.7k characters, mono `CLAUDE.md` 22.2k, user-level `CLAUDE.md` 5.7k, skill listing 20.0k, plus a 33–52k brief. In the checkout used here the three instruction files measure 23,757, 22,225 and 5,749 bytes. The itemised components add up to about 30k of the 66k first-call tokens. The rest is tool definitions and harness text that the audit did not itemise.

Three kinds of waste are visible:

- **Content no worker needs** (C1): the skill listing, the user-level `CLAUDE.md`, connectors and unused tools. The user-level file also contradicts the worker rules. It tells an agent to lease and release a workspace on every prompt, and the worker rules forbid touching leases.
- **The same rule stated several times.** The pre-push rebase block (1,703 characters) is in the worker rules and again in the brief. The file-count split rule is stated three times: 4,577 characters in mono `CLAUDE.md`, 1,331 in the worker rules and about 2.4k in the brief. The no-`timeout` rule, the check-bypass rule, the PR body-file recipe and the empty-diff rule each appear two or three times.
- **Text sent to runs that cannot use it.** An investigation that may touch only one markdown file receives the file-count split protocol. The "Declaring your run finished" section is 9.7k characters in every brief. The list of operational docs in mono `CLAUDE.md` is 4.0k characters.

**Change.**

1. Launch Claude workers without the personal environment: no skills (`--disable-slash-commands`), no inherited MCP servers (`--strict-mcp-config`), unused built-in tools removed by name (`--disallowedTools` already carries one), and the user-level `CLAUDE.md` excluded. Check whether `--setting-sources project,local` excludes that file; if it does not, give Claude workers a scoped configuration directory as the other two drivers have. Confirm with `/context` in a worker-configured session what remains.
2. State each rule once. Keep the long form in the worker rules and a one-line pointer in the brief.
3. Send kind-specific text only to the kinds that need it.
4. Move the file-count protocol and the operational-docs list out of mono `CLAUDE.md` into files that are read on demand.

**Estimated saving.** Step 1, measured part: 25.7k characters ≈ 6.4k tokens × 30.9k calls ≈ 200M, plus 11M fewer cache-write tokens (6.4k × 1,700 runs). Step 1, unmeasured part: if 12k tokens of tool definitions are removable, 12k × 30.9k ≈ 370M. Steps 2–4: about 19k characters ≈ 4.8k tokens, of which ≈2k is in mono `CLAUDE.md` and reaches every Claude and Codex call (2k × 62k ≈ 125M) and ≈2.75k reaches non-review calls (2.75k × 35k ≈ 95M). Total 0.35–0.8B.

Embedded material in review briefs is small by comparison. The gap between the median and p90 review start is 27k tokens over 6–9 calls, under 100M a week in total. The 906k-character lockfile diff sent for mono#3594 was a one-off, but the embed path has no lockfile filter, and its 500-line cap should have excluded that diff, so how it got in is worth a trace.

**Effort.** Small. The launch flags and prompt edits are local to the Claude driver and the prompt composer.

**Risk and quality.** Low. Removing contradictory and duplicated instructions should help, not hurt. Two things to check: hooks must still fire with the new flags, and a worker that used a removed tool must have an alternative. `--bare` and `--safe-mode` are not suitable, because both skip the hooks or `CLAUDE.md` that Boss relies on.

**Validate.** Compare first-call context and first-call cache-write tokens before and after, per run type, from the same transcript fields the audit used. Watch failed-to-start and blocked rates for a week.

### R4. Replace polling with blocking waits

**Evidence.** Polling and wait turns cost ≈860M _est._: Codex `write_stdin` polls 570M (4,705 calls), Codex `wait` and `sleep` 103M, and Claude monitors, sleep loops, wait commands and CI checks 189M. Codex polling is also the likely reason Codex work runs cost more than Claude ones: the 314 non-review Codex runs averaged 6.7M, and 4.6M if all the polling was theirs, which equals a Sonnet run. C9 shows this is instructed behaviour.

**Change.**

1. Claude: for a command expected to outlast the 10-minute foreground limit, start it in the background, declare the wait and end the turn. Claude Code re-invokes the session when a background command exits, so no poll is needed. Verify that wake-up in a worker pane before changing the brief.
2. Claude: deny CI watch loops (`gh pr checks --watch`, `gh run watch`) in the existing PreToolUse hook. The worker rules already forbid watching CI, and `gh pr checks` and `gh run` were still called 204 times.
3. Codex: raise `background_terminal_max_timeout` in the per-run `config.toml` (the documented default is 300,000 ms, the value the preamble already asks for) and raise the yield in the preamble to match.
4. Record the yield and the wall-clock gap of every poll, so the remaining polls can be explained.

**Estimated saving.** Claude: about 60% of 189M ≈ 110M. Codex: about 70% of 673M ≈ 470M, but only if Codex implementation work returns; at the current split most of it has already gone. Grok polling is unknown.

**Effort.** Small.

**Risk and quality.** Low. A wait that never wakes would stall a run, which the stale-worker sweep already catches. No gate is weakened: the same commands run to the same exit codes.

**Validate.** Count wait and poll calls per run before and after, and compare time to PR.

**Weighted.** Weaker. Polls are cache reads with almost no output.

### R5. Set an auto-compact window and alarm on runaway runs

**Evidence.** No Claude worker compacted (C2). Ten Fable runs used 791M, 65% of all Fable tokens, with peak contexts of 495–781k. The three with known call counts averaged 477–529k of context per call (136.5M / 258, 121.4M / 230, 89.7M / 188). Anthropic and Chroma both report that answer quality falls as context grows, so the extra context is unlikely to be buying quality.

**Change.**

1. Launch Claude workers with a 200k auto-compact window (`--autocompact 200k` or `CLAUDE_CODE_AUTO_COMPACT_WINDOW=200000`; the minimum is 100k). The median long run ends at 161k, so most runs are untouched.
2. Add compact instructions to the worker rules so a summary keeps the execution context, the task description, the open checklist and the done-declaration command verbatim.
3. Raise an attention item when a run passes a token or call budget, for example 25M tokens or 150 calls. Run cost is already tailed live.

**Estimated saving.** A 200k window holds the average context near 140k. For the three known runs, 676 calls × 1.15 (extra calls to re-read) × 140k ≈ 109M against 348M, a 69% cut. Applying 50–65% to the ten runs gives ≈0.4–0.5B.

**Effort.** Small.

**Risk and quality.** A summary can drop a constraint. Step 2 and the existing nudges cover the protocol parts. Compacting a large context is itself a large request, so the window should not be set so low that runs compact repeatedly.

**Validate.** Compare peak context, calls per run and tokens per run for Fable runs before and after. Read ten compacted runs to check that nothing required was lost.

**Weighted.** Weaker (≈12% of Fable's list-price spend), because the saving is cache reads. There is no price step at 200k: the 1M window is billed at standard rates.

### R6. Generate a review guide once per PR, on a cheaper path

**Evidence.** 675 guide runs cost 476M, about 2.3 per PR at a median 0.64M. The guide is for a human reader only (C8), and it is regenerated as the PR's head moves.

**Change.** Generate the guide when the PR is ready for a person (review rounds settled and CI green) or when someone first opens the PR in the app. Drop effort from `high` to `medium`. Longer term, produce it with one direct call through the existing utility-model path instead of an agent session.

**Estimated saving.** One guide per PR: (675 − 290) / 675 = 57%, ≈270M. A single-call guide of 20–50k tokens instead of 640k: up to ≈430M.

**Effort.** Small for the trigger and effort. Medium for the single-call path.

**Risk and quality.** A guide could be missing or stale when someone opens a PR early. On-demand generation covers that at the cost of a short wait. A single-call guide cannot explore the repository, which matters less for a reading aid than for a review.

**Validate.** Guide runs per PR, and whether guides are opened at all. The app can record opens.

### R7. Reading discipline

**Evidence.** File reads and searches are about 80% of the tool-result tokens carried in context: Claude shell reads 530M and `Read` 305M, Codex shell reads 821M (_est._). A `Read` call carries ≈237k tokens over its lifetime (305M / 1,287) against ≈31k for a shell read, because it returns whole files. Reads are also most of the calls: 55% of Claude tool calls.

**Change.**

1. In the existing PreToolUse hook, refuse a `Read` of a large file without `offset` and `limit`, and say to locate with `rg -n` first. Spotify's published setup uses a 350-line threshold. Lower `CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS` (default 25,000) as a backstop.
2. Tell workers to issue independent searches in one turn, and measure calls per run.
3. Experiment: route "where does X live" questions to a subagent on a cheap model that returns a summary. Claude Code's built-in Explore agent now runs on the parent's model, so this needs an agent defined with `model: haiku` or `CLAUDE_CODE_SUBAGENT_MODEL`.
4. Experiment: an on-demand crate map file. It must not go into the fixed context. By the rule of thumb, 2.5k tokens there costs ≈155M a week, which is about 2.4 avoided calls of ≈100k in each of the ≈650 Claude and Codex implementation and revision runs.

**Estimated saving.** Step 1: 60% of 305M ≈ 180M. Steps 2 and 3: if a quarter of Claude's read calls disappear from the main loop, 4.2k calls × ≈100k ≈ 420M. Steps 2–4 are experiments: 0.3–0.6B.

**Effort.** Small for step 1. Medium for the rest.

**Risk and quality.** Spotify reports about 90% fewer main-context tokens on bulk reads, and also that the cheap reader "missed a subtle thread-safety bug", that summaries lack reliable line numbers so the main agent re-reads before editing, and that "you can't delegate reasoning". So this is for locating code in implementation runs, not for reviewers, who must read the diff themselves. A subagent's own calls still draw usage; the gain is the cheaper model and the smaller main context.

**Validate.** Split implementation runs by hash. Compare calls per run, tokens per run, review findings per PR and CI-fix revisions per PR.

**Weighted.** Stronger. A tool result is written to cache once at up to 2× the input price, which at current Opus and Sonnet prices equals about 40 later reads. Smaller results cut the most expensive token class.

### R8. Coordinator

**Evidence.** The coordinator session used 310M over 1,719 calls (average 180k, p90 291k, maximum 366k) with 8 compactions, plus 90M in subagents. That is 4% of the total, and the main session is 15% of all Opus tokens (310M of 2,001M). It has no size-based rotation (C11). It accepts cross-session messages, each of which Claude Code delivers as a new turn "sending your full context each time".

**Change.**

1. Launch with a 150k auto-compact window. The handoff already re-delivers its brief after compaction.
2. Rotate to a fresh session through the existing handoff when the context passes a threshold, instead of only on events.
3. Measure how many of the 1,719 calls were inbound messages, and batch them if they dominate.

**Estimated saving.** Holding the average context near 100k: 1,719 × 80k ≈ 138M, less a few million for extra compactions. With fewer turns from batching, up to ≈190M.

**Effort.** Small.

**Risk and quality.** A coordinator that forgets earlier decisions is worse than an expensive one. The handoff document exists for exactly this and should be checked against a week of real sessions before the window is lowered further. Moving the coordinator to Sonnet (`BOSS_COORDINATOR_MODEL`) would take another ≈170M off Opus but is a judgement call on coordinator quality that this data cannot settle.

**Validate.** Context per call and compactions per day before and after, and a manual check of handoff fidelity.

### R9. Model and effort selection

mono#3746 already moves progression runs and the unstick revisions they create to the standard tier. That covers the 128M that 11 unstick revisions spent on Fable (≈100M saved at the non-Fable average of 2.25M per run) and is not repeated here. One thing to check against it: the audit shows progression-spawned chores running on Fable (169M on 11-18), although on `main` those chores are created without a reasoning mode.

**Evidence and change.**

- **Effort ladder.** `medium` maps to `high` and `large` to `xhigh` (C12). Claude Code's default for Fable 5.2, Opus 5.5 and Sonnet 5.5 is `medium`, and Anthropic's API guidance for Fable 5.2 is to start at `high` and use `xhigh` only where a gain is measured. The 37 investigation runs at `large` used 935M. Try `high` for them.
- **CI-fix without evidence.** These revisions are pinned to investigation reasoning and therefore Fable (C12). Fetch the failing log first, or use the standard tier at high effort. The whole CI-fix line is 138M measured, so this is bounded.
- **Supervisor.** It runs on Opus for Deep batches (376 runs, 83M) to merge two finding lists. Run it on Sonnet always.
- **Sol for revisions.** A finding revision cost 6.4M on gpt-6.1-sol (864M / 135) against 2.2M on Claude (523M / 233). If Codex is put back into the split before R4 lands, keep engine-minted revisions off it: 135 × 4.2M ≈ 570M at the measured volume.
- **Investigations.** Keep Fable for `large` investigations and send small and medium ones to Opus. 26 such runs used 271M, part of which mono#3746 already covers.

**Estimated saving.** 0.1–0.3B in tokens. The larger effect is relief on model-specific limits: Claude Code documents separate Opus and Sonnet limits alongside the weekly limit shared by all models.

**Effort.** Small: table and constant changes.

**Risk and quality.** Lower effort and cheaper models can miss more. One 2026 study found a cheaper model matching a larger one on review at 3.2× lower cost, with low absolute accuracy for both, so reviewer tier is worth testing and not assuming. R1 and R2 already reduce Opus use the most. Do not also move the first-round Deep reviewer off Opus until their effect is known.

**Validate.** Per change, split by hash and compare outcome rates (delivered, blocked), findings per PR and tokens per run.

### R10. Failed, orphaned and overlapping runs

**Evidence.** Failed, orphaned, cancelled and abandoned executions used ≈334M (4.5% of measured). 56 of 912 non-review items ran more than once, with 73 extra executions. Three implementation executions overlapped a live one on the same item and used 81M, although a double-spawn guard exists (`engine/dispatch/src/coordinator/execution.rs:586-756`). Runs that ended blocked used 205M, 3.3M each, so most were blocked late.

**Change.** Break the 334M down by failure reason before fixing anything. Trace the three overlaps to the path that got past the guard. For orphaned runs, check whether the next attempt starts from the preserved workspace and a summary of the lost run or from nothing.

**Estimated saving.** Unknown until the breakdown exists. If a third is avoidable, ≈0.1–0.15B.

**Validate.** Weekly tokens by end status and reason.

### Not ranked: cache lifetime for short runs

This one depends on what the quotas count, so it has no estimate in tokens. Claude Code gives a subscription's main conversation the 1-hour cache lifetime, which doubles the price of every cache write. A reviewer or supervisor finishes in a median 1.7 minutes and never needs an hour. If the quota weighs writes as the price list does, launching review roles with `CLAUDE_CODE_PROMPT_CACHE_TTL=5m` cuts their write cost by 37.5% (1.25× instead of 2×) with no change in behaviour. Implementation runs that wait on long builds should keep the 1-hour lifetime. Try it once instrumentation gap 1 is closed.

## Quick wins and structural changes

**Quick wins.** Each is a setting, a constant, a launch flag or a prompt edit, with no schema or protocol change. Together ≈1.3–1.5B _est._

- R3 steps 1 and 2: launch flags and duplicate removal.
- R5 step 1: auto-compact window for workers. R8 step 1: the same for the coordinator.
- R1 steps 1 and 2: recursion guard and post-merge effort.
- R2 step 2: `BOSS_MIN_REVIEW_CHANGED_LINES`.
- R4 steps 2 and 3: deny CI watching; raise the Codex poll window.
- R9: supervisor on Sonnet; `large` to `high` on Fable as a trial.
- Instrumentation gaps 1–3 below.

**Structural changes.** These change a pipeline shape or add a mechanism and need an A/B.

- R2 step 1: delta re-review as a batch shape.
- R1 step 3: the Deep classifier and post-merge eligibility.
- R6: guide on demand, then as a single call.
- R7 steps 2–4: batched search, exploration subagent, crate map.
- R8 step 2: size-based coordinator rotation.
- Quota-aware dispatch: read the quota snapshots Boss already fetches and defer optional stages (post-merge review, guides) when a model family is close to its weekly limit.

## Task shape

- **A PR has a pipeline cost that is mostly independent of its size.** About 21M of the ≈30M average is pipeline. Reviewers start at 28–59k and make 6–9 calls whatever the diff. Whether two small tasks in one PR cost less than two PRs depends on how pipeline tokens grow with diff size. That can be measured from data Boss already has: per-PR tokens joined to the additions, deletions and file counts stored with each review batch. This should be done before changing how the planner sizes tasks.
- **Follow-ups are the clearest case of work cut too small.** Each post-merge finding becomes its own task, PR and review rounds. Batching a day's follow-ups per area into one PR would pay the pipeline once.
- **Splitting has a price.** A change split to satisfy the file-count limit pays one more pipeline, ≈21M on average. This is not an argument to relax the check. It is a number to have when choosing between a split and a one-PR exception.
- **The largest runs are too large.** Ten runs used 791M. R5 bounds their context. The better fix for a 250-call run is to break it at a phase boundary with a written handoff, which the preserved workspace already supports.

## Instrumentation gaps

1. **What the quotas count.** Persist the provider quota snapshots that `driver-quota` already fetches (they are in memory only, C10), and regress the weekly percentage against cache reads, cache writes and output per model. Until then nobody knows whether cutting cache reads helps the limit. Also run `/usage` in Claude Code on the Boss host: on a subscription it shows what counted against plan limits over 24 hours or 7 days, attributed to skills, subagents, plugins and MCP servers, with recommendations.
2. **Grok.** A July capture of a Grok session's update stream shows a `turn_completed` record with `inputTokens`, `outputTokens`, `cachedReadTokens`, `reasoningTokens`, `modelCalls` and `costUsdTicks` per turn (`docs/investigations/ghostty-grok-pane-viability-artifacts/ghosttykit_host/evidence/esc_interrupt/session_telemetry_excerpt.md`). The audit found only `_meta.totalTokens` in current sessions. First check a current `updates.jsonl` for that record under the `_x.ai/session/update` method. If it is there, sum it into `work_runs` (the parser already matches the record and drops the usage, C10). If it is gone, store the context-size sum as an explicitly estimated value and reconcile it weekly against the billing endpoint that `driver-quota` already calls.
3. **Prices.** Update `cost_pricing.rs` and add Fable, GPT and Grok, so `boss cost --usd` gives a weighted view. Split cache writes by lifetime; the 5-minute and 1-hour counts are already parsed.
4. **Tokens per merged PR.** Make it a first-class roll-up across every execution on a PR, raw and weighted. It is the metric every A/B above needs.
5. **Review yield.** Per review round and role: findings by severity and category, which ones passed the gate, and whether the resulting revision or follow-up changed code and merged. R1 and R2 are quality trade-offs that cannot be judged without it.
6. **Why PRs are Deep.** A histogram of which Deep criterion fired, from the classification snapshot stored with each batch.
7. **Calls and context by category.** Surface `rounds` and peak context per run type, so "calls per run" stops being approximated by tool calls.
8. **Waits and polls.** Count them per run with their tokens, and record the requested yield for Codex.
9. **New tokens per tool class.** The audit estimated carried tokens. The size of each result when first written is what the weighted view needs.
10. **Fixed context.** Run `/context` in a worker-configured session to itemise the ≈36k starting tokens the audit did not break down.
11. **Waste by reason.** Tokens by end status and failure reason, weekly.
12. **Documentation.** `docs/forensic-surfaces.md` still says per-task cost is not in `state.db`. It is now.

## Research survey

Entries marked **(read)** were fetched and quoted for this document. Entries marked **(summarised)** were read through an extraction pass and should be re-checked before a number is relied on.

**Routing reads to a cheap model**

- [Portal by Spotify cut my Claude Code token usage by 90%](https://engineering.atspotify.com/2026/9/portal-by-spotify-cut-my-claude-code-token-usage-by-90/), Spotify Engineering, 2026-09-03 **(read)**. Bulk reads go to a Gemini 2.5 Flash worker that returns a condensed analysis. Two PreToolUse hooks enforce it: one blocks `Read` of files over a line threshold (default 350), one blocks `cat`, `head` and `tail` on large files; reads with `offset` and `limit` pass. "Mean bulk-read savings were around a whopping 90%." The figure covers bulk reads only, not session cost. Caveats in the post: "You can't delegate reasoning"; "The worker model found surface-level patterns but missed a subtle thread-safety bug in my testing"; summaries "don't include reliable line numbers"; responses take 10–30 seconds. The post's own lesson is that advisory rules were ignored until hooks enforced them.
- [Spotify's Portal routes Claude Code file reads to a cheap model](https://agentrex.ai/blog/spotify-portal-cheap-model-read-routing/), AgentRex, 2026-09-08 **(summarised)**. Secondary coverage of the same work.

**Prompt caching**

- [Prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching), Anthropic **(read)**. Cache writes cost 1.25× input for 5 minutes and 2× for 1 hour; a read refreshes the entry for free; the prefix order is tools, system, messages; changing tool definitions invalidates the whole cache.
- [Pricing](https://platform.claude.com/docs/en/about-claude/pricing), Anthropic **(read)**. Source of the prices used in the weighted view. Cache reads are 0.05× input on Opus 5.5 and Sonnet 5.5 and 0.025× on Fable 5.2, below the 0.1× that older models use.
- [Rate limits](https://platform.claude.com/docs/en/api/rate-limits), Anthropic **(read)**. On the API, cache reads do not count toward input-tokens-per-minute limits; cache writes do. This is about API rate limits, not subscription limits.
- [How Claude Code uses prompt caching](https://code.claude.com/docs/en/prompt-caching), Anthropic **(read)**. The main conversation gets the 1-hour lifetime on a subscription within plan usage, and subagents 5 minutes; `CLAUDE_CODE_PROMPT_CACHE_TTL` or `promptCacheTtl` overrides it. "In Claude Code, the cache is effectively scoped to one machine and directory", so workers in different workspaces do not share a prefix beyond the tool definitions. Switching model mid-session rebuilds the whole cache.
- [Manage costs effectively](https://code.claude.com/docs/en/costs), Anthropic **(read)**. Source of the `/usage` plan breakdown, the advice to keep `CLAUDE.md` under 200 lines, hook-based output trimming, and the list of things that send full context while idle (scheduled tasks, cross-session messages). Notes separate limits per model family.
- [Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching), OpenAI **(summarised)**. Automatic prefix caching above 1,024 tokens, cached input discounted, entries kept for 5–10 minutes of inactivity and up to an hour.
- [Prompt caching](https://docs.x.ai/developers/advanced-api-usage/prompt-caching), xAI **(summarised)**. Automatic caching; cached tokens are reported in `usage.prompt_tokens_details.cached_tokens`.

**Context management**

- [Model configuration](https://code.claude.com/docs/en/model-config), Anthropic **(read)**. "Models running with a native 1M window compact before the window fills, at about 967K tokens by default." The window is set with `--autocompact`, `autoCompactWindow` or `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, minimum 100k. "The 1M context window uses standard model pricing with no premium for tokens beyond 200K, except on Haiku 5.5." Default effort is `medium` on Fable 5.2, Opus 5.5 and Sonnet 5.5.
- [CLI reference](https://code.claude.com/docs/en/cli-reference), Anthropic **(read)**. `--disable-slash-commands` ("Disable all skills and commands for this session"), `--strict-mcp-config`, `--disallowedTools` (a bare name "removes the matching tools from Claude's context"), `--tools`, `--setting-sources`, `--exclude-dynamic-system-prompt-sections`, `--bare`.
- [Context editing](https://platform.claude.com/docs/en/build-with-claude/context-editing) and [Managing context](https://claude.com/blog/context-management), Anthropic **(summarised)**. Clearing old tool results cut tokens by 84% in a 100-turn evaluation. This is an API feature, not a Claude Code setting.
- [Effective context engineering for AI agents](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents), Anthropic, 2025-09-29 **(summarised)**. Aim for the "smallest possible set of high-signal tokens"; subagents should return a summary of 1,000–2,000 tokens.
- [Context Rot](https://www.trychroma.com/research/context-rot), Chroma, 2025-07-14 **(summarised)**, and [Context windows](https://platform.claude.com/docs/en/build-with-claude/context-windows), Anthropic **(summarised)**. Performance degrades as input length grows, across 18 models. Supports compaction on quality grounds as well as cost.

**Subagents and multi-agent cost**

- [Subagents](https://code.claude.com/docs/en/sub-agents), Anthropic **(summarised)**. A subagent works in its own context and returns only a summary. Explore now runs on the parent's model unless one is defined with `model: haiku`.
- [How we built our multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system), Anthropic, 2025-06-13 **(summarised)**. Multi-agent systems used about 15× the tokens of a chat; most coding tasks are named a poor fit. A reason to use subagents narrowly.

**Code search**

- [SWE-grep](https://cognition.com/blog/swe-grep), Cognition, 2025-10-16 **(summarised)**. Agents spent over 60% of their first turn retrieving context; a fast search subagent runs 8 parallel tool calls for at most 4 turns. Matches the audit: 55% of Claude tool calls are reads.
- [Aider repository map](https://aider.chat/docs/repomap.html) **(summarised)**. A ranked symbol map with a default budget of 1k tokens.
- [Cursor semantic search](https://cursor.com/blog/semsearch), 2025-11-06 **(summarised)**. Semantic search plus grep gave 12.5% higher answer accuracy on average. [Why Cline doesn't index your codebase](https://cline.bot/blog/why-cline-doesnt-index-your-codebase-and-why-thats-a-good-thing) **(summarised)** argues the opposite on staleness. No published result settles it for a repository of this shape.

**Model routing**

- [FrugalGPT](https://arxiv.org/abs/2305.05176), 2023, and [RouteLLM](https://lmsys.org/blog/2024-07-01-routellm/), 2024 **(summarised)**. Cascades and learned routers matched the strongest model at large cost reductions on question-answering benchmarks. Neither was measured on agentic coding.
- [Aider architect/editor](https://aider.chat/2024/09/26/architect.html), 2024 **(summarised)**. A strong planner with a cheap editor scored higher than either alone.
- [Bigger Isn't Always Better](https://arxiv.org/abs/2606.15689), 2026 **(summarised)**. On 150 review samples a cheaper model matched a larger one (F1 0.365 against 0.343) at 3.2× lower cost, and accuracy fell from 0.657 on diffs under 10 lines to 0.043 over 150 lines.
- Anthropic's API guidance is to try the strongest model at lower effort before building a cascade, because caches are per model and a cascade gives up cache reuse.

**Automated review**

- [Code review on pull requests](https://code.claude.com/docs/en/code-review-pull-requests.md), Anthropic **(summarised)**. A review after a push "focuses on what changed since the last review and posts fewer nits and pre-existing findings". [CodeRabbit](https://docs.coderabbit.ai/overview/pull-request-review) **(summarised)** documents the same incremental model.
- [Make LLMs shut up](https://greptile.com/blog/make-llms-shut-up), Greptile, 2024-12-18 **(summarised)**. About 79% of review comments were nits, and the model's own severity scoring was "nearly random". Relevant to C5, which trusts severity and category labels.
- [CodeRabbit in the wild](https://arxiv.org/abs/2607.03316), 2026 **(summarised)**. Of 31,073 reviews, 56.3% of comments were rejected.
- [Building Bugbot](https://cursor.com/blog/building-bugbot), Cursor, 2026-01-15 **(summarised)**. Eight parallel passes with voting were later replaced by a single agentic pass. [SWR-Bench](https://arxiv.org/abs/2509.01494), 2025 **(summarised)**, found that aggregating several reviews raised F1. The evidence on one reviewer against several is mixed.
- No study was found that compares delta-only with full re-review, or one reviewer with two different models.

**Polling**

- [openai/codex#13733](https://github.com/openai/codex/issues/13733), opened 2026-03-06, still open **(read)**. "Each poll = 1 full API request with complete conversation history." A requested yield is clamped between 5 seconds and `background_terminal_max_timeout`.
- [Codex configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference), OpenAI **(summarised)**. `background_terminal_max_timeout` defaults to 300,000 ms; `model_auto_compact_token_limit` and `tool_output_token_limit` exist.
- [Scheduled tasks](https://code.claude.com/docs/en/scheduled-tasks) and [Tools reference](https://code.claude.com/docs/en/tools-reference), Anthropic **(summarised)**. Monitor "avoids polling altogether"; a foreground command that times out moves to the background; the foreground limit is 10 minutes.

## Follow-up changes to file

None of these is made here. Each is a separate change.

1. Record Grok token usage in `work_runs` (gap 2).
2. Persist provider quota snapshots and report quota against token classes (gap 1).
3. Update the price table and add a tokens-per-merged-PR roll-up (gaps 3 and 4).
4. Report review yield per round and role, and the Deep-criterion histogram (gaps 5 and 6).
5. Launch Claude workers without the personal environment (R3 step 1).
6. Remove duplicated rules from the brief, the worker rules and mono `CLAUDE.md`, and send kind-specific text only where it applies (R3 steps 2–4).
7. Set an auto-compact window for Claude workers and the coordinator, with compact instructions (R5, R8).
8. Post-merge review: recursion guard, `medium` effort, embedded diff (R1 steps 1 and 2).
9. Post-merge eligibility and the Deep classifier (R1 step 3), after the histogram.
10. Delta re-review mode and the revision gate (R2), behind an A/B.
11. Review guide once per PR (R6).
12. Blocking waits in the Claude brief; a longer Codex poll window; deny CI watching in the hook (R4).
13. Ranged-read hook for Claude workers (R7 step 1), then the exploration-subagent experiment.
14. Supervisor on Sonnet, and CI-fix without evidence off the investigation tier (R9).
15. Break down failed and orphaned run tokens by reason, and trace the three overlapping executions (R10).

## Method and limits

- **Data.** The audit's numbers were taken as given. No query was run against Boss state for this document, so every "needs one query" above is unverified.
- **Code.** Read on `main` at `c88d1058`. Feature-flag defaults in the repository (review batches, guides and progression default to off) differ from the configuration that produced the audit, where all three were clearly on.
- **Derived figures.** API calls are approximated by tool calls. Characters are converted at 4 per token, as in the audit; the first-call measurements suggest instruction text is denser than that, which would make R3 larger.
- **Grok.** Every Grok number is the audit's estimate of input only.
- **List prices.** Used as weights, not as what a subscription is charged.
- **One observed session.** The list of skills, tools and connectors in C1 comes from a single Claude worker session and from the launch code, not from a survey of transcripts.
- **No experiment was run.** Every saving is an estimate from the measured week, and the estimates overlap.
