# Running checks

Use the repo wrapper:

```bash
./tools/checks <subcommand> [flags]
```

The wrapper builds and runs the checks binary (preferring Bazel, falling back to Cargo).

## Subcommands

## `run`

Run configured checks for a computed change set.

```bash
./tools/checks run [--all] [--base-ref <ref>] [--format <human|json>] [--external-checks-url <url>]
```

Flags:

- `--all`: run checks for all tracked files.
- `--base-ref <ref>`: run checks against changes since `<ref>`.
- `--format human|json`: output format (`human` default).
- `--external-checks-url <url>`: fetch an external root `CHECKS.yaml` or `CHECKS.toml` and merge it before local config resolution.

Behavior:

- Exit code is `1` if any check reports an `error` finding.
- `warning` and `info` findings do not fail the command.

## `list`

List check IDs configured for the computed change set.

```bash
./tools/checks list [--all] [--base-ref <ref>] [--external-checks-url <url>]
```

If no checks apply, output is:

```text
No configured checks found.
```

## Common workflows

Run on current local diff:

```bash
./tools/checks run
```

Run all checks locally before large refactors:

```bash
./tools/checks run --all
```

Use base ref in CI-like flows:

```bash
./tools/checks run --base-ref main --format=json
```

## PR workflow integration

Use `./tools/create-pr` for `gh pr create` and `gh pr edit` paths.

- It runs `./tools/checks run` first.
- Emergency local bypass for this pre-PR gate:

```bash
FLUNGE_SKIP_CHECKS=1 ./tools/create-pr create ...
```

`FLUNGE_SKIP_CHECKS=1` skips the preflight check run in `tools/create-pr`; it does not disable CI checks.
