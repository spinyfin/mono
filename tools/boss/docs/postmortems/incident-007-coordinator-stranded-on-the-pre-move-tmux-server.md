# Incident 007 — Coordinator stranded on the pre-move tmux server

- **Date:** 2026-08-25
- **Severity:** High — total loss of the operator control plane. No data loss; no worker affected. One coordinator conversation discarded to restore service.
- **Status:** Service restored 18:58 UTC. Fix in mono#2821. Caused by a remediation for [incident 006](incident-006-coordinator-pane-blank-after-host-reboot.md).

## 1. Summary

The coordinator pane rendered blank again, two days after incident 006 and for an unrelated reason — one this repository introduced while fixing that incident.

Incident 006's follow-up work moved Boss's private tmux server off the label-addressed default socket in `/tmp` and onto an explicit socket under the durable state root (#2816), so that a boot-time `/tmp` clear could never again strand the coordinator. That change also replaced the attach protocol's server _label_ with a socket _path_: the engine sends `tmux_socket_path`, and the app runs `tmux -S <path> attach-session`. There is no longer any way to name `-L boss` to the app.

A coordinator session created before that upgrade was still alive on the old `-L boss` server, because nothing tears the coordinator down between app restarts. On the next Boss start, the new engine found it and did exactly what #2816 told it to: route this lifecycle call through the server that actually holds the session, so as not to spawn a second coordinator on top of a live one. That routing is a correct instinct guarding a real invariant. But the handle it returns addresses `-L boss`, and `request_coordinator_attachment` requires a socket path:

```rust
let Some(tmux_socket_path) = tmux.socket_path().map(|path| path.display().to_string()) else {
    tracing::error!("coordinator tmux attach skipped: handle has no socket path");
    return;
};
```

So the engine could probe, health-check, and manage the legacy coordinator perfectly well, and could never attach to it. Worse, the legacy session satisfied every liveness check, so `restart_if_dead` concluded the coordinator was healthy and left it alone. Nothing created a replacement, and nothing removed the session that was blocking one. The result was an **absorbing state**: two log lines every ten seconds, forever, across any number of restarts.

```
WARN  coordinator tmux: a live coordinator session survives on the pre-move -L boss server
ERROR coordinator tmux attach skipped: handle has no socket path
```

The shape is worth naming, because it is the same shape as incident 006 one layer up: a recovery path that could observe the problem but not act on it, and a state no amount of restarting could leave.

## 2. The irony: this one needed the absence of a reboot

Incident 006 was triggered by a host reboot clearing `/tmp`. This incident requires that **not** to have happened.

The stranded session lives on the `-L boss` server, whose socket is in `/tmp`. A reboot would have destroyed it, and the engine would then have created a healthy coordinator on the durable socket with no trouble at all. The bug is only reachable in the window where an old coordinator survives an engine upgrade — that is, on a machine that upgraded Boss without rebooting since before #2816 landed.

That is what made the operator's initial hypothesis ("possibly moving the socket") right about the cause and understandably wrong about the trigger. It also explains why the migration path was never exercised: it requires a specific ordering — coordinator created on the old scheme, engine upgraded underneath it, no reboot in between — that no test and no ordinary development cycle produces, since developers restart and rebuild constantly.

Confirmation that no reboot occurred: the legacy tmux server was still running as pid 28085 with a session created `Sun Aug 23 13:30:05`, two days earlier. A reboot cannot leave that process alive.

## 3. Timeline

All times UTC on 2026-08-25 unless noted.

| Time           | Event                                                                                                                                                                                |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 08-23 19:26    | Incident 006's fix merges (#2812).                                                                                                                                                   |
| 08-23 20:30    | A coordinator session is created on the `-L boss` server (the pre-move scheme) while restoring service during incident 006.                                                          |
| 08-24          | #2816 merges: the private tmux server moves to `<state root>/tmux.sock`, and the attach protocol's server label is replaced by a socket path.                                        |
| (before 08-25) | Boss is upgraded to a build containing #2816. The Aug 23 coordinator session keeps running on the old server — nothing tears it down.                                                |
| 08-25 ~18:54   | Operator restarts Boss. The new engine finds the surviving legacy session, routes to it, and refuses to attach for want of a socket path. Pane blank.                                |
| 18:54–18:58    | The supervisor repeats the same pair of log lines every ten seconds. No convergence is possible; restarting Boss cannot help.                                                        |
| ~18:55         | Operator opens a standalone Claude Code session to investigate, control plane being unavailable.                                                                                     |
| 18:57:57       | Last `attach skipped: handle has no socket path`.                                                                                                                                    |
| 18:58:0x       | Investigator removes the stranded session: `tmux -L boss kill-session -t boss-coordinator`.                                                                                          |
| 18:58:07       | The supervisor's next pass finds no legacy coordinator, creates one on the durable socket, and attaches. `attached app Boss pane to coordinator tmux session`. **Service restored.** |

Restoration took one command once the log had been read. The diagnosis came from the engine's own trace log, which named the failure exactly — including the diagnostic line added as a remediation for incident 006 (§6.4 there). That worked as intended.

## 4. Impact

- Operator control plane unavailable for roughly four minutes of engine uptime, plus however long the machine sat restarted before the operator noticed.
- The Aug 23 coordinator conversation was discarded. It was already unreachable — the app cannot attach to a legacy-server session — and would not have survived the next reboot regardless, since that server's socket lives in `/tmp`.
- No data loss. No work item, execution, or PR affected. No worker affected.

## 5. Root cause

A migration that changed how a resource is _addressed_ did not change how a pre-existing resource is _disposed of_.

#2816 correctly handled three of the four cases. Worker sessions on the legacy server got a real boot-time drain (`drain_legacy_label_server`). New coordinators are created on the socket. Lifecycle operations route to whichever server holds the session. The fourth case — a coordinator that already existed on the old server — got routing but no migration, and routing alone cannot terminate: the routed-to handle is one the attach path structurally cannot consume, and nothing else would ever remove the session.

The coordinator was excluded from the worker drain deliberately (`tmux_adoption` skips `COORDINATOR_SESSION_NAME`, because its lifecycle is owned by `coordinator_tmux`), which is defensible. The gap is that the owner it was handed to implemented routing rather than migration.

## 6. Fix

`resolve_active_handle` is replaced by `migrate_legacy_coordinator_if_present`. When a coordinator session is found on the pre-move `-L boss` server, the engine now removes it and lets the ordinary lifecycle create a replacement on the durable socket, rather than routing at it forever.

This preserves the invariant the routing was protecting — there is never a moment with two live coordinators, because the removal happens before any creation — while actually converging. It is precisely what had to be done by hand to restore service, so the fix is to make the engine do automatically what an operator otherwise must.

Two details worth calling out:

**Removal does not require a matching token.** The normal teardown path, `kill_session_verified`, refuses on a token mismatch — a good rule that stops one execution tearing down another's session. Applied here it would reintroduce the wedge: an unrecognized token would leave a session that cannot be attached to and cannot be removed, which is the whole failure. So a token mismatch falls back to `kill_legacy_label_session`, a new method that refuses on a socket handle and is therefore usable only against the pre-move server. Verified teardown remains the only entry point for the durable server.

**Four tests were replaced, not deleted.** The previous tests asserted the routing contract (`resolve_active_handle_routes_to_the_legacy_server_when_a_coordinator_survives_there`, and an `ensure_for_attach` test asserting a legacy coordinator is recovered _in place_). That contract is the defect, so those tests encoded the bug as the specification. They are replaced by tests for the migration contract, including the anti-wedge case: a legacy session whose token does not match is still removed.

## 7. Detection

Detection was again a human noticing a blank pane, and again the engine had logged the exact cause on the first failing pass and every ten seconds after. Incident 006's follow-up list already carries the fix that would close this: `restart_failures` is incremented only in the coordinator supervisor's success branch, so an unbounded run of failures never trips the restart-ceiling attention. That item is filed and unstarted. This incident is the second occurrence of "the engine knew, and nothing said so."

Note also that this failure would not have tripped that attention even once fixed, because the supervisor was not failing — `restart_if_dead` was returning `Ok(None)` ("healthy, nothing to do") on every pass. The pane was blank while the coordinator's own health check reported success. An alarm on repeated _attach_ failure is a separate signal from an alarm on repeated _restart_ failure, and only the second is on the follow-up list.

## 8. What went well

- The trace log named the failure precisely, on the first pass, in two lines. Diagnosis took minutes.
- Restoration was one command, and the fix is a codification of it.
- The engine never spawned a second coordinator against the same `state.db`. The invariant #2816 was protecting held throughout — the routing did its job; it just had no exit.

## 9. What went badly

- **A remediation for one incident caused the next one, four days later.** #2816 was filed off incident 006 as a hardening measure, and shipped with a migration path for workers but not for the coordinator.
- **The migration path was untested, and structurally so.** This is the same finding as incident 006 §4.1, one layer up: reaching the failing state requires an upgrade across a specific boundary with no reboot in between, which no test constructs and no development cycle produces. A migration whose trigger condition is "a resource created by the previous version is still alive" cannot be validated by any test that builds its world from scratch.
- **A blank pane still has no alarm.** Two incidents, same detection mechanism: a human looked at the window.

## 10. Follow-ups

1. **Alarm on repeated coordinator attach failure**, distinctly from restart failure. Both incidents presented as a blank pane with a healthy-looking supervisor; only an attach-side signal catches that. This is not covered by the existing restart-ceiling follow-up from incident 006.
2. **Audit #2816 for other pre-move resources without a disposal path.** Workers were drained and the coordinator was routed; confirm nothing else was addressed by label and left to a routing decision.
3. **Establish a pattern for migration testing.** Both this and incident 006 §4.1 failed for want of a way to construct "state created by the previous version." A fixture that stands up the old-scheme artifact and then runs the new code against it would cover a class, not a case.
4. **Consider making an unusable handle unrepresentable.** The proximate defect is that a `Tmux` handle whose `socket_path()` is `None` reached a consumer that requires one, and the type system permitted it. A separate type for the legacy handle — usable for probe and teardown, never accepted by attach — would have made this a compile error rather than a runtime `return`.
