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
captures the composed PATH in `BOSS_WORKER_PATH`. Worker-owned `BASH_ENV` and
`ZDOTDIR/.zshenv` restore that value as the leading PATH segment in
noninteractive Bash and zsh tool shells, and install a DEBUG trap that repeats
the restore before later commands. That covers a driver which sources an
`export PATH=...` snapshot after startup (Claude Code's Bash tool:
`source <snapshot> && eval '<cmd>'`). Driver-added private helper directories
remain available after that segment; they cannot shadow the composed tools.
The zsh file disables further profile loading. Codex additionally disables
login shells, shell snapshots and profile environment reconstruction through
correctly scoped configuration. This affects new sessions; existing sessions
retain the configuration with which they started.

The order at the shell boundary is therefore:

```text
before: shell profiles / driver login pass -> arbitrary host PATH order
after:  worker launchers -> bundled binaries (when present) -> host tools
        nested Bash and zsh tool shells restore this composed order,
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
