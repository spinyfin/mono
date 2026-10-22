# boss-protocol-spoke

The wire contract between a Boss hub and its spokes: the `boss.spoke.v1`
protobuf schema, the Rust types and tonic services Bazel generates from it, and
the small contract helpers both ends must agree on. It is the first step of the
hub-and-spoke engine ([design](../docs/designs/hub-and-spoke-distributed-engine.md),
[investigation §2.2](../docs/investigations/hub-and-spoke-distributed-engine-2026-10-21.md)).
Nothing consumes it yet; the journal, the spoke daemon and the hub's stream
manager are separate crates that depend on it.

A spoke dials the hub over mutual TLS and holds one bidirectional
`SpokeHub.Connect` stream. The spoke sends `Hello`, the hub answers `Welcome`
(or a readable `Reject`), and from then on sequenced `Record`s flow up and
sequenced `Command`s flow down, each side acknowledging the other's durable
prefix with a `Cursor`. The `Enrollment` service issues and renews the host
certificate. `proto/boss/spoke/v1/spoke.proto` is the reference; its header
comment states the rules below in one place.

## Layout

| Path                              | What it is                                                                                         |
| --------------------------------- | -------------------------------------------------------------------------------------------------- |
| `proto/boss/spoke/v1/spoke.proto` | The schema. The path matches the package, as protobuf tooling expects.                             |
| `src/lib.rs`                      | Re-exports the generated items as `v1` and declares the modules below.                             |
| `src/limits.rs`                   | Frame, chunk and payload bounds; negotiated downward only.                                         |
| `src/validate.rs`                 | `Validate` for decoded messages: identities set, oneofs present, enums specified, sizes in bounds. |
| `src/negotiate.rs`                | Epoch and feature-revision selection for `Hello`, and the typed `Reject`.                          |
| `src/worker_event.rs`             | `boss_protocol::WorkerEvent` ⇄ wire `WorkerEvent`.                                                 |
| `src/identity.rs`                 | The `urn:boss:host:<id>` certificate identity and the rule that `Hello.host_id` must match it.     |
| `compat/`                         | The released-schema baseline and the script behind the breaking-change gate.                       |
| `tests/`                          | Old/new-peer semantics, mTLS duplex streaming, schema rules, toolchain coherence.                  |
| `//third_party/prost`             | The Bazel prost/tonic codegen toolchain this package uses.                                         |

## How Bazel builds it

`proto_library` compiles the schema, `rust_prost_library` runs the
`protoc-gen-prost` and `protoc-gen-tonic` plugins over it, and the result is the
`spoke_v1_proto` crate that `protocol-spoke` wraps. Everything is declared
inputs and pinned tools: protoc is the prebuilt binary the `protobuf` module
provides, the plugins are built by Bazel, and nothing reads an ambient `protoc`
or runs a Cargo build script.

| Piece                       | Pin                     | Where                                         |
| --------------------------- | ----------------------- | --------------------------------------------- |
| `rules_rust_prost`          | 0.70.0 (patched, below) | `MODULE.bazel`                                |
| `protobuf` / `rules_proto`  | 34.0.bcr.1 / 7.1.0      | `MODULE.bazel`                                |
| `prost`, `prost-build`      | 0.13.5                  | `Cargo.lock`                                  |
| `tonic`, `tonic-build`      | 0.12.3                  | `Cargo.lock`                                  |
| `protoc-gen-prost`          | 0.4.0                   | `Cargo.toml` (build-dependency), `Cargo.lock` |
| `protoc-gen-tonic`          | 0.4.1                   | `Cargo.toml` (build-dependency), `Cargo.lock` |
| `buf` (breaking check only) | 1.73.0, per platform    | `MODULE.bazel`                                |

Three decisions are not obvious from the build files:

- **The runtime and the generator come from one lockfile.** `rules_rust_prost`
  ships its own copy of prost, tonic and tokio. Using it would put a second
  tokio in the graph, and tonic's public API is written in terms of tokio's
  traits, so a `TcpStream` or `tokio-rustls` stream from the repo's tokio could
  not be handed to it. `//third_party/prost` is a replacement toolchain that
  takes the runtime crates and the two plugins from `@mono_crates`. The plugins
  are declared as `[build-dependencies]` of this crate (there is no build
  script) with `gen_binaries` annotations in `MODULE.bazel`.
  `tests/toolchain_coherence.rs` fails if a lockfile bump leaves the generator
  and the runtime on different releases.
- **`rules_rust_prost` carries a patch**
  (`third_party/patches/rules_rust_prost-0.70.0-providers-dict.patch`). The
  repo's `rules_rust` patch (the test-wrapper backport) makes
  `rustc_compile_action` return a dict of providers, where stock 0.70.0
  returned a list, so every proto aspect failed with "Couldn't find a CrateInfo
  in the list of providers". The same patch also files `CcInfo` under the wrong
  key on Bazel 9, so no rlib in this repo carries one; the prost patch tolerates
  that. Drop it when `rules_rust` is upgraded past the backport.
- **`tonic_opts = ["no_transport"]`.** An rpc named `Connect` collides with the
  `connect(dst)` constructor tonic adds to every generated client. Clients here
  are built from a configured mTLS `Channel`, so the shortcut is never used.

This crate has a `Cargo.toml` so crate_universe can resolve its dependencies,
but it is a Bazel crate: the generated code exists only in Bazel's output tree,
so `cargo build -p boss-protocol-spoke` is not a supported path.

To upgrade prost or tonic, move the runtime crates and the matching plugin
together in `Cargo.toml`, let Bazel refresh `Cargo.lock`, and run
`bazel test //tools/boss/protocol-spoke/...`.

## Evolution rules

