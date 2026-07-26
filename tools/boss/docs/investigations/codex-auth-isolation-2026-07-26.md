# Codex auth isolation and refresh contract

- **Date:** 2026-07-26
- **Work item:** Codex authentication isolation and refresh contract
- **Pinned version:** `codex-cli 0.145.0`
- **Related:** design `codex-as-a-first-class-agent-driver` (PR #2285); crate `tools/boss/codex_auth`

## Question

How do concurrent per-run `CODEX_HOME` directories obtain authentication without sharing mutable credential state? Does Codex refresh or rewrite `auth.json`? What policy is safe for Boss?

## Method

Live probes against `codex-cli 0.145.0` on this host. Scratch git repos + scratch `CODEX_HOME` trees. Credentials were never printed: probes recorded only SHA-256 fingerprints of `auth.json`, `last_refresh` timestamps, JWT `exp` claims, and redacted event types. Host operator auth was restored after forced-refresh experiments that rotated tokens.

## Findings

### F1 — Auth is a file inside `CODEX_HOME`

`codex doctor` reports file-mode auth at `$CODEX_HOME/auth.json`. Observed shape (keys only):

- `OPENAI_API_KEY` (null when using ChatGPT OAuth)
- `tokens.{id_token,access_token,refresh_token,account_id}`
- `last_refresh` (ISO-8601)

No env-var credential handoff is required for ChatGPT OAuth mode.

### F2 — Codex **does** rewrite `auth.json` on token refresh

| Setup                                                         | Force expired `access_token`, keep real `refresh_token` | Result                                                                                                                                |
| ------------------------------------------------------------- | ------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| Writable **byte copy** of `auth.json` in scratch `CODEX_HOME` | Yes                                                     | Run succeeded (`rc=0`). Per-run `auth.json` fingerprint changed; `last_refresh` advanced; host auth fingerprint **unchanged**.        |
| Same, but `auth.json` mode `0444` (read-only)                 | Yes                                                     | Run failed. Logs: `Failed to refresh token: Permission denied (os error 13)`. Fingerprint unchanged. Stream errors then auth failure. |
| Valid (non-expired) access token on a writable copy           | N/A                                                     | Successful short `codex exec` **did not** rewrite `auth.json` (no refresh needed).                                                    |

Conclusion: refresh is real, persists to `$CODEX_HOME/auth.json`, and **requires a writable regular file**.

### F3 — Symlink / shared mutable `auth.json` is not a safe concurrent policy

The design spike used `ln -sf ~/.codex/auth.json "$CH/auth.json"`. That:

1. Shares one mutable credential file across every worker that points at it.
2. Couples Boss workers to the operator's interactive auth file.
3. Was not validated under concurrent refresh.

Probe (two concurrent `CODEX_HOME`s both symlinked to one controlled canonical `auth.json` with expired access tokens) produced refresh failures (`refresh token has already been used` / races). Host interactive auth was never used as the symlink target in failure cases.

**Rejected for Boss:** untested symlink to the operator interactive auth file as the per-run auth materialisation strategy.

### F4 — Independent writable snapshots isolate mutations

Two concurrent scratch homes, each with an **independent byte copy** of the same expired-access snapshot:

- Both runs completed (`rc=0`).
- Each rewrote its own `auth.json` with a **distinct** fingerprint / refresh-token hash.
- Host auth fingerprint stayed unchanged during the runs.

So per-run copies contain refresh side-effects. Without an adoption step, rotated tokens stay trapped in the run home and are discarded on teardown — future runs that re-snapshot a stale source can then fail to refresh.

### F5 — Read-only "immutable file" is not a supported run policy

Making the per-run file immutable/`0444` prevents the rewrite Codex needs on refresh. "Immutable" in the Boss policy means **immutable source snapshot at provision time** (a point-in-time byte copy), not a chmod-locked run file.

## Supported policy

Implemented in `boss-codex-auth` (`tools/boss/codex_auth`):

**`SnapshotWithRefreshAdoption`**

1. **Source** must be a **regular file** (symlink sources refused). Prefer a Boss-managed source path; operator `~/.codex/auth.json` may be the initial source but is snapshotted, not shared live.
2. **Provision:** exclusive file lock beside the source → validate JSON structure (no token logging) → byte-copy into `$CODEX_HOME/auth.json` mode `0600` as a regular file (replace any pre-existing symlink destination).
3. **Run:** Codex may rewrite the per-run file on refresh; mutations stay local.
4. **Teardown:** exclusive lock → if per-run fingerprint differs and `last_refresh` is newer than the source's, atomically replace the source with the run-local bytes (adopt rotation).
5. **Logging:** paths, policy name, fingerprints (`sha256` short), `last_refresh` only — never token/API-key material.

### Explicit non-policies

| Approach                                                    | Status                        |
| ----------------------------------------------------------- | ----------------------------- |
| Symlink per-run `auth.json` → operator `~/.codex/auth.json` | **Rejected**                  |
| Read-only / `chflags uchg` per-run `auth.json`              | **Rejected** (breaks refresh) |
| Logging full `auth.json` or token values                    | **Forbidden**                 |

## Implications for `CodexDriver` (T-11)

T-11's design text currently says "`auth.json` symlink". That must be updated to call `boss_codex_auth::snapshot_auth_into_codex_home` at provision and `adopt_refresh_if_newer` from teardown (via `DriverRuntimeState` carrying source path + provision fingerprint). This investigation crate is the reusable unit; the driver wires it later.

## Reproduction (no secret output)

```sh
# Writable copy + force refresh (uses operator refresh_token once — restore host after)
CH=$(mktemp -d)
cp ~/.codex/auth.json "$CH/auth.json"   # then expire access_token in the COPY only
# run: CODEX_HOME=$CH codex exec --json --skip-git-repo-check \
#   --dangerously-bypass-approvals-and-sandbox 'reply with exactly: pong' < /dev/null
# compare sha256 of $CH/auth.json before/after — expect change on expiry path
```

Automated coverage lives in `//tools/boss/codex_auth:codex_auth_test` (synthetic auth JSON only; no live Codex, no real credentials).
