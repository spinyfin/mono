# Runshim: Repo-Local Bazel Command Shims

## Overview

Runshim is a small launcher that makes Bazel-built repo tools feel like normal
commands on the user's `PATH`.

The intended workflow is:

1. a repo declares command names and Bazel executable labels in a root config
   file,
2. the user installs one global `runshim` binary plus symlinks for those
   command names into a directory such as `~/bin`,
3. invoking one of those symlinked commands from inside the repo causes
   Runshim to find the current repo, `bazel build` the configured target, and
   `exec` the built binary with the original args.

This is similar in spirit to `dotslash` because a checked-in file defines a
portable command entry point. The important difference is that Runshim does not
download or verify prebuilt artifacts. It delegates execution to the local
repo's Bazel graph and toolchains.

Example:

```text
~/bin/boss -> ~/bin/runshim
~/bin/cube -> ~/bin/runshim
```

With this repo config:

```toml
version = 1

[tools.boss]
target = "//tools/boss/cli:boss"

[tools.cube]
target = "//tools/cube:cube"
```

Running `boss task list` from `/path/to/repo/subdir` should:

1. locate `/path/to/repo/RUNSHIM.toml`,
2. resolve `boss -> //tools/boss/cli:boss`,
3. run `bazel build //tools/boss/cli:boss` from the repo root,
4. resolve the built executable path from Bazel metadata,
5. replace the Runshim process with that built binary,
6. preserve the original `cwd` (`/path/to/repo/subdir`) and args.

## Goals

- Make repo-owned Bazel binaries invokable as short commands on the user's
  normal shell path.
- Keep the checked-in configuration minimal: command name to runnable Bazel
  label.
- Resolve the repo from the current working directory rather than from the
  installed symlink location.
- Use `bazel build` and then directly `exec` the built binary rather than
  delegating to `bazel run`.
- Preserve normal CLI behavior: inherited stdio, signal handling, exit codes,
  and current working directory.
- Keep the fast path quiet when Bazel has little or no work to do.
- Make installation idempotent and cheap across many repos.

## Non-Goals

- Reproducing `dotslash`'s artifact download, hashing, or multi-platform
  binary distribution model.
- Supporting arbitrary non-executable Bazel targets.
- Supporting Windows in v1. The first version can be Unix-only and rely on
  symlinks plus `exec`.
- Adding a large wrapper language around Bazel. Bazel remains the source of
  truth for how the tool is built.
- Inventing repo-global package management or cross-repo version negotiation
  for the `runshim` binary itself.

## Why This Exists

Today repo-local tools often fall into one of three awkward shapes:

- checked-in shell wrappers such as `./tools/checks`,
- long Bazel commands such as `bazel run //tools/boss/cli:boss -- ...`,
- globally installed binaries that can drift away from the repo's version.

Runshim aims for a narrower model:

- the repo owns the mapping from command name to Bazel target,
- the user gets a short command on their `PATH`,
- the executed binary is always built from the current repo checkout.

That gives a `dotslash`-like ergonomic entry point without treating built
artifacts as a distribution format.

## Design Principles

### Repo-Resolved, Not Install-Resolved

The symlink name only selects the logical tool name. The repo that owns the
target is determined by the caller's current working directory. That allows one
global `~/bin/boss` symlink to work across multiple repos, as long as each repo
provides a `boss` entry in its own config.

### Bazel Owns Executable Resolution

Runshim should not guess the executable path by string manipulation on the
label. Configuration transitions, launcher scripts, and rule-specific output
shapes make that brittle. Instead, Runshim should ask Bazel for the target's
executable through `FilesToRunProvider`.

### Preserve The User's Shell Semantics

The final tool should behave as if the user ran it directly: same `argv`,
inherited environment, same `stdin/stdout/stderr`, same terminal control, and
same exit status. That is why the runtime path should end with `exec`, not
spawn-and-wait.

### Quiet Fast Path

Most invocations after the first build should be nearly silent. If Bazel has no
work to do, users should see the tool's output, not Bazel chatter.

## User Model

Runshim has two modes:

1. `runshim` command mode for installation and diagnostics.
2. tool mode when invoked through a symlink such as `boss` or `cube`.

Recommended commands:

```bash
bazel run //tools/runshim -- install
runshim install
runshim doctor
runshim list
```

Recommended tool-mode usage:

