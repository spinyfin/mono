# Incident 006 — Coordinator pane blank after a host reboot

- **Date:** 2026-08-23
- **Severity:** High — total loss of the operator control plane for ~20 minutes. No data loss; no worker was affected.
- **Status:** Service restored 18:30 UTC. Both root causes fixed in mono#2812. One latent defect (§6.2) had not yet caused an outage and would have become severe once workers move to tmux.

## 1. Summary

After a macOS host restart on 2026-08-23, Boss's coordinator terminal pane rendered permanently blank. The engine was healthy, the app was healthy, and the pane's own plumbing was healthy — but the coordinator's tmux session was never created, so there was nothing for the pane to attach to. Because the coordinator is the operator's only way to talk to Boss, the control plane was entirely unavailable. The operator worked around it by driving a standalone Claude Code session (this investigation) instead.

Two independent defects are involved, and only the first caused the outage.

**Root cause 1 — a tmux error string that Boss does not recognize (the outage).** `boss_tmux::is_absent_session_stderr` classifies three tmux stderr shapes as "there is nothing here": `can't find session`, `session not found`, and `no server running`. macOS clears `/tmp` on boot, which deletes Boss's private tmux socket at `/tmp/tmux-<uid>/boss` outright. With the socket file _gone_ rather than merely stale, tmux emits a fourth shape Boss had never seen:

```
error connecting to /private/tmp/tmux-501/boss (No such file or directory)
```

That falls through to `command_failed`, so `Tmux::list_sessions` returns `Err` instead of an empty inventory. Every coordinator recovery path funnels through that one call, so the engine could never reach the branch that recreates the session. It retried every 60 seconds and failed identically every time.

**Root cause 2 — tmux sanitizes Boss's field delimiter when the engine has no locale (latent).** `list_sessions` asks tmux for `#{session_name}\t#{@boss_spawn_token}` and splits on the TAB. tmux's `server_client_print()` passes output through `utf8_sanitize()` for any client it does not consider UTF-8 capable, and `utf8_sanitize()` rewrites every non-`isprint()` byte to `_` — the TAB included. tmux decides UTF-8 capability from the client's locale, and the engine is launched by LaunchServices with no `LANG`/`LC_ALL`/`LC_CTYPE` at all. So every `list_sessions` call that returns at least one row fails to parse.

This one did not cause the outage — during the outage window the tmux server had zero sessions, so there were no rows to mangle — but it is still firing on the operator's machine as of this writing, once a minute, and it also disables the worker tmux adoption sweep.

## 2. Why a _host_ restart, when Boss had been restarted many times without incident

This is the question the incident turns on, and the answer is that a host restart does three things at once that an application restart does none of.

**Boss restarts were survivable because nothing that mattered was reset.** The coordinator tmux session is deliberately independent of the app — `server.rs` supervises it "so an exited Claude child is recreated by the engine even while the app is closed." The engine is likewise long-lived; it is only stopped on app exit when `BOSS_ENGINE_STOP_ON_EXIT=1`, which is not the default. So quitting and relaunching Boss left the tmux server running, the coordinator session alive, the socket in place, and — critically — the _same engine process_ with whatever environment it was originally started with. `reconcile_existing` found a live session with a matching token and simply reattached.

**A host restart resets all three at once:**

1. **`/tmp` is cleared.** The tmux socket file disappears. This is what flips tmux's error text from `no server running on <socket>` (which Boss handles) to `error connecting to <socket> (No such file or directory)` (which Boss did not). This alone is sufficient to cause the outage, and it is the only one of the three that a Boss restart can never reproduce — a Boss restart leaves the socket file on disk even after the tmux server exits, which is precisely why the "no server running" branch was the only one that had ever been exercised in practice.
2. **The long-lived engine dies.** Its replacement is spawned by the Finder-launched app rather than adopted from a CLI autostart, so for the first time in a fortnight the engine inherits launchd's environment — with no locale, arming root cause 2 (§2.2).
3. **The pre-existing coordinator session is destroyed**, so there was nothing to adopt and the engine was forced down the create path — the path that was broken.

The two root causes therefore have the same underlying shape and the same trigger: **Boss inherits an impoverished environment from LaunchServices, and depends on `/tmp` surviving.** Both assumptions hold across app restarts and break across host restarts, which is why 100+ Boss restarts were clean and the first reboot was not.

