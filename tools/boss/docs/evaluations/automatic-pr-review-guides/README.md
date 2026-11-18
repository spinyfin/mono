# Production review-guide prompt comparison corpus

This evaluation preserves the six pinned comparisons in the [design](../../designs/automatic-pr-review-guides.md). The saved September evidence remains in `cases/`; new runs go into a separate dated `runs/` directory. The historical archive reports are not modified. Model choice remains `gpt-6-astra` at `high`.

**Human grounding review and prompt acceptance are pending.** The [review template](grounding-review-template.md) enumerates references from both variants and all saved retry guides, with unchecked rows and empty verdict/notes columns. Reference kinds are mechanical heading-based hints. They are not assessments of whether the source supports a claim.

## Inputs and provenance

[corpus-manifest.json](corpus-manifest.json) fixes the repository, PR, base tip, merge base and head for all six cases. Its `prompt_version` describes the saved September outputs, not the current renderer. Every saved packet matches all three manifest revisions. The new recipe verifies those identities again and checks out the immutable head with access to the merge-base objects, including retained GitHub PR refs for squash-merged heads.

The recovered `cases/` files are prior evidence, not fresh generations. Their original prompts embed full source packets and predate the production pinned-workspace contract. Two cases have no saved guides because those old prompts exceeded the backend input limit. `mono-2890` has a saved production guide that failed validation; the invalid response and all attempt records remain available.

## Reproducible invocation

Run from the repository root, with Bazel, Python 3, jj, authenticated gh and authenticated Codex on PATH:

```sh
tools/boss/docs/evaluations/automatic-pr-review-guides/run-corpus.sh --prepare-only
tools/boss/docs/evaluations/automatic-pr-review-guides/run-corpus.sh
tools/boss/docs/evaluations/automatic-pr-review-guides/run-corpus.sh --template-only
```

Pass case ids to restrict a run, and `--run-dir /absolute/path` to record another attempt. Existing event streams cannot be overwritten, so a second attempt on the same day needs `--run-dir`. `--recollect` invokes `boss_pr_review_sources::collect_pinned_source_packet` against pinned GitHub revisions and saves a separate packet, then verifies its revision triple. It never overwrites the saved packet or historical reports. Preparation and reference extraction do not spend model quota; generation does.

`run-corpus.sh` delegates to `run-corpus.py` and builds `//tools/boss/engine/review-guide:corpus-tool` through Bazel. The tool calls the production prompt renderer, source-context renderer, collector and output validator. Its baseline retains the historical four-section request and adds the same current pinned-workspace access context. Production uses the current versioned prompt unchanged. Both receive the same saved PR description/file manifest and an empty registered-screenshot manifest; no machine-local archive or engine database is read.

Each new case directory records exact prompts, raw Codex events, stderr, final assistant messages, submitted guides, separate variant timing/usage, production validation, and source/renderer hashes. A missing or failed generation is an error, not a passing pair. `--prepare-only` renders prompts and verifies source availability without generating guides.

## Standalone transport and fidelity

The generator is `codex exec -m gpt-6-astra -c model_reasoning_effort=high`, using the production prompt. It runs against a pinned checkout in a read-only sandbox, with web search disabled and no project instructions. It uses the CLI authentication already available to the caller but does not load the worker-specific config or inherit live Boss/Cube bindings.

The production prompt submits through `BOSS_BIN propose review-guide --body`. For this standalone evaluation, `capture-submission.py` replaces that endpoint: it calls the production validator and emits accepted Markdown into the command event stream. The model receives validation errors and can correct and resubmit within the same turn. The recipe captures that submission rather than mistaking the final assistant acknowledgement for the guide. No live engine is contacted or guide published. Baseline output is the final Markdown response.

This is a prompt evaluation, not a test of interactive pane dispatch, engine persistence, retry scheduling, or the engine-specific permission hook. The source resolver uses jj to read the same immutable objects, including unchanged context; the engine uses its cube-backed git adapter. Both enforce textual source and line bounds with the production guide validator. The read-only standalone sandbox permits the local stdout capture endpoint without requiring the engine submission socket. Invocation prompts use stdin to avoid argv size limits.

## Saved September timing and usage (historical)

The following measurements belong only to the recovered September generations. They must not be interpreted as results of the current prompt.

| Case        | Variant                                       | Wall time | Input tokens | Cached input tokens | Output tokens | Reasoning output tokens | Turn failed                                                     |
| ----------- | --------------------------------------------- | --------- | ------------ | ------------------- | ------------- | ----------------------- | --------------------------------------------------------------- |
| flunge-1545 | baseline                                      | 38s       | 29,984       | 12,160              | 2,106         | 240                     | false                                                           |
| flunge-1545 | production                                    | 56s       | 30,419       | 12,160              | 3,019         | 732                     | false                                                           |
| flunge-1552 | baseline                                      | 46s       | 22,175       | 12,160              | 2,554         | 518                     | false                                                           |
| flunge-1552 | production                                    | 54s       | 22,610       | 12,160              | 2,860         | 516                     | false                                                           |
| flunge-1605 | baseline                                      | —         | —            | —                   | —             | —                       | (skipped — prompt exceeded `MAX_PROMPT_CHARS`: 1,559,562 chars) |
| flunge-1605 | production                                    | —         | —            | —                   | —             | —                       | (skipped — prompt exceeded `MAX_PROMPT_CHARS`: 1,562,064 chars) |
| mono-2860   | baseline                                      | —         | —            | —                   | —             | —                       | (skipped — prompt exceeded `MAX_PROMPT_CHARS`: 2,285,631 chars) |
| mono-2860   | production                                    | —         | —            | —                   | —             | —                       | (skipped — prompt exceeded `MAX_PROMPT_CHARS`: 2,288,133 chars) |
| mono-2886   | baseline                                      | 57s       | 46,819       | 12,160              | 2,852         | 795                     | false                                                           |
| mono-2886   | production                                    | 56s       | 47,254       | 12,160              | 2,943         | 516                     | false                                                           |
| mono-2890   | baseline                                      | 50s       | 66,586       | 12,160              | 2,743         | 378                     | false                                                           |
| mono-2890   | production (3rd/accepted-but-invalid attempt) | 144s      | 965,908      | 872,192             | 6,782         | 569                     | false                                                           |