```bash
boss task list
cube workspace lease mono --task "write runshim design doc"
```

For debugging or scripts, Runshim may also support:

```bash
runshim exec boss -- task list
```

The symlinked path remains the main UX. The explicit `exec` form is useful for
testing without creating or repairing symlinks.

## Repository Configuration

### Canonical File

The v1 design should standardize on a single repo-root file:

```text
RUNSHIM.toml
```

TOML is the better initial choice than YAML because:

- the config shape is small and table-oriented,
- command names can naturally be the table keys,
- parsing and validation are straightforward,
- the repo already uses TOML for several checked-in configs.

We can add YAML later if there is strong demand. V1 should not support both
formats simultaneously because that introduces precedence and conflict rules for
little benefit.

### Proposed Schema

```toml
version = 1

[install]
link_dir = "~/bin"

[tools.boss]
target = "//tools/boss/cli:boss"

[tools.cube]
target = "//tools/cube:cube"
```

Field rules:

- `version` is required for forward compatibility.
- `[install].link_dir` is optional. If absent, Runshim uses `~/bin`.
- each key under `[tools]` is the installed command name.
- each tool entry must provide exactly one executable Bazel `target`.

V1 intentionally omits per-tool default args, environment overrides, and custom
bazel flags. Those can be added later without changing the core model.

### Naming Rules

Tool names should be safe path basenames:

- ASCII letters, numbers, `.`, `_`, and `-`,
- no path separators,
- no empty names,
- no `.` or `..`.

That keeps install behavior predictable and avoids surprising link targets.

## Installer Design

### Entry Point

The bootstrap command should be:

```bash
bazel run //tools/runshim -- install
```

That works even before `runshim` is globally installed.

Once installed, the user can rerun:

```bash
runshim install
```

from any repo that contains `RUNSHIM.toml`.

### Install Behavior

`runshim install` should:

1. locate the repo root from the current working directory,
2. parse `RUNSHIM.toml`,
3. ensure the target link directory exists,
4. install or refresh the global `runshim` binary in that directory,
5. create symlinks for every configured tool name pointing at that binary.

Example result:

```text
~/bin/runshim
~/bin/boss -> runshim
~/bin/cube -> runshim
```

The install should be idempotent:

- if `~/bin/boss` already points at `runshim`, leave it alone,
- if the global `runshim` binary already matches the installed path, replace it
  atomically only when needed,
- rerunning install after adding new tools only adds the missing symlinks.

### Link Directory

`~/bin` is the default because it is short, conventional, and matches the
intended user model. The installer should:

- create it if it does not exist,
- warn if it is not on `PATH`,
- allow `--link-dir` to override the default.

### Removal And Pruning

V1 should not try to implement a full repo-aware uninstall story. Because the
same generic `~/bin/boss -> runshim` symlink can serve many repos, aggressive
pruning would be more surprising than helpful.

The simplest v1 behavior is:

- `install` only creates or refreshes links,
- stale links are harmless,
- a later `prune` command can be added if we decide to keep per-user install
  metadata.

## Runtime Behavior

### 1. Determine Invocation Mode

If `argv[0]` basename is `runshim`, parse subcommands.

Otherwise:

- use the basename of `argv[0]` as the requested tool name,
- pass `argv[1..]` through unchanged to the final executable.

### 2. Discover Repo Root

Walk upward from the caller's current working directory looking for
`RUNSHIM.toml`. The first match wins.

This gives the right behavior for:

- invoking a tool from the repo root,
- invoking from any nested subdirectory,
- nested repos, where the nearest config should win.

If no config is found, exit with a clear error such as:

```text
runshim: no RUNSHIM.toml found from /current/path upward
```

### 3. Resolve Tool Entry

Parse the config and look up the tool-name key under `[tools]`.

If the symlinked name is not configured in the current repo, fail clearly:

```text
runshim: tool "boss" is not configured in /path/to/repo/RUNSHIM.toml
```

### 4. Build The Target

Run:

```bash
bazel build <target>
```

from the discovered repo root.

Runshim should use the user's normal Bazel entry point from `PATH`, typically
`bazel` or a `bazel` symlink to Bazelisk. It should not bypass the repo's
normal `.bazelrc`, module resolution, or toolchain setup.

### 5. Resolve The Built Executable

After a successful build, Runshim should ask Bazel for the executable file via
the target's `FilesToRunProvider.executable` rather than constructing a
`bazel-bin/...` path manually.

