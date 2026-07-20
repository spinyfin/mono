# Design: OAuth Device-Flow Auth for Issue Sync

- **Status:** Implemented. Shipped end to end against a registered OAuth App; live org-approval / SSO validation is still outstanding (§9, R5).
- **Parent project:** OAuth device-flow auth for issue sync.
- **Shipped:** T-1–T-7, all merged 2026-05-29 — PRs spinyfin/mono#908, #913, #917, #939, #938, #951, #950. The T-0 App registration was done separately as T767.
- **Builds on:** investigation T753 — [`oauth-device-flow-scopes-vs-issue-sync-2026-05-28.md`](../investigations/oauth-device-flow-scopes-vs-issue-sync-2026-05-28.md) (PR spinyfin/mono#897), whose conclusions are treated as fixed constraints.
- **Extends:** [`external-issue-tracker-sync-github-projects.md`](external-issue-tracker-sync-github-projects.md) — Design Question 11, "Credentials", is the seam this doc fills in.

This document describes the design **as built**. It was written before
implementation and revised afterwards against the merged PRs, so it reflects
what actually shipped rather than what was originally proposed. Points where
the implementation deliberately diverged from the original plan — or where
planned behavior did not ship — are called out inline as **As built**.

---

## Overview

Before this project, Boss's GitHub issue-sync reconciler authenticated
**implicitly**: `credentials.rs` ran `gh auth status` once and, on success,
every `gh api` shellout inside `CommandGhRunner` inherited whatever login the
user's locally-installed `gh` happened to carry. The effective GitHub
permissions were invisible to Boss and unenforced: they were "whatever scopes
the user's `gh auth login` happened to grant." `TrackerCredential::ambient()`
is literally an empty token marker that means "use ambient auth."

> **Code locations.** The implementation landed at the paths this doc cites
> (`engine/src/external_tracker/…`, `protocol/src/…`). A later engine
> restructure split these into crates: the device-flow client, state machine,
> keychain store, and org probe now live in the `boss_github_tracker` crate
> (`tools/boss/github_tracker/src/{github_oauth.rs, github.rs}`); the engine
> keeps the orchestration (`engine/core/src/app/{server.rs, github_auth.rs}`)
> and the reconciler (`engine/core/src/external_tracker/…`), with an
> `OrgStateSink` port keeping the `boss_engine → boss_github_tracker`
> dependency edge one-directional.

This project replaced that implicit reliance with an **in-app OAuth device
authorization flow** that obtains an explicit GitHub **user token** with a
known scope set, stored securely, owned by the engine, and driven from the
**existing issue-sync settings** in the product UI (the `ExternalTrackerSection`
in `app-macos/Sources/ContentView.swift`, where a product is bound to an
org/repo/project).

Per T753, the chosen identity is an **OAuth App** (not a GitHub App): a GitHub
App user token only works on repos/orgs where the App is _installed_, and we
explicitly do not want per-repo installation. The device flow yields a
classic-scoped OAuth user token; the exact scope set T753 concluded is
**`repo project`**.

The split of responsibility follows the product's "engine owns reconciliation,
UI is a thin client" principle:

- **Engine owns** the device-flow state machine, the polling loop, the token,
  and keychain persistence. It exposes a few small RPC verbs and pushes auth
  state as events.
- **UI is a thin driver:** it sends "start authorization / cancel / disconnect"
  and renders whatever auth state the engine pushes (the `user_code`, the
  verification URL, polling/success/error). It never sees or stores the token.

---

## Goals

- Obtain a GitHub **user token** for issue sync via an **in-app OAuth device
  authorization flow**, with the **exact scope set T753 concluded** (`repo
project`), so Boss controls and knows the token's scopes instead of
  inheriting whatever `gh auth login` carried.
- Drive the flow from the **existing issue-sync settings surface**
  (`ExternalTrackerSection`), not a new settings area: start authorization,
  show the user code + verification URL (optionally open the browser), show
  polling / success / error, and offer disconnect / re-authorize. Show current
  status: connected as which user, with which scopes.
- **Engine owns the flow and the token.** The device-code request, the poll
  loop (honoring `interval` / `authorization_pending` / `slow_down` / expiry),
  and token capture all run in the engine. The UI is a thin driver.
- **Secure token storage.** The token lives in the macOS Keychain (or
  equivalent), **never** in plaintext config and **never** in Boss runtime
  state (`state.db`) or any environment a worker can read.
- **Rewire issue sync** so the reconciler's GitHub calls use the stored OAuth
  token (REST + Projects v2 GraphQL) instead of ambient `gh`, with a defined
  migration/fallback for users currently relying on `gh`.
- **Surface org / SSO state.** When the OAuth App is not yet approved for the
  `spinyfin` org, or when SAML SSO authorization is required, the user is told
  exactly what org-owner / SSO action unblocks sync, and the flow recovers once
  it is taken.
- **Graceful failure handling** for network errors, denied/expired device
  codes, tokens rejected by org SSO, and revoked tokens — each with a distinct,
  actionable UI state.

## Non-Goals

- **A new settings area.** We extend the existing `ExternalTrackerSection`; we
  do not invent a separate "Accounts" or "Integrations" pane.
- **GitHub App / per-repo installation.** Rejected by T753 and re-confirmed
  here (see Alternatives). The `boss shake` GitHub App
  (`cli/src/github_app.rs`) that _creates_ issues is a separate identity and is
  untouched by this work.
- **Consolidating the two GitHub identities.** After this ships there are two:
  the `shake` GitHub App (issue creation) and the sync OAuth App (read / close
  / status). Whether to merge them is deferred (T753 §4 open decision 6).
- **Fine-grained / least-privilege scoping.** T753 established that an OAuth App
  device flow can only mint coarse classic-scoped tokens; `repo` is
  all-or-nothing. Narrowing below `repo project` is impossible on this path and
  is explicitly out of scope.
- **Multi-account / multi-host.** v1 stores **one** github.com user token. GitHub
  Enterprise hosts, or per-product distinct accounts, are out of scope (the
  token is host-scoped to github.com and shared across all GitHub-bound
  products).
- **Programmatic server-side token revocation.** Revoking a token via
  `DELETE /applications/{client_id}/token` requires the OAuth App **client
  secret**, which we will not ship in the app (see Provisioning). "Disconnect"
  deletes the local token; full server-side revocation is a documented
  user-driven step in GitHub settings.
- **Registering the OAuth App.** Creating the App in the `spinyfin` org,
  enabling device flow, and obtaining the `client_id` was a human/setup
  prerequisite handled outside this design (see §9 — completed as T767). No
  client secret is shipped.
- **Refresh-token rotation.** OAuth App user tokens are non-expiring by default
  (see Token lifecycle); there is no refresh token to rotate. Expiring-token
  support is a GitHub App feature we are not using.

---

## Background: constraints inherited from T753

T753 audited the six GitHub operations the reconciler performs and concluded:

- **Scope string to request:** `repo project` (baseline: private
  `spinyfin/mono` + org-owned Project, current feature set). `repo` covers all
  issue read/write/comment/label operations across any org repo that can appear
  on the board; `project` covers the Projects v2 read **and** the
  `set_project_status` mutation (Behavior 6). Narrower variants exist
  (`public_repo` if every surfaced repo is public; `read:project` if Behavior 6
  is dropped) but the baseline is `repo project`.
- **OAuth App, not GitHub App.** Device flow for an OAuth App issues a
  classic-scoped token. A GitHub App user token only works where the App is
  installed — the per-repo install we are avoiding.
- **No least-privilege is achievable** on this path. Classic `repo` is coarse;
  the cross-repo nature of the board actually requires that breadth.
- **Org approval is a hard gate.** Until a `spinyfin` owner approves the Boss
  OAuth App, the token reaches only the org's _public_ resources, which kills
  sync against a private repo + org-owned project. No OAuth-side workaround.
- **SAML SSO** (if `spinyfin` enforces it) requires the user to have an active
  SAML session when authorizing and to SSO-authorize the token; access can
  lapse and require re-authorization.

This design takes all of these as fixed and concerns itself with _how Boss
obtains, stores, and uses_ such a token, and _how the UI surfaces the org/SSO
prerequisites_.

---

## Alternatives Considered

### Alternative A — GitHub App with per-repo (or org-wide) installation

Register a **GitHub App**, install it on `spinyfin`, and use a GitHub App user
token (fine-grained permissions: `issues:write`, `projects:write`,
`contents:read`).

**Why not.** This is the path T753 explicitly rejected and the project brief
re-rejects. A GitHub App user token only works on repos/orgs where the App is
_installed_; we do not want per-repo installation friction, and a fine-grained
token cannot enumerate "every repo that can appear on the board" because the
board's repo set is unbounded and grows over time. (T753 notes an _org-wide_
install could in principle be least-privilege and cover the cross-repo need —
but it is still an installation model the project decided against, and it
overlaps awkwardly with the existing `shake` GitHub App identity.) Fine-grained
permissions are attractive for least-privilege, but the install requirement is
the disqualifier.

