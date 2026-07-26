-------------------------- MODULE PrReviewLifecycle --------------------------
(***************************************************************************)
(* A model of the Boss engine's `pr_review` execution lifecycle and the     *)
(* churn guard in `pr_review_recovery`.                                     *)
(*                                                                          *)
(* Transcribed from `spinyfin/mono` at commit                               *)
(*   d7c26f6c9c72cd6181138724702eb7fda586f0a0                               *)
(* Paths below are relative to `tools/boss/`.                               *)
(*                                                                          *)
(* PR #2331 ("scope pr_review churn guard to pr_review-kind executions")    *)
(* is OPEN and NOT merged at that commit, so this models main's behaviour   *)
(* including whatever defects it has. Nothing in this module encodes the    *)
(* proposed fix (no `Option<ExecutionKind>` parameter on the count).        *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, Sequences

CONSTANTS
    MaxExecs,              \* state-space bound: executions ever created
    MaxImpls,              \* state-space bound: implementation-kind executions.
                           \* Separate from MaxExecs so impl churn can never
                           \* consume the whole budget and starve the review of
                           \* a slot. Without this the model produces liveness
                           \* "violations" that are pure bound artifacts: a
                           \* trace where MaxExecs is exhausted by impls, no
                           \* pr_review is ever created, and the behaviour just
                           \* stutters. That is the model running out of room,
                           \* not the engine failing.
    MaxTime,               \* state-space bound: logical clock ticks
    MaxDeaths,             \* state-space bound: total worker deaths
    ChurnWindow,           \* ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS  (work.rs:44)
    ChurnThreshold,        \* ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD    (work.rs:48)
    TransientResumeEnabled,\* is the death class one `transient_recovery` handles?
    DeathStatuses,         \* which statuses a reaper may write; see below
    KindScopedCount        \* see below -- POST-HOC CONTROL, not part of the blind spec

(***************************************************************************)
(* VOCABULARY — protocol/src/types/execution.rs                             *)
(***************************************************************************)

\* execution.rs:92-106. All twelve, for the record. The five commented
\* out are unreachable in THIS lifecycle: a pr_review is minted directly
\* into `ready` and a work_execution never sits in waiting_review /
\* waiting_merge (those are *task* states, not execution states).
AllStatuses == { "queued", "ready", "waiting_dependency", "running",
                 "waiting_human", "waiting_review", "waiting_merge",
                 "completed", "failed", "abandoned", "cancelled", "orphaned" }

Terminal == { "completed", "failed", "abandoned", "cancelled", "orphaned" }  \* is_terminal(), execution.rs:126-131
Live     == { "running", "waiting_human" }                                   \* is_live(),     execution.rs:133-135

\* Statuses actually reachable here.
Statuses == { "ready", "running" } \union Terminal

\* execution.rs:14-35 has eleven kinds. They collapse to two for this
\* question, because of a load-bearing structural asymmetry:
\* `execution_kind_for_work_item` (work/exec_status_helpers.rs:3-31)
\* can never return PrReview -- every generic redispatch path mints an
\* *implementation* kind. So "impl" here stands for the whole family
\* {chore,task,revision,investigation,...}_implementation, and only the
\* three dedicated sites can mint "pr_review".
Kinds == { "impl", "pr_review" }

\* What a reaper can write. Every sweep in the incident table writes
\* `orphaned` (spawn_ack_sweep.rs:341, stale_worker_sweep.rs:304,
\* dead_pid_sweep.rs:685, dead_pane_sweep, host_reconcile.rs,
\* cube_lease_heartbeat.rs:749). The pane-spawn / SlotBusy path writes
\* `failed` (coordinator.rs:6709-6789). Explicit cancellation writes
\* `cancelled`; give-up writes `abandoned`.
\* DeathStatuses is a CONSTANT (see above), normally
\*   { "orphaned", "failed", "cancelled", "abandoned" }.
\* Configs narrow it to isolate a death class: restricting it to the
\* statuses the guard actually counts is what makes a liveness
\* counterexample about the GUARD rather than about `cancelled` slipping
\* past it. AllDeathStatuses records the full set for reference.
AllDeathStatuses == { "orphaned", "failed", "cancelled", "abandoned" }

\* The status set the churn guard COUNTS.
\*   work/dispatch.rs:830-846 `count_recent_terminal_executions`
\*   work/dispatch.rs:848-869 `list_recent_terminal_execution_ids`
\* Both: status IN ('orphaned','abandoned','failed'). Note `cancelled`
\* is absent, and there is NO kind predicate at all.
GuardCountedStatuses == { "orphaned", "abandoned", "failed" }

\* The status set the candidate query treats as a DEAD REVIEW.
\*   work/dispatch.rs:789-824 `list_dead_pr_review_candidates`
\* status IN ('orphaned','abandoned','failed','cancelled') AND kind='pr_review'.
DeadReviewStatuses == { "orphaned", "abandoned", "failed", "cancelled" }

(***************************************************************************)
(* STATE                                                                    *)
(***************************************************************************)

VARIABLES
    execs,   \* Seq of [kind, status, created] -- the work_executions rows
    clock,   \* logical epoch seconds
    prOpen,  \* tasks.pr_url IS NOT NULL AND != ''
    parked,  \* an open `churn_guard_parked` attention item (work.rs:60)
    deaths,  \* bound on how many times a worker may die
    sweep    \* pr_review_recovery's *in-flight pass* state; see below

vars == << execs, clock, prOpen, parked, deaths, sweep >>

ExecIds == DOMAIN execs

(***************************************************************************)
(* QUERIES                                                                  *)
(***************************************************************************)

\* The window predicate is keyed on `created_at`, NOT on death time:
\*   AND CAST(created_at AS INTEGER) >= ?2        (dispatch.rs:834, :853)
\* `created_at` is epoch-seconds-as-string (work/audit_misc.rs:76-78), so
\* the CAST is sound. Written as addition to stay inside Nat.
InWindow(i) == execs[i].created + ChurnWindow >= clock

\* count_recent_terminal_executions -- dispatch.rs:830-846.
\* NO kind filter. `KindScopedCount = FALSE` is main, faithfully, and is
\* what the blind spec was written and first checked with.
\*
\* KindScopedCount = TRUE was added AFTER the blind run had already
\* produced the counterexample below, purely as a control: it shows the
\* invariant is falsifiable-but-not-always-false, i.e. that TLC is
\* discriminating between two implementations rather than rejecting
\* everything. It is not a transcription of PR #2331's actual patch.
CountRecentTerminal ==
    Cardinality({ i \in ExecIds : /\ execs[i].status \in GuardCountedStatuses
                                  /\ InWindow(i)
                                  /\ (KindScopedCount => execs[i].kind = "pr_review") })

\* The same count restricted to the reviews' OWN history. Not a function
\* in main -- this exists only to state the safety property below.
CountRecentTerminalReviews ==
    Cardinality({ i \in ExecIds : /\ execs[i].kind = "pr_review"
                                  /\ execs[i].status \in DeadReviewStatuses
                                  /\ InWindow(i) })

ReviewIds == { i \in ExecIds : execs[i].kind = "pr_review" }

\* dispatch.rs:812-819: "latest" is scoped to kind='pr_review' rows.
\* Ids are monotonic in creation order here, so max id == latest.
LatestReview == IF ReviewIds = {} THEN 0
                ELSE CHOOSE i \in ReviewIds : \A j \in ReviewIds : j <= i

\* finalize_pr_review_pass is the ONLY path that takes a pr_review to
\* `completed` (dispatch.rs:777-783 doc comment), so this is exactly
\* "a ReviewResult was produced".
ReviewDone == \E i \in ExecIds : /\ execs[i].kind = "pr_review"
                                 /\ execs[i].status = "completed"

\* list_dead_pr_review_candidates' WHERE clause.
HasDeadReviewCandidate ==
    /\ prOpen
    /\ ReviewIds # {}
    /\ execs[LatestReview].status \in DeadReviewStatuses

AnyLive        == \E i \in ExecIds : execs[i].status \notin Terminal
PendingReview  == \E i \in ExecIds : /\ execs[i].kind = "pr_review"
                                     /\ execs[i].status \notin Terminal

NewExec(k) == [ kind |-> k, status |-> "ready", created |-> clock ]

(***************************************************************************)
(* INIT                                                                     *)
(***************************************************************************)

Init ==
    /\ execs  = << >>
    /\ clock  = 0
    /\ prOpen = FALSE
    /\ parked = FALSE
    /\ deaths = 0
    /\ sweep  = [ pc |-> "idle", cand |-> 0, cnt |-> 0, nExecs |-> 0 ]

(***************************************************************************)
(* WORKER LIFECYCLE ACTIONS                                                 *)
(***************************************************************************)

\* Any generic dispatch/redispatch. Always mints an *implementation*
\* kind -- exec_status_helpers.rs:3-31 has no PrReview arm. Enabled
\* after prOpen too: that models revision_implementation runs answering
\* review comments, which also feed the guard's counter.
ImplCount == Cardinality({ i \in ExecIds : execs[i].kind = "impl" })

DispatchImpl ==
    /\ ~AnyLive
    /\ ImplCount < MaxImpls
    /\ Len(execs) < MaxExecs
    /\ execs' = Append(execs, NewExec("impl"))
    /\ UNCHANGED << clock, prOpen, parked, deaths, sweep >>

StartExec ==
    \E i \in ExecIds :
        /\ execs[i].status = "ready"
        /\ execs' = [ execs EXCEPT ![i].status = "running" ]
        /\ UNCHANGED << clock, prOpen, parked, deaths, sweep >>

\* One of the reapers wins. `mark_execution_orphaned` bails if the row
\* is already terminal (work/executions_runs.rs:97-102) -- that CAS is
\* what makes concurrent reapers safe, and it is why exactly one death
\* status lands per execution. Requiring status="running" models it.
Die ==
    \E i \in ExecIds, d \in DeathStatuses :
        /\ deaths < MaxDeaths
        /\ execs[i].status = "running"
        /\ execs' = [ execs EXCEPT ![i].status = d ]
        /\ deaths' = deaths + 1
        /\ UNCHANGED << clock, prOpen, parked, sweep >>

ImplCompletes ==
    \E i \in ExecIds :
        /\ execs[i].kind = "impl"
        /\ execs[i].status = "running"
        /\ execs' = [ execs EXCEPT ![i].status = "completed" ]
        /\ prOpen' = TRUE
        /\ UNCHANGED << clock, parked, deaths, sweep >>

\* The FIRST review only. After that, `pr_review_recovery` is the sole
\* path back, because a dead review is:
\*   - not demoted to todo on SlotBusy   (coordinator.rs:6857)
\*   - given no inline request_execution (coordinator.rs:7029)
\*   - excluded from orphan_sweep        (work/dispatch.rs:742-754)
\* create_pr_review_execution_dedup (dispatch.rs:100) reuses a live one,
\* hence the ~PendingReview guard.
RequestFirstReview ==
    /\ prOpen
    /\ ReviewIds = {}
    /\ ~PendingReview
    /\ Len(execs) < MaxExecs
    /\ execs' = Append(execs, NewExec("pr_review"))
    /\ UNCHANGED << clock, prOpen, parked, deaths, sweep >>

ReviewCompletes ==
    \E i \in ExecIds :
        /\ execs[i].kind = "pr_review"
        /\ execs[i].status = "running"
        /\ execs' = [ execs EXCEPT ![i].status = "completed" ]
        /\ UNCHANGED << clock, prOpen, parked, deaths, sweep >>

(***************************************************************************)
(* pr_review_recovery::run_one_pass -- SPLIT INTO STEPS ON PURPOSE          *)
(*                                                                          *)
(* Every sweep is an independent, unsynchronised `tokio::spawn`             *)
(* (sweep_loop.rs `spawn_sweep_loop`: fire-immediately-then-sleep, no       *)
(* cross-sweep lock). All WorkDb calls share one sqlite connection behind   *)
(* a std::sync::Mutex, so each INDIVIDUAL call is atomic -- but a pass is   *)
(* a *sequence* of such calls with NO enclosing transaction. So the pass    *)
(* is modelled as three separate actions with a program counter, letting    *)
(* any other action interleave between them. A single-atomic-pass model     *)
(* would hide exactly the interesting states.                               *)
(***************************************************************************)

RecoveryScan ==            \* list_dead_pr_review_candidates
    /\ sweep.pc = "idle"
    /\ HasDeadReviewCandidate
    /\ sweep' = [ pc |-> "scanned", cand |-> LatestReview, cnt |-> 0,
                  nExecs |-> Len(execs) ]
    /\ UNCHANGED << execs, clock, prOpen, parked, deaths >>

RecoveryCount ==           \* count_recent_terminal_executions
    /\ sweep.pc = "scanned"
    /\ sweep' = [ sweep EXCEPT !.pc = "acting", !.cnt = CountRecentTerminal ]
    /\ UNCHANGED << execs, clock, prOpen, parked, deaths >>

RecoveryPark ==            \* pr_review_recovery.rs:155-175 -- the `continue`
    /\ sweep.pc = "acting"
    /\ sweep.cnt >= ChurnThreshold
    /\ parked' = TRUE
    /\ sweep' = [ sweep EXCEPT !.pc = "idle" ]
    /\ UNCHANGED << execs, clock, prOpen, deaths >>

\* request_pr_review. Clearing `parked` mirrors the self-heal at
\* work/dispatch_helpers.rs:1362 and :1374.
RecoveryRefire ==
    /\ sweep.pc = "acting"
    /\ sweep.cnt < ChurnThreshold
    /\ Len(execs) < MaxExecs
    /\ ~PendingReview
    /\ execs' = Append(execs, NewExec("pr_review"))
    /\ parked' = FALSE
    /\ sweep' = [ sweep EXCEPT !.pc = "idle" ]
    /\ UNCHANGED << clock, prOpen, deaths >>

\* Not a real transition: lets a pass retire when the MODEL's exec bound
\* is exhausted, so hitting the bound does not deadlock the spec.
RecoveryAbort ==
    /\ sweep.pc = "acting"
    /\ sweep.cnt < ChurnThreshold
    /\ (Len(execs) = MaxExecs \/ PendingReview)
    /\ sweep' = [ sweep EXCEPT !.pc = "idle" ]
    /\ UNCHANGED << execs, clock, prOpen, parked, deaths >>

(***************************************************************************)
(* The OTHER recovery path.  transient_recovery.rs:444 calls                *)
(* request_resume_execution (work/executions_runs.rs:184-233), which        *)
(* copies `dead.kind` verbatim -- so a dead pr_review is resumed AS a       *)
(* pr_review. This path never consults the churn guard.                     *)
(*                                                                          *)
(* Gated on a constant because it only covers the death classes             *)
(* transient_recovery classifies as transient. A SlotBusy pane-spawn        *)
(* `failed` is explicitly NOT one of them, which is the incident case.      *)
(***************************************************************************)
TransientResume ==
    /\ TransientResumeEnabled
    /\ ReviewIds # {}
    /\ execs[LatestReview].status = "orphaned"
    /\ ~PendingReview
    /\ Len(execs) < MaxExecs
    /\ execs' = Append(execs, NewExec("pr_review"))
    /\ UNCHANGED << clock, prOpen, parked, deaths, sweep >>

Tick ==
    /\ clock < MaxTime
    /\ clock' = clock + 1
    /\ UNCHANGED << execs, prOpen, parked, deaths, sweep >>

(***************************************************************************)
(* SPEC                                                                     *)
(***************************************************************************)

Next ==
    \/ DispatchImpl \/ StartExec \/ Die \/ ImplCompletes
    \/ RequestFirstReview \/ ReviewCompletes
    \/ RecoveryScan \/ RecoveryCount \/ RecoveryPark \/ RecoveryRefire \/ RecoveryAbort
    \/ TransientResume
    \/ Tick

\* No fairness on Die or DispatchImpl -- failures are permitted, never
\* forced. Everything the engine is supposed to do on its own gets weak
\* fairness: the sweeps do run, forever, on their interval.
Fairness ==
    /\ WF_vars(StartExec)
    /\ WF_vars(ImplCompletes)
    /\ WF_vars(RequestFirstReview)
    /\ WF_vars(ReviewCompletes)
    /\ WF_vars(RecoveryScan)
    /\ WF_vars(RecoveryCount)
    /\ WF_vars(RecoveryPark)
    /\ WF_vars(RecoveryRefire)
    /\ WF_vars(RecoveryAbort)
    /\ WF_vars(TransientResume)
    /\ WF_vars(Tick)

Spec == Init /\ [][Next]_vars /\ Fairness

(***************************************************************************)
(* PROPERTIES                                                               *)
(*                                                                          *)
(* Written from what the code is FOR, not from what it does. See the        *)
(* writeup: transcribing the guard's own comment instead gives an           *)
(* invariant that main satisfies, and finds nothing.                        *)
(***************************************************************************)

TypeOK ==
    /\ clock  \in 0..MaxTime
    /\ deaths \in 0..MaxDeaths
    /\ prOpen \in BOOLEAN
    /\ parked \in BOOLEAN
    /\ sweep.pc \in { "idle", "scanned", "acting" }
    /\ sweep.nExecs \in 0..MaxExecs
    /\ \A i \in ExecIds : /\ execs[i].kind   \in Kinds
                          /\ execs[i].status \in Statuses

\* SAFETY. `pr_review_recovery` exists so an open PR is never left
\* unreviewed; its guard is there to stop a REVIEW that keeps dying.
\* So parking is only ever justified by the item's own review history.
\* This says nothing about how the count should be implemented.
GuardParksOnlyOnReviewChurn ==
    parked => CountRecentTerminalReviews >= ChurnThreshold

\* Does the sweep act on the state it scanned?  This is a PROBE, not a
\* property anyone would write to hunt a bug: it asserts the pass is
\* atomic, which it plainly is not (no enclosing transaction around
\* run_one_pass's sequence of WorkDb calls). Its counterexample is the
\* cheapest way to make TLC exhibit the mid-pass interleaving window.
NoMutationMidPass ==
    (sweep.pc \in { "scanned", "acting" }) => Len(execs) = sweep.nExecs

\* CONSISTENCY. The set of statuses the guard counts as "a death" must
\* agree with the set the candidate query calls "a dead review".
\* Constant-level: needs no state exploration at all.
GuardAndCandidateAgreeOnDeath ==
    GuardCountedStatuses = DeadReviewStatuses

\* LIVENESS. An open PR whose review dies eventually gets a
\* ReviewResult. Meaningful only because `deaths` is bounded: after at
\* most MaxDeaths failures the system must still converge.
ReviewEventuallyLands == prOpen ~> ReviewDone

\* Weaker probe: does a parked item ever recover on its own, i.e. does
\* the window drain and the sweep self-heal?
ParkingIsNotPermanent == parked ~> ~parked

StateConstraint == Len(execs) <= MaxExecs /\ clock <= MaxTime

=============================================================================
