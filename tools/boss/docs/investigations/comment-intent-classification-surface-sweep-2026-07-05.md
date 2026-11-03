# Surface sweep — Comment intent classification & handling

**Status:** Resolved — no regressions found; every specified surface is PRESENT in shipped code.
**Date:** 2026-07-05
**Author:** Boss worker (investigation*implementation, exec_18bf87c430de1bc0_7d)
**Area:** verification / incident-002 P6 backstop (silent-regression sweep) — not a code defect
**Design:** [`comment-triggered-document-revisions.md`](../designs/comment-triggered-document-revisions.md) (project: \_Comment intent classification & handling*)

## Summary

This is the incident-002 P6 backstop sweep for the _Comment intent classification & handling_ project: confirm that every user-facing surface the design specifies still exists in shipped code on `main`, catching silent regressions where a surface was deleted or downgraded to a placeholder (as happened in incident-002 when a merged planner badge was deleted during a forward-port and stayed gone for days).

**Result: clean.** Every surface the design enumerates is **PRESENT** and matches the design's description. No surface is REGRESSED (downgraded to a stub/placeholder), and no surface is ABSENT except the magic-wand affordance — whose removal is itself a surface the design _ships_ (see [Migration](#the-magic-wand-removal-surface), and it is correctly gone).

The notable finding is not a gap but the opposite: the design reads like a forward-looking, phased plan (its "Current-state grounding" still describes the magic wand as live, and its task breakdown is written in the future tense), yet **the code has raced ahead of the prose** — essentially the whole three-phase design (P1 classifier + intent model, P2 buckets 1&3 + magic-wand removal, P3a–P3d answer agent + thread UI) is already implemented and merged. Only the P3a "Status" note in the design was updated to reflect shipped code; the rest of the doc lags reality. That is a documentation-freshness observation, not a regression — flagged as a follow-up at the end.

## Method

1. Opened the design doc at the path the work item specified — it exists and is current.
2. The design has **no literal `§Surfacing` heading**. The heading that enumerates concrete surfaces is **`### UI / thread behavior (thin client)`** (the five client surfaces), complemented by **`#### Engine RPC surface`** (endpoints), the schema DDL blocks scattered through the body, the worker/dispatch surfaces resolved in **`## Risks / open questions`** (the P3a note), and the **`## Migration: retiring the magic wand`** section (the removal surface). I treated the union of those as the effective surface enumeration.
3. For each surface, verified shipped code on the current checkout of `main` with exact `file:line` evidence (three parallel read-only code sweeps over `tools/boss/engine`, `tools/boss/protocol`, and `tools/boss/app-macos`). No product code was modified.

Design `.md` files under `tools/boss/docs/` are excluded from PRESENT/ABSENT verdicts — only shipped `.rs`/`.swift` code counts.

## Surface inventory

Legend: **PRESENT** — exists and matches the design; **REGRESSED** — exists but downgraded to a placeholder/stub or lost its described behaviour; **ABSENT** — gone entirely.

### Client (macOS) surfaces — design §"UI / thread behavior (thin client)"

| #   | Surface                                                                                                              | Design §ref                               | Status      | Evidence (shipped code)                                                                                                                                                                                                                        |
| --- | -------------------------------------------------------------------------------------------------------------------- | ----------------------------------------- | ----------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | Classification badge on each comment (`classifying…` → directive/question/larger-change icon, clickable to override) | §UI (1); §Classifier "Latency/UX"         | **PRESENT** | `app-macos/Sources/Comments/CommentSidebar.swift:296` `IntentBadge`; `:330` `Label("classifying…", …)`; `:302-310` override menu → `layer.setIntent`; intent enum `Comments/Comment.swift:8-25`; RPC wiring `CommentLayer.swift:472 setIntent` |
| 2   | `[Revise]` banner (`{n} unresolved comment(s). [Revise]`)                                                            | §UI (2); §Buckets 1&3 ¶ line 204          | **PRESENT** | `CommentSidebar.swift:91` `ReviseBanner`; `:121` `"\(count) unresolved comments."`; `:103-104` `Button("Revise") { layer.reviseDoc() }`                                                                                                        |
| 3   | Thinking/typing indicator while an answer agent runs                                                                 | §UI (3); §Bucket 2 "Thinking indicator"   | **PRESENT** | `CommentSidebar.swift:235` `ThinkingIndicatorView`; `:239` `Label("Thinking…", …)`; gated on `comment.answerAgentRunning` at `:386-388`                                                                                                        |
| 4   | Inline thread entries (nudge / answer / follow-up) + follow-up composer                                              | §UI (4); §Reply/link mechanics            | **PRESENT** | `CommentSidebar.swift:188` `ThreadEntriesView` (kind icons `:214-218`); `:253` `FollowupComposer` → `layer.postFollowup`; model `Comment.swift:132` `threadEntries`                                                                            |
| 5   | `in_revision` / `resolved` / `reopened` chip on 1&3-track comments                                                   | §UI (5); §Reply/link mechanics ¶ line 392 | **PRESENT** | `CommentSidebar.swift:132` `RevisionChip`; labels `:145-147`; state `Comment.swift:238-245 RevisionChipState`                                                                                                                                  |

