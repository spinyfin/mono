# checkleft: line-scoped detection and fixes

**Status:** investigation / design proposal — no code changes in this PR.
**Scope:** `tools/checkleft/` in `spinyfin/mono`.
**Author:** boss worker `exec_18bdf645c189b038_179`.

## Problem statement

checkleft already restricts _which files_ a check looks at to the set changed by a
PR (or local working tree). It does **not** restrict _which lines within those
files_ produce findings or fixes. The result: a formatting check that fails on a
file you touched will flag — and, in `fix` mode, rewrite — formatting problems on
lines you never went near. On a large, long-lived file that has drifted from the
formatter's preferred style, touching one line can drag dozens of unrelated
reformatting edits into your diff, or block your PR on pre-existing formatting debt
that is not yours to fix.

The premise of this investigation: checkleft already does change detection, and the
"these line regions changed" information it computes should become the single source
of truth used as a **post-filter** applied to both reported findings and applied
fixes. Formatting checks (`format/rust`, `format/bazel`, `format/*`) are the
motivating case, but the mechanism should generalize to any line-anchored finding.

This document establishes, precisely, what change detection produces today; designs
a uniform detection post-filter; evaluates strategies for the genuinely hard part
(filtering whole-file formatter rewrites down to changed regions) and recommends
one; specifies which check classes the filter should apply to; and lays out a
phased implementation plan with effort estimates.

The headline finding: **the hardest prerequisite is already built.** checkleft
already parses per-hunk line ranges from the diff and carries them, per file, on the
`ChangeSet` that every check receives. Detection-side line filtering is a small,
low-risk addition that reuses existing data at an existing choke point. Fix-side
filtering is materially harder and is the subject of most of this document.

---

## Current-state findings

All references are to `tools/checkleft/` at the commit this branch is based on.

### Change detection: two layers

Change detection splits cleanly into two layers:

1. **`src/change_detection/`** decides _what base to diff against_. It classifies
   the CI/local scenario and resolves a base SHA, returning a `ChangePlan`
   (`src/change_detection/mod.rs:32-40`):

   ```rust
   pub enum ChangePlan {
       All,                                          // --all
       Scoped { base_sha: String, scenario: Scenario },
       Empty { reason: EmptyReason },
   }
   ```

   It does **not** run the diff itself.

2. **`src/vcs.rs`** (+ `src/vcs/patch_line_deltas.rs`) runs the actual diff against
   that base and parses the result into a `ChangeSet` (the type checks consume).

### Scenarios and base-ref resolution

`Scenario` (`src/change_detection/scenario.rs:7-21`): `PullRequest { base_branch }`,
`MergeQueue`, `PushToDefault`, `PushToBranch { branch }`, `Local`. The base SHA for
each is resolved centrally in `select_base` (`src/change_detection/base.rs:180-244`):

| Scenario        | Base resolution                                                                                                                  |
| --------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `PullRequest`   | `merge-base(origin/<base_branch>, HEAD)`, falling back to bare `base_branch` (`base.rs:202-213`)                                 |
| `MergeQueue`    | GitHub `merge_group.base_sha`, else `HEAD^1` (`base.rs:215, 247-271`)                                                            |
| `PushToDefault` | `HEAD^1` (`base.rs:218, 280-282`)                                                                                                |
| `PushToBranch`  | `merge-base(origin/<default_branch>, HEAD)` (`base.rs:228-239`)                                                                  |
| `Local`         | `merge-base(origin/<default_branch>, HEAD)` → `WorkingTree { base_sha }`, includes staged + uncommitted (`base.rs:242, 298-316`) |

