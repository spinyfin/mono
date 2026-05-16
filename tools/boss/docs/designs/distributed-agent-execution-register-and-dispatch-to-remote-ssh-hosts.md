# Distributed Agent Execution: Register and Dispatch to Remote SSH Hosts

## Overview

Boss V2 today runs every worker locally on the coordinator's machine. The user has spare capacity on other machines — initially `zakalwe`, a second macOS host — and wants Boss to opportunistically use them to widen the worker pool. This design adds a registry of SSH-reachable remote hosts, a capability-aware scheduler, and a remote dispatch path that presents remote runs identically to local runs in the macOS app.

The shape:

- Each registered host has a fixed cube-managed worker pool and a set of capability tags.
- The engine's existing coordinator picks a host whose capabilities satisfy the chore's requirements, then leases a workspace on that host and spawns a worker there.
- Local dispatch is the `host_id = "local"` special case of remote dispatch — the same `HostAdapter` interface, the same execution and run records, the same hook-event path.
- The remote needs cube + claude + gh + the per-project toolchain. No Boss-specific daemon. SSH transport reuses the existing event-shim and events-socket protocol unchanged.

This design covers v1 only: static host registration, no hot add/remove, no auto-provisioning, no sandboxing beyond what local workers already get.

## Goals

- Boss can register a remote machine via SSH alias and dispatch workers there.
- A remote run looks identical to a local run in the macOS app: same kanban, same agent surfaces, same transcript, same probe / interrupt / stop semantics.
- Scheduling load-balances across local + remote hosts, while respecting host capability tags so chores never land on a host that cannot build them.
- The first concrete deliverable lets the user register `zakalwe` and watch Boss schedule a chore there end-to-end.
- The remote-host footprint stays minimal: cube + claude + gh + project toolchain. No Boss daemon.
- Workspace identity remains host-qualified in durable Boss state; no host-local filesystem paths leak into Boss's interpretation layer.

## Non-Goals

- Building Boss's own credential or SSH-key management. The user configures SSH out of band — agent forwarding, key auth, ssh-config aliases — and Boss assumes connections to a registered alias succeed without prompts.
- Multi-tenant remote pools. One user only.
- Sandboxing or isolation hardening beyond what already applies to local workers.
- Auto-provisioning of remote toolchains (bazel, xcode, claude auth). The user installs them.
- Hot add/remove of hosts mid-run. Static registration is fine for v1; a re-register on disable/re-enable is acceptable.
- Re-architecting hook events, probe, or transcript streaming. This design reuses the existing surfaces unchanged and threads them across an SSH boundary.
- Cross-host workspace migration. A run that starts on host A stays on host A until it completes or fails.

## Existing Surfaces Reused

Before the design proper, this is what the design intentionally does not change:

- `cube workspace lease / release` JSON contract — used on the remote exactly as it is locally.
- `events.sock` and `event-shim` — the worker writes hook events through the shim to a Unix socket. The remote's shim is pointed at an SSH-forwarded socket and is otherwise unaware it is remote.
- Engine RPC and the macOS-app rendering of agent surfaces — they read in-process state that the coordinator already maintains; the coordinator simply tracks which host a run lives on.
- The `work_executions` / `work_runs` schema from `work-execution.md`. This design adds columns and one new table, not a parallel execution model.

## Alternatives Considered

### Alternative A: Pure SSH exec, no remote daemon (chosen)

Engine SSHes to the remote and runs `cube workspace lease … && claude …` directly, with the events socket SSH-remote-forwarded back to the engine. Probe / interrupt go over a second SSH channel multiplexed via `ControlMaster`. The remote runs no Boss-specific long-lived process.

Pros:

- Reuses the existing local worker stack wholesale: same event-shim binary, same socket protocol, same claude launch shape. The "remote" axis becomes one new `HostAdapter` implementation; everything above it is unchanged.
- No new binary to design, version, distribute, secure, or recover when it crashes.
- Failure mode is well-understood: when SSH dies, the remote process group dies via `SIGHUP` (assuming a proper TTY-less session and `ServerAliveInterval`).
- The SSH-forwarded Unix socket is a transparent stand-in for the local socket; the remote shim does not need to know it is remote.
- Matches the user's original intuition. The user explicitly relaxed the no-daemon constraint, but only as a way to unlock a meaningfully simpler design — Alternative B does not meet that bar (see below).

Cons:

