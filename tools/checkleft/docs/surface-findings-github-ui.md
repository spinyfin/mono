# Surfacing checkleft findings in the GitHub UI

By default `checkleft run` reports findings as human-readable text on stdout and
exits non-zero when any finding is an error. In CI those findings are only
visible to someone who opens the build log.

The `--annotations` flag activates one or more **annotation backends** that
surface the same findings directly in the GitHub UI — as inline annotations on
the PR diff, a checkleft-named entry in the Checks tab, or alerts in the
Security / code-scanning tab. Annotation backends are a side output; the normal
stdout rendering and exit-code semantics are unchanged.

## Flag reference

| Flag                       | Description                                                                                                                                                                    |
| -------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `--annotations=<mode>`     | Enable an annotation backend. Repeatable. Supported values: `gha`, `check-run`, `sarif`, `none`. Default: none (off).                                                          |
| `--annotations-out=<path>` | Output file path for `--annotations=sarif`. Required when `sarif` is active.                                                                                                   |
| `--annotations-strict`     | Make annotation-posting failures fatal instead of logged warnings. Off by default — a posting failure never turns a clean run red, nor masks a dirty one.                      |
| `--upload`                 | Upload SARIF findings to GitHub code scanning after the run. Can be combined with `--annotations=sarif --annotations-out` to also write SARIF to a file. Non-fatal by default. |

Multiple backends can be active at once: `--annotations=gha --annotations=sarif`.

---

## Backend 1 — GHA workflow commands (`--annotations=gha`)

**For GitHub Actions.** Prints `::error::` / `::warning::` / `::notice::` lines
to stderr; the GHA runner converts these into inline annotations on the PR diff
and in the job annotation summary. Zero credentials required — the runner parses
them from output with no token. Self-disables automatically when not running
under GitHub Actions, so it is safe to pass unconditionally.

**GitHub UI surface:** inline annotations on the "Files changed" diff + job
annotation summary. No separate Checks-tab entry; annotations are attributed to
the running workflow's check.

**Buildkite:** not applicable — `::error::` lines are GHA-only and are silently
suppressed on non-GHA platforms.

### GHA recipe

```yaml
# .github/workflows/checks.yml
jobs:
  checkleft:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Run checkleft
        run: checkleft run --annotations=gha
```

No permissions, no secrets, no checked-in files needed.

### Notes

- GitHub imposes a per-step annotation cap (historically around 10 per severity
  level per step). When checkleft exceeds the cap it logs a warning naming how
  many findings were dropped, so the truncation is never silent.
- Annotations are attributed to the workflow's own check, not a separate
  "checkleft" check. Use `--annotations=check-run` if a distinct checkleft check
  in the Checks tab is needed.

---

## Backend 2 — GitHub Check Runs API (`--annotations=check-run`)

**For GitHub Actions and Buildkite.** checkleft calls the GitHub Check Runs REST
API to create a dedicated **"checkleft" check run** against the head commit and
attach inline PR-diff annotations to it. This is the only backend that creates
an independently-named checkleft check in the Checks tab — visible separately
from the CI provider's own jobs.

**GitHub UI surface:** a "checkleft" entry in the **Checks tab** (own name, own
conclusion, summary of error/warning counts) + inline annotations on the
"Files changed" diff + the annotations list on the check page.