### 2.1 "Surely the client has a locale?"

It does not, and the disbelief is warranted — this is one of the more counter-intuitive corners of macOS. Reading the live engine's environment during the incident:

```
$ ps -E 865 | tr ' ' '\n' | grep '=' | cut -d= -f1 | sort
_  __CF_USER_TEXT_ENCODING  __CFBundleIdentifier  ANTHROPIC_API_KEY  BOSS_APP_PID
BOSS_BIN_DIR  COMMAND_MODE  HOME  LaunchInstanceID  LOGNAME  OLDPWD  OSLogRateLimit
PATH  PWD  SECURITYSESSIONID  SHELL  SHLVL  SSH_AUTH_SOCK  TMPDIR  USER
XPC_FLAGS  XPC_SERVICE_NAME
```

Twenty-two variables, and not one `LANG` or `LC_*`. The set is a textbook launchd GUI session — `XPC_SERVICE_NAME`, `XPC_FLAGS`, `LaunchInstanceID`, `SECURITYSESSIONID`, `__CFBundleIdentifier`, `COMMAND_MODE`, `OSLogRateLimit`. (`PATH` is present in its augmented form because the app repairs it explicitly.)

The reason is that **`LANG` on macOS is set by your terminal emulator, not by the operating system.** Terminal.app and iTerm2 each have a "set locale environment variables on startup" preference, on by default, and they synthesize `LANG` from the user's region at shell startup. Nothing else does. launchd does not, so no GUI-launched application has one, and neither does any CLI process it spawns. Every shell any of us has ever opened has had `LANG` set, which is exactly why its absence is surprising — the variable is ubiquitous in the environment where we look at environments, and absent in the one where Boss actually runs.

macOS does propagate the encoding, just not through the channel tmux reads. `__CF_USER_TEXT_ENCODING` is present in the list above; that is Core Foundation's private mechanism, consumed by `CFLocale`/`NSLocale`. The system knows the user's locale perfectly well — `defaults read -g AppleLocale` returns `en_US` on this machine — it simply never expresses it as a POSIX environment variable. A Cocoa app is unaffected. A POSIX CLI tool spawned by one, like tmux, sees the C locale and behaves accordingly.

So this is not a broken or unusual machine. **Every** Finder-launched Boss that spawns its own engine spawns a locale-less one, and every tmux command that engine issues is sanitized. Why that had not bitten before this reboot is §2.2.

### 2.2 Where the locale used to come from

Boss is always launched from Finder. That single fact settles this, by elimination rather than by direct evidence, and it is worth setting out the chain because two earlier drafts of this section guessed and were wrong.

