# boss-engine-pr-review

Owns the reviewer worker's two text boundaries: rendering the prompts a
`pr_review` execution is launched with, and parsing the `ReviewResult`
it produces. It exists so the engine's dispatch and completion paths can
launch a reviewer and consume its verdict without carrying the review
rubric, the prompt wording, or the JSON-scraping fallback themselves.

A reviewer worker operates read-only: it reads a PR's diff and changed
files, writes one structured `ReviewResult`, and never commits, pushes,
or posts to GitHub. See the crate-level docs in `src/lib.rs` for the
three layers that enforce that mandate and for the output contract.

## Architecture

The crate is a pure text-in / text-out library — no I/O, no async, no
knowledge of how a reviewer is spawned or where its artifact lands. It
is organised around the round trip of a single review.

`types` defines the vocabulary: `ReviewResult` and its `ReviewFinding`
parts (severity, category, confidence), the `PrReviewContext` of
pre-fetched PR metadata a prompt is rendered against, and the
`ReviewScope` that selects a rubric.

`render` turns that vocabulary into prompts — the reviewer's initial
prompt and CLAUDE.md, and the revision instructions handed back to the
producing worker when a review warrants changes. `boss_engine`'s
`worker_setup::render_claude_md` delegates here for
`WorkerKind::Reviewer`.

`parsing` handles the return leg: `ReviewResult::from_json` for the
artifact the reviewer writes, `extract_review_result` as the legacy
fenced-JSON scraper for when that file is absent, `passes_severity_gate`
for the engine's revision decision, and `classify_changed_files` to pick
a scope from a file list before the prompt is rendered.

`supersession_scan` is the deterministic incident-002 remediation: it
flags supersession/obsolescence language in a PR's narrative so the
reviewer is required to check the claim against a design doc. Its two
halves sit on either side of the round trip — the engine's runner scans
narrative text to populate `PrReviewContext::supersession_flags`, and
`render` embeds the rendered flag block in the reviewer prompt — so the
module lives here with them rather than in the engine.

Only `boss-engine` depends on this crate, and the edge is one-directional.
