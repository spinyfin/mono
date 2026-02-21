# Checkleft JS Componentizer

This directory contains the pinned JavaScript toolchain used by `checkleft` to
build external checks in `source` mode (`language = "javascript"` and
`build_adapter = "javascript-component"`).

`checkleft` runs:

1. `corepack pnpm install --frozen-lockfile` (cached by lockfile hash)
2. `node scripts/build_check.mjs --repo-root ... --entry ... --out ...`

The build script bundles the check entrypoint and componentizes it with `jco`.
