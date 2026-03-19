# checkleft

Status: experimental / under active development. Not yet recommended for
general use. The CLI behavior, built-in checks, and library API may change
without notice.

`checkleft` is a repository convention checker. It runs built-in and external
checks against the files in a source tree and reports findings as human-readable
output or JSON.

## Install

```bash
cargo install checkleft
```

## Usage

Run from the root of a Git or Jujutsu repository:

```bash
checkleft run
checkleft run --verbose
checkleft run --all
checkleft run --external-checks-url https://example.com/CHECKS.yaml
checkleft list
```

`checkleft` looks for `CHECKS.yaml` or `CHECKS.toml` files from the repository
root down to the file being evaluated.

The root config can also set `settings.external_checks_url` to merge an
externally hosted root config before applying local root and child overrides.
The CLI flag `--external-checks-url` provides the same behavior for repos that
do not yet have a root config file.

## Minimal config

```yaml
checks:
  - id: typo
    check: typo
```

## Notes

- `checkleft` shells out to `git` or `jj` to discover repository state.
- Some built-in checks are specific to Bazel- or monorepo-style workflows.