### Engine endpoint / RPC surfaces — design §"Engine RPC surface" et al.

| #   | Surface                                                                                      | Design §ref                                     | Status      | Evidence (shipped code)                                                                                                                                                                                                                   |
| --- | -------------------------------------------------------------------------------------------- | ----------------------------------------------- | ----------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 6   | `CommentsReviseDoc` RPC + `handle_comments_revise_doc` + `ReviseDocInput`/`ReviseDocOutcome` | §Engine RPC surface                             | **PRESENT** | `protocol/src/wire.rs:346` variant; routed `engine/core/src/app.rs:2719`; handler `app/comments.rs:711`; types `protocol/src/types.rs:2742` (`ReviseDocInput`), `:2761` (`ReviseDocOutcome`); logic in `work/revise_doc.rs`               |
| 7   | `CommentsSetIntent` manual-override RPC                                                      | §Misclassification/override ¶ line 129; task 1b | **PRESENT** | `wire.rs:356` variant; routed `app.rs:2720`; handler `app/comments.rs:860`                                                                                                                                                                |
| 8   | Classifier (detached-async LLM call off `CommentsCreate`, not a client RPC)                  | §The classifier (P1)                            | **PRESENT** | module `engine/core/src/comment_classifier.rs` (`call_classifier:146`, `parse_classifier_reply:168`); dispatched `app/comments.rs:48 spawn_comment_classifier`. Matches design intent: engine-triggered, no `CommentsClassify` client RPC |
| 9   | Answer-agent spawn + reply + re-spawn on follow-up (bucket 2)                                | §Bucket 2; tasks 3b/3c                          | **PRESENT** | `app/comments.rs:157 spawn_answer_agent`; `:202 respawn_answer_agent_for_followup` (follow-up loop)                                                                                                                                       |

### Schema / data surfaces

| #   | Surface                                                                                                        | Design §ref                                | Status      | Evidence (shipped code)                                                                                                                                                            |
| --- | -------------------------------------------------------------------------------------------------------------- | ------------------------------------------ | ----------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 10  | `work_comments` intent columns (`intent`, `intent_confidence`, `intent_classified_at`, `intent_overridden_by`) | §Classifier "Output" (SQL block line 120)  | **PRESENT** | `engine/core/src/work/migrations_b.rs:1650-1661` (`migrate_work_comments_intent_columns`); struct `protocol/src/types.rs:3548-3564`                                                |
| 11  | `work_comments.revise_task_id` column + index                                                                  | §Association model (SQL block line 213)    | **PRESENT** | `migrations_b.rs:1694-1699` (`migrate_work_comments_revise_task_id_column` + `work_comments_by_revise_task` index); struct `types.rs:3575`                                         |
| 12  | `comment_thread_entries` table (shared nudge/answer/follow-up store)                                           | §Reply/link mechanics (SQL block line 377) | **PRESENT** | `migrations_b.rs:1097` (`migrate_comment_thread_entries_table`); CRUD `work/comment_thread_entries.rs`; struct `types.rs:2266`                                                     |
| 13  | `answer_agent_runs` table                                                                                      | §Bucket 2 (SQL block line 340)             | **PRESENT** | `migrations_b.rs:1069` (`migrate_answer_agent_runs_table`); CRUD `work/answer_agent_runs.rs`; struct `types.rs:2207`                                                               |
| 14  | New comment status values `in_revision` / `answering` / `answered` / `awaiting_followup`; `dispatched` removed | §Comment/thread state machine              | **PRESENT** | `types.rs:954/961/966/975` (the four new constants); `dispatched` correctly gone — appears only in the retirement migration `migrations_b.rs:1725` (`WHERE status = 'dispatched'`) |

### Worker / dispatch surfaces (answer-agent read-only sandbox) — design §Risks P3a

| #   | Surface                                                                                                                                              | Design §ref                                   | Status      | Evidence (shipped code)                                                                                                                                                                                                                                       |
| --- | ---------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------- | ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 15  | `WorkerKind::AnswerAgent` + `answer_agent` execution kind + exhaustive `worker_kind_for_execution`                                                   | §Risks (P3a resolved); task 3a                | **PRESENT** | `worker_setup.rs:80` enum, `:114 AnswerAgent`, `:151 worker_kind_for_execution` (no `_` arm); execution kind `protocol/src/types.rs:423`, const `EXECUTION_KIND_ANSWER_AGENT` `:671`; used at `runner.rs:491`, `host_adapter.rs:717`                          |
| 16  | `dontAsk` deny-by-default posture: `permissions_value` + `answer_agent_allow_rules` + `answer_agent_deny_rules` + forced `--permission-mode dontAsk` | §Risks (P3a resolved)                         | **PRESENT** | `worker_setup.rs:550 permissions_value` (`"defaultMode":"dontAsk"` at `:553`); allow `:730`, deny `:755`; forced mode `:135 forced_permission_mode`; CLI emission `driver/claude.rs:309-315` (test asserts `dontAsk` present / `auto` absent `claude.rs:983`) |
| 17  | Read-only answer-agent `CLAUDE.md`                                                                                                                   | §Risks (P3a resolved) — "read-only CLAUDE.md" | **PRESENT** | `answer_agent.rs` module; `worker_setup.rs:242-246 render_answer_agent_claude_md`                                                                                                                                                                             |