- Probe / interrupt / stop now depend on a second SSH channel rather than a local IPC call. `ControlMaster` mitigates the cost (one persistent connection, cheap channel mux) but the engine has to track the master socket and reconnect on drop.
- Hook-event JSONL tailing (`~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`) is engine-local today. For remote workers the engine has to read that file over the SSH channel, either by tailing via `ssh host tail -f` in a subprocess or by routing the same data through the forwarded events socket.
- Transient SSH drops require explicit reconnect and run-state reconciliation. With a remote daemon this would be a single RPC retry.

### Alternative B: Minimal remote relay daemon (`boss-host`)

A small Boss-aware daemon on the remote brokers between engine and worker. The daemon exposes a typed RPC over the SSH channel for `spawn / list / signal / probe / stream-transcript`. The daemon owns subprocess lifecycle, transcript reads, and event forwarding.

Pros:

- Clean lifecycle boundary on the remote. The daemon survives transient SSH drops and reaps zombies.
- Typed RPC is a clearer contract than "wrap the right SSH invocations."
- Probe / interrupt / stop become single RPCs.

Cons:

- Adds a new binary that must be built, versioned, distributed, kept compatible across engine upgrades, and recovered when it crashes itself. None of that infrastructure exists today.
- Duplicates what the engine already does for local workers: subprocess management, transcript tailing, event normalization. The daemon would either be a thin SSH-replacement (in which case it adds no value) or a parallel mini-coordinator (in which case it is a substantial new component).
- Reverses the "fewer moving parts" win that motivated the relaxation in the first place.

Rejected because the implementation-clarity bar is not cleared: the daemon's only real wins are reconnection ergonomics and probe-as-RPC, both achievable in Alternative A with `ControlMaster` + a focused reconnect path.

### Alternative C: Pull model — remote asks engine for work

Remote periodically polls the engine for work and reports back. Avoids inbound SSH for streaming.

Pros: works behind NAT / firewalls without inbound SSH.

Cons: adds latency for every interaction (spawn, probe, hook event); inverts the coordinator's event-driven model; requires the remote to know how to authenticate to the engine; complicates "interrupt right now"; provides no benefit for the home-network case that motivates this design.

Rejected because Boss V2 today is single-user, single-LAN, single-coordinator. The pull model solves a problem the user does not have.

## Chosen Approach

### Architecture

```
                ┌─────────────────────────────────────────┐
                │  coordinator host (engine + macOS app)  │
                │                                          │
                │  ┌──────────┐    ┌────────────────────┐ │
                │  │ engine   │───▶│ HostAdapter trait  │ │
                │  │ (coord)  │    │  ├─ LocalAdapter   │ │
                │  └────▲─────┘    │  └─ SshAdapter[N]  │ │
                │       │          └────────────────────┘ │
                │       │  events.sock + transcript-tail  │
                │       │  (live worker state surface)    │
                └───────┼─────────────────────────────────┘
                        │
                        │  SSH (ControlMaster mux)
                        │   • stdio channel  → cube/claude
                        │   • -R sock        → events.sock (engine-side)
                        │   • -R sock        → transcript-tail readback
                        │   • control channel → probe / interrupt
                        │
                ┌───────▼─────────────────────────────────┐
                │  remote host (e.g. zakalwe)              │
                │                                          │
                │  cube workspace lease ──▶ workspace path │
                │       │                                   │
                │       ▼                                   │
                │   claude ──▶ event-shim ──▶ forwarded sock │
                │       │                                   │
                │       ▼                                   │
                │   bazel / xcodebuild / gh ──▶ GitHub     │
                └─────────────────────────────────────────┘
```

The remote runs exactly the same component set as the local worker: `cube` leases a workspace, `claude` runs inside it, `event-shim` forwards hook events to a Unix socket. The only difference is that the socket on the remote is the local end of an SSH remote-forward whose far end is the engine's `events.sock`.

### Q1 — Worker Spawn Transport

**Decision:** pure SSH exec via the engine's `SshHostAdapter`, multiplexed over an `OpenSSH ControlMaster` connection.

The `HostAdapter` trait abstracts spawn / probe / interrupt across local and remote. The local adapter does what the engine does today. The SSH adapter:

