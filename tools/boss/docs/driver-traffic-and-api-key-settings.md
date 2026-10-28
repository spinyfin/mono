# Driver traffic split and API key storage: reference

The Engine Settings pane keeps its captions to one sentence each. This
document is where the detail that used to live in those captions moved to.

## Driver traffic split

The three shares (Codex / Claude / Grok) control what percentage of new
work is sent to each driver. They always sum to 100%.

- **PR reviews and automation keep their own pinned driver, outside the
  split.** This is the single most useful fact here: it explains why a
  driver with a large configured share can still appear to receive almost
  no work — reviews and automation never draw from the split at all, so a
  high share only ever governs the work-item traffic that does.
- **An explicit per-row driver, or a product's default driver, overrides
  the split** for that row.
- **Changing the split only affects work dispatched afterwards.** Nothing
  already running is reassigned.
- **Shares are renormalised over the drivers eligible for a given
  work-item kind.** For a kind some driver cannot run, its share is
  redistributed proportionally among the remaining eligible drivers, so an
  ineligible driver never receives work it cannot do.
- **Allocation is deterministic per work item** — the same row under the
  same split always lands on the same driver.
- **Raising a share drains Claude first, then the remaining driver, each only
  as far as zero; lowering a share hands the whole surplus to Claude (or to
  Codex when Claude itself is lowered).** The three shares therefore always
  sum to 100% at every intermediate step, so the split is never in a state the
  engine would reject mid-edit.
- **Any one or two drivers may sit at 0%.** A driver at zero receives no work
  at all, and every such state is reachable by repeated single-share edits.

## Codex Sol rollout

The **Sol for eligible Codex work** control beside Driver Traffic is an
independent engine setting, `codex_sol_percentage`: 0–100%, default **50%**.
It applies after the Codex driver has been selected; it never changes the
Claude/Grok split. Changes apply to new model decisions. At 0%, automatic
selection keeps its existing model. No running execution is reassigned.

Sol (`gpt-6.1-sol`, verified against `codex debug models --bundled`) is
eligible for standard reasoning with trivial or small effort, and standard
engine-created CI-fix, merge-conflict, and PR-review-finding revisions at any
effort. Investigation reasoning, design/investigation kinds, and other
medium/large/max work retain Astra. Reviewer/automation pool policies and
existing explicit model overrides keep their own models.

The draw is SHA-256 of `codex-sol:<execution id>`, using the first eight
bytes as an unsigned big-endian integer modulo 100. Sol wins when that
bucket is below the configured percentage. `execution_sol_decisions` records
eligibility, bucket, percentage, arm, and selected model. The draw is fixed;
an escalation can change only the selected model and escalation marker.
`work_executions.model` records the model actually launched. Unlaunched
decisions do not count as experimental observations. Selection fails with
a clear error if the installed CLI catalog lacks Sol or its required effort
levels; it never substitutes another model after that failure.

A launched Sol execution that fails, is abandoned/cancelled/orphaned, declares
itself blocked, or exits undeclared permanently escalates that work item to
Astra. A failed run within an execution also records the escalation. New
executions cannot erase this history by succeeding or by changing the share.

### Comparing outcomes

Run `boss cost sol` (or `boss --json cost sol`) for an all-history comparison
of recorded eligible work. Each PR family enrolls in the arm of its first
launched eligible execution. Subsequent revisions and retries, including
escalation to Astra, remain charged to that enrollment arm. PR families
without a recorded eligible assignment are excluded. A family first enrolled by a
revision is included, but its CI measurement starts after enrollment.

The report shows both arms and their denominators:

- First-attempt CI pass rate: first terminal CI observation on the first
  published head after enrollment. Pending/missing observations do not count
  as passes; later heads cannot replace the initial result.
- Revisions per merged PR: distinct revision work items with a launched
  execution before the observed merge, including nested revision chains.
- Automated review findings per PR: recorded accepted verdict findings up
  to merge, or all findings so far for open PRs.
- Token cost per merged PR: all recorded runs in the PR family up to merge,
  including original work, revisions, and escalated retries. Input, output,
  and cache token totals are included. Unmeasured runs are shown explicitly
  and make the per-PR average unknown. These are token counts; no unverified
  Sol/Astra dollar prices are assumed.

These are observed outcomes, not an assertion that Sol and Astra cost the
same per token. The comparison is observational when classification, rollout
share, or the mix of revision-first families changes during collection.

## API key storage

The engine needs an Anthropic API key for live worker summaries and pane
summarization; both are disabled until one is available.

Saving a key in Settings stores it and restarts the engine so the new
value takes effect; the Settings value overrides any `ANTHROPIC_API_KEY`
already present in the engine's inherited environment.

Where it's stored depends on the build:

- **Signed release builds** store the key in the macOS Keychain.
- **Ad-hoc dev builds** store it in a private file under Application
  Support instead (Keychain access control does not work reliably for
  unsigned/ad-hoc-signed binaries).

Either way the entry is keyed by `APIKeyStore.service` — check that
service identifier first when debugging where a key actually went (e.g.
inspecting Keychain Access, or finding the Application Support file).