Hubs and spokes are routinely different builds, so the schema follows
protobuf's compatibility rules strictly. A reader must stay correct when it
meets something it does not know.

- Field numbers and enum numbers are permanent. A removed field or enum value
  must `reserved` both its number and its name.
- A new field is optional with a documented default, and a reader behaves
  correctly when it is absent. A field is never repurposed; a changed meaning is
  a new field or a new message.
- Every enum has a `*_UNSPECIFIED = 0` member. Unknown enum numbers are kept as
  the raw integer and treated as "a newer peer sent something I do not
  understand": the most conservative behaviour, never success, never a panic.
- An unset oneof is malformed or newer than the reader, never success.
  `ValidationError::is_unsupported` separates those from plainly malformed
  frames so a hub can quarantine the record and answer with a typed unsupported
  result.
- `Hello` carries a wire-epoch range and named features; `Welcome` selects one
  epoch and one revision per feature, once per connection, frozen until
  reconnect. Build fingerprints are informational and never decide
  compatibility.
- The epoch stays at 1 across ordinary releases. Additive fields, new optional
  messages and new drivers do not change it. Only an incompatible base envelope,
  handshake or ack ordering justifies a new epoch. A feature whose meaning must
  change gets a new revision (`transcript-delta/2`), not a new meaning.
- With no common epoch the hub answers `Reject` and closes. `Reject` and the
  `Hello` epoch fields never change meaning, so an incompatible peer always
  learns what to upgrade.
- prost does not keep unknown fields through a decode and re-encode. A journal
  keeps the original bytes of every record and replays those, and an upgrade
  keeps decoders for records still outstanding.

### The compatibility gate

`proto_breaking_test` runs a pinned `buf breaking` with FILE-level
compatibility (wire, JSON and generated-source), plus the rule that deleting a
field or enum value is allowed only if both its number and its name are
reserved. Both sides are compiled by buf from source, so the check needs no
protoc and no network. It fails in two situations:

1. **Drift.** The schema differs from `compat/baseline/`, the checked-in copy of
   the released contract. A backward-compatible change is recorded with
   `bazel run //tools/boss/protocol-spoke:update_baseline`, which first checks
   the change against the old baseline and refuses a breaking one. Every wire
   change therefore appears as a visible diff of `compat/baseline/`; review it as
   a contract change.
2. **Not compatible with the merge base.** For pull requests, CI stages the
   baseline from the PR's merge base into `compat/merge_base/`
   (`.buildkite/steps/stage-spoke-proto-merge-base.sh`) as an input file of the
   test. This is what stops a pull request from making a breaking change and
   regenerating the baseline to match. Staging is best effort: if it cannot
   run, the test reports that and checks the checked-in baseline alone.

If a change is genuinely breaking, do not edit the baseline. Add the new shape
in a new package (`boss.spoke.v2`) or keep the old field.

The gate proves the schema only grows. It cannot prove readers behave correctly
when they meet the growth, which is what the peer tests are for.

## Contract helpers

- **Validation.** Decoding is total, so receivers validate before acting: scalar
  zero values are not identities, singular messages must be present, enums must
  be specified and known, sizes stay within `limits`, and each `Record` and
  `Command` body is run-scoped, host-scoped or either, as the design requires.
  Validation does not decide policy. Which environment variables a `Spawn` may
  set for a driver, and which `WorkspaceOp` fields each operation requires or
  forbids, belong to the components that execute those commands.
- **Negotiation.** `Contracts::first_release()` is the epoch-1 contract set: the
  eight execution features are required (`execution`, `journal`, `workspace`,
  `worker-event`, `worker-api`, `proposal-json`, `transcript-delta`,
  `artifact`), `pane-capture` and `upgrade` are optional. `select` checks the
  epoch range before anything else in `Hello`, so a future-epoch peer is told to
  upgrade even if the rest of its `Hello` no longer parses; `confirm` lets the
  spoke check the hub's `Welcome`.
- **Identity.** A host certificate names exactly one host through a URI SAN
  `urn:boss:host:<id>`; host ids are 1-128 bytes of `[A-Za-z0-9._-]`. The
  certificate authenticates and `Hello.host_id` only claims.
- **Worker events.** `worker_event::to_wire` / `from_wire` map the seven hook
  variants. Tool values travel as JSON bytes, `Null` as empty bytes. An unknown
  stop reason maps to `Other`, never `Completed`.

## Tests

All run under `bazel test //tools/boss/protocol-spoke/...` in the repository
test sandbox. Loopback TCP is allowed there, so the mTLS test is an ordinary
`rust_test`.

| Target                     | Covers                                                                                                                                                                |
| -------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `protocol-spoke_test`      | Unit tests for validation, negotiation, limits, identity and the `WorkerEvent` mapping.                                                                               |
| `compat_test`              | Old and new peers: unknown fields, enum numbers and oneof variants; defaults for absent optional fields; negotiation across builds; pinned bytes for the handshake.   |
| `mtls_duplex_test`         | A tonic hub and spoke over real mTLS: records and commands in both directions, refusal of missing, rogue and mismatched certificates, readable `Reject`, frame bound. |
| `schema_rules_test`        | Rules every version must satisfy, on Bazel's own descriptor: proto3, `*_UNSPECIFIED = 0`, one `body` oneof on framing messages, the service shapes.                   |
| `toolchain_coherence_test` | Generator and runtime come from the same prost/tonic releases.                                                                                                        |
| `proto_breaking_test`      | The compatibility gate above.                                                                                                                                         |

`buf` is pinned for macOS arm64 and Linux x86_64/arm64; on any other platform
`proto_breaking_test` is skipped as incompatible rather than failing analysis.