1. Ensures a `ControlMaster` connection to the alias exists (opens one if not).
2. Sets up SSH remote-forwarded Unix sockets for `events.sock` and a transcript-readback socket on the remote side, both pointing back to engine-local sockets.
3. Issues a single SSH exec for the worker: a short wrapper script that runs `cube workspace lease … --json`, exports `BOSS_EVENTS_SOCKET` and the transcript-readback path, then exec's `claude` with the rendered prompt.
4. Streams stdio back over the master channel.
5. Opens a second channel on the same master for probe / signal traffic, addressed by remote PID returned at spawn.

The wrapper script lives in the user's PATH on the remote (`~/.local/bin/boss-remote-run` or similar). It is shipped via the same out-of-band mechanism the user uses to install cube and claude — Boss does not deploy it. Its contents are stable and documented; users can update it manually when the contract changes (rare, version-pinned in the design doc).

### Q2 — Hook-Event Transport

**Decision:** SSH remote port-forwarded Unix socket; the existing `event-shim` binary on the remote sees what looks like a local socket.

Mechanism:

- Engine binds `events.sock` locally as today.
- SSH adapter opens the worker session with `-R /tmp/boss-events-<run>.sock:<engine events.sock>` so the remote sees `/tmp/boss-events-<run>.sock` as a Unix socket that forwards every byte to the engine's listener.
- The worker's shim is configured (via env var) to write to the forwarded socket.
- `peer_pid()` lookup on the engine side will see the SSH forwarder's PID instead of the worker. The engine cannot use peer pid to correlate; correlation must move to a token-in-event-envelope mechanism. This is the one piece of `events_socket.rs` that needs to change — the existing code already uses pid lookup, so we add an alternative correlation key (`run_id` token written by the shim, validated against the registry) and gate it behind a `host != local` check.

This is the smallest hook-transport change that lets `WorkerEvent` traffic flow over the network without re-architecting the protocol.

### Q3 — Cube on the Remote

**Decision:** independent cube per host; the engine treats remote cube as an opaque pool reached over SSH.

The engine SSHes and runs `cube workspace lease <repo> --task <summary> --json`. The remote's cube has its own state.db at `~/Library/Application Support/cube/state.db` and its own workspace pool at `~/Documents/dev/workspaces/`. The engine never reads those — it only stores the returned `workspace_id`, `lease_id`, and `workspace_path`, and uses the path only in subsequent SSH-side commands targeted at that same host.

Bootstrap (user-installed, out of scope for this design): the user runs `cube repo ensure --origin <repo>` on the remote, with however many workspaces they want pre-cloned. Boss reports `cube workspace lease` failures clearly (see Q6) but does not try to materialize the pool itself.

### Q4 — Repo State and PR Push Semantics

**Decision:** same GitHub remote as local workers; assume `gh` is installed and authed on the remote.

Each remote worker pushes to the same `spinyfin/mono` GitHub remote as a local worker. PR commits are authored by whatever `git config user.{name,email}` says on the remote. The user is responsible for setting that. The host-add CLI surfaces this as a checklist item printed at registration.

A future polish step could have `cube repo ensure` enforce `user.name` / `user.email` from the cube config; out of scope here.

### Q5 — Scheduling, Load Balancing, and Capability Matching

**Decision:** a `hosts` table with per-host capability tags, per-chore required tags, and a two-stage scheduler: capability filter, then free-slots-first with branch affinity as tiebreaker.

#### Host registry

`hosts` table (full schema in the "Storage Additions" section):

- `id` — short stable id, e.g. `local`, `zakalwe`. Used as the durable host attribution key.
- `ssh_target` — ssh-config alias or `user@host[:port]`. Absent for `local`.
- `pool_size` — max concurrent workers on this host.
- `enabled` — operator off-switch without deletion.
- `last_seen_at` — timestamp of most recent successful heartbeat.

A `local` host row is created on first run with `ssh_target = NULL` and capabilities auto-discovered. Local dispatch is `host_id = "local"`.

#### Capability model

`host_capabilities` rows: `(host_id, capability, source)`.

`capability` is a free-form opaque string. Recommended convention:

- `os=macos` / `os=linux`
- `arch=arm64` / `arch=x86_64`
- `xcode>=15` (signing identities, simulator runtimes)
- toolchain markers: `bazel`, `pnpm`, `cargo`
- bespoke tags users add ad hoc

`source` is `auto` or `user`. Auto-discovered tags are recomputed on registration and on the heartbeat tick (see "Reachability"); user tags persist.

#### Required capabilities for a chore

