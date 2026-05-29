# OAuth Device-Flow Scopes vs. Current Issue-Sync Needs

**Date:** 2026-05-28
**Parent project:** External issue tracker sync (GitHub Projects)
**Status:** Investigation writeup. No code changes. Output is the concrete OAuth scope set the device flow must request, the org-level prerequisites, and the open decisions for the implementing chore(s).

## TL;DR

The current GitHub issue-sync reconciler performs **six distinct GitHub operations** against `spinyfin/mono` and its org-owned Project: it reads the project + nested issue content (GraphQL), reads single issues (REST), closes issues (REST PATCH), reads + posts issue comments (REST), reads project metadata + mutates a project status field (GraphQL), and adds issue labels (REST). Three of those write.

Mapped to OAuth **classic** scopes, the union is **`repo` + `project`** for a private repo (or `public_repo` + `project` if every repo the project surfaces is public). There is **no standalone `issues` classic scope** — issue read/write/comments/labels on a private repo are only reachable through the coarse `repo` scope. The project status-field *mutation* forces the writable `project` scope rather than the read-only `read:project`.

An OAuth App device flow **can** technically perform every current operation: device flow issues classic-scoped OAuth user tokens, and both `repo` and `project` are requestable that way. There is **no operation that OAuth device flow cannot replicate**. But two non-scope constraints gate it:

1. **No least-privilege.** Classic `repo` is all-or-nothing across *all* of the consenting user's private repos and most repo subsystems; classic OAuth cannot express "issues only, one repo only." Only fine-grained tokens / GitHub Apps can, and **device flow issues neither** — OAuth Apps don't use fine-grained permissions.
2. **Org gating.** `spinyfin` (a new-ish org has OAuth App access restrictions **on by default**) must have an owner **approve the Boss OAuth App**, and — if the org enforces SAML — the user must **SSO-authorize** the token. Without approval the token sees only the org's *public* resources, which kills sync against a private repo + org-owned project. There is no pure-OAuth workaround for an owner who refuses to approve.

