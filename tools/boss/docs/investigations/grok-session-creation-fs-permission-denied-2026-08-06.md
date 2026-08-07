# Grok session creation fails `FS_PERMISSION_DENIED`, parking the pane at the start menu

- **Date:** 2026-08-06
- **Kind:** incident root-cause analysis + fix (`tools/boss/engine/driver/src/grok/`)
- **Observed build:** Grok `0.2.118 Beta`, `grok-4.5 (medium) · always-approve`; Boss.app 1.0.502, binaries dated 2026-08-04 11:41
- **Related:** [grok-permission-isolation-2026-07-27.md](./grok-permission-isolation-2026-07-27.md) (the Seatbelt posture this regressed against)

## Symptom

A dispatched Grok worker's pane came up, and the CLI printed:

```
Session creation failed: Permission denied.: {
        "code": "FS_PERMISSION_DENIED",
    "detail": "Operation not permitted (os error 1)"
}
```

then sat on its start menu (`New worktree` / `Resume session` / `Changelog` / `Quit`) with an empty prompt. No session was ever created, so the spawn command was never consumed and nothing ran. `bossctl agents status` showed the slot live with `activity: spawning` and a real `shell_pid`, holding its interactive slot and its cube lease.

## Verdict (read this first)

| Question                                          | Answer                                                                                                                                                    |
| ------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| What path is denied?                              | `$GROK_HOME/sessions/…` — a **symlink** whose target is inside Boss's state root, which the Boss-owned Seatbelt profile denies `file-read*`/`file-write*` |
| Deterministic or intermittent?                    | **Deterministic** for every local macOS Grok worker on a build containing mono#2662; it cannot succeed and cannot fail intermittently                     |
| Same seam as the earlier sandbox fixes?           | Same profile, new collision. The scoped `HOME` and IOKit fixes are intact and unrelated — this is the Boss-data-dir fence meeting a new symlink           |
| Did the engine's driver-start detection cover it? | **Yes, and it is shipped.** It is not the gap. The gap was that nothing checked this _before_ spawning                                                    |

## Root cause

Two independently correct changes composed into a denial.

