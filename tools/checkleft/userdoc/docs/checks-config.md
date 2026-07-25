# Configuring checks

`CHECKS.yaml` (preferred) or `CHECKS.toml` defines which checks run and how each check is configured. Both formats are equivalent; YAML is the recommended choice for new repos.

## File location and hierarchy

- Put a root `CHECKS.yaml` at repo root for default policy.
- Add child `CHECKS.yaml` files in subdirectories for scoped overrides.
- For a changed file, checks are resolved from root to that file's directory.
- Child entries override parent entries when `id` is the same.

## Top-level structure

```yaml
# Root CHECKS.yaml (applies repo-wide unless overridden in child directories).
# Each checks entry defines one configured check instance.
checks:
  - id: file/size

    # Optional; defaults to true.
    enabled: true

    # Check-specific config passed to the file/size implementation.
    config:
      max_lines: 500

    # Optional shared policy controls (applied by the framework, not check code).
    policy:
      severity: error
      allow_bypass: true
      bypass_name: BYPASS_FILE_SIZE
```

TOML is also supported:

```toml
# Root CHECKS.toml
[[checks]]
id = "file/size"
enabled = true

[checks.config]
max_lines = 500

[checks.policy]
severity = "error"
allow_bypass = true
bypass_name = "BYPASS_FILE_SIZE"
```

## `settings`

Supported keys:

- `include_config_files` (boolean, default `false`)
- `external_checks_url` (string, root config only)

When `false`, changed `CHECKS.yaml` / `CHECKS.toml` files are excluded from check scheduling.

When `external_checks_url` is set in the repository root config, `checkleft`
fetches that remote `CHECKS.yaml` or `CHECKS.toml`, applies it first, and then
merges the local root config and any child configs on top.

## `check_definitions`

Optional top-level section controlling where first-party check definitions are loaded from.

```yaml
check_definitions:
  exec_paths:
    - tools/checkleft/checks # relative dir(s) containing check definition yaml files
  allow_override_bundled: true
```

Supported keys:

- `exec_paths` (list of strings): relative directories to search for check-definition yaml files. Each definition lives at `<dir>/<name>/check.yaml`. Inherited by child configs.
- `allow_override_bundled` (boolean, default `false`): when `true`, a definition found in `exec_paths` with the same name as a bundled definition takes precedence over the bundled copy. When `false` (the default), bundled definitions win.

`exec_paths` is not allowed in remotely-fetched external configs — a path source would reach into the consuming repo's local filesystem.

**Default behavior (no `check_definitions` section):** a check whose `id` (or `check`) matches the name of a first-party bundled definition resolves to that bundled def automatically — no `implementation:` line needed, no install required.

## `checks` entry

Supported keys:

- `id` (required): check instance ID used in output.
- `check` (optional): check definition name; defaults to `id`.
- `implementation` (optional): explicit external package reference — `generated:<id>` or a checked-in manifest path. For first-party (bundled) and exec-path checks, omit this field; resolution is automatic from the `id`/`check` name.
- `enabled` (optional, default `true`): disable with `false`.
- `config` (optional table): check-specific configuration.
- `policy` (optional table): framework-managed severity/bypass controls.
- `scope` (optional `files|changeset`, default `files`): what the check's scheduling is keyed on. See [Changeset-scope checks](#changeset-scope-checks).

`policy` keys:

- `severity` (optional `error|warning|info`): overrides finding severity for the check instance.
- `allow_bypass` (optional boolean): enables BYPASS directives for the check instance.
- `bypass_name` (optional string): directive name; defaults to `BYPASS_<ID>` if omitted.
- `changed_lines_only` (optional boolean, default `false`): restrict this check's findings to PR-changed lines. See [Line-scoped findings](#line-scoped-findings).

## Changeset-scope checks

Most checks look at individual files: `scope: files` (the default) schedules a check once per distinct configuration and runs it only against the changed files that resolved to it.

A check with `scope: changeset` has no file to point at — it inspects the changeset as a whole, for example the PR description or the commit message. Set `scope: changeset` on such a check and it is scheduled exactly once per invocation, regardless of the changed-file set (including a changeset with no file changes, or one where every changed file is excluded):