Source: the saved per-case `usage-*.json`, `events-*.jsonl`, `retries-production.json`, and `too-large.json` files. The reported mono-2890 production usage is the final unsuccessful attempt, not a validated acceptance. Individual attempts retain their separate measurements.

## Fresh paired generations (2026-11-18)

[runs/2026-11-18](runs/2026-11-18) holds one baseline and one production generation per case, all with `gpt-6-astra` at `high` through the production prompt renderer (`review-guide-v12`, template hash in `generator.json`). Every case directory has the exact prompts, raw event streams, stderr, final messages, submitted guides, `usage-*.json`, `validation-production.txt` and `provenance.json`. All six production guides passed the production validator (`VALID`), each after exactly one accepted submission. Baseline guides are the final assistant message and are not validator-gated, so their pass/fail status says nothing about their links; see the mechanical check below.

Timing and usage are separate per variant (token counts are cumulative over the whole turn, including cached re-reads of the checkout):

| Case        | Variant    | Wall time | Input tokens | Cached input tokens | Output tokens | Reasoning output tokens | Accepted submissions | Turn failed |
| ----------- | ---------- | --------- | ------------ | ------------------- | ------------- | ----------------------- | -------------------- | ----------- |
| flunge-1545 | baseline   | 80s       | 128,556      | 103,936             | 3,040         | 283                     | 0                    | false       |
| flunge-1545 | production | 102s      | 202,990      | 169,216             | 4,183         | 327                     | 1                    | false       |
| flunge-1552 | baseline   | 91s       | 154,515      | 124,160             | 3,300         | 284                     | 0                    | false       |
| flunge-1552 | production | 163s      | 146,753      | 122,368             | 7,353         | 534                     | 1                    | false       |
| flunge-1605 | baseline   | 139s      | 482,072      | 412,800             | 5,307         | 753                     | 0                    | false       |
| flunge-1605 | production | 289s      | 926,581      | 846,464             | 11,966        | 1,190                   | 1                    | false       |
| mono-2860   | baseline   | 123s      | 517,994      | 448,384             | 4,872         | 570                     | 0                    | false       |
| mono-2860   | production | 184s      | 603,738      | 539,136             | 7,190         | 761                     | 1                    | false       |
| mono-2886   | baseline   | 73s       | 150,625      | 122,496             | 2,865         | 60                      | 0                    | false       |
| mono-2886   | production | 117s      | 254,936      | 205,824             | 4,653         | 459                     | 1                    | false       |
| mono-2890   | baseline   | 143s      | 376,526      | 315,008             | 4,636         | 501                     | 0                    | false       |
| mono-2890   | production | 164s      | 393,551      | 330,752             | 6,522         | 340                     | 1                    | false       |

Unlike the saved September prompts, which embedded full source packets, the current prompts are small (about 5-20 KB) because the model reads source from the pinned checkout. Every case now runs; none is skipped for prompt size.

[runs/2026-10-27](runs/2026-10-27) is an earlier, interrupted attempt kept as a record. Only `flunge-1545` finished there; the rest failed on network errors (`stderr-*.txt`, `turn_failed: true`) or never ran. It is not used by the template or the tables above.

## Mechanical link verification

`verify-links.py` checks every pinned blob link in the template without judging support: the revision resolves in the pinned checkout, the file exists at that revision, and the line range lies inside the file. Result at the time of this run: 509 links pass; 9 are PR/diff links and 4 point at an external repository, which are not line-checked; 4 have a line range past the end of the file. All four are in saved September outputs, which are not edited: `cases/flunge-1552/guide-baseline.md` (`jest.config.cjs#L1-L8`, file has 7 lines), `cases/mono-2886/guide-production-attempt3.md` (`BossCaptureTests.swift#L347-L408`, 406 lines), `cases/mono-2890/guide-production-attempt1.md` (`MarkdownFindState.swift#L169-L189`, 173 lines) and `cases/mono-2890/guide-production-attempt2.md` (`MarkdownFindState.swift#L104-L182`, 173 lines). Every link in the fresh `runs/2026-11-18` guides passes. A pass here only means the link resolves, not that it shows what the guide claims. Baseline guides also contain links to machine-local checkout paths, which `checkleft` reports as `md/link-integrity` warnings; they are raw model output, are not GitHub references, and so are not template rows.

## Evidence retention

The repo owner approved preserving this fixed evaluation corpus together on 2026-10-27. `CHECKS.yaml` scopes the file-count exception to the saved cases and the dated run evidence (`runs/2026-10-27` and `runs/2026-11-18`), and the file-size exception to the eight original oversized prompt files. Implementation and every other check remain subject to normal validation.

## Human review

Open every link in the [template](grounding-review-template.md) in GitHub. Confirm its intended file, immutable revision, before/after side, line range, and the claim or example it supports. Record a verdict and notes only after that human check. Objective structural/link validation is separate from this judgement. Prompt acceptance is intentionally blank until that review is complete. The template covers the saved September outputs and the fresh `runs/2026-11-18` guides; `run-corpus.sh --template-only` regenerates it, discarding any recorded verdicts, so do that only before review starts.