Annotation batches are ≤ 50 per request (GitHub's limit); checkleft issues as
many `PATCH` requests as needed to post all findings, so no findings are dropped.

### Credential model

The token must have **`Checks: write`** permission.

| CI                   | Recommended                                                | Notes                                                                                                                            |
| -------------------- | ---------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| GitHub Actions       | Ambient `GITHUB_TOKEN` (with `permissions: checks: write`) | No extra secrets                                                                                                                 |
| Buildkite            | Fine-grained PAT with `Checks: write`                      | Lighter to provision than a GitHub App; stored as a Buildkite secret                                                             |
| Buildkite (fallback) | GitHub App installation token                              | For environments where PAT provisioning is not possible; see [GitHub App setup](#github-app-setup-check-runs-buildkite-fallback) |

Token resolution order (first found wins):

1. `CHECKS_GITHUB_TOKEN` — highest priority; use this in CI pipelines
2. `GH_TOKEN`
3. `GITHUB_TOKEN`
4. Output of `gh auth token` (local dev fallback)
5. GitHub App installation token — minted from `CHECKS_GITHUB_APP_ID` +
   `CHECKS_GITHUB_APP_PRIVATE_KEY` (see below)

If none of the above are available checkleft logs an actionable error and, by
default, continues with the content-driven exit code unchanged.

### GHA recipe

```yaml
# .github/workflows/checks.yml
jobs:
  checkleft:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      checks: write
    steps:
      - uses: actions/checkout@v4
      - name: Run checkleft
        run: checkleft run --annotations=check-run
        env:
          CHECKS_GITHUB_TOKEN: ${{ github.token }}
```

No checked-in files. The ambient `github.token` is automatically a GitHub App
installation token with `checks: write` when granted above.

### Buildkite recipe — recommended (fine-grained PAT)

Store a fine-grained PAT with **`Checks: write`** as a Buildkite secret (e.g.
`CHECKLEFT_GH_PAT`). No GitHub App provisioning needed.

```yaml
# .buildkite/pipeline.yml
steps:
  - label: ":white_check_mark: checkleft"
    command: checkleft run --annotations=check-run
    env:
      CHECKS_GITHUB_TOKEN: "$$CHECKLEFT_GH_PAT"
```

### Buildkite recipe — fallback (GitHub App)

When PAT provisioning is not an option, provision a GitHub App and store its
credentials as Buildkite secrets (see
[GitHub App setup](#github-app-setup-check-runs-buildkite-fallback)).

```yaml
# .buildkite/pipeline.yml
steps:
  - label: ":white_check_mark: checkleft"
    command: checkleft run --annotations=check-run
    env:
      CHECKS_GITHUB_APP_ID: "$$CHECKS_GITHUB_APP_ID"
      CHECKS_GITHUB_APP_PRIVATE_KEY: "$$CHECKS_GITHUB_APP_PEM"
      # Optional — omit to let checkleft discover the installation automatically:
      # CHECKS_GITHUB_INSTALLATION_ID: "$$CHECKS_GITHUB_INSTALLATION_ID"
```

### Repository override

If checkleft cannot resolve the GitHub repository from the environment (e.g. a
shallow clone with no `origin` remote), set:

```
CHECKS_REPOSITORY=owner/repo
```

### GitHub Enterprise Server

checkleft honors `GITHUB_API_URL` (set automatically by GHA on GHES) to route
REST calls to the enterprise host. No additional configuration is needed on GHA.
On Buildkite, set `GITHUB_API_URL=https://github.example.com/api/v3` explicitly.

---

## GitHub App setup: Check Runs (Buildkite fallback) {#github-app-setup-check-runs-buildkite-fallback}

This is the fallback for the `--annotations=check-run` Buildkite path when a
fine-grained PAT is not an option. Skip this section if you are using a
fine-grained PAT or running on GHA.

**Create a GitHub App:**

1. Go to **GitHub → Organization Settings → Developer settings → GitHub Apps →
   New GitHub App**.
2. Set a name (e.g. `checkleft-ci`), disable **Webhook**, and under
   **Repository permissions** grant **Checks: Read & write**.
3. After creation, note the **App ID** from the app's settings page.
4. Generate a **private key** (PEM) from the app's settings page.
5. Install the app on the target repository (Organization Settings →
   GitHub Apps → Install).

**Store credentials as Buildkite secrets:**

| Secret name                     | Value                                                                           |
| ------------------------------- | ------------------------------------------------------------------------------- |
| `CHECKS_GITHUB_APP_ID`          | The numeric App ID from step 3                                                  |
| `CHECKS_GITHUB_APP_PEM`         | The full PEM private key from step 4                                            |
| `CHECKS_GITHUB_INSTALLATION_ID` | (Optional) The installation ID — checkleft discovers it automatically if absent |

checkleft mints a short-lived installation token at runtime using these
credentials. No long-lived token is stored in the pipeline.

---

## Backend 3 — SARIF (`--annotations=sarif`)

**For GitHub Actions (and Buildkite with a separate upload step).** checkleft
writes a **SARIF 2.1.0** file; a subsequent step uploads it to GitHub code
scanning. This surfaces findings in the **Security → Code scanning alerts** tab
with persistent alerts, fingerprint-based deduplication, dismissal, and "fixed"
tracking — the richest alert lifecycle of all backends.

**GitHub UI surface:** **Security → Code scanning alerts** tab (durable alerts
with dedupe and dismiss) + inline annotations on the "Files changed" diff +
a code-scanning check entry.

**Private repositories require GitHub Advanced Security (GHAS).** The SARIF
serializer always produces a valid file; uploading it to code scanning on a
private repo requires GHAS to be licensed and enabled for the repository. Public
repos work without GHAS.

### GHA recipe (upload via `github/codeql-action/upload-sarif`)

```yaml
# .github/workflows/checks.yml
jobs:
  checkleft:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      security-events: write
    steps:
      - uses: actions/checkout@v4
      - name: Run checkleft
        run: checkleft run --annotations=sarif --annotations-out=checkleft.sarif
      - name: Upload SARIF to code scanning
        uses: github/codeql-action/upload-sarif@v3
        if: always() # upload even when checkleft found errors
        with:
          sarif_file: checkleft.sarif
```

No checked-in files. The ambient `github.token` with `security-events: write`
is all that is needed.

### Buildkite recipe (integrated upload via `--upload`)

checkleft can handle the upload itself. Requires a token with `security_events`
scope (resolved from `CHECKS_GITHUB_TOKEN` / `GH_TOKEN` / `GITHUB_TOKEN` / `gh
auth token`, in that order). The upload is **non-fatal** — a missing token,
missing repository, or API error is logged as a warning and checkleft exits with
its content-driven exit code.

```yaml
# .buildkite/pipeline.yml
steps:
  - label: ":white_check_mark: checkleft"
    command: checkleft run --upload
    env:
      CHECKS_GITHUB_TOKEN: "$$CHECKLEFT_GH_PAT" # token with security_events scope
```

To also write the SARIF file locally (for artifact archiving):

```yaml
command: checkleft run --annotations=sarif --annotations-out=checkleft.sarif --upload
```

### SARIF upload caps

GitHub code scanning imposes caps on SARIF uploads (roughly 5 000 results per
upload, ~10 MB compressed). When checkleft exceeds the cap it logs how many
findings were truncated before uploading; the truncation is never silent.

---

## Combining backends

`--annotations` is repeatable. Running multiple backends at once is supported.
Example — GHA workflow commands for inline diff annotations AND a SARIF file for
code scanning in one step (useful on GHA when GHAS is enabled):

```yaml
- name: Run checkleft
  run: checkleft run --annotations=gha --annotations=sarif --annotations-out=checkleft.sarif
- uses: github/codeql-action/upload-sarif@v3
  if: always()
  with:
    sarif_file: checkleft.sarif
```

---

## Quick-reference: backend comparison

| Backend                        | GHA                                            | Buildkite                                       | GitHub UI surface                               | Credentials              |
| ------------------------------ | ---------------------------------------------- | ----------------------------------------------- | ----------------------------------------------- | ------------------------ |
| `--annotations=gha`            | ✅ zero-credential                             | ❌ ignored                                      | Inline PR diff + job annotations                | None                     |
| `--annotations=check-run`      | ✅ ambient token                               | ✅ PAT or GitHub App                            | **Own check in Checks tab** + inline PR diff    | `Checks: write`          |
| `--annotations=sarif` + upload | ✅ ambient token (needs GHAS on private repos) | ✅ via `--upload` (needs GHAS on private repos) | **Security/code-scanning tab** + inline PR diff | `security-events: write` |

**Choosing a starting point:**

- **GHA only, zero configuration:** use `--annotations=gha`.
- **GHA + a named checkleft check in the Checks tab:** use `--annotations=check-run` with `permissions: checks: write`.
- **Buildkite:** use `--annotations=check-run` with a fine-grained PAT (`Checks: write`) as `CHECKS_GITHUB_TOKEN`.
- **Rich alert lifecycle (Security tab, dedup, "fixed" tracking):** use `--annotations=sarif` + upload (requires GHAS on private repos).