Crucially for this design, there is **one** base-SHA resolution per run, and the diff
that produces the changeset uses exactly that SHA. `Scenario::Local` deliberately
prefers `origin/<default_branch>` over a possibly-stale local bookmark
(`base.rs:293-304`; test `row6_local_prefers_origin_ref_over_stale_local`,
`base.rs:602-618`), because in jj/cube worker workspaces the local `main` is often
at HEAD and a merge-base against it would collapse to HEAD and silently empty the
changeset. **Any new line-range source must reuse this resolved base SHA, never a
bare `main`** — re-deriving the base independently would reintroduce exactly the
stale-changeset class of defect that `select_base_local` exists to prevent.

### What change detection produces today: **hunk ranges, already**

The changeset carrier is `ChangeSet` (`src/input.rs:9-24`). It is **not** paths-only.
It carries, per file, the precise changed hunk ranges:

```rust
pub struct ChangeSet {
    pub changed_files: Vec<ChangedFile>,                       // path + kind + rename src
    pub file_line_deltas: HashMap<PathBuf, FileLineDelta>,     // per-file added/removed counts
    pub file_diffs: HashMap<PathBuf, FileDiff>,                // per-file hunks  ← the ranges
    // ... commit_description, pr_description, change_id, repository
}
```

with (`src/input.rs:87-112`):

```rust
pub struct FileDiff { pub hunks: Vec<DiffHunk> }

pub struct DiffHunk {
    pub old_start: usize, pub old_lines: usize,
    pub new_start: usize, pub new_lines: usize,   // ← post-image line range
    pub added_lines: usize, pub removed_lines: usize,
}
```

These are populated by `attach_line_deltas` (`src/vcs.rs:586-598`), which feeds the
patch text from `git diff --patch <base> HEAD` into the hunk parser
`parse_file_diffs_from_git_patch` (`src/vcs/patch_line_deltas.rs:14-123`). The parser
is a complete unified-diff state machine: it handles `@@ -a,b +c,d @@` headers
(`parse_hunk_header`, `patch_line_deltas.rs:145-163`), new files (`old_start=0`),
deletions (keyed under old path), renames (keyed by new path), and binary files
(skipped — no `@@` lines). It is well tested (`patch_line_deltas.rs:171-262`).

The per-check changeset built during scheduling copies `file_diffs` through for each
changed file (`src/runner.rs:1119-1124`), so the hunk ranges are available inside the
finding-filtering choke point described below. They are also lowered to the WASM
guest ABI (`lower_diff_hunk`/`lower_file_diff`, `src/external/runtime.rs:1392-1408`),
so external checks already _receive_ hunks as input — but nothing uses them to filter
findings.

**One precision caveat that drives the design.** The changeset diff uses git's
default context (`--patch`, i.e. `-U3`), not `--unified=0`. With 3 lines of context,
a `DiffHunk`'s `new_start..new_start+new_lines` span includes up to 3 _unchanged_
context lines on each side of the real edits. The parser tracks a per-hunk
`added_lines` _count_ but not _which_ lines within the hunk were added. So the
hunk spans available today over-approximate the truly-changed line set. For a precise
"only changed lines" filter we need exact added-line positions — see the Detection
design below.

### How findings are represented (and a gap)

A finding is `Finding` (`src/output.rs:11-19`) with an optional single-point location
(`src/output.rs:40-45`):

```rust
pub struct Finding {
    pub severity: Severity,
    pub message: String,
    pub location: Option<Location>,            // None = check-level / file-level
    pub remediations: Vec<String>,
    pub suggested_fix: Option<SuggestedFix>,
}
pub struct Location { pub path: PathBuf, pub line: Option<u32>, pub column: Option<u32> }
```

