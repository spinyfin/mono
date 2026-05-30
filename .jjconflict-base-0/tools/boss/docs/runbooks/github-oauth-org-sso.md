# GitHub OAuth: Org Approval and SAML/SSO Authorization

This runbook documents the steps an org owner (and individual users) must follow to allow the Boss OAuth App to access private `spinyfin` resources. It covers the initial org-level approval, SAML SSO token authorization, and how to verify that access is working.

See [`tools/boss/docs/designs/oauth-device-flow-auth-for-issue-sync.md`](../designs/oauth-device-flow-auth-for-issue-sync.md) for the architecture and design rationale (§7 org/SSO handling, §9 provisioning prerequisite).

---

## Background

Boss uses a GitHub OAuth App with the device flow to obtain a user token for issue sync. A valid token can still be blocked by two independent org-level gates:

1. **Org third-party OAuth app approval** — `spinyfin` may restrict which OAuth Apps members can authorize. If restrictions are enabled, an org owner must explicitly grant the Boss app access before any member's token can reach private org resources.
2. **SAML SSO authorization** — if `spinyfin` enforces SAML SSO, each individual OAuth token must be SSO-authorized before it can read private org data. This is a per-user, per-token action.

Both gates are surfaced as distinct states in the engine (`NeedsOrgApproval`, `NeedsSso`) and shown as actionable banners in the issue-sync settings UI and via `boss github auth status`.

---

## 1. Check whether org approval restrictions are enabled

1. Go to `https://github.com/organizations/spinyfin/settings/oauth_application_policy` (requires org owner access).
2. Look at the **Third-party application access policy** section.
   - If the policy shows **"No restrictions"**: all member-authorized OAuth Apps are automatically trusted. No per-app approval is needed — skip to §3.
   - If the policy shows **"Access restricted"**: the Boss app must be explicitly approved before member tokens can reach private org resources. Continue with §2.

---

## 2. Approve the Boss OAuth App for the org (owner action)

This is a one-time action per app. Once approved, all current and future member tokens for the Boss app gain org access automatically (subject to SAML SSO, §3).

### Option A — Proactive owner approval

1. Go to `https://github.com/organizations/spinyfin/settings/oauth_application_policy`.
2. Find the Boss OAuth App in the **Approved** or **Pending** list.
   - If it appears under **Pending requests**: click **Approve** next to it.
   - If it does not appear: a member must first connect via `boss github auth login` (or the Boss settings UI). This puts the app in the **Pending requests** queue. Then approve it.
3. Confirm approval.

### Option B — Approve after a member requests access

When a member authenticates via the Boss device flow and the org has access restrictions enabled, Boss detects the blocked state and surfaces a "Request access" link. The member submits the access request; the owner then:

1. Navigates to `https://github.com/organizations/spinyfin/settings/oauth_application_policy`.
2. Finds the Boss OAuth App under **Pending requests**.
3. Clicks **Approve**.

### Org-owned apps are auto-trusted

If the Boss OAuth App is registered under the `spinyfin` org (not a personal account), it is automatically trusted and §2 does not apply. Org-owned apps bypass the third-party access restriction.

### Validation

The definitive validation is a successful device-flow auth followed by a clean `boss github auth status` showing `Org access: OK`. A `403` on org-private project data (visible as `NeedsOrgApproval` in `boss github auth status`) means the org approval step is still missing.

---

## 3. SAML SSO authorization (per-user action)

If `spinyfin` enforces SAML SSO, each OAuth token must be authorized for the org after it is minted. This is separate from org approval and must be done by each individual user.

### How to SSO-authorize a token

1. Complete the Boss device flow: `boss github auth login` (or the Boss settings UI Connect button).
2. After `Authorized as @<user>` is printed, run `boss github auth status`.
   - If the output shows `Org access: needs SAML SSO authorization` with an `Authorize:` URL, copy that URL and open it in a browser.
   - If the output shows `Org access: OK`, no SSO action is needed.
3. In the browser, you will be taken to GitHub's SAML SSO authorization page for the `spinyfin` org. Click **Authorize**.
4. Run `boss github auth status` again to confirm `Org access: OK`.

### Finding the SSO URL manually

If you need to re-authorize a token later (e.g., after a SAML session expires or the token is re-minted):

1. Run `boss github auth status`. If SSO authorization is needed, the output includes the direct authorization URL.
2. Alternatively, open `https://github.com/settings/connections/applications/<client-id>` (substituting the Boss OAuth App's client ID), navigate to `spinyfin` in the **Organization access** section, and click **Authorize** next to `spinyfin`.

### When SSO authorization expires

GitHub SAML SSO authorization for an OAuth token does not expire on its own — it remains valid until:
- The token is revoked.
- The org SAML settings change in a way that invalidates existing authorizations.
- The user explicitly de-authorizes the app.

If sync starts returning 403 errors after working correctly, re-run `boss github auth status` to check whether SSO re-authorization is needed.

---

## 4. End-to-end verification

After completing §2 (if needed) and §3 (if needed), verify end-to-end access:

```sh
# Check auth state — both org approval and SSO should be resolved:
boss github auth status

# Expected output:
# Authorized as @<login>
# Scopes: repo, project
# Org access: OK

# Trigger an immediate external-tracker reconcile to confirm sync works:
boss product sync-external-tracker <product-id-or-slug>
```

A successful reconcile with no auth-related attention items confirms the full auth chain is working.

---

## 5. Troubleshooting

| Symptom | Likely cause | Action |
|---|---|---|
| `boss github auth status` → `Org access: needs org-owner approval` | Org has access restrictions and Boss app is not yet approved | Org owner follows §2 |
| `boss github auth status` → `Org access: needs SAML SSO authorization` | Token not SSO-authorized for `spinyfin` | User follows §3 |
| `boss github auth status` → `Org access: unknown (probe failed)` | Probe failed for an unexpected reason | Check network; retry; check GitHub status; file a bug if persistent |
| Sync returns 401 attention items | Token revoked or expired | Run `boss github auth login` to re-authenticate |
| Sync returns 403 attention items even after `Org access: OK` | Org policy for specific resources (e.g., write-back close) blocked | Check org's fine-grained repository permissions; the token has the scope but the org policy may block it |
| Device code expires during `boss github auth login` | User did not complete the browser step in time | Re-run `boss github auth login` |

---

## 6. CLI quick reference

```sh
# Start the device flow (open the printed URL, enter the code):
boss github auth login

# Check the stored token and org/SSO state:
boss github auth status

# Remove the stored token (reverts to ambient gh auth):
boss github auth logout

# Machine-readable output (all three verbs support --json):
boss github auth status --json
```

---

## 7. Related runbooks

- [External Tracker Sync](external-tracker-sync.md) — binding a GitHub project to a Boss product and triggering reconcile.
