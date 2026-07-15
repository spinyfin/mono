# Fleet scaling, the slot model, and team semantics

_Direction notes from an operator/coordinator discussion, 2026-07-15. Nothing in this document is scheduled work; it captures conclusions and design intuitions to inform future projects (remote workers, the slot-model rethink, and any future "team semantics" features). Where an idea was explicitly deferred or rejected, that is recorded too._

## Context: the 2026-07-15 saturation experiment

For the first time, every slot across Bridge Crew, Lower Decks, and the automation pool ran simultaneously — 22 concurrent workers on one laptop. The machine reached a load average of ~152 (roughly 10x oversubscription, ~9,800 threads). Outcomes:

- **Individual tasks took over an hour** — far slower than the same work run at lower concurrency. Past the core count, added local concurrency was negative-sum for these compile-heavy workloads: total throughput went _down_, not just latency up.
- Infrastructure failed in cascades: pane spawns could not produce a shell within the 60s ack window, tripping the spawn-capability breaker twice; `cube pr create`'s checkleft gate stretched to multiple minutes and read as a hang; the serial dispatch drain compounded everything.
- The experiment was judged worthwhile: it produced hard numbers justifying remote workers, and it shook loose a series of real engine fixes (breaker flag-gating, drain parallelization, queue telemetry, orphan reaping, pause visibility/audit).

**Conclusion: this class of workload scales with remote support and well-provisioned machines, not with more local slots.** Remote worker support is the operator-owned path forward.

## Near-term: a local concurrency cap

Slot counts are not a capacity control — they were sized for scheduling availability, not for what the machine can sustain. A **global local-concurrency cap**, distinct from slot counts, is agreed as the missing knob until remote capacity exists.

Interaction to watch: the automation-spillover work and the automation-pool size increase both _raise_ the achievable local concurrency ceiling. They are tactical improvements on the current model and should not be allowed to recreate the saturation scenario by default.

## The slot model is flawed and will be rethought

Today's slot conflates three distinct concerns:

1. **UI real estate** — a pane/tab position in the app.
2. **Capacity control** — how much work can run at once.
3. **Worker identity** — which named agent occupies the seat.

That conflation is the common shape behind several recurring defect classes: engine↔app slot-state desyncs (`SlotBusy` on a slot the app considers occupied), capacity being mis-expressed as seat availability, and pause/park states that read as "waiting for a slot" when slots are not the constraint.

**Future direction (a project for another day, not scheduled):** a dynamic model — panes are brought up on demand, bounded by explicit _local and remote_ limits rather than a fixed seat grid. Capacity becomes budgets; UI becomes presentation; identity becomes its own thing (below). Until that lands, slot-machinery changes should be treated as tactical, and changes that deepen slot-model coupling — particularly anything that further entangles dispatch policy with pane management — deserve extra scrutiny at review time.

## Character identity survives the slot model

A worry: dynamic panes end the tableau of named crew members waiting at their stations. The identity layer is worth keeping regardless — "Kim is running a whole-repo sweep" is instantly graspable in a way an execution id never is, and that legibility repeatedly proved operationally useful.

In a dynamic model, the roster detaches from seats: identities form a pool; a pane spawns, a crew member is assigned for the mission, and returns to the roster when the pane closes. The charm survives; the fixed seats do not.

## Team semantics: a selection rule for humanisms

Boss deliberately emulates a team, which makes human semantics tempting: specializations, individual memory, inter-agent chat, even performance reviews. The overfitting worry is real — agents should also get _inhuman_ advantages (e.g. a core shared memory) where human structure is merely a workaround for human limitations.

The selection rule that emerged:

> **Adopt a human team structure if and only if (a) it compensates for a constraint that genuinely applies to agents, or (b) it makes the system more legible to the human operating it. Refuse structures that simulate human _limitations_ for flavor** (forgetting, ego, information hoarding, "not my area").

Applying the rule to the candidates discussed:

| Humanism                           | Underlying human constraint                    | Applies to agents?                                                               | Verdict                                                                                                      |
| ---------------------------------- | ---------------------------------------------- | -------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| Individual private memory          | Humans _cannot_ share memory                   | Partly — sharing is possible, but retrieval may not scale with store size        | **Hybrid, as a hypothesis to test**: shared core store + individual specialized memories, uplifted on demand |
| Specialization                     | Loading context into a head is expensive       | **Yes** — context windows are small, investigation is expensive                  | Keep, as _domain grooves_: memory attaches to domains; characters carry the domains they have worked         |
| Watercooler / chat channel         | Formal channels miss cross-cutting information | **Yes** — workers currently coordinate only through merged code and the operator | Keep: it is pub/sub coordination wearing a legible skin                                                      |
| Performance reviews                | Incentive management + feedback aggregation    | Incentives: no. Aggregation: **yes**                                             | Keep the aggregation half; discard the incentive half                                                        |
| Forgetting, fatigue, ego, hoarding | (Human limitations)                            | —                                                                                | Refuse; these would be pure simulation                                                                       |