There is **no end-line / end-column / range** on a finding. A finding is a single
`(path, line?, column?)` point. Annotations nominally carry `start_line`/`end_line`
(`src/annotate/mod.rs:26-42`) but always set `end_line == start_line` ("ranges are
deferred to a future task"), and SARIF/Check Run/GHA backends all collapse to a
single line (`sarif.rs:98-106`, `check_run.rs:146-160`, `annotate/mod.rs:119-146`).
**Implication:** for line-anchored findings, intersection with a changed hunk is a
single-point-in-range test — there is no multi-line straddle to resolve today.
(Should ranged findings ever be added, the intersection semantics in the Detection
design below already cover them.)

### The choke point for a detection post-filter

`apply_policy_to_result` (`src/runner.rs:1310-1351`) is the single place every
per-check result is normalized before it is streamed to the terminal, returned, JSON-
serialized, annotated (SARIF / Check Run / GHA), counted toward the exit code, or fed
to the fix planner. It already hosts the two existing location filters:

```rust
fn apply_policy_to_result(mut result, policy, changeset, exclusion) -> CheckResult {
    scope_findings_to_changeset(&mut result, changeset);   // file-level scope (runner.rs:1316)
    drop_excluded_findings(&mut result, exclusion);        // exclusion globs   (runner.rs:1317)
    // ... bypass handling, severity normalization
}
```

`scope_findings_to_changeset` (`runner.rs:1284-1290`) drops any finding whose
`location.path` is not a changed file (location-less findings are kept). It runs on
both the built-in path (`runner.rs:209`) and the external/WASM path (`runner.rs:311`),
and it runs **before** `reporter.stream_findings(&result)` (`runner.rs:216, 318`), so
it also suppresses live terminal streaming. A line-level filter belongs right here,
immediately after the file-level one.

### Existing change-scoping is file-level and universal

There is **no per-check "run only on changed files" flag**. Change-scoping is a
framework-wide invariant: the runner hands each check a changeset containing only the
(exclusion-filtered) changed files, and `scope_findings_to_changeset` guarantees no
finding escapes that file set. `--all` mode swaps in `all_files_changeset` so the
same per-file membership test is a natural no-op (`runner/tests.rs:1197-1222`). This
is the key distinction the proposed feature must preserve:

- **Existing:** "only _run/report_ this check on changed **files**" — universal,
  file-granular.
- **Proposed:** "only _report/fix_ findings on changed **lines**" — opt-in,
  line-granular, composed _after_ the file filter.

They compose cleanly: file filter first (drops findings on untouched files), line
filter second (drops findings on untouched lines of touched files). No double-filter
or conflict — the line filter only ever sees findings that already survived the file
filter.

### How fixes are applied (two mechanisms)

There are two distinct fix paths with different content models:

1. **Declarative external fixes — whole-file replacement.** Every formatter and
   linter (`format/rust`, `format/bazel`, `format/biome`, `format/oxc`,
   `format/prettier`, `lint/oxc`, `lint/biome`) is a declarative YAML check under
   `tools/checkleft/checks/`. Its fix runs the tool's own in-place write mode
   (rustfmt `{{file}}`, buildifier `-mode=fix`, `--write`, etc.) inside a
   `WritableSandbox` (`src/fix/safety.rs`), then detects changes by **whole-file
   content hash** and copies back **entire files** (`safety.rs:144-166, 210-240`;
   `src/external/declarative/executor.rs:1145-1148`). There is no edit range, no
   patch object, no replacement-text span anywhere on this path — the unit of
   application is the file. The fix outcome is just `applied: Vec<PathBuf>`
   (`executor.rs:860-879`).

2. **Built-in `suggested_fix` — true text-span edit.** `SuggestedFix { edits:
Vec<FileEdit> }` with `FileEdit { path, old_text, new_text }` (`src/output.rs:47-58`)
   is a line-anchored `old_text → new_text` splice (`apply_positioned_edit`,
   `runner.rs:1389-1446`), refused when ambiguous. **But declarative formatters never
   populate it** (`transform.rs:185, 273`; `executor.rs:426` set `suggested_fix:
None`). The span model exists but no formatter emits it.

No formatter is invoked with a **native range-formatting flag** today, and the
declarative arg-template engine supports only four substitution tokens —
`{{files}}`, `{{file}}`, `{{repo_root}}`, `{{config.KEY}}` (`executor.rs:706-776`) —
so there is **no way to express a line range** in the current config schema even if a
tool supported it. (rustfmt's `--file-lines` is nightly-only and unstable; this repo
pins stable rustfmt, so it is not a viable lever here regardless.)

The fix scheduler orders overlapping checks **lint → other → format**
(`src/fix/scheduler.rs:17-36`) and the runner runs up to `DEFAULT_FIX_PASSES = 10`
convergence passes (`runner.rs:85, 650-769`).

### Per-check policy schema (where a new flag goes)

The per-check `policy:` block parses into `ParsedCheckPolicyConfig`
(`src/config.rs:514-524`) and resolves to `CheckPolicyConfig` (`src/config.rs:93-101`):

```rust
pub struct CheckPolicyConfig {
    pub severity: Option<Severity>,
    pub allow_bypass: Option<bool>,
    pub bypass_name: Option<String>,
    pub stale_exclusion_mode: Option<StaleExclusionMode>,
}
```

It is flattened into `EffectiveCheckPolicy` (`runner.rs:54-64`) and consumed by
`apply_policy_to_result`. A new opt-in fits naturally as one more `policy` field,
parsed in `parse_policy_config` (`config.rs:846-887`).

---

## Recommended design

### Part A — Detection post-filter (recommended; low risk)

**Goal:** drop a line-anchored finding unless its line lies inside a PR-changed
region of its file.

**1. Produce a precise changed-line set per file.** The existing `file_diffs` hunks
over-approximate (default `-U3` context, no per-line added flags). Two ways to get
exact added-line numbers; recommend the second:

- _Option A1 (extra git call):_ run one additional `git diff --unified=0 <base_sha>
HEAD` reusing the **same** `base_sha` already on the `ChangePlan`
  (`base_revision_from_plan`, `mod.rs:194-202`), and parse it with the existing
  `parse_file_diffs_from_git_patch`. With `-U0`, each hunk's
  `new_start..new_start+new_lines` is exactly the added lines. Simple, but adds a
  second full-tree diff and a second parse.
- _Option A2 (recommended — enhance the existing parser):_ the parser at
  `patch_line_deltas.rs:92-106` already walks every `+`/`-` line; have it record the
  **post-image line number of each `+` line** (a running counter seeded from
  `new_start` at each hunk header). Store that as a compact set of changed-line
  ranges on `FileDiff` (e.g. `added_line_ranges: Vec<(u32, u32)>`). This needs no
  extra git invocation, no extra base resolution, and stays consistent with the
  changeset's own diff by construction. It is purely additive to a well-tested
  parser.

  This works on the default-context diff because we count actual `+` lines rather
  than trusting the hunk span. (Renames/new files/deletions already flow through the
  same parser; a pure deletion contributes no added lines and therefore no changed-
  line region, which is correct — there is no post-image line to anchor a finding to.)

Define a reusable helper, e.g. `ChangeSet::changed_lines(path: &Path) -> Option<&LineRanges>`,
returning the precise added-line ranges for a changed file (or `None` for files not
in the changeset — those are already dropped by the file filter).

**2. Intersection semantics.** For a finding with location `(path, line)`:

| Finding shape                                       | Rule                                                                                                                                                                                                                                 |
| --------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `location: None` (check-level / file-level)         | **Keep** — not line-anchored; mirrors existing file filter's treatment of `None`.                                                                                                                                                    |
| `line: None` (whole-file finding, e.g. `file/size`) | **Keep** — see generality; whole-file findings opt out by nature.                                                                                                                                                                    |
| `line: Some(n)`, single point                       | **Keep iff** `n ∈ changed_lines(path)`.                                                                                                                                                                                              |
| ranged finding `[start, end]` (future)              | **Keep iff** `[start,end] ∩ changed_lines(path) ≠ ∅`. Do **not** clip the reported range — report the whole finding or none. Clipping a diagnostic's span would misrepresent the tool's output and risks pointing at the wrong code. |

The "keep if it intersects at all" rule (rather than "keep only if fully inside") is
the right default: a finding that straddles a changed and unchanged line is still
_about_ something you touched. Dropping it would hide a real regression introduced on
the boundary; clipping it would lie about the span.

**3. Insertion point.** Add `scope_findings_to_changed_lines(&mut result, changeset,
policy)` in `apply_policy_to_result` immediately after `scope_findings_to_changeset`
(`runner.rs:1317`). Gate it on the per-check opt-in (Part D) so it is a no-op unless
the check opted in. Because this is the same choke point as the existing file filter,
it automatically covers terminal (incl. live streaming), JSON, SARIF, Check Run, GHA
annotations, the exit code, and the set of findings handed to the fix planner — all
consistently, with no per-sink change. Annotations already carry line ranges and are
built downstream of this filter, so they are filtered for free.

### Part B — Fix post-filter (the hard part)

Formatters rewrite whole files; you cannot run rustfmt or buildifier on a hunk, and
no formatter in checkleft's set exposes usable native range formatting. Three
strategies were evaluated.

**B1 — Whole-file format, then partial-apply only the changed-region edits
(RECOMMENDED).** Run the formatter on the whole file in the sandbox exactly as today,
then, instead of copying the whole formatted file back, compute the diff between the
**original** staged bytes and the **formatted** bytes, keep only the edit hunks that
intersect the PR-changed line regions, and apply just those hunks to the real file.

Why this is feasible here:

- The sandbox lifecycle is stage → run → `detect_changes` → `copy_back`
  (`safety.rs:8-25`). The interception point is between `detect_changes` and
  `copy_back` in `run_declarative_fix` (`executor.rs:1145-1148`).
- The diff machinery to compute and parse format edits already exists in-repo
  (`parse_file_diffs_from_git_patch`); the `FileEdit { old_text, new_text }` model
  (`output.rs:53-58`) is a natural target representation for the kept hunks; and
  `line_start_offset` (`runner.rs:1353-…`) already converts 1-based lines to byte
  offsets for splicing.
- One change required to the sandbox: today `WritableSandbox` retains only the
  **SHA-256** of each pre-fix file (`safety.rs:112-118`), not the bytes. Partial-apply
  needs the original bytes to diff against the formatted output. Retain the staged
  pre-fix content (or re-read it from the staged copy before the fixer runs) for
  files belonging to a line-scoped check.

Correctness hazards (must be handled, and bound what we can promise):

- **Reflow across the boundary.** A formatter edit may merge/split lines spanning a
  changed/unchanged boundary (e.g. rustfmt joining a short call across the last
  changed line and the first unchanged line). A format hunk that _touches_ any
  changed line should be applied **in full** (intersection, not clipping) — applying
  half of a reflow produces invalid syntax. This means a line-scoped format fix can
  still edit a few adjacent unchanged lines when an edit genuinely straddles the
  boundary; that is correct and unavoidable, and is far smaller than whole-file.
- **Context-dependent indentation.** Re-indentation of a changed line can depend on
  unchanged surrounding structure; because we run the formatter on the **whole** file
  (not a fragment), the indentation it computes is already correct in context. We are
  only _selecting which of its edits to keep_, never re-deriving formatting from a
  fragment — so this hazard is avoided by construction.
- **Adjacent-but-not-inside edits.** A format edit entirely on unchanged lines
  (pre-existing debt) is dropped — that is the whole point. Edits adjacent to but not
  intersecting a changed line are dropped; only intersecting hunks are kept.
- **Idempotency / re-check.** After a partial apply, the file is _not_ fully
  formatted, so a subsequent `format --check` on the whole file would still report it.
  The fix filter and the detection filter must use the **same** changed-line set so
  that the residual (unchanged-line) formatting findings are themselves filtered out
  of the report. The multipass loop (`runner.rs:710`) must treat "only unchanged-line
  edits remain" as converged, not loop to 10 passes trying to fully format.

**B2 — Native range formatting where supported, fall back to B1.** Inventory of
checkleft's formatters: rustfmt (`--file-lines` exists but is **nightly/unstable**;
repo pins stable → unusable), buildifier (**no** range mode), prettier
(`--range-start/--range-end`, **single contiguous range only** — cannot express the
multiple disjoint hunks a PR typically has), biome (no stable range format), oxfmt
(no range mode). Conclusion: native range formatting is a dead end for this toolset —
at best it would help one tool (prettier) for the degenerate single-hunk case. **Not
recommended** as a primary strategy; B1 is needed regardless, so build B1 and skip B2.

