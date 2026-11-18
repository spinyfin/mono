# Production review-guide prompt comparison corpus

This evaluation preserves the six pinned comparisons in the [design](../../designs/automatic-pr-review-guides.md). The saved September evidence remains in `cases/`; new runs go into a separate dated `runs/` directory. Published evidence uses the deterministic identifier redaction described below; source links and line numbers are preserved. The historical archive reports are not modified. Model choice remains `gpt-6-astra` at `high`.

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

The work directory defaults to `$XDG_CACHE_HOME/boss-review-guide-corpus` (or `~/.cache/boss-review-guide-corpus`); pass `--work-dir /absolute/path` to both the recipe and `verify-links.py` to override it. Clones and checkouts stay outside the repository, and provenance records their paths relative to that work directory.

Pass case ids to restrict a run, and `--run-dir /absolute/path` to record another attempt. Existing event streams cannot be overwritten, so a second attempt on the same day needs `--run-dir`. `--recollect` invokes `boss_pr_review_sources::collect_pinned_source_packet` against pinned GitHub revisions and saves a separate packet, then verifies its revision triple. It never overwrites the saved packet or historical reports. Preparation and reference extraction do not spend model quota; generation does.

`run-corpus.sh` delegates to `run-corpus.py` and builds `//tools/boss/engine/review-guide:corpus-tool` through Bazel. The tool calls the production prompt renderer, source-context renderer, collector and output validator. Its baseline retains the historical four-section request and adds the same current pinned-workspace access context. Production uses the current versioned prompt unchanged. Both receive the same saved PR description/file manifest, task context reconstructed from that packet’s PR title and body, and an empty registered-screenshot manifest. The runner and corpus tool share `compose_prompt`, which appends source context, mandatory task context and screenshots in the same order.

**Context reconstruction limitation:** historical engine task, revision and project records are unavailable in the pinned repository evidence. The saved PR title and body explicitly substitute for the originating task; revisions are empty and project is absent. These inputs use the production composition contract but do not claim to reconstruct the original engine records. Each case’s provenance records this limitation; no project is invented and no machine-local archive or engine database is read. The shared renderer describes an empty revision list and absent project as no revisions and no containing project: those are controlled evaluation assumptions, not reconstructed historical facts. This run cannot establish the quality of historical project or revision orientation.

Each new case directory records publication copies of prompts, Codex events, stderr, final assistant messages, submitted guides, separate variant timing/usage, production validation, and source/renderer hashes. A missing or failed generation is an error, not a passing pair. `--prepare-only` renders prompts and verifies source availability without generating guides.

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

## Earlier paired generations without task context (2026-11-18)

**These earlier generations omit the mandatory task context and are not production-faithful evaluations.** They remain separate evidence, not acceptance results. [runs/2026-11-18](runs/2026-11-18) holds one baseline and one production generation per case, all with `gpt-6-astra` at `high` through the production prompt renderer (`review-guide-v12`, template hash in `generator.json`). Every case directory has the exact prompts, raw event streams, stderr, final messages, submitted guides, `usage-*.json`, `validation-production.txt` and `provenance.json`. All six production guides passed the production validator (`VALID`), each after exactly one accepted submission. Baseline guides are the final assistant message and are not validator-gated, so their pass/fail status says nothing about their links; see the mechanical check below.

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

[runs/2026-10-27](runs/2026-10-27) is an earlier, interrupted attempt kept as a record. Only `flunge-1545` finished there; the rest failed on network errors (`stderr-*.txt`, `turn_failed: true`) or never ran. Its available guides are included in the reference template; it is not represented as a complete paired run.

## Paired generations with reconstructed task context (2026-11-18)

[runs/2026-11-18-context-complete](runs/2026-11-18-context-complete) contains six new pairs generated with the shared production composer and PR-derived task context. All twelve model turns exited successfully; all six production guides passed the production validator. Each variant retains separate prompts, events, timing and token usage. The historical engine-record limitation described above applies to this run too; human grounding and prompt acceptance remain pending.