Required tags live as `work_capability_requirements` rows keyed by `(subject_kind, subject_id)` for `product`, `project`, and `chore`. Precedence (most-specific wins): **chore > project > product**. The scheduler unions the lowest-precedence non-empty set with any overrides above it — a chore inherits product-level "os=macos" unless the chore explicitly relaxes it.

This is intentionally permissive — the design picks tag set union with explicit override semantics rather than a more elaborate constraint language. If users find that insufficient, a follow-up can introduce structured operators; v1 stays flat.

#### Discovery vs declaration

Both. At `bossctl hosts add`, the engine SSHes the new host and probes:

- `uname -s` → `os=`
- `uname -m` → `arch=`
- `xcode-select -p` and version → `xcode=N`
- `which bazel`, `which gh`, `which cube`, `which claude` → toolchain tags
- `cube workspace list --json` → at least one workspace exists for the target repo? sets `cube-pool-ready`

The user may add or remove tags afterwards via `bossctl hosts tag add/remove`. Auto-discovered tags are refreshed on the periodic heartbeat; user-tagged rows are left alone.

#### Load balancing

Once the capability filter narrows the candidate set:

1. Drop any host with no free slots (`active_run_count >= pool_size`).
2. Drop any host with `enabled = 0` or `last_seen_at` older than the failed-heartbeat threshold (see Q6).
3. Among the remainder, prefer the host that previously ran a run for this execution's PR branch (branch affinity — preserves bazel disk cache). Fall back to most-free-slots, fall back to lexicographic host id for determinism in tests.

Branch affinity is the only optimization in v1. Round-robin and pinned-host-affinity-as-policy are follow-ups.

#### Reachability detection

Two-track:

- **Periodic heartbeat:** every 60s the engine runs `ssh -o BatchMode=yes -o ConnectTimeout=5 <alias> true`. Success bumps `last_seen_at`; failure increments a counter and, after three consecutive failures, marks the host `unhealthy` (computed flag, not stored — `last_seen_at` + threshold is enough).
- **Lazy on-dispatch-failure:** any SSH or cube-lease failure during dispatch immediately marks `last_seen_at` stale and surfaces an attention item; the scheduler re-evaluates and may pick a different host.

Recovery is automatic: when a heartbeat succeeds again, the host becomes eligible.

#### No-eligible-host

If the capability filter yields zero hosts, the execution does not silently sit in todo. The coordinator creates a `decision_required` attention item: "no registered host satisfies <tags>; either register one or relax the requirement," and the kanban renders the chore with an explicit "no host" badge. The execution remains in `queued` until either capabilities change or a host comes online.

#### Pin escape hatch

`work_executions.pinned_host_id` (nullable). When set, capability matching is bypassed entirely and the chore goes only to that host. Used when the user knows something the tags do not — keychain state, locally-cached secrets, an unfinished iteration on disk. Surfaced on the chore detail surface so it is not invisible.

### Q6 — Failure Modes

Each row: how Boss detects it, what Boss surfaces, whether the chore retries elsewhere.

| Failure | Detection | Surface | Retry policy |
| --- | --- | --- | --- |
| SSH connection drops mid-run | `ControlMaster` reports channel close; engine sees stdio EOF and missing heartbeat | run marked `failed` with reason `host_unreachable`; attention item on chore; `last_seen_at` invalidated | retry on a different eligible host, up to `boss.distributed.max_host_retries` (default 1). After exhaustion, leave the execution in `failed` |
| Remote out of disk (zakalwe today: pkgbuild EPIPE / ENOSPC) | run exits with disk-error pattern in transcript OR cube returns ENOSPC at lease time | run reason `host_disk_full`; host marked unhealthy until next successful heartbeat | retry on a different host once; never auto-recover the original until the user clears space and re-enables |
| `cube workspace lease` fails (no free workspaces, stale lock) | non-zero exit, JSON error | run reason `host_pool_exhausted`; attention item: "increase pool size or wait" | do not retry on the same host; retry on another eligible host if any |
| Remote `claude` missing or unauthed | wrapper script exits with a documented sentinel code | run reason `host_missing_claude`; host marked `degraded` (not removed) | do not retry on the same host; surface registration-time checklist mismatch |
| Remote `gh` missing or unauthed | gh failure detected from worker logs at PR-create time | run reason `host_missing_gh`; host marked `degraded` | same as above |
| Worker SIGKILLed on remote (OOM, logout) | `ControlMaster` channel exits non-zero with signal code; transcript ends abruptly | run reason `worker_killed`; treated as a `host_unreachable` variant for retry purposes | retry once; if it happens twice on the same host, mark host `degraded` |
| Clock skew between hosts | event timestamps drift visibly from engine receipt time | the engine **never trusts remote timestamps** for ordering; it stamps engine-receipt time on hook events and uses remote timestamps only as informational metadata | n/a; design avoids the problem rather than reconciling it |

