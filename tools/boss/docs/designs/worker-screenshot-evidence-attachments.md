# Worker screenshot evidence: attachments with retention

- **Date:** 2026-07-31
- **Related designs:** [worker-proposal-api](worker-proposal-api-replace-fragile-worker-to-engine-seams.md) (transport, attribution, typed refusals), [attentions](attentions.md)
- **Prior art in-repo:** `tools/boss/app-macos/Sources/BossCapture.swift` (the `--capture-to` route), `tools/boss/engine/grok-home-retention` and `tools/boss/engine/codex-rollout-retention` (retention sweep shape)

**TL;DR:** A worker that changes UI can render a screenshot but has no way to show it to a human reviewer, so reviewers get prose about an image nobody can look at. This adds `boss attach <path>`: a worker-tier verb that hands the engine an image, which validates it, stores it content-addressed under the state root, and serves it from a loopback HTTP gallery. The worker pastes the gallery URL into its PR body; the reviewer clicks it from the GitHub PR. Retention is age-based (90 days) with a total-bytes backstop, swept on a schedule and on demand via `bossctl attachments sweep`, and reclaimed links explain themselves instead of 404ing.

## The problem

Repo policy forbids committing capture PNGs — `tools/boss/app-macos/README.md` and every worker's generated CLAUDE.md say so. **That policy is correct and this design does not change it.** Screenshots do not belong in source control, and the one PR in `spinyfin/mono` that ever linked images anyway (via `raw.githubusercontent.com` against its own branch) 404s today, because merging deleted the branch.

The gap is that nothing replaced it. Workers render evidence, look at it, delete it, and describe it in prose. Evidence a human cannot look at is not evidence.

**Why there is no workaround.** `gh` has no image-upload command. GitHub's `user-attachments` endpoint — what browser drag-and-drop uses — is not part of the public REST API and needs a browser session plus a CSRF token. There is no supported path from a headless worker to an image a GitHub PR page can render.

## Goals

- A reviewer looking at a GitHub PR can see the images the worker rendered, in a few clicks, without the worker having violated the no-commit policy.
- Evidence outlives the cube workspace that produced it (workspaces are released and recycled).
- A bad submission fails loudly and immediately, with a typed error the worker can act on mid-run. Nothing is silently downscaled, truncated, or dropped.
- Storage growth is bounded and the bound is enforced by shipped code, not by a promise.

## Non-goals

