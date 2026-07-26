# checkleft-sandbox

[`github.com/brianduff/checkleft-sandbox`](https://github.com/brianduff/checkleft-sandbox) (private) is a small manual playground for exercising checkleft against a real Rust + Bazel project. It is a precursor to automated checkleft consumer testing, not part of that plan — use it to try things by hand (new checks, prebuilt release bumps, annotation backends), not as a substitute for checkleft's own unit/e2e suite in this repo.

## Where work on the sandbox lives

- **GitHub:** `brianduff/checkleft-sandbox`, default branch `main`.
- **Cube pool:** repo slug `checkleft-sandbox` (workspace prefix `checkleft-sandbox-agent-`).
- **Boss product:** product named `checkleft-sandbox`, remote `git@github.com:brianduff/checkleft-sandbox.git`.

File work against the `checkleft-sandbox` product (not as a per-task repo override under the Boss product — products that already own a repo reject cross-repo task overrides). Workers dispatch into the cube pool the normal way.

checkleft's **source** still lives in this monorepo at `tools/checkleft/` (see the root `AGENTS.md` note). The sandbox only **consumes** published checkleft binaries.

## How the sandbox gets checkleft

checkleft is fetched as a **prebuilt binary via [rules_multitool](https://github.com/theoremlp/rules_multitool)** (`multitool.lock.json` at the sandbox root) — not via repobin and not by building from this monorepo checkout.

- Release assets come from public `spinyfin/mono` tags named `checkleft-v*` (see [`buildkite-release-setup.md`](buildkite-release-setup.md)). Assets download anonymously over HTTPS; no GitHub token is required in the sandbox CI.
- **Bump** = edit the URL + `sha256` per platform in `multitool.lock.json` to point at a newer `checkleft-v*` release, then update any comments in `CHECKS.yaml` that mention the pin.
- The sandbox root `BUILD.bazel` must `exports_files(["multitool.lock.json"])` so the `//:multitool.lock.json` label in `MODULE.bazel` resolves.

### Local invocation

checkleft must run at the **repo root** (it detects the git work tree). Under `bazel run`, Bazel chdirs into the target's runfiles tree, so the sandbox's documented pattern wraps the run:

```sh
# Diff against main:
bazel run --run_under="cd $PWD && " @multitool//tools/checkleft -- run --base-ref origin/main

# Or everything:
bazel run --run_under="cd $PWD && " @multitool//tools/checkleft -- run --all
```

(Inside this monorepo, `bazel run //tools/checkleft -- run` is different: checkleft honours `BUILD_WORKING_DIRECTORY` so the monorepo path works without `--run_under`. The sandbox predates that convenience path and still uses the explicit `cd`.)

### CI (GitHub Actions)

`.github/workflows/checkleft.yml`:

| Event          | Command                                                                                                                             |
| -------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| pull request   | `checkleft run --base-ref origin/<base> --default-branch <base>` (needs `fetch-depth: 0` plus an explicit fetch of the base branch) |
| push to `main` | `checkleft run --all`                                                                                                               |

The workflow builds `@multitool//tools/checkleft` with Bazel, then invokes the resulting binary directly (same binary multitool would run) rather than using the local `--run_under` form.

## Toolchain pins

The sandbox mirrors a known-good flunge-style pin set so consumer friction matches a real external repo:

| Tool                   | Pin (as of the 2026-06-16 wiring) |
| ---------------------- | --------------------------------- |
| Bazel                  | 9.x (see `.bazelversion`)         |
| rules_rust             | 0.68.1                            |
| Rust edition / version | 2024 / 1.93.1                     |

Re-check `MODULE.bazel` / `.bazelversion` before assuming these are still current — they are sandbox-owned and can move independently of mono.

## CHECKS.yaml and version caveats

`CHECKS.yaml` only enables checks that exist in the **pinned** checkleft binary. When the pin is old, newer canned checks present in current mono (`format/rust`, `lint/rust`, `format/bazel`, `lint/bazel`, …) will not load until you:

1. Bump `multitool.lock.json` to a release that ships them.
2. Add the corresponding entries to `CHECKS.yaml`.

A clean `checkleft run` on an old pin only proves the pin's check set is green — not that the sandbox exercises every check mono currently ships.

## Related docs

- checkleft lives in mono: root [`AGENTS.md`](../../../AGENTS.md)
- Publishing the prebuilts the sandbox consumes: [`buildkite-release-setup.md`](buildkite-release-setup.md)
- In-source `LINT.IfChange` markers need an enabled `file/ifchange` instance: checkleft userdoc [`canned-checks.md`](../userdoc/docs/canned-checks.md#fileifchange)