`degraded` is a derived state, not a column: hosts with auto-discovery flags that no longer match required tags are filtered out as if they were missing those tags.

### Q7 — Live Status, Transcript, Probe

The engine's existing live-worker-state surface reads from `live_worker_state.rs` and the transcript tail. For remote workers:

- **Transcript:** the wrapper script writes `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl` exactly as today. The engine tails that file over a second SSH-forwarded readback socket: a tiny remote helper (`tail -F <jsonl>`-equivalent in stable Rust, packaged in the same wrapper artifact as `boss-remote-run`) pipes the JSONL into the forwarded socket; the engine reads it the same way it reads the local file today.
- **Probe:** `bossctl agents probe` issues a probe over the control channel. The remote-side handler delivers it to the worker's stdin (claude reads `probe` as an interactive prompt) and reports back over the same channel. Already shaky for local workers per the `bossctl_probe_doesnt_reach_live_workers` feedback — design here treats probe as a thin wrapper around the local probe path, so any local fixes inherit to remote without further design work.
- **Interrupt / stop:** the control channel sends `SIGINT` / `SIGTERM` to the remote worker's process group via `kill -<sig> <pid>`. Remote PID is captured at spawn and stored on the run.
- **Live activity dot:** computed from the same hook-event stream as today. The remote shim's events go through the forwarded socket; the engine's live-state code does not need to know which host emitted them.

The hard constraint — "from the user's perspective the surface is identical to local" — is satisfied because every surface above is fed by the same data plumbing it already uses. The remote-ness is invisible to the rendering layer; it shows up only as a "host" attribute on the run record.

### Q8 — Workspace Identity Across Hosts

**Decision:** durable workspace identity in Boss is the pair `(host_id, cube_workspace_id)`. Stored as two columns on `work_executions` and `work_runs`; never as a filesystem path.

Today's `mono-agent-003` is host-implicit. Going forward the runner stores `host_id="zakalwe"` and `cube_workspace_id="mono-agent-003"` separately. The pair is the durable identity. `workspace_path` remains on the run but is interpreted only on the host that produced it — it is a debugging aid, not durable identity.

The `cube_workspace_id_is_not_identity` feedback already calls this out for PR attribution. This design keeps the same posture: PR attribution flows through GitHub (the source of truth) and the `pr_url` snapshot on the execution.

### Q9 — Engine + Bossctl Surface Changes

New `bossctl hosts` subcommand:

```text
bossctl hosts add <id> --ssh-target <alias-or-user@host> --pool-size N \
    [--tag os=macos --tag bazel ...]
bossctl hosts list [--enabled]
bossctl hosts show <id>
bossctl hosts tag add <id> <tag> [<tag>...]
bossctl hosts tag remove <id> <tag> [<tag>...]
bossctl hosts disable <id>
bossctl hosts enable <id>
bossctl hosts remove <id>        # only if no live runs
bossctl hosts probe <id>         # one-shot heartbeat + capability refresh
```

Modified verbs:

- `bossctl agents list` gains a `host` column and a `--host <id>` filter.
- `bossctl workspace summary` groups by host or accepts `--host <id>`.
- `boss task show` includes "ran on host" for each run.
- macOS app: kanban cards and agent surfaces gain a small host badge (text id, no icon for v1). When a run is on `local`, the badge is suppressed to preserve identical-looking UI for the common case.

The `boss` CLI gains:

- `boss project set-required-capabilities --project <id> --tag os=macos ...`
- `boss chore set-required-capabilities --chore <id> --tag ...`
- `boss task set-pinned-host --task <id> --host <id>` (pin escape hatch)

Engine RPC additions are limited to the read paths the macOS app and `bossctl` need: `ListHosts`, `GetHost`, `ProbeHost`. Writes go through `bossctl` → engine RPC the same way other admin commands do today.

### Q10 — Security and Blast Radius

The host on the coordinator side SSHes to the remote and runs arbitrary commands as the SSH user. Constraints:

