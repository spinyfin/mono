# bossctl

`bossctl` is the Boss-only control CLI for the Boss V2 engine. It is the
low-level admin and coordinator counterpart to the user-facing `boss`
command: where `boss` manages the _work taxonomy_ (products / projects /
tasks / chores), `bossctl` owns the _control verbs_ used to inspect and
steer the running system — worker sessions, the dispatch pipeline,
engine metrics, the remote-host registry, and diagnostic logs.

It is deliberately privileged. `bossctl` is run by the coordinator
session inside the Boss libghostty pane; worker sessions do not have it
on `PATH`, and that absence is part of how the engine distinguishes
Boss-tier requests from ordinary worker traffic.

## Architecture

`bossctl` is a single `clap`-derived binary organised as a tree of
subcommands. Each verb resolves into one of two backends:

- **Engine RPC.** Most steering verbs (`agents`, `probe`, `work`,
  `workspace`, `reveal`, `metrics reset`, `metrics show --live`) open a
  `BossClient` from `boss-client` and call into the long-running
  `boss-engine` daemon. These require a live engine — they act on the
  engine's in-memory `LiveWorkerState` registry and its scheduling and
  dispatch machinery.
- **Direct file/DB reads.** Diagnostic verbs that must work when the
  engine is _wedged_ (`metrics list`, `metrics show`, `hosts *`,
  `dispatch *`, `live-status debug`, `logs`) read the engine's `state.db`
  or its on-disk JSONL event/log streams directly, bypassing RPC. This
  is intentional: an operator needs to inspect a stuck engine, and the
  file-scan path stays available even when the socket does not answer.

A recurring concern is **worker reference resolution**: an `agent`
argument may be a run id, a numeric slot id, a crew name (e.g. `Riker`,
matched case-insensitively over currently-live slots), or a friendly
work-item id (`T42`, `P7`). Names and slot ids resolve only against live
slots so a typo cannot silently match a closed run; bare run ids may
fall through to the persisted historical `WorkRun` record.

`selected-product` answers the question that reference resolution
cannot answer on its own: **which product is the operator looking at?**
Short IDs are scoped per product, so resolving one against the wrong
product does not fail — it returns a real row, with a real status and a
real PR, for the wrong work item. The macOS app reports its product
chooser to the engine (`report_selected_product`, accepted only from the
registered app session) and the engine is the system of record; this
verb reads it back. It never falls back to a default or first product:
when there is no answer it exits non-zero with the specific case
(`app_not_connected`, `no_selection`, `product_unknown`), because a
guess here is worse than a refusal. The selection is in-memory and
session-scoped, so it dies with the app that reported it rather than
outliving the UI it describes.

The `agents` family steers individual workers — listing live slots (including
their durable tmux session, token-adoption state, pane-death flag, and last
terminal output time), showing status, printing the exact safe attach command
with `agents attach <execution>`, focusing or stopping a pane, sending user-typed text,
interrupting a turn, launching or reaping an execution, and dumping the
recent transcript (text / raw JSONL / engine-rendered markdown). `probe`
**interrupts the worker's current turn** and delivers the coordinator's
prompt there and then: it sends the driver's own interrupt gesture, waits
for the turn to actually end, writes the text, and reports the settled
delivery state — so the command blocks for a few seconds and exits non-zero
when the text did not land. Interrupting is the default because the reason
to probe a running worker is almost always to redirect it _now_, and the
boundary a waiting probe watches for may be the run's last. `--no-interrupt`
opts back into boundary delivery for a message that genuinely can wait,
since an interrupt aborts whatever the worker was mid-way through.
`--urgent` is queue priority, not a different transport. `work` mirrors `boss`'s
dispatch verbs for symmetry. `dispatch` and `live-status` expose the
internals of the dispatch pipeline for triage when a work item never
reaches a worker pane.

## How it fits

`bossctl` depends on `boss-protocol` for the shared domain types and RPC
shapes, on `boss-client` for the typed engine client, and on
`boss-engine` for engine-side helpers (transcript rendering, state-root
resolution, and the `state.db` schema it reads directly). It is a
top-level binary — nothing else in the monorepo depends on it. Verbs
whose engine-side surface does not yet exist return a structured
`not_implemented` response rather than failing, so the coordinator can
discover which controls are still pending.
