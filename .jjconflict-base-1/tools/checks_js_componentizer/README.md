# Checkleft JS Componentizer

This directory contains the pinned JavaScript toolchain used by `checkleft` to
build external checks in `source` mode (`language = "javascript"` or
`language = "typescript"`; the build adapter is derived from language).

`checkleft` runs:

1. `corepack pnpm install --frozen-lockfile` (cached by shared toolchain state)
2. `node scripts/build_check.mjs --repo-root ... --entry ... --out ...`

The build script bundles the check entrypoint and componentizes it with `jco`.

`checkleft` copies the checked-in toolchain inputs from this directory into a
per-user cache root before installing dependencies. Derived state
now lives under:

- `${XDG_CACHE_HOME:-$HOME/.cache}/checkleft/toolchains/js-componentizer/toolchains/<toolchain-hash>/`
- `${XDG_CACHE_HOME:-$HOME/.cache}/checkleft/repos/<repo>-<repo-hash>/source-mode/artifacts/<build-hash>/`

That cache is disposable. Removing the matching toolchain or repo artifact
directories forces `pnpm install` and source-mode rebuilds on the next run.