- **Relaxing the no-commit policy.** The point is to give it a viable alternative.
- **Side branches, an assets repo, or gists.** Same problem with extra steps: they either violate the policy or rot on merge.
- **Making the render route first-class.** Producing an image (the app's `--capture-to`, or the undocumented XCTest + `NSHostingView` + `cacheDisplay` technique that survives only as prose in PR bodies) is a separate concern from delivering one. Worth doing; not this.
- **A macOS app surface.** See "Alternatives considered".

## Shape

### Ingest — `boss attach <path> [--caption …]`

A new worker-tier RPC, `SubmitAttachment`, sibling to `SubmitProposal`. It reuses the proposal API's transport, its peer-pid attribution (`app::proposals::attribute_caller`), and its typed refusal vocabulary (`ProposalSubmissionError`) — the attribution failure modes are literally identical, and a worker that has learned to read one refusal has learned to read both.

**Not a `ProposalKind`.** A proposal is a JSON payload carrying a judgment that the apply pipeline dispositions (`proposed` → `applied`/`rejected`) and that a human may need to act on. An attachment is bytes with no judgment attached and nothing to decide. Modelling it as a proposal would mean base64 in `payload_json`, an apply policy with no apply, and a state machine with one reachable state.

**The worker submits a path, not bytes.** The engine opens the file itself. That keeps a multi-megabyte PNG off the socket and makes the stored bytes the ones actually on disk. It works because worker and engine share a machine — the same local-peer constraint the proposal API already carries.

Because the engine reads a path the _worker_ chose, that is a confused-deputy shape: unconstrained, a worker could name `state.db` and have the engine copy it into an HTTP-served store. Three independent gates close it:

1. **Path confinement.** The path is canonicalised (so a symlink out of the workspace is resolved and _then_ caught, not followed blindly) and must land under the _attributed_ execution's own cube workspace, one of its `bazel-*` symlink targets, or a system temp dir. The execution is peer-resolved, so a worker cannot borrow another run's workspace by claiming its id.
2. **Format sniffing.** The bytes must be a real PNG or JPEG by magic number, with dimensions read from the header. The filename extension is never consulted — it is a claim, the header is evidence. No database, key file, or transcript passes.
3. **Caps.** 8 MiB per image, 1..=10000 px per side, 24 per execution, 96 per work item. Count caps are checked _before_ any bytes are written, so a looping worker never leaves a blob behind.

Every breach is a typed refusal naming the offending field. Oversize is refused, never downscaled: storing something other than what the worker rendered would make the evidence a lie.

### Storage — content-addressed under the state root

`<state_root>/attachments/<first two hex chars>/<sha256>.<png|jpg>`, with metadata in a new `work_attachments` table.

- **Why the state root:** a cube workspace is ephemeral scratch, released at the end of a run and re-leased by unrelated work. Evidence there would be gone before a reviewer looked. The state root's lifetime is the install's.
- **Why content addressing:** deduplication falls out (a revision chain that re-renders an unchanged view stores one blob), and ingest is idempotent without a separate key — `UNIQUE (execution_id, content_digest)` makes resubmitting the same image a replay rather than a duplicate. The bytes already are an idempotency key.
- **Why not in SQLite:** a screenshot is not a database row, and keeping bytes outside means retention can reclaim them while leaving the row.

### Association — both execution and work item

`execution_id` records which run produced the image; `work_item_id` (derived from the execution) is the reviewer-facing scope.

A revision chain produces several runs against one PR, and the reviewer's question is "what evidence exists for this PR", not "what did this one run render" — so the gallery is per work item. Within it, images are grouped by execution, newest run first and labelled _latest run_, which is how a reviewer tells a fix from what it replaced.

### Surfacing — a loopback HTTP gallery, linked from the PR body

**This is the part that solves the problem, and it is the acceptance bar: a mechanism that stores images but leaves the reviewer unable to see them has not solved anything.**

The engine serves two routes on `127.0.0.1`:

- `GET /w/<work-item-id>` — HTML gallery, grouped by execution, newest first
- `GET /a/<attachment-id>` — the image bytes

`boss attach` prints the gallery URL; the worker puts it in the PR body under `## Evidence`; the reviewer clicks it from the GitHub PR page.

**Why a link and not an inline image.** GitHub's markdown sanitiser linkifies `http` and `https` and nothing else — not `file:`, not a custom scheme — so a loopback `http://` link is the only shape that survives into a PR body _and_ can reach bytes on the reviewer's machine. Inline `![](…)` is deliberately not attempted: GitHub proxies image sources through camo, which fetches from GitHub's servers and cannot reach anyone's loopback. A link is the honest shape.

**Why a fixed port.** A URL pasted into a PR body has to still resolve after the engine restarts, so the port cannot be rolled per boot. Default 8419, `BOSS_ATTACHMENT_PORT` to move it, `0` to disable.

**Why bind-then-publish.** The port is published to the rest of the engine only after a successful bind (an `Arc<OnceLock<u16>>` the server sets). If the bind fails — most often a second engine, e.g. a `--socket-path` fixture running beside an operator's production engine — no URL is minted at all and `boss attach` says so plainly. Minting a URL against a port this engine does not own would hand a reviewer _someone else's_ gallery, which is worse than no link.

**Why it is safe to run.** It binds loopback only, so nothing off-host can reach it. Two further gates handle the browser-based attacker: the `Host` header must name loopback (a page at `evil.com` whose DNS answers `127.0.0.1` still sends `Host: evil.com`, and is refused — the standard DNS-rebinding defence), and no CORS headers are ever emitted, so cross-origin script cannot read a response even if it elicits one. Ids are engine-minted and unguessable, the surface is read-only, `GET`/`HEAD` are the only accepted methods, and every worker-supplied string is HTML-escaped.

**Why hand-rolled.** There is no HTTP server anywhere in this repo to reuse — `tools/boss/http_retry` is a client-side retry helper. What this serves is a handful of static blobs and one generated page to a browser on the same machine; pulling in `hyper`/`axum` would add a vendored dependency tree to the process that owns the engine database in exchange for routing that fits in a `match`.

### Retention

Age-based with a total-bytes backstop, following the `codex-rollout-retention` / `grok-home-retention` precedent exactly — same policy shape, same env-var override style, same "candidates come from recorded rows, liveness comes from execution status, never file mtime" rule — so an operator has one retention model to learn rather than three.

- **90 days** (`BOSS_ATTACHMENT_RETENTION_DAYS`), against 14 for the home sweeps. The artefacts are ~1000x smaller and their audience shows up on a human schedule; a PR link that dies in a fortnight is barely better than no link.
- **1 GiB backstop** (`BOSS_ATTACHMENT_MAX_TOTAL_BYTES`), reclaiming oldest-first inside the age window.
- **Live executions are never reclaimed**, regardless of age or the byte cap.
- Enforced by `attachment_retention_sweep` (hourly, spawned in `serve`) and on demand by `bossctl attachments sweep [--dry-run]`.

Two rules the home sweeps do not need:

- **Blobs are shared.** Content addressing means several rows can point at one file, so a blob is deleted only once every row referencing it is reclaimed. Reclaiming one row of a deduplicated pair must not blank the other.
- **Rows outlive blobs.** Reclaiming deletes bytes and stamps `reclaimed_at`; it does not delete the row. A gallery link in an already-merged PR body then answers "captured by execution X on <date>, reclaimed by retention on <date>" instead of a bare 404 that reads like a bug. Tombstones go with their execution via `ON DELETE CASCADE` when execution retention prunes it.

**The orphan pass.** The home sweeps deliberately never scan a directory — they would risk deleting an operator's interactive `~/.codex`. This store has no such hazard: `<state_root>/attachments` is created, written, and owned by the engine alone. Scanning it is therefore both safe and _necessary_, because a crash between "blob written" and "row inserted" leaves bytes no row will ever reference, which no row-driven sweep would ever reclaim. A one-hour grace period keeps the pass off blobs that are merely mid-ingest.

### Worker instructions

The mechanism is inert if nothing tells workers to use it. `worker_setup.rs`'s generated CLAUDE.md and `app-macos/README.md` both change from an unqualified "do not commit capture PNGs" — with no alternative, which is why workers delete them — to "do not commit, and do not delete either: `boss attach` it and put the URL in the PR body". The conformance golden is updated alongside.

## Alternatives considered

### A. A new `ProposalKind`

Rejected: the payload would be base64 bytes in a JSON column, the apply policy would have nothing to apply, and the state machine would have one reachable state. What is genuinely shared with proposals — transport, attribution, typed refusals — is reused directly instead.

### B. Serve only through the macOS app

Rejected against the brief's constraint. Review happens on a GitHub PR page; a mechanism reachable only from an app window leaves the reviewer on GitHub with nothing to click. The engine is a daemon that is running whenever Boss is in use, which is a materially weaker requirement than "the app is open and focused". An app-side viewer would be a nicety on top of this, not a substitute for it.

### C. Post the images to the PR via the API

Rejected as impossible, not undesirable — verified: `gh` has no image-upload command and GitHub's attachment endpoint is not public API. If that ever changes, this design's storage and retention halves are unaffected; only the surfacing half would gain a second route.

### D. Bytes over the socket instead of a path

Rejected: it puts multi-megabyte frames through the frontend socket for no gain, since the engine can open the file directly. The path-based form needs confinement, which the design specifies and the implementation tests.

## Risks

- **A reviewer on a different machine sees nothing.** The link resolves only on the host running the engine. That is inherent to any solution that does not upload to GitHub, which is not available. The gallery is at least explicit about it rather than showing a broken image.
- **Links die at retention.** Bounded retention is a hard constraint; 90 days is the mitigation, and the tombstone response means a dead link explains itself. A shared blob surviving until its last row ages out means a re-rendered identical image effectively refreshes the bytes.
- **Port collision between engines.** Handled by bind-then-publish: the loser mints no URLs rather than pointing at the winner's gallery. `BOSS_ATTACHMENT_PORT` moves it.
- **Any local process can read the store over loopback.** True, and the same is true of `state.db`. The contents are screenshots the worker chose to publish to a reviewer. The `Host` gate and absent CORS headers close the browser-mediated paths, which are the ones a local user does not already have.