This avoids rule-specific path guessing and correctly handles:

- launcher wrappers,
- configuration transitions,
- runfiles-aware executable entry points,
- platform-specific output paths.

If Bazel reports no executable, the error should say that the configured target
is not runnable.

### 6. `exec` The Built Binary

Runshim should `exec` the resolved path with:

- the original argument vector,
- the original process environment,
- the original current working directory.

The build step runs from the repo root, but the final tool should run from the
user's original `cwd`. That is important for tools whose behavior depends on
the directory where the user invoked them.

## Bazel Output Policy

The output policy should optimize for the common incremental-build case while
still making failures and slow builds understandable.

### Default Policy

1. start the build with stdout/stderr captured,
2. if the build succeeds quickly, print nothing,
3. if the build fails, replay the captured output to stderr and exit with the
   Bazel status,
4. if the build exceeds a "slow build" threshold, stop pretending the command
   is instantaneous and surface progress.

### Suggested Thresholds

- `slow_build_notice`: 3 seconds
- `attach_live_output`: 10 seconds

Suggested behavior:

- before 3 seconds: fully silent,
- after 3 seconds: print `runshim: building //tools/boss/cli:boss...`,
- after 10 seconds: attach live Bazel output to stderr until the build
  completes.

The exact numbers can be constants in v1. We do not need config fields for
them immediately.

### Quiet Build Flags

When Runshim drives `bazel build`, it should pass quiet UI flags so that any
attached output is still readable:

```text
--color=no
--curses=no
--show_result=0
--ui_event_filters=-info
```

The goal is not to hide important errors. The goal is to keep the successful
fast path free of transient progress noise.

### Opt-In Verbosity

For debugging, support an escape hatch such as:

```bash
RUNSHIM_VERBOSE=1 boss task list
```

In verbose mode, Runshim should stream Bazel output immediately and skip the
suppression logic.

## Executable Resolution Details

The design should explicitly distinguish "build target" from "path to launch".

Why not infer `bazel-bin` paths directly:

- the target may produce a launcher script rather than the underlying binary,
- output directories encode configuration details that Runshim should not have
  to understand,
- some executable rules require runfiles metadata that is easiest to honor by
  using Bazel's own executable metadata.

Using `FilesToRunProvider.executable` keeps the interface clean:

- config stores a normal Bazel label,
- Bazel answers whether it is runnable,
- Runshim launches exactly the path Bazel intends end users to execute.

## Diagnostics

The v1 binary should make the common failures obvious:

- repo config not found from the current directory,
- malformed `RUNSHIM.toml`,
- tool name not present in config,
- invalid Bazel label syntax,
- `bazel build` failure,
- configured target is not executable,
- resolved executable path missing after build,
- install link directory not writable.

`runshim doctor` should validate the current repo and print:

- discovered repo root,
- discovered config path,
- configured tools,
- whether each target appears executable,
- installed link directory,
- whether that directory is currently on `PATH`.

## Trust And Security Model

Runshim's trust boundary is the current repo plus the local Bazel toolchain.

That means:

- it is appropriate for repos the user already trusts to build and execute,
- it is not a sandbox,
- it does not verify downloaded standalone artifacts the way `dotslash` does,
- entering an untrusted repo and invoking one of its configured tools is
  equivalent to opting into that repo's Bazel-defined execution behavior.

This is an intentional tradeoff. The point is convenient execution of repo-owned
tools, not artifact distribution across untrusted environments.

## Implementation Sketch

The smallest viable implementation is:

1. Rust binary at `//tools/runshim:runshim`.
2. `clap`-based CLI with `install`, `doctor`, `list`, and optional `exec`
   subcommands.
3. TOML config parsing with a small typed schema.
4. upward repo-root discovery based on `RUNSHIM.toml`.
5. Bazel command runner with buffered output and slow-build escalation.
6. executable resolution through Bazel query metadata.
7. Unix `exec` handoff to the built binary.

This should stay intentionally small. The design is good if it replaces
repo-local one-off wrappers, not if it grows into a second build system.

## Follow-Up Extensions

Useful follow-ups after the core design works:

- per-tool `bazel_args`,
- explicit aliases for one target under multiple command names,
- repo-local default env vars,
- install metadata plus safe pruning,
- shell completion for `runshim` subcommands,
- optional YAML support if TOML becomes a real constraint.
