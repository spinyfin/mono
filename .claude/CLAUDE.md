# Boss worker rules

You are running inside a Boss-managed worker session. The engine
spawned you in a leased cube workspace and is observing this
session via claude hooks routed to its events socket.

## Pull requests are the deliverable

**A task is not complete until a PR exists for it.** Local
commits are NOT enough. Workers that stop with only local
commits are treated as incomplete and the engine will probe
you to push and open a PR before transitioning the work item
to review.

- Push your branch and open a PR with `gh pr create` once
your branch has commits and tests pass.
- **If a PR for this branch already exists** (e.g. you are
resuming work via `--prefer`, or addressing review
comments), push your new commits to update it; do NOT
open a duplicate PR. Check first with
`gh pr list --head $(jj log -r @ --no-graph -T 'bookmarks' | head -1)`
or simply `gh pr view` from inside the workspace.
- Do not hard-wrap PR bodies — GitHub renders single newlines
inside paragraphs as visible breaks.
- Before ending the run, print the PR URL on its own line as
the final thing in your final response so the engine can
pick it up automatically.
- Before pushing, verify your changes are real with
`jj diff -r @`. If the diff is empty, you have made no
changes — do NOT commit, push, or open a PR. Stop and
explain what went wrong instead.

## Your workspace

- Workspace path: `/Users/brianduff/Documents/dev/workspaces/mono-agent-006`
- Cube lease id: `a37cbeb1-b9e8-4caa-a4e6-6b0fc9a8650c`

The lease is held for the lifetime of this run. Do not lease,
release, or otherwise mutate cube state — the engine owns lease
lifecycle.

## VCS

Use `jj` for all VCS operations. Do not invoke `git` directly
except via `gh` for GitHub operations.

- `jj git fetch` to sync with origin.
- `jj new main` for a fresh task; `jj edit <bookmark>` to resume.
- `jj describe -m '...'` to set commit messages; `jj git push
-b <bookmark>` to publish.
- Never run `jj git push --deleted` or `git push --delete`
without explicit user approval.
- `.claude/` is gitignored by the engine on every spawn. Do
not force-track or commit anything inside it (no
`--force`, no `jj file track .claude/...`) — those files
are per-worker plumbing, not part of the project.

### Commit messages must be inline

Never invoke `git commit`, `git rebase`, `jj commit`, or
`jj describe` without an explicit `-m "…"` message. The same
rule applies to amend and squash flows (`git commit --amend`,
`jj squash`, `jj split`): pass `-m` inline. The worker
environment intentionally has no usable `$EDITOR`, so any
command that falls through to one will fail fast — fix it by
re-running with `-m`, not by changing the editor.

## Creating a PR from a jj workspace

This workspace uses jj, not plain git. There is **no `.git/` at the
workspace root** — the backing git store lives at `.jj/repo/store/git`.
Raw `gh` invocations that rely on git-directory discovery will fail
with `fatal: not a git repository` unless you point them at it.

**Rule: prefix every `gh` call with `GIT_DIR=.jj/repo/store/git`.**
This applies to `gh pr create`, `gh pr view`, `gh pr checks`,
`gh pr list`, `gh api`, and any other `gh` verb that touches git
state. Exporting it once at the top of a sequence of commands is fine.

**Rule: pass `--head <bookmark> --base main` to `gh pr create`.**
`gh` cannot infer HEAD from a jj checkout, so the bookmark name must
be given explicitly. Same for any `gh` verb that needs HEAD context.

**Rule: `jj git push -b <bookmark>` requires `--allow-new` the first
time the bookmark is pushed.** Subsequent pushes of the same bookmark
do not need the flag.

### Canonical PR creation recipe

Copy-paste this block; substitute `my-feature` with your bookmark name:

```sh
# Describe the commit (inline -m is required — no editor)
jj describe -m "your commit message"

# Create a named bookmark pointing at the current commit
jj bookmark create my-feature -r @

# Push — first push of a new bookmark requires --allow-new
GIT_DIR=.jj/repo/store/git jj git push -b my-feature --allow-new

# Open the PR
GIT_DIR=.jj/repo/store/git gh pr create \\
--head my-feature --base main \\
--title "Your PR title" \\
--body "PR description"
```

To update an existing PR after new commits:

```sh
jj git push -b my-feature   # no --allow-new needed
```

## Boundaries

- Do not modify files outside this workspace. Sibling workspaces
under `~/Documents/dev/workspaces/` belong to other workers
and concurrent edits will corrupt their state.
- Do not modify cube's database, lease state, or workspace
registry. The engine reconciles state on its own.
- The Boss runtime state under `~/Library/Application Support/Boss/`
(state.db, dispatch-events, engine-audit.log, the events
socket, executions/, …) is the coordinator's territory.
Workers must never read, write, or otherwise touch it —
the engine enforces this via permission deny rules and
audits every attempt. If you need work-taxonomy context,
ask the coordinator to inject it; do not query the DB
yourself. `bossctl` is similarly coordinator-only.

## Referring to work items

Work items (tasks, chores, projects) carry both a primary id (`task_18aefd71a9458550_17`) and a short friendly id (`#42`). When referring to a work item in chat with the user:

- **Prefer the friendly id by default.** Say `#42` instead of the hex blob. The friendly id is what appears on the kanban card and is what the user reads and speaks aloud.
- **Format:** `#42` for single-product chat, `Boss #42` or `Flunge #7` when the message spans multiple products (use the product slug in lowercase, followed by space and the number).
- **Parenthetical primary id on first mention.** When you file or refer to a new work item in the same session, quote the primary id once on first mention so the user has the canonical handle: "Filed `#42` (task_18aefd71a9458550_17) for the migration."
- **Fall back to the primary id only when:** the user explicitly asked for it; you are producing output for a system that doesn't speak friendly ids (a SQL query, a debugger, a tool that requires the primary id); or the friendly id would be ambiguous outside of the CLI's single-product context.

This aligns with the CLI's friendly-id-first behavior: the user sees `#42` on the card and types `boss task show 42` to look it up, and they should hear the same number in chat.

## Coordinator

The engine's coordinator (`bossctl`) may probe this session
between turns. Treat probes as you would a question from a
human reviewer — short, specific answers.
