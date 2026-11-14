# Antigravity shared HOME and permissions enforced by hooks

- **Measurement date:** 2026-11-14 UTC (November 13–14 local time).
- **Scope:** compare a shared real `HOME` with per-run `HOME`; documentation only, with no driver or configuration rollout.
- **Host:** macOS 27.0.1 (26A434), arm64.
- **Related:** [driver design](../designs/antigravity-driver-fourth-driver-google-gemini-cli.md), [corrected credential study](scoped-home-credential-delegation.md), [hook study](agy-hook-surface.md), [acceptance ledger](../designs/antigravity-driver-acceptance-ledger.md).

Shared `HOME` deserves reconsideration: one persistent settings file does not imply one session policy, and successful concurrent use is evidence against assuming that the shared store is inherently unsafe. This investigation separates those observations from the stronger claim that a global hook can safely replace every worker permission boundary.

## Verdict

**Retain per-run CLI-state isolation as the implementation baseline for now, and reopen shared HOME as a credible candidate.** Different session postures are expressible, and the shared-store experiment below replaces the earlier unmeasured concurrency assertion with a bounded observation. Those findings remove the original categorical reasons for rejecting alternative C.

They do not yet establish a complete fail-closed replacement for every worker kind. Measured shell, file, background, and MCP tool denials held, but MCP initialization occurs outside tool admission; browser and interactive input paths remain unqualified; settings-only structural restrictions still need an explicit hook policy. The actual owner's interactive coexistence was unavailable. These are blockers to adopting shared HOME with hook-only posture, rather than evidence that the shared store corrupts data.

The choice follows the existing isolation convention while these boundaries are unresolved. Shared HOME would become the simpler provisioning choice if those acceptance conditions pass. The corrected credential study remains valid: it did not establish permanent sign-in loss, and neither HOME layout removes refresh-collision exposure. Retaining the baseline does not authorize replaying the historical keychain symlink recipe under the current safety restriction.

## Versions and method

| Specimen                  | Identity                                                                          | Role                                                       |
| ------------------------- | --------------------------------------------------------------------------------- | ---------------------------------------------------------- |
| Installed `agy`           | 1.5.7; SHA-256 `e872cc54b5381f401c798fc947937a0a930876b849bc3fee654996ba4ab236a8` | Real-home authentication preflight; left installed         |
| Current release specimen  | 1.5.9; SHA-256 `fad3628ad91fe8613a72f074a1c5fde7cd816517788a5b9be96703f7d256741a` | New measurements, from a scratch executable                |
| Prior hook evidence       | 1.1.12 and 1.5.5                                                                  | Inherited evidence, explicitly identified below            |
| Prior credential evidence | 1.5.5, 1.5.6, and 1.5.7                                                           | Inherited evidence, not repeated through a keychain bridge |

The official macOS arm64 [release manifest](https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/darwin_arm64.json) returned 1.5.9 at measurement time. The downloaded archive matched its SHA-512, `690cd6a5a5fdb23564fb1b14f7f6ae368fa3636b7630f56c574a6dfc0e2791a0e5b93d6515be1650c8a268cfb28440e1b9cbaa1c5b85aa762ab25b51c1fccda5`. `agy --version` returned `1.5.9`; `codesign --verify --strict` succeeded. Its signer was Google LLC, team `EQHXZ8M8AV`, identifier `cli`, CDHash `8f8248f85031d6d6f14a889922bd6f7ea2915c03`.

