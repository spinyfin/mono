# Shared-source-store write gate

Every cube workspace is a _secondary_ jj workspace. Its `.jj/repo` is a
pointer file naming the one object store that all leased workspaces of a
repository share: `<cube repos root>/<repo>/.jj/repo`, which sits next to a
real, non-bare `.git`. Plain `git` run inside a workspace fails with
`fatal: not a git repository`, because the workspace has no `.git` of its own.

That failure is a feature. The shared store is owned by cube and the engine.
The only sanctioned ways to change it are `jj` inside the worker's own
workspace (`jj git fetch`, `jj new`, `jj describe`, `jj bookmark set`, …) and
the `cube` helpers (`"$CUBE_BIN" pr create` / `pr update`). A raw git write
to the store changes it for every workspace and every future dispatch at
once, and nothing repairs it automatically.

## The incident this gate exists for

A repository's shared store became a shallow clone. Every dispatch for that
repository then failed for three days: recovery-bookmark creation walked
history past the shallow boundary and hit a missing commit. The boundary
commit put the shallowing fetch inside a two-day window in which the only
runs touching that repository were CI-fix and review runs on a PR that
changed the repository's own CI script — a script that runs
`git fetch --depth=200 origin main` under its PR-build environment. A worker
reproducing that script locally ran it against the shared store.

Workers reached the store in two observed ways:

1. exporting `GIT_DIR=<cube repos root>/<repo>/.git` (seen in several
   transcripts; a common "fix" for the not-a-git-repository error), or
