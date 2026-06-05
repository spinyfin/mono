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
  - id: file-size

    # Optional; defaults to true.
    enabled: true

    # Check-specific config passed to the file-size implementation.
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
id = "file-size"
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
- `check_def_source` (string): default definition source for bare `implementation` names. Either `bundled` (use the copy embedded in the `checkleft` binary) or a relative directory path (resolve `<dir>/<name>/check.yaml`). Inherited by child configs.

When `false`, changed `CHECKS.yaml` / `CHECKS.toml` files are excluded from check scheduling.

When `external_checks_url` is set in the repository root config, `checkleft`
fetches that remote `CHECKS.yaml` or `CHECKS.toml`, applies it first, and then
merges the local root config and any child configs on top.

`check_def_source` lets a config choose where check *definitions* come from
without rewriting every check entry. Set it to `bundled` and a target repo gets
the first-party checks with zero install (the manifests ship inside the
`checkleft` binary). Set it to a path such as `tools/checkleft/checks` to use the
checked-in, always-head definitions on disk. A per-check `source` (below)
overrides it. In a remotely-fetched external config only `bundled` is permitted —
a path source would reach into the consuming repo's local filesystem.

## `checks` entry

Supported keys:

- `id` (required): check instance ID used in output.
- `check` (optional): implementation ID; defaults to `id`.
- `implementation` (optional): external package reference. Either an explicit reference — a checked-in manifest path, `generated:<id>`, or `bundled:<name>` — or, when a `source` / `check_def_source` directive is in effect, a bare definition name resolved against that source.
- `source` (optional): per-check override of `settings.check_def_source` for a bare `implementation` name (`bundled` or a relative directory path). Cannot be combined with an explicit `implementation` reference.
- `enabled` (optional, default `true`): disable with `false`.
- `config` (optional table): check-specific configuration.
- `policy` (optional table): framework-managed severity/bypass controls.

`policy` keys:

- `severity` (optional `error|warning|info`): overrides finding severity for the check instance.
- `allow_bypass` (optional boolean): enables BYPASS directives for the check instance.
- `bypass_name` (optional string): directive name; defaults to `BYPASS_<ID>` if omitted.

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

## Pattern: Bundled (zero-install) first-party check

A target repo with no checkleft definition files on disk can still run a
first-party check whose definition ships inside the `checkleft` binary:

```yaml
checks:
  - id: buildifier-declarative
    implementation: bundled:buildifier
```

To switch many checks between the embedded copies and an on-disk, always-head
checkout at once, set the source once and reference bare definition names:

```toml
# bundled everywhere (zero install)
[settings]
check_def_source = "bundled"

[[checks]]
id = "buildifier-declarative"
implementation = "buildifier"
```

```toml
# mono: use the checked-in (head) definitions on disk instead
[settings]
check_def_source = "tools/checkleft/checks"

[[checks]]
id = "buildifier-declarative"
implementation = "buildifier"
```

Both forms resolve definition `buildifier` (i.e. `<source>/buildifier/check.yaml`,
or the embedded equivalent). Only the `check_def_source` line changes.

## Pattern: Disable a parent check in a child directory

Root `CHECKS.yaml`:

```yaml
checks:
  - id: file-size
```

`backend/generated/CHECKS.yaml`:

```yaml
checks:
  - id: file-size
    enabled: false
```

## Validation notes

- Unknown `check` implementation IDs produce an error finding.
- Invalid check config shapes are surfaced as check execution errors.
- Invalid `policy.severity` values fail config resolution.