```yaml
checks:
  - id: boss/no-boss-isms
    scope: changeset
```

Because there is no changed file to key directory-scoped config resolution on, a `scope: changeset` check must be declared in a `CHECKS` file that is the repo root (or an ancestor `external_checks_url` config) — a subdirectory `CHECKS.yaml` override is not consulted for these checks.

A `scope: changeset` check that finds something has no file to attach the finding to, so it should emit a **locationless finding** (`location: None` / no `location` field), optionally naming which non-file surface it refers to via the `surface` field (`pr-description` or `commit-message`) so the terminal and GitHub check-run summary render `<pr description>` / `<commit message>` instead of `<unknown>`. Locationless findings are preserved by the framework — they still fail the build, still print in the terminal and `--format=json` output, and their check id and message are appended beneath the counts line in the GitHub check-run summary. They are the only shape available for a non-file subject: unlike a synthetic path (e.g. `<pr-description>`), a real file path is what every other output surface (SARIF, inline check-run annotations, GHA workflow commands) requires, so a synthetic one would be silently dropped before reaching any of them.

## Line-scoped findings

Change-scoping is file-granular by default: a check only runs against — and only reports findings on — the files a PR (or working tree) changed. It does not, by default, restrict findings any further to the specific lines that changed within those files; a content check enabled over whole file contents will flag pre-existing violations anywhere in a touched file, not just on the lines the PR actually added.

Set `policy.changed_lines_only: true` on a check instance to additionally drop any line-anchored finding whose line falls outside the PR-changed (added) lines of its file:

```yaml
checks:
  - id: text/forbidden-pattern
    policy:
      changed_lines_only: true
```