### The memory model is a hypothesis, not a conclusion

An earlier draft of this section claimed shared memory is strictly better than individual memory. The operator's correction stands: **that is a hypothesis worth testing, not a fact.** LLMs likely saturate when carrying too much memory context — retrieval gets harder as the store grows — so a single ever-growing shared store may degrade exactly the way a cluttered head does. The more likely ideal: **a shared core store plus individual specialized memories that can be uplifted on demand** — a character arrives with a narrow default working set (the domains they have worked), pulls from the shared core when needed, and validated individual lessons can be promoted into the core. This still captures the real value of specialization (amortized investigation cost — a worker who already knows a subsystem's cache topology does not re-derive it, which the flunge regional-standings saga demonstrated across four fix attempts) without the human downside of siloed knowledge, while respecting retrieval limits. Whatever ships should be instrumented well enough to test the hypothesis rather than assume it.

Also: **memory takes many forms, and most of them already exist.** The repo itself is memory — its markdown docs answered a live operational question the same day this was written; the code is memory; the Boss metadata (task descriptions, audit tags, effort classifications, dispatch events) is memory. An agent-memory store is one more layer on top of these, not a replacement for them — the design problem is retrieval _across_ substrates (when should a worker consult a design doc vs a remembered lesson vs the row's audit trail), and any new store should justify itself against "could this have lived in the repo or the metadata instead?"

Two failure modes to design against, both observed live in the coordinator's own memory system:

- **Stale memory is worse than no memory.** A meaningful fraction of remembered facts were corrected or invalidated within a single day (bugs marked broken got fixed; "known" topology changed). Character memories need grooming, decay, and correction paths — and their contents must be auditable, because a worker quietly acting on a wrong remembered lesson is the worst failure mode available. This grooming is a natural fit for a maintenance agent, but is explicitly **not** being added to Boothby's v1 scope (see below).
- **Specialists erode fungibility.** The moment the dispatcher prefers the specialist, work queues behind a busy character while generalists idle — a miniature of the saturation lesson. Specialization should be a routing _preference with a timeout_, never a hard bind.

### The watercooler is not (just) a humanism

Concrete motivating incident: two workers independently extracted overlapping crate boundaries from the same subtree (duplicate chores producing colliding PRs), purely because neither could see the other's in-flight work. A channel where a worker announces "I'm extracting the GitHub transport out of engine/core" — and others can see it before duplicating — is coordination infrastructure. The Slack-like skin matters because the operator reads it: a chat surface is skimmable and auditable in a way a message bus is not.

### Performance reviews, seriously

The joke survives scrutiny in one specific form: Boss already accumulates per-row performance data with nowhere to go — effort-classification audit tags, escalation markers, review cycles, PR outcomes. A "performance review" is the periodic per-character aggregation of that data plus run outcomes, used to tune routing, effort estimation, and prompt guidance (e.g. "this character's PRs pass review first-try; route them larger refactors"). It is a feedback loop wearing an HR costume; the costume is optional, the loop is not.

## Worker self-retrospectives (idea; deliberately unscoped)

Operator-endorsed idea, recorded here rather than attached to any existing subsystem: a lightweight **end-of-run self-retrospective** — a few structured fields emitted at the Stop boundary (what slowed me down / what I would do differently / what tooling fought me), persisted alongside the execution.

Rationale: logs record what happened; the worker can report what it _was like_. During the saturation experiment, workers experienced every failure (hung-looking gates, lock waits, empty check output driving `--all` escalations) long before the operator noticed, and all of that context evaporated at end-of-run because nothing asks for it. Aggregating retros across the fleet would turn every execution into improvement telemetry for Boss itself.

**Explicitly deferred:** this was considered as a Boothby input and rejected for now — Boothby is already a very complex subsystem and should not absorb new scope. If pursued, it is its own feature with its own design, and whatever consumes the aggregated retros is a separate decision.

## Summary of dispositions

- **Agreed / near-term:** global local-concurrency cap (capacity ≠ slot counts).
- **Agreed / direction:** remote workers + provisioned machines are how this workload scales; the saturation experiment is the supporting evidence.
- **Future project (unscheduled):** replace the slot model with dynamic pane provisioning under local + remote budgets; keep the character roster, detached from seats.
- **Design intuitions for future team-semantics work:** memory model as a hypothesis to test — shared core store + individual specialized memories with on-demand uplift, mindful that retrieval may degrade as stores grow and that the repo, code, and Boss metadata are already memory substrates; domain-attached specialization as preference-with-timeout; a watercooler channel as legible pub/sub; performance reviews as per-character feedback aggregation; refuse simulated human limitations. Memory contents must be groomed and auditable.
- **Idea parked, unscoped:** worker end-of-run self-retrospectives; explicitly not part of Boothby.
