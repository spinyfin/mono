# Worker tool environment

Bare `checkleft run` must dispatch through the leased repository's
`REPOBIN.toml` when that file declares checkleft. It must not depend on
which checkleft is installed first on the host. The worker's generated
`checkleft` launcher executes the engine-owned repobin by absolute path;
repobin builds the configured Bazel target from the current checkout and
preserves the tool's exit status and working directory. Workspaces with
no `REPOBIN.toml`, or with a config that does not declare checkleft, do
not receive a launcher, so host lookup is unchanged.

## Observed failure and ownership

Investigation on 2026-09-16 established these distinct facts:

- A worker shell resolved `~/bin/checkleft` before
  `~/.cargo/bin/checkleft`. That observation does **not** establish a universal
  ordering for workers or shells.
- Running in the same cube workspace with the cargo directory first reproduced
  exactly 18 `configured check references unknown implementation` errors and
  exit status 1. Those were stale-binary errors, not broken repository checks.
- Cube leases and provisions the checkout. Its setup steps run in subprocesses;
  they do not export a worker environment. Mono's `.cube/setup.yaml` prepares a
  GhosttyKit placeholder, not PATH.
- Boss `spawn_flow::WorkerPaneLaunch` seeds a fixed system PATH, then launches
  an interactive login shell. The shell's startup files rebuild PATH. The
  shared local `PaneSpawnRunner` subsequently prepends the bundled tools and
  per-workspace launchers before invoking the selected driver.
- Codex can run another login shell for tool calls, demoting those prepends.
  The generated config intended to prohibit this, but `allow_login_shell` was
  emitted after `[notice.external_config_migration_prompts.projects]`. Parsing
  that worker's actual config found `false` in that table and no
  top-level value. The old test checked ordering against `[features]` only,
  missing the earlier table. It now checks against the first table.

The reports of cargo-first ordering and the observed repobin-first ordering
are compatible: shell startup and driver shell mode both affect the result.
There is no evidence that every driver explicitly sorts cargo ahead of repobin.
Codex's extra login/profile pass is the confirmed mechanism for undoing the
engine's intended order. Its supported controls are documented in the
[Codex configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference).

## Composition and sealing

The shared local spawn path materializes a worker-owned tool directory. Boss
and cube retain their existing engine-versioned launchers; checkleft uses the
engine-bundled repobin, which resolves the repository's configured build or pin.
Neither launcher discovery nor execution searches PATH for checkleft or repobin.
A missing dispatcher exits 127 with a specific diagnostic. A failure to write
the environment aborts spawn; it cannot quietly restore host-tool selection.

After driver environment directives and launcher prepends, the spawn script
exports only the launcher directory and optional `BOSS_BIN_DIR` in
`BOSS_WORKER_TOOL_PATH`. Worker-owned `BASH_ENV` and
`ZDOTDIR/.zshenv` restore that value as the leading PATH segment in
noninteractive Bash and zsh tool shells, and install a DEBUG trap that repeats
the restore before later commands. That covers a driver which sources an
`export PATH=...` snapshot after startup (Claude Code's Bash tool:
`source <snapshot> && eval '<cmd>'`). Driver-added private helper directories
remain available after that segment. Project toolchain prepends (virtualenvs,
node_modules, nvm, or rustup) still precede host tools; the restore never
re-prepends the host PATH ahead of them.
The zsh file disables further profile loading. Codex additionally disables
login shells, shell snapshots and profile environment reconstruction through
correctly scoped configuration. This affects new sessions; existing sessions
retain the configuration with which they started.

The order at the shell boundary is therefore:

```text
before: shell profiles / driver login pass -> arbitrary host PATH order
after:  worker launchers -> bundled binaries (when present) -> host tools
        nested Bash and zsh tool shells restore only the worker-owned prefix,
        including after a snapshot that re-exports PATH
```

Stale installations remain untouched. In noninteractive Bash and zsh tool
shells that inherit `BASH_ENV` / `ZDOTDIR`, the launcher directory is the
leading PATH segment for command lookup, even when other directories still
contain that filename. Codex tool shells are also covered by
`allow_login_shell = false` and `shell_snapshot = false` in the generated
config. Host tools and authentication variables remain inherited. This is a
seal on tool lookup, not a hermetic machine image or an environment-variable
security boundary. The reusable launcher and shell-environment functions live
in `boss-engine-worker-bin`; future work can apply the same composition point
to explicit toolchain manifests, locale and SDK selection, and
credential-variable inheritance. Those policies should be specified
independently of this fix.

## The progression agent: no workspace

A `progression` run (the agent that diagnoses one stuck work item) holds no
code checkout and no cube lease. It is spawned through the same local path as
every other worker, but its working directory is an engine-owned scratch
directory, `<system temp>/boss-progression-scratch/<execution id>`, outside every
repository, created before the spawn and removed when the run is torn down.
The spawn machinery needs a directory to write the agent-rules file, the settings
pointer and the initial-input script, and nothing else lives there.