This is a strict narrowing on top of the existing file-level scoping, applied at the same choke point that produces every other finding-derived output (terminal, `--format=json`, SARIF, GitHub Check Run / inline annotations, the exit code, and the fix planner's finding set) — so a check opting in is filtered consistently everywhere, with no per-sink special-casing.

Keep/drop rules:

- A finding with no `location` (check-level / `scope: changeset` findings) is always kept — it isn't anchored to a file, let alone a line.
- A finding with a `location` but no `line` (a whole-file finding, e.g. `file/size`'s line-count violation) is always kept — line-scoping only narrows line-anchored findings, and a whole-file property has no changed-line region to test against.
- A finding with `location.line: Some(n)` is kept iff `n` lies inside one of the file's PR-added line ranges (the file's diff hunks, precisely — not the wider `-U3` context git includes around each hunk).
- If the changeset carries no diff data for a file at all (`--all` / whole-repo mode, which never computes hunks), `changed_lines_only` is a no-op for that file — there is no changed-line data available, so every finding is kept, exactly like the file-level scope check's own `--all` no-op.
- A file present in the changeset but with zero added lines (for example, a rename with no content changes) legitimately produces no changed-line findings for that file when `changed_lines_only` is set — that is not a bug.

`changed_lines_only` defaults to `false`, so a check that does not set it behaves exactly as before — this is a strictly opt-in, additive filter. It only applies to the _detection_ side: fix application is unaffected, and a whole-file fix (e.g. a formatter rewrite) still rewrites the whole file even when its findings are filtered down to changed lines.

## Excluding files from checks

The `exclude` key provides a unified, framework-enforced way to tell checkleft "do not run any check (or a specific check) on these paths." It works across all check kinds — declarative, built-in Rust, and WASM component — and operates at two layers: repo-wide global and per-check.

### Global excludes

A top-level `exclude` key, sibling to `checks:`, `settings:`, and `check_definitions:`, declares paths that no check should ever run on. Common uses: vendored trees, generated files, lock files.

```yaml
# Root CHECKS.yaml
exclude:
  - "mobile/ios/vendor/**" # vendored: never check, by any check
  - "**/*.generated.*"
  - "Cargo.lock"
  - "MODULE.bazel.lock"

checks:
  - id: format/oxc
  - id: file/size
    config:
      max_lines: 3000
```

### Per-check excludes

An `exclude` key on a check entry — sibling to `config:`, `policy:`, and `enabled:` — narrows the exclusion to that one check instance. Use this when a path should be checked by most checks but not by a specific one (for example, test-reference files that must not be reformatted but should still be flagged for size or lint violations).

```yaml
checks:
  # Don't format these three reference files with oxc, but still check
  # everything else about them.
  - id: format/oxc
    exclude:
      - "frontend/testdata/report-*.reference.html"

  - id: file/size
    config:
      max_lines: 3000
```

### Canonical name and aliases

The canonical key name is `exclude`. The aliases `exclude_files` and `exclude_globs` are also accepted for backward compatibility, both at the top level and inside a check entry. All three names are equivalent — choose `exclude` for new configuration.

Checks that historically placed their exclusion list inside the `config` block (e.g. `file/size`'s `exclude_files` key) continue to work unchanged; the framework reads from both positions and merges them into one matcher. For new configuration, prefer the framework-level position (sibling to `config:`, not inside it).

```yaml
checks:
  # Legacy position — still honored, now framework-enforced.
  - id: file/size
    config:
      max_lines: 3000
      exclude_files:
        - "**/*.md"
        - "**/*.lock"

  # Preferred position — equivalent, and works for all check kinds.
  - id: file/size
    exclude:
      - "**/*.md"
      - "**/*.lock"
    config:
      max_lines: 3000
```

### Glob syntax

Each entry is a glob string using globset syntax. `**` matches across directory boundaries. Globs are authored relative to the `CHECKS.yaml` file that declares them and are normalized to repo-root paths for matching.

An empty list is rejected — use `enabled: false` to disable a check entirely.

### Precedence vs `applies_to`

Excludes are **subtractive and always win**: they apply as a second stage after positive file selection (`applies_to` for declarative checks; the intrinsic changed-file set for other check kinds). Whatever the positive selection produces, the effective file set subtracts any excluded paths.

```
effective(check, file) = positive(check, file) AND NOT excluded(check, file)
```

Because exclusion is a separate key from `applies_to`, a per-repo `applies_to` override — which replaces the definition's positive list entirely — cannot accidentally erase the repo's excludes, and the excludes cannot be defeated by a retarget.

### Inheritance through the `CHECKS` hierarchy

**Global excludes accumulate (union) down the hierarchy.** The effective global exclude set for any directory is the union of every ancestor `CHECKS.yaml`'s top-level `exclude`. A child config can only add more excludes — it cannot re-enable checking of a path that a parent excluded.

**Per-check excludes follow the check entry.** A per-check `exclude` is part of the check entry's identity. When a child `CHECKS.yaml` redefines a check (same `id`), the child's entry fully replaces the parent's — including its per-check excludes. This is consistent with how the rest of a check entry inherits.

Remote root configs fetched via `external_checks_url` participate in the same global-exclude union, applied first, with the local root and child configs unioning on top.

### Behavior guarantees

The framework enforces exclusion at two points, covering all check kinds uniformly:

1. **Selection-time subtraction.** Before a check runs, excluded paths are removed from the file set the check will operate on. For declarative checks, excluded files never reach the `{{files}}` argument list — so they are neither checked nor reformatted on `fix`. For programmatic and component checks, the host lowers a pre-filtered changeset into the check so the guest never sees excluded paths.

2. **Finding-location post-filter.** After a check returns, any finding whose `location.path` is excluded for that check instance is dropped. This provides a uniform guarantee — "no findings on an excluded path" — regardless of check kind, including future or third-party checks that might derive paths independently.

Together these guarantees mean excluded paths:

- do not trigger check execution
- are never reformatted on `fix`
- never produce findings

### Relationship to `bypass` and tool-native ignores

**`exclude` vs `bypass`**: use `exclude` for permanent, path-based out-of-scope declarations (vendored trees, generated files). Use `bypass` for a one-off, logged exception on a path that is normally in scope. They can coexist on the same check instance — an excluded path is silently out of scope; a bypassed path ran, failed, and was excepted with a recorded reason.

**`exclude` vs tool-native ignore files** (`.prettierignore`, `.gitignore`): the framework `exclude` is the authoritative mechanism. Because checkleft passes files explicitly to declarative check tools, whether a tool honors its own ignore file for explicitly-passed arguments is tool-specific and not guaranteed. Framework excludes work uniformly regardless of which tool a check wraps and do not depend on any ignore file on disk.

## Overriding `applies_to` for declarative checks

Declarative checks (format/bazel, format/rust, format/prettier, lint/js, lint/rust, etc.) declare which files they run on via an `applies_to` glob list in their check definition. A consuming repo can restrict or retarget that file set from its CHECKS.yaml without forking the definition — by setting `applies_to` inside the per-check `config` block.

**Semantics:** the repo's `applies_to` list **replaces** the definition's list entirely (it does not merge or extend). This is the simplest model and matches the word "override": whatever the definition declared, the repo wins. There is one `applies_to` vocabulary (positive globs) used in both the definition and the override.

```yaml
checks:
  # Run format/prettier only on frontend source, not on the whole repo.
  - id: format/prettier
    config:
      applies_to:
        - "frontend/**"

  # Run format/rust only under the tools/ subtree.
  - id: format/rust
    config:
      applies_to:
        - "tools/**/*.rs"
        - "lib/**/*.rs"
```

Rules:

- The override must be a non-empty list of glob strings. An empty list is rejected — use `enabled: false` to disable the check entirely.
- Each glob uses the same syntax as the check definition's `applies_to` (globset patterns; `**` matches across directory boundaries).
- When no `applies_to` key appears in `config`, the definition's own list is used unchanged.
- The override applies to all declarative checks uniformly — it is a framework feature, not specific to any one check.

## Pattern: First-party (bundled) check — zero install

First-party checks whose definitions ship inside the `checkleft` binary resolve automatically from `id` (or `check`). No `implementation:` line needed:

```yaml
checks:
  - id: format/bazel
```

With a custom instance ID:

```yaml
checks:
  - id: my-bazel-format
    check: format/bazel
```

## Pattern: Always-head definitions from disk (e.g. mono)

A repo that maintains its own check definitions on disk can point `exec_paths` at them. With `allow_override_bundled: true`, the on-disk copy takes precedence over the bundled snapshot — so the repo always runs its checked-in (head) version:

```yaml
check_definitions:
  exec_paths:
    - tools/checkleft/checks
  allow_override_bundled: true

checks:
  - id: format/bazel # resolves to tools/checkleft/checks/format/bazel.yaml
```

## Pattern: Multiple instances of one implementation

You can instantiate the same implementation more than once by using unique IDs with `check: ...`.

```yaml
checks:
  - id: forbidden-generated-outputs
    check: forbidden-paths
    config:
      rules:
        - remediation: "Generated outputs must not be checked in. Remove them from the change."
          when: [added, modified, renamed]
          patterns: ["**/target/**", "**/node_modules/**"]

  - id: forbidden-ios-build-dir
    check: forbidden-paths
    config:
      rules:
        - remediation: "iOS build directories must not be checked in. Remove them from the change."
          when: [added, modified, renamed]
          patterns: ["mobile/ios/.build/**"]
```

## Pattern: Repo-local external check from a generated index

```yaml
checks:
  - id: frontend-no-legacy-api
    check: frontend-no-legacy-api
    implementation: generated:frontend-no-legacy-api
```

Generated implementations are resolved through the configured generated index,
for example from a Bazel-produced `check_index` target.

## Pattern: Disable a parent check in a child directory

Root `CHECKS.yaml`:

```yaml
checks:
  - id: file/size
```

`backend/generated/CHECKS.yaml`:

```yaml
checks:
  - id: file/size
    enabled: false
```

## Validation notes

- Unknown `check` implementation IDs produce an error finding.
- Invalid check config shapes are surfaced as check execution errors.
- Invalid `policy.severity` values fail config resolution.
