# Locationless and virtual-path finding rendering

- **Date:** 2026-07-24
- **Work item:** Investigation: locationless / changeset-scoped finding rendering
- **Parent project:** Boss-ism leakage: generic regex check in checkleft, adopted by boss and flunge
- **Consumed by:** the changeset-scheduling task, which needs to know what shape a finding about the PR description or commit message should take
- **Scope:** research only; no production code changed. A throwaway probe was added, run, and reverted.

A check that inspects the PR description or the commit message has no file to point at. This investigation establishes what checkleft actually does with the two candidate shapes for such a finding — a bare locationless `Finding`, and a `Finding` carrying a synthetic path such as `<pr-description>` — across every output surface.

## Verdict

Emit **bare locationless findings**. The synthetic-path option is not merely inadvisable, it is non-functional: a finding whose location path is not a changed file is silently discarded by the framework before it reaches any output surface, so a synthetic path yields _zero_ output rather than a nicely-rendered annotation.

Locationless findings survive the framework by explicit design, and they already drive the terminal output, the JSON output, the process exit code, the check-run conclusion, and the check-run finding counts.

They have one real gap: **the message text is invisible in every GitHub UI surface.** SARIF and check-run annotations both require a file path, so locationless findings are filtered out of both. The changeset-scheduling task should therefore pair "emit locationless" with a small framework change that renders locationless findings into the check-run summary body. That is one fix in one place, versus minting fake paths that every downstream consumer and every exclusion glob then has to special-case.

## Method

Read the four surfaces — `tools/checkleft/src/annotate/mod.rs`, `annotate/sarif.rs`, `annotate/check_run.rs`, and `render_finding` in `src/main.rs` — then confirmed the behaviour empirically. The probe was a temporary test module appended to `tools/checkleft/src/runner/tests.rs` (so it could reach the private framework filters), run under `bazel test //tools/checkleft:checkleft_lib_test_rest`, and reverted afterwards. Every concrete number and JSON fragment quoted below is real probe output, not a prediction.

One claim below could not be verified locally and is flagged as such: how GitHub itself renders an annotation whose path does not exist in the PR diff. Confirming that requires posting a check run to a live PR, which is outward-facing and out of scope for an investigation.

## Finding 1 — the framework silently drops findings on paths outside the changeset

This is the decisive fact, and it kills the synthetic-path option.

`scope_findings_to_changeset` (`tools/checkleft/src/runner.rs:1330`) retains a finding only if it has **no location** or if its `location.path` is one of the run's changed files:

```rust
result.findings.retain(|finding| match &finding.location {
    None => true,
    Some(location) => changed.contains(location.path.as_path()),
});
```

Probe result, feeding three findings (one locationless, one on `<pr-description>` with a line, one on `<pr-description>` with no line) through the filter against a changeset containing only `src/lib.rs`:

```
[scope] before: 3 findings
[scope] after scope_findings_to_changeset: 1 findings
[scope]   survivor location=None
```

Both synthetic-path findings were discarded. There is no warning and no log line — from the author's point of view the check simply produced nothing.

This filter is not specific to one kind of check. `apply_policy_to_result`, which calls it, runs on the result of **both** built-in checks (`runner.rs:236`) and external/wasm checks (`runner.rs:343`). Since this project's check is planned in the wasm form, that matters: **a wasm check cannot emit a finding on a path outside the changeset at all.** The only two shapes that survive are locationless, or a path that is genuinely in the changeset.

The existing exemption for framework-meta findings does not help. Config diagnostics, bypass notices, and stale-exclusion findings do land on an unchanged `CHECKS` file, but as the comment on `drop_excluded_findings` records, they "never flow through here — they are produced outside the per-check result path". That escape hatch is available to the framework, not to a check.

## Finding 2 — injecting the synthetic path into the changeset works, but is a bad trade

For completeness the probe tested the obvious workaround: declaring `<pr-description>` as a changed file so it passes the scope filter.

```
[scope] virtual path declared as changed file -> survives: true
```

It works mechanically, and it is the wrong lever. `changeset.changed_files` feeds eligible-file counts, per-check file selection, progress reporting, and every _other_ check's view of the change. A pseudo-entry there would inflate counts and expose a nonexistent file to unrelated checks. Rescuing one finding by lying to the whole framework is a poor exchange.

## Finding 3 — synthetic paths collide with ordinary exclusion globs

Assume for a moment the scope filter were relaxed. The synthetic path then has to survive `drop_excluded_findings`, and angle-bracket paths match common catch-all globs:

| exclusion glob | excludes `<pr-description>`? |
| -------------- | ---------------------------- |
| `**/*.rs`      | no                           |
| `docs/**`      | no                           |
| `**`           | **yes**                      |
| `*`            | **yes**                      |

Any check instance or repo configured with a broad `**` or `*` exclusion would silently drop the description finding. This is a second silent-loss channel layered on the first.

## Finding 4 — how each surface renders each shape

`annotation_from_finding` (`annotate/mod.rs`) returns `None` for a finding with no location, which is the single upstream gate for SARIF, check-run annotations, and GHA workflow commands. A finding _with_ a location but no line is anchored at line 1 — GitHub requires a line, so a file-level finding lands at the top of the file.

```
[annot] locationless -> false
[annot] virtual(no line) -> path="<pr-description>" start_line=1 col=None
```

