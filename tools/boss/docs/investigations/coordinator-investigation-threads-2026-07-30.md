# Making coordinator investigation threads legible and conversational

- **Date:** 2026-07-30
- **Kind:** design-space exploration. No code changed; nothing implemented. Deliverable is this writeup.
- **Question:** Too many concurrent threads of conversation with the coordinator. Investigation work that was meant to be filed as tasks has migrated into coordinator background agents, where its results live only in a chat scroll. Map the space, assess the operator's ideas, and produce a decision rule.
- **Primary evidence:** one coordinator session (2026-07-30), inlined into the brief because no worker can read coordinator conversation state. Eight background agents, ~10 pending operator decisions, zero attentions raised.
- **Method:** source reading at current HEAD (`c20fb6bd`) plus the design-doc corpus. Every behavioural claim below carries a `file:line`. Claims I could not verify are marked and repeated under [Open questions](#open-questions).
- **Related:** [`attentions`](../designs/attentions.md), [`notification-dedup-scoring`](../designs/notification-dedup-scoring.md), [`comment-triggered-document-revisions`](../designs/comment-triggered-document-revisions.md), [`comments-in-markdown-viewer`](../designs/comments-in-markdown-viewer.md), [`transcript-viewer`](../designs/transcript-viewer.md), [`worker-proposal-api…`](../designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md), [`retire-the-coordinator-s-memory…`](../designs/retire-the-coordinator-s-memory-make-the-defaults-teach-the-right-thing.md)

---

## Verdict (read this first)

**The operator's fifth point is the whole problem, and it has a precise mechanical cause.** Interactivity requires a _warm participant_ — one whose context survives between turns. Boss has exactly one warm participant today (the coordinator session) and no way to make a second one. Every other surface re-engages a worker by **spawning a brand-new process from a cold prompt**. There is no session resume anywhere in the engine or the app: I searched for `--resume`, `--continue`, `--fork-session`, and `resume_session` across `tools/boss/engine/` and `tools/boss/app-macos/Sources/` and found zero hits outside markdown. Work migrated to coordinator background agents because the coordinator is the only place a follow-up question costs nothing.

**The comments feature is not "slow and clunky" in a fixable-by-polish sense. It is structurally the wrong shape.** A single operator reply to an answered comment triggers: an out-of-band LLM classification call, then a _new_ `answer_agent` execution, then the full dispatch pipeline — `cube repo ensure` (≤60s budget), `cube workspace lease` plus fallback (≤30s each), `cube change create` ([`coordinator/scheduler.rs:1845-1847`](../../engine/core/src/coordinator/scheduler.rs)) — then a pane spawn with a 60s ack grace ([`spawn_ack_sweep.rs:80`](../../engine/core/src/spawn_ack_sweep.rs)), then a fresh Claude process that **re-fetches the whole document from GitHub over `gh api`** and reads the prior thread back as flattened prose ([`runner/prompt.rs:1475`](../../engine/core/src/runner/prompt.rs)). The floor is tens of seconds of dispatch before a token of thinking, and essentially none of it is UI friction. See [§2](#2-the-comment-round-trip-traced).

**But there is a warm path already in the codebase, and it is the most important finding here.** The engine can write into a _live_ worker pane as if the operator typed it — `SendInputToWorker` → `SendToPane` → `submitText` ([`wire.rs:1818-1821`](../../protocol/src/wire.rs), [`GhosttyTerminalView.swift:898`](../../app-macos/Sources/Ghostty/GhosttyTerminalView.swift)). The transient-recovery sweep already relies on this being cheap: it nudges a live idle worker "rather than tearing it down… a nudge is cheaper than orphan+respawn" ([`dispatch-events/src/lib.rs:222-226`](../../engine/dispatch-events/src/lib.rs)). A worker whose process is still alive at its REPL **is** a warm participant. Nothing keeps one alive for conversation, but nothing structural prevents it either. This changes the design question from "can we have warm threads at all" to "are we willing to hold a slot open for one".

**On the four ideas:** the kanban filter (idea 1) is a real but small fix aimed at the wrong problem — the rows are _absent_, not merely hard to see. The privileged agent scope (idea 3) is the highest-leverage of the four and is closer to shipping than the operator thinks; the read/write line is already drawn and enforced, and the gap is a narrow, nameable set of filesystem paths. Threading (idea 4) is real, but only two or three of Slack's properties are load-bearing and Boss can already supply most of them. The decision rule (idea 5) is deliverable now, as prompt text.

**On the two additions:** the pinned decision register is **not** the same thing as attentions, but only because of _who writes it_ — and that difference is the entire reason it might not rot. It is the cheapest high-value item in this document. The coordinator-pane rewrite is **not justified by anything in this brief**: I could not find a single requirement here that the TUI embedding forecloses. See [§7](#7-replacing-the-coordinator-pane).

---

## What I verified vs. what I inferred

**Verified by reading source at HEAD:** the comment reply code path and every stage in it; the absence of session resume; the worker permission deny rules and their per-driver expression; the kanban's filter inventory; attention lifecycle states and the absence of any staleness sweep; the `boss` / `bossctl` CLI split; the coordinator pane's launch and I/O mechanism; the existence and shape of `boss propose`, `boss decision`, and `boss comment`.

**Inferred, and flagged as such in place:** wall-clock numbers. I did not run a live comment reply and time it — that would have required mutating engine state, which is out of a worker's remit. My latency claims are built from the engine's _own_ documented timeout budgets and stall thresholds, which are a lower bound on what the engine considers normal, not a measurement of the median. I state them as envelopes, not measurements.

**Could not determine:** the current attention inventory (the store is in the engine runtime DB under `~/Library/Application Support/Boss/`, which workers are explicitly denied — see [§4](#4-idea-3-a-boss-developing-boss-agent-scope)). This is itself evidence for idea 3: the brief asked me to verify the debris claim against the live inventory and the sandbox is precisely what stopped me. I verified the _mechanism_ that would produce debris instead, which is the more durable finding.

---

## 1. The mechanism: warm context is the scarce resource

Start here, because it reorders everything else.

A conversation is cheap when the other party remembers. The coordinator remembers: its session is one long-lived Claude Code process ([`BossPaneModel.swift:8-10`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)), restarted only when the child exits ([`BossPaneModel.swift:97-104`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)). Asking it a follow-up costs one turn.

Every other agent in Boss is one-shot. A worker is spawned with a composed prompt, does its work, and its process ends. There is no mechanism to re-enter it with its context intact:

- **No resume flag anywhere.** No `--resume`, `--continue`, `--fork-session`, or `resume_session` in the engine or app source.
- **"Resume" in Boss means re-dispatch, not re-entry.** The transient-recovery sweep "auto-resumed it on the same workspace" ([`dispatch-events/src/lib.rs:209-214`](../../engine/dispatch-events/src/lib.rs)) — same _workspace_, new process. `PaneDeathReconcile` preserves "workspace/lease… for resume redispatch" ([`dispatch-events/src/lib.rs:190-193`](../../engine/dispatch-events/src/lib.rs)) — again, the disk state, not the conversation.
- **Transcripts are read-only forensics.** The transcript viewer shipped in 2026-05 ([`transcript-viewer.md:3`](../designs/transcript-viewer.md)) and renders executions after the fact ([`TranscriptViewerView.swift:5`](../../app-macos/Sources/TranscriptViewerView.swift)). Nothing feeds a transcript back into a process as live context.

**The one exception, and it matters.** While a worker's process is still alive, the engine can talk to it warmly. `SendInputToWorker` resolves a run to a pane slot and forwards a `SendToPane` request "which the app routes through the same libghostty surface a real keystroke takes" ([`wire.rs:1818-1821`](../../protocol/src/wire.rs)). The app pastes the body and synthesises a Return ([`GhosttyTerminalView.swift:898-909`](../../app-macos/Sources/Ghostty/GhosttyTerminalView.swift)) — the explicit Return is required because bracketed paste alone leaves the text sitting in the input buffer. `Esc` is available the same way ([`GhosttyTerminalView.swift:963`](../../app-macos/Sources/Ghostty/GhosttyTerminalView.swift)). This is how `bossctl agents send`, `bossctl probe`, and the intervene affordance work.

And the engine already treats this as the cheap option. `TransientRecoveryNudge`: _"sent a runtime nudge to a live idle worker rather than tearing it down. The worker's `claude` process is still alive at its REPL and can receive input; a nudge is cheaper than orphan+respawn"_ ([`dispatch-events/src/lib.rs:222-226`](../../engine/dispatch-events/src/lib.rs)).

**So: warm re-engagement of a non-coordinator agent is already implemented, already used, and already understood to be cheap.** What does not exist is any reason for a worker to stay alive after its deliverable lands. The slot is released, the pane is freed, the lease returns. Holding one open for conversation is a _policy_ decision with a real cost (a slot out of 16 interactive), not a missing capability.

That reframes the design question. It is not "can a thread hold a resumable agent" — it is **"is a conversation worth an occupied slot, and for how long?"** That question has an answer, and it is not obviously no.

### The interactivity budget

I use this throughout as the test the brief demands. One operator reply → one agent response:

| Path                                     | Context source                                                             | Wall-clock floor                                | Verdict                                                                 |
| ---------------------------------------- | -------------------------------------------------------------------------- | ----------------------------------------------- | ----------------------------------------------------------------------- |
| Coordinator chat                         | Warm, in-process                                                           | Seconds                                         | **Meets it.** This is why work migrated here.                           |
| Live worker pane via `SendInputToWorker` | Warm, in-process                                                           | Seconds                                         | **Meets it** — if a pane is still alive. Nothing keeps one alive today. |
| Comment follow-up (bucket 2)             | Cold: fresh process, doc re-fetched from GitHub, thread flattened to prose | Tens of seconds of dispatch before any thinking | **Fails it.**                                                           |
| Filed row / revision task                | Cold: full dispatch, plus a PR round trip                                  | Minutes                                         | **Fails it**, and is not trying to meet it.                             |

---

## 2. The comment round trip, traced

The brief asked me to establish _why_ comments are too slow rather than accept the characterisation. Here is the path a single operator reply takes.

**Step 1 — the reply lands.** `CommentsPostFollowup` ([`wire.rs:358-377`](../../protocol/src/wire.rs)) → `handle_comments_post_followup` ([`app/comments.rs:609`](../../engine/core/src/app/comments.rs)). It transitions the comment `answered → awaiting_followup`, appends an `operator_followup` thread entry, publishes a `comment_topic` invalidation, and returns. This part is fast and fine.

**Step 2 — an LLM classifies the reply.** Off the critical path, `spawn_followup_classifier` ([`app/comments.rs:700`](../../engine/core/src/app/comments.rs)) calls the utility model to decide whether the follow-up is another `question` or a `revision` request. If there is no utility-model credential, the comment simply stays `awaiting_followup` with **no retry** ([`app/comments.rs:714`](../../engine/core/src/app/comments.rs)) — a silent dead end.

**Step 3 — a whole new worker is spawned.** On `question`, `respawn_answer_agent_for_followup` ([`app/comments.rs:328`](../../engine/core/src/app/comments.rs)) resolves the repo, flips `awaiting_followup → answering`, and creates a fresh `answer_agent` execution. This is a full dispatch, identical in shape to the first one — `spawn_answer_agent` ([`app/comments.rs:251`](../../engine/core/src/app/comments.rs)) and the respawn differ only in source status and `thread_turn`.

**Step 4 — the dispatch pipeline runs.** The engine's own documented budgets: `cube repo ensure` ≤60s, workspace lease and its fallback ≤30s each, then `cube change create` ([`coordinator/scheduler.rs:1845-1847`](../../engine/core/src/coordinator/scheduler.rs)). The dispatch-event stages exist because these steps have visibly hung in production — the `CubeWorkspaceLeaseAttempted` stage was added after an incident where the engine "sat silent for ~46 seconds with no event between `worker_claimed` and the next stage" ([`dispatch-events/src/lib.rs:110-118`](../../engine/dispatch-events/src/lib.rs)), and `cube repo ensure` "on a cold/large repo can run for tens of seconds" ([`dispatch-events/src/lib.rs:86-89`](../../engine/dispatch-events/src/lib.rs)). Pane spawn then gets a 60s ack grace ([`spawn_ack_sweep.rs:80`](../../engine/core/src/spawn_ack_sweep.rs)).

**Step 5 — the agent rebuilds context from cold.** `compose_answer_agent_prompt` ([`runner/prompt.rs:1475`](../../engine/core/src/runner/prompt.rs)) re-fetches the _entire document_ from GitHub via `gh api` on every turn, embeds it in the prompt, and appends the prior thread as flattened `**kind** (author):\n body` prose ([`runner/prompt.rs:1565-1573`](../../engine/core/src/runner/prompt.rs)). The doc is fetched rather than read from the leased checkout because the checkout is at whatever ref cube gave it, not necessarily the doc's branch ([`runner/prompt.rs:1459-1466`](../../engine/core/src/runner/prompt.rs)). The agent then reads its read-only `CLAUDE.md` ([`answer_agent.rs:66`](../../engine/core/src/answer_agent.rs)) and starts investigating.

**Step 6 — one reply, then it dies.** `boss comment reply` is the single allowlisted mutation ([`answer_agent.rs:53`](../../engine/core/src/answer_agent.rs)); a second call fails because the tracking run row is no longer `running` ([`comment_commands.rs:29-36`](../../cli/src/comment_commands.rs)).

### What this means

**Almost none of the cost is UI.** The dispatch and cold-context reconstruction dominate by orders of magnitude. Polishing the sidebar would not move it.

**Three further defects fall out of the trace, each worth filing on its own:**

1. **You cannot reply while it is thinking.** A follow-up arriving while a prior run is still `answering` is rejected with a `WorkError`; the code comment concedes the design describes queuing this case and it is "not yet implemented — the operator sees a WorkError and can retry once the in-flight run completes" ([`app/comments.rs:638-644`](../../engine/core/src/app/comments.rs)). In a conversation, the ability to add "…oh, and also" mid-thought is table stakes.

2. **Work-item comments get no answer agent at all.** Bucket 2 is gated on `resolve_doc_owner`, which returns `None` for `artifact_kind = 'work_item'` — asserted directly in test ([`work/tests/t11.rs:51-59`](../../engine/core/src/work/tests/t11.rs)). So a question on a _chore's description_ — the most natural place to ask one — has no conversational path. Only comments on design/investigation doc PRs do.

3. **A dropped classification is silent and terminal.** No credential, transport failure, or malformed reply leaves the comment parked in `awaiting_followup` forever with no retry ([`app/comments.rs:697-714`](../../engine/core/src/app/comments.rs)). There is a sibling sweep for comments stranded in `answering` ([`stranded_answering_sweep.rs:1`](../../engine/core/src/stranded_answering_sweep.rs)) but no equivalent for `awaiting_followup`.

**Conclusion: comments cannot be polished into the needed surface.** The state machine is right — classify, answer, follow up, reclassify is exactly the loop the operator wants. The _execution model underneath it_ is wrong: it re-instantiates the participant on every turn. Comments become viable as a conversational surface only if a turn can be served by a warm participant. That is the same fix as everything else in this document.

---

## 3. Taxonomy of the work that is escaping

Six classes, drawn from the session evidence. Per class: what forces it out, and whether it _should_ be a row.

### Class A — needs coordinator-private state

_Examples from the session:_ the `MAX_CANON` spawn diagnosis (required reading the spawn diagnostics and engine logs); the lossy dispatch-event log finding; the idle-session-reaping policy question.

_What forces it out:_ the executor gate, correctly. A worker cannot read `~/Library/Application Support/Boss/**` — denied by explicit glob ([`worker_setup.rs:777-795`](../../engine/core/src/worker_setup.rs)) — nor invoke any `bossctl` verb ([`worker_setup.rs:805-806`](../../engine/core/src/worker_setup.rs)).

**Should be a row: yes, and this is the single biggest win available.** Today the coordinator's only compliant option is to inline the evidence verbatim into a brief before filing, which its contract explicitly instructs ([`BossPaneModel.swift:510`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)) — and which is exactly the transcription tax that makes filing feel heavyweight. Idea 3 dissolves this class. See [§4](#4-idea-3-a-boss-developing-boss-agent-scope).

### Class B — single lookup, yes/no

_Examples:_ "is `lib.rs` in the changeset?"; "does this verb exist?"

_What forces it out:_ nothing forces it — filing would be absurd. The contract already carves this out: inline is fine for "a single trivial lookup (one CLI call or one file peek)" ([`BossPaneModel.swift:510`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)).

**Should be a row: no.** Correctly handled today. The operator's reason 2 ("the file-a-task-generate-a-pr workflow seems very heavyweight") is _right_ for this class and it is already exempt.

### Class C — interactive / iterative

_Examples:_ the checkleft `format/rust` thread, which spawned four sub-threads, disproved two operator hypotheses and corrected two coordinator claims. The question changed three times.

_What forces it out:_ nothing in the taxonomy — the **absence of a warm second participant**. A filed row is a one-shot brief; the value here came entirely from the back-and-forth. Filing this as a row and getting a doc PR back would have answered the _first_ version of the question, which turned out to be the wrong one.

**Should be a row: no, not as it stands — and this is the honest answer, not a dodge.** But the _conclusion_ should durably land somewhere (see the decision rule, [§8](#8-decision-rule-background-agent-vs-filed-row)). This class is why the operator's framing "more questions than answers" is correct: the fix is not a better row, it is a warm participant that is not the coordinator.

### Class D — needs to interrupt synchronously

_Examples:_ "this PR is ready to merge, do you want it?"; the ~10 pending decisions.

_What forces it out:_ the operator's reason 3 — attentions exist for exactly this and go unread. The mechanism behind that is real and I verified it: see [§6](#6-the-pinned-decision-register).

**Should be a row: no — it is not work, it is a decision.** Filing a task to represent "the operator needs to decide X" is a category error; that is what attentions are. The defect is in the attention _lifecycle_, not in the concept.

### Class E — genuinely wants a durable repo artifact

_Examples:_ this document. The codex TUI pivot pricing analysis. Anything a future worker or coordinator session will need to read.

_What forces it out:_ nothing — it works, and there are 30+ files in `tools/boss/docs/investigations/` proving it.

**Should be a row: yes, unambiguously.** The operator's own caveat is correct: "there are upsides to having persistent .md files in the repo recording the results of investigations." I'd sharpen it — the repo is the _only_ durable store a future agent can read. Coordinator memory cannot be read by workers by design, and its contract explicitly restricts it to operator facts, directing everything about Boss's behaviour to "a work item (or… a repo doc workers can read)" ([`BossPaneModel.swift:464-466`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)).

### Class F — bounded background research to produce a brief

_The operator's own example_, which they consider legitimately an agent job. Scoping the appoint Buildkite work; sweeping the code to size a problem before filing.

_What forces it out:_ nothing — it is _pre-work for_ filing, not a substitute for it. Its output is consumed immediately by the row it produces.

**Should be a row: no.** The contract already mandates this shape ([`BossPaneModel.swift:510`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)) and it is working as intended. The output's durability is genuinely handled: it lands _inside the filed row's description_, which is durable engine state.

### Testing the operator's "am I over-indexing on heavyweight-ness?"

**Partly yes.** Classes B and F feel heavyweight to file because they _should not be filed_ and already aren't. Class E is not heavyweight at all — a doc PR is the appropriate ceremony for a durable artifact, and the corpus shows it working.

**Partly no, and this is the real finding.** Class A feels heavyweight for a legitimate reason that is not ceremony: the coordinator must **hand-transcribe coordinator-private evidence into the brief** before a row is executable. That is a genuine tax, it scales with the evidence, and it is the tax idea 3 removes. The operator's instinct that something is too heavy is correct; they have mislocated it as PR ceremony when it is actually evidence transcription.

**Of the eight escaped investigations in the session, my read is: roughly the class-A ones should have been rows and could not be; the class-C one should not have been a row at all.** I cannot classify all eight precisely — I only have the brief's summary of them, not their content. Flagged as an open question.

---

## 4. Idea 3: a boss-developing-boss agent scope

Taken second because it is the highest-leverage idea and because [§5](#5-idea-1-a-kanban-filter-for-investigations)'s verdict depends on it.

### What read-only Boss access a worker has today

The operator's belief is correct but the boundary is sharper than they expect. It is drawn in exactly one place — `deny_rules` ([`worker_setup.rs:768`](../../engine/core/src/worker_setup.rs)) — and it has three parts:

**1. The entire engine data directory is denied, read included.** Derived from the events socket's parent, both the bare directory and its subtree, for both `Read` and `Edit` ([`worker_setup.rs:777-795`](../../engine/core/src/worker_setup.rs)). The comment is explicit that even an `ls` should fail. `Write(path)` is deliberately absent because it is inert in Claude Code's permission engine — `Edit(path)` covers both tools.

**2. Every `bossctl` shape is denied** — bare, `:*` glob, and absolute-path form, the last specifically to stop a `$HOME/bin` bypass ([`worker_setup.rs:797-806`](../../engine/core/src/worker_setup.rs)). Rationale in the source: _"Workers don't drive the coordinator, they answer to it."_

**3. `boss engine start` / `stop` are denied**; the rest of the `boss` surface is allowed because "the rest of the `boss` surface (list/show/etc.) talks to the engine over its IPC socket which is fine" ([`worker_setup.rs:809-816`](../../engine/core/src/worker_setup.rs)).

So **"read-only boss access" means the `boss` CLI's read verbs over IPC** — `product`, `project`, `task`, `chore`, `comment`, `decision`, `automation`, `attention`, `context`, `pr`, `github`, `editorial` ([`cli/src/main.rs:243-299`](../../cli/src/main.rs)) — plus the curated `boss context` bundle, which is one round trip returning the worker's own task, project, product, sibling tasks with dependency edges, open attention groups, and its own proposals ([`cli/src/context.rs:1-13`](../../cli/src/context.rs)).

**Per-driver expression differs, as the brief anticipated.** Claude ships deny rules in a generated `settings.json` ([`worker_setup.rs:655`](../../engine/core/src/worker_setup.rs)); Codex writes deny-only PreToolUse guardrails into `CODEX_HOME` ([`driver/src/codex.rs:1102`](../../engine/driver/src/codex.rs)); Grok's `--sandbox`/`--allow`/`--deny` argv path is explicitly noted as **not yet wired** ([`driver/src/grok.rs:13`, `:120`](../../engine/driver/src/grok.rs)). A privileged scope must be expressible in all three or be restricted to drivers that can enforce it — otherwise "privileged" silently means "unenforced" on Grok.

### The gaps, named

For each thing the coordinator did in the session that a worker could not:

| Coordinator action                                                            | Blocked by                                                                             | Read access that would close it                                                                                                                            |
| ----------------------------------------------------------------------------- | -------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Read engine runtime state (execution rows, live worker registry, lease state) | Data-dir `Read` deny                                                                   | `state.db` read, or better, `bossctl`-equivalent read verbs over IPC                                                                                       |
| Read `dispatch-events/*.jsonl` to diagnose a stall                            | Data-dir `Read` deny                                                                   | The dispatch-events store; `Stage` timelines are already structured and typed ([`dispatch-events/src/lib.rs:54`](../../engine/dispatch-events/src/lib.rs)) |
| Read `engine-trace.jsonl`                                                     | Data-dir `Read` deny                                                                   | The trace store                                                                                                                                            |
| Read another run's transcript                                                 | Data-dir `Read` deny                                                                   | The transcript store — already has a viewer and a `bossctl agents transcript` verb, both coordinator-side                                                  |
| `bossctl agents list / status` to see the fleet                               | `bossctl` deny (all shapes)                                                            | A read-only subset of `bossctl`                                                                                                                            |
| `bossctl dispatch diagnose` / `tail`                                          | `bossctl` deny                                                                         | Same                                                                                                                                                       |
| Read the diagnostics directory                                                | Data-dir `Read` deny                                                                   | Same store                                                                                                                                                 |
| Read the coordinator conversation itself                                      | Not stored anywhere the engine can reach — see [§7](#7-replacing-the-coordinator-pane) | **Nothing closes this.** It is not a permission gap; the data does not exist in any addressable form.                                                      |

Note the shape: **seven of eight gaps are one deny glob**, and the eighth is not a permission problem at all. This is a much smaller change than "coordinator-level access" implies.

### The read/write line, and why it holds

The line is already drawn and already enforced, so a privileged _read_ scope does not need to invent it:

- **Reads over IPC are already allowed** and the engine mediates them. Extending reads means extending what the engine will _answer_, not handing out a file handle. Prefer new read verbs over relaxing the filesystem deny — a verb is auditable, versioned, and cannot be used to read a credential that happens to sit in the same directory.
- **Taxonomy writes stay closed.** The executor gate ([`BossPaneModel.swift:446`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)) exists because a planner once materialised six rows whose entire deliverable was an action on the coordinator's private memory store. Nothing here touches that.
- **The hand-back already exists, and this is the load-bearing point.** When such an agent concludes "and therefore rescope these three rows", it does **not** write taxonomy — it files a proposal. `boss propose` is the mediated worker→engine submission verb ([`cli/src/propose.rs:1-14`](../../cli/src/propose.rs)) with kinds `attention`, `effort_escalation`, `blocked`, `deferred_scope`, `followup_task`, `automation_outcome`, `pr_created` ([`protocol/src/types/proposal.rs:27-35`](../../protocol/src/types/proposal.rs)). Validation, rate caps and attribution are engine-side; a malformed submission returns `ProposalRejected` the worker can act on. Idempotency is automatic and derived from the execution id ([`cli/src/propose.rs:37-42`](../../cli/src/propose.rs)).

**So the correct posture for a privileged read scope is: more reads, zero new writes, and every conclusion that implies a taxonomy change goes out as a proposal for the coordinator to accept or reject.** That is the same posture the answer agent already runs under — deny-by-default `dontAsk` with a single allowlisted mutation ([`answer_agent.rs:14-32`](../../engine/core/src/answer_agent.rs)) — which is a working precedent, not a hypothetical.

### Blast radius

**What stops it becoming the default?** Nothing automatic — it needs an explicit gate, and I'd argue for the narrowest one that works:

- **Per-product is the natural axis and matches the operator's framing** ("purely for boss-developing-boss"). Only work against the product that owns `spinyfin/mono` is eligible. A worker on `appoint` or `flunge` has no legitimate reason to read Boss's runtime DB, and scoping this way means the blast radius cannot grow by accident as new products are added.
- **Per-kind is insufficient alone.** An `investigation` row against `appoint` should not get engine internals.
- **Opt-in per row, on top of per-product**, so the coordinator states the need at filing time and the privilege is visible on the row. This also gives an audit trail: a row that requested privilege and did not need it is a filing error someone can see.

**The real risk is not exfiltration, it is context poisoning.** A worker that can read the engine DB can read _other work items' state_ and start reasoning about work that is not its own — the failure mode the executor gate exists to prevent, arriving by a different door. Mitigation: prefer scoped read verbs (`boss engine dispatch-events --execution <own-id>`) over raw DB access, so the engine can enforce "your own run" the way `boss context` and `boss propose --list` already resolve caller identity from the socket peer rather than a flag ([`cli/src/context.rs:24-27`](../../cli/src/context.rs)). Where a diagnosis genuinely needs another run's data, that is an argument for a specific verb, not for the whole directory.

**Verdict: idea 3 is sound, is the highest-leverage of the four, and is smaller than it looks.** It converts class A from unfilable to filable, which removes the evidence-transcription tax the operator has been feeling as PR ceremony. It does not touch the interactivity requirement.

---

## 5. Idea 1: a kanban filter for investigations

### What exists today

Board filters, exhaustively: a product picker ([`WorkBoardToolbar.swift:389`](../../app-macos/Sources/WorkBoardToolbar.swift)), a multi-select project filter ([`WorkBoardToolbar.swift:324`](../../app-macos/Sources/WorkBoardToolbar.swift)), a chores-only toggle ([`ChatViewModel.swift:167`](../../app-macos/Sources/ChatViewModel.swift)), a blocked-only toggle ([`ChatViewModel.swift:173`](../../app-macos/Sources/ChatViewModel.swift)), and free-text search ([`ChatViewModel.swift:222`](../../app-macos/Sources/ChatViewModel.swift)). Grouping offers exactly two options — `none` and `project` ([`Models+WorkBoard.swift:47-61`](../../app-macos/Sources/Models+WorkBoard.swift)).

**There is no `kind` facet.** Investigations are visible as product-level work items and get a doc-link affordance on the card ([`WorkBoardCard.swift:143`](../../app-macos/Sources/WorkBoardCard.swift)); reasoning mode renders as a chip only when it is `investigation`, since `standard` is the overwhelming majority ([`WorkCardBadges.swift:987`](../../app-macos/Sources/WorkCardBadges.swift)).

**One genuine defect found while looking.** Product-level work items — which is what most investigations are — are appended **only when no project filter is active** ([`ChatViewModel.swift:877-884`](../../app-macos/Sources/ChatViewModel.swift)). The comment calls this legitimate ("they have no project, so a project filter legitimately excludes them"), and as _set logic_ it is. But the operator-facing consequence is that filtering to a project **hides every investigation on the board**, including one a live worker is producing right now. There is precedent that this is felt as a bug: the same block already carries a fix for investigations being wrongly hidden by the chores toggle (issue #886). This one is unfixed and worth filing.

### Assessment

**A filter is not the problem, and the evidence is unambiguous.** Of the eight investigations in that session, **none was filed**. A filter improves the legibility of rows that exist; the rows do not exist. Building it first would produce an empty, well-filtered lane.

**The rows are absent, not hidden.** So idea 1 is _downstream_ of idea 3 and of the decision rule: fix what gets filed, and the filter becomes worth building. Fix the filter first and nothing changes.

**Verdict: real but small, and mis-sequenced.** Not a dead end — the project-filter defect above is a genuine bug and a `kind` facet would be nice once the lane has contents. But it addresses none of the five things the operator described, and it does not touch interactivity at all. Do it third, or as a cheap side-effect of other work.

---

## 6. The pinned decision register

The operator: _"keeping pinned the 'decision set' that I need to make or the 'open threads' that I need to resolve."_

### The uncomfortable adjacency, addressed

This is close to what attentions were supposed to be. I want to be direct about that rather than route around it.

**What attentions actually are.** An agent-authored, human-actionable notification that always carries an action, batched into groups so answering ten questions about one doc produces one revision ([`attentions.md`](../designs/attentions.md), goals). Member states are `open` / `partially_answered` ([`work/attentions.rs:269`, `:370`](../../engine/core/src/work/attentions.rs)); group terminal states are `actioned` ([`work/attentions.rs:1244`](../../engine/core/src/work/attentions.rs)) and `dismissed` ([`work/attentions.rs:647`](../../engine/core/src/work/attentions.rs)). Shipped across seven PRs with a CLI and two app surfaces.

**Why they litter — verified mechanically.** There is **no time-based staleness handling of any kind**. I grepped `stale|expire|expiry|retention|prune|gc` across `work/attentions.rs` and `attentions_detector.rs`: every hit is about _merge-sweep idempotency_, not about age. An attention has exactly three ways out — a human answers it, a human dismisses it, or a merge folds it. **Nothing removes an attention because the world moved on.** An attention raised about a PR that has since merged, a file that has since been deleted, or a question the operator answered verbally in chat sits `open` forever.

That is the debris. The operator's diagnosis is correct, and it is a lifecycle omission, not a flaw in the concept.

**What is already planned, and whether it is the prerequisite.** [`notification-dedup-scoring`](../designs/notification-dedup-scoring.md) adds a `score` column, an `attention_merges` provenance ledger, taxonomy-aware verdicts ("already covered by an existing row"), and a **sensibility filter** for attentions that are "stale, moot, or not actionable". The store side has landed — `score` and `linked_work_item_id` are live columns ([`work/attentions.rs:37`](../../engine/core/src/work/attentions.rs)), `attention_merges` has insert and query helpers ([`work/attentions.rs:1299-1363`](../../engine/core/src/work/attentions.rs)) — behind three flags that are **all off by default**: `notification_dedup`, `notification_dedup_taxonomy`, `notification_dedup_sensibility` ([`feature-flags/src/lib.rs:155-174`](../../engine/feature-flags/src/lib.rs)).

**So: it is the prerequisite for the dedup axis, and it is roughly half-built and switched off.** But it addresses a _different axis_ from the one that matters most here. Dedup answers "three agents said the same thing"; the sensibility filter is the closest thing to staleness but is deliberately conservative and LLM-judgement-based, gated on High confidence. Neither gives an attention a _deterministic_ reason to expire. **Turning the existing flags on is cheaper than any new surface and should be tried before anything is built.** That is a concrete, near-free experiment the operator can run today.

### Why a decision register is nonetheless a different thing

One difference, and it is the whole difference: **who writes it, and therefore who is accountable for it being current.**

An attention is raised by a _worker_, about _its own run_, at the end of that run. The worker then dies. Nobody owns the row afterwards. Rot is structurally guaranteed.

A decision register would be written by the _coordinator_, which is **still alive**, still holds the conversation in context, and is the same party that will surface the item to the operator next time. That is a categorically better custodian. The coordinator can mark an item resolved the moment the operator says "merge it", because it is _in the conversation where that was said_. No worker can ever do that.

**That is the lifecycle argument, and it is the only one that matters.** A register the coordinator writes and prunes can stay current in a way an agent-authored notification cannot.

### The lifecycle, stated (because the brief forbids proposing a surface without one)

| Transition   | Who         | Trigger                                                                                                             |
| ------------ | ----------- | ------------------------------------------------------------------------------------------------------------------- |
| Opened       | Coordinator | It surfaces a decision the operator must make                                                                       |
| Resolved     | Coordinator | The operator decides, in conversation. Same turn.                                                                   |
| Superseded   | Coordinator | A later decision moots it — pointer to the successor                                                                |
| Auto-expired | Engine      | The referenced artifact reached a terminal state (PR merged/closed, row `done`/`cancelled`). Deterministic, no LLM. |
| Dismissed    | Operator    | Direct gesture in the UI                                                                                            |
| Deduped      | Engine      | The dedup machinery above, once its flags are on                                                                    |

**Auto-expiry is the piece attentions lack and the piece that must ship in v1, not phase 2.** Most register items will reference a PR or a row; both already have terminal-state detection the engine runs on a poll (`mark_merged` → `mark_chore_pr_merged`, and the `TaskStatus::is_terminal()` set). Wiring "referenced artifact went terminal → expire the item" is deterministic, cheap, and is exactly the transition whose absence rotted attentions.

**What the operator sees if it goes stale.** The register must show a per-item last-touched time and visibly age. An item untouched for N days should render as _suspect_, not silently equal to a fresh one. Attentions gave every row identical visual weight forever, which is why a stale one was indistinguishable from a live one and the whole surface became noise.

### The executor-gate split

**Filable as ordinary rows:** the schema, the engine store, the RPCs, auto-expiry, and the app surface. All engine-plus-app work whose input and output live in the repo. Standard project shape.

**Not filable — coordinator behaviour:** _populating_ it. A register that exists and is never written to is worse than none, because it looks authoritative while being empty. What obliges the coordinator is the same thing that obliges it to pass `--effort` and `--reasoning` on every create: a rule in `bossSystemPrompt` ([`BossPaneModel.swift:408`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)). That is a chore against the Swift source, **not** an edit to the runtime `CLAUDE.md`, which the app rewrites on every launch ([`BossPaneModel.swift:187`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)).

**Precedent for the shape.** `boss decision` already models the _closed_ half — product-scoped standing rulings, `wontfix` / `decided` ([`protocol/src/types/decision.rs:22-28`](../../protocol/src/types/decision.rs)), with an `active` / `superseded` / `revoked` lifecycle and a `superseded_by` pointer ([`protocol/src/types/decision.rs:67-72`](../../protocol/src/types/decision.rs)). A pending-decision register is the missing _open_ half of the same idea, and could plausibly reuse that store rather than adding a third notification-shaped table. Worth a serious look before building new — the naming alone ("decision") suggests it was half-anticipated.

**Verdict: the cheapest high-value item in this document.** Recommend it before threading and before any pane work.

---

## 7. Replacing the coordinator pane

The operator is open to this and warns _"there's a /lot/ of complexity in doing that well."_ They are right, and the honest answer is stronger than "it's expensive."

### What the embedding actually is

The coordinator is `claude --model <slug> --permission-mode auto` ([`BossPaneModel.swift:8-10`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)) launched via `exec` into a libghostty surface in a dedicated working directory under Application Support ([`BossPaneModel.swift:12-18`, `:56-88`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)). Its system prompt is written to that directory's `CLAUDE.md` on every start ([`BossPaneModel.swift:187`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)). `exec` is deliberate so there is no shell to fall back into, and a single Ctrl-C reaches Claude as interrupt-current-turn rather than killing a shell ([`BossPaneModel.swift:78-82`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)). On child exit the surface restarts after 1.5s ([`BossPaneModel.swift:97-104`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)). It is hosted in a resizable side panel ([`ContentView.swift:817`](../../app-macos/Sources/ContentView.swift)).

**Input in:** the app can paste text and synthesise Return or Esc ([`GhosttyTerminalView.swift:898`, `:937`, `:963`](../../app-macos/Sources/Ghostty/GhosttyTerminalView.swift)).

**Output out:** nothing. I searched for scrollback readback, `ghostty_surface_text` in a read direction, and any surface-text extraction — the only hits are the paste path and reflow. **The app cannot read what the coordinator says.**

### What that forecloses — precisely

This is where "the terminal can't do it" needs checking rather than asserting.

**Genuinely foreclosed:** _the app cannot derive anything from the conversation._ No pinned decision set inferred from chat, no thread extraction from the scroll, no unread state on coordinator output, no "jump to where we discussed X". A one-way pipe cannot be made two-way by UI work.

**Not foreclosed — merely unimplemented:** _the coordinator can still emit structured data, because it has a CLI._ It is not limited to printing prose at a terminal; it can call `boss` and `bossctl` and write to engine state. Every requirement in this brief that involves the coordinator _recording_ something is reachable today:

- The pinned decision register: coordinator calls a `boss` verb. No pane change needed.
- Threading with durable state: same.
- Filing rows, raising attentions, linking artifacts: same, and already happens.
- Warm conversational replies from a non-coordinator agent: `SendInputToWorker` into a live pane. Not a coordinator-pane concern at all.

**The distinction that matters: the app cannot _observe_ the coordinator, but the coordinator can _report_.** Reporting is sufficient for every requirement in this brief. Observation would only be needed for something like automatic thread extraction from chat — which is not asked for, and which I would argue against anyway, since inferred structure is exactly how attentions filled with debris.

### What "doing it well" would owe

The TUI supplies all of this today at zero cost to Boss. A replacement would have to re-solve every line:

**Interaction:** token-by-token streaming; interrupt mid-turn (Esc) with correct queued-input semantics; permission prompts and their modes; keybindings and chords; the operator typing `!` to run a shell command inline; any interactive program invoked in the pane (an auth handshake, `gh auth login`, a pager, an editor).

**Rendering:** tool-use blocks; diffs; file references; syntax highlighting; markdown; long-output truncation and expansion; terminal resize and reflow (already non-trivial — reflow is called out as O(history) on a hot path, [`GhosttyTerminalView.swift:853`](../../app-macos/Sources/Ghostty/GhosttyTerminalView.swift)).

**Session:** scrollback with selection and copy/paste; session resume; transcript capture; crash recovery; the restart-on-exit loop.

**Accessibility:** VoiceOver, focus order, Dynamic Type, reduced motion. Terminal emulators get much of this from the platform; a bespoke SwiftUI surface owns all of it.

Each is small. There are twenty of them. Collectively they are the reason terminal-embedded agents are common and bespoke agent UIs are rare.

### The ongoing cost, which is the part that is usually understated

**Build cost is one-time; protocol-tracking cost is forever.** Claude Code is an upstream artifact that keeps improving. Today Boss gets every upstream improvement for free — new tool renderings, better diffs, new permission modes, new slash commands — because it hosts the real thing. A bespoke surface would have to drive Claude through a programmatic interface and re-implement the presentation of whatever that interface emits. Every upstream change to the event vocabulary becomes a Boss maintenance task, and every upstream _feature_ becomes a Boss feature request.

There is direct evidence in this repo of how expensive protocol-tracking is. The Codex driver work has produced a design doc, an execution-shape postmortem, a progress-channel decision investigation, a hook-trust investigation, and a PreToolUse guard-coverage investigation — five documents, for one non-Claude driver, mostly about _keeping up with an upstream tool's interface_. The Grok driver shows the same pattern, plus a live version-drift finding where the installed binary had already moved past the driver's pin ([`grok-notification-vocabulary-and-leader-process-2026-07-29.md`](./grok-notification-vocabulary-and-leader-process-2026-07-29.md)). A coordinator surface would sign Boss up for that same treadmill on its single most important session, permanently.

### Recommendation

**Sceptical, and the scepticism survives contact with the requirements.** I looked for a requirement in this brief that the embedding forecloses and could not find one. Every requirement is reachable through the coordinator _reporting_ via CLI rather than the app _observing_ the pane:

| Requirement                                                               | Reachable inside the TUI?                                                       |
| ------------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| Pinned decision register                                                  | **Yes** — coordinator writes via a `boss` verb                                  |
| Threaded, addressable conversations with state                            | **Yes** — same                                                                  |
| Warm interactive replies on an investigation                              | **Yes** — `SendInputToWorker` to a live pane; unrelated to the coordinator pane |
| Investigation visibility on the kanban                                    | **Yes** — engine + app work                                                     |
| Privileged read scope for boss-on-boss agents                             | **Yes** — worker permission config                                              |
| Auto-derive threads/decisions from chat without the coordinator reporting | **No**                                                                          |

Only the last row needs the rewrite, and I would argue it is undesirable regardless: inferred structure is how the existing surface filled with debris. Making the coordinator _declare_ its open threads is both cheaper and more reliable than making the app _guess_ them.

**Minimum change that delivers most of the benefit:** the decision register + a `boss` write verb the coordinator calls, plus a prompt rule obliging it to. Small, independently valuable, no pane change, ships in a wave rather than a project.

**What the incremental path forecloses later:** very little, and this is worth stating plainly because it is the usual objection. A register written through a `boss` verb is a normal engine store — a future native surface would render it, not replace it. The one thing the incremental path _does_ bake in is the assumption that **the coordinator is a reliable reporter**. If that proves false in practice — if the coordinator systematically forgets to record open threads even with a prompt rule — that is the evidence that would justify revisiting the rewrite, because at that point observation genuinely does beat reporting. **Ship the increment first specifically so that question gets answered with data rather than speculation.**

---

## 8. Idea 4: threading

### Existing thread-shaped surfaces, enumerated

Read before inventing. Everything I found:

| Surface                                                                   | Shape                                                                                                                                                                                                | Right home?                                                                                                                                                                                                                                                                  |
| ------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Comment threads** (`work_comments` + `comment_thread_entries`)          | Genuinely threaded: ordered entries, typed (`answer` / `operator_followup` / nudge), per-comment status machine, live invalidation over `comment_topic` ([`wire.rs:73`](../../protocol/src/wire.rs)) | **Closest existing fit, and already conversational in state-machine terms.** Disqualified today by its cold execution model ([§2](#2-the-comment-round-trip-traced)) and by being anchored to a _document span_ — most coordinator threads are not about a doc region.       |
| **Typed work-item comments** (`boss task comment` / `boss chore comment`) | Same store, `artifact_kind='work_item'`, anchor defaults to the item name ([`comment_commands.rs:21-28`](../../cli/src/comment_commands.rs))                                                         | **The container is right — a thread on a work item.** But it is inert: `resolve_doc_owner` returns `None` for `work_item` ([`work/tests/t11.rs:51-59`](../../engine/core/src/work/tests/t11.rs)), so no classifier routing and no answer agent. A comment here goes nowhere. |
| **Attentions**                                                            | Typed, grouped, actionable, with a CLI and two app surfaces                                                                                                                                          | **Not a thread** — a discrete item, explicitly a non-goal ("A general inbox / chat with the agent", [`attentions.md`](../designs/attentions.md) non-goals). Related but distinct.                                                                                            |
| **`boss decision`**                                                       | Product-scoped rulings with `active`/`superseded`/`revoked` and a `superseded_by` pointer                                                                                                            | Not a thread; **but the best existing lifecycle precedent** in the codebase, and a candidate home for the register ([§6](#6-the-pinned-decision-register)).                                                                                                                  |
| **`boss propose`**                                                        | Worker→coordinator mediated hand-back, idempotent, engine-validated                                                                                                                                  | Not a thread, but **the mechanism any threading proposal must use** to stay inside the executor gate.                                                                                                                                                                        |
| **Transcript viewer**                                                     | Post-hoc rendering of an execution                                                                                                                                                                   | Read-only forensics. Not a participant.                                                                                                                                                                                                                                      |
| **Coordinator chat scroll**                                               | The only container in use today                                                                                                                                                                      | No addressability, no state, no per-thread "where did we land", and the app cannot read it ([§7](#7-replacing-the-coordinator-pane)).                                                                                                                                        |

### The Slack analogy, tested to destruction

The operator finds Slack apt and email archaic. Both instincts are informative. Decomposing what a Slack thread actually provides:

**Load-bearing here:**

1. **A stable, addressable container.** You can link to a thread and return to it. _Boss cannot supply this for a coordinator conversation at all_ — the chat scroll has no addresses. It **can** supply it for anything with a row id.
2. **Long-lived participants.** The reason a Slack reply is cheap is that the other party still exists. _This is the crux, and it is [§1](#1-the-mechanism-warm-context-is-the-scarce-resource)._ Boss can supply it only for a live pane.
3. **Cheap append.** Adding to a thread costs nothing and does not restart anything. _Boss fails this today_ — a comment follow-up costs a full dispatch.
4. **Threads can go quiet without dying, and resume later.** _Boss has no representation of a parked thread._
5. **Read/unread and notification on new activity.** _Partially present_ — `comment_topic` invalidation already drives live sidebar updates ([`wire.rs:69-74`](../../protocol/src/wire.rs)); there is no read state.

**Not load-bearing, or actively wrong:** channels (Boss has products/projects already, and a second hierarchy would fight them); presence; emoji reactions; real-time typing indicators; @-mentions across a team of one; message editing.

**Where the analogy breaks, and why the operator's discomfort with email is correct.** In Slack, threads are cheap because participants are _free_ — a human is already there. In Boss a participant costs an execution slot and a workspace lease. **The scarce resource is not the container, it is the participant.** That is why "just add threads" would produce a beautiful surface that still cannot hold a conversation. Email is worse still for this domain, and the operator's instinct is right about why: email's model is _store-and-forward between absent parties_, which is precisely the cold-context model that is already the problem. Email would formalise the defect.

**Two or three properties whose absence causes the pain: (2) long-lived participants, (1) an addressable container, (3) cheap append.** In that order. Property 2 is doing most of the work; without it, 1 and 3 are cosmetic.

### Option A — minimal: make work-item comment threads live

Reuse the shipped comment store. Make `artifact_kind='work_item'` a first-class conversational target: classifier routing, answer agent, follow-up loop — everything bucket 2 already does for doc PRs. Container = the work item. State = the existing comment status machine, which already has `active` / `answering` / `answered` / `awaiting_followup`.

- **Container:** a work item. Addressable, already has a card, already has an id.
- **Reuses:** the entire comment substrate, the classifier, the answer agent, `comment_topic` invalidation, the sidebar.
- **Interactivity:** ✗ **Does not solve the stated problem.** Every turn is a cold `answer_agent` dispatch. Marked as not-a-candidate for the interactivity requirement, per the brief's instruction — it is a legitimate _legibility_ improvement and nothing more.
- **Gets wrong:** it looks like it solves the problem and doesn't. If built alone, the operator would try it once, find it slow, and go back to the coordinator chat — the exact failure being investigated.
- **Cost:** small. Mostly removing a scope guard and pointing `resolve_doc_owner` at work items.

### Option B — the warm thread: hold the participant open

The structurally different option. A thread owns a **live agent session**, not a series of dispatches.

Mechanically: when an investigation completes, instead of releasing the slot, the engine keeps the pane alive in an idle-but-attached state. Operator replies route through `SendInputToWorker` — the existing warm path ([`wire.rs:1818`](../../protocol/src/wire.rs)) — into the still-running process, which answers from context it never lost. A reap policy closes the thread after an idle timeout, returning the slot and the lease; after that, replies fall back to the cold path or file a fresh row.

- **Container:** the work item that spawned the run, or a thread row that references it.
- **Interactivity:** ✓ **Solves it while warm.** Seconds, from retained context. This is the only option in this document that does.
- **Reuses:** `SendInputToWorker`, the pane pool, live-worker state, `comment_thread_entries` for durable transcript.
- **Gets wrong — and these are real, not decorative:**
  - **It occupies a slot.** Interactive slots are 1–16. A handful of parked threads meaningfully reduce throughput. Needs its own pool or a hard cap.
  - **It holds a cube lease.** Parked leases are a known failure surface with existing sweeps (`lost_workspace_sweep`, `CubeLeaseAutoReap`). Adding a deliberately-idle lease-holder means teaching every one of those sweeps that idle is legitimate here — the "cross-driver idle-session-reaping policy question" already open from the session evidence is _exactly this problem_, arriving early.
  - **The window is finite and the cliff is invisible.** After reap, the same question costs a full cold dispatch. The UI must show whether a thread is warm, because a reply that took 3 seconds yesterday taking 90 today with no explanation is worse than consistent slowness.
  - **It re-poses the permission question.** A warm agent answering follow-ups is doing coordinator-ish investigation and will want the class-A reads from [§4](#4-idea-3-a-boss-developing-boss-agent-scope). These two ideas are coupled.
- **Cost:** the largest of the three. New lifecycle, new pool policy, sweep coordination, UI state.

### Option C — durable threads, warm-when-possible (the composite)

State that a thread is a _durable_ object whose warmth is an _optimisation_. Thread rows persist with `open` / `answered` / `parked` / `resolved` and a last-touched time. Replies go to the live pane when one is attached; otherwise they fall back to the cold path with an honest "this will take a minute" affordance.

- **Interactivity:** ✓ while warm, ✗ when cold — but **stated in the UI**, which is the difference between a system that is sometimes slow and a system that feels broken.
- **Gets wrong:** two code paths for one gesture, and a user-visible performance cliff. Mitigated only by being explicit about it, never by hiding it.
- **Cost:** Option A + Option B, sequenced. But it is the shape I would actually build, because the durable half is independently useful and the warm half can land later without redesign.

### Dedup and staleness, as a prerequisite

Per the brief's constraint, not a phase 2:

- **A thread with no terminal state is debris by construction.** `resolved` and `parked` must exist in v1, and `parked` must be visually distinct from `open` — otherwise every abandoned thread reads as pending forever, which is the attentions failure exactly.
- **Auto-expiry on artifact terminality**, deterministic, as in [§6](#6-the-pinned-decision-register): the referenced row or PR reaching a terminal state closes the thread.
- **Dedup rides the existing machinery.** `notification-dedup-scoring`'s taxonomy-aware verdict — "this is already covered by an open row" — is the right primitive and is already half-built ([`work/attentions.rs:1299-1363`](../../engine/core/src/work/attentions.rs)) behind off-by-default flags ([`feature-flags/src/lib.rs:155-174`](../../engine/feature-flags/src/lib.rs)). **Turn those on and observe before building thread-specific dedup.** It addresses a genuinely different axis from staleness, and neither substitutes for the other.
- **A last-touched time on every thread, rendered.** Cheap, and the single most effective anti-debris affordance available.

### Recommendation

**Option C, sequenced: durable thread state first (which is largely the decision register from [§6](#6-the-pinned-decision-register)), warm participants second.** Option A alone is a trap — it looks like the fix and isn't. Option B alone lands a fast thing with no memory of itself.

---

## 9. Decision rule: background agent vs filed row

Ordered, first-match-wins, mirroring the form of the existing reasoning-classification ([`BossPaneModel.swift:558-571`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)) and effort-estimation ([`BossPaneModel.swift:583-600`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)) heuristics. Decidable before the work starts, from the shape of the ask.

> ### Investigation routing (top-to-bottom, first match wins)
>
> Applies when the operator asks a question or requests understanding, before any work begins.
>
> 1. **Single lookup — one CLI call or one file peek → answer inline.** No agent, no row. (Confidence high.) This is the existing inline carve-out; nothing changes.
> 2. **The deliverable is a durable artifact a future session must read → file an `investigation` row.** Tells: "write this up", "document how X works", "I want a record", or you can name a future reader who will need it. (Confidence high.) A doc PR is the correct ceremony; do not substitute a background agent.
> 3. **The output's only consumer is a work item you are about to file → background agent, fold the findings into the brief.** This is bounded pre-work: gathering logs, sweeping code to scope, reading PRs. (Confidence high.) The output lands durably **in the filed row's description**. Existing rule; unchanged.
> 4. **The operator is actively steering and the question may change → background agent, and say so.** Tells: the ask arrived mid-conversation as a follow-up; it is a hypothesis to test rather than a scope to cover; the operator has already refined it once. (Confidence medium.) **A filed row is a one-shot brief and cannot absorb a changing question.** Announce the choice: _"running this as an agent — say the word if you want it filed as an investigation instead."_
>    - **On completion, state the disposition explicitly, in one line:** filed as a row · recorded as a decision · dropped. **Never let a finding end as prose only.**
>    - **If the answer turns out to be durable, file an investigation row retroactively** with the agent's findings inlined verbatim in the brief. Converting late is cheap; losing the finding is not.
> 5. **The evidence is coordinator-private and the answer is durable → file an `investigation` row with the evidence inlined verbatim.** Engine DB rows, log lines, dispatch events, transcript excerpts, conversation quotes. (Confidence high.) The brief is the only channel that context can cross; the executor gate makes this mandatory, not optional.
> 6. **Otherwise → background agent.** (Confidence low. Reason: "fallback.") Apply rule 4's disposition requirement.
>
> ### Where a background agent's output lands
>
> Rules 3, 4 and 6 permit a background agent. **Its report survives only in the chat scroll and a scratchpad file that is in no repo.** That is accepted for rule 3 (the output is consumed by the row it produces) and is a **known, unfixed gap** for rules 4 and 6. Until a durable surface exists, the disposition line in rule 4 is the only thing standing between a finding and its loss. Do not treat "I reported it in chat" as durable.
>
> ### Edge cases
>
> - **The investigate-family markers in the effort heuristic (rule 2 there) do NOT decide this.** They bump _size_. Route from the ask's shape.
> - **Rule 2 beats rule 4.** If the operator wants a writeup, a changing question is a reason to sharpen the brief, not to skip filing.
> - **Rule 5 beats rule 4** when the answer is durable; rule 4 beats rule 5 when it is not. A coordinator-private _and_ transient question is an agent job.
> - **Genuinely ambiguous between rules 2 and 4 → ask.** "Investigation row (writeup) or background agent (fast, chat-only)?" This mirrors the existing investigation-vs-chore ambiguity rule ([`BossPaneModel.swift:733`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)); do not silently default.

**Where this belongs.** `bossSystemPrompt` in [`tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift:408`](../../app-macos/Sources/Ghostty/BossPaneModel.swift) — the checked-in Swift string literal. **Not** the runtime `CLAUDE.md`, which the app rewrites on every launch ([`BossPaneModel.swift:187`](../../app-macos/Sources/Ghostty/BossPaneModel.swift)).

---

## Recommended follow-up work

Not filed — recommended, per the brief. Sized with the standard heuristic.

**Do first (cheap, unblocks judgement):**

| #   | Work                                                                                                        | Effort    | Target                                                     | Notes                                                                                                             |
| --- | ----------------------------------------------------------------------------------------------------------- | --------- | ---------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| 1   | Add the investigation-routing rule to `bossSystemPrompt`                                                    | `small`   | **System-prompt source** — `BossPaneModel.swift`           | Text is drafted above. Not the runtime `CLAUDE.md`.                                                               |
| 2   | Turn on `notification_dedup` (+ `_taxonomy`, `_sensibility`) and observe the attention inventory for a week | `trivial` | **Coordinator-only** — a flag toggle plus a judgement call | Half-built and switched off. Cheapest possible test of the debris hypothesis; may moot part of the register work. |
| 3   | Fix: a project filter hides every product-level investigation                                               | `trivial` | `ChatViewModel.swift:877-884`                              | Direct sibling of the already-fixed issue #886.                                                                   |

**Do next (the substance):**

| #   | Work                                                                                                                              | Effort            | Target                            | Notes                                                                                                                                                         |
| --- | --------------------------------------------------------------------------------------------------------------------------------- | ----------------- | --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 4   | Pending-decision register: schema, engine store, RPC, deterministic auto-expiry on artifact terminality, app surface              | `large` (project) | Engine + app                      | Evaluate reusing the `product_decisions` store first. Lifecycle in v1, not phase 2.                                                                           |
| 5   | Prompt rule obliging the coordinator to keep the register current                                                                 | `small`           | **System-prompt source**          | Blocked on #4. Without it #4 is worse than nothing.                                                                                                           |
| 6   | Boss-developing-boss read scope: per-product opt-in, scoped read verbs over IPC, writes stay closed, hand-back via `boss propose` | `large` (project) | Engine + worker permission config | Must be expressible on all three drivers or explicitly restricted; Grok's deny path is unwired ([`driver/src/grok.rs:120`](../../engine/driver/src/grok.rs)). |

**Do after (needs #4 and #6):**

| #   | Work                                                                         | Effort            | Target       | Notes                                                                                                                      |
| --- | ---------------------------------------------------------------------------- | ----------------- | ------------ | -------------------------------------------------------------------------------------------------------------------------- |
| 7   | Warm thread participants: idle-hold policy, reap timeout, warm/cold UI state | `large` (project) | Engine + app | The only thing that meets the interactivity requirement. Couples to the open cross-driver idle-reaping question.           |
| 8   | Make work-item comment threads live (scope guard + `resolve_doc_owner`)      | `medium`          | Engine       | Only worth doing _with_ #7. Alone it is the trap in [§8 Option A](#option-a--minimal-make-work-item-comment-threads-live). |
| 9   | `kind` facet on the kanban                                                   | `small`           | app          | Only after filing behaviour changes.                                                                                       |

**Defect write-ups found in passing (each independently filable, `small` or less):**

- Follow-up rejected with a `WorkError` while a prior answer run is `answering` ([`app/comments.rs:638-644`](../../engine/core/src/app/comments.rs)) — design describes queuing; not implemented.
- Follow-up classifier failure strands a comment in `awaiting_followup` permanently with no retry and no sweep ([`app/comments.rs:697-714`](../../engine/core/src/app/comments.rs)) — `answering` has `stranded_answering_sweep`; `awaiting_followup` has nothing.
- The answer agent re-fetches the entire document from GitHub on every turn ([`runner/prompt.rs:1475`](../../engine/core/src/runner/prompt.rs)) — correct today given the checkout's ref, but pure waste on a follow-up turn.

**Explicitly not recommended:** replacing the coordinator pane. No requirement in this brief needs it ([§7](#7-replacing-the-coordinator-pane)). Revisit only if the register ships and the coordinator demonstrably fails to keep it current.

---

## Open questions

1. **What is the actual measured wall-clock of one comment follow-up?** I traced the path and cited the engine's own timeout budgets, but did not time a live round trip — that needs engine state mutation, outside a worker's remit. The envelope is defensible; a median is not. **Someone with coordinator access should measure it before #7 is scoped.**

2. **What is in the attention inventory right now?** The debris claim is mechanically explained ([§6](#6-the-pinned-decision-register)) but not empirically confirmed. The store is inside the sandbox that blocked me — which is itself the argument for #6.

3. **How long should a warm thread hold a slot?** Determines whether #7 is affordable. Needs data on how long after a result the operator actually follows up. Nothing in the codebase records this.

4. **Should warm threads have their own pool?** Interactive is 1–16, automation 17–24, review 25–32. Parked threads in the interactive pool would compete with real work. A fourth range is plausible but I have no basis for sizing it.

5. **Can the privileged read scope be enforced on Grok?** Its `--sandbox`/`--allow`/`--deny` path is explicitly unwired ([`driver/src/grok.rs:13`, `:120`](../../engine/driver/src/grok.rs)). If not, is per-driver eligibility acceptable, or does the scope wait?

6. **Should the register reuse `product_decisions` or get its own store?** The lifecycle vocabulary is a near-match and the naming suggests the open half was anticipated. I did not read enough of that store's query patterns to judge whether mixing pending and settled decisions would break its "surface when filing near work" behaviour.

7. **Of the eight escaped investigations, how many were genuinely class A vs. class C?** My taxonomy is built from the brief's summaries, not the investigations themselves. The A:C ratio decides whether #6 or #7 is the better first investment. **The coordinator can answer this from its own session state; I cannot.**

8. **Does the operator want threads on work items, or threads on questions?** I assumed the work item is the natural container because it is addressable and already rendered. But several session threads (the checkleft one) spanned multiple rows and belonged to neither. A thread that is its own first-class object is a different design, and I did not explore it.

9. **Would the coordinator actually keep a register current?** The whole incremental recommendation in [§7](#7-replacing-the-coordinator-pane) rests on the coordinator being a reliable reporter. There is no evidence either way, because nothing has ever asked it to be one. This is the assumption most worth testing early, and #4 plus #5 is the test.