1. A Finder launch gives the app launchd's environment, which contains no `LANG`/`LC_*` (§2.1). An engine spawned by the app inherits that. So **an app-spawned engine can never have a locale** — not sometimes, never.
2. The engine running during this incident was app-spawned: `BOSS_APP_PID` is present in its environment, which `server.rs` documents as something "the app always sets." And it behaves exactly as a locale-less engine must — a failed `list_sessions` on every supervisor pass, once a minute, indefinitely.
3. Engines running before the reboot did not behave that way. The archived log covering 2026-08-20 to 08-21 spans about thirty-four hours with a coordinator session alive (nothing tears it down between app restarts) and the supervisor present (#2727, 2026-08-18). A locale-less engine polling that session would have logged on the order of two thousand parse failures. It logged **one**.
4. Therefore those engines were not spawned by the app.
5. The only other way an engine starts is the CLI's transparent autostart: "the CLI transparently starts the engine — the engine is always needed to track work," on by default and gated by `--no-engine-autostart`. Such an engine inherits the invoking shell's environment, and every shell has a locale — terminal emulators set `LANG` themselves (§2.1), including the Ghostty panes that worker sessions run `boss` commands from.
6. The app does not then displace it. `EngineProcessController` checks for a running engine, fingerprints its binary against the bundled one, and on a match logs `[engine version-check ok] running=… matches bundled — attaching to pid=…`.

The steady state was therefore: some `boss` invocation — a worker session, a terminal — starts the engine with a good locale; that engine outlives every subsequent app restart; the Finder-launched app attaches to it rather than replacing it; the coordinator session it creates is enumerated correctly. Nothing was ever launched by hand, and the locale still arrived. It is an emergent property of autostart-plus-attach, which is why no one would have predicted the dependency and why it is invisible in any single component's design.

A host reboot is the one event that breaks the chain. The CLI-started engine dies with the host, and the first thing up afterwards is the Finder-launched app, which finds no engine and spawns one itself. That engine was the first app-spawned — and therefore first locale-less — engine in a fortnight, and it was the one that had to handle the reboot.

Step 5 is the one link established by elimination rather than by a record: nothing logs which path started a given engine, or what locale it inherited. §9 recommends fixing that, so the next occurrence is a lookup rather than a reconstruction.

It is worth noting that the app _already knows_ about the LaunchServices problem. `EngineProcessController.swift:531` explicitly repairs `PATH` for the engine, with a comment naming "launchd GUI session PATH (`/usr/bin:/bin:/usr/sbin:/sbin`)" as the reason. The locale was simply not considered at the time. The gap was in the generalization, not in the awareness.

## 3. Timeline

All times UTC on 2026-08-23. Anchors are from `engine-trace.jsonl`.

| Time        | Event                                                                                                                                                                                                 |
| ----------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| ~18:05      | Operator restarts the host. `/tmp` is cleared, destroying `/tmp/tmux-501/boss` and the coordinator tmux session. The long-lived engine dies with the host.                                            |
| 18:10:14    | A fresh engine starts, spawned by the Dock-launched `/Applications/Boss.app`. It has an augmented `PATH` but no `LANG`/`LC_ALL`/`LC_CTYPE`.                                                           |
| 18:10:37    | App registers. `ensure_for_attach` → `reconcile_existing` → `list_sessions` → `Err` (ENOENT). `failed to create or recover coordinator tmux session`. Pane blank.                                     |
| 18:10–18:19 | Coordinator supervisor retries every 60s. Identical ENOENT failure each pass. No self-recovery is possible.                                                                                           |
| 18:14:34    | Operator relaunches Boss. Same failure.                                                                                                                                                               |
| ~18:15      | Operator opens a standalone Claude Code session to investigate, having no working control plane.                                                                                                      |
| 18:19:05    | Investigator creates a `boss-socket-keepalive` tmux session on the private server as a mitigation attempt, intending to restore the socket so the engine could list it.                               |
| 18:19:27    | Failure mode _changes_: the server now has a row, so root cause 2 activates. `unexpected tmux list-sessions row: "boss-socket-keepalive_"`. Still no recovery.                                        |
| 18:26:58    | Operator relaunches Boss again. Fails — now on the parse error rather than ENOENT.                                                                                                                    |
| ~18:29      | The keepalive's `sleep 600` ends. Its tmux server exits with no sessions left, **leaving the socket file behind**. tmux's error text reverts to `no server running`, which Boss classifies correctly. |
| 18:30:05    | Supervisor's next pass gets an empty inventory, takes the create path, and succeeds. `attached app Boss pane to coordinator tmux session`. **Service restored.**                                      |
| 18:30:07 →  | Root cause 2 continues firing once per minute against the now-live coordinator session, and disables the worker adoption sweep. Ongoing until the fix ships.                                          |

Service was restored as an indirect side effect of the mitigation rather than by it: the keepalive did not make the engine list successfully, but its _expiry_ left behind the socket file whose absence was the actual problem. Had the investigator created the keepalive with a longer lifetime, the outage would have continued, in a different disguise.

## 4. Impact

- Operator control plane fully unavailable ~18:05–18:30 (~25 min, of which ~20 min with an engine running and failing).
- No data loss. No work item, execution, or PR was affected.
- No worker was affected. Workers do not yet use tmux, so the broken adoption sweep had no user-visible consequence.
- Two operator restarts of Boss were spent on a failure that no restart could fix, plus the investigation time.

## 4.1 Neither path was ever tested, and neither could have been

Both defects sat in code that is well covered by the usual measures. `boss_tmux` has a substantial test module; `is_absent_session_stderr` has dedicated tests; `parse_session` is exercised on every `list_sessions` test. Coverage did not help, and would not have helped at any percentage, because in both cases the untested thing was not a branch — it was an assumption about what tmux does.

**The fixtures encode the same incomplete model as the code.** `is_absent_session_stderr` recognized three stderr shapes, and the test suite asserted exactly those three: `no_private_server_is_an_empty_session_inventory` uses `no server running on …`, and the kill-session tests use `can't find session` and `no server running`. Every string in a fixture is a string someone already thought of. The fourth shape was missing from the code because it was missing from the author's model of tmux, and it was missing from the tests for precisely the same reason. A test written from the same head as the code cannot discover an unknown unknown; it can only pin down what is already believed. That is not a lapse in test-writing discipline — it is the ceiling of what fixture-based tests can do about a contract with an external program.

**No test in the repository has ever executed the tmux binary.** Every test in `tools/boss/tmux` drives a `StubRunner`; the engine's tmux tests drive `FakeTmuxServer`, `FakeTmux`, `ScriptedTmux`, `RecordingTmuxRunner`. All of them return hand-authored rows, and every one of those rows contains a real TAB — because the author wrote the row the way tmux is documented to emit it. Root cause 2 is invisible to that entire apparatus by construction: the sanitization happens inside tmux, in a code path selected by the _calling process's environment_, and a stub cannot model a behaviour it never invokes. One hundred percent line and branch coverage of `parse_session` would have found nothing.

The injectable `CommandRunner` seam is what makes this layer pleasant to test, and it is also what made both defects unreachable. That trade is usually worth it, but it means the tmux boundary has no test that would fail if our beliefs about tmux were wrong — and both root causes here are exactly that.

**The reboot state is unreachable without a reboot.** The triggering combination — a committed coordinator record, no tmux server, and no socket file — cannot be produced by quitting and relaunching Boss, because an app restart leaves the socket file on disk. It cannot be produced by killing the tmux server either, for the same reason. It requires `/tmp` to be cleared, which in practice means restarting the machine. Nobody reboots as a test step, and no automated test does either, so in thirteen days of daily use the `no server running` branch was exercised constantly and the ENOENT branch was never exercised once — not in CI, not in manual testing, not accidentally.

**The alarm for this failure was untested in the failing direction.** §5 covers the mechanics: `restart_failures` is only incremented on success, so the ceiling attention cannot fire on a hard-failing coordinator. There is no test asserting that repeated failures raise it. The safety net had the same shape as the bug — plausible on reading, never exercised in the state that matters.

The common thread across all four is that the untested paths were the ones nobody had a way to reach. Reaching them is the work; see §9.

## 5. Detection

Detection was entirely manual: the operator noticed a blank pane. The engine logged the true cause, with a fully diagnostic message, on the very first failing pass at 18:10:37 and once a minute thereafter — but nothing surfaced it. `raise_coordinator_restart_ceiling_attention` exists for exactly this situation and did not help, because the ceiling is driven by `restart_failures`, which is only incremented in the `Ok(Some(record))` **success** branch (`server.rs`); the `Err` branch that was firing every pass leaves the counter at zero forever. A hard-failing coordinator is precisely the case the attention was meant to catch, and it is the one case that cannot trip it.

## 6. Fixes

### 6.1 Recognize a missing socket as an absent server (the outage)

`is_absent_session_stderr` now also accepts `error connecting to …(No such file or directory)`. Deliberately ENOENT only: `Permission denied` or `Connection refused` describe a socket that _does_ exist and is not ours to assume is empty, and must keep surfacing as real errors.

### 6.2 Give tmux a UTF-8 locale (the latent defect)

`boss_command_runner::RealCommandRunner` now sets `LC_CTYPE=UTF-8` on every child it spawns, unless the process already has a UTF-8 locale to pass down. Fixing it in the one production spawner rather than at the `Tmux` layer means every tmux call site is covered at once — not just `list_sessions`, but `capture_pane`, whose output is silently corrupted by the same sanitizer wherever it contains control characters. `LC_CTYPE` rather than `LANG`/`LC_ALL` pins the character encoding without imposing a language or region on the child.

The `CommandRunner` trait was left unchanged; threading an env parameter through it would have touched roughly fourteen implementations across boss and cube for no additional benefit.

### 6.3 Make the parse failure self-diagnosing

A sanitized row is indistinguishable from a session whose name simply contains an underscore, which is what made root cause 2 hard to see. `parse_session` now names the locale cause when a row has no TAB but does contain `_`.

### 6.4 Record the launch environment instead of inferring it

Every question §2.2 had to answer by elimination was a question the engine could simply have written down. It now does. At startup the engine logs `engine starting (launch environment)` with how it was started (`launched_by=app` when `BOSS_APP_PID` is present, which only the macOS app sets, otherwise `standalone`), the inherited `LC_ALL`/`LC_CTYPE`/`LANG` verbatim (distinguishing unset from empty), whether any of them names a UTF-8 charset, and what was forced onto children. A missing UTF-8 locale additionally logs a warning naming the tmux consequence.

`bossctl doctor tmux` reports the same locale fields next to the tmux version, since a usable tmux binary is only half of "tmux works". It reports the invoking shell's locale, not the engine's — the engine's own startup line remains authoritative for the engine — and says so.

Verified in both directions:

```
$ bossctl doctor tmux
tmux ready: /opt/homebrew/Cellar/tmux/3.6a/bin/tmux 3.6
locale (this shell): LC_ALL=<unset>,LC_CTYPE=<unset>,LANG=en_US.UTF-8
locale charset: UTF-8

$ env -u LANG -u LC_ALL -u LC_CTYPE bossctl doctor tmux
locale (this shell): LC_ALL=<unset>,LC_CTYPE=<unset>,LANG=<unset>
locale charset: none inherited — children get LC_CTYPE=UTF-8 forced
```

## 7. What went well

- The engine's trace log contained the exact failing command, its exit code, and its verbatim stderr on the first failing pass. Once someone read it, root cause 1 took minutes.
- The failure was fail-safe: the engine refused to guess, and never destroyed or recycled a session it could not enumerate.
- `remain-on-exit` and the token-mirror discipline meant there was no ambiguity about what was and was not a real session.

## 8. What went badly

- **The investigation began by guessing.** The investigator diagnosed root cause 1 correctly from source, then spent roughly fifteen minutes trying to nudge the live system into recovering — including the keepalive session, which actively changed the failure signature mid-incident and made the operator's second relaunch fail for a _different_ reason than the first. Reading `engine-trace.jsonl` first would have produced both root causes in a fraction of the time. The forensic surfaces existed and were documented; they simply were not consulted.
- **A mitigation was applied to a live system without understanding its effect.** Creating a session on the private tmux server was reasoned about for blast radius (would the sweep reap it?) but not for whether it could change the failure mode. It did.
- **The one alarm designed for this failure cannot fire on it** (§5).

## 9. Follow-ups

Item 3 shipped with the fix (see §6.4). The rest are filed as chores against the Boss product and are not in mono#2812.

1. **`restart_failures` is incremented in the success branch** of the coordinator supervisor (`server.rs`), so five _successful_ restarts trip the failure ceiling while an unbounded run of failures never does. Fixing this is what would have alarmed on this incident.
2. **`TmuxPreflight::Unavailable => continue`** in the same loop skips the pass without touching `delay`, hot-spinning the supervisor whenever tmux is unavailable.
3. ~~**Log how the engine was started and what locale it inherited.**~~ **Done — shipped with the fix (§6.4).** §2.2's central link had to be established by elimination because nothing recorded either fact; it is now recorded at startup.
4. **Add at least one test that executes the real tmux binary** against a scratch server label, asserting that a `list-sessions` row round-trips with its delimiter intact. Every existing tmux test drives a stub returning hand-authored rows, so the entire suite is blind to what tmux actually does (§4.1). One integration test would have caught root cause 2 on the day it was written.
5. **Audit Boss's other LaunchServices-environment assumptions.** `PATH` was repaired; locale was not. Nothing systematically checks for the next one. A startup probe that logs the engine's inherited environment would make the class visible.
6. **Consider not depending on `/tmp` for the private tmux socket.** A socket under the Boss state root would be unaffected by a boot-time clear, and would make this entire class of reboot-only failure impossible rather than merely handled.
7. **Add an end-to-end test that exercises the reboot shape** — no socket, no server, an existing coordinator record — since that combination is unreachable by any app-level restart and was therefore never covered.