What it can run is an allowlist under `--permission-mode dontAsk`, which the
dispatcher forces so it cannot be downgraded: `Read`, `Grep` and `Glob`;
`boss propose`, read-only `boss task show|list` and `boss pr status` (in both
the bare `boss` and `"$BOSS_BIN"` spellings); and `gh pr view|checks`. Everything
else is denied: file writes, all of `jj`, `git` and `cube`, mutating `gh` verbs
and `gh api` with a mutating method or field flag. The ordinary worker denylist
still applies on top. The confinement differs per driver, and none of them uses
the Standard posture:

- **Claude** relies on the allowlist and deny belt above under `dontAsk`.
- **Codex** gets no `--sandbox` flag; its config renders a `progression`
  permission profile instead: a read-only filesystem, the attributed engine
  socket, and network access to `github.com` and `api.github.com` only. A host
  allowlist cannot tell a GitHub read from an authenticated write, so a
  deny-by-default `PreToolUse` guard (matcher `.*`) is installed as the second
  layer. It admits only `boss propose`, `boss task show|list`, `boss pr status`,
  `gh pr view|checks` and a few read-only file commands (`cat`, `ls`, `head`,
  `tail`, `nl`, `wc`, `rg` without `--pre`), as a single command with no
  chaining, redirection or substitution, and rejects mutating `gh`, `jj`, `git`,
  `cube` and every file write.
- **Grok** receives the same allow and deny rules as `--allow` / `--deny`
  arguments under `--permission-mode dontAsk`. On local macOS it runs with
  `--sandbox off` like every other local kind, so those rules are the only
  confinement there; elsewhere it also gets the read-only sandbox profile.

The PATH and launcher environment described above applies unchanged: the boss
and cube launchers are materialized in the worker tool directory and the sealed
`BOSS_WORKER_TOOL_PATH` is exported. Because the scratch directory has no
`REPOBIN.toml`, it gets no checkleft launcher, and `BOSS_LEASE_ID` is empty.
The pane's pid is registered at the confined progression tier of the worker RPC
surface (with its neighbourhood captured first), on a fresh spawn and again when
the pane is re-adopted after an engine restart.

### Engine-side probes before the first turn

The agent's confinement does not limit what the engine itself reads while
composing the prompt. Before the agent's first turn, for an item with a PR, the
engine (not the agent) runs a fixed set of bounded probes and renders each
result, or its failure, under "Engine evidence probes". They run with the
engine's own credentials, outside the worker allowlist:

- `gh pr view <url> --json baseRefName,baseRefOid,headRefOid,statusCheckRollup`
  for the current head and check rollup.
- `gh api repos/<owner>/<repo>/git/ref/heads/<branch>` for the live tip of the
  PR's base branch (the PR's recorded base commit is only its fork point).
- A GraphQL `defaultBranchRef.target.history(first: 10)` query for recent
  check outcomes on the default branch, run only when a check is failing.
- For merge-conflict items, a blob-less scratch fetch of the two commits into a
  temporary directory created beside (not inside)
  `boss-progression-scratch/<execution id>` and removed when the probe ends.
  It fetches from the product's configured remote, falling back to the PR's
  repository over HTTPS, with `GIT_TERMINAL_PROMPT=0` so missing credentials
  fail the probe instead of waiting on a prompt. SSH defaults to
  `ssh -o BatchMode=yes` to prevent prompts through `/dev/tty`; a configured
  `core.sshCommand` retains its key and connection options with BatchMode
  appended. Existing `GIT_SSH_COMMAND` or `GIT_SSH` settings are preserved
  unchanged and must themselves provide noninteractive behavior.
- A CI log tail for each failing check whose stored attempt is not already for
  the current head.

Each probe has a finite time bound (120 seconds for `gh`, 300 seconds for the
conflict fetch). A probe that fails or times out is rendered as such and the
agent treats that evidence as unknown.

## Validation

The worker-bin Bazel tests put a stale checkleft ahead of the composed PATH
before invoking Bash and macOS zsh login tool shells. Both must resolve the
worker launcher, forward `run`, and preserve a failing tool's exit status.
A second test sources a snapshot that re-exports PATH (the Claude Code Bash
tool command shape) inside `bash -c` / `zsh -c` and requires the same
launcher resolution. Another test verifies that a missing dispatcher fails
instead of falling back. The zsh executable is a declared test input; the
hermetic test sandbox remains enabled. Core spawn tests verify the same
environment clause is applied to the real local spawn script, that a
workspace with no `REPOBIN.toml` does not receive a checkleft launcher, and
that a workspace which declares checkleft does.