### The magic-wand removal surface — design §"Migration: retiring the magic wand"

The design _ships a removal_: the correct shipped state is that the magic-wand affordance is gone, replaced by surfaces 1–5. Verified — the removal is complete.

| #   | Removed surface                                                                     | Design §ref                   | Status                                                             | Evidence                                                                                                                                                        |
| --- | ----------------------------------------------------------------------------------- | ----------------------------- | ------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 18a | `CommentsDispatchMagicWand` / `ApplyMagicWand` / `DiscardMagicWand` RPCs + handlers | §Migration (1,2); task 2e     | **Removed as designed** (ABSENT, intended)                         | No such variant/handler in any `.rs` (grep clean); removal PR #1738                                                                                             |
| 18b | `engine/core/src/magic_wand.rs` dispatch module                                     | §Migration (3)                | **Removed as designed**                                            | `find tools/boss -name magic_wand.rs` → nothing                                                                                                                 |
| 18c | `MagicWandResultSheet.swift` + sidebar wand affordance                              | §Migration (5)                | **Removed as designed**                                            | No such Swift file; no `magic`/`wand` reference anywhere under `app-macos/Sources/Comments/`                                                                    |
| 18d | `dispatched` comment status                                                         | §Migration (4)                | **Removed as designed**                                            | No `COMMENT_STATUS_DISPATCHED`; only referenced by the retirement migration                                                                                     |
| 18e | `magic_wand_dispatches` table (DDL retained, unread)                                | §Migration (6) ¶ line 425/433 | **PRESENT by design** (kept as historical record, no live writers) | DDL `migrations_b.rs:1017-1035` + retirement migration `:1724 migrate_retire_magic_wand_dispatched_comments`. Design explicitly says do _not_ drop it — matches |

## Implementing PRs (evidence trail)

The design (PR #1681, merged ~2026-06-30) was implemented rapidly across, at least:

- **P1 — classifier + intent model:** #1690 (intent columns + async classifier plumbing), #1711 (classifier → `claude_client` crate), #1786 (classifier unit tests).
- **P2 — buckets 1&3 + magic-wand removal:** #1709 (`CommentsReviseDoc`), #1730 (completion reconciliation, P2c), #1738 (**magic-wand removal**), #1740 (banner + chips UI, P2f).
- **P3 — answer agent + thread loop:** #1713 (P3a: `answer_agent_runs` + execution kind + restricted dispatch), #1756 (P3c: follow-up reclassification + bridge), #1757 (P3d: thread UI).
- **Thin-client wiring:** #1794 (engine-back `CommentLayer`, P529 Phase-2), #1798 (swap intent/banner/revise stubs for real RPCs), #1803 (swap P3d thread-UI stubs for real answer-agent flow).

## Observations (not regressions)

- **`set_comment_status` validator scope.** The public `set_comment_status` validator (`engine/core/src/work/comments.rs:154-156`) accepts only `active | resolved | orphaned | dismissed` and rejects the four new bucket states. This is **by design, not a regression**: the new states (`in_revision`, `answering`, `answered`, `awaiting_followup`) are reachable only via dedicated guarded transition methods (`transition_comment_to_answering` `comments.rs:191`, `transition_comment_answering_to_active` `:214`, `transition_comment_to_answered` `:236`, etc.), matching the state-machine's "who/idempotency" columns. No user-facing surface depends on `set_comment_status` accepting them.

## Conclusion

**Every user-facing surface the design specifies is PRESENT in shipped code and matches its description.** There are zero REGRESSED and zero unexpectedly-ABSENT surfaces. The only ABSENT surfaces are the magic-wand affordances, whose removal is a deliberate surface the design ships and which is correctly complete. No restoration task is warranted from this sweep.

## Follow-up (out of scope for this investigation — for the operator to file)

- **Refresh the design doc to reflect shipped reality.** The design's "Current-state grounding" still describes the magic wand as live infrastructure, and its "Proposed implementation task breakdown" reads in the future tense, even though P1–P3d are all merged. Only the P3a "Status" note was updated. A documentation pass marking each phase's task as shipped (with PR references) would prevent a future reader from mistaking an implemented design for an unbuilt plan. Doc-only change; no code impact.
