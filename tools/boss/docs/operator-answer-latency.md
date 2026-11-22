# Answer latency: from the click to the card clearing

Answering or dismissing a question card should clear the card at once. When it
does not, the time is in one of the hops below. Each hop logs one line, and the
app's `request=` is the engine's `request_id`, so one answer can be followed
end to end.

## The hops

### App

Unified log, subsystem `dev.spinyfin.bossmacapp`, category `operator-answer`:

```sh
log stream --predicate 'subsystem == "dev.spinyfin.bossmacapp" AND category == "operator-answer"'
```

| Line                                                     | Meaning                                                                                                                            |
| -------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| `answer click sent` / `dismiss click sent`               | The request was written to the engine socket.                                                                                      |
| `answer reply applied` (`since_click`)                   | The engine's `work_item_updated` reply was applied to the model. The card moves on this reply alone; no board refetch is involved. |
| `question card cleared` (`since_click`, `since_reply`)   | The question block left the card: the board repainted.                                                                             |
| `question card not cleared 10.0 seconds after the reply` | The card was off screen, or the board did not repaint.                                                                             |
| `answer refused` (`since_click`)                         | The engine refused the answer; the question stays on the card.                                                                     |

### Engine

Engine log, message prefix `operator answer`:

| Line                                            | `elapsed_ms` measures                                                                                                                                                                                                                                         |
| ----------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `operator answer handler entered`               | Socket receipt to handler start. A session runs one mutation at a time, so this is time spent behind the session's earlier requests.                                                                                                                          |
| `operator answer durable write completed`       | The answer's transaction, including any wait for the database connection.                                                                                                                                                                                     |
| `operator answer board publication completed`   | Execution reconcile plus fan-out of the invalidation push.                                                                                                                                                                                                    |
| `operator answer: launched the work it created` | The launch of the run the answer created or resumed. Only logged for an answer that launches (a run with force-launch lineage, or an architectural-mismatch answer). The reply does not wait for cube or spawn; this line is logged when the launch finishes. |
| `operator answer reply enqueued`                | Socket receipt to the reply entering the session's outbound queue: the engine's total.                                                                                                                                                                        |
| `operator answer reply socket write completed`  | Socket receipt to the reply frame leaving the socket. The gap from `reply enqueued` is outbound queue time.                                                                                                                                                   |
| `operator answer refused`                       | The transaction that refused the answer.                                                                                                                                                                                                                      |

## Reading a slow answer

- **Large `handler entered`**: an earlier request on the same session was still
  running. Look for the request that finished just before it.
- **Large `durable write completed`**: the write waited for the shared database
  connection. A wait or hold of 100 ms or more also logs
  `work DB connection lock timing` with the same `request_id`; its `wait_ms`
  is the time queued, and the holder logs its own line with `held_ms`, its
  `verb` or `background` name, and the `caller` that took the connection.
- **Large `launched the work it created`**: workspace lease and worker spawn
  were slow. The card has already moved; this line is the launch finishing.
- **Large gap between the engine's `reply socket write completed` and the app's
  `answer reply applied`**: the app's main thread was busy.
- **Large `since_reply` on `question card cleared`**: the model had the answer
  and the board was slow to repaint.

## What an answer does not wait for

Reads that feed the board do not take the connection mutations use:

- The app's board read (`get_work_tree`) runs on its own read-only connection
  and in the session's read lane, so it neither holds the shared connection nor
  blocks the session's mutations.
- The scheduler heartbeat's queued-display pass, which recomputes the label of
  every queued card in one read transaction, runs on its own read-only
  connection (`ExecutionCoordinator::queued_display_db`) because it holds that
  connection for a read transaction over every queued card, and on the shared
  connection that time would be added to every mutation that arrives mid-pass.
- Launching the run an answer created or resumed (workspace lease, pane spawn)
  finishes off the session handler, so the card clears while that launch is
  still in flight.

A new read that walks many cards belongs on a read-only connection
(`WorkDb::reader`) for the same reason: on the shared connection its duration
is added to every mutation that arrives while it runs.
