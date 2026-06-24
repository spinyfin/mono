# repobin

`repobin` is a standalone command dispatcher that lets a repository expose its
Bazel-built binaries as ordinary commands on your `PATH`. You install one small
shim per tool; invoking a shim from inside the repo finds the repo-declared
Bazel target, builds it, and `exec`s the freshly built binary — so you always
run head-of-tree without remembering target labels or `bazel run` incantations.

Status: experimental / under active development. The CLI behavior and config
format may change without notice.

## Architecture

A single `repobin` binary plays two roles, chosen by `argv[0]`. Installed as a
symlink named after a configured tool (`boss`, `cube`, …) it acts as a
dispatcher; invoked as `repobin` it exposes the `install`, `doctor`, `list`,
and `exec` subcommands.

Dispatch resolves a tool in layers. First it walks up from the working
directory looking for a repo-root `REPOBIN.toml`. A matching `[tools]` entry
names a Bazel target, which is built from the current checkout (HEAD) and
resolved to a runnable executable via a `BazelAdapter` (the production impl
shells out to `bazel build` and `cquery`). A matching `[pins]` entry instead
points at an upstream repo + version tag: repobin resolves the tag to a commit,
checks that commit out into a local cache, and builds the tool from source _at
the pinned tag_ (see [Pinned tools](#pinned-tools)). When no `REPOBIN.toml`
matches — or it declares neither a tool nor a pin for the requested name —
dispatch falls back to _default mode_: a `repobin.yaml` peer to the installed
binary maps the tool to a remote repo (optionally pinned to a SHA), which is
cloned into a local cache and built from the `REPOBIN.toml` inside that
checkout. The canonical target therefore always lives in the source repo, never
duplicated in the yaml.

Two caches keep repeated invocations cheap. A _dispatch cache_ records the
resolved executable path keyed by repo root and target, witnessed by the
mtimes of the target's `BUILD` file, sources, and the built binary; on a warm
hit it skips `bazel build`/`cquery` entirely and re-execs the cached path,
falling back to the slow path on any mismatch or corruption. A _repo cache_
backs default mode: HEAD-tracking and pinned checkouts live in separate slots,
refreshes are gated by a fetch stamp and `git ls-remote`, and concurrent
invocations serialise on a per-cache `flock`.

This crate is standalone — it is not part of the Boss system, though Boss's
own CLIs (`boss`, `cube`) are typical tools dispatched through it.

## Install

```bash
cargo install repobin
```

## Usage

Check in a repo-root `REPOBIN.toml`:

```toml
version = 1

[tools.boss]
target = "//tools/boss/cli:boss"

[tools.cube]
target = "//tools/cube:cube"
```

Then install the `repobin` binary plus tool symlinks:

```bash
bazel run //tools/repobin:repobin -- install
repobin install
repobin install --bin-dir ~/.local/bin
```

If you use `direnv`, a lightweight setup is to make sure the same install
directory is on `PATH` while you are in the repo:

```bash
export REPOBIN_BIN_DIR="${REPOBIN_BIN_DIR:-$HOME/bin}"
PATH_add "$REPOBIN_BIN_DIR"
```

That keeps `boss`, `cube`, and other configured commands available without
having `.envrc` mutate global install state on directory entry.

Once installed, invoking a configured tool from inside that repo will:

1. find the nearest `REPOBIN.toml`,
2. build the configured Bazel target,
3. resolve the runnable executable from Bazel metadata,
4. replace the current process with the built binary.

Examples:

```bash
boss task list
cube workspace lease mono --task "prepare repobin publish"
repobin doctor
repobin list
repobin exec boss -- task list
```

## Default mode

When a tool is invoked from a working directory that has no matching
`REPOBIN.toml` (or the matching file does not declare that tool), `repobin`
falls back to a `repobin.yaml` peer to the installed binary:

```yaml
version: 1
tools:
  boss:
    repo: git@github.com:spinyfin/mono.git
    sha: 4baa8fa5e7b2c1d09a3f6b8c2e1d4f7a9b5c3e8d # optional: pin to a specific commit
  cube:
    repo: git@github.com:spinyfin/mono.git
    # no sha → always tracks HEAD
```

The yaml only carries the repo URL and an optional commit SHA — the canonical
Bazel target lives in the target repo's `REPOBIN.toml` and is read from the
cached checkout after refresh, so renaming a target in the source repo
automatically takes effect on the next default-mode invocation.

**Pinning a tool to a specific SHA** (`sha:` field): when present, repobin
checks out exactly that commit before building, rather than following HEAD.
Both full (40-char) and abbreviated (short) hex SHAs are accepted. The pinned
checkout is stored in a separate cache slot from the HEAD-tracking checkout, so
pinned and unpinned builds of the same tool never share build artefacts. If the
requested SHA is not reachable in the remote (typo, GC'd commit, etc.), repobin
fails with a clear error naming the tool and the offending `repobin.yaml`;
there is no silent fall-back to HEAD. Remove `sha:` or leave it unset to return
to the default HEAD-tracking behaviour.

`repobin install` writes this file automatically by recording each local
tool's name against `git remote get-url origin`. Re-installing from another
repo merges new entries; existing entries are kept. Pass `--no-defaults` to
skip writing the file. Note: `repobin install` does not write `sha:` entries;
edit the file by hand when you want to pin a tool.

In default mode the configured repo is shallow-cloned into the cache and the
build runs from that clone (using the target declared in
`<checkout>/REPOBIN.toml`). Pinned tools use a full clone so any commit is
reachable. The routine head/cached/default-mode dispatch is silent; genuine
errors (failed clone, failed cache write, etc.) still surface on stderr. Set
`REPOBIN_VERBOSE=1` to print a one-line notice on every default-mode dispatch,
which distinguishes pinned from floating:

```text
repobin: running `boss` from git@github.com:spinyfin/mono.git @ 4baa8fa (pinned; default mode — not in a configured workspace)
repobin: running `cube` from git@github.com:spinyfin/mono.git @ 7a8b9c0 (cached; default mode — not in a configured workspace)
```

When the underlying tool is invoked with `--json`, the notice is suppressed on
both stdout and stderr regardless of `REPOBIN_VERBOSE`, so
`boss --json … 2>&1 | jq` parses cleanly.

The cache lives at `$XDG_CACHE_HOME/repobin/repos/<slug>-<hash>/checkout` (or
`~/.cache/repobin/repos/...`) for HEAD-tracking tools, and
`<slug>-<hash>/pinned/checkout` for pinned tools. Subsequent invocations reuse
the checkout: a `fetch_stamp` gates whether to refresh (default 5 min, override
with `REPOBIN_DEFAULTS_TTL_SECS`). Past the gate, `repobin` runs `git ls-remote
origin HEAD`; if the remote sha differs from the local sha, it
`fetch --depth=1 origin HEAD` + `reset --hard FETCH_HEAD`. Concurrent
invocations serialise on a per-cache `flock`. Override the cache root via
`REPOBIN_CACHE_DIR`.

`repobin doctor` lists the active defaults file.

## Pinned tools

Default mode (above) pins by **commit SHA** in a user-local `repobin.yaml`. A
_pinned tool_ is different: it is declared in the consuming repo's checked-in
`REPOBIN.toml` and pins by **version tag**, so the pin travels with the repo and
is reviewed like any other source change. repobin builds the tool from source at
that tag — its usual build-from-source flow, just at the tagged commit rather
than HEAD. This replaces bespoke per-repo tool-execution logic (e.g. a
hand-rolled `checkleft` install plus a `bin/checkleft.lock`) with one mechanism.

The tool may live in a different repo than the consumer. For example `checkleft`
lives in `spinyfin/mono` and is released as `checkleft-v*` git tags, while the
consumer is a separate repo. Declare a `[pins.<tool>]` entry naming the upstream
repo and tag:

```toml
version = 1

[tools.app]
target = "//app:app"            # local tool, built at the current checkout's HEAD

[pins.checkleft]
repo = "git@github.com:spinyfin/mono.git"
tag  = "checkleft-v0.1.0-alpha.5"
```

`repobin install` creates a `checkleft` symlink alongside the local tools, so
`checkleft ...` from inside the repo dispatches the pinned build. (Pinned tools
are not written into `repobin.yaml` defaults — a pin is self-contained.)

### How the source is obtained at a tag

The pinned version is acquired as a **git tag in the upstream repo**, resolved
to a single commit and built from that checkout. This is deterministic and
matches how the tool is already released (a tag pointing at the release commit):

1. `git ls-remote` resolves `tag` → commit SHA without cloning. For annotated
   tags the peeled commit (`<tag>^{}`) is used, so the result is always a
   commit.
2. The upstream repo is full-cloned into a per-SHA cache slot
   (`<cache>/repos/<slug>-<hash>/pins/<sha>/checkout`) and that commit is
   checked out. Distinct tags get distinct slots, so they never share build
   artefacts.
3. The tool's Bazel target is read from that checkout's own `REPOBIN.toml`
   (`[tools.<tool>].target`) — the target is never duplicated in the consumer,
   so renaming it upstream needs no consumer change.
4. The target is built the usual way (`bazel build` + executable resolution),
   keyed in the dispatch cache by the SHA-specific checkout path.

### Reproducibility and the lockfile

A pinned build is reproducible: the same tag resolves to the same commit, which
produces the same binary inputs. To make the resolved version verifiable —
the role `bin/checkleft.lock` played before — repobin records it in a
`REPOBIN.lock` next to `REPOBIN.toml`:

```toml
# Auto-generated by repobin. Records the exact commit each pinned tool
# resolved to. Commit this file so pinned builds stay reproducible.
version = 1

[tools.checkleft]
repo = "git@github.com:spinyfin/mono.git"
tag = "checkleft-v0.1.0-alpha.5"
resolved = "4baa8fa5e7b2c1d09a3f6b8c2e1d4f7a9b5c3e8d"
```

Resolution is **lock-first**: if `REPOBIN.lock` already records the tool at the
configured repo + tag, that commit is authoritative and repobin builds it
without contacting the remote. The lock is (re)written only on first use or when
the `tag` in `REPOBIN.toml` changes. Commit `REPOBIN.lock` so CI and teammates
build the exact same commit. `repobin list` and `repobin doctor` surface each
pin's tag and its resolved commit.

If the configured tag does not exist in the upstream repo, repobin fails with a
clear error naming the tool, repo, and tag — there is no silent fall-back to
HEAD.

## Notes

- `repobin` currently supports Bazel-backed tools only.
- It expects a working `bazel` entry point on `PATH` and a `git` entry point
  for default-mode and pinned-tool clones (and `git ls-remote` for resolving
  pinned tags).
- `repobin install` defaults to `~/bin` and warns if the chosen directory is
  not on `PATH`.
- If you use `direnv`, prefer adding the chosen `repobin` bin dir to `PATH`
  rather than running `repobin install` from `.envrc`.