**B3 — Band-aids (REJECTED).** (a) Suppressing the check entirely on any partially-
touched file leaves real formatting problems on changed lines unfixed and unreported —
defeats the purpose. (b) Applying the whole-file fix anyway is the current behavior
and the exact problem we are solving. Both rejected.

**Recommendation:** implement **B1** (whole-file format → diff → partial-apply of
intersecting hunks), sharing the changed-line set with the detection filter. Do not
pursue B2.

### Part C — Generality: which checks this applies to

Line-scoping is **opt-in per check**, never global, because many checks are whole-file
or structural by nature and would be silently broken by it.

| Class                                                     | Examples                                                                                                                                                                                      | Line-scope?                                                      |
| --------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------- |
| Formatting (whole-file rewrite, line-anchorable findings) | `format/rust`, `format/bazel`, `format/biome`, `format/oxc`, `format/prettier`                                                                                                                | **Yes** (detection + fix via B1) — the motivating case           |
| Lint diagnostics (per line/column)                        | `lint/oxc`, `lint/biome`, `code_patterns`, `forbidden_imports_deps`, `frontend_no_legacy_api`, `todo_expiry`, `typo`, `workflow_action_version`, `workflow_run_patterns`, `md/link-integrity` | **Yes** (detection; fix via `suggested_fix` spans where present) |
| Whole-file size/structure                                 | `file/size` (line-count is a whole-file property; emits `line: None`), `file/ifchange`, `file/forbidden-path`                                                                                 | **No** — finding has no per-changed-line anchor                  |
| Repo/target structural                                    | `repo_visibility`, `rust_test_rule_coverage`, `workflow_shell_strict` (emit `line: None` or package-level)                                                                                    | **No**                                                           |