| Surface                     | Bare locationless                             | Synthetic path (if it survived scoping)               |
| --------------------------- | --------------------------------------------- | ----------------------------------------------------- |
| Terminal (`render_finding`) | renders; location shows as `--> <unknown>`    | renders; location shows as `--> <pr-description>`     |
| `--format=json`             | renders faithfully with `location: null`      | renders with the synthetic path                       |
| Exit code                   | counted; an error finding still fails the run | counted                                               |
| SARIF results               | **dropped** — 0 results _and_ 0 rules         | 1 result, `artifactLocation.uri` = the synthetic path |
| Check-run annotations       | **dropped** — 0 annotations                   | 1 annotation with `path` = the synthetic path         |
| Check-run conclusion        | still `failure`                               | `failure`                                             |
| Check-run summary counts    | counted — "1 finding: 1 error"                | counted                                               |
| GHA workflow commands       | **dropped**                                   | `::error file=<pr-description>,line=1,…`              |

The check-run row is the important asymmetry, and it is worth stating plainly. A run whose only finding is locationless posts a **red check with no visible explanation**:

```
[checkrun] annotations from locationless-only = 0
[checkrun] conclusion(locationless-only) = failure
[checkrun] summary(locationless-only) = checkleft found 1 finding: 1 error, 0 warnings, 0 notices.
```

`output_summary` emits counts only — never message text. So a reviewer sees a failed checkleft check claiming one error, with nothing anywhere in the GitHub UI saying what it was. The message exists only in the CI log.

SARIF is worse in one respect: the locationless finding is dropped from `results` _and_ its check id never enters the `tool.driver.rules` catalog, so there is no residual trace of the rule having fired.

For reference, this is what a surviving synthetic-path finding serializes to — correct in shape, pointing at a file that does not exist:

```json
{
  "ruleId": "boss/no-boss-isms",
  "level": "error",
  "message": { "text": "PR description leaks a work-item id" },
  "locations": [
    {
      "physicalLocation": {
        "artifactLocation": { "uri": "<pr-description>" },
        "region": { "startLine": 1 }
      }
    }
  ]
}
```

## Finding 5 — angle brackets are not a valid SARIF URI

If a synthetic path is ever adopted, `<pr-description>` is a poor spelling of it. SARIF types `artifactLocation.uri` as a URI reference, and `<` and `>` are excluded characters under RFC 3986, so the value above is not a well-formed URI reference. Whether GitHub's code-scanning ingest rejects it depends on how strictly it enforces the schema's `format` keyword, which is commonly advisory — so this is a risk rather than a certainty, and it was not tested against the live service.

The GHA workflow-command path has no such problem: `escape_workflow_property` encodes `%`, `\r`, `\n`, `:`, and `,`, and angle brackets pass through harmlessly.

A path-shaped alternative such as `.checkleft/pr-description` would avoid the URI question entirely. It does not avoid Findings 1–3.

Note also that the terminal renderer already prints `<unknown>` for a locationless finding, so angle-bracket pseudo-syntax is established in checkleft's _human_ output. That precedent does not extend to machine surfaces that expect real paths.

## Recommendation for the changeset-scheduling task

1. **Emit findings about the PR description and commit message as bare locationless findings** — `location: None`. This is the only shape a wasm-form check can emit for a non-file subject, and it is the shape the framework's filters explicitly preserve.
2. **Do not mint synthetic paths, and do not relax `scope_findings_to_changeset` to accommodate one.** That filter is load-bearing: it is what stops clippy and rustfmt over-reporting outside the change. Punching a hole in it for virtual paths weakens a guarantee that currently holds uniformly.
3. **Do not inject pseudo-entries into `changeset.changed_files`.**
4. **Treat the GitHub-visibility gap as a separate, framework-level fix**, not as a reason to choose synthetic paths. The narrowest version: have the check-run backend append locationless findings' `check_id` and `message` to `output.summary`, which is already free-form Markdown and already posted on every run. This makes changeset-scoped findings visible in the Checks tab without inventing a file. SARIF has no equivalent locationless slot and can reasonably keep omitting them.

Until item 4 lands, a locationless finding is fully effective as a **gate** — it fails the build and prints in the log — and invisible as an **explanation** in the GitHub UI. That is acceptable for a first cut, provided the check's log message is self-explanatory, but it should not stay that way.

## Follow-up code changes

These are out of scope for this doc-only change and are noted for filing separately.

- **Surface locationless findings in the check-run summary body.** Extend `annotate/check_run.rs` so `output.summary` lists locationless findings (check id plus message) beneath the existing counts line. This closes the "red check, no explanation" gap identified in Finding 4 and is the prerequisite for locationless findings being a good reviewer experience rather than merely a correct gate.
- **Consider logging dropped findings in `scope_findings_to_changeset`.** The filter currently discards findings with no diagnostic at all, which is what made the synthetic-path failure mode invisible during this investigation. A `debug!`-level line naming the dropped path would have answered this investigation's central question in one run, and would help any future check author who accidentally reports outside the changeset.

## Open questions

- Does GitHub render a check-run annotation whose `path` is not present in the PR diff? Expected behaviour is that the API accepts it (GitHub does not validate path existence) and it appears in the check page's annotation list but not inline on "Files changed", since there is no diff line to anchor to. **Not verified here** — it requires posting to a live PR. It only matters if the synthetic-path option is revisited, which this document recommends against on other grounds.
- Should the check-run summary listing be capped? If a PR description leaks many matches, an unbounded list could be large. A cap through the existing `cap_with_log` helper would keep it consistent with the other backends' loud-truncation discipline.