### Alternative B — Manual fine-grained PAT pasted into Settings

Have the user mint a fine-grained or classic PAT in GitHub's UI and paste it
into a text field in the issue-sync settings. Boss stores it in the keychain.

**Why not.** Three problems. (1) It pushes scope selection onto the user, who
will over- or under-scope it — the exact opacity this project exists to remove.
(2) It is a worse UX than device flow: copy-paste of a long secret vs. typing a
short user code into a browser already logged into GitHub. (3) Fine-grained PATs
on an org require org-owner _approval per token_ and expire on a fixed
schedule, reintroducing recurring manual toil. Device flow gives us a known,
fixed scope string we choose, and a browser-based consent that carries SSO.
(Note: the keychain _storage_ and the `TrackerCredentialResolver` plumbing
designed below are reusable if a "bring-your-own-PAT" escape hatch is ever
wanted — but it is not the primary path.)

### Alternative C — Keep ambient `gh`, or use the OAuth browser (web) flow

(C1) Status quo: keep relying on `gh auth status`. (C2) Use the standard OAuth
**web** flow (authorization-code with a redirect URI) instead of device flow.

**Why not C1.** It is precisely what the project replaces: invisible,
unenforced scopes; a hard dependency on a correctly-logged-in `gh` binary on
the user's machine; no in-app status or control. (C1 is, however, retained as
the _fallback_ path for un-migrated users — see Wiring §"Migration".)

**Why not C2.** The authorization-code web flow needs a redirect URI and, at
the token-exchange step, the OAuth App **client secret**. A desktop app cannot
keep a client secret confidential — embedding it in the shipped `.app` means
anyone can extract it. **Device flow is the purpose-built grant for clients
that cannot hold a secret:** the device-code → token exchange requires only the
public `client_id`, no secret, and no redirect URI / loopback HTTP server. It
also presents a clean "type this code in your browser" UX that naturally
carries org SSO authorization. This is the decisive reason device flow wins for
a distributed desktop app.

### Chosen: OAuth App + device authorization flow, engine-owned

The remainder of this document specifies it.

---

## Chosen Approach

### 1. Component ownership and end-to-end sequence

```
 ┌─────────────┐   FrontendRequest        ┌──────────────────────────────┐
 │  macOS app  │ ───────────────────────► │   engine (boss-engine)        │
 │  (thin UI)  │   GitHubAuthStart{}       │                               │
 │             │ ◄─────────────────────── │   github_oauth::DeviceFlow    │
 │ Settings →  │   FrontendEvent::         │     ├─ POST /login/device/code│
 │ External    │   GitHubAuthState{...}    │     ├─ poll /login/oauth/...  │
 │ Tracker     │   (pending→authorized→err)│     └─ KeychainTokenStore     │
 └─────────────┘                           └──────────────────────────────┘
        │                                              │
        │ user opens verification_uri,                 │ on success: store token
        │ types user_code in browser  ───────────────►│ in OS keychain; reconciler
        │ (carries org-approval + SSO)                 │ uses it via GH_TOKEN
        ▼                                              ▼
   github.com  ◄──────────── gh api (GH_TOKEN=<oauth>) ──────────── reconciler
```

