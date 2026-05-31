# Boss: Work Subscription Design

## Problem

Boss now has multiple clients that can mutate Work state:

- the macOS app,
- the `boss` CLI,
- future agents and automations.

All of those writes already go through the engine, which is correct. The
remaining gap is propagation: today the macOS app only sees direct responses to
its own requests. If a CLI command creates or updates a work item, the app does
not learn about that change until it reconnects or the human manually refreshes.

The current engine protocol also has no notion of subscriptions or unsolicited
notifications. It is effectively request/response over a long-lived socket.

That is the wrong shape for a system where:

- multiple frontends can be connected at once,
- one client can change state that another client is displaying,
- future `boss watch` or automation workflows may want live updates.

## Goals

- Make work mutations from any client show up in the UI immediately.
- Keep the engine as the single source of truth for all work state.
- Support multiple simultaneous clients over the same engine socket.
- Reuse existing read APIs such as `list_products` and `get_work_tree`.
- Create infrastructure that can later support agent and terminal topics too.

## Non-Goals

- Designing a durable event log or replay system in the first phase.
- Replacing the existing request/response reads with a patch-only model.
- Building a full CRDT or conflict-free collaborative editing system.
- Solving every future live-stream case in one step.

## Current Limitations

### No Shared Subscription Broker

The engine currently accepts socket connections and serves requests, but there
is no shared server-global topic registry. A client can receive only:

- the direct response to its own request,
- agent-stream events produced inside that same connection context.

There is no engine-owned way to say "notify every interested client that product
`prod_123` changed."

### No Correlation for Mixed Response and Push Traffic

The current protocol uses `FrontendRequest` and `FrontendEvent` as line-delimited
JSON messages. That works for simple request/response flows, but it becomes
fragile once the server may send both:

- direct responses to a request, and
- asynchronous push notifications caused by some other client.

Without request correlation, a client cannot safely distinguish "the response to
my `list_products` request" from "an unrelated `work.product` invalidation."

### UI Refresh is Local-Only

The macOS app already knows how to refresh itself:

- `sendListProducts()`
- `sendGetWorkTree(productId:)`

It already uses those reads after many of its own local writes. The missing
piece is an engine-driven trigger telling it when a different client changed the
same underlying state.

## Proposed Design

### 1. Introduce Shared Server State

Refactor the engine server around a shared `ServerState` owned by `run_server`,
rather than keeping important state inside each connection handler.

Recommended shape:

```text
ServerState
├── WorkDb
├── AgentRegistry
├── SessionRegistry
└── TopicBroker
```

Where:

- `WorkDb` remains the canonical persistence layer.
- `AgentRegistry` becomes server-global rather than per connection.
- `SessionRegistry` tracks connected clients and their outbound channels.
- `TopicBroker` owns topic membership and fanout.

This is required because a subscription system is inherently cross-connection.
The broker cannot live inside a single connection task.

### 2. Add a Framed Protocol with Request IDs

Keep the existing request and event payload enums, but stop sending them as raw
top-level messages with implicit ordering assumptions.

Instead, wrap them in envelopes:

```json
{ "request_id": "r-17", "payload": { "type": "list_products" } }
{ "request_id": "r-17", "payload": { "type": "products_list", "products": [] } }
{ "request_id": null, "payload": { "type": "topic_event", ... } }
```

Rules:

- client requests include a `request_id`,
- direct responses echo that same `request_id`,
- unsolicited notifications use `request_id = null`.

This lets a client have one long-lived connection and safely process both:

- normal replies,
- asynchronous subscription events.

### 3. Add Subscribe and Unsubscribe Requests

Extend the frontend protocol with:

- `subscribe { topics: [...] }`
- `unsubscribe { topics: [...] }`

And corresponding responses:

- `subscribed { topics: [...], current_revision: N }`
- `unsubscribed { topics: [...] }`

The initial implementation should allow a session to subscribe to multiple
topics at once and hold those subscriptions until disconnect or explicit
unsubscribe.

### 4. Start with a Small Work Topic Catalog

The first version does not need arbitrarily fine granularity.

It only needs enough topic coverage to make the current Work UI update
immediately:

- `work.products`
  Covers product list changes such as create, rename, status change, archive.
- `work.product.<product_id>`
  Covers all project, task, and chore changes under one product, plus direct
  changes to that product.

Optional future topics:

- `work.project.<project_id>`
- `work.item.<item_id>`
- `agent.list`
- `agent.<agent_id>`

The first cut should resist overfitting. `work.products` plus
`work.product.<id>` is enough to solve the actual user problem.

### 5. Publish Invalidation Events, Not Full Patches, First

The initial topic payload should be invalidation-oriented rather than a full
patch language.

Recommended notification shape:

```json
{
  "request_id": null,
  "payload": {
    "type": "topic_event",
    "topic": "work.product.prod_123",
    "revision": 42,
    "origin_session_id": "session-7",
    "origin_request_id": "r-19",
    "event": {
      "type": "work_invalidated",
      "reason": "task_updated",
      "product_id": "prod_123",
      "item_ids": ["task_9"]
    }
  }
}
```