**1. The fence.** `render_macos_seatbelt_profile` (mono#2639, 2026-08-03) fences Boss's data directory at the kernel level:

```lisp
(deny file-read* file-write* (literal "…/Boss") (subpath "…/Boss"))
```

`boss_data_dir` is `events_socket_path.parent()`, which in production is `$HOME/Library/Application Support/Boss` — the whole Boss state root, holding `state.db`, the events socket, and the control token.

**2. The symlink.** mono#2662 (2026-08-04, "Retain isolated worker transcripts durably") made every isolated driver home's `sessions` directory a symlink into that same state root, so a killed or orphaned worker's transcript survives its temporary home:

```
$GROK_HOME/sessions → <state root>/executions/<run_id>/transcripts/grok/sessions
```

**Seatbelt matches the path the kernel resolves, not the path the process names.** So a write to `$GROK_HOME/sessions/<session>.json` is evaluated against the link's target inside the fenced state root — and denied — even though `$GROK_HOME` itself is a granted `(subpath …)` writable root. The denial surfaces as `EPERM`, whose `strerror` text is exactly `Operation not permitted`, which the Grok CLI reports as `FS_PERMISSION_DENIED` / `os error 1`.

### Confirmed against the kernel

Reproduced with the real profile shape and the real symlink layout, outside Boss:

```console
$ /usr/bin/sandbox-exec -f profile.sb /usr/bin/touch grokhome/sessions/session.json
touch: grokhome/sessions/session.json: Operation not permitted
$ /usr/bin/touch grokhome/sessions/session2.json        # same write, no sandbox
$
```

This is now a test — `permissions::tests::seatbelt_grants_the_sessions_link_target_without_unfencing_the_state_root` — which drives the _rendered_ profile through `sandbox-exec` and asserts all three behaviours: denied without the grant, allowed with it, and the rest of the state root still fenced.

### Confirmed against the live CLI

The same two profiles, driving real `grok 0.2.118` (the incident build) on the affected host, with a real `$GROK_HOME/sessions` symlink into a stand-in state root. Without the grant:

```console
$ sandbox-exec -f before.sb grok -p 'Reply with exactly: OK' --always-approve --trust --sandbox off …
Couldn't create session: Permission denied.: {
  "code": "FS_PERMISSION_DENIED",
  "detail": "Operation not permitted (os error 1)"
}
```

Byte-for-byte the reported failure. With the grant, the same invocation completes a turn and lands its session in the durable directory:

```console
$ sandbox-exec -f after.sb grok -p 'Reply with exactly: OK' …
OK
$ ls boss-data/executions/run-live/transcripts/grok/sessions
%2F…%2Fws
```

And the fence is unchanged around everything else — under the _granted_ profile:

```console
$ sandbox-exec -f after.sb touch boss-data/state.db
touch: …/state.db: Operation not permitted
$ sandbox-exec -f after.sb cat boss-data/secret.txt
cat: …/secret.txt: Operation not permitted
$ sandbox-exec -f after.sb touch boss-data/executions/other-run/transcripts/grok/sessions/x
touch: …/other-run/…/sessions/x: Operation not permitted
```

The grant is one run wide: a sibling execution's transcript directory stays denied.

### Why it is deterministic, and why earlier Grok workers were fine

Nothing about this varies by workspace, host, slot, first-run-vs-resume, or concurrency: both the fence and the link are rendered identically for every local macOS Grok worker. Grok workers that ran successfully ran on builds predating mono#2662. The installed bundle's binaries are dated 2026-08-04 11:41, i.e. at/after that landing — which is why the failure appeared when it did rather than gradually.

## Fix

**1. Grant precisely the denied path.** `render_macos_seatbelt_profile` now takes this run's resolved sessions destination and emits, _after_ the fence (Seatbelt takes the last matching rule):

```lisp
(allow file-read* file-write* (literal "<…>/executions/<run_id>/transcripts/grok/sessions") (subpath …))
```

One directory wide — this run's sessions directory and nothing else. `state.db`, the events socket, the control token, and other runs' artifacts stay denied, as do the CLI-level `Read`/`Edit` belts in `structural_deny_rules`: those govern what the _agent_ may touch through its tools, a separate question from what the Grok process needs in order to persist its own transcript.

The path is **resolved and verified** (`transcript_store::resolve_durable_sessions_dir`), never reconstructed. Granting a guessed path would be worse than granting none: the sandbox would fence off the real destination while looking correct.

**2. Probe it before spawning.** Boss already ran a fail-fast capability preflight under the exact Seatbelt profile the pane will use, but it only checked `grok models`, `cube`, `gh`, and `jj` — nothing established that Grok could write its own session state. The preflight now performs a **real write** through `$GROK_HOME/sessions` under that profile and fails with the kernel's own text if it is denied.

This is the general guard, and the reason it is a write rather than a permission calculation: any future fence that shadows the session store — not just this one — turns into a pre-spawn error naming the path, before a pane, slot, or cube lease is committed.

## Why the engine did not surface it (and what was actually missing)

The engine-side detection for "the pane acked, the driver never started" is `spawn_ack_sweep` **pass 2**, added in mono#2560 (2026-07-30) and hardened in mono#2620 (2026-08-03). Both predate the running build's 2026-08-04 binaries, so it is **shipped, not merely merged**.

It also genuinely covers Grok. Pass 2 reads `unverified_driver_starts`, which consults only `driver_signal_at` and `spawned_at` — blind to `shell_pid`, to `activity`, and to the driver's capability set. `spawn_ack_sweep_induced_failure_tests` already demonstrates the whole path end to end against a **real** OS process: the three older guards each decline (dead-pid finds the shell alive; `mark_stalled_spawns` declines because Grok omits `AwaitingInputSignal`; pass 1 declines because `shell_pid > 0`), then pass 2 fires, marks the execution `orphaned`, reaps the pane, releases the pool slot, force-releases the cube lease, and raises an attention item.

So this pane was on a 300s (`DRIVER_START_GRACE_SECS`) fuse the whole time; the operator's `bossctl agents stop` simply beat it. The observation "`spawning` indefinitely" is what the state looks like _inside_ that window, not evidence the window never expires.

What that leaves is a real but different gap, and it is the one fixed above: **detection is not diagnosis.** Pass 2 can only ever report "no driver signal in 300s" — it never sees `FS_PERMISSION_DENIED`, because the text lives in the pane's scrollback and the driver never emitted an event. A deterministic permission fault should not cost a pane, a slot, a lease, and a five-minute fuse to discover; it should fail at provisioning with the underlying error attached. That is what the preflight probe now does.
