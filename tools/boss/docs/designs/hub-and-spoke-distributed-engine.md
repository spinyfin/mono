# Hub-and-spoke engine: durable host observations without distributed work authority

- **Date:** 2026-10-21
- **Status:** proposed design; no implementation ships with this document
- **Provenance:** [merged investigation, PR #3232](https://github.com/spinyfin/mono/pull/3232), read in full from `main`; merge commit `a0121ee144b8f3659565f4c66d94f7a03ce19451`
- **Primary source:** [Hub-and-spoke investigation](../investigations/hub-and-spoke-distributed-engine-2026-10-21.md), particularly §§2.1–2.10 and the ordering in §3.2
- **Related contracts:** [worker liveness](../worker-liveness-contract.md), [worker proposals](worker-proposal-api-replace-fragile-worker-to-engine-seams.md), [host suspension](host-suspension-engine-side-sleep-awareness-and-clocks-that-do-not-count-suspended-time.md), [board measurements](../work-tree-scale-measurement.md)

**The central bet is that a worker host can durably finish assigned execution without acquiring authority to advance work.** A spoke records observations and performs already-authorized host operations; only the hub decides work status, dispatch, review and merge.

## Verdict

Adopt the investigation's outbound mTLS protobuf/gRPC spoke and laptop hub first. Ship remote capacity before local migration and full offline replay qualification; ship an always-on hub only after option B is complete and the genuine remote app path passes the investigation's performance gate. Keep summarisation on the hub, give managed remote hosts a dedicated worker user, and ship read-only pane snapshots over the stream.

## Goals

- Remove the laptop CPU ceiling by running real tmux workers on other hosts, reached through one persistent, spoke-initiated stream per host.
- Preserve one work authority while assigned workers continue through hub outages, including durable completion evidence, local cube heartbeats and eventual ordered reconciliation.
- Make host identity, command ambiguity, replay, credentials and mixed-version upgrades explicit contracts an implementer can test.
- Unify local execution with remote execution, then support an independently deployed hub for continued scheduling/review/merge while the laptop is absent.
- Address hub board scaling independently; the investigation's capacity estimates remain hypotheses, not promised limits.

## Non-goals

- Multiple active hubs, replicated work databases, consensus, autonomous spoke scheduling or cross-host live migration of a running worker.
- A generic remote shell, inbound spoke ports, automatic NAT traversal, relays, or SSH as a prerequisite for enrollment/control/upgrades.
- Replacing local app/CLI JSON IPC in option B, distributing the hub's model/GitHub credentials, or moving the summariser to spokes.
- Isolation between workers sharing one Unix user. Host/user containment limits damage but does not make sibling processes mutually untrusted.
- An interactive remote Ghostty terminal in this project. The required viewer is read-only; existing separately configured SSH remains an operator convenience, not a planned deliverable.

## Basis and verification boundary

The investigation is authoritative for the baseline and recommendation. References below to “§2.x” mean its sections; this design supplies missing contracts rather than repeating Part 1. No baseline correction is asserted. Source inspection of the existing adapter and proposal authentication identifies integration obligations, not a reason to change the recommendation.

| Input                                  | Decision supplied here                                                                          |
| -------------------------------------- | ----------------------------------------------------------------------------------------------- |
| §§2.1–2.2: ownership and wire sketch   | Crate boundaries, numbered schema, negotiation, authorization and adapter mapping               |
| §§2.3, 2.5: autonomy and failures      | Durable receipt versus application, command uncertainty, replay barriers, clocks and retention  |
| §2.4: capacity                         | Reservation accounting and an independent board work graph                                      |
| §§2.6–2.7: deployment and trust        | Enrollment transaction, certificate lifecycle, service user, provisioning contract and rollback |
| §§2.8–2.10: visibility and containment | Transcript custody, pane capture, prompt input and real-path acceptance                         |
| §3.2: phases                           | PR-sized dependencies in the final section                                                      |

The toolchain check is **validation of the selected protobuf approach**, not a comparison with JSON. On this checkout, `MODULE.bazel:8` pins `rules_rust` 0.70.0, and `MODULE.bazel:120` onward configures Rust 1.99.0 / edition 2024. `Cargo.toml`, `Cargo.lock` and `MODULE.bazel` declare neither prost nor tonic. Upstream supplies prost as the separate [rules_rust_prost 0.70.0 module](https://github.com/bazelbuild/rules_rust/blob/0.70.0/extensions/prost/MODULE.bazel), including a prost/tonic generator toolchain, not an automatically enabled part of `rules_rust`.

Actual Bazel invocations on 2026-10-21, with ordinary caches enabled:

- `bazel build //tools/boss/protocol:protocol`: exit 0, existing Rust protocol library built successfully with cache reuse.
- `bazel build @rules_rust_prost//private:protoc_wrapper`: exit 1, `No repository visible as '@rules_rust_prost' from main repository`; no compilation occurred.

**Prost generation and compilation with this repository's pinned toolchain remain unverified.** The run explicitly forbids prototypes and build-file changes and permits explicitly scoping an open question. Therefore the protocol PR owns adding the module/dependencies and the successful real Bazel generation, build, round-trip and gRPC tests before dependent implementation lands. The failed invocation above establishes a missing dependency declaration, not an incompatibility or a successful prost verification. No temporary prototype was made. This is the only build-evidence gap carried from the design into implementation.

## Alternatives considered

These retain the investigation's decision (§3.1); they do not reopen its baseline study.

| Approach                                                         | Checkable reason it is not chosen                                                                                                                                                                                  | Existing practice and why it remains valid there                                                                                                                                                                                |
| ---------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Persistent SSH plus a structured helper                          | Completing assigned work across disconnection requires a surviving supervisor, event journal and replay. At that point the helper duplicates the spoke while retaining SSH provisioning/reachability requirements. | The existing `SshHostAdapter` is a migration source; its missing controls and lost forwarded hooks are documented in §1.1. SSH remains useful for optional operator access, which does not claim offline completion guarantees. |
| Extend the current serde JSON wire to remote hosts               | It would require new version/capability rules and unknown-variant handling anyway. The selected schema has independently checkable field-number compatibility and generated peers.                                 | Local app IPC can continue using serde defaults because it is not being rolled out as an independently upgraded fleet protocol. Protobuf alone also does not prove semantic compatibility; peer tests remain required.          |
| Spoke-side summarisation with the restricted hub proxy from §2.1 | It adds config/result reconciliation and model-proxy authorization while still requiring the hub and sending the full transcript. It does not improve offline summary availability or the shared model budget.     | The existing hub summariser already owns policy, cadence and credential resolution; retaining it reuses those controls. No claim is made that host-local computation is inherently unsuitable.                                  |
| Move the hub before deploying resilient spokes                   | It exposes app latency, authentication and reconnect changes before removing the known worker CPU limit. A relocated hub still needs durable spoke replay.                                                         | Remote services are a sound eventual deployment; option A is explicitly in scope after B and a measured client gate, not rejected as a topology.                                                                                |

## Chosen approach

### Ownership, crates and load-bearing invariants

Use `tools/boss/protocol-spoke` for generated wire types and validation; `engine/spoke-journal` for durable host records; `engine/host-execution` for the extracted local cube/recovery and tmux/driver operations; `engine/spoke` for the daemon; and `engine/spoke-stream` for transport/session plumbing. Each distinct unit has its own Bazel target with only named consumers visible. None imports `engine/core`, `WorkDb`, or coordinator policy. `engine/core` supplies the authoritative stream consumer and `SpokeHostAdapter`.

Reuse the `tmux`, `driver`, `transcript-tail`, `recovery`, `log-files`, `backoff`, `command-runner`, `proposal-validation` and rustls infrastructure. Extract the local `CubeJsonTransport` implementation and its decoding into `host-execution`; do not create a second cube CLI implementation. The wire crate's conversions may depend on the lower-level `boss-protocol` event/proposal types, never on the engine. Package downloading uses the existing HTTP/retry primitives; a provisioning provider does not grow its own client/retry stack.

1. **One execution owner:** the hub persists `(execution_id, run_id, assignment_generation, host_id, spawn_token)` before any command leaves. It never assigns a second host while the first assignment might still execute. Operator abandonment requires a visible fencing decision; silence alone is insufficient.
2. **Durable before acknowledging:** receipt means the original bytes and deduplication key survived a committed transaction, not that a network write completed. A receipt is not command success, proposal acceptance or a work transition.
3. **Stable identity:** reconnects and daemon restarts preserve journal identity and sequence. OS reboot changes `boot_id`; reinstall with lost journal changes `journal_id` and requires explicit recovery. Pids and pane numbers alone never identify a worker.
4. **Replay-safe work effects:** receipt, projection and external effects have separate durable cursors/keys. Replaying an observation must not create a second completion, proposal, lease release or GitHub action.
5. **Honest absence:** network silence, expired certificates and timeouts do not prove process death. `Absent`, `Unknown`, and `Exited(exit_code)` are separate observations; only the hub's existing policy decides their work consequences.
6. **Restricted trust:** a host certificate authenticates one host's reports. It never authenticates an app user, authorizes arbitrary DB mutations, or proves a claim about another host.

### Wire schema

The following is the proposed `boss/spoke/v1/spoke.proto` schema, expressed here only as design text. All IDs are opaque UTF-8 strings except 32-byte digests. Missing message/oneof values are invalid unless explicitly optional; scalar zero/empty values must be validated rather than treated as usable identities. Numeric enum values are permanent. The additional data-plane schemas below are part of the same file.

```proto
syntax = "proto3";
package boss.spoke.v1;

service SpokeHub {
  rpc Connect(stream SpokeFrame) returns (stream HubFrame);
}
service Enrollment {
  rpc Enroll(EnrollRequest) returns (Certificate);
  rpc Renew(RenewRequest) returns (Certificate);
}
message Empty {}
message Cursor { string journal_id = 1; uint64 through = 2; }
message Stamp { string boot_id = 1; int64 wall_unix_ms = 2;
  uint64 awake_ns = 3; uint64 elapsed_ns = 4; }
message Feature { string name = 1; uint32 revision = 2; }
message Run { string execution_id = 1; string run_id = 2;
  uint64 assignment_generation = 3; string spawn_token = 4; }
message Hello {
  string host_id = 1; string journal_id = 2; string boot_id = 3;
  uint32 min_epoch = 4; uint32 max_epoch = 5;
  repeated Feature features = 6; string build = 7;
  Cursor received_commands = 8; uint64 first_retained = 9;
  uint64 last_record = 10; Inventory inventory = 11;
  bytes instance_nonce = 12;
}
message Welcome {
  uint32 epoch = 1; repeated Feature features = 2;
  string hub_id = 3; string authority_generation = 4;
  uint64 connection_generation = 5; Cursor received_records = 6;
  string command_journal_id = 7; uint32 max_frame_bytes = 8;
  uint32 max_inflight_bytes = 9; string build = 10;
}
message Reject { string code = 1; string diagnostic = 2;
  uint32 supported_min_epoch = 3; uint32 supported_max_epoch = 4; }
message Pulse { bytes nonce = 1; Stamp stamp = 2; }
message SpokeFrame { oneof body {
  Hello hello = 1; Record record = 2; Cursor command_receipt = 3;
  Pulse pulse_reply = 4;
} }
message HubFrame { uint64 connection_generation = 6; oneof body {
  Welcome welcome = 1; Reject reject = 2; Command command = 3;
  Cursor record_receipt = 4; Pulse pulse = 5;
} }
message Record {
  uint64 sequence = 1; Stamp observed = 2; Run run = 3;
  oneof body {
    CommandResult result = 10; WorkerEvent event = 11;
    PaneState pane = 12; TranscriptDelta transcript = 13;
    Capacity capacity = 14; Proposal proposal = 15;
    ArtifactChunk artifact = 16; InventorySnapshot inventory = 17;
    Gap gap = 18; PaneCapture capture = 19; WorkerRequest request = 20;
  }
}
message Command {
  uint64 sequence = 1; string command_id = 2; Run run = 3;
  int64 not_after_unix_ms = 4;
  oneof body {
    Spawn spawn = 10; Empty adopt = 11; WorkspaceOp workspace = 12;
    Deliver deliver = 13; Stop stop = 14; ReadArtifact read = 15;
    Capture capture = 16; Empty inventory = 17;
    ProposalDecision proposal_decision = 18; Upgrade upgrade = 19;
    WorkerReply worker_reply = 20;
  }
}
enum ResultCode { RESULT_UNSPECIFIED = 0; OK = 1; REJECTED = 2;
  ABSENT = 3; UNKNOWN = 4; OUTCOME_UNCERTAIN = 5; }
message CommandResult {
  string command_id = 1; ResultCode code = 2; string reason = 3;
  oneof value { PaneState pane = 10; WorkspaceResult workspace = 11;
    Empty completed = 12; }
}
message Inventory { string os = 1; string arch = 2;
  repeated string driver_ids = 3; string cube_version = 4;
  string tmux_version = 5; uint32 configured_slots = 6;
  repeated string failed_preflights = 7; }
message InventorySnapshot { repeated RunObservation runs = 1;
  uint64 through_record = 2; uint32 page = 3; bool last_page = 4; }
message RunObservation { Run run = 1; PaneState pane = 2; }
message Spawn {
  string lease_id = 1; string workspace_id = 2; string driver_id = 3;
  string model = 4; bytes prompt_utf8 = 5; bytes settings_json = 6;
  repeated Env env = 7; repeated Feature required_features = 8;
  uint32 cpu_millis = 9; uint64 memory_bytes = 10;
  bytes run_context_json = 11; string context_revision = 12;
  bytes worker_policy_json = 13; string policy_revision = 14;
}
message Env { string name = 1; string value = 2; }
enum WorkspaceKind {
  WORKSPACE_UNSPECIFIED = 0; ENSURE_REPO = 1; LEASE = 2;
  RELEASE = 3; HEARTBEAT = 4; FORCE_RELEASE = 5;
  CREATE_CHANGE = 6; GOTO_PR = 7; GOTO_REVISION = 8;
  STATUS = 9; LIST_WORKSPACES = 10; LIST_REPOS = 11;
  CREATE_BOOKMARK = 12; DIFF_BOOKMARK = 13; RESTORE_BOOKMARK = 14;
  DISCARD_BASELINE = 15; RESTORE_REBASED = 16; CHECKPOINT_BASELINE = 17;
  PREPARE_RECOVERY_PR = 18; RECONCILE_BOOKMARK = 19; RELEASE_HEADS = 20;
}
message WorkspaceOp {
  WorkspaceKind kind = 1; string repo_id = 2; string origin = 3;
  string workspace_id = 4; string lease_id = 5; string title = 6;
  string task_label = 7; bool allow_dirty = 8;
  repeated string exclude_workspace_ids = 9; uint64 ttl_seconds = 10;
  uint64 pr_number = 11; string revision = 12; string reason = 13;
  Bookmark bookmark = 14; optional Bookmark predecessor = 15;
  string inherited_base = 16; bool self_retry = 17;
  string pr_bookmark = 18; string trunk = 19;
}
message Bookmark { string repo_id = 1; string execution_id = 2;
  string head = 3; string recovery = 4; string base = 5;
  string anchor_commit = 6; }
message WorkspaceResult {
  string repo_id = 1; string workspace_id = 2; string lease_id = 3;
  string display_path = 4; string change_id = 5;
  bytes cube_result_json = 6; optional Bookmark bookmark = 7;
}
enum DeliveryKind { DELIVERY_UNSPECIFIED = 0; PROBE = 1;
  INTERRUPT = 2; NUDGE = 3; ANSWER = 4; }
message Deliver { DeliveryKind kind = 1; bytes input_utf8 = 2; }
message Stop { uint32 grace_ms = 1; }
enum PaneLife { PANE_UNSPECIFIED = 0; ALIVE = 1; EXITED = 2;
  MISSING = 3; INDETERMINATE = 4; }
message PaneState { PaneLife life = 1; string session = 2;
  string pane_id = 3; uint64 pid = 4; optional int32 exit_code = 5;
  uint64 activity_awake_ns = 6; string boot_id = 7; }
message Capacity { uint64 generation = 1; uint32 total_slots = 2;
  uint32 active_slots = 3; uint32 reserved_slots = 4;
  uint64 free_disk_bytes = 5; uint64 available_memory_bytes = 6;
  uint32 available_cpu_millis = 7; double load_one_minute = 8;
  uint64 journal_bytes = 9; Inventory inventory = 10;
  uint64 applied_command_through = 11; }
message WorkerEvent {
  string session_id = 1;
  oneof event {
    SessionStart session_start = 10; string user_prompt = 11;
    ToolUse pre_tool_use = 12; ToolUse post_tool_use = 13;
    TurnStop stop = 14; string notification = 15; string session_end = 16;
  }
}
enum StartSource { START_UNSPECIFIED = 0; STARTUP = 1; RESUME = 2;
  CLEAR = 3; COMPACT = 4; OTHER_START = 5; }
message SessionStart { StartSource source = 1; optional string model = 2; }
message ToolUse { string name = 1; bytes input_json = 2; bytes response_json = 3; }
enum StopReason { STOP_UNSPECIFIED = 0; COMPLETED = 1;
  AWAITING_INPUT = 2; INTERRUPTED = 3; OTHER_STOP = 4; }
message TurnStop { bool stop_hook_active = 1; StopReason reason = 2;
  string transcript_generation = 3; uint64 transcript_through = 4; }
message TranscriptDelta { string generation = 1; uint64 offset = 2;
  bytes data = 3; bytes sha256 = 4; bool eof = 5; }
enum ProposalKind { PROPOSAL_UNSPECIFIED = 0; PR_CREATED = 1;
  RUN_DONE = 2; WAIT = 3; QUESTION = 4; REVIEW_REPORT = 5;
  REVIEW_VERDICT = 6; OTHER_VALIDATED = 7; }
message Proposal { string proposal_id = 1; ProposalKind kind = 2;
  string payload_contract = 3; bytes canonical_json = 4;
  string transcript_generation = 5; uint64 transcript_through = 6; }
message ProposalDecision { string proposal_id = 1; ResultCode code = 2;
  string reason = 3; bytes result_json = 4; }
enum WorkerMethod { WORKER_METHOD_UNSPECIFIED = 0; RUN_CONTEXT = 1;
  PR_STATUS = 2; PR_BODY = 3; PROPOSAL_STATUS = 4; }
message WorkerRequest { string request_id = 1; WorkerMethod method = 2;
  string proposal_id = 3; }
message WorkerReply { string request_id = 1; ResultCode code = 2;
  bytes result_json = 3; string revision = 4; int64 observed_unix_ms = 5;
  string reason = 6; }
enum ArtifactKind { ARTIFACT_UNSPECIFIED = 0; REVIEW_RESULT = 1;
  WORKER_LOG = 2; SPOKE_LOG = 3; PR_URL = 4; ATTACHMENT = 5; }
message ReadArtifact { string artifact_id = 1; ArtifactKind kind = 2;
  uint64 offset = 3; uint32 max_bytes = 4; }
message ArtifactChunk { string artifact_id = 1; ArtifactKind kind = 2;
  uint64 offset = 3; bytes data = 4; bytes sha256 = 5;
  bool eof = 6; uint64 total_bytes = 7; }
message Gap { string resource = 1; uint64 from_offset = 2;
  uint64 through_offset = 3; string reason = 4; }
message Capture { string subscription_id = 1; uint32 interval_ms = 2;
  uint32 max_bytes = 3; uint32 ttl_seconds = 4; }
message PaneCapture { string subscription_id = 1; bytes text_utf8 = 2;
  bool truncated = 3; }
message Upgrade { string release_id = 1; string manifest_url = 2;
  bytes manifest_sha256 = 3; }
message EnrollRequest { string host_id = 1; string job_id = 2;
  string request_id = 3; bytes token = 4; bytes csr_der = 5; }
message RenewRequest { string request_id = 1; bytes csr_der = 2; }
message Certificate { bytes leaf_der = 1; repeated bytes chain_der = 2;
  int64 expires_unix_ms = 3; string host_id = 4; string serial = 5; }
```

**Payload semantics.** `Record.run` is required for run observations/results and absent for host capacity/logs/inventory. A result for a host-scoped command may omit it. `WorkspaceOp` is a discriminated typed operation, not an argv tunnel: its enum selects a validator defining required and forbidden fields. Paths received from a peer are display-only; workspace IDs resolve in the host's lease registry. The cube JSON result is a bounded, versioned compatibility payload decoded using the existing cube result structs, not arbitrary command output. `Bookmark` maps the recovery crate's durable identity; policy such as PR base selection remains hub-side, and only the authorized filesystem operations cross the wire.

`WorkerEvent` matches the seven variants in `protocol/src/worker_event.rs:18` without reducing tool values to strings; the JSON byte fields preserve plugin-specific objects. The hub still performs contextual Stop classification. File-ingress drivers reuse their existing parsers to emit the same observations. Unknown stop reasons do not prove completion. Proposals deliberately retain the existing canonical JSON validators under a negotiated `proposal-json/1` contract; `OTHER_VALIDATED` requires a specifically supported payload contract, never a generic engine method name. Structured output and attachments use registered artifact IDs and digest/size checks, not caller-supplied host paths.

`WorkerRequest` reads are bound to `Record.run`; the hub derives the work/PR from its assignment rather than accepting an arbitrary work ID or repository. Replies echo a stable request ID and the data's actual revision/time. The `worker-api/1` capability pins these JSON schemas to the existing client types and the run context/policy snapshot. Proposal submission uses Proposal, proposal decisions use ProposalDecision, and artifact registration uses ArtifactChunk metadata; there is no second mutation tunnel. For Spawn, the environment validator permits only driver-declared variables and hub-generated run identity. It rejects overrides of host state/socket/bin/home paths, regenerates host-specific settings from driver contracts, and does not transport provider or hub credentials.

### Negotiation, admission and schema evolution

TLS authenticates before application frames. First frame is `Hello`; the host ID must equal the certificate SAN and enrolled row. The hub checks journal continuity and selects epoch 1 and feature revisions once per connection. Required first-release features are `execution/1`, `journal/1`, `workspace/1`, `worker-event/1`, `worker-api/1`, `proposal-json/1`, `transcript-delta/1`, and `artifact/1`. `pane-capture/1` and `upgrade/1` are optional negotiated features; the first managed release must support both, but a mixed-version connection can retain execution without them. Scheduling inventory is separate from protocol capabilities.

No ordinary release increments the epoch. Reserve removed field names/numbers, never change a field's meaning or wire type, make additions optional with documented defaults, and give changed feature semantics a new revision. Unknown enum/oneof values yield a typed unsupported result or quarantine the record; they never map to success. A genuinely incompatible base ordering/authentication contract requires a new epoch. The stable Hello/Reject exchange remains readable so incompatible peers receive an upgrade reason rather than silence.

Retain raw encoded journal bytes; prost is not assumed to preserve unknown fields when re-encoding. Store the selected epoch/features alongside each record batch. An upgrade must retain decoders for outstanding older records until their queues drain. Dispatch is blocked when required capabilities or the security minimum are missing; fingerprint inequality alone is informational.

Bazel produces generated Rust and a descriptor set from declared proto inputs with pinned protoc/plugins in the execution toolchain, never ambient `protoc` or a Cargo build script that downloads tools. Start with `rules_rust_prost` 0.70.0 and its matching generator/runtime versions; pin any necessary override as a coherent set after its real build proves compatibility. A descriptor compatibility test under `bazel test` runs a pinned `buf breaking --against` baseline using FILE-level compatibility plus repository enum/default checks. CI supplies the merge-base descriptor as an input artifact, not a mutable network lookup inside a cached action. Versioned old/new peer fixtures exercise defaults, unknown fields/enums, optional feature absence, rejection and mTLS duplex streaming. A schema check cannot replace those semantic tests.

Default limits are 1 MiB per decoded frame, 64 KiB transcript/artifact chunks, 512 KiB prompt/settings combined, 256 KiB hook/proposal payloads, and 256 KiB bulk bytes outstanding per host. Limits are negotiated downward, never upward beyond local policy. Oversized payloads use a declared artifact reference where supported or fail explicitly before spawn/receipt; no silent truncation of completion evidence. The hub caps concurrent authenticated hosts and per-host durable ingress bytes. Connections have bounded read deadlines and no protobuf compression in epoch 1.

### Journal, acknowledgement and crash recovery

Use a private SQLite WAL database (`journal.db`, `synchronous=FULL`) in the spoke state directory, reusing rusqlite. It stores host/journal identity, next sequence, raw outbound records, command inbox/results, execution/lease identities, artifact cursors, producer deduplication keys and migration version. A single daemon lock prevents concurrent writers/supervisors. The hub uses its existing WorkDb transaction boundary for the command outbox, durable ingress and application cursor; transport owns no second work database.

| Durable table / key                                             | Meaning and retention                                                                                                                                |
| --------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| Spoke `records(journal_id, sequence)`                           | Immutable encoded observation plus digest; assigned in commit order before send. Receipt permits compaction subject to the retained recovery window. |
| Spoke `commands(command_journal_id, sequence)`                  | Command ID, request hash, phase and result. A repeated ID with different bytes is rejected. Terminal tombstones remain while the assignment exists.  |
| Spoke `executions(run_id, spawn_token)`                         | Assignment generation, lease/workspace, deterministic session identity, bootstrap phase and terminal evidence; survives record compaction.           |
| Spoke `sources(run_id, producer_id, producer_sequence)`         | Deduplicates shim retries and file-ingress observations independently of network replay.                                                             |
| Hub `spoke_ingress(host_id, journal_id, sequence)`              | Original bytes, selected contract, observed time and processing disposition; contiguous durable receipt is advanced in the same transaction.         |
| Hub `host_commands(host_id, command_id)`                        | Durable request and per-host sequence; receipt and final result tracked independently.                                                               |
| Hub `spoke_effects(host_id, journal_id, sequence, effect_kind)` | Projection/outbox idempotency keys; external effects use the existing durable proposal/completion mechanisms.                                        |

There is one monotonically increasing observation sequence per journal, including capacity and pane captures. Hello, receipts and nonce challenges are transport controls and are not journaled; acknowledging an acknowledgement would otherwise recurse. This resolves the sketch's “every message” wording without changing durable observation ordering. Commands have their own hub-issued per-host sequence and journal identity. Sequence never derives from wall time and never resets on reconnect or reboot.

For hub receipt: validate framing, host/run ownership, contract and bounds; insert bytes and advance the highest contiguous receipt in one transaction; then send `record_receipt`. Project records asynchronously in order through the existing event/proposal/completion paths. The projection transaction records each effect or a durable work outbox entry before advancing its application cursor. A completion awaiting later transcript chunks becomes a pending outbox item; it must not hold the cursor or stop independent transcript materialization, which would deadlock on its own watermark. A crash after receipt but before application therefore replays locally. Network side effects are retried by their stable operation keys, not by rerunning an unrecorded event handler.

Transcript/artifact files are derived from durable ingress. Write chunks at their exact offsets, compare the digest on overlap, fsync before recording the materialization watermark, and replay after a crash between file and cursor commits. Never append the same bytes twice. An unexpected gap/changed overlap stops that resource's projection and raises an attention; it does not fabricate a complete transcript. Stop and completion proposals carry transcript watermarks so their consumers wait for the required bytes/structured output or record an explicit unavailable-evidence outcome. Receipt itself never waits for a model call, filesystem projection or GitHub.

For command receipt: commit the command inbox first, acknowledge the contiguous inbox prefix, execute, then journal the result. A caller succeeds only on a matching result, not on receipt. Command timeouts return `OutcomeUncertain` when receipt/execution may have happened. Keep the command ID on retry/query; do not create a new Spawn or automatically fail over hosts. Expired commands are durably rejected in sequence so one stale command cannot block the queue forever.

**Non-transactional host effects require operation-specific recovery:**

- Spawn records intent, then a launching phase before invoking tmux. Session name derives from the spawn token; set token/assignment environment and `remain-on-exit` before executing the driver. On recovery adopt exactly that session and token. If launch may have happened and the session is now absent, return uncertain/absent evidence and require hub reconciliation; never blindly spawn again.
- Lease acquisition is serialized by assignment. Reconcile an interrupted acquisition against cube's recorded lease owner before retrying. Release and heartbeat address a particular lease; bookmark operations compare expected identities and use the existing recovery helpers. No arbitrary shell retry is permitted.
- Deliver can crash between injecting bytes and recording success. Mark it attempting before injection; after such a crash report `OUTCOME_UNCERTAIN`, never resend input automatically. Interrupt has the same ambiguity unless positive pane/turn evidence settles it.
- Stop is repeatable only against the same token/boot/session identity. Observed exit or absence settles it; a recycled pid is never signalled. Grace is capped at 30 seconds and uses awake time.

On reconnect, the hub atomically claims a new connection generation for that host and fences the previous reader/writer. The spoke accepts commands only on the selected stream/generation. Generation belongs to HubFrame, outside the immutable Command bytes/hash, so replay can retain command identity while using a new stream. A second live daemon with a different instance nonce is refused while the current stream answers a fresh nonce challenge; a broken stream must close or time out before replacement. This limits accidental duplicate installations; it cannot make a malicious holder of a copied private key trustworthy.

Welcome advertises the **hub's durable receipt**, not the spoke's remembered sent watermark. The spoke replays from the next sequence, the hub deduplicates overlaps, and the hub resends unreceipted commands with unchanged IDs. A paged inventory is journaled after the replay cutoff. Existing runs are reconciled against both that inventory and all earlier events before enabling new dispatch. Capacity from backlog never marks a host reachable: a current-stream nonce reply plus a fresh post-replay capacity report is required.

Keep receipted records for at least seven days and while their assignment remains unreconciled; keep command/run tombstones until hub finalization and the recovery retention window both pass. If a restored hub asks below the first retained sequence, or either journal ID unexpectedly changes, quarantine dispatch and surface `replay_gap`; never reset the cursor upward automatically. Restore changes `authority_generation` and blocks new commands pending explicit host reconciliation. Signed upgrade rollback must not roll back the work DB or journal identity. Missing history is a recovery incident, not a successful replay.

Use an 8 GiB default spoke spool budget with a separately reserved 64 MiB lifecycle allowance. At 75% stop new dispatch admission and pane captures; at 90% stop admitting transcript bytes, retaining source offsets/files for later streaming. Assign sequence numbers to bulk chunks only as send credit becomes available, so a transcript backlog does not preallocate millions of records ahead of lifecycle evidence. Control records have priority before sequence assignment; already numbered records retain order. Use backpressure and source retention, never drop unacknowledged lifecycle records. If disk/fsync fails, reject local receipt so the shim keeps its pending event, mark the daemon unhealthy and preserve active panes. An exhausted host cannot promise indefinite lossless offline work; it exposes the failure and any irrecoverable source gap explicitly.

### Worker ingress and offline behavior

The spoke exposes owner-only local hook and worker-RPC Unix sockets. Authenticate the peer with the platform's local peer credentials and ancestry against the spoke-owned run registry; `_boss_run_id` is a consistency check, never the credential. Reuse the current hook normalizer, proposal validators and file-ingress parsers. Give the shim pending buffer stable producer IDs/sequences and scan registered pending buffers on spoke startup, so recovery does not require another hook to arrive. Persist a cursor in the same transaction as the normalized observation; deduplicate overlapping hook/file feeds using the driver's existing source rules.

The worker RPC gateway exposes a versioned allowlist: bound run context, PR status/body cache, proposal submission/status, and artifact/attachment registration. It does not expose the general app API or arbitrary `bossctl` DB access. Online requests route over the same stream using run-scoped contracts; context/config needed for execution is included in the provisioned run snapshot, and cached reads return their observed revision/time. A cache miss while offline is `hub_unavailable`, never invented state. Any additional worker API used by composed prompts must be inventoried and supported before enabling that execution kind remotely.

Run the existing stateless worker-policy/tool-guard logic at the spoke against the hub-provided policy revision. Offline execution retains the last assigned restrictions; it cannot interpret lost hub connectivity as permission. An operation requiring a fresh hub/human authorization parks or returns unavailable. Hook policy responses are computed locally before the tool proceeds; journaling the associated observation does not itself authorize the tool. Reuse/extract the existing policy implementation rather than creating weaker remote rules.

For `propose done`, a durable local response is **queued for hub validation**, distinct from the hub's accepted/rejected result. The worker may finish its turn and park after the receipt; the spoke preserves the pane, workspace and completion evidence. It does not finalize/reap/release merely because it queued a declaration. On reconnect the existing hub validation/finalization path runs, returns a `ProposalDecision`, and authorizes Stop/Release as appropriate. A rejected declaration can receive a hub-delivered continuation. PR creation still uses the host user's GitHub credential and the existing cube publication path; PR URL files/proposals become durable artifacts/events. Questions wait for the hub/operator; no local answers or next-task dispatch are invented.

The spoke heartbeats leases for active or parked assigned runs locally even without a hub. It reports failures and retains workspace identity; the hub keeps reap policy. Spoke restart, hub restart, host sleep and certificate expiry do not authorize local lease release. An unreachable hub therefore permits completion of already assigned work, not pipeline progress (§2.3).

### Hub stream manager and adapter contract

The manager owns authenticated connection state, per-host generations, bounded queues, command futures, receipt negotiation and replay admission. It injects received records into a hub-owned `SpokeObservationSink`; that sink enforces the WorkDb ownership transaction and returns a durable receipt. No socket task holds a DB lock while awaiting network I/O. Transport failure rejects pending waits as unavailable/uncertain but retains durable commands.

`SpokeHostAdapter` is a cheap handle `(host_id, stream_manager, hub policy services)`. Extend the existing trait rather than bypassing it in new callers. Preserve these contracts:

| Existing surface                                                              | Spoke mapping and result                                                                                                                                                                |
| ----------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ensure_repo`, lease/release/heartbeat/force-release, change/goto/status/list | Validated `WorkspaceOp`; existing typed cube results. Heartbeats already delegated to the spoke become policy/status queries, never a competing timer.                                  |
| Recovery/bookmark methods in `host_adapter.rs:153–248`                        | Typed workspace operations using the extracted `recovery` executor; hub retains GitHub/base-selection policy. No `Jj` implementation that accepts arbitrary remote argv.                |
| `spawn_worker`                                                                | Hub composes prompt/config, reserves assignment and sends Spawn. `OK` with matching PaneState maps to the existing alive outcome; unknown remains unresolved, not failed-and-retryable. |
| Probe, interrupt, nudge, answer and stop                                      | Deliver/Stop against Run identity; distinguish received, delivered, turn-ended and uncertain in the existing control result model.                                                      |
| Reattachment and liveness                                                     | Inventory + Adopt/PaneState; retire event forwarding and remote pid probes. Missing exit evidence is retained as unknown/absent for policy.                                             |
| Transcript and worker log readback                                            | Read the hub materialization by run/generation; request bounded registered artifacts when needed. A remote display path is never opened on the hub.                                     |
| Structured outputs                                                            | Chunk/digest-checked registered artifacts with existing validation; preserve `NotAvailable` versus positively invalid output.                                                           |

The first remote rollout has explicit host transport state (`legacy_ssh`, `spoke_pending`, `spoke_ready`). Never switch a live assignment between adapters. Stop assigning new SSH runs once spoke readiness is proven, let existing legacy runs finish under their recorded transport, and remove `ssh_spawn`, `remote_reattach`, `remote_lease_reconcile`, `remote_transcript`, wrapper distribution and `SshHostAdapter` only after the migration acceptance establishes no remaining callers/legacy runs. Tests must enforce that no worker operation silently falls back to SSH.

### Enrollment, service user and credential lifecycle

Use a hub-specific CA and host-scoped client certificates with SAN `urn:boss:host:<opaque-host-id>`, clientAuth only, and a 30-day lifetime. The hub endpoint presents a server certificate rooted in provisioned trust; verify hostname and hub identity. The CA key stays on the hub in its protected state/secret facility and never enters a spoke package, worker environment or transcript. App-user credentials in option A use a separate trust audience; a spoke cert cannot authenticate that endpoint.

Enrollment is a server-authenticated TLS RPC with the single-use secret, rate-limited separately from Connect; renewal additionally requires valid host mTLS. A pending host is not schedulable. The hub generates a 256-bit token, stores only its hash with host/job/expiry (15 minutes), and receives a locally generated CSR. One transaction consumes the token and binds `(host_id, job_id, request_id, csr_digest)` to the issued certificate. Retries with exactly that tuple return the stored certificate even after token consumption within the job's bounded recovery window; different keys or revoked jobs fail. Responses do not imply host readiness.

Renew at ten days remaining with jitter. Generate a replacement key locally, persist the request before sending, install the certificate/key atomically and reconnect; accept the previous certificate for at most 24 hours, capped by its expiry. Existing streams are closed at certificate expiry/revocation, not left authenticated indefinitely. Renewal, disable, revoke and remove are audited hub operations. Disable blocks new assignments but retains control/observations. Revoke closes streams and rejects renewal/re-enrollment until explicit new host authorization; it cannot kill an offline worker. Expired identities recover through the same managed provisioning job, preserving host journal and pending evidence when available. Rotate CA trust by an authenticated overlap bundle before replacing the issuer; emergency compromise requires managed reprovisioning rather than trusting a message from the compromised identity.

**Worker-user decision:** managed non-laptop hosts run the service, private tmux server, cube and drivers as a dedicated unprivileged `boss-worker` user, with its own home, jj store, Bazel caches and driver/GitHub credentials. Root installs packages and the service definition only. The hub does not copy the operator's credentials. The local spoke runs as the logged-in operator to preserve current keychain/driver access and the existing cube store; this is a deliberate containment difference, not equivalent security.

The provisioning preflight must run as the eventual service user using the installed service environment, not an administrator's interactive shell. It verifies tool versions, authenticated driver/GitHub operations without printing secrets, repo access, writable private storage, disk/capacity, hub reachability and clock/certificate validity. Credentials are provisioned through the fleet's secret mechanism or managed login policy. A missing credential leaves the host visibly unschedulable; enrollment alone cannot turn it ready. Same-user workers can read same-user secrets; dedicated-user containment protects the operator account, not each sibling.

### Managed provisioning and upgrades

The investigation names MDM and cloud-init but records no actual provider/API or existing fleet integration. That decision is absent, not assumed to have been made elsewhere. Choose a provider adapter boundary and require a short repository-backed provider selection/discovery task before platform adapters; it must identify real authenticated secret delivery and machine identity, not merely choose a vendor name. This does not block protocol, supervisor or hub work.

The hub provisioning job contract is `CreateHost(platform, machine_selector, release, worker_user) -> job_id`, `DeliverSecret(job_id, expiry, secret)`, `Inspect(job_id) -> staged result`, `Retry(job_id)`, and `Cancel(job_id)`. Each uses a stable idempotency key and provider machine identity bound to the pending host. Jobs persist transitions `requested → package_verified → installed → enrolled → connected → preflight_passed → ready`, with typed stage errors. A retry resumes its stage and cannot authorize a different machine/key. MDM jobs execute the macOS installer; a cloud API creates/selects a Linux instance and cloud-init invokes the bootstrap. Initial provisioning has no requirement for the spoke to be running.

The authenticated management channel delivers hub trust, the release verification key, host/job ID and desired version. A separate provider secret facility delivers the token into an owner-only temporary file only when the job is ready; it never appears in argv, logs or persistent user-data. Cloud-init/MDM payloads contain secret references and public trust, not enrollment secrets. The installer fetches an HTTPS manifest/package, verifies the signature and digest, checks OS/architecture/version, installs owner-only state and the launchd/systemd service, enrolls as the worker user, deletes the credential file, then starts the daemon. No manual SSH/download/enroll step is accepted as the finished flow. Provider integration credentials remain hub-side and are restricted to the managed fleet.

Publish signed macOS arm64 and Linux amd64/arm64 packages through the repo's release infrastructure. Reuse `tools/release` for assets/checksum sidecars and the existing macOS codesign path; the sidecar is not an authenticity signature. Add an Ed25519 detached signature over the exact UTF-8 manifest bytes, verified before parsing, under a release key separate from the host CA. The manifest identifies release ID, platform, digest, URL, minimum management/execution contracts and journal-reader versions. An offered URL must be HTTPS on an approved package origin. Verify before execution; no unsigned fallback. Enrollment/upgrade management remains available even when execution epochs do not overlap.

Upgrade stages a verified binary next to the active one, records a rollback marker, stops only the daemon, atomically switches and restarts under its service manager. Tmux workers, their environment and journals remain intact. Health requires lock acquisition, journal readability, local inventory and a successful compatible management handshake; use a two-minute startup deadline. Keep the previous binary until health passes. Journal migrations are expand/contract with an explicit oldest readable version; forbid automatic binary rollback if the old binary cannot read the new journal. Managed provisioning is the recovery path when neither binary starts or the management contract is incompatible. Security-minimum upgrades block new work while retaining active observations; they do not silently kill workers.

### Local spoke migration

Keep the persisted host identity `local`; it becomes the host on the original laptop even after the hub moves. Add immutable machine identity separately and stop interpreting the string `local` as “same filesystem as this hub.” An always-on hub's own worker host, if any, gets its own host ID.

For option B, bundle the spoke with the app and let the engine supervise a child that dials the hub's loopback mTLS endpoint. Bootstrap its owner-only host credential through a private inherited descriptor, not argv or a weaker unauthenticated protocol. The same schema, negotiation and adapter run locally. On parent exit the child may exit cleanly, but it never kills tmux or releases live leases; `boss-event` buffers until a restarted spoke drains it. Managed remote services remain independently supervised.

Migrate only while new dispatch is paused: the hub inventories its existing local sessions, persists assignment/token/lease mappings, starts the spoke against the existing private tmux socket and transfers observation ownership. The spoke validates each token before adoption and reports an inventory barrier. Only after that barrier does the hub stop its old observers and route new operations through the adapter. A durable migration generation makes a crash between these steps retryable without two supervisors. Legacy sessions missing trustworthy identity remain on their old path until drained; never guess from a pid.

Remove direct worker tmux/pid sweeps, local lease heartbeat execution, the local unlimited-slot exception and panic/shutdown worker-kill paths after adoption tests pass. The hub may still host its coordinator session, which is explicitly distinct from worker execution. Preserve local pane viewing through an explicit same-machine capability and the spoke-owned socket; remote viewer choice never follows the literal host ID.

### Capacity, clocks and observability

Persist host enablement separately from transport state (`connecting`, `replaying`, `ready`, `unreachable`, `incompatible`, `revoked`). A nonce challenge every five seconds and three unanswered challenges marks unreachable for dispatch; buffered Capacity cannot satisfy a challenge. After six hours of hub-observed unreachability raise deduplicated per-run attention without orphaning. Host removal requires the existing explicit recovery decision. Keep observations and control enabled when only dispatch is disabled.

Capacity reports carry a monotonically increasing generation, OS resource observations and the hub-command prefix incorporated into reservations. The hub computes available budget from the newest report minus reservations issued after that prefix, also capped by configured pool and global workload budgets. The spoke atomically reserves again at Spawn admission and rejects stale overcommit. CPU/memory/disk thresholds and driver availability are host policy; unknown or stale capacity admits no new runs. Load is advisory, free disk is a hard admission guard. Reconciliation releases a reservation once, by assignment identity, not by a guessed slot count.

Every durable observation has wall time for display/audit and a same-boot pair of awake/elapsed counters. On macOS use the clock-pair contract in the host-suspension design; on Linux use the corresponding suspend-excluding/inclusive monotonic clocks. Persist sampled ledger intervals and reuse the suspension crate if it has landed; otherwise implement the same small host-clock contract without importing hub policy. Never subtract counters across boot IDs or extrapolate a spoke's awake time from hub wall time while disconnected. Pause stale/spawn-ack/backstop judgments until replay and fresh inventory finish; unknown clock intervals remain unknown rather than counting as inactivity. Wall-clock skew affects certificate validity and needs a visible preflight/health error, not disabled TLS time checking.

Logs reuse `log-files` rotation and are bounded stream requests. Audit joins use `(host_id, journal_id, sequence, command_id, run_id)`; payload text/secrets are excluded from transport logs. Expose receive/apply/materialize watermarks, journal bytes/oldest unacked age, command uncertainty, replay gaps, reconnect count, credential expiry, capacity age and projection lag. Hub transcript copies feed the existing redaction/summariser and cost readers; offline summaries remain stale, and replay schedules one summary of the latest window rather than a model call per historical event.

Pane capture is a five-second-default, 64 KiB text snapshot with a renewable 30-second subscription TTL, only while a visible viewer subscribes. It uses the existing app subscription path via the hub; it expires on disconnect and is lowest priority under backlog. Show host, observation time, truncation and stale/unreachable state, render escaped text rather than terminal escape actions, and provide existing explicit probe/interrupt/stop controls. It is not a terminal byte protocol and grants no arbitrary keystroke API.

### Rollout and gates

Every new transport/integration initially ships inert until the hub listener/trust, signed release and managed provisioning provider are configured by the fleet administrator and an enrolled host passes preflight. PRs must name their particular enabling action. A readiness flag is not a substitute for the remote acceptance below.

| Milestone                    | Required evidence before enabling it                                                                                                                                                                                                          | What failure blocks                                                                                                |
| ---------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| Remote capacity              | Real installed spoke + isolated hub + real cube/tmux + one supported driver complete a task and publish a PR; controls, transcripts, proposals and signed provisioning work without SSH. Include hub/spoke restart and duplicate-spawn cases. | New spoke production dispatch, not independent protocol/package development.                                       |
| Local migration and capacity | Adoption/crash barrier and reservation tests with actual tmux/cube; one execution owner and no local bypass.                                                                                                                                  | Migration/automatic capacity rollout.                                                                              |
| Option B complete            | Genuine end-to-end fault campaign across disconnect, sleep, restart, reboot, replay, queued done and artifact recovery; repo test/report artifacts.                                                                                           | All option A implementation tasks. Already shipped remote capacity remains usable.                                 |
| Option A client gate         | Genuine authenticated app/hub/spoke path at the §2.3 network matrix and thresholds; no duplicate writes or unbounded queues.                                                                                                                  | Always-on hub deployment/promotion. It cannot retroactively gate building the candidate app transport it measures. |

The B campaign covers every record kind and command phase, lost receipts, duplicated deliveries, disk pressure, certificate expiration/revocation, journal rollback/gaps, file rotation, stopped shim with no later hook, and hub crash between receipt/projection/external effect. Unit fault injection proves state-machine properties; real tmux, cube, sockets and the actual completion path prove integration. Fixtures run only through `bazel test` under the repository test sandbox, with isolated state roots and no production engine access.

For A, first add an independently authenticated app endpoint, explicit endpoint selection/autostart rules, revisioned cached snapshots and idempotency keys for mutating requests. A persistent hub dedup table returns the same result for a repeated user/request key; an action that might have reached an older server is shown uncertain, not replayed blindly. Keep reads bounded, coalesce replaceable runtime updates and prioritize command acknowledgements over transcript/pane bytes. The CLI shares the remote client contract; the coordinator lives on the hub and has a separately authorized read/control surface, never a spoke identity.

The **validation**, not topology comparison, then records opening board/details, switching products, answering questions and stopping workers at 8/32/64 workers, 20/50/150 ms RTT, 10/100 Mbit/s, packet loss and reconnect. At 50 ms / 10 Mbit/s require cached navigation p95 ≤100 ms, ordinary mutation acknowledgement p95 ≤500 ms, and brief-disconnect board recovery ≤2 seconds, with bounded queues and no duplicate mutations (§2.3). Commit instrumentation, the reproducible runner and measured report. A failing result identifies defects in the tested path and blocks hub relocation; it is not permission to relax thresholds or a claim that a hand-built mock passed the app.

An independently packaged hub then owns state/backup/updates/coordinator on the always-on host. Migration takes a quiesced consistent DB snapshot and stops the old authority before the new hub accepts commands, preserving hub identity/cursors and a recorded authority generation. Rollback never runs both hubs: stop the new service and reconcile any newer commands before reopening the old one. The app must not autostart a local replacement when remote connection fails. B remains supported if A's gate fails.

Board scale follows §2.4 independently: measure invalidation sources first, cache per-item runtimes and split runtime versus structural invalidations, then maintain a hub board model with complete-projection parity checks for moves/deletes/related cards. Batch durable hook receipts without acknowledging before commit, and bound parallel dispatch while retaining atomic reservations. No worker-count ceiling is declared from the investigation's estimates.

## Risks / open questions

- **Toolchain gate:** prost/prost-build generation is not proven by the existing library build. The protocol task must land successful Bazel/CI evidence before consumers; fixing this requires implementation files prohibited in this design run.
- **Provisioning discovery:** no provider identity or secret-delivery contract is recorded in the source. The dedicated discovery entry must produce a concrete managed macOS and Linux path with documented API/identity constraints. An untrusted bootstrap URL or manual shell install cannot satisfy that task.
- **Bounded autonomy:** disk exhaustion, lost source files and revoked/expired identity can prevent delivery. Preserve evidence and expose gaps; do not describe bounded journals as an unconditional no-loss guarantee.
- **Non-transactional effects:** tmux input and some cube operations cannot share the journal transaction. Conservative uncertainty may need operator reconciliation; it is preferable to duplicate input or execution. This is a defined outcome, not a promised exactly-once effect.
- **Authorization parity:** the worker gateway is a substantial migration because existing proposals authenticate local peers (`engine/core/src/app/proposals.rs:198`). The real acceptance must exercise the composed worker prompt's commands, including proposals and output capture, before remote dispatch is enabled.
- **Same-user containment:** a managed worker can compromise sibling workers and the spoke identity under that user. This design does not claim per-worker security isolation; the host is the authenticated damage boundary.
- **Sizing:** extraction may expose additional existing core dependencies. Keep one-way crate edges and split only if a capability exceeds a reviewable PR; do not place `WorkDb` in the spoke to make extraction easier.
- **Status is perishable:** build results and missing integrations above describe this design review. Refresh those observations and the task evidence when they land without silently rewriting the durable ownership/replay argument.

No human choice is required before reviewing this proposal. The dedicated-user, summariser and read-only-viewer choices resolve the investigation's product forks; build and provider findings have explicit evidence gates below.

## Proposed implementation task breakdown

Breakdown size: 37 entries (37 in-scope, 0 deferred) — the wire/journal/executor and gateway, hub consumers, managed provisioning, three legacy transport removals, local/offline migrations, remote app rollout and independent board pipeline have separate reviewable callers and acceptance boundaries.

All entries deliver to `spinyfin/mono`, each as one PR, normally within about 1,500 changed lines / 25 files. Effort reflects one worker session per capability. The legacy removal split is grounded in current source: `ssh_spawn.rs` alone is 1,462 lines, the three observation/reconciliation files total 1,125, and the wrapper/distribution Rust files add 854 before their adapter callers. These are separate deletions after acceptance, not one oversized adapter PR. Dependencies below are gates, not an instruction to serialize the entire list. The source phases are §3.2: prompt/protocol/spoke/hub/provisioning first, local and capacity next, offline qualification next, A afterward; board work is independent.

At depth zero, Prompt input, Protocol, Provisioner discovery and Invalidation measurement can run in parallel. After Protocol, Spoke journal, Hub stream and Host credentials can run in parallel in separate crates. Managed package/provider work proceeds alongside hub consumers. Local spoke and Capacity scheduling develop in parallel after the hub contracts exist; their production rollout follows remote acceptance, without making provisioning a false implementation dependency. A's remote app endpoint begins only after B qualification; its client and gate then precede hub deployment. Board runtime caching and invalidation separation are parallel after measurement; hook batching and dispatch concurrency are independent of them.

Substantial shared-file ordering is explicit: Workspace executor precedes Supervisor extraction; Hub adapter precedes Hub observation projection in `host_adapter.rs` / server wiring; Hub observation projection precedes hook-write batching in the ingress transaction code; the remote app endpoint precedes client behavior in shared request/envelope definitions. The later task must forward-port the earlier one's changes preservingly, never replace them with an older snapshot. Incidental BUILD/module registration conflicts do not serialize otherwise independent crates.

### Prompt input off argv

Close the argv disclosure independently of distributed execution (§3.2 step 1). Change the driver spawn plan and its local/legacy wrapper callers together so prompt text reaches the supported driver's stdin or prompt-file input while preserving interactive behavior.

- **Deliverables:** typed prompt input in `driver`; local and wrapper launch wiring with no prompt expansion into argv; Bazel regression coverage inspecting the actual launched process arguments and prompt receipt.
- **Effort:** medium
- **Dependencies:** none

Scope: in-scope

### Bazel spoke protocol and compatibility gate

Land `protocol-spoke` (§3.2 step 2), including the schema above, mappings and the missing Bazel integration. This is the hard toolchain gate: a declared module alone is insufficient. Keep runtime and generator versions coherent and dependencies visible only to actual consumers.

- **Deliverables:** pinned prost/prost-build/tonic generation and Rust types through Bazel; README plus descriptor breaking-change CI check; successful real Bazel build and old/new-peer/duplex mTLS tests on supported CI platforms.
- **Effort:** large
- **Dependencies:** none

Scope: in-scope

### Spoke journal and outbound session

Build the durable spoke transport foundation (§3.2 step 3): journal identity, command inbox, observation sequence and reconnecting outbound stream. Include a minimal runnable daemon whose real caller journals/replays capacity; this is not an unused persistence abstraction.

- **Deliverables:** `spoke-journal` storage/migrations/lock and bounded queues; daemon Connect/Hello/receipt/reconnect loop using existing backoff; Bazel crash/replay/gap tests against a real tonic test hub.
- **Effort:** large
- **Dependencies:** Bazel spoke protocol and compatibility gate

Scope: in-scope

### Host workspace and recovery executor

Extract and reuse the existing local cube transport and recovery filesystem operations for the spoke. Wire the typed workspace commands to that executor in the daemon, keeping lease policy and GitHub decisions in the hub (§§2.1–2.2).

- **Deliverables:** lower-level host execution crate and migrated local caller; complete WorkspaceOp validation/dispatch with lease reconciliation; Bazel tests for interrupted acquisition, recovery bookmark preservation and rejected paths/operations.
- **Effort:** large
- **Dependencies:** Spoke journal and outbound session

Scope: in-scope

### Spoke worker supervisor

Run provisioned drivers in token-identified tmux sessions (§3.2 step 3). Reuse host-execution, tmux and driver code, with durable Spawn/Adopt/Deliver/Stop phases and real host preflight.

- **Deliverables:** driver provisioning and prompt-safe launch under the service user; token-based adoption/control plus local lease heartbeat ownership; real-tmux Bazel tests for duplicate spawn, crash ambiguity, daemon restart and sibling-safe stop.
- **Effort:** large
- **Dependencies:** Prompt input off argv; Host workspace and recovery executor

Scope: in-scope

### Spoke hook and transcript ingestion

Move host-local observations into the daemon (§3.2 step 3), including hook authentication, file-ingress driver observations and transcript framing. This capability ends at durable normalized records; hub application is a separate consumer.

- **Deliverables:** peer-authenticated hook ingress and stable pending-event dedup/drain; reused transcript discovery/tail with generation/offset retention; Bazel tests for no-next-hook recovery, rotation, duplicate feeds and spool pressure.
- **Effort:** large
- **Dependencies:** Spoke worker supervisor

Scope: in-scope

### Spoke worker RPC gateway

Make the existing worker command surface usable from assigned remote workers, including queued proposals (§§2.2–2.3). Inventory the commands emitted by real prompt composition and expose only their run-scoped subset; preserve validators and local caller attribution.

- **Deliverables:** restricted local gateway with cached context and explicit online/offline outcomes; durable proposal/PR-output/artifact registration with CLI receipt handling; Bazel integration tests driving the actual worker CLI against the spoke, including queued done and denied cross-run calls.
- **Effort:** large
- **Dependencies:** Spoke hook and transcript ingestion

Scope: in-scope

### Hub authenticated stream manager

Add the hub's per-host stream registry and durable command/receipt transport (§3.2 step 4). Exercise it with the generated client and injected authorization/sink implementations before engine policy is wired; no new dispatch is enabled by this PR.

- **Deliverables:** mTLS identity checks, negotiation and connection fencing; bounded command futures/outbox interface and record receipt sink; Bazel tests for old streams, wrong host certs, replay admission and timeout uncertainty.
- **Effort:** large
- **Dependencies:** Bazel spoke protocol and compatibility gate

Scope: in-scope

### Hub execution adapter and controls

Implement SpokeHostAdapter for workspace/recovery/spawn and existing probe/interrupt/stop callers (§3.2 step 4). Persist assignment ownership before sending commands and preserve uncertain outcomes. Leave legacy runs on their recorded transport until final migration.

- **Deliverables:** adapter method parity including recovery operations; dispatch/control routing and atomic reservations/command persistence; real-stream Bazel tests for ownership, late results and no duplicate redispatch on timeout.
- **Effort:** large
- **Dependencies:** Spoke worker supervisor; Hub authenticated stream manager

Scope: in-scope

### Hub durable observation projection

Connect authenticated ingress to existing live-state, completion, proposal and liveness consumers (§3.2 step 4). Receipt and effect application must survive independently; wire the worker gateway's allowed online methods here. This follows adapter wiring because both substantially touch server integration and host ownership.

- **Deliverables:** WorkDb ingress/effect transactions and replay cursors; event/proposal/PaneState reducers plus worker request authorization; Bazel crash tests proving queued done and repeated observations produce one authoritative effect.
- **Effort:** large
- **Dependencies:** Hub execution adapter and controls; Spoke worker RPC gateway

Scope: in-scope

### Hub transcript and artifact custody

Materialize remote transcript/output bytes on the hub and use them in summary, cost and structured-output readers (§§2.1, 2.8; step 4). Preserve incomplete/unavailable evidence distinctions and register retention roots with existing cleanup code.

- **Deliverables:** offset/digest-safe transcript and artifact materialization with replay; existing consumers routed by run identity rather than remote paths; Bazel tests for projection crashes, watermarks, gaps and structured-output validation.
- **Effort:** large
- **Dependencies:** Hub durable observation projection

Scope: in-scope

### Read-only remote pane viewer

Deliver the increment-one remote pane experience (§2.8) using short-lived capture subscriptions, bounded text and the existing app subscription surface. No interactive terminal protocol or SSH setup is introduced.

- **Deliverables:** spoke capture and hub subscription routing with bounds/TTL; app viewer with host/time/staleness and existing controls; Bazel service/UI tests for unsubscribe, disconnect, escaping and disabled capture capability.
- **Effort:** medium
- **Dependencies:** Hub execution adapter and controls

Scope: in-scope

### Host enrollment and certificate lifecycle

Implement the trust boundary independently of platform installation (§3.2 step 5). Use host-bound single-use enrollment, atomic CSR retry identity, renewal overlap and revocation; keep management compatible independently of execution capabilities.

- **Deliverables:** CA/issuer storage and restricted Enroll/Renew service; host credential lifecycle and revocation hooks consumed by the stream authorization boundary; Bazel tests for replayed tokens, changed keys, expiry, overlap and revoked reconnect.
- **Effort:** large
- **Dependencies:** Bazel spoke protocol and compatibility gate

Scope: in-scope

### Signed spoke packages and services

Create independently installable daemon packages using the repository release pipeline (§2.6). Include the dedicated-user service environment and preserve live tmux/journal state across service restart; no host is enrolled merely by installing files.

- **Deliverables:** signed manifests/platform packages and pinned verification trust; launchd/systemd definitions and idempotent bootstrap installer; Bazel packaging/verification tests for wrong platform, digest/signature failure and owner-only paths.
- **Effort:** large
- **Dependencies:** Spoke worker supervisor

Scope: in-scope

### Managed provisioner selection

Resolve the investigation's unrecorded provider choice before implementing platform adapters. Inspect the actual managed fleet interfaces through authorized configuration/documentation, select supported macOS MDM and Linux cloud APIs, and commit a concrete report and contract fixtures. Do not change private operator state as the deliverable.

- **Deliverables:** repo report naming providers, API versions, job/identity semantics and evidence; secret-delivery/readiness/failure mapping to the design's job contract; sanitized request/response fixtures and acceptance procedure for both platforms.
- **Effort:** medium
- **Dependencies:** none

Scope: in-scope

### Hub managed provisioning jobs

Replace shell-driven add-host setup with persistent provider jobs and typed readiness (§3.2 step 5). Reuse host registration and the existing app progress/error surface; provider-specific execution belongs to the following adapters.

- **Deliverables:** persistent idempotent job stages and authenticated provider interface; host enrollment/release binding plus retry/cancel/status API; app add-host/status wiring and Bazel tests using the selected provider contract fixtures.
- **Effort:** large
- **Dependencies:** Managed provisioner selection; Host enrollment and certificate lifecycle; Signed spoke packages and services; Hub authenticated stream manager

Scope: in-scope

### Managed Linux host provisioning

Implement the selected cloud API/cloud-init adapter against the job contract (§2.2). The provider channel installs the package, delivers the enrollment secret and credentials to the dedicated user, starts the service and reports readiness without SSH.

- **Deliverables:** concrete cloud provisioner and versioned cloud-init payload; secret-reference bootstrap and as-user tool/credential preflight; Bazel provider-contract tests plus a repo-owned fresh-host acceptance runner/report format.
- **Effort:** large
- **Dependencies:** Hub managed provisioning jobs

Scope: in-scope

### Managed macOS host provisioning

Implement the selected MDM adapter for already managed machines (§2.2), including policy/job retries and service-user credential access. This can run in parallel with Linux provisioning because provider crates/payloads are separate and shared job APIs already landed.

- **Deliverables:** concrete MDM job adapter and installer payload; authenticated secret delivery and service-user/launchd preflight; Bazel provider-contract tests plus a repo-owned fresh-host acceptance runner/report format.
- **Effort:** large
- **Dependencies:** Hub managed provisioning jobs

Scope: in-scope

### Verified spoke rolling upgrades

Offer, fetch, verify and atomically switch signed releases over the management contract (§2.6). Preserve workers, enforce reader compatibility and recover through the same managed installer if the service cannot update itself.

- **Deliverables:** hub-approved release offer and spoke staged updater; service health/rollback state machine with journal reader checks; Bazel tests for interrupted switch, bad signatures, mixed versions and retained workers.
- **Effort:** large
- **Dependencies:** Signed spoke packages and services; Host enrollment and certificate lifecycle; Spoke journal and outbound session; Hub authenticated stream manager

Scope: in-scope

### Remote capacity qualification

Complete and qualify the first production increment using the genuine installed path (§3.2 steps 3–5). This is an independent acceptance campaign for the already reviewed services, followed by separately sized legacy removals.

- **Deliverables:** repo-owned end-to-end harness and recorded fresh-host results for supported platforms, a real driver and real PR publication; explicit host-transport drain/readiness rollout; no-SSH regression checks and a drain prerequisite for removing legacy code.
- **Effort:** large
- **Dependencies:** Hub transcript and artifact custody; Read-only remote pane viewer; Managed Linux host provisioning; Managed macOS host provisioning; Verified spoke rolling upgrades

Scope: in-scope

### Retire SSH observation and recovery paths

Remove remote event forwarding, pid liveness reconciliation and transcript shell readback (§3.2 step 4) after the acceptance rollout disables legacy dispatch and drains old assignments. Do this before launch removal because reattachment imports forwarding helpers from `ssh_spawn`.

- **Deliverables:** removal of `remote_reattach`, `remote_lease_reconcile` and `remote_transcript`; adapter/server callers using inventory, PaneState and hub transcript custody; parity tests preserving unknown-versus-dead and unavailable-versus-invalid behavior.
- **Effort:** medium
- **Dependencies:** Remote capacity qualification

Scope: in-scope

### Retire SSH launch handshakes

Remove the superseded detached-worker launch and stderr/pid handshake (§3.2 step 4) after its observation consumers are gone. Migrate any remaining launch fixtures to the actual spoke path rather than retaining an unexercised fallback; forward-port the preceding adapter edits.

- **Deliverables:** removal of `ssh_spawn.rs` and legacy launch callers; explicit migration-required failure for an unmigrated host instead of fallback; passing launch/control tests through SpokeHostAdapter.
- **Effort:** medium
- **Dependencies:** Retire SSH observation and recovery paths

Scope: in-scope

### Retire SSH wrapper provisioning and adapter

Delete the obsolete wrapper/distribution mechanism and remaining SshHostAdapter now that its execution and observation consumers have moved. Keep any separately used operator SSH utility intact; this capability removes worker execution's SSH dependency.

- **Deliverables:** removal of the remote wrapper, its distribution/provisioning path and remaining adapter; host registration routed exclusively through managed enrollment; Bazel architecture checks preventing new worker SSH fallbacks.
- **Effort:** medium
- **Dependencies:** Retire SSH launch handshakes

Scope: in-scope

### Local spoke adoption

Route the local worker host through the same daemon/adapter (§3.2 step 6). Bundle/supervise the child, retain the `local` machine identity and transfer existing sessions behind a durable adoption barrier before deleting direct hub worker observers.

- **Deliverables:** private local enrollment/child lifecycle and existing-session migration; removal of local worker tmux/pid/lease/panic paths once adopted; real-tmux Bazel tests for migration crashes, preserved sessions and same-machine viewer selection.
- **Effort:** large
- **Dependencies:** Hub transcript and artifact custody

Scope: in-scope

### Capacity scheduling and unreachable hosts

Replace static free-seat inference with fresh per-host resource budgets and outstanding reservations (§3.2 step 7). Retain host enablement independently from reachability and preserve operator-only abandonment of silent runs.

- **Deliverables:** generation-aware budget/reservation scheduling on every host; fresh-challenge readiness and unreachable attention behavior; Bazel tests for concurrent admission, replayed capacity, stale reports and silence without orphaning.
- **Effort:** large
- **Dependencies:** Hub durable observation projection

Scope: in-scope

### Awake-time replay hardening

Complete the clock and horizon semantics for option B (§3.2 step 8), including replay barriers and offline lease/parked-worker continuity. Reuse the suspension crate if present; otherwise supply the same small clock-pair contract within this capability.

- **Deliverables:** persisted per-boot awake ledger and clock-aware consumers; replay/inventory barriers that suspend stale/spawn-ack/backstop judgments; Bazel fault tests for hub sleep, spoke sleep/reboot, clock skew and offline parked completion.
- **Effort:** large
- **Dependencies:** Local spoke adoption; Capacity scheduling and unreachable hosts

Scope: in-scope

### Option B end-to-end qualification

Run and commit the independent real-path acceptance campaign for offline completion (§2.3), using the installed hub/spoke, actual tmux/cube and worker CLI. It validates the chosen design; it does not compare deployment topologies. All test assets and the report land in the repo.

- **Deliverables:** Bazel fault-campaign targets covering every record kind/command phase and the failure matrix above; reproducible real sleep/disconnect/reboot procedure and measured evidence; option-B acceptance report with receipt/application/custody invariants and any fixed failures.
- **Effort:** large
- **Dependencies:** Awake-time replay hardening; Retire SSH wrapper provisioning and adapter

Scope: in-scope

### Authenticated remote app endpoint

After B completes, add the hub's independent remote app/CLI trust boundary for option A (§3.2 step 9). Preserve local IPC, distinguish user identities from spoke identities, and persist mutation idempotency/results so uncertain remote writes can be reconciled.

- **Deliverables:** authenticated remote frontend endpoint and shared Rust client support; durable user/request deduplication and coordinator access authorization; Bazel tests for duplicate/lost replies, revocation and rejected spoke-as-user credentials.
- **Effort:** large
- **Dependencies:** Option B end-to-end qualification

Scope: in-scope

### Remote app connection and mutation behavior

Use the real authenticated endpoint in the macOS app (§2.3). Make connection loss visible, keep cached revisioned state, recover subscription gaps and preserve explicit uncertain outcomes; remote disconnect must never autostart a competing local hub.

- **Deliverables:** endpoint selection/auth and autostart rules; bounded cached subscriptions plus idempotent/uncertain action UX and remote coordinator view; Bazel app/client tests for reconnect, stale navigation, duplicate responses and command priority.
- **Effort:** large
- **Dependencies:** Authenticated remote app endpoint

Scope: in-scope

### Remote app latency and reconnect gate

Measure the genuine app path against the full §2.3 matrix, after the candidate endpoint/client exists and before always-on deployment. Commit instrumentation and results; if thresholds fail, fix the measured cause before declaring the gate passed, without relaxing requirements.

- **Deliverables:** repo network-matrix runner and request/byte/queue/UI timing instrumentation; local/remote median-tail and reconnect report for the named user actions; passing 50 ms/10 Mbit/s gate with mutation/queue correctness evidence.
- **Effort:** large
- **Dependencies:** Remote app connection and mutation behavior

Scope: in-scope

### Always-on hub packaging and migration

Deliver option A only after its client gate passes (§3.2 step 9). Package an independently maintained hub service with state migration/backup/rollback procedures, coordinator hosting and one-authority enforcement; server operation cannot require an online laptop.

- **Deliverables:** signed independent hub package/service/update configuration; quiesced authority-preserving migration and backup/rollback tooling/runbook; Bazel lifecycle tests and recorded failover/rollback procedure proving no simultaneous authority or local autostart fallback.
- **Effort:** large
- **Dependencies:** Remote app latency and reconnect gate

Scope: in-scope

### Hub invalidation measurement

Start the independent §3.2 step-10 track by validating the runtime-invalidation premise in §2.4. Extend the existing scale fixture and tracing, rather than inferring traffic proportions from the worker-count estimates.

- **Deliverables:** source/reason fanout and assembly timing instrumentation; reproducible Bazel fixture at 8/32/64 workers; repo report of runtime versus structural invalidations, DB writes and dispatch backlog.
- **Effort:** medium
- **Dependencies:** none

Scope: in-scope

### Cached per-item board runtimes

Avoid repeated runtime reconstruction in existing board assembly (§2.4), with explicit cache invalidation for each runtime input. This is useful with existing local workers and has no spoke deployment dependency.

- **Deliverables:** per-item runtime cache and bounded lifecycle; equivalence tests against uncached runtime calculations; before/after scale-fixture timing and memory evidence.
- **Effort:** medium
- **Dependencies:** Hub invalidation measurement

Scope: in-scope

### Separate runtime and structural invalidations

Route frequent one-card runtime changes without full product assembly (§2.4). Preserve structural changes, product moves, relationships and deletion behavior; scope reconciliation to actual structural work.

- **Deliverables:** typed runtime/structural invalidation routing and consumers; Bazel coverage for moves/deletes/related cards and runtime-only updates; measurement showing the intended reduction in full assemblies.
- **Effort:** large
- **Dependencies:** Hub invalidation measurement

Scope: in-scope

### Incremental hub board projection

Maintain the hub's board projection incrementally after the cheap measures (§2.4). Plan the event interface alongside local adoption; consume existing per-run runtime inputs so the implementation need not wait for the local transport migration.

- **Deliverables:** incremental model with explicit affected relationships/revisions and snapshot recovery; complete-projection parity tests for runtime and structural event sequences; scale-fixture CPU/memory/wire measurements and bounded fallback behavior.
- **Effort:** large
- **Dependencies:** Cached per-item board runtimes; Separate runtime and structural invalidations

Scope: in-scope

### Batched durable hook writes

Amortize hub ingress writes without weakening durable receipts (§2.4). Follow the observation projection PR because both change the same transaction path; apply the same batching to existing local ingress, independent of spoke rollout.

- **Deliverables:** bounded size/time batching with per-host fairness; receipt advancement only after commit and prompt control priority; Bazel crash/ordering tests and throughput/tail-latency measurements.
- **Effort:** medium
- **Dependencies:** Hub durable observation projection; Hub invalidation measurement

Scope: in-scope

### Bounded concurrent dispatch

Remove the serial dispatch-drain bottleneck (§2.4) while preserving atomic execution/host reservation and workload budgets. This PR is independent of the board model but follows the capacity scheduler because both substantially edit dispatch admission.

- **Deliverables:** bounded parallel drain with per-execution exclusion; cancellation/error reconciliation without leaked reservations; Bazel contention tests and dispatch-backlog measurements.
- **Effort:** medium
- **Dependencies:** Capacity scheduling and unreachable hosts; Hub invalidation measurement

Scope: in-scope