Why invalidation-first is the right tradeoff:

- the UI already has snapshot reads and knows how to render them,
- the engine does not need to invent a patch grammar immediately,
- correctness is easier because reads come from the canonical DB,
- fine-grained deltas can be added later without blocking immediate sync.

### 6. Add a Monotonic Work Revision

Introduce a server-visible `work_revision` that increments on every successful
work mutation.

This revision should be advanced in the same logical mutation path that commits
the database change, and it should be included in:

- `subscribed` responses,
- `topic_event` notifications,
- relevant read responses such as `products_list` and `work_tree`.

The revision gives clients a clean way to reason about ordering and races:

- "my snapshot is revision 41,"
- "I received a notification at revision 42,"
- "I need to refetch."

The first phase does not need a durable event log. A monotonic revision is
enough.

### 7. Mutation Flow

For each work mutation, the engine should:

1. validate and write through `WorkDb`,
2. compute the affected topics,
3. bump `work_revision`,
4. send the direct response to the requester,
5. publish invalidation notifications to subscribed sessions.

Example mappings:

- `create_product`
  Publish `work.products` and `work.product.<new_id>`.
- `update_product`
  Publish `work.products` and `work.product.<id>`.
- `create_project`
  Publish `work.product.<product_id>`.
- `create_task`
  Publish `work.product.<product_id>`.
- `create_chore`
  Publish `work.product.<product_id>`.
- `update_work_item`
  Publish `work.product.<product_id>`, and also `work.products` if the item is a
  product.
- `delete_work_item`
  Publish `work.product.<product_id>`.
- `reorder_project_tasks`
  Publish `work.product.<product_id>`.

The engine should publish after the write is committed, never before.

### 8. UI Behavior

The macOS app should keep one long-lived engine connection as it does today, but
it should subscribe when that connection becomes ready.

Recommended behavior:

### On Connect

1. subscribe to `work.products`,
2. if a product is selected, subscribe to `work.product.<product_id>`,
3. issue `list_products`,
4. issue `get_work_tree(product_id)` for the selected product.

### On Product Selection Change

1. unsubscribe from the old `work.product.<old_id>` topic,
2. subscribe to `work.product.<new_id>`,
3. fetch `get_work_tree(new_id)`.

### On Notification

- `work.products` invalidated:
  Call `sendListProducts()`.
- selected `work.product.<id>` invalidated:
  Call `sendGetWorkTree(productId: id)`.

This is intentionally simple. The current app already knows how to rebuild its
state from those reads.

### 9. CLI Behavior

Normal CLI commands do not need subscriptions.

Their behavior stays:

- send a request,
- receive the direct response,
- print output,
- exit.

The benefit comes from the engine publishing the same mutation to other sessions
that remain connected, such as the macOS app.

This design also enables future commands like:

```bash
boss watch work --product boss
boss task list --watch
```

But watch mode should be a follow-up, not part of the initial implementation.

### 10. Backpressure and Coalescing

A topic broker needs explicit policy for slow or disconnected clients.

Recommended first-phase rules:

- each session gets a bounded outbound queue,
- identical pending work invalidations for the same topic may be coalesced,
- if a client cannot keep up, disconnect it,
- reconnecting clients resubscribe and refetch snapshots.

This is acceptable because work invalidations are cheap to replay by refetching
the authoritative state.

### 11. Why Not Watch SQLite Directly?

Because it solves the wrong problem.

Direct DB watching would:

- bypass engine-level validation and semantics,
- give poor or no detail about which logical entity changed,
- be awkward across multiple processes and future transports,
- not help with agent/terminal live topics later.

The engine is already the write path. It should own notification semantics too.

## Migration Plan

#### Phase 1: Protocol Foundations

- add request envelopes with `request_id`,
- add server-managed session IDs,
- add `subscribe` / `unsubscribe`,
- add a shared `TopicBroker`.

#### Phase 2: Work Invalidation Topics

- add `work.products` and `work.product.<id>`,
- publish invalidations from all work mutations,
- include `work_revision` in subscribe responses and work reads.

#### Phase 3: UI Adoption

- have the macOS app subscribe on connect,
- refetch `list_products` and `get_work_tree` on relevant topic events,
- optionally ignore self-originated invalidations or debounce them.

#### Phase 4: Follow-Ups

- add `boss watch`,
- add agent and terminal topic subscriptions,
- add finer-grained deltas if profiling shows full refetches are too expensive.

## Key Tradeoff

The important design decision is:

- **subscriptions plus invalidation now**
- **fine-grained delta streaming later if needed**

That gives immediate CLI-to-UI propagation with a small, robust change surface.
It also preserves the engine as the only source of truth and sets up a clean
general-purpose pub/sub foundation for the rest of Boss.

## Related Designs

- [`main`](main.md)
- [`work-taxonomy`](work-taxonomy.md)
- [`work-kanban`](work-kanban.md)
- [`work-cli`](work-cli.md)
