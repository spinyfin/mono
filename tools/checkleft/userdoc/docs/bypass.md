# Bypassing checks

This page describes bypass support for checks that opt into bypass.

## Scope

- Bypass is opt-in per check.
- By default, checks do not allow bypass.
- In the current default config, bypass is enabled for:
  - `api-breaking-surface`
  - `no-usfa-typo` (directive name: `BYPASS_NO_USFA_TYPO`)

```toml
[[checks]]
id = "api-breaking-surface"

[checks.config]
allow_bypass = true

[[checks]]
id = "no-usfa-typo"
check = "typo"

[checks.config]
allow_bypass = true
bypass_name = "BYPASS_NO_USFA_TYPO"
```

## Directive format

Use a single-line directive in commit or PR description:

```text
BYPASS_<CHECK_NAME>=<specific legitimate reason>
```

For `api-breaking-surface`:

```text
BYPASS_API_BREAKING_SURFACE=No public API behavior changed; docs update would be misleading.
```

For `no-usfa-typo`:

```text
BYPASS_NO_USFA_TYPO=Legacy upstream terminology is intentionally retained in this change.
```

## Where directives are read from

Checks parse directives from:

- current commit description
- PR description

If both contain the same bypass name, PR description wins.

## Behavior when bypass applies

When a check has `allow_bypass = true` and a matching directive with non-empty reason exists:

- the normal failure is bypassed
- the check emits a `warning` finding recording bypass use and reason

This keeps bypass use visible in output and CI logs.

## Behavior when bypass is enabled but not used

If policy fails and bypass is enabled but no directive exists:

- normal policy failure is emitted
- remediation text includes bypass instructions and warns against convenience bypasses

## CI/environment context

The checks CLI can resolve PR description context from:

- `CHECKS_PR_DESCRIPTION` (explicit text)
- `CHECKS_CHANGE_ID` or `CHECKS_PR_NUMBER`
- `CHECKS_REPOSITORY`
- GitHub token from `CHECKS_GITHUB_TOKEN` (or `GH_TOKEN` / `GITHUB_TOKEN`)

In Buildkite, `.buildkite/scripts/run_checks.sh` wires `CHECKS_PR_NUMBER` for PR builds. GitHub auth is read from `GITHUB_TOKEN` / `GH_TOKEN` (or `CHECKS_GITHUB_TOKEN` if set).

## Policy guidance

Bypasses are for rare, legitimate exceptions with concrete rationale.

Do not use bypasses to skip required work for convenience.
