# Checkleft: Evolving `forbidden-paths`

## Overview

Checkleft should evolve the existing `forbidden-paths` built-in check from a
single flat list of path globs into a rule-based path-policy check that can
differentiate by change kind.

The current implementation is already close to the desired behavior:

- it evaluates repository-relative file paths,
- it uses familiar glob-style matching,
- it ignores deleted files and reports path-based findings consistently.

What is missing is policy structure:

- different reasons for different forbidden patterns,
- support for "only forbid this on add" or "only forbid this on delete",
- a way to group multiple patterns under one reason.

Rather than adding a second overlapping check, we should make this an evolution
of `forbidden-paths`.

## Goals

- Keep one built-in check for path-based repository policy.
- Support rule-level reasons so findings explain why a path is forbidden.
- Support multiple patterns per reason.
- Allow rules to scope themselves to `added`, `modified`, `deleted`, and
  `renamed` changes.
- Continue using gitignore/glob-style path matching rather than regex.
- Preserve Checkleft's existing severity and bypass policy model.

## Non-Goals

- Regex matching in v1 of the redesign.
- Content-based inspection of files.
- A separate filename-only matcher.
- Any framework-wide API change.

Filename-specific policy can still be expressed with path patterns such as
`**/*.swp` or `**/package-lock.json`, which keeps the model smaller and more
consistent.

## Why Evolve The Existing Check

Keeping this within `forbidden-paths` is the better design because the policy
surface is fundamentally the same: a changed path should or should not be
present in a change.

Adding a second check for "forbidden new files" would create unnecessary
overlap:

- both checks would inspect the same `changed_files` input,
- both would evaluate path patterns,
- both would emit nearly identical findings,
- authors would have to choose between two similar configuration styles.

A single rule-based check is easier to explain and easier to adopt.

## Proposed User-Facing Model

### Check Configuration

Keep the built-in check name as `forbidden-paths`.

YAML example:

```yaml
checks:
  - id: forbidden-paths
    policy:
      severity: error
      allow_bypass: true
    config:
      remediation: Remove the path from the change or move it to an approved location.
      rules:
        - reason: Generated outputs must not be checked in.
          when: [added]
          patterns:
            - "**/dist/**"
            - "**/build/**"
            - "backend/generated/**"

        - reason: Editor scratch files do not belong in the repo.
          when: [added, modified]
          patterns:
            - "**/*.swp"
            - "**/*~"
            - "**/.DS_Store"

        - reason: Do not remove compatibility config.
          when: [deleted]
          patterns:
            - "backend/legacy/config.toml"
```

Proposed config schema:

- `rules` (new preferred array)
- `remediation` (optional string)

Per-rule keys:

- `reason` (required string)
- `when` (required array of `added|modified|deleted|renamed`)
- `patterns` (required array of gitignore/glob-style strings)

### Matching Semantics

Patterns should be matched against the repository-relative path, using the same
globset-based style the existing check already uses.

Implications:

- exact path matches are written as literal paths,
- subtree matches are written as globs such as `backend/generated/**`,
- filename-anywhere matches are written as globs such as `**/package-lock.json`
  or `**/*.swp`.

This keeps the config compact and avoids a second axis of target selection like
"path vs filename".

### Change-Kind Semantics

Each rule should declare the change kinds it applies to.

Supported values:

- `added`
- `modified`
- `deleted`
- `renamed`

Interpretation:

- a rule with `when: [added]` blocks newly introduced matching paths,
- a rule with `when: [modified]` blocks edits to matching paths,
- a rule with `when: [deleted]` blocks removal of matching paths,
- a rule with `when: [renamed]` blocks renames of matching paths.

For renamed files, matching should check both the current path and the old path
if available. That avoids missing policies that care about the source location
or the destination location of the rename.

### Findings

Each matching rule should yield at most one finding per file, even if multiple
patterns in that rule match.

Suggested message shape:

```text
path `frontend/dist/app.js` is forbidden for added changes: Generated outputs must not be checked in. (matched `**/dist/**`)
```

Location should point at the relevant changed path, with no line number.

Severity should continue to follow normal Checkleft policy resolution:

- default severity: `error`,
- per-instance override via `[checks.policy].severity`,
- normal bypass handling via `[checks.policy].allow_bypass`.

## Config Validation

The new rule-based config should fail validation for:

- missing or empty `rules`,
- a rule with an empty `reason`,
- a rule with an empty `when`,
- a rule with no `patterns`,
- an invalid change-kind value,
- invalid glob syntax.

## Implementation Sketch

This feature should extend the existing module rather than add a new one:

```text
tools/checkleft/src/checks/forbidden_paths.rs
```

Proposed internal direction:

```rust
struct ForbiddenPathsConfig {
    rules: Vec<ForbiddenPathRule>,
    exclude_globs: Vec<String>,
    remediation: Option<String>,
    severity: Option<String>,
}

struct ForbiddenPathRule {
    reason: String,
    when: Vec<ConfiguredChangeKind>,
    patterns: Vec<String>,
}

enum ConfiguredChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}
```

Compilation should produce:

- a compiled globset per rule,
- a change-kind filter per rule,
- optional compiled exclusions,
- default remediation and severity values.

Check flow:

1. Parse config.
2. Iterate `changeset.changed_files`.
3. Skip a file only when no configured rule applies to its `ChangeKind`.
4. Apply `exclude_globs` consistently to the relevant path under evaluation.
5. For each matching rule, emit one finding containing:
   - the configured reason,
   - the matched pattern,
   - optional remediation text.

For `renamed` files, evaluate both:

- `changed_file.path`,
- `changed_file.old_path`, when present.

## Testing Expectations

Unit tests should cover:

- `added` rules only fire on added files,
- `modified` rules only fire on modified files,
- `deleted` rules can now fire,
- `renamed` rules check old and new path values,
- multiple patterns under one reason,
- multiple reasons matching one file,
- invalid config cases.

Runner-level tests are useful to confirm YAML config shape, policy overrides,
and bypass behavior work end-to-end.

## Open Questions

- Whether `exclude_globs` should remain top-level only, or also be allowed
  per-rule in a later iteration.
- Whether the legacy `severity` field should remain supported long-term or be
  documented as deprecated in favor of `[checks.policy].severity`.
