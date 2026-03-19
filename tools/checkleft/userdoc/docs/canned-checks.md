# Built-in checks

This page documents the built-in check implementations currently registered in the checks binary.

## `api-breaking-surface`

Purpose:

- Flags configured backend API-surface changes unless companion docs/spec files are also updated.

Config keys:

- `trigger_globs` (required, array of glob strings)
- `required_globs` (required, array of glob strings)
- `message` (optional string)
- `remediation` (optional string)

Notes:

- Findings default to `error`. Override per instance with `[checks.policy].severity`.
- Enable bypass per instance with `[checks.policy].allow_bypass` (see [Bypass mechanism](bypass.md)).

## `docs-link-integrity`

Purpose:

- Validates internal markdown links in changed `*.md` files anywhere in the repo.

Config keys:

- None.

Notes:

- External URLs (`http`, `https`, `mailto`, `tel`) and same-page anchors are ignored.
- Severity is `warning`.

## `file-size`

Purpose:

- Flags files exceeding a max line count.

Config keys:

- `max_lines` (optional integer, default `500`)
- `exclude_globs` (optional array of glob strings)

Notes:

- Findings default to `warning`. Override per instance with `[checks.policy].severity`.
- Enable bypass per instance with `[checks.policy].allow_bypass`.

## `forbidden-imports-deps`

Purpose:

- Flags line-level matches for forbidden import/dependency regex patterns.

Config keys:

- `rules` (required array)

Per-rule keys:

- `pattern` (required regex string)
- `message` (required string)
- `include_globs` (optional array of globs)
- `exclude_globs` (optional array of globs)
- `severity` (optional `error|warning|info`)
- `remediation` (optional string)

Top-level defaults:

- `severity` (optional default for rules, default `error`)
- `remediation` (optional default for rules)

## `forbidden-paths`

Purpose:

- Flags changed file paths matching rule-scoped forbidden globs.

Config keys:

- `rules` (required array)
- `exclude_globs` (optional array of glob strings)
- `severity` (optional `error|warning|info`, default `error`)
- `remediation` (optional string)

Per-rule keys:

- `remediation` (required string)
- `when` (required array of `added|modified|deleted|renamed`)
- `patterns` (required array of glob strings)

Notes:

- Rules match repository-relative paths, so filename policies can use patterns like `**/*.swp` or `**/package-lock.json`.
- Findings default to `error`. Override per instance with `[checks.policy].severity`.
- Enable bypass per instance with `[checks.policy].allow_bypass`.

## `frontend-no-legacy-api`

Purpose:

- Prevents frontend imports from deprecated module suffixes.

Config keys:

- `legacy_modules` (required array of module suffixes)
- `severity` (optional `error|warning|info`, default `error`)
- `remediation` (optional string)

## `ifchange-thenchange`

Purpose:

- Enforces `LINT.IfChange` / `LINT.ThenChange` contracts so linked files or
  linked blocks change together.

Config keys:

- None.

Syntax:

```text
LINT.IfChange
LINT.IfChange(label)
LINT.ThenChange(path)
LINT.ThenChange(path:label)
```

Notes:

- Directives should live on their own lines inside normal source comments.
- `ThenChange(path)` requires any change to the linked file.
- `ThenChange(path:label)` requires a touched `LINT.IfChange(label)` block in
  the linked file.
- Severity defaults to `error`.
- Use normal per-instance bypass policy if you need an exception.

## `rust-test-rule-coverage`

Purpose:

- Requires new Rust test files to be in packages with a Bazel `rust_test(...)` rule.

Config keys:

- None.

Severity:

- `error` by default; can be overridden per instance with `[checks.policy].severity`.

## `todo-expiry`

Purpose:

- Requires `TODO`/`FIXME` comments to include owner and date metadata.

Config keys:

- `required_pattern` (optional regex string)
- `severity` (optional `error|warning|info`, default `warning`)
- `remediation` (optional string)

Default accepted format:

```text
TODO(@owner,YYYY-MM-DD): ...
FIXME(@owner,YYYY-MM-DD): ...
```

## `typo`

Purpose:

- Flags configured terminology typos in changed files.

Config keys:

- `rules` (required array)

Per-rule keys:

- `typo` (required string)
- `canonical` (required string)
- `kind` (optional `word|substring`, default `word`)
- `guidance` (optional string)

Severity:

- `error`.

## `workflow-action-version`

Purpose:

- Enforces configured `uses:` action version pins in GitHub workflow files.

Config keys:

- `rules` (required array of `{ action, version }`)
- `severity` (optional `error|warning|info`, default `error`)
- `remediation` (optional string)

## `workflow-run-patterns`

Purpose:

- Flags GitHub workflow `run:` scripts that match configured regex rule patterns.

Config keys:

- `rules` (required array)

Per-rule keys:

- `pattern` (required regex string)
- `message` (required string)
- `must_include` (optional array of string tokens)
- `severity` (optional `error|warning|info`)
- `remediation` (optional string)

Top-level defaults:

- `severity` (optional default for rules, default `error`)
- `remediation` (optional default for rules)

## `workflow-shell-strict`

Purpose:

- Requires multi-line workflow `run:` scripts to begin with `set -euo pipefail`.

Config keys:

- None.

Severity:

- `error`.
