# Incident 009 — Boss tmux server segfault on a focus-out after kill-session

- **Date:** 2026-10-30; all timeline times are CDT (UTC−05:00), the host's local zone.
- **Impact:** Boss's tmux server died with `SIGSEGV` at 11:21:39, taking every tmux session with it at once: all running workers and the coordinating Claude session. One implementation run, two PR-review runs and one PR review-guide run were orphaned. The implementation run then sat for roughly 15 minutes as an `active` row with nothing running and nothing visible explaining why, until it was relaunched by hand at about 11:37. The engine itself stayed up. Separately, at 11:54:59 a diagnostic repro of the bug crashed inside the app's process tree and made Boss.app exit with no crash report (§4.5).
- **Status:** Service restored within seconds (new tmux server at 11:21:48); the held implementation run was relaunched manually. The underlying tmux bug is still present on the installed tmux (Homebrew 3.6a). A mitigation in the engine is in progress (see §8). This document changes no code.
- **Provenance:** The macOS crash report for the tmux server, engine logs and the engine audit trail for the incident window, disassembly of the installed tmux binary, tmux 3.6a source, a reproduction on a private tmux server, and Boss source.
- **Source baseline:** `main` at `bf00a35fd4ae` (mono#3411). All Boss `path:line` references below refer to this revision; tmux references are to the 3.6a release source.

## 1. Summary

Boss runs every worker and its own coordinating Claude session inside a single tmux server on a private socket. At 11:21:39.38 that server — Homebrew tmux 3.6a, running since 2026-10-23 15:34 — crashed with a NULL-pointer dereference in `tty_keys_next`. Every session died at the same instant.

The crash is a tmux bug. When a session is killed, tmux sets `c->session = NULL` on every client still attached to it. If one of those clients then sends a focus-out sequence (`ESC [ O`) before it exits, tmux 3.6a's key handler dereferences `c->session->curw` and the server segfaults. Boss turns `focus-events` on server-wide, and the Boss app's panes are real tmux clients attached to worker sessions, so Boss provides both halves of the trigger. Two hundred milliseconds before the crash, the engine killed the session of a worker whose spawn had just failed; the most likely sequence (**inference**) is that the app's pane for that slot lost focus as the app reacted, and its focus-out arrived at the server after the kill.

Upstream fixed this on 2026-09-25 ([tmux@7a816f9](https://github.com/tmux/tmux/commit/7a816f90e7087d8738901a40c03df08f50c55fcc), "Discard keys if there is no session, fixes crash…"). The fix ships in tmux 3.8, not 3.7c.

### Verdict

The root cause is an upstream tmux crash that Boss's configuration and app architecture reliably expose: Boss kills sessions that still have app clients attached, and those clients send focus events. The blast radius — every worker and the coordinating session at once — comes from running everything in one tmux server. The recovery gap that followed — an orphaned implementation run held invisibly for 15 minutes — is a separate Boss defect in how crash recovery behaves while dispatch is paused.

## 2. Evidence and method

- **Crash report.** macOS wrote `tmux-2026-10-30-112146.ips` for pid 6021: `EXC_BAD_ACCESS`, `SIGSEGV`, `KERN_INVALID_ADDRESS at 0x00000000000000d8`. Faulting backtrace: `tty_keys_next+3036 ← tty_read_callback+132 ← event_base_loop ← proc_loop ← server_start`. The binary UUID in the report matches the installed Homebrew 3.6a binary.
- **Disassembly.** At the faulting offset the instructions are `ldr x8,[x20,#0xa98]; ldr x8,[x8,#0xd8]`, i.e. load `c->session`, then load `session->curw`. A fault address of `0xd8` means `c->session` was NULL.
- **tmux source.** That load is in the `KEYC_FOCUS_OUT` branch of `tty_keys_next` (`tty-keys.c`, 3.6a:979-982), and `server_destroy_session` (`server-fn.c`, 3.6a:456-467) is what sets `c->session` to NULL (see §4).
- **Engine logs and audit trail** for 11:13–11:40: the worker spawn into slot 1 and its failure, the session kill, the coordinating session's liveness checks, the shell-pid probes, the stale-worker sweep, and the orphan sweep's per-minute pause skips.
- **Reproduction** on a private tmux server, never the Boss socket (§4.4).

That the app's pane sent the focus-out is an **inference**. The crash requires a focus-out from a client whose session has just been destroyed; the only session destroyed in the window was the failed worker's, 200 ms earlier, and the app's pane for that slot was attached to it. Nothing logs focus events, so no record pins the sender.

## 3. Timeline

| Time (CDT)       | Observation                                                                                                                                                                                                                                   |
| ---------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 2026-10-23 15:34 | The Boss tmux server (Homebrew tmux 3.6a) starts. It runs continuously until the crash.                                                                                                                                                       |
| ~11:13           | The engine restarts for unrelated reasons and re-adopts the existing tmux sessions. Global dispatch is deliberately paused at this time.                                                                                                      |
| 11:21:19         | A worker spawn into slot 1 starts. Its pane stays blank.                                                                                                                                                                                      |
| 11:21:36         | The coordinating session's liveness check (`tmux_session_exists`) passes for the last time.                                                                                                                                                   |
| 11:21:39         | The spawn fails: "driver composer never became ready within 20s".                                                                                                                                                                             |
| 11:21:39.18      | The engine's `reap_tmux_worker` kills that worker's tmux session.                                                                                                                                                                             |
| 11:21:39.38      | The tmux server segfaults in `tty_keys_next`. Every session dies.                                                                                                                                                                             |
| 11:21:46         | The engine finds the coordinating session `already_gone` and recreates it.                                                                                                                                                                    |
| 11:21:48         | A new tmux server starts on the Boss socket.                                                                                                                                                                                                  |
| 11:21:53–54      | Probes of worker shell pids return `ESRCH`.                                                                                                                                                                                                   |
| 11:21:56         | The stale-worker sweep reports each worker's "tmux session … is absent from list-sessions" and reaps the executions "running → orphaned". The reconcile audit text claims the row was reset for redispatch; no such transition happened (§5). |
| 11:21:56 onward  | PR-review recovery re-fires the two orphaned PR-review runs. The orphan sweep declines to redispatch the orphaned implementation run because dispatch is paused, and repeats that every minute. The row stays `active` with no execution.     |
| 11:36            | The person watching the board notices the implementation run is not running.                                                                                                                                                                  |
| 11:36:53         | Last pause skip for that run by the orphan sweep.                                                                                                                                                                                             |
| ~11:37           | The implementation run is relaunched manually.                                                                                                                                                                                                |
| ~11:54:59.988    | During diagnosis, a repro's private tmux server (pid 36764, a descendant of the app) segfaults with the same bug (§4.4, §4.5).                                                                                                                |
| 11:54:59.98      | Boss.app (pid 67090) exits via `exit(1)` with no crash report (§4.5).                                                                                                                                                                         |
| 11:55:03         | Boss.app is relaunched from the Dock.                                                                                                                                                                                                         |

## 4. Root cause

### 4.1 The tmux bug

In tmux 3.6a, `tty_keys_next` handles a focus-out from a client like this (`tty-keys.c`, 3.6a:979-982):

```c
if (key == KEYC_FOCUS_OUT) {
    c->flags &= ~CLIENT_FOCUSED;
    window_update_focus(c->session->curw->window);
```

There is no check that the client still has a session. `server_destroy_session` (`server-fn.c`, 3.6a:456-467) runs when a session is killed and, for every client attached to it, either moves the client to another session (when `detach-on-destroy` allows) or sets `c->session = NULL` and marks it `CLIENT_EXIT`. Boss does not set `detach-on-destroy`, so the client is left with a NULL session until it actually exits. Any focus-out that the server reads from that client in the meantime crashes the server.

The upstream fix ([tmux@7a816f9](https://github.com/tmux/tmux/commit/7a816f90e7087d8738901a40c03df08f50c55fcc), 2026-09-25) discards keys from a client with no session. It is in tmux 3.8. It is not in 3.7c, and Homebrew stable now offers 3.8.

### 4.2 How Boss supplies the trigger

- **Focus events are on.** Boss sets `focus-events on` as a server option (`tools/boss/engine/core/src/tmux_session_options.rs:29`), so tmux asks attached clients' terminals to report focus changes.
- **The app's panes are tmux clients.** Each pane runs `tmux … attach-session -t <session>` (`tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift:67`, `tools/boss/app-macos/Sources/Ghostty/WorkersWorkspaceModel.swift:345`). Their terminal emits `ESC [ I` / `ESC [ O` as the pane gains or loses focus.
- **The engine kills sessions with clients still attached.** `reap_tmux_worker` (`tools/boss/engine/core/src/app/tmux_teardown.rs:47`) runs `kill-session` on a failed or finished worker's session without first detaching the app's client.

Any time the app's pane loses focus in the short window between that kill and the client's own exit, the server crashes. The window is small, which is why this took a week of uptime to hit, but nothing in Boss closes it.

### 4.3 The trigger on 2026-10-30

At 11:21:19 a worker spawn into slot 1 started and its pane stayed blank. At 11:21:39 it failed ("driver composer never became ready within 20s"), and `reap_tmux_worker` killed its session at 11:21:39.18. The server died 200 ms later, at 11:21:39.38, reading a focus-out. The most likely sender (**inference**) is the app's pane for slot 1, losing focus as the app reacted to the failed spawn.

### 4.4 Reproduction

On a private tmux server (tmux 3.6a, separate socket, `focus-events on`), with a client attached to a session named `victim`:

1. Stop the client with `SIGSTOP`, so it cannot exit when its session goes away.
2. `kill-session -t victim`.
3. Write `ESC [ O` into the client's terminal.

The server died with the same frames as the incident (`tty_keys_next+3036`, fault address `0xd8`). A control run that wrote `a` instead of `ESC [ O` at step 3 survived. The repro produced its own crash report, `tmux-2026-10-30-115502.ips` (see §6.2), and its crash made Boss.app exit (§4.5).

### 4.5 Secondary incident: the repro's crash made Boss.app exit (11:54:59)

Boss.app (pid 67090) exited at 11:54:59.98 via `exit(1)`: not a crash, and not killed. launchd logged "exited due to exit(1), ran for 2470006ms", and loginwindow logged `kLSNotifyApplicationAbnormalDeath`. There is no Boss crash report, and the app logged nothing in its last 7 seconds. The person watching the board relaunched it from the Dock at 11:55:03.

**Cause (investigated).** The diagnostic repro's private tmux server (pid 36764) segfaulted at about 11:54:59.988 with the same `tty_keys_next` bug. It used a private socket, but it was still a process descendant of the app: app → engine → Boss tmux server → pane → diagnostic agent → repro script → tmux. Its crash report shows `"coalitionName": "dev.spinyfin.bossmacapp"` and `"responsiblePid": 67090`.

**Mechanism (investigated).** On macOS, child processes inherit their parent's Mach exception ports across `fork`/`exec`. Boss.app's exception handler is Breakpad, linked via sentry-native in GhosttyKit. Breakpad's `exception_handler.cc` `WaitForMessage` does:

```c
if (!breakpad_exc_server(&receive.header, &reply.header))
  exit(1);
```

That is the only exit path consistent with the evidence; the handler's other path writes a minidump, and none was written. Why `exc_server` rejected the message is a **guess (unverified)**: it rejected the kernel's message for another process's exception on macOS 27.0.1. The 11:21 crash was reported as being in a different process tree and did not take the app down.

**Consequence.** Any segfault anywhere in the app's process tree (engine, tmux, worker panes, agents, builds) can make the app quit with no crash report.

## 5. Contributing factors

- **One tmux server is a single point of failure.** Every worker and the coordinating session share one server, so one crash ended all of them at once.
- **Nothing watches the tmux server.** Boss records no tmux server pid, start time or exit. The engine learned of the crash only indirectly, through sessions disappearing, 7 seconds later (11:21:39 → 11:21:46 for the coordinating session, 11:21:56 for workers). Attribution required finding the macOS crash report by hand.
- **Crash recovery silently held the implementation run while dispatch was paused.** The orphan sweep checks the global pause before redispatching and skips the row, counting it under `dispatch_paused_skipped` (`tools/boss/engine/core/src/orphan_sweep.rs:880-905`). It did that every minute from 11:21:56 to 11:36:53. The row stayed `active` with no execution, no visible wait reason and no attention item, so nothing on the board distinguished it from a running worker.
- **PR-review recovery and implementation recovery disagree about the pause.** The two orphaned PR-review runs were re-fired automatically during the same pause that held the implementation run. One path treats recovery as exempt from the pause and the other does not.
- **The reconcile audit text overstated what happened.** The dead-worker audit line (`tools/boss/engine/core/src/dead_pid_sweep.rs:691`) claimed the row was reset for redispatch. No such transition occurred, which made the audit trail actively misleading about why nothing was running.
- **The PR review-guide run was never retried.** It was orphaned along with the others and no recovery path picked it up.

## 6. What went well, what went poorly

### 6.1 Went well

- The engine survived the crash and recreated the coordinating session within about 10 seconds.
- PR-review recovery re-fired the two orphaned PR-review runs without help.
- The fresh coordinating session picked up its context from the handoff the previous session had written, so nothing had to be re-explained.
- The macOS crash report, a read of the tmux source and a private repro pinned the root cause within about 30 minutes, including the upstream fix and the first release that carries it.

### 6.2 Went poorly

- **The held run went unnoticed for 15 minutes.** It was found by a person looking at the board, not by any signal Boss raised.
- **Detection relied on side effects.** Without tmux server monitoring, the first signals were "session already gone" and `ESRCH`, not "the tmux server crashed".
- **Repro artifacts were not attributable.** The diagnostic repro at 11:55 produced a second crash report (`tmux-2026-10-30-115502.ips`) that was briefly mistaken for a recurrence on the Boss server. Repros need names or locations that make them unmistakable.
- **The repro took the app down.** The repro ran on a private socket but inside the app's process tree, so its crash made Boss.app exit (§4.5). Using a separate socket was not enough isolation.

## 7. Lessons

- Boss depends on tmux as infrastructure, so tmux crashes are Boss outages. The tmux version and the server's lifecycle need the same visibility as the engine's.
- Killing a session that still has interactive clients attached is a sharp edge on any tmux version; detach first.
- Crash repros must run outside the app's process tree (detached via launchd, a `launchctl asuser`-style launch, or a separate terminal), not merely on a private socket. Inherited exception ports tie a child's crash to the app.
- Recovery that is deliberately held must look held. An `active` row with nothing running and no reason attached reads as a bug to whoever is watching the board, and audit text must describe what actually happened rather than what the code intended.

## 8. Action items

**In progress:**

- **Detach before kill, and warn on affected tmux versions.** The engine detaches every attached client before each `kill-session`, which removes the exposure on any tmux version. It also detects affected tmux versions at start and adoption and shows a visible warning banner in the app. Per the repo owner's decision it warns and never refuses to start.

**To do:**

- **Upgrade tmux to 3.8 and restart the Boss tmux server at a quiet moment.** Restarting the server ends every session, so it must be timed. Install with `HOMEBREW_NO_INSTALL_CLEANUP=1` so the old binary path stays valid for the running server until the restart.

**Proposed, not yet scheduled:**

- **Monitor the tmux server.** Record the server pid and start time, watch it for exit, and on exit emit an event with the pid, uptime and recent tmux commands, linking the matching macOS crash report.
- **Make held crash recovery visible.** When crash recovery declines to redispatch because dispatch is paused, leave a visible held state instead of an `active` row with nothing running, and make the reconcile audit text describe what actually happened. Open design question: should crash recovery of explicitly started work bypass the pause, as PR-review recovery already does?

- **Stop the app's process tree inheriting its exception ports.** When the app spawns the engine, clear inherited exception ports: `posix_spawn` with `posix_spawnattr_setexceptionports_np(attr, EXC_MASK_ALL, MACH_PORT_NULL, EXCEPTION_DEFAULT, THREAD_STATE_NONE)`. Foundation's `Process` cannot do this, so that launch moves off it. Optionally do the same for engine → tmux spawns. Regression test: a child segfaults and the parent survives.

**Already tracked (related):** the board shows revisions awaiting review like primary tasks. During the incident, a revision whose run had completed and was waiting on automated review looked stuck, which added to the confusion over what was and was not running.

**Rejected fixes:**

- Turning off `focus-events`. It hides the trigger rather than removing it, and loses focus reporting that panes rely on.
- Swallowing focus sequences in the app or engine. Same objection, and it would have to track every sequence tmux treats as a key.
- Silently auto-restarting the tmux server without recording why it died. That repeats this incident's worst detection gap.
- Refusing to start the engine on an affected tmux version. A warning is enough; refusing to start turns a latent crash into a guaranteed outage.