The engine owns everything security-sensitive. The token never crosses back to
the app; the app only ever receives display-safe fields (`user_code`,
`verification_uri`, the connected login, the granted scopes, and error states).

### 2. Device-flow client (engine)

Shipped in PR #913 as `github_oauth.rs` (now in `boss_github_tracker`). It
uses `reqwest` directly — **not** `gh` — because the device-flow endpoints are
unauthenticated `github.com` endpoints, not `api.github.com`, and we want full
control over the `Accept: application/json` header and the poll timing.

**As built.** `DeviceFlow` takes injectable endpoint URLs so unit tests run
against `wiremock` (every poll branch, validation, and probe outcome is
covered without touching github.com). Poll-loop cancellation is a
`tokio::sync::watch` channel (what `GitHubAuthCancel` flips). The `client_id`
shipped as an in-tree constant rather than an injected `option_env!` — see §9.

**Step 1 — request device + user code.**

```
POST https://github.com/login/device/code
Accept: application/json
Body (form): client_id=<CLIENT_ID>&scope=repo%20project
→ 200 {
    "device_code":      "<opaque>",
    "user_code":        "WDJB-MJHT",
    "verification_uri": "https://github.com/login/device",
    "expires_in":       900,        // seconds (typically 15 min)
    "interval":         5           // min seconds between polls
  }
```

The engine returns `user_code`, `verification_uri`, `expires_in`, and `interval`
to the UI (via the `GitHubAuthState` event, state = `PendingUserAuth`). It keeps
`device_code` private (it is a bearer-equivalent secret for the poll step).

**Step 2 — present to user.** The UI shows the `user_code` and
`verification_uri` and offers "Open in browser" (which opens
`verification_uri` — GitHub then prompts the user to enter the code; some flows
support `verification_uri_complete` with the code pre-filled, which we use when
present). The browser step is where the user (a) logs into GitHub, (b) consents
to the requested scopes, (c) authorizes the App for the `spinyfin` org, and (d)
completes SAML SSO if enforced — all in one place.

**Step 3 — poll for the token.**

```
POST https://github.com/login/oauth/access_token
Accept: application/json
Body (form): client_id=<CLIENT_ID>
             &device_code=<device_code>
             &grant_type=urn:ietf:params:oauth:grant-type:device_code
```

Poll loop, sleeping `interval` seconds between attempts, handling the documented
error codes:

