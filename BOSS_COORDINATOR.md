# Coordinator guidance for Boss (spinyfin/mono)

This file is read by the Boss **coordinator** session only, for work on the Boss product. Workers never see it; their rules are in `AGENTS.md`, `CLAUDE.md`, and `.claude/CLAUDE.md`. Keep this file to rules that are specific to developing Boss with Boss and that a coordinator needs in order to file good briefs, review results, or investigate the product. Generic coordinator behaviour belongs in the generic prompt (`bossSystemPrompt` in `tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift`), not here.

## Investigating Boss's own engine: snapshot first, never the live DB

When an investigation concerns Boss's own engine history (runs, executions, attention items, dispatch decisions, audit trail), take a **read-only snapshot of the engine state database before briefing an agent**, and point the agent at the snapshot. Never let an agent — background subagent or cube worker — open or query the live engine's `state.db`: the engine holds it open in WAL mode, a reader can block its checkpoints, and anything an agent reads from the live file is volatile by the time it is briefed.

Make the snapshot with SQLite's online backup so it is consistent, into a temp path outside Application Support:

```sh
sqlite3 "$HOME/Library/Application Support/Boss/state.db" ".backup '/tmp/boss-state-snapshot-$(date +%Y%m%dT%H%M%S).db'"
```

Put the snapshot path, the time it was taken, and the schema tables of interest in the brief. For provenance questions ("who deleted this row", "which surface changed this"), start from `engine-audit.log` rather than the DB; `tools/boss/docs/forensic-surfaces.md` says what each surface can answer and how long it is retained.

Cube workers cannot read any of this — `~/Library/Application Support/Boss/` is off limits to them. If a chore needs engine evidence, inline the relevant rows or log lines verbatim into the brief.

## Exercising Boss itself in a brief

Never ask a worker or agent to launch the installed `/Applications/Boss.app`, `open -a Boss`, or start an engine that can reach production state: this is someone's laptop, and an unisolated launch puts a window on their screen and kills the engine they are using. Briefs that need a running engine must direct the worker to the isolated-engine recipe in the repo's `.claude/CLAUDE.md` (a non-production `--socket-path`, `BOSS_EVENTS_SOCKET` unset) and the quiet `--capture-to` screenshot path for UI checks.

## Build and validation gates to put in every Boss brief

- Bazel is the only build and test path in this repo. Briefs say `bazel build` / `bazel test`; they never offer `cargo`, a `bazel-bin/` binary, or `bazel run` on a test target as an alternative, and they never ask for cache bypasses.
- Pre-push lint is `checkleft run` with no flags. Do not brief `checkleft --all` outside CI or a change to checkleft itself.
- Root `CHECKS.yaml` forbids Boss work-item id shapes in PR text, commit messages, and changed source lines. Describe work by subject in briefs so workers do not echo an id into a PR.

## Where Boss deliverables land

- Design docs: `tools/boss/docs/designs/`. Investigation writeups: `tools/boss/docs/investigations/`. Postmortems: `tools/boss/docs/postmortems/`. Operator runbooks: `tools/boss/docs/runbooks/`. Name the directory in an investigation or design brief so the worker does not invent a new one.
- Operational contracts a brief can cite instead of re-deriving: worker liveness (`tools/boss/docs/worker-liveness-contract.md`), attention lifecycle (`tools/boss/docs/attention-lifecycle.md`), post-crash recovery (`tools/boss/docs/post-crash-recovery.md`), coordinator session handoff (`tools/boss/docs/coordinator-session-handoff.md`), and this file's own mechanism (`tools/boss/docs/coordinator-product-guidance.md`).

## Changing this file

A durable Boss-specific coordinator lesson goes here, via a chore against the Boss product that quotes the rule verbatim. Do not grow the generic prompt with it, and do not put it in `AGENTS.md`. The engine caps this file at 32 KiB; keep it a set of rules, not a manual.