The `line: None` rule in the detection filter means several whole-file checks are
_already_ immune even if mis-flagged, but the opt-in is the real guard.

### Part D — Config & UX

Add one per-check `policy` field. Proposed name: `changed_lines_only` (boolean,
default `false`). It lives alongside the existing per-check opt-ins:

```yaml
# CHECKS.yaml
checks:
  - id: format/rust
    policy:
      changed_lines_only: true # report & fix only formatting on PR-changed lines
```

Plumbing: `ParsedCheckPolicyConfig` (`config.rs:514`) → `CheckPolicyConfig`
(`config.rs:93`) → parse in `parse_policy_config` (`config.rs:846`) → carry on
`EffectiveCheckPolicy` (`runner.rs:54`) → consumed by `scope_findings_to_changed_lines`
in `apply_policy_to_result` and by the B1 fix path.

Defaults and UX:

- **Default `false`** — preserves today's behavior exactly; this is a strictly
  additive opt-in. No existing repo changes behavior until it sets the flag.
- **`--all` interaction:** in `--all` mode the changeset is every file with no diff
  hunks; `changed_lines_only` must **no-op** in `--all` (treat "no hunk data" as "all
  lines changed"), mirroring how the file filter no-ops under `--all`.
- **Output:** filtered findings simply do not appear (terminal, JSON, SARIF, Check
  Run, GHA) — all flow through the one choke point, so they are consistent by
  construction. Consider a one-line summary note ("N findings on unchanged lines
  suppressed by `changed_lines_only`") so suppression is visible, not silent.
- **Annotations** already carry line ranges and are built after the filter, so they
  are filtered consistently with no extra work.

### Part E — Interaction with existing change-scoping (no conflict)

The proposed filter is strictly downstream of and narrower than the existing file
filter. Composition order in `apply_policy_to_result`:

1. `scope_findings_to_changeset` — drop findings on untouched **files** (existing).
2. `drop_excluded_findings` — drop excluded paths (existing).
3. `scope_findings_to_changed_lines` — _(new, opt-in)_ drop findings on untouched
   **lines** of touched files.

Each stage only sees what the previous kept, so there is no double-filtering and no
way for the line filter to resurrect or conflict with the file filter. The line
filter draws its hunk ranges from the _same_ `changeset.file_diffs`, produced from the
_same_ resolved `base_sha`, as everything else — closing the door on the
`Scenario::Local` stale-base defect class.

---

## Rejected alternatives

| Alternative                                                               | Why rejected                                                                                                                                                                                           |
| ------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Global "line-scope everything" toggle                                     | Breaks whole-file checks (`file/size`, `repo_visibility`); must be per-check opt-in (Part C/D).                                                                                                        |
| Native range formatting (B2) as primary                                   | Only prettier has a usable flag, and only for a single contiguous range; rustfmt's `--file-lines` is nightly/unstable and the repo pins stable; buildifier/biome/oxfmt have none. Does not generalize. |
| Suppress the check on partially-touched files (B3a)                       | Hides real findings/formatting on the lines you actually changed.                                                                                                                                      |
| Apply whole-file fix anyway (B3b)                                         | Is the current behavior — the exact problem being solved.                                                                                                                                              |
| Re-derive a fresh `--unified=0` diff against bare `main` for the line set | Reintroduces the stale-base defect that `select_base_local` guards against (`base.rs:293-304`). Must reuse the resolved `base_sha`.                                                                    |
| Clip ranged findings to the changed sub-range                             | Misrepresents the tool's diagnostic span; "keep whole finding if it intersects" is correct.                                                                                                            |
| Run the formatter on an extracted hunk fragment                           | Loses surrounding context needed for correct indentation/reflow; B1 runs the formatter whole-file and only _selects_ edits.                                                                            |

---

## Edge-case table

| Case                                                     | Detection behavior                                                           | Fix behavior                                                                         |
| -------------------------------------------------------- | ---------------------------------------------------------------------------- | ------------------------------------------------------------------------------------ |
| New file (all lines added)                               | Whole file is "changed"; every finding kept                                  | All formatter edits intersect; effectively whole-file fix (correct — it's all new)   |
| Deleted file                                             | No post-image lines; no findings to anchor (file gone)                       | No fix                                                                               |
| Renamed file, body unchanged                             | Zero added lines → no changed-line findings                                  | No format edits kept                                                                 |
| Renamed + edited                                         | Added lines from the diff (keyed by new path) drive the filter               | Only edits intersecting added lines kept                                             |
| Finding with `line: None` (whole-file, e.g. `file/size`) | Kept (whole-file findings opt out by nature)                                 | n/a (no line-scoped fix)                                                             |
| Finding with `location: None` (check-level error)        | Kept                                                                         | n/a                                                                                  |
| Finding on a context line just outside a changed hunk    | Dropped (not in added-line set)                                              | Pre-existing-debt edit dropped                                                       |
| Format reflow straddling changed/unchanged boundary      | n/a                                                                          | Whole intersecting hunk applied (may touch a few adjacent unchanged lines — correct) |
| Binary file in diff                                      | No hunks (parser skips); no line findings                                    | No fix                                                                               |
| `--all` mode                                             | `changed_lines_only` no-ops (treat as all lines changed)                     | No-ops                                                                               |
| Partial fix leaves file not-fully-formatted              | Residual unchanged-line findings filtered out by the _same_ changed-line set | Multipass treats "only unchanged-line edits remain" as converged                     |
| Two PRs touching disjoint regions of one file            | Each filters to its own hunks (base differs per run)                         | Each fixes only its own region                                                       |

---

## Phased implementation plan

**Phase 1 — Detection-only line filter (low risk, high value).**
Enhance the patch parser to record exact added-line ranges per file (Part A, Option
A2); add `ChangeSet::changed_lines`; add the `changed_lines_only` policy field and its
plumbing; add `scope_findings_to_changed_lines` in `apply_policy_to_result`; wire the
`--all` and `line: None`/`None` no-op rules. Opt the formatting/lint checks in. This
delivers the bulk of the user-visible value (no more pre-existing-debt findings on
your PR) with no changes to the fix path. **Effort: small.** Risk: low — additive,
reuses one well-tested parser and one existing choke point; fully behind an opt-in
defaulting to off.

**Phase 2 — Fix-side partial-apply (B1).**
Retain pre-fix bytes in `WritableSandbox` for line-scoped checks; between
`detect_changes` and `copy_back`, diff original-vs-formatted, keep only intersecting
hunks (sharing Phase 1's changed-line set), and apply them (via `FileEdit`/byte-offset
splice). Make the multipass loop converge on "only unchanged-line edits remain."
Extensive tests for reflow/indentation/boundary hazards. **Effort: medium → large.**
Risk: moderate — touches the safety/copy-back core and the fixer's correctness
guarantees; needs careful testing so a partial apply never produces invalid syntax and
never writes outside the fixable set.

**Sizing for planning:** Phase 1 is a single small chore and a clean standalone win —
ship it first. Phase 2 is a small stack (sandbox byte-retention + partial-apply +
multipass convergence + a test suite), best split into its own follow-up rather than
bundled with Phase 1. Recommend landing Phase 1, validating it in CI on the
`format/*` checks, then scoping Phase 2 as a separate effort.

---

## Risks

- **Phase 2 correctness:** a mis-selected hunk that splits a reflow yields invalid
  source. Mitigation: apply intersecting hunks **in full**, never clip; run the
  formatter whole-file so indentation is always context-correct; gate behind opt-in;
  heavy test coverage.
- **Base consistency:** any independent re-derivation of the base SHA risks the
  `Scenario::Local` stale-changeset defect. Mitigation: reuse `changeset.file_diffs` /
  the resolved `base_sha` exclusively (Part E).
- **User surprise from residual debt:** a line-scoped formatter leaves the file not
  fully formatted. This is intended, but should be surfaced (the summary note in Part
  D) so it is not mistaken for a bug.
- **Opt-in drift:** if a structural check is mistakenly opted in, line-scoping could
  hide real findings. Mitigation: the `line: None` keep-rule, plus documenting the
  Part C categorization next to the flag.

---

## Open questions

- Should `changed_lines_only` be settable globally as a _default_ (with per-check
  override) once the safe set is established, or remain strictly per-check? This
  document recommends per-check only until Phase 1 has soaked.
- For ranged findings (if/when annotations gain real `end_line`), confirm "intersect →
  keep whole" is the desired product behavior versus reporting a clipped range.
- Phase 2: is "leave changed lines formatted, unchanged lines untouched" acceptable as
  a permanent file state in this repo's CI, given that a later unrelated PR touching
  those lines will then reformat them? (This is the intended steady state, but worth an
  explicit product decision.)

### Follow-up code work (out of scope for this investigation PR)

This is a design document only. The implementation is captured as proposed follow-up
tasks (see the worker's structured followups): Phase 1 (detection-only line filter)
and Phase 2 (fix-side partial-apply). File them separately per the investigation
workflow.