| Case        | Variant    | Wall time | Input tokens | Cached input tokens | Output tokens | Accepted submissions |
| ----------- | ---------- | --------- | ------------ | ------------------- | ------------- | -------------------- |
| flunge-1545 | baseline   | 74.5s     | 166,909      | 133,632             | 3,000         | 0                    |
| flunge-1545 | production | 96.8s     | 227,868      | 191,104             | 3,954         | 1                    |
| flunge-1552 | baseline   | 94.0s     | 188,506      | 169,344             | 3,700         | 0                    |
| flunge-1552 | production | 106.2s    | 235,787      | 193,536             | 4,408         | 1                    |
| flunge-1605 | baseline   | 119.4s    | 391,483      | 319,744             | 5,017         | 0                    |
| flunge-1605 | production | 187.1s    | 690,865      | 605,568             | 7,525         | 1                    |
| mono-2860   | baseline   | 137.1s    | 489,909      | 420,352             | 5,488         | 0                    |
| mono-2860   | production | 176.2s    | 728,312      | 648,576             | 7,062         | 1                    |
| mono-2886   | baseline   | 86.1s     | 220,976      | 169,600             | 3,521         | 0                    |
| mono-2886   | production | 97.3s     | 238,395      | 193,792             | 4,098         | 1                    |
| mono-2890   | baseline   | 123.2s    | 413,430      | 351,104             | 4,712         | 0                    |
| mono-2890   | production | 163.0s    | 524,844      | 452,224             | 6,610         | 1                    |

`runs/2026-11-18-context` preserves an interrupted attempt: its first baseline completed, then a concurrent Bazel invocation removed the local corpus-tool output before the next prompt could be rendered. The recipe now copies the built executable into a content-addressed location in the work directory before generating. The complete run above uses that stable copy; its binary hash is in `generator.json`.

## Mechanical link verification

`verify-links.py --work-dir /absolute/path` checks pinned blob links in the template without judging support: the revision resolves in the pinned checkout, the file exists at that revision, and the line range lies inside the file. Non-GitHub targets, including machine-local baseline links, have their own unchecked rows and are counted separately as not navigable. PR/diff links and external repositories are counted separately from checked blob ranges.

The complete table has 826 unchecked rows: 727 pinned blob references pass, 74 non-GitHub targets are counted separately, 17 are PR/diff links, and four point at repositories not pinned locally. The verifier exits 1 for exactly the four annotated historical range failures below; no new pinned-link range failures were found. All twelve saved prompts match the final compiled composer after publication redaction, and all 131 captured source objects passed original-hash and sanitized-text verification.

Four known out-of-bounds ranges in the saved September outputs are marked **mechanical failure** in the template’s Kind column, leaving all human judgement columns blank: `cases/flunge-1552/guide-baseline.md` (`jest.config.cjs#L1-L8`, file has 7 lines), `cases/mono-2886/guide-production-attempt3.md` (`BossCaptureTests.swift#L347-L408`, 406 lines), `cases/mono-2890/guide-production-attempt1.md` (`MarkdownFindState.swift#L169-L189`, 173 lines) and `cases/mono-2890/guide-production-attempt2.md` (`MarkdownFindState.swift#L104-L182`, 173 lines). Detection compares the range endpoints with the pinned object’s line count. Original targets remain unchanged; the annotation exposes the defect without rewriting model evidence or implying a grounding verdict.

## Evidence retention

The repo owner approved the one-off evaluation file-length exception on 2026-10-27 and the one-PR file-count exception (up to 360 non-deleted files) on 2026-11-18 16:18 UTC. Commit-description directives carry these exceptions; there are no permanent corpus exclusions in `CHECKS.yaml`. Other checks remain unchanged.

### Publication sanitization

`run-corpus.py` applies `publication_text` to publication copies: standalone internal task/project identifiers are replaced with `[internal-id-redacted]`. JSON string values are transformed recursively, including source snapshots and event payloads; JSONL is transformed one record at a time. No lines are added or removed from source strings, and GitHub targets and revisions are unchanged. `publication.json` records original and published SHA-256 hashes for each transformed file. Packet content hashes remain the original capture hashes; they are provenance, not hashes of the redacted publication strings.

The source verifier applies the identical transformation to both pinned object text and saved text before comparing them. The transformation is deterministic and idempotent; published evidence is explicitly not byte-for-byte raw evidence where a redaction occurred. Original objects remain recoverable by the pinned repository revisions. Existing timing/usage values and invalid link endpoints are preserved. Markdown publication copies follow repository formatting; event streams retain generated text except for identifier redaction. New runs apply publication sanitization before creating their local reference template.

## Human review

Open every link in the [template](grounding-review-template.md). Confirm its intended file, immutable revision, before/after side, line range, and the claim or example it supports; record local-path navigation failures too. Record a verdict and notes only after that human check. Objective structural/link validation is separate from this judgement. Prompt acceptance remains pending until that review is complete.

The committed template includes all saved guides and available dated-run guides, including interrupted runs. Only `--template-only` writes it; ordinary generation and `--prepare-only` write a template inside their run directory. External run directories are supported and receive stable `file:` source labels. Any checked box, nonempty verdict/notes cell, or completed sign-off prevents replacement unless `--force-template` is explicitly passed. The formatter-stable sign-off uses `(name)`, `(YYYY-MM-DD)` and `(pending)` placeholders.