| Response                                                 | Action                                                                         |
| -------------------------------------------------------- | ------------------------------------------------------------------------------ |
| `access_token` present                                   | **Success.** Validate (Step 4), store, emit `Authorized`.                      |
| `error=authorization_pending`                            | User hasn't finished yet. Keep polling at current interval.                    |
| `error=slow_down`                                        | Increase interval by 5s (GitHub's required backoff) and continue.              |
| `error=expired_token`                                    | Device code expired (`expires_in` elapsed). Emit `Expired`; user must restart. |
| `error=access_denied`                                    | User clicked "Cancel" / denied. Emit `Denied`; stop.                           |
| `error=incorrect_device_code` / `unsupported_grant_type` | Programming/config error. Emit `Error`; log.                                   |
| HTTP 5xx / network error                                 | Transient. Retry on the next interval; do not abort the flow.                  |

The loop has a hard wall-clock cap at `expires_in` (plus a small grace) so it
cannot spin forever. A `GitHubAuthCancel` RPC aborts it early.

**Step 4 — validate the captured token.** Before declaring success, the engine
calls `GET https://api.github.com/user` with the token to capture the login,
and reads the `X-OAuth-Scopes` response header to record what was actually
granted (GitHub may grant fewer scopes than requested). It then runs the
**org/SSO probe** (§7) so the very first status the user sees already reflects
whether org approval / SSO is outstanding. Only after this does it persist and
emit `Authorized`.

### 3. Auth state machine

The engine tracks a single `GitHubAuthState` per host (github.com), persisted
in memory plus the durable token in the keychain (the _in-progress_ flow state
is intentionally **not** persisted — see Risk R4):

```
Disconnected
   └─(GitHubAuthStart)→ RequestingCode
        └─(device/code 200)→ PendingUserAuth { user_code, verification_uri, expires_at, interval }
             ├─(token 200 + validate)→ Authorized { login, scopes, org_state }
             ├─(expired_token)→ Expired ──(GitHubAuthStart)→ RequestingCode
             ├─(access_denied)→ Denied
             └─(GitHubAuthCancel)→ Disconnected
   Authorized
        ├─(GitHubAuthDisconnect)→ Disconnected  (delete keychain item)
        └─(org/SSO probe fails)→ Authorized { org_state: NeedsOrgApproval | NeedsSso }
```

`org_state` is a sub-state of `Authorized`: the token is valid for the _user_
but may not yet reach private `spinyfin` resources. This is what powers the
distinct UI messaging in §7.

**As built — no `Reauthorize` state.** The original design had a
`Reauthorize` state entered when sync hit a 401 (token revoked). That state
was never added to the protocol: the shipped `GitHubAuthStateDto` has exactly
the seven states above, and a sync-time 401 does **not** transition the auth
state machine. Instead the reconciler maps 401 to a dedicated
`TrackerError::TokenRevoked` and raises a distinct
`external_tracker_token_revoked` attention item prompting the user to
reconnect (PR #938). The keychain item is left in place and Settings continues
to show `Authorized` until the user disconnects or re-runs the flow — a known
gap flagged for follow-up (see §8).

**As built — state survives engine restart.** T-4 (PR #939) added
`restore_from_store()`: at engine boot the controller rehydrates
`Authorized { org_state: Unknown }` from the keychain, so connected status
survives restarts without re-auth. (The original design only said the
_in-progress_ flow is not persisted — that part is unchanged; see Risk R4.)

### 4. Engine ↔ App RPC additions

These follow the **exact** pattern already used by `SetProductExternalTracker`:
a `FrontendRequest` variant in `protocol/src/wire.rs`, an input struct in
`protocol/src/types.rs`, a handler arm in `engine/src/app.rs`, a `send*` method
in `app-macos/Sources/EngineClient.swift`, and a bridge method in
`app-macos/Sources/ChatViewModel.swift`.

New **requests** (app → engine):

```rust
// protocol/src/wire.rs — FrontendRequest variants
GitHubAuthStart      {}            // begin device flow (host = github.com)
GitHubAuthCancel     {}            // abort an in-progress flow
GitHubAuthDisconnect {}            // delete stored token, return to Disconnected
GitHubAuthStatus     {}            // request current state (engine replies with an event)
```

New **event** (engine → app). As built (PR #939) it is pushed on a dedicated
global topic, `TOPIC_GITHUB_AUTH` (`"github.auth"`, `protocol/src/wire.rs`),
which the app subscribes to — a small contract addition over the original
"same as `WorkItemUpdated`" sketch:

```rust
// protocol/src/wire.rs — FrontendEvent variant
GitHubAuthState { state: GitHubAuthStateDto }

// protocol/src/types.rs
pub enum GitHubAuthStateDto {
    Disconnected,
    RequestingCode,
    PendingUserAuth { user_code: String, verification_uri: String,
                      verification_uri_complete: Option<String>,
                      expires_at: i64, interval_seconds: u32 },
    Authorized { login: String, granted_scopes: Vec<String>,
                 org_state: OrgAuthState },
    Expired,
    Denied,
    Error { message: String },
}

pub enum OrgAuthState { Ok, NeedsOrgApproval { request_url: String },
                        NeedsSso { sso_url: String }, Unknown }
```

The DTO carries **only display-safe** fields. `device_code` and the access
token are never in any DTO. This is the boundary that satisfies "the UI never
sees the token."

**Why a pushed event rather than a reply.** The flow is long-lived (the user
takes seconds-to-minutes in the browser). Modeling it as request→reply would
force the UI to poll. Instead the UI fires `GitHubAuthStart` once and then
re-renders on each `GitHubAuthState` event the engine pushes as the poll loop
advances — identical in spirit to how work-item updates stream today. (The
RPC handlers also reply with the current state, so callers that _do_ want
request→reply semantics — the CLI, the UI's status-on-connect — get it.)

**As built — `GitHubAuthStatus` doubles as "Re-check."** When a token is
present, the status handler re-runs the org/SSO probe (§7) before replying.
This is how the org-approval / SSO banner clears without a full re-auth: the
UI's "Re-check" button, the UI's status request on connect, and
`boss github auth status` all route through it.

> **Note on the engine→app `EngineRequest` channel.** The engine-app-rpc design
> adds a separate `FrontendEvent::EngineRequest` / `FrontendRequest::EngineResponse`
> pair for engine-_initiated_ calls (pane spawning). We do **not** need it here
> for the auth flow itself (auth is app-initiated). It _is_ the mechanism we
> would use if we chose app-mediated keychain storage (see §5 alternative).

### 5. Token storage

**Requirements (from the brief):** secure storage; never plaintext config;
never in `state.db`; never readable by workers.

**How the constraints are met structurally.** The reconciler runs **inside the
engine process**, not inside worker (`claude`) processes. Workers are spawned
into libghostty panes with a specific env (`BOSS_LEASE_ID`,
`BOSS_EVENTS_SOCKET`, …) that does **not** include the token. The only process
that ever holds the token is the engine; the only place it is exposed to a
child is the `GH_TOKEN` env of the `gh` subprocesses the engine _itself_ spawns
for sync (§6) — those are children of the engine, never of a worker. So the
"not readable by workers" guarantee is a property of _where the token is read
and used_, enforced independently of the at-rest store.

**Chosen at-rest store: engine-owned OS keychain via the `keyring` crate.**
The `keyring` crate (v3, `apple-native` feature) is already vendored in this
workspace and proven by `tools/hood/src/creds.rs` (which stores a Robinhood
OAuth token in the macOS keychain). The engine writes a generic-password item:

```
service: "dev.spinyfin.boss.github"
account: "oauth-user-token@github.com"
value:   JSON { token, granted_scopes, login, obtained_at }
```

`KeychainTokenStore` (shipped in PR #917) wraps `keyring::Entry` with
`get` / `set` / `delete`, storing the `TokenRecord` (which gained serde
derives for the JSON round-trip) at exactly the service/account above. The
backend is abstracted behind a `KeystoreBackend` trait so tests inject a fake
store and never touch the real keychain in CI. The controller persists the
token to the keychain **before** broadcasting `Authorized` (PR #939), so a
reconcile tick firing immediately after auth already resolves it. This keeps
the entire flow
self-contained in the engine, with **no dependency on the app being connected**
to read the token at sync time, and matches the project's "engine owns the
token" directive and the `TrackerCredentialResolver` extension point that
`external-issue-tracker-sync` §11 already anticipated ("the PAT lives in the OS
keychain … resolved by a different `TrackerCredentialResolver` impl").

A new resolver replaces/augments `GhAuthStatusResolver`:

```rust
// external_tracker/credentials.rs
pub struct KeychainOAuthResolver { store: KeychainTokenStore, fallback: GhAuthStatusResolver }

impl TrackerCredentialResolver for KeychainOAuthResolver {
    async fn resolve(&self, kind, config) -> Result<TrackerCredential, _> {
        match self.store.get()? {
            Some(rec) => Ok(TrackerCredential { token: rec.token }),     // OAuth token wins
            None      => self.fallback.resolve(kind, config).await,      // ambient gh
        }
    }
}
```

`TrackerCredential.token` already exists and already means "non-empty = explicit
token, empty = ambient" — so the type does not change, only the resolver.
As built, keychain read errors are logged as warnings and treated as "no
stored token" — sync degrades to ambient `gh` rather than failing hard, and
non-GitHub tracker kinds delegate straight to the fallback resolver.

**Token lifecycle.**

- **Expiry / refresh.** OAuth **App** user tokens are **non-expiring** by default
  and carry **no refresh token**. (Expiring user tokens with refresh are a
  GitHub _App_ feature; we are an OAuth App and will leave expiration off.) So
  there is no refresh loop. "Re-auth" is simply re-running the device flow,
  which overwrites the stored token.
- **Revocation / disconnect.** `GitHubAuthDisconnect` deletes the keychain item
  and returns to `Disconnected`. Because we ship no client secret, Boss cannot
  call `DELETE /applications/{client_id}/token` to revoke server-side; the
  disconnect UI therefore also links to GitHub → Settings → Applications →
  Authorized OAuth Apps so a user who wants full server-side revocation can do
  it. (As built, a token detected as already-revoked during sync — 401 —
  raises an `external_tracker_token_revoked` attention item but does **not**
  clear the keychain item or change the auth state; see §3 and §8.)
- **Clearing on disconnect** is unconditional and local even if the network is
  down.

**Alternative storage considered — app-mediated `APIKeyStore`.** The app
already has `APIKeyStore` (`app-macos/Sources/Settings/APIKeyStore.swift`),
which stores the Anthropic API key in the **data-protection keychain**
(`kSecUseDataProtectionKeychain`, `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`)
gated by the app's `keychain-access-groups` entitlement, with a `0600` file
fallback for ad-hoc dev builds. We could store the OAuth token there and have
the engine fetch it over the `EngineRequest` channel. **Trade-off:** the
data-protection keychain is _more_ secure (device-only, entitlement-scoped,
unavailable to the unentitled engine binary), but it (a) crosses the
engine/app boundary for a token the brief says the engine should own, (b)
introduces an "app must be online" dependency for the engine to read the token
each sync, and (c) couples sync availability to the GUI. Given "engine owns the
token" + "engine reconciler must work whether or not the app is foregrounded,"
**engine-direct keychain is chosen**; app-mediated storage is the natural
upgrade if engine keychain access proves unreliable (see Risk R1) or if the
stronger data-protection guarantees become a requirement. **No plaintext file
fallback is offered for the OAuth token** — unlike the Anthropic key, a
`repo`-scoped GitHub token is write-capable and higher-value; in environments
without a usable keychain the resolver reports `Disconnected` and sync falls
back to ambient `gh` rather than writing the token to disk in plaintext.

### 6. Wiring into issue sync

The reconciler (`external_tracker/reconcile.rs`) already obtains a
`TrackerCredential` through a `TrackerCredentialResolver` and passes it in
`TrackerContext.credential`. Two changes thread the token to the wire:

1. **Resolver swap.** The engine constructs `KeychainOAuthResolver` (§5) instead
   of the bare `GhAuthStatusResolver`. When a stored OAuth token exists,
   `TrackerCredential.token` is the OAuth token; otherwise it is the ambient
   empty marker.

2. **`GhRunner` honors the token.** When `token` is non-empty, set
   `GH_TOKEN=<token>` on each `gh` invocation (`gh` honors `GH_TOKEN`). This is
   the minimal-blast-radius integration T753 recommended: the GraphQL and REST
   call sites are otherwise unchanged, and the scope analysis is identical
   whether calls go through `gh` or raw HTTP. As built (PR #938) the token is
   threaded as a **parameter** — all four `GhRunner` trait methods (`graphql` /
   `rest_get` / `rest_patch` / `rest_post`) take `token: Option<&str>`, and
   `CommandGhRunner` adds `.env("GH_TOKEN", t)` when present — rather than as a
   field on `CommandGhRunner` as originally sketched. Resolution is
   per-product: `run_one_pass` / `spawn_loop` take a `credential_resolver`
   parameter and resolve the credential before each product's `TrackerContext`
   is built; `app.rs` wires `KeychainOAuthResolver(KeychainTokenStore::new())`.
   Tests assert `GH_TOKEN` is set when a token is present and absent when
   ambient.

   > A future hardening (not v1) could drop `gh` for sync entirely and use
   > `reqwest` with an `Authorization: Bearer` header, removing the `gh` binary
   > dependency. Out of scope here; `GH_TOKEN` is the smaller, lower-risk step.

**Precedence and migration.**

- **Precedence:** a stored OAuth token always wins. If present, sync uses it; the
  ambient `gh` path is not consulted.
- **Migration for existing `gh` users:** until a user completes the device flow,
  `KeychainOAuthResolver` falls through to `GhAuthStatusResolver` and sync keeps
  working exactly as today. There is **no forced cutover** — existing users are
  unaffected until they click "Connect." The settings UI shows which mode is
  active ("Connected via OAuth as @user" vs. "Using local `gh` login").
- **After connecting:** the OAuth token takes precedence on the very next
  reconcile tick. Disconnecting reverts to the ambient `gh` fallback.
- The reconciler's existing failure classification (transient / permission /
  not-found) is reused, extended with a distinct 401 class: as built,
  `map_graphql_error` maps HTTP **401 → `TrackerError::TokenRevoked`** (revoked
  or expired OAuth token → `external_tracker_token_revoked` attention item)
  and HTTP **403 → `TrackerError::Auth`** (org-approval / SSO likely causes,
  named in the attention-item body). A successful fetch resolves all three
  stale auth attention kinds (`auth_failed`, `token_revoked`,
  `transient_errors`). The originally-planned 401 → `Reauthorize` auth-state
  flip did not ship (§3, §8).

### 7. Org / SSO handling

T753 established that a valid user token can still be inert against private
`spinyfin` resources until (a) an org owner approves the OAuth App and (b), if
SAML is enforced, the user SSO-authorizes the token. The design surfaces and
recovers from both:

**Detection (the org/SSO probe).** The engine issues a cheap probe against an
org-private resource — the bound product's `organization(login).projectV2`
GraphQL query (the same call `fetch_items` makes). As built
(`probe_and_record_org_state`, PR #939) the probe runs for **every
GitHub-bound product** (org read from each product's stored `GitHubConfig`,
probed once per distinct org), raises or resolves per-product
`github_oauth_org_unapproved` / `github_oauth_sso_required` attention items,
and aggregates the per-org results into the single `org_state` the UI shows.
It classifies each result:

- **Success** → `OrgAuthState::Ok`.
- **403 / 200-with-null-org** where the same token can read _public_ resources
  → `NeedsOrgApproval`. GitHub's UI exposes a "request access" / owner-approval
  page for the org; we link to
  `https://github.com/orgs/spinyfin/policies/applications` (owner) and surface
  the user-facing "request approval" affordance.
- **403 with an `X-GitHub-SSO: required; url=<...>` header** → `NeedsSso`,
  carrying the SSO authorization URL GitHub provides in that header. The user
  opens it, establishes a SAML session, and authorizes the token for the org.

**Recovery UX.** Each non-`Ok` state renders a distinct, actionable banner in
the issue-sync settings:

- _NeedsOrgApproval:_ "Connected as @user, but the Boss app is not yet approved
  for the **spinyfin** organization. An org owner must approve it before sync
  can read private issues. [Open org settings] [Re-check]."
- _NeedsSso:_ "Your token needs SAML SSO authorization for **spinyfin**.
  [Authorize via SSO] [Re-check]." (The button opens the `sso_url` from the
  header.)

"Re-check" re-runs the probe (via `GitHubAuthStatus`) without re-doing the
whole device flow. The probe also runs automatically on each freshly-
`Authorized` transition and when the UI requests status on connect.

**As built — no automatic re-probe from the reconciler.** The design said a
sync-time 403 would trigger a re-probe so the banner clears on its own. That
did not ship: the reconciler raises an `external_tracker_auth_failed`
attention item on 403 but does not call back into the auth controller. The
banner clears only when the user (or the CLI) requests status. This is a
follow-up gap, not a deliberate simplification.

### 8. Failure handling (consolidated)

| Failure                                                    | Where detected           | Engine behavior                                                                                          | UI state                             |
| ---------------------------------------------------------- | ------------------------ | -------------------------------------------------------------------------------------------------------- | ------------------------------------ |
| Network error on `device/code`                             | Step 1                   | Retry briefly, then abort flow                                                                           | `Error{message}` + retry affordance  |
| Network error / 5xx while polling                          | Step 3                   | Keep polling at interval; do not abort                                                                   | stays `PendingUserAuth`              |
| `slow_down`                                                | Step 3                   | interval += 5s                                                                                           | unchanged                            |
| `authorization_pending`                                    | Step 3                   | keep polling                                                                                             | `PendingUserAuth` (spinner)          |
| Device code expired                                        | Step 3                   | stop poll                                                                                                | `Expired` → "Start over"             |
| User denied in browser                                     | Step 3                   | stop poll                                                                                                | `Denied`                             |
| Granted scopes < requested                                 | Step 4                   | store anyway, record actual scopes                                                                       | `Authorized` + "limited scopes" note |
| Org not approved                                           | Step 4 / status re-check | `org_state=NeedsOrgApproval` + `github_oauth_org_unapproved` attention item                              | org-approval banner                  |
| SAML SSO required                                          | Step 4 / status re-check | `org_state=NeedsSso` (capture `sso_url`) + `github_oauth_sso_required` attention item                    | SSO banner                           |
| Token revoked / invalid                                    | sync 401                 | `TrackerError::TokenRevoked`; raise attention item (keychain item **not** cleared, auth state unchanged) | "Reconnect" prompt + attention item  |
| Permission denied on write (403, has scope but org policy) | sync (close/label)       | existing attention item; do not retry                                                                    | product attention item               |
| Keychain unavailable                                       | resolve                  | log; fall back to ambient `gh`                                                                           | "Using local gh login" + warning     |

All sync-time auth failures also raise a **`WorkAttentionItem`** on the affected
product (reusing the existing attention-item surface that
`ExternalTrackerAttentionTests` already covers), so the problem is visible even
if the user isn't in the settings sheet.

**Known gap after implementation.** A revoked token leaves the auth state
stuck at `Authorized` and the keychain item in place: the attention item is
the only signal, and Settings still reads "Connected as @user." Closing that
loop — either a `Reauthorize` state or having the reconciler clear the
keychain item and notify the controller — is deliberate follow-up work, not
part of what shipped.

### 9. OAuth App provisioning (human/setup task — completed)

**Status: steps (1)–(3) done (T767 / T-0).** The `spinyfin` OAuth App was
registered with device flow enabled, and the resulting **public** `client_id`
(`Ov23li9VOztDIjoOA7eW`) shipped in PR #913. Steps (4) and (5) — org-owner
approval and the SAML SSO authorization — are now documented as an operator
procedure in `tools/boss/docs/runbooks/github-oauth-org-sso.md` (PR #950)
rather than performed by this project; nothing in the merged PRs records that
the approval itself has been granted.

**As built — `client_id` is a plain in-tree constant**, not the
`option_env!("BOSS_GITHUB_OAUTH_CLIENT_ID")` compile-time injection step (3)
proposed. A device-flow `client_id` is public by construction and there is one
production App, so build-time injection bought nothing; the trade-off is that
pointing a build at a different OAuth App now requires a code edit rather than
an env var. Consequently the originally-planned "Connect button disabled when
the build has no `client_id`" affordance was not built — there is no
unconfigured state to represent.

The original prerequisite, retained for provenance:

1. **Register an OAuth App in the `spinyfin` org** (Settings → Developer
   settings → OAuth Apps → New). Name it (e.g. "Boss"), set a homepage URL. A
   callback URL is required by the form but unused by device flow (any valid URL
   is fine).
2. **Enable device flow** on the App ("Enable Device Flow" checkbox) — without
   it, `POST /login/device/code` is rejected.
3. **Provision the resulting `client_id` into the app.** The `client_id` is
   **public** (not a secret) and is the only credential the device flow needs.
   It should be supplied to the engine the same way other build-time identifiers
   are — e.g. a compile-time `option_env!("BOSS_GITHUB_OAUTH_CLIENT_ID")`
   constant (mirroring how `cli/src/github_app.rs` embeds
   `BOSS_SHAKE_APP_ID`) — or an engine config field. **No client secret is
   embedded** (device flow doesn't need one, and a desktop app can't keep one).
4. **Org owner approves the App for `spinyfin`** (OAuth App access policy) — the
   hard gate from T753. Document this in a runbook.
5. **Document the SAML SSO authorization step** if `spinyfin` enforces SSO.

---

## Implementation Task Breakdown (as shipped)

Each task was one PR; all seven merged on 2026-05-29. Ordering reflects
dependencies.

**T-0 (prerequisite, human/setup — not a code PR). Done as T767.** Registered
the `spinyfin` OAuth App, enabled device flow, obtained the `client_id`. The
org-approval + SSO runbook shipped with T-7 rather than T-0.

**T-1 — Protocol additions** (`boss-protocol`, PR #908). Add the `GitHubAuthStart`,
`GitHubAuthCancel`, `GitHubAuthDisconnect`, `GitHubAuthStatus` `FrontendRequest`
variants; the `GitHubAuthState` `FrontendEvent` variant; and the
`GitHubAuthStateDto` / `OrgAuthState` types with serde + round-trip tests.
Wire-format only; no behavior. _Depends on: none._

**T-2 — Device-flow client + state machine** (engine, PR #913). New
`external_tracker/github_oauth.rs`: `DeviceFlow` (device-code request, poll loop
honoring `interval`/`authorization_pending`/`slow_down`/expiry/`access_denied`),
token validation (`GET /user`, `X-OAuth-Scopes`), and the `GitHubAuthState`
machine. `client_id` read from an injected constant/config. Unit tests with a
mock HTTP server covering each poll branch. _Depends on: T-1._

**T-3 — Keychain token storage** (engine, PR #917). `KeychainTokenStore` over
`keyring::Entry` (service/account from §5); `KeychainOAuthResolver` that prefers
the stored token and falls back to `GhAuthStatusResolver`. Tests follow the
`hood`/`APIKeyStore` pattern (inject a fake backend; never touch the real
keychain in CI). _Depends on: T-2 (consumes the captured token); can develop in
parallel with T-2 against a stub._

**T-4 — Engine RPC handlers + auth-flow orchestration** (engine, `app.rs`, PR #939).
Handle the four new requests; own the single per-host `GitHubAuthState`; push
`GitHubAuthState` events as the flow advances; run the org/SSO probe; raise
attention items on auth failure. _Depends on: T-1, T-2, T-3._

**T-5 — Sync rewiring** (engine, PR #938, `external_tracker/github.rs` + reconciler
construction). Thread `TrackerCredential.token` into `CommandGhRunner` as
`GH_TOKEN`; swap in `KeychainOAuthResolver`; map sync-time 401 → `Reauthorize` +
attention item; map 403 → org/SSO re-probe. Tests assert `GH_TOKEN` is set when
a token is present and unset (ambient) when not. _Depends on: T-3._ _(Can land
before or after T-4; they touch different engine areas.)_

**T-6 — Settings UI** (`app-macos`, PR #951). Extend `ExternalTrackerSection` in
`ContentView.swift`: a "GitHub account" subsection with Connect / Disconnect /
Re-authorize, the `user_code` + verification URL display with "Open in
browser," polling/success/error/expired/denied states, the org-approval and SSO
banners, and a status line ("Connected as @user · scopes: repo, project" /
"Using local gh login"). Add `send*` methods to `EngineClient.swift` and bridge
methods to `ChatViewModel.swift`; handle the `GitHubAuthState` event. Swift
tests mirror `ExternalTrackerTests` (DTO decode, state rendering). _Depends on:
T-1 (wire types), T-4 (engine handlers)._

As built, the state→UI mapping was factored into a pure `GitHubAuthPresentation`
model in `Models.swift` (mirroring `ExternalTrackerAttentionPresentation`) so
every state's status line, action set, and banner is unit-testable without
rendering SwiftUI. The app subscribes to the global `github.auth` topic and
requests status on connect, so a keychain-persisted token restored at engine
boot shows as connected without waiting for a transition. The "Open in browser"
action prefers `verification_uri_complete` when GitHub supplies it.

**T-7 — `boss` CLI parity + runbook** (cli + docs, PR #950). `boss github auth
{login,status,logout}` verbs that drive the same engine RPCs (useful for
headless/testing), and the org-owner approval + SSO runbook under
`tools/boss/docs/runbooks/github-oauth-org-sso.md`. _Depends on: T-4._

As built, all three verbs support `--json`. `login` polls `GitHubAuthStatus`
to a terminal state rather than consuming the pushed event stream — the CLI is
request→reply where the UI is event-driven. Because `status` re-probes org
state (§4), it doubles as the headless "Re-check." End-to-end validation
against a live engine was deferred to integration testing.

Critical path: **T-1 → T-2 → T-3 → T-4 → T-6**. T-5 branches off T-3; T-7
branches off T-4. T-0 gates _acceptance_ (real end-to-end auth against
`spinyfin`) but not development.

---

## Risks / Open Questions

Each risk below carries the outcome the implementation produced.

- **R1 — Engine keychain access reliability.** The engine is a child of the
  app but is a distinct binary without the app's `keychain-access-groups`
  entitlement, so it uses the **login** keychain (generic password) via
  `keyring`, not the app's data-protection keychain. If the engine binary's
  code-signing identity / path changes between builds, macOS may prompt or deny
  on re-read. **Open question:** is engine-direct keychain access stable enough
  across our dev + Developer-ID builds, or should we adopt the app-mediated
  `APIKeyStore` route (§5 alternative) despite the app-online dependency?
  **Outcome:** engine-direct keychain shipped as designed (PR #917) and no
  reliability problem surfaced during implementation. Note this was never
  exercised against a Developer-ID build in this project, so the risk is
  unretired rather than disproven; the app-mediated `APIKeyStore` route remains
  the fallback if it bites.

- **R2 — `gh` vs raw HTTP for sync.** v1 keeps `gh` + `GH_TOKEN` for minimal
  blast radius. Does `gh` reliably prefer `GH_TOKEN` over an ambient `gh auth`
  login in all configurations (e.g. when `gh` has its own keyring entry)? If
  not, we may need `GH_TOKEN` + `GH_CONFIG_DIR` isolation, or to move sync to
  `reqwest`. **Outcome:** T-5 (PR #938) threaded `GH_TOKEN` and asserts in
  tests that it is set when a token is present and absent when ambient — but
  those are unit tests over the runner, not an empirical check that `gh`
  prefers `GH_TOKEN` over its own keyring entry. The precedence question this
  risk actually asks remains unverified against a real `gh` install.

- **R3 — Scope confirmation (carried from T753).** Baseline `repo project`
  assumes private `spinyfin/mono` + Behavior 6 on. If `mono` is public,
  `public_repo` suffices; if Behavior 6 is dropped, `read:project` suffices.
  Also re-confirm `read:org` is genuinely not required against the _real_ org
  project (T753 marked this high-confidence but not empirically verified).
  **Outcome:** the App was provisioned and the flow requests `repo project` as
  planned. `read:org` was not empirically re-confirmed against the real org
  project.

- **R4 — In-progress flow not persisted across engine restart.** If the engine
  restarts mid-flow (between device-code issuance and token capture), the
  in-progress state is lost and the user must click "Connect" again; the worst
  case is a dangling unused device code that expires harmlessly. **Decision:**
  acceptable for v1 (the durable thing — the token — _is_ persisted). Flag if a
  reviewer wants restart-survivable flow state. **Outcome:** unchanged as
  designed. T-4 additionally made the _authorized_ state restart-survivable via
  `restore_from_store()` (§3), which was not in the original plan; the
  in-progress flow state is still deliberately ephemeral.

- **R5 — Org approval is an existential dependency (T753 §4.4).** If the
  `spinyfin` owner will not approve a third-party OAuth App, this entire
  direction is blocked and we fall back to an org-wide GitHub App install or a
  member-minted SSO-authorized PAT. **Outcome:** partially retired. The App
  was registered (T767) and the whole stack shipped against it, but none of the
  merged PRs evidence that a `spinyfin` owner has granted org approval — T-7
  explicitly deferred live end-to-end validation to integration testing. The
  approval procedure is documented in
  [`../runbooks/github-oauth-org-sso.md`](../runbooks/github-oauth-org-sso.md);
  until it is performed and verified, sync against private `spinyfin`
  resources is unproven on this path.

- **R6 — Two GitHub identities.** `shake` (GitHub App, issue creation) and sync
  (OAuth App, read/close/status) will both touch the org/project. Keep them
  separate (chosen for v1: simpler, two consents) or consolidate later (T753 §4
  open decision 6)? **Outcome:** unchanged — the two identities shipped
  separately, as planned. Still noted for a future project.

- **R7 — No programmatic revocation.** Because we ship no client secret,
  "Disconnect" only deletes the local token; full server-side revocation is a
  user-driven step in GitHub settings. **Open question:** is local deletion +
  documented manual revoke acceptable, or do we need a tiny server-side
  component holding the client secret to offer one-click revoke? _v1 recommends
  local-only; confirm with reviewer._ **Outcome:** shipped local-only.
  `boss github auth logout` and the Settings "Disconnect" button delete the
  keychain item; server-side revocation remains a documented manual step in
  the runbook.

## References

- T753 investigation —
  [`oauth-device-flow-scopes-vs-issue-sync-2026-05-28.md`](../investigations/oauth-device-flow-scopes-vs-issue-sync-2026-05-28.md)
  (PR spinyfin/mono#897).
- [`external-issue-tracker-sync-github-projects.md`](external-issue-tracker-sync-github-projects.md)
  — the sync design this auth work plugs into (esp. §11 Credentials).
- [`engine-app-rpc.md`](engine-app-rpc.md) — the frontend-socket request/event
  pattern these RPC additions follow.
- Org-approval + SSO runbook —
  [`../runbooks/github-oauth-org-sso.md`](../runbooks/github-oauth-org-sso.md)
  (PR spinyfin/mono#950).
- Where the shipped code lives **today** (after the engine crate split):
  `github_tracker/src/{github_oauth.rs, github.rs, lib.rs}` (device flow,
  state machine, keychain store, org probe, `GhRunner`/`TrackerError`);
  `engine/core/src/app/{server.rs, github_auth.rs}` (orchestration, forwarder
  task, RPC handlers); `engine/core/src/external_tracker/{org_state_sink.rs,
reconcile/logic.rs}`; `protocol/src/{wire.rs, types/common.rs}`;
  `app-macos/Sources/{ContentView.swift, EngineClient.swift,
ChatViewModel+GitHub.swift, Models.swift}` with
  `app-macos/Tests/BossTests/GitHubAuthTests.swift`; `cli/src/main.rs`.
- Precedents followed: storage — `tools/hood/src/creds.rs` and
  `app-macos/Sources/Settings/APIKeyStore.swift`; provisioning —
  `cli/src/github_app.rs`.
- GitHub docs: [Authorizing OAuth apps — device flow](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#device-flow),
  [Scopes for OAuth apps](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/scopes-for-oauth-apps),
  [About OAuth app access restrictions](https://docs.github.com/en/organizations/managing-oauth-access-to-your-organizations-data/about-oauth-app-access-restrictions).