- **Remote never touches Boss state.** The wrapper script does not read or write `~/Library/Application Support/Boss/` (it does not exist on the remote anyway). The existing `workers_isolated_from_boss_runtime` feedback continues to hold by construction.
- **Engine validates remote input the same as local input.** `normalize_hook_event` is the validating boundary today; it stays the boundary tomorrow. The forwarded socket means a malicious remote could in principle send anything that looks like a hook event, but that is no different from a malicious local worker. The `run_id` correlation token added in Q2 must be unguessable per run; engine refuses events with unknown tokens.
- **GitHub is the source of truth for PR artifacts.** Per `github_is_source_of_truth_for_pr_artifacts`, Boss does not mirror PR contents from the remote. The remote pushes to GitHub; Boss reads from GitHub.
- **No new privileged surface on the coordinator.** Engine code that handles SSH errors must not shell out with unsanitized host fields. Host id and ssh_target are validated at insert time (regex: `^[a-zA-Z0-9._@:-]+$`).

The security posture is: nothing new on the wire that was not already exposed by the worker model; nothing new on the coordinator beyond a few SSH subprocess invocations whose arguments come from the validated `hosts` table.

## Storage Additions

```text
hosts
- id TEXT PRIMARY KEY                  # e.g. "local", "zakalwe"
- ssh_target TEXT                      # NULL for local
- pool_size INTEGER NOT NULL
- enabled INTEGER NOT NULL             # 0 or 1
- last_seen_at TEXT                    # Unix epoch seconds, decimal string
- last_error_text TEXT
- created_at TEXT NOT NULL

host_capabilities
- host_id TEXT NOT NULL REFERENCES hosts(id)
- capability TEXT NOT NULL
- source TEXT NOT NULL                 # "auto" or "user"
- PRIMARY KEY (host_id, capability)

work_capability_requirements
- subject_kind TEXT NOT NULL           # "product" | "project" | "chore"
- subject_id TEXT NOT NULL
- capability TEXT NOT NULL
- PRIMARY KEY (subject_kind, subject_id, capability)
```

Additions to existing tables (per `work-execution.md`):

```text
work_executions
+ pinned_host_id TEXT                  # NULL = no pin
+ host_id TEXT                         # populated when a run first picks a host;
                                        # subsequent runs reuse this for affinity

work_runs
+ host_id TEXT NOT NULL DEFAULT 'local'
+ cube_workspace_id TEXT               # the cube-side workspace id (not the path)
+ remote_pid INTEGER                   # for interrupt / signal addressing
```

Timestamps continue to use the Unix-epoch-decimal-string format from `work-taxonomy.md`.

No new transcript layout is required. Transcripts already live under `~/Library/Application Support/Boss/executions/<execution-id>/runs/<run-id>/` on the coordinator; remote runs write into the same path because the JSONL data is forwarded back over the readback socket and written by the engine, not the remote.

## Phased Implementation Plan

Each phase is a separately shippable PR. Phases 1–3 are the minimum to get the user's "register zakalwe and watch a chore run there" outcome.

### Phase 1: Host Registry (no dispatch change)

- Add `hosts`, `host_capabilities`, `work_capability_requirements` tables.
- Insert a `local` host row at engine first-run with auto-discovered capabilities.
- `bossctl hosts add / list / show / tag / enable / disable / remove`.
- No change to scheduler; everything still runs locally.

Outcome: the user can register `zakalwe`, declare its tags, list hosts. Nothing dispatches to it yet.

### Phase 2: `HostAdapter` Trait Refactor

- Introduce `HostAdapter` with `LocalHostAdapter` only.
- Refactor `spawn_flow.rs` and the worker/lifecycle paths to go through the adapter.
- No behavior change. This is a pure refactor in preparation for Phase 3.

Outcome: a stable adapter seam. The diff is mechanical and reviewable on its own.

### Phase 3: `SshHostAdapter` and Remote Spawn

- Implement `SshHostAdapter`: `ControlMaster` lifecycle, events-socket remote-forward, transcript-readback socket, control-channel exec, wrapper-script contract.
- Add the `run_id` correlation token to the event protocol (Q2).
- Wire the scheduler's capability filter and branch-affinity tiebreaker.
- Wrapper script `boss-remote-run` documented in `tools/boss/docs/runbooks/` (separate PR is fine).

Outcome: the user installs cube + claude + gh + the wrapper on zakalwe, runs `bossctl hosts add zakalwe …`, and a capability-matching chore lands there. PR is created identically to a local run. UI shows the host badge.