**Exact scope string to request:** `repo project` (baseline: private repo, current feature set). See [Conclusion](#4-conclusion) for the narrower variants.

---

## 1. Audit of the current issue-sync flow

### Where the code lives

The sync layer is the `external_tracker` module in the engine:

| File | Role |
|------|------|
| `tools/boss/engine/src/external_tracker/mod.rs` | The `ExternalTracker` trait, upstream data types, registry. |
| `tools/boss/engine/src/external_tracker/github.rs` | The `GitHubTracker` impl — **every** GitHub network call lives here. |
| `tools/boss/engine/src/external_tracker/reconcile.rs` | The reconciler loop. Touches GitHub **only** through the trait — it contains no direct `gh`/REST/GraphQL calls. |
| `tools/boss/engine/src/external_tracker/credentials.rs` | Credential resolution: `gh auth status`. |

This is important for scoping the audit: the entire GitHub API surface of the sync flow is the set of methods on `GitHubTracker`. The reconciler decides *when* to call them but adds no new endpoints.

### How calls are made today (credential + transport)

All network operations shell out to the locally-installed `gh` CLI via an internal `GhRunner` abstraction (`github.rs:76-243`):

- `gh api graphql -f query=<q> -F k=v` — GraphQL queries and mutations.
- `gh api <path>` — REST GET.
- `gh api -X PATCH <path> -f k=v` — REST PATCH.
- `gh api -X POST --input - <path>` — REST POST with a JSON body.

Credentials are **ambient**: `credentials.rs` runs `gh auth status --hostname github.com` once and, on success, every subsequent `gh api` call inherits the user's existing `gh` login (`TrackerCredential::ambient()` is an empty token — a placeholder). This is exactly the implicit reliance the project wants to replace: the sync flow's effective GitHub permissions are *whatever scopes the user's `gh auth login` happened to carry*, which is invisible and unenforced from Boss's side.

### Operation inventory

Every GitHub operation the sync flow performs, in the order the reconciler can trigger them:

| # | Trait method (`github.rs`) | Boss behavior | Transport | Endpoint / GraphQL root | R/W | Driven live by reconciler? |
|---|---|---|---|---|---|---|
| 1 | `fetch_items` | List/import/reconcile (every tick) | GraphQL query | `organization(login).projectV2(number).items` → per-item `content { ... on Issue { number, title, body, state, stateReason, url, repository.nameWithOwner, labels, assignees, closedByPullRequestsReferences, updatedAt } }` + `fieldValues` (ProjectV2 single-select "Status") | **Read** | Yes — `reconcile.rs:403` |
| 2 | `fetch_item` | Single-issue probe | REST GET | `repos/{org}/{repo}/issues/{n}` | **Read** | Defined; **not** wired into the live loop (only exercised by tests). Still part of the trait contract. |
| 3 | `close_issue` | Behavior 5 (PR-merge close, always on) and Behavior 3 (reverse-close, opt-in) | REST PATCH | `repos/{org}/{repo}/issues/{n}` with `state=closed`, `state_reason=completed\|not_planned` | **Write** | Yes — `reconcile.rs:622` |
| 4 | `post_closing_pr_comment` | Linkage comment after a close | REST **GET** then **POST** | GET `repos/{org}/{repo}/issues/{n}/comments` (idempotency scan), then POST the same path (`"Closed by <pr_url>"`) | **Read + Write** | Yes — `reconcile.rs:645` |
| 5 | `set_project_status` | Behavior 6 (task → active/Doing) | GraphQL query **+ mutation** | query `organization.projectV2.fields` (project node id, Status field id, option ids); mutation `updateProjectV2ItemFieldValue` | **Read + Write (project)** | Yes — `reconcile.rs:735` |
| 6 | `add_label` | Behavior 7 (stamp imported issues with the `tracked` label) | REST POST | `repos/{owner}/issues/{n}/labels` with `{"labels":["tracked"]}` | **Write** | Yes — `reconcile.rs:764` and `reconcile.rs:1041` |

Read operations: 1, 2, plus the GET halves of 4 and 5.
Write operations: 3, the POST half of 4, the mutation half of 5, and 6.

### Two details that materially affect scope breadth

- **The list query reads issue *content*, not just project structure.** Operation 1 is the "project-association query on issues" the brief calls out. The `projectV2.items` traversal yields project-board membership and the Status field, but it also dives into each item's `content` to pull the issue title, body, state, labels, assignees, and `closedByPullRequestsReferences`. Project membership and issue content sit behind **different permission gates** (see §2), so this one query straddles both the project scope and the repo scope.

- **The flow is cross-repo within the org.** `add_label` (`github.rs:929-938`) deliberately parses the repo out of `UpstreamRef.canonical_id` (`owner/repo#number`) rather than using the configured `repo`, with the comment: *"GitHub Projects items can reference issues across repos in the same org."* So the bound Project can surface — and the sync flow can write to — issues in **multiple repos** in `spinyfin`, not only the configured one. Any per-repo scoping model has to account for every repo that can appear on the board, not just `spinyfin/mono`.

### Out of scope: the merge-signal pipeline

Behavior 5 (PR-merge close) gates on "the linked PR has merged," which the reconciler learns from Boss's **existing** merge-detection pipeline (`merge_poller.rs`, `pr_url_capture.rs`, `completion.rs`) — those shell out to `gh pr view` / `gh pr list` against **Boss's own PRs in Boss's own repos**, using the worker/engine-local `gh`. That is a **separate credential surface** from the issue-sync token and is **not** part of this audit: the sync token never calls `gh pr ...`. PR-merge state arrives at the reconciler as already-resolved internal state, and PR *associations* on the upstream issue come from the `closedByPullRequestsReferences` field already covered by Operation 1. Conclusion: the merge pipeline adds **no** scope requirements to the OAuth sync token.

### Cross-reference: the `boss shake` GitHub App (a different identity)

`tools/boss/cli/src/github_app.rs` already authenticates as a **GitHub App** (sign a JWT with the App's RSA key → exchange for an installation access token) to *create* issues against `spinyfin/mono` and add them to the Project (`addProjectV2ItemById`). This is the "GitHub App path" the project has decided **against** for sync. It is a distinct identity and credential from the sync flow and is unaffected by this work — but note that after this project ships there will be **two** GitHub identities touching the same org/project (a GitHub App for `shake`'s issue *creation*, an OAuth App for sync's read/close/status). Whether to consolidate is an open decision (§4).

---

## 2. Mapping each operation to the GitHub permission it requires

GitHub OAuth **classic** scopes (the only kind an OAuth App / device flow can issue — see §3) and what each operation needs. Verbatim scope descriptions are from GitHub's [Scopes for OAuth apps](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/scopes-for-oauth-apps).

| Operation | Required classic scope (private repo) | If repo is public | Org dimension |
|---|---|---|---|
| 1 — `fetch_items` (project + issue content) | `read:project` **and** `repo` | `read:project` + `public_repo` (or no repo scope for purely public read) | **Yes** — org-owned `projectV2`; token must be authorized for `spinyfin` |
| 2 — `fetch_item` (REST issue read) | `repo` | none / `public_repo` | repo-level only |
| 3 — `close_issue` (REST PATCH state) | `repo` | `public_repo` | repo-level only |
| 4 — `post_closing_pr_comment` (read + post comment) | `repo` | `public_repo` | repo-level only |
| 5 — `set_project_status` (projectV2 mutation) | **`project`** (read+write) | `project` | **Yes** — org-owned `projectV2` mutation; token must be authorized for `spinyfin` |
| 6 — `add_label` (REST add label) | `repo` | `public_repo` | repo-level only (across any org repo on the board) |

### Why these scopes, precisely

- **There is no `issues` classic scope.** GitHub's OAuth-classic model does not expose a standalone issues permission (that granularity exists only for fine-grained tokens / GitHub Apps, e.g. `issues:read`/`issues:write`). For an OAuth App, read/write of issues, issue comments, and labels on a **private** repo is reachable **only** through `repo` ("full access to public and private repositories"). On a **public** repo, `public_repo` ("read/write access to code, commit statuses, repository projects, collaborators…") covers issue writes, and unauthenticated/no-scope read covers public issue reads. This means Operations 2, 3, 4, and 6 all collapse to a single requirement: **`repo`** (private) or **`public_repo`** (public).

- **Reading the project needs `read:project`; mutating it needs `project`.** GitHub's [Projects v2 API guide](https://docs.github.com/en/issues/planning-and-tracking-with-projects/automating-your-project/using-the-api-to-manage-projects) states a token needs *"the `read:project` scope (for queries) or `project` scope (for queries and mutations)."* Operation 1 only queries the project, so `read:project` would suffice **for the project part of it**. But Operation 5 (`set_project_status`) runs `updateProjectV2ItemFieldValue`, a **mutation** — that forces the writable **`project`** scope. Since `project` is a superset of `read:project`, the union needs only `project`.

- **The list query needs `repo` *in addition to* the project scope.** The project scope grants access to the board's structure and items, but the nested **issue content** (title/body/labels/assignees, and `closedByPullRequestsReferences`, which reads PR data) lives behind repo access. A token holding `project` but not `repo` would be able to enumerate the board yet get null/inaccessible `content` for private-repo issues. So Operation 1 genuinely requires **both** `project` (or `read:project`) **and** `repo`. (This mirrors the documented GitHub-App case where a projectV2 mutation that references a repo also needs that repo's `Contents` permission — project structure and repo content are separately gated.)

- **Org-owned Projects v2 add an org dimension, but it is *authorization*, not an extra scope.** Operations 1 and 5 hit `organization(login: "spinyfin").projectV2`. The scope that unlocks org projects is still `read:project`/`project` (their descriptions read *"user and organization projects"*). What the org adds is **third-party-app authorization**: per [About OAuth app access restrictions](https://docs.github.com/en/organizations/managing-oauth-access-to-your-organizations-data/about-oauth-app-access-restrictions), if the org has OAuth App access restrictions enabled, *"if the organization does not approve the application, then the application will only be able to access the organization's public resources."* So the scope string is the same, but the token is inert against private org resources until an owner approves the app (§3).

### Scopes the current flow does **not** need

- **`read:org`** — its grant is "org membership, organization projects [v1], team membership." The sync flow reads no membership or teams, and Projects **v2** is covered by `read:project`/`project`, **not** `read:org`. Assessed as **not required**. (Low-confidence caveat in §4: org-owned projectV2 + OAuth scope interactions are thinly documented; verify empirically before finalizing.)
- **`read:user` / `user`** — no profile reads. Not needed.
- **`repo:status`, `workflow`, `gist`, `notifications`, etc.** — not touched.

---

## 3. Intersection: needed vs. possible via OAuth device flow

### Device flow issues classic-scoped OAuth tokens

Confirmed against GitHub docs:

- **OAuth Apps use scopes, not fine-grained permissions.** *"Unlike a traditional OAuth token, a user access token from a GitHub App does not use scopes. Instead, it uses fine-grained permissions."* The corollary: an **OAuth App** token is classic-scoped. Device flow is an OAuth App (or GitHub App) authorization grant; for an **OAuth App** it yields a **classic-scoped** token. Device flow **cannot** mint a fine-grained token for an OAuth App.
- **Device flow must be explicitly enabled** in the app's settings, and the device-code request takes a **`scope`** parameter: *"A space-delimited list of the scopes that your app is requesting access to"* — ordinary classic scopes. (See [Authorizing OAuth apps](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps).)

### Can device flow do everything the current flow does? Yes — at the protocol level.

Both `repo` and `project` are valid classic scopes requestable via device flow. With those two scopes consented, an OAuth user token can perform **all six** operations: the GraphQL project read + issue content, the single-issue REST read, the issue close, the comment read/post, the project status mutation, and the label add. **No current operation is impossible under OAuth device flow.** The flow does not need any capability that classic scopes can't express *in terms of raw API access*.

The catch is everything *around* that raw access:

### Gap 1 — No least-privilege (inherent, unavoidable on this path)

Classic `repo` grants the token **read/write to every private repository the consenting user can access**, plus code, deployments, webhooks, collaborators, and more — far beyond "close issues on `spinyfin/mono`." There is **no classic scope** that says "issues only" or "this one repo only." The cross-repo behavior (§1) actually *needs* this breadth (the board can surface issues from many org repos), but the cost is that the user consents to a very broad grant. The only mechanisms that *could* narrow this — fine-grained PATs (per-repo, `issues:write` + `projects:write`) or a GitHub App with repo-scoped installation permissions — are **exactly the two things device flow for an OAuth App cannot produce**. This is a structural tension with the project's stated motivation: the OAuth-App path was chosen to avoid per-repo *App installation*, and the price of that choice is a coarse, non-least-privilege token.

### Gap 2 — Org approval is a hard prerequisite (can block entirely)

`spinyfin` is an organization, and `mono` is (almost certainly) a **private** repo with an **org-owned** Project. New orgs have OAuth App access restrictions **on by default**. Until an **org owner approves the Boss OAuth App** for `spinyfin`, a device-flow token — no matter what scopes it holds — can reach only the org's **public** resources. For a private repo + org project that means: **sync is dead until approval**. This is not a scope problem and has no OAuth-side workaround; it is an organizational action an owner must take. If an owner refuses, the fallbacks are all unattractive: (a) make repo/project public — not viable; (b) fall back to a GitHub App installed org-wide on "all repositories" — the path the project rejected, though note an *org-wide* install would actually satisfy the cross-repo need and could be least-privilege; (c) a classic PAT minted by an authorized org member — operationally the same opacity as today's ambient `gh`, defeating the in-app goal.

### Gap 3 — SAML SSO authorization (recurring friction, not a hard block if SSO is configured)

If `spinyfin` enforces SAML SSO, then per GitHub: *"you must have an active SAML session for each organization each time you authorize an OAuth app,"* and to access org resources via the app you *"must set up an active SSO session and then authorize the app."* The device flow's browser verification step (where the user enters the user-code) **can** carry the SSO authorization, but: (a) it adds a step, and (b) the token's org access can lapse and require re-authorization. This is survivable but must be designed for (clear re-auth UX, attention item when access lapses).

### Gap 4 — None at the operation level

To restate plainly for the brief: **there is no current operation that an OAuth classic scope cannot express.** Every gap above is about *granularity* (Gap 1) or *org policy* (Gaps 2–3), not about a missing capability. The fallback discussion (Gap 2) is the only place where "cannot be replicated" could arise — and even then it's contingent on an org owner declining approval, not on OAuth being incapable.

### Concrete minimal scope set

For the current feature set against a **private** `spinyfin/mono` + org-owned Project:

```
repo project
```

- `repo` — Operations 1 (issue content), 2, 3, 4, 6 (all issue read/write/comment/label, across any org repo on the board).
- `project` — Operations 1 (project read) and 5 (project status **mutation**).

Narrower variants (decision levers, see §4):

- **If every repo the project can surface is public:** `public_repo project`.
- **If Behavior 6 (`set_project_status`) is dropped:** `repo read:project` — the read-only project scope suffices once no mutation remains. (`set_project_status` is the *only* thing forcing the writable `project` scope.)
- **If both:** `public_repo read:project`.

---

## 4. Conclusion

### Exact scope string to request in the device flow

```
repo project
```

(Baseline: private `spinyfin/mono`, current feature set including Behavior 6 board-status mirroring. Request scopes as the space-delimited `scope` parameter on the device-code request.)

Substitute `public_repo` for `repo` only after confirming **every** repo whose issues can land on the bound Project is public; substitute `read:project` for `project` only if Behavior 6 (project status mutation) is removed.

### Org-level prerequisites the user / org owner must satisfy

1. **Enable device flow** on the Boss OAuth App (app settings checkbox) — without it the device-code request is rejected.
2. **Org owner approves the Boss OAuth App for `spinyfin`.** Mandatory if OAuth App access restrictions are enabled (default). Without approval the token reaches only public org resources and sync against the private repo + org project fails. This is a hard gate.
3. **SAML SSO authorization** (if `spinyfin` enforces SAML): the user must have an active SAML session when authorizing, and the token must be SSO-authorized for `spinyfin`. Plan for re-authorization when the session lapses.

### Open risks / decisions for the implementing chore(s)

1. **Confirm `spinyfin/mono` visibility (private vs public).** Drives `repo` vs `public_repo`. Baseline assumption here: **private** (the `boss shake` code notes a "corporate environment"; org-owned project). Verify before finalizing the scope string.
2. **Decide whether to keep Behavior 6 (`set_project_status`).** It is the sole reason the token needs the writable `project` scope instead of `read:project`. If board-status mirroring is dropped or deferred, the project-side grant narrows to read-only.
3. **Cross-repo breadth is real.** The bound Project can surface (and the flow writes labels to) issues across **multiple** `spinyfin` repos. `repo` covers this automatically; any future attempt to narrow via fine-grained tokens would have to enumerate every repo that can appear on the board — which is unbounded as the board grows. This is a strong argument that the coarse `repo` scope is effectively required for this design.
4. **Org approval + SSO are existential prerequisites, not nice-to-haves.** If the `spinyfin` owner won't approve a third-party OAuth App, the entire OAuth-device-flow direction is blocked and the fallback (org-wide GitHub App install, or a member-minted SSO-authorized PAT) must be revisited. Confirm `spinyfin`'s OAuth App access policy and SSO posture **before** investing in device-flow implementation.
5. **`read:org` is assessed as NOT required** (no membership/team reads; projectV2 covered by the project scope). Confidence is high but not absolute — GitHub documents org-owned projectV2 + OAuth scope interactions thinly. Validate empirically (a real token with `repo project` against the org project) before locking the scope string.
6. **Two GitHub identities.** `boss shake` uses a GitHub App (JWT → installation token) to *create* issues on the same org/project; sync would use an OAuth App. Decide whether to keep them separate (simpler, two consents) or consolidate onto one identity. (Note: a single org-wide GitHub App install could, in principle, serve both *and* be least-privilege — worth weighing against the "no per-repo install" rationale, since org-wide ≠ per-repo.)
7. **`read:project` vs `project` re-confirm after any feature change.** Tie the requested scope to the live operation set; if mutations are added or removed, revisit.

### Follow-up code changes (out of scope for this investigation — file separately)

Per the investigation constraint, no code was changed here. The implementing chores are:

- Register/create the Boss **OAuth App** in `spinyfin`; enable device flow; record client id.
- Implement the device-flow handshake (device-code request → poll for token → store) in the engine/CLI. Store the resulting token in the OS keychain (matches the "future PAT-in-keychain" resolver described in the design doc's Q11), replacing/augmenting `GhAuthStatusResolver`.
- Thread the token into `GhRunner`. Lowest-friction integration: keep the existing `gh api` shellouts but pass the OAuth token via the `GH_TOKEN` environment variable (gh honors it), so `github.rs` barely changes. The scope analysis here is identical whether calls go through `gh` or a raw HTTP client.
- Surface an attention item on auth/approval/SSO failure (the design's T11 hook) distinguishing "not authenticated," "app not approved for org," and "SSO authorization required."
- Org-owner runbook: approve the OAuth App for `spinyfin` and (if applicable) document the SSO-authorization step.

---

## Sources

- [Scopes for OAuth apps — GitHub Docs](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/scopes-for-oauth-apps)
- [Authorizing OAuth apps (device flow, scope parameter, enablement) — GitHub Docs](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps)
- [Using the API to manage Projects (read:project vs project for projectV2) — GitHub Docs](https://docs.github.com/en/issues/planning-and-tracking-with-projects/automating-your-project/using-the-api-to-manage-projects)
- [About OAuth app access restrictions (org approval, public-only access without it) — GitHub Docs](https://docs.github.com/en/organizations/managing-oauth-access-to-your-organizations-data/about-oauth-app-access-restrictions)
- [Generating a user access token for a GitHub App (OAuth tokens use scopes; GitHub App tokens use fine-grained permissions) — GitHub Docs](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-user-access-token-for-a-github-app)
- Code audited: `tools/boss/engine/src/external_tracker/{github.rs, mod.rs, reconcile.rs, credentials.rs}`; cross-reference `tools/boss/cli/src/github_app.rs`; parent design `tools/boss/docs/designs/external-issue-tracker-sync-github-projects.md`.
