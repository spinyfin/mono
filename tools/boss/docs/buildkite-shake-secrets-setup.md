# Buildkite secrets setup for `boss shake` release pipeline

The `boss-release` Buildkite step builds `Boss.app` with three GitHub App credentials embedded at compile time and publishes a versioned GitHub Release on every merge to `main`. This document covers one-time provisioning of those credentials in Buildkite.

## Background

`boss shake` authenticates as a GitHub App to file issues against `spinyfin/mono`. The App credentials are embedded into the binary at compile time via `option_env!()` macros in `tools/boss/cli/src/github_app.rs`. A release build without these credentials compiles but `boss shake` fails at runtime; the release step therefore fails fast if any secret is unset before spending build time.

## Secret names

The three secrets must be available to the pipeline as environment variables with exactly these names:

| Secret name | Value |
|---|---|
| `BOSS_SHAKE_APP_ID` | GitHub App ID (integer, e.g. `12345`) |
| `BOSS_SHAKE_INSTALLATION_ID` | Installation ID for the `spinyfin/mono` repo installation |
| `BOSS_SHAKE_PRIVATE_KEY_PEM` | Full PEM contents of the GitHub App's RSA private key |

## Where to find the values

1. Open the [GitHub App settings page](https://github.com/settings/apps) (or the org's Apps page at `https://github.com/organizations/spinyfin/settings/apps`).
2. Select the Boss shake App.
3. The **App ID** is shown at the top of the General tab.
4. The **Installation ID** can be found at `https://api.github.com/repos/spinyfin/mono/installation` when authenticated as the App (the `id` field in the response), or via `gh api /repos/spinyfin/mono/installation --jq .id`.
5. To get the **private key PEM**: scroll to the bottom of the App's General tab → "Private keys" → either download the existing key (if you have it) or generate a new one. The downloaded file is the PEM.

## Provisioning in Buildkite

Secrets are set as pipeline-level environment variables in Buildkite. The `boss-release` step then inherits them automatically.

1. In Buildkite, open the **spinyfin/mono pipeline → Settings → Environment Variables**.
2. Add each of the three secrets above:
   - Click **Add** (or **New Environment Variable**).
   - Enter the secret name exactly as shown in the table above.
   - Paste the value (see below for `BOSS_SHAKE_PRIVATE_KEY_PEM`).
   - Tick **Secret** (or the equivalent "hidden from logs" checkbox) so the value is masked in build logs.
3. Save.

### Pasting `BOSS_SHAKE_PRIVATE_KEY_PEM`

Buildkite environment variable values accept multi-line strings. Paste the full PEM block including the `-----BEGIN RSA PRIVATE KEY-----` header and `-----END RSA PRIVATE KEY-----` footer:

```
-----BEGIN RSA PRIVATE KEY-----
MIIEpAIBAAKCAQEA...
(remaining base64 lines)
...
-----END RSA PRIVATE KEY-----
```

Do not add extra whitespace before or after the block. The value must be identical to the file GitHub provides when you download or generate a private key.

## Scoping

All three secrets must be available to the `boss-release` step. If your Buildkite setup supports step-level secret scoping, scope them to the `boss-release` step. If the pipeline uses pipeline-level environment variables (the common setup), they are available to all steps; that is fine — the secrets are only meaningful to the release step and are ignored by other steps that don't reference them.

## Verification

After provisioning, trigger a build on `main` (e.g. merge any PR) and confirm:

1. The `boss-release` step appears in the Buildkite build (it is skipped on non-main branches).
2. The step completes green.
3. A new GitHub Release appears:

   ```sh
   gh release view v1.0.0 --repo spinyfin/mono
   ```

   (Replace `v1.0.0` with the actual tag if earlier releases already exist.)

4. The release has a `Boss-1.0.N.zip` artifact attached. Download and unzip it; `Boss.app` should launch and `boss shake` should file a test issue successfully.

## Key rotation

When the GitHub App private key is rotated:

1. Generate a new private key from the GitHub App settings page.
2. Update the `BOSS_SHAKE_PRIVATE_KEY_PEM` Buildkite environment variable with the new PEM.
3. The next merge to `main` will publish a release binary with the new key embedded.
4. Revoke the old key from the GitHub App settings page once the new release is confirmed working.

## Troubleshooting

**`boss-release` step fails immediately with "is not set" error**

One of the three secrets is missing from the pipeline environment. Check the Buildkite pipeline settings (Settings → Environment Variables) and confirm all three are present and not empty.

**`boss-release` step fails with a Bazel error about missing credentials**

This is the `option_env!` compile-time check failing. It means the env vars are set in the pipeline environment but Bazel is not forwarding them to the rustc action. Verify that `.bazelrc` contains the three `build --action_env=BOSS_SHAKE_*` lines — they should already be present at the bottom of the boss shake credentials block.

**`gh release create` fails with a 422 error**

A release with the computed tag already exists. This can happen if the step was re-run manually after a failure mid-way through. Either delete the partial release with `gh release delete <tag> --repo spinyfin/mono --yes` and re-run, or bump the version logic if the release was actually published.

**The `.app` doesn't pass Gatekeeper on the user's machine**

The release build is not notarized. A downloaded `.app` without a notarization ticket triggers Gatekeeper on macOS 10.15+. Notarization is out of scope for this chore — see the open question in PR #\<this PR\>. Users can right-click → Open to bypass the warning until notarization is wired up.
