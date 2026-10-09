# boss-client

`boss-client` is the typed RPC client that Boss command-line tools use to
talk to the engine over its frontend Unix-domain socket. It owns the
connection mechanics — engine discovery, optional autostart, connecting,
and correlating requests with responses — so each CLI can issue
`FrontendRequest`s and get back `FrontendEvent`s without re-implementing
the wire protocol. It exists to keep that one set of rules in a single
place shared by the `boss` CLI, `bossctl`, and the engine itself.

## How it fits

The crate sits between the protocol definitions in `boss-protocol` and the
front-end binaries that drive the engine. `BossClient` is a
single-connection client: it sends a framed-JSON request envelope tagged
with a generated `request_id`, then reads engine events until it sees the
one carrying the matching id. It depends only on `boss-protocol` for the
request/event types, deliberately avoiding any dependency on `boss-engine`
itself — small on-disk shapes the engine also defines (such as the
control-token file) are duplicated here rather than imported, so a CLI
never has to pull in the whole engine crate.

`Discovery` captures everything needed to find and, if necessary, launch
the engine: the socket path, the PID-file path, whether autostart is
allowed, the resolved engine command, and timeouts. The engine-command
resolver is the most involved piece — it walks an ordered chain of sources
(explicit `BOSS_ENGINE_CMD`/`BOSS_ENGINE_BIN` overrides, a workspace
`bazel-bin` build, a sibling binary next to the running executable, and
finally a bare `boss-engine` on `PATH`), recording every step it tried so a
failed autostart can explain exactly how it got there. The resolver is
written as a pure function over explicit inputs so tests can exercise it
deterministically without touching process environment.

Stopping the engine prefers a token-authenticated `Shutdown` RPC — the same
authority the macOS app uses — and falls back to `SIGTERM` only when the RPC
path is unavailable, giving a developer a recoverable kill switch for a
wedged engine on a non-standard layout.

## Riding out an engine restart

An engine restart (an update, a crash-and-relaunch) leaves the socket
missing or refusing connections for a few seconds. `BossClient::connect`
waits that out instead of failing: connect-phase failures (socket missing,
connection refused, connect timeout) are retried with exponential backoff
(250ms doubling to a 5s cap, with jitter) for up to **10 minutes**, printing
a one-line notice to stderr on the first retry and every 15s after — never
to stdout, so `--json` output stays clean. After the budget it fails with
`EngineUnreachable`, naming the socket path and the time waited (`boss`
exits 5). Opt out with `--no-retry`, `--engine-max-wait <secs>`, or
`BOSS_ENGINE_MAX_WAIT_SECS` (`0` disables). `connect_socket` is a bare
single connection with no retry, for engine control and tests.

`send_request` also recovers from a dropped connection, but only when that
cannot apply a request twice. Only a first write that fails with zero bytes
written proves nothing was delivered; then any request is resent on a fresh
connection. Everything else — a reply that never came, a partial write, a
failed flush — _may_ have been delivered, and that suspicion is sticky across
attempts. Such a request is resent only if `replay_safety` marks it
idempotent or guarded by an engine-side check (reads, `Set*` setters,
`SubmitProposal`, `SubmitAttachment`, guarded `CreateTask`/`CreateChore`/
`CreateInvestigation` within the duplicate-guard window, …); anything else
fails with `OutcomeUnknown` — "check state before retrying" — and is never
resent. For a guarded create the window is re-checked after the reconnect,
immediately before the resend, and the reconnect wait is capped to what is
left of it; if the window closes the call fails with `OutcomeUnknown`. If
the engine's duplicate guard refuses a replayed create, the client reports
the existing item as the created one (it was almost certainly this
request's first delivery) instead of a conflict. The full classification
table is in `src/replay.rs`; unclassified requests default to _not_
resendable.

Every connect (including autostart and readiness probes) is bounded by the
remaining retry budget and a 5s per-attempt cap, so a stalled listener
cannot hold a call past `--engine-max-wait`.

Autostart keeps its meaning (`--no-engine-autostart` still forbids it). When
it applies, the client starts at most one engine per call and waits for an
engine that is already starting or restarting (live pid file) rather than
racing a second. A Boss worker (`BOSS_RUN_ID` set) never starts an engine:
the spawn itself refuses, so `boss engine start` fails there too.

The backoff and jitter arithmetic lives in `tools/boss/backoff`, shared with
`boss-http-retry`; this crate adds the IPC-specific budget, notices and
replay rules.

## Consumers

`boss-engine`, `bossctl`, and the `boss` CLI all depend on this crate to
reach a running engine; it depends on `boss-protocol` for the shared types.