The exact [1.5.9 archive](https://storage.googleapis.com/antigravity-public/antigravity-cli/1.5.9-4515810895593472/darwin-arm/cli_mac_arm64.tar.gz) identifies the specimen independently of the moving manifest.

The [published changelog](https://antigravity.google/docs/changelog/) still called 1.5.8 latest when consulted. That release specifically fixed permission rules for eager MCP and Chrome DevTools tools. Release notes were a reason to remeasure both MCP paths, not evidence that hook interception works.

All model probes used `gemini-3.8-flash-low`, the real `HOME`, distinct `--new-project` conversations, session-only `--settings`, and private `--log-file` paths. They used noninteractive `-p` with closed stdin, not a real Boss dispatch or GhosttyKit pane. They do not count toward the acceptance ledger. The probe directories, logs, backups, and executable were ignored local apparatus; only this document is a tracked change.

### Credential and host safety

Real-home `agy models` exited 0 before model probes. Both executable versions could list models without sign-in. Supervisors ran under `caffeinate -dimsu`, retained their children, handled termination and interruption, and cleaned up before returning. The soak also monitored its parent, descendant process identities, CLI authentication diagnostics, and visible authorization-window metadata continuously. No detached tmux server, `nohup`, keychain accessor, token extraction, token copy, keychain unlock, ACL change, or dialog dismissal was used.

The current safety restriction forbids linking a fresh home to the real login keychain. The historical bridge study describes exactly such a symlink; its past measurements remain evidence, but that recipe was **not replayed**. No alternative credential mechanism was invented.

Before each attempt, the two trees the CLI would modify, `.gemini/config` and `.gemini/antigravity-cli`, were backed up, including existing project entries, databases, and the `cli.log` symlink. Backup admission rejected credential-shaped filenames. The unrelated top-level OAuth files were neither read nor copied. After all owned processes exited, generated entries were removed and originals restored; comparison checked every file's contents and mode and every symlink target. The pilot, verdict matrix, tool-surface matrix, and completed soak each returned `RESTORE_MANIFEST_EXACT True`. An independent post-soak comparison confirmed both trees matched, the originally absent `hooks.json` was absent again, and the running-`agy` inventory was empty. The installed executable's hash was unchanged.

The owner had no running `agy` process at the initial or pre-soak inventories. A synthetic owner-like process is not the owner's interactive session. Consequently, this execution cannot supply that particular coexistence acceptance result.

## 1. Concurrent sessions in the shared store

The soak used sixteen separate scratch workspaces and projects. Starts were staggered by 20 seconds with seeded ±6-second jitter, with a successful `agy models` preflight before each. Each session admitted only its own exact heartbeat command through the global hook, started it as a background tool, and waited for automatic completion. That command wrote a session-specific counter once per minute for 45 minutes and printed its completion canary.

This duration covers the token's logged natural expiry; no process is paused to synchronize refresh and no token is aged. The test exercises concurrent transcript, conversation, presence, summary-database, and workspace writes. It is a sustained tool workload, not sixteen idle processes, but it is not sixteen independent builds or an arbitrary production workload.

All sixteen loaded the same initially valid token expiry, **06:46:00.710084 UTC**. They subsequently logged sixteen natural refreshes between **06:45:53.587738 and 06:46:46.974211 UTC**, with a minimum observed gap of **351.883 ms**. None logged a failed token save or logout in that interval. This is a staggered natural-expiry result; it does not provoke the tens-of-milliseconds collision from the credential study.

| Completed-soak measurement  | Observed result                                                                                                                                 |
| --------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| Parent session window       | 06:08:54.939–06:59:25.439 UTC; all sixteen overlapped for **39 minutes 41.247 seconds**                                                         |
| Tool workload               | 16/16 final counters reached 44 after 45 writes; all sixteen transcripts recorded background completion, command exit 0, and the correct canary |
| CLI processes               | **15 exited 0; one exited 3** after a final-continuation TLS handshake timeout                                                                  |
| Natural refresh             | 16/16 refreshed; zero failed-save or logout diagnostics                                                                                         |
| Hook identity               | 16 unique parent conversation IDs, 16 pre-tool events, 16 post-tool events; 15 final `NO_TOOL_CALL` stops and one final `ERROR` stop            |
| Projects and transcripts    | 16 distinct project IDs and matching workspaces; 16 readable JSONL transcripts, with no other run's canary                                      |
| SQLite                      | All sixteen conversation databases and the shared summary database returned `ok` from read-only `PRAGMA integrity_check`                        |
| Lock/corruption diagnostics | No `SQLITE_BUSY`, `SQLITE_LOCKED`, database-locked, or corruption diagnostic in the sixteen logs                                                |
| Authorization UI            | None detected in at least 5,100 visible-window samples; no sign-in prompt                                                                       |
| Postflight                  | Real-home `agy models` exited 0                                                                                                                 |
| Cleanup                     | Zero owned processes remaining; exact restoration of both backed-up trees                                                                       |

Session 13 completed its heartbeat tool at 06:58:38 UTC, then failed its model continuation at 06:58:48 with `net/http: TLS handshake timeout` against `daily-cloudcode-pa.googleapis.com`. The same process logged TLS failures for user-info and telemetry requests; no other soak session logged that error. Its refresh had succeeded at 06:46:35, and its transcript retained the successful tool result. This is an observed network-path failure, not a clean sixteen-session success or evidence of database corruption. No causal link between HOME sharing and that timeout was established, and the experiment was not retried to replace the failure with a green result.

An earlier attempt was deliberately aborted after about nine minutes because its supervisor retained exited preflight PIDs as ancestry roots. Cleanup also initially compared parent IDs, so reparenting could invalidate ownership comparisons; it was corrected to use process start time. The first restoration encountered a still-changing generated directory and was repeated only after owned processes were confirmed gone, then matched the original file manifest exactly. No `agy` remained in that final inventory. That attempt contributes no duration, expiry, or concurrency acceptance evidence. The replacement supervisor excludes exited roots, tracks descendant birth identity across reparenting, and checks that its recorded processes are gone before restoring files. Its 45-minute duration was selected against the already logged expiry, leaving more than 30 minutes of sixteen-way overlap.

The logs are not error-free. All sixteen sessions, and the single-session pilot, emit anonymous cache/experiment/plugin warnings before silent authentication. Every such message preceded `ChainedAuth: authenticated via keyring` in the live audit; none was a sign-in prompt or a `Logging out` event. Other repeated startup diagnostics report an unavailable `/usr/bin/cs`, a relative Gemini-directory setting falling back to its default, and inapplicable admin controls. These are distinguished from database contention, credential-save failures, and errors after authentication rather than silently counted as a clean log.

### What the earlier credential results actually establish

The [corrected verdict](scoped-home-credential-delegation.md#verdict) does not disqualify the bridge. Staggered per-run sessions completed 16/16 with no authorization UI; staggered real-home sessions also succeeded. A failed keychain save after refreshes separated by tens of milliseconds led the vendor CLI to remove the keychain item until a later save recreated it. In the forced sixteen-way burst, that interval was an hour; staggered collisions recovered in seconds.

No failed save was observed in that study's real-home arm. That is not proof that sharing the real home prevents the race. Staggered refresh success does not establish behavior for sessions started together, repeated expiry cycles, offline startup, or sleep/wake. Both HOME options share the same sign-in dependency.

## 2. Permission enforcement and coverage

### Hook outcomes under allow-all mode

Every cell below used `--dangerously-skip-permissions`. An independent marker file established whether the action ran; CLI exit 0 alone was not treated as success. Each row is one completed session on 1.5.9, rather than a statistical reliability claim.

| `PreToolUse` behavior                             | Marker  | Observation                         |
| ------------------------------------------------- | ------- | ----------------------------------- |
| `decision: allow`                                 | Created | Tool ran; pre- and post-tool events |
| `decision: deny`                                  | Absent  | Explicit hook denial                |
| Unknown decision                                  | Absent  | Unsupported-decision error          |
| Hook exits 2                                      | Absent  | Hook execution error                |
| Invalid JSON                                      | Absent  | Hook JSON decoding error            |
| `{}`                                              | Absent  | Tool denied                         |
| Three-second handler with one-second hook timeout | Absent  | Hook command killed; tool denied    |
| Empty stdout, exit 0                              | Created | Tool ran; pre- and post-tool events |

Thus loaded hooks fail closed on the measured execution failures, but **silent success is permissive**. A managed authorization path must produce an explicit decision and deny unknown tools, malformed arguments, missing policy, and failed policy lookup. A progress-only forwarder may be silent; it cannot serve as the permission decision. The previous hook study also found that `ask` and `force_ask` do not impose approval under the skip flag, so the adapter must never emit them as a substitute for denial.

This matrix does not establish behavior when the hook configuration is missing, disabled, or no longer discovered. Hook liveness must be checked independently; successful earlier callbacks do not prove the chain remains installed. The existing [Codex chain-attestation implementation](../../engine/driver/src/codex/guard_chain.rs) is the reuse precedent for that distinction.

### Tool and feature coverage

| Surface                                                            | New observation on 1.5.9                                                           | Remaining boundary                                                                                         |
| ------------------------------------------------------------------ | ---------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| Shell `run_command`                                                | Allow and deny verified independently                                              | Inspect the whole command, including interpreters and redirection                                          |
| Native file creation, `write_file`                                 | Denied; marker absent                                                              | Canonicalize paths and protect state/configuration, not just the workspace                                 |
| Native file edit, `edit_file`                                      | Denied; original content unchanged                                                 | Older `replace_file_content` and `multi_replace_file_content` names were unavailable, not tested successes |
| Background command admission                                       | Allowed short sleep completed with a later post-tool event; denied sleep never ran | Admission is not ongoing inspection of subprocess actions                                                  |
| Lazy MCP, `call_mcp_tool`                                          | Allow reached the local server; deny did not                                       | Guard must decode server/tool/arguments                                                                    |
| Eager MCP, `mcp_home_study_marker`                                 | Same result as lazy MCP                                                            | A shell-only matcher would miss this name                                                                  |
| Native subagents                                                   | Earlier 1.5.5 study observed child hooks and distinct IDs                          | Inherit policy while filtering only lifecycle events; do not exempt child tool calls                       |
| Browser                                                            | Requested `browser_subagent` was unavailable; no browser action occurred           | Coverage remains unqualified                                                                               |
| `!`-prefixed headless input                                        | Became model `run_command`/`write_file` attempts, both denied                      | Not evidence about the interactive TUI's direct shell shortcut                                             |
| Terminal input, `manage_task send_input`                           | Not exercised                                                                      | Must deny stdin-driven command channels or inspect every submitted command                                 |
| Startup, discovery, MCP initialization, updater, transcript writes | Occur outside model tool admission                                                 | A tool hook is not an operating-system sandbox                                                             |

The local MCP server offered one harmless marker tool. Its independent request log contained exactly two `tools/call` requests: one from each allow control. Neither deny case reached the handler. Initialization and tool discovery still contacted the server before any model tool authorization. Sharing the owner's enabled servers therefore imports startup behavior even if every eventual MCP tool call is denied.

The [CLI browser instructions](https://antigravity.google/docs/browser/#connect-chrome-and-run-browser) require Chrome remote debugging and an approval prompt. This investigation did not turn on debugging or act on a prompt. Generic eager-MCP coverage does **not** establish the browser child's complete tool path. Browser-enabled hook-only permission enforcement remains an acceptance blocker until measured or structurally disabled for workers.

### Every worker kind needs an explicit policy

The source of truth is [execution-contracts' `WorkerKind`](../../engine/execution-contracts/src/worker_kind.rs), with the driver-facing mirror in [driver `lib.rs`](../../engine/driver/src/lib.rs). It currently contains six variants, including `ReviewGuide`.

| Kind        | Required managed-hook behavior                                                                                     |
| ----------- | ------------------------------------------------------------------------------------------------------------------ |
| Standard    | Permit authorized workspace work; retain structural state, command, publication, and launch guards                 |
| Reviewer    | Permit review reads; deny file edits, shell-mediated writes, VCS publication, and GitHub mutation                  |
| Triage      | Permit investigation and the designated automation decision/create operation; deny implementation and publication  |
| AnswerAgent | Default deny; permit the existing read capabilities and one thread-reply operation only                            |
| ReviewGuide | Preserve its restricted source-read/submission policy; it remains pinned to Codex, so do not silently enable `agy` |
| Progression | Default deny; permit designated reads/proposals; deny file writes, VCS/Cube commands, and GitHub mutation          |

The hook has enough input and environment to select different policies in one home. This is **expressibility**, not proof that all six policies have been implemented and tested. The study's marker policies are not the production guard chain. An exhaustive adapter must reuse the existing guards and capability tables, translate the current native tool vocabulary, and test positive and negative cases for every eligible kind.

The structural deny set includes Boss state access, `bossctl`, `sudo`, and destructive recursive removal. Existing [Grok permission rendering](../../engine/driver/src/grok/permissions.rs) explicitly notes that some restrictions live only in CLI settings and that a shell write can evade an edit-tool restriction. Removing the settings layer therefore requires moving those rules into the hook policy; wiring the existing progress forwarder alone is insufficient. Tests must use harmless scratch equivalents, never attempts against real Boss state.

Run identity must resolve to engine-owned policy; an agent-supplied worker-kind string is not authority. Completely unmanaged sessions take the owner fast path. Partially present, invalid, expired, or unresolvable managed context must deny, and the spawn path must verify the context before enabling allow-all. Nested CLI launches with erased run variables and attempts to rewrite/disable the global hook need explicit denial. Shared `HOME` is not isolation from a hostile process running as the same operating-system user.

## 3. Owner coexistence and hook latency

The prototype's first shell branch exits silently when `BOSS_RUN_ID` is absent, before payload parsing, Python startup, logging, or any transport. The managed branch records synthetic probe events only; it never contacts a Boss socket. Production installation must merge its named entry with the owner's hooks, preserve unrelated entries, and remove only that entry on uninstall.

A direct subprocess microbenchmark ran 1,000 calls per arm under load, with malformed stdin and all `BOSS_*` variables absent. Every call exited 0 with empty stdout and stderr. The baseline was the same shell executing `exit 0`.

| Arm                | Median   | p95      | p99      | Maximum   |
| ------------------ | -------- | -------- | -------- | --------- |
| Shell baseline     | 3.886 ms | 4.904 ms | 8.179 ms | 11.000 ms |
| Owner no-op branch | 3.933 ms | 4.661 ms | 5.929 ms | 10.720 ms |

The median difference was 0.046 ms, below the noise evident in the other quantiles. The added hook still costs a process launch: approximately four milliseconds per event on this host. That is small, not zero, and it is not an end-to-end interactive latency measurement. Installing six event handlers incurs the cost six times when all six events occur, and tool-heavy turns emit additional events.

Two additional real CLI controls, with all `BOSS_*` variables absent, ran while sixteen managed sessions were active in the subsequently aborted attempt. Each executed its marker command, returned `OWNER_DONE`, and exited 0; one prompt included 60,000 bytes of filler. Their conversation IDs were `22bb61b8-03a1-4859-b701-d02f4c22bd06` and `33cbd66b-9983-46e7-beb4-0497b233570f`. Neither ID appeared in the hook recorder. These remain valid short no-op measurements, but do not make the aborted attempt a completed soak.

The owner fast path's absence of logging or transport follows from its branch placement, empty outputs, and those controls; no production `boss-event` was invoked. Existing owner-hook composition and the owner's actual interactive workload still need qualification. A headless synthetic control is not that interactive session.

## 4. Shared settings, trust, onboarding, and project state

`agy --help` on the measured release describes `--settings` as session-only and never saved. The pilot and matrices supplied `trustedWorkspaces` there. The pilot's before-restoration diff excluded settings, and a live readback during the replacement soak matched the original settings SHA-256, `94b455cdc7de60f2412a489b5f3c2e9ec2b1e9f10dba27143ff2263754be0306`. Thus the original design's categorical claim that a Reviewer and Standard worker cannot have different settings in one HOME is no longer supportable.

| State                  | Shared-HOME treatment                                                                       | Per-run-HOME treatment                                                               |
| ---------------------- | ------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------ |
| Global hooks           | Install one named dispatcher once; validate it at every spawn and detect later disarming    | Materialize a private hook configuration                                             |
| Permission settings    | Session-only JSON and/or hook policy; never persist allow-all into the owner's settings     | Private settings plus guards                                                         |
| Workspace trust        | Pass exact workspace paths per session; no growing persistent trust list                    | Preseed the private trust list                                                       |
| Onboarding and consent | Preserve the owner's state; headless probes needed no new onboarding file                   | Fresh-home onboarding still needs an explicit product decision about consent         |
| Projects               | `--new-project` creates a new global project JSON; conversation/project identity is per run | Projects live in the private tree                                                    |
| Logs and transcripts   | Private `--log-file`; capture each conversation by its hook identity                        | Private CLI tree, still copied into durable storage                                  |
| Host tools and caches  | Inherit their normal HOME-based configuration                                               | Preserve host configuration and existing cache locations through shared provisioning |

One-time hook installation therefore does not mean zero per-run work. Workspace trust, managed identity, private logs, and project/conversation allocation remain per run. CLI initialization also writes built-in assets, caches, summary state, and presence files without a model tool call. Per-project separation does not make those resources private.

Worker instructions also remain per run. A shared global rules file would affect the owner's sessions and could not carry concurrent workers' different briefs. Keep those instructions in the existing initial-brief/control-directory flow outside the repository; qualify that flow with shared HOME rather than installing worker rules into the owner's global configuration. Likewise, enabled owner MCP servers and startup sidecars need an explicit worker selection policy: tool denials alone do not prevent their initialization.

The headless probes do not settle interactive first-run onboarding or prove that every permission setting has the same semantics as the skip flag. The [planned permission/trust characterization](../designs/antigravity-driver-fourth-driver-google-gemini-cli.md#permission-trust-and-soft-deny-characterisation) remains necessary in both outcomes.

## 5. Transcript and conversation identity

The pilot's `SessionStart` identified conversation `020a5a80-7343-40ca-8ad5-5f63ab703f7f` and supplied this concrete path shape:

```text
$HOME/.gemini/antigravity-cli/brain/<conversationId>/.system_generated/logs/transcript_full.jsonl
```

The allow/deny matrices likewise produced distinct IDs and readable transcripts copied before restoration. Prefer the actual `transcriptPath` payload over constructing a filename from a documentation example: the [hook reference](https://antigravity.google/docs/hooks) describes `transcript.jsonl`, while this binary emitted `transcript_full.jsonl`.

The engine should bind the first parent `SessionStart` to the run, validate the path against that identity and the expected store, persist the binding, and copy the transcript into the existing durable store before cleanup. Do not use “most recent conversation,” a workspace search, or a later child's `SessionStart` to recover identity. A global directory does not invalidate UUID-based lookup; missing startup delivery and child attribution remain the same correctness problems in both HOME layouts.

The concurrent readback matched all sixteen `SessionStart` IDs to distinct summary-database project IDs and the correct workspace, and all sixteen paths were copied before restoration. Fifteen transcripts contained six valid JSONL records; the TLS-failed session contained five, retaining its completed background-tool message but no final model answer. No transcript contained another run's canary. Preserve the `Stop.error` alongside the transcript: in that failed session, it supplied the continuation error absent from the final transcript record.

The sixteen background-completion messages explicitly reported command exit 0. That particular transcript observation does not establish reliable exit-status fields for every tool or change the driver's general `CommandOutcomeObservation` capability.

## 6. Updater and shared executable

All new probes exported the exact value `AGY_CLI_DISABLE_AUTO_UPDATE=true`. The pilot's CLI log recorded that automatic updates were disabled. The earlier study established that `=1` is ineffective and that simultaneous updaters can collide on shared staging paths even with per-home locks.

The variable controls the worker process; it does not stop the owner's session or a manual `agy update` from replacing the installed executable. Therefore **it is not sufficient to pin a shared installed binary**. Both HOME outcomes should launch a qualified Boss-owned executable path, keep the updater disabled, verify its identity at spawn, and arrange upgrades outside active worker startup. Do not run an updater race against the owner's installed copy.

All sixteen final soak logs recorded auto-update disabled. Both the installed executable and the scratch specimen retained the SHA-256 values listed above. This establishes absence of worker-driven replacement in this experiment; no concurrent owner updater was exercised. No update manager is implemented here.

## Comparison and implementation consequences

Selecting shared HOME would replace the design's current invariant that no mutable CLI artifact is shared and the owner's CLI tree is unreachable. The replacement must specify per-run conversation ownership, independently selected permission policy, protection of other runs' and the owner's data from model tools, and safe lifecycle management of shared configuration. Project IDs separate application state; they do not create filesystem confinement. Existing isolation assertions would need an intentional design revision before implementation.

| Decision factor                            | Shared real HOME with managed hooks                                                 | Per-run HOME with credential delegation                                                                             |
| ------------------------------------------ | ----------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| Different simultaneous permission postures | Expressible through environment-selected hooks and session settings                 | Private settings already separate them                                                                              |
| Shared sign-in                             | Direct use of the vendor's existing credential path                                 | Historical bridge works in staggered measurements; current safety restriction prevents replaying its symlink recipe |
| Refresh collision exposure                 | Not eliminated by sharing HOME                                                      | Not evidence of permanent credential destruction; synchronized refresh remains open                                 |
| Mutable state and owner customizations     | Shared; global-hook lifecycle and owner coexistence become product responsibilities | Private CLI state reduces cross-run and owner coupling                                                              |
| Host tool configuration and caches         | Normal locations naturally retained                                                 | Additional provisioning remains necessary; cache behavior must stay unchanged                                       |
| Permission completeness                    | Browser/interactive paths and the complete per-kind guard chain remain unqualified  | Still needs permission characterization; private state alone is not permission enforcement                          |
| Retention                                  | Must distinguish Boss conversations/projects from the owner's data                  | Can reclaim a verified Boss-owned home                                                                              |
| Binary drift                               | Disable worker updater and use a separate pin                                       | Same requirement                                                                                                    |

The following are changes to the [existing driver breakdown](../designs/antigravity-driver-fourth-driver-google-gemini-cli.md#proposed-implementation-task-breakdown), not code delivered here.

| Planned work                                                                        | If shared HOME is selected                                                                                                                                                            | If per-run HOME is retained                                                                                                                    |
| ----------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| Pane viability, hook-surface, and scoped-HOME investigations                        | Retain their observations; add actual owner coexistence and complete hook coverage; keep credential-refresh findings without a new bridge rollout                                     | Retain their observations and corrected credential verdict; resolve the permitted provisioning recipe                                          |
| Per-run HOME provisioning and credential delegation                                 | Remove home allocation, keychain bridge, and relocated host-tool configuration; replace with shared-config installation, session settings, project allocation, and spawn verification | Retain isolation and host-tool provisioning; reconcile the documented credential recipe with current safety restrictions before implementation |
| Per-run HOME retention sweep                                                        | Remove home deletion; replace with ownership-safe project/conversation cleanup and durable transcript retention                                                                       | Retain bounded private-home reclamation                                                                                                        |
| Global-root/control-directory hook wiring                                           | Change to one persistent dispatcher with a measured owner no-op, atomic merge/uninstall, and per-run context lookup                                                                   | Retain private wiring; preserve existing transport reuse                                                                                       |
| Permission config rendering per worker kind                                         | Replace shared-file rendering with session settings and exhaustive managed-hook policy; move settings-only structural denies into that policy                                         | Retain private rendering; investigate session settings as an implementation simplification                                                     |
| Hook payload adapter and guards                                                     | Expand qualification to every tool route, hook liveness, partial context, and owner-hook composition                                                                                  | Same dialect/deny correctness work; owner-hook composition is unnecessary                                                                      |
| Pane spawn and monitor                                                              | Preserve real HOME; pass project/session settings, private log, and managed identity                                                                                                  | Retain scoped HOME and delegation; pane delivery behavior unchanged                                                                            |
| Progress normalizer and identity filtering                                          | Preserve parent binding; validate paths in the shared store                                                                                                                           | Same binding and child filtering with a private root                                                                                           |
| Transcript access and durable store                                                 | Retain; add ownership checks before any shared-store cleanup                                                                                                                          | Retain; copy before home reclamation                                                                                                           |
| Permission/trust/soft-deny characterization                                         | Change the choice to include session settings plus hooks; require browser and owner coverage                                                                                          | Retain strict/plan/sandbox comparison and consent questions                                                                                    |
| Conformance suite                                                                   | Add shared-store concurrency, owner coexistence, and global-config lifecycle cases                                                                                                    | Retain private-state/delegation cases and refresh-start scheduling coverage                                                                    |
| Skeleton, model menu, registry, control verbs, turn boundaries, soft-deny detection | Retain                                                                                                                                                                                | Retain                                                                                                                                         |
| Four-way traffic split, acceptance ledger, submit-key fix                           | No change; already delivered                                                                                                                                                          | No change; already delivered                                                                                                                   |
| Conflict/CI eligibility and remote-host support                                     | Remain separate deferred work; HOME sharing supplies neither reliable command outcomes nor remote authentication                                                                      | Same                                                                                                                                           |

## Remaining acceptance work

- Exercise the actual owner's interactive session alongside the managed fleet without altering its configuration or permissions.
- Qualify the browser path and interactive command-entry surfaces, or prove they are unavailable to managed workers.
- Implement and verify the complete per-kind policy using existing guards; this study's synthetic marker policy is not a substitute.
- Test missing/disabled hooks, malformed/partial run identity, owner-hook composition, large payloads, and guarded attempts to change the enforcement configuration.
- Compare naturally synchronized starts and repeated expiry cycles without forcing credential state, and preserve the corrected credential conclusions.
- Resolve the permitted provisioning mechanism before repeating any scoped-home credential experiment; no keychain workaround is implied.

These are prerequisites for claiming a qualified shared-HOME driver, not permission to enable allocated traffic. The acceptance ledger continues to require real pinned Boss executions reaching merged PRs.