### Phase 4: Probe / Interrupt / Stop Over SSH

- Implement the control-channel handlers on the wrapper side.
- Wire `bossctl agents probe / interrupt / stop` through `HostAdapter`.
- Local probe still goes through the same trait method, so any fixes to the existing shaky probe surface apply uniformly.

### Phase 5: Heartbeat and Reachability

- Background heartbeat loop in the engine.
- `last_seen_at` updates, `hosts probe` one-shot, capability refresh on heartbeat.
- Lazy-on-failure stale-marking.

### Phase 6: Failure Classification

- The full Q6 failure-mode classifier on top of the run-failure path.
- Surface `host_disk_full`, `host_pool_exhausted`, etc. as run reasons with corresponding attention items.

### Phase 7: UI Polish

- macOS app: host badges on kanban / agent rows, host filter on agent list.
- `bossctl workspace summary --host` and `boss task show` host attribution.

### Phase 8: Capability Auto-Refresh and Retry-on-Different-Host

- Re-probe auto-discovered capabilities on the heartbeat.
- Implement the `max_host_retries` policy from Q6 properly.

## Risks and Open Questions

These should be resolved before Phase 3 ships.

- **Wrapper script distribution.** The design assumes the user installs `boss-remote-run` on each remote alongside cube and claude. If the wrapper contract changes, every remote must be re-installed. Should the wrapper instead be `scp`'d at registration time from a copy embedded in the engine? Embedding is appealing because the engine knows its own contract, but adds a deploy-on-add step. **Recommendation:** v1 ships the wrapper as a user-installed artifact; v1.1 evaluates auto-deploy after the contract stabilizes.
- **`ControlMaster` socket lifecycle on coordinator reboot.** Stale control sockets at `~/.ssh/cm-*` can break subsequent connections. Engine startup should sweep its own control sockets. Detail to nail down: socket path policy (engine-owned dir vs `~/.ssh`).
- **`peer_pid` vs `run_id` correlation.** The events socket today identifies workers by peer pid. Adding `run_id` correlation is a protocol change; existing local workers will need to start sending it too (event-shim change). Backwards-compat shim: accept either, prefer `run_id`, plan to remove pid lookup in a later cleanup.
- **Multiple repos per host.** This design assumes each host hosts one cube pool per repo, matching the current local model. If a host ever hosts pools for multiple repos, capability tags can cover it (`repo=mono`), but the registry has no explicit notion of which repos a host services. v1 punts; if it bites, add `host_repos` later.
- **`gh` auth drift on the remote.** GitHub tokens expire silently. The Phase 6 failure detection catches it at PR-create time, which is late. Should the heartbeat run `gh auth status` as part of capability discovery? Probably yes — cheap, catches the failure mode hours earlier. **Recommendation:** include in Phase 1 capability discovery.
- **Engine ↔ wrapper version skew.** If the engine ships a new event envelope format and the remote wrapper is old, hook events break. Solutions: (a) the wrapper does not interpret events, only forwards; the shim binary version matters more, so ship the shim under cube's umbrella. (b) version-handshake at SSH session start. **Recommendation:** option (a). The shim is small enough to live in the cube repo and update with cube; the wrapper stays stable.
- **Branch-affinity scope.** Affinity uses `pr_url` as the affinity key, which is unset until the first run pushes. For the very first run on a branch the engine falls back to free-slots-first. Is that good enough, or do we need a pre-PR affinity hint (e.g. project id)? **Recommendation:** good enough for v1; revisit if cold-cache cost is high.
- **What does "identical to local" actually mean for transcripts when SSH drops mid-stream?** A truncated transcript on a remote run is more likely than on a local run because the network failure mode exists. The UI should distinguish "transcript ended because run completed" from "transcript ended because SSH died and we don't know what came after," but the existing local model does not draw this distinction either. Probably a v1.1 polish item; flagging here so the reviewer knows it is not addressed.

## Related Designs

- [`work-execution.md`](work-execution.md) — the execution / run / lease model this design extends.
- [`work-taxonomy.md`](work-taxonomy.md) — work-item shape, timestamp format, repo identity.
- [`worker-live-status.md`](worker-live-status.md) — the surface that must remain identical between local and remote runs.
- [`engine-app-rpc.md`](engine-app-rpc.md) — the engine ↔ macOS app contract; host attribution flows through here.