2. `cd`-ing into `<cube repos root>/<repo>`, the non-bare primary checkout,
   and running git there (raw `git fetch` entries in the store's reflog).

A third route was the engine's own doing: `pr_review_guide` workers were
spawned with `GIT_DIR` exported at the pinned workspace's store so the prompt's
`git show <sha>:<path>` / `git diff <sha> <sha>` reads would resolve. That is
why a read-only review-guide worker could run `git diff` successfully in a
workspace that has no `.git`. It also meant every bare `git` in that session
was a store operation. The engine no longer exports `GIT_DIR` to any worker;
see [Review-guide workers](#review-guide-workers) below.

## What the gate does

`boss-git-store-guard.py` is a `PreToolUse` guard shared by all three drivers.
Claude runs it as a settings-file hook, Codex materialises it into its
attested `CODEX_HOME/guards/` chain, and Grok wraps it in its hook adapter —
the same arrangement as the Boss-data-dir path guard. It is armed for every
local worker kind (standard, revision, reviewer, triage, answer agent, review
guide); remote SSH workers do not get it because the script is never shipped
to the remote host.

The script derives cube's repos root at decision time from the leased
workspace's `.jj/repo` pointer (`BOSS_WORKSPACE` on the hook command, falling
back to the tool call's cwd and walking upwards). A workspace with no pointer
is not a cube lease, so there is no shared store to protect and the gate has
nothing to say.

For every `git` invocation in a shell command it works out where git would
find its repository the way git itself does:

- `--git-dir` / `--work-tree` on the command line;
- `GIT_DIR`, `GIT_WORK_TREE`, `GIT_COMMON_DIR`, `GIT_OBJECT_DIRECTORY` as a
  prefix assignment (`GIT_DIR=… git …`, `env GIT_DIR=… git …`), as an
  `export` earlier in the same command, or inherited from the hook's own
  environment;
- `-C <dir>` or `-C<dir>` (resolved sequentially), a preceding `cd`/`pushd`, and finally the tool call's working
  directory (which persists across a worker's shell calls, so a `cd` into the
  store in an earlier call still counts);
- single-level shell variables (`S=<store>; git -C "$S" …`) and
  `sh -c '…'` / `bash -c '…'` / `eval` bodies, which are inspected
  recursively;
- for `clone`, `init` and `worktree`, the directories they create.

The shared shell tokenizer preserves logical lines, heredoc data and subshell
boundaries. The gate restores parent directory and environment state after a
subshell and treats directory state across conditionals and loops as uncertain.
Pipeline elements cannot change the tracked parent directory or environment.
Comments are removed before lexing and heredoc detection; malformed syntax
cannot hide later commands from the shared denylist guards. Function definitions
with store involvement fail closed rather than applying an unexecuted body.
Exported variables retain their export attribute when reassigned, including
names exported before assignment, allexport mode, and declaration export flags.
Directory
changes through `builtin` are tracked; relative changes with `CDPATH` are
uncertain. Argument-appending launchers such as `xargs` cannot establish a
read-only git invocation, and `jj util exec` payloads are inspected.

An unjudged git word with a store reference is refused. Unrecognized git or
wrapper options fail closed only when the store is involved. Clone option
values are consumed before identifying its destination. Shell stdin, script
files and interpreter code are refused when the command references the store
or starts there. Quoted heredoc data can pass; command substitutions in expanding
heredocs are refused with store involvement. Sourcing scripts and executing
unjudged paths are refused when the store is involved, including files written
earlier in the command. Shell options stop at the script operand, so a script's
own `-c` argument cannot masquerade as inspected shell code. Command words held
in variables and eval arguments are expanded before dispatch.

Scripts and other programs are refused when their effective git-location
environment points into the store. From a store working directory, only
read-only git, jj, plain readers and inspected shell bodies are permitted.
Run CI scripts in a throwaway clone, even when the shell command itself
contains no git invocation.

It then refuses when any of those resolves under the repos root — as spelled
or once symlinks are resolved — unless the subcommand is clearly read-only.
Read-only means `log`, `show`, `diff`, `diff-tree`, `rev-parse`, `rev-list`,
`cat-file`, `ls-files`, `ls-tree`, `ls-remote`, `merge-base`, `status`,
`blame`, `describe`, `show-ref`, `for-each-ref`, `grep`, `shortlog` and
similar, plus the listing forms of `branch`, `tag`, `remote`, `config`,
`stash`, `reflog`, `worktree`, `notes` and `submodule`. Anything else is a
write: the rule is default-deny, so an unknown subcommand (an alias, a typo)
is refused rather than guessed about. `-c alias.*=…` disqualifies the command
from the read-only list outright.

A `fetch`, `pull` or `clone` carrying `--depth`, `--shallow-since`,
`--shallow-exclude`, `--update-shallow`, `--deepen` or `--unshallow` that
would touch the store is refused with its own message naming the shallowing
hazard, in every spelling and from every directory.

When a mutating git's target cannot be resolved (command substitution, a
variable the gate cannot see) and the command text names the repos root or
derives it through `.jj/repo`, `jj root`, or `jj workspace root`, the gate
refuses. Shallow operations with an unresolved target always fail closed in
a cube lease; a throwaway destination under `$TMPDIR` still resolves and passes.

The refusal is loud and says what to do instead: jj inside the worker's own
workspace for VCS work, the cube PR helpers for publishing, and a throwaway
clone under the worker's own temp directory (`git clone <remote URL>
"$TMPDIR/ci-repro"`; `jj git remote list` prints the URL) for reproducing a
CI script that runs raw git. It never rewrites the command, never warns-only,
and makes no exception by worker or driver.

Like every Boss guard it fails closed on a payload it cannot read, and it
approves every non-shell tool call (it judges `Bash` commands only; file edits
cannot run git).

## What still works

- Read-only git against the store, including the explicit form
  `git --git-dir <repos root>/<repo>/.git log -1` and `GIT_DIR=… git show …`.
- `jj` — jj's own `git fetch` is how the store is meant to be synced. The
  gate permits jj when no shared-store git environment is supplied.
- Recognized git commands whose repository resolves elsewhere: a throwaway clone under the
  worker's temp dir (shallow or not), a bare `git status` from the workspace
  (which simply fails, as before), `git clone <store> "$TMPDIR/x"` (the store
  is only the _source_).
- Mentioning the store path inside a quoted argument (a commit message, a
  `--body`), which is data, not a target.

## Review-guide workers

`pr_review_guide` workers read pinned revisions from a read-only workspace.
The engine passes the store as
`BOSS_REVIEW_GUIDE_GIT_DIR` (and explicitly unsets `GIT_DIR`), and the prompt
spells the two permitted reads out as
`git --git-dir {{GIT_DIR}} show {{BASE_SHA}}:<path>` and
`git --git-dir {{GIT_DIR}} diff --no-ext-diff --no-textconv {{BASE_SHA}} {{HEAD_SHA}} -- <path>`
(prompt contract `review-guide-v11`, update contract `review-guide-update-v5`;
see `tools/boss/docs/designs/automatic-pr-review-guides.md`). The shared
review-guide command guard accepts `--git-dir <path>` only when `<path>` is
the engine-supplied store, and still pins the revisions; the shared-source-
store gate lets those reads through because `show` and `diff` are read-only.

## Where the pieces live

- Script and decision matrix: `tools/boss/engine/driver/src/git_store_guard.rs`
  (including its inline `tests` module).
- Content-addressed materialisation next to the worker settings file:
  `tools/boss/engine/core/src/worker_setup/git_store_guard.rs`; the
  engine-side wiring tests are in the `git_store_guard_tests` module of
  `tools/boss/engine/core/src/worker_setup_tests/settings_hooks.rs`.
- Per-driver arming: `claude.rs` (`tool_use_interception_wiring`),
  `codex.rs` (`materialize_guards`), `grok/hooks.rs` (`guard_commands`), each
  with an end-to-end refusal test through that driver's own hook path.
- Review-guide read grammar: `tools/boss/engine/driver/src/codex/review_guide_reads.rs`.
