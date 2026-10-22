# Managed provisioner selection: self-hosted Linux first, no paid management dependency

- **Date:** 2026-10-22
- **Kind:** investigation report (no code; inputs to the existing implementation entries are listed at the end)
- **Parent design:** [Hub-and-spoke engine](../designs/hub-and-spoke-distributed-engine.md), sections "Enrollment, service user and credential lifecycle" and "Managed provisioning and upgrades"
- **Primary source:** [Hub-and-spoke investigation](hub-and-spoke-distributed-engine-2026-10-21.md) §2.2 (automated bootstrap, enrollment and identity), §2.6, §2.7
- **Unblocks:** the design's "Hub managed provisioning jobs", "Managed Linux host provisioning" and "Managed macOS host provisioning" entries

The design requires a provisioning choice with authenticated secret delivery and machine identity. This report applies the operator's constraints in [PR #3257](https://github.com/spinyfin/mono/pull/3257): initial Linux boxes on local fiber over Tailscale, multiple provisioners, and no paid management product for the small fleet. A CI cloud runbook establishes feasibility, not cost superiority.

## Verdict

**Linux: self-hosted machines on local fiber over Tailscale first.** Select an existing managed machine rather than require VM creation. Use the Fleet Free script-runner integration evaluated below on both Linux and macOS, with systemd versus launchd installers. Tailscale supplies private reachability; it does not install software or replace provisioning authentication or Boss mTLS. A fleet administrator enrolls the management agent once; subsequent Boss installation and recovery are unattended, without SSH.

**macOS: narrowly recommend self-hosted Fleet Free as a script runner, subject to a free-edition acceptance gate.** This is not a recommendation to buy Premium or assume its MDM features are free. Fleet lists script execution, inventory, REST API and API-only access in Free; application deployment, zero-touch setup, account creation/password sync, RBAC and audit logging are Premium. Boss's signed installer and hub job history supply the installation and job evidence here. Secret-variable delivery is documented separately without a clear entitlement promise in the pricing matrix: prove it on a pinned server with no Premium/trial license before declaring this adapter usable. No paid fallback is authorized. [Pricing](https://fleetdm.com/pricing), [scripts](https://fleetdm.com/guides/scripts), [custom variables](https://fleetdm.com/guides/secrets-in-scripts-and-configuration-profiles).

**Endpoint Central Free is a credible alternative, evaluated below.** Its free allowance covers the documented small fleet. Fleet remains the narrower recommendation because its documented per-execution script results fit the job contract and it avoids adding a Windows management server. This is a documentation-based recommendation, not an observed deployment of either product.

**GCE is an optional provider, not the initial Linux requirement.** Its mapping and illustrative fixtures below remain a cloud comparison. GCE credentials, Secret Manager and VM identity must not be dependencies of self-hosted enrollment. No cloud purchase is justified by this report.

This report and the updated parent design supersede the earlier “cloud API plus cloud-init for Linux” requirement with an authenticated management agent on existing Linux machines, or a cloud API for cloud machines. They preserve investigation §§2.2, 2.6 and 2.7's trust and unattended installation requirements; no manual spoke-install path is introduced.

## Cost basis

Self-hosting first is the operator's decision, based on prior CI cost analysis, as recorded in the review brief for [PR #3257](https://github.com/spinyfin/mono/pull/3257). This report adopts that decision; it does not reproduce the analysis or claim a newly measured break-even point. The cited repository runbooks document existing home hardware and a working GCE path, but do not establish comparable cost per worker.

For an optional re-check when considering cloud burst capacity, compare equivalent measured throughput, RAM and disk capacity rather than nominal vCPUs:

- Local monthly cost: measured average watts × active hours / 1000 × electricity tariff, plus incremental storage, maintenance and management-server cost. For new hardware add amortized purchase cost; distinguish this from already-owned capacity.
- Cloud monthly cost: instance rate × billed hours, disks, snapshots, egress, IP and secret-service charges, using actual region, discounts and idle shutdown policy.
- Divide each by completed representative worker tasks, accounting for cache behavior, contention, administration effort and local power/fiber availability.

No new prices or workload measurements were collected here. The formula supports a future cloud-burst decision; it does not gate the already-selected self-hosted path or claim that GCE is cheaper. Bursty demand or an independent failure domain may justify cloud capacity; selecting a provider per host/job keeps that option open.

## What the fleet actually is (evidence from authorized documentation)

Everything below comes from documents checked into `spinyfin/mono`. No private operator state was read or changed.

| Fact                                                                                                                                                                                                                                                                       | Source                                                                                                      |
| -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| The `bazel-any` CI queue is "a heterogeneous fleet mixing personal Macs and Linux cloud agents". Personal Macs run the agent "under a real developer's user account using their `~/.ssh/` keys".                                                                           | `.buildkite/README.md` ("Pushing from CI")                                                                  |
| The macOS agents are `anaplian` and `skaffen` (two registrations each). The cited CI runbooks do not establish an existing MDM deployment.                                                                                                                                 | `.buildkite/linux-agents-runbook.md` (host inventory); the cited CI runbooks do not document MDM enrollment |
| The home Linux hosts (`empiricist`, `zoologist`, `diziet`) are Intel NUC / Beelink mini PCs with no out-of-band management; a 2026-08-23 power loss required "physically walking up to three machines".                                                                    | `.buildkite/linux-agents-runbook.md` ("Power loss and unattended recovery")                                 |
| The only API-driven host creation the repository documents is `gcloud compute instances create` on GCE (`n2-standard-16`, Ubuntu 26.04 LTS image family, `enable-oslogin=TRUE`, outbound-only, no inbound firewall rule, default service account, external IP for egress). | `.buildkite/gce-agent-runbook.md` ("Create the VM")                                                         |
| The Linux service-user pattern already in use is a package-created system user (`buildkite-agent`) with `HOME` under `/var/lib`, a systemd unit, `0600` credentials and `apt-mark hold` on the agent version.                                                              | `.buildkite/gce-agent-runbook.md` ("Install the Buildkite agent", "Configure the agent", "Start the agent") |
| The GCE runbook describes Tailscale for optional operator access; its nodes are "user-owned and untagged" and key expiry must be disabled by hand. It is not a provisioning channel.                                                                                       | `.buildkite/gce-agent-runbook.md` ("Tailnet and passwordless SSH")                                          |
| Azure (`az login --service-principal`) exists only for the flunge container deploy step; there is no Azure VM footprint.                                                                                                                                                   | `.buildkite/gce-agent-runbook.md` ("Azure CLI — flunge deploy step")                                        |
| Boss's current remote path provisions by pushing a shell wrapper over SSH and probing with `uname` / `gh auth status`; there is no provider integration to inherit.                                                                                                        | `tools/boss/engine/core/src/host_provisioning.rs` (`provision_remote_host`), investigation §1.1             |
| Release assets are published to GitHub Releases through `tools/release` with checksum sidecars; this is the HTTPS package origin the installer will fetch from.                                                                                                            | `tools/release/release.toml`; design "Managed provisioning and upgrades"                                    |

Consequences for the selection:

- Home machines are the initial Linux target. Installing a management agent is a prerequisite to establish, just as on the Macs, not a reason to exclude them.
- CI inventory is not authorization to repurpose CI machines or credentials. The managed Boss service uses its own dedicated worker environment.
- Private connectivity does not make a sleeping hub or powered-off NUC available. Hosts without out-of-band power control still require physical power recovery.

## Candidates and free-tier fit

| Candidate                                               | Assessment                                                                                                                                                       |
| ------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Self-hosted Linux + Fleet Free (initial recommendation) | Existing compute over fiber/Tailscale; management agent plus systemd installer, subject to the free-edition secret-delivery gate.                                |
| GCE (optional cloud provider)                           | Documented creation API, signed instance identity, Secret Manager and guest attributes; no workload cost evidence supports making it the default.                |
| Azure / AWS                                             | Possible providers behind the same contract; no worker-VM runbook in the cited repository evidence. That does not establish whether the operator has an account. |
| Fleet Free on macOS                                     | Documented script run/results API; not a free substitute for all Premium MDM features.                                                                           |
| Endpoint Central Free                                   | A serious 25-endpoint alternative with configurations and software distribution; detailed evaluation below.                                                      |
| Jamf Pro / Kandji / SimpleMDM                           | Paid management products are not selected under the no-paid-solution constraint; this is not a claim that their current APIs lack required capabilities.         |
| MicroMDM / NanoMDM                                      | Raw Apple MDM requires APNs operations and does not itself supply a general script-job/secret-delivery contract.                                                 |

### Fleet: entitlement and operating limits

Free is used for inventory and individual script execution. Signed package verification, service-user creation, systemd/launchd setup, enrollment retries and Boss job audit records are installer/hub responsibilities. Do not depend on paid application deployment, automated account setup, OS-update enforcement or zero-touch setup. Free also lacks the advertised Premium RBAC and audit logging: treat its API credential as fleet-wide privileged access, not a least-privilege per-host credential. Self-hosting still requires server maintenance and infrastructure.

An unmarked API endpoint does not prove free entitlement. Run secret write, substitution, masking and invalidation against a genuinely unlicensed Free server outside any trial, recording exact versions and permissions. If a required operation needs Premium or leaks a token, fail acceptance and re-evaluate the free alternatives. Do not buy a license or embed secrets directly into stored scripts to make acceptance pass.

### Endpoint Central evaluation

ManageEngine's [on-premises edition matrix](https://www.manageengine.com/products/desktop-central/edition-comparison-matrix.html) lists Free for 25 endpoints, including custom scripts, Windows/Mac/Linux configurations, inventory and application distribution. It is a continuing Free edition, not merely a trial. Count all managed endpoints against that allowance. It offers a broader management feature set than the narrow Fleet Free script path.

The [on-premises server requirements](https://www.manageengine.com/products/desktop-central/help/server/system-requirements.html) require Windows for the management server even though endpoints include Linux and macOS. This adds a Windows host/VM, licensing if needed, patching and backups to this fleet. Cloud is a distinct deployment option with its own API and entitlement verification; a free product license does not make either deployment costless.

The published [Cloud API index](https://www.manageengine.com/products/desktop-central/help/api/cloud/api-index.html) documents configuration and inventory operations. It does not establish on-premises Free support for the exact per-execution results and protected per-job secret delivery required here. Aggregate configuration success cannot be assumed to be a particular installer's exit status.

Before replacing Fleet, demonstrate on the actual Free edition: stable machine identity lookup; authenticated script submission and durable per-run result retrieval on both platforms; offline/timeout/retry handling; protected per-host secret delivery with no value in stored script parameters, logs or UI/API output; and cancellation/token invalidation. These are unverified fit questions, not assertions that Endpoint Central lacks the capabilities. Given the Windows server overhead and unproven job/secret API mapping, prefer Fleet's narrower documented script contract for the initial acceptance attempt. Neither recommendation authorizes paid features.

## Provider contract mapping

The design's hub job contract is `CreateHost(platform, machine_selector, release, worker_user) -> job_id`, `DeliverSecret(job_id, expiry, secret)`, `Inspect(job_id) -> staged result`, `Retry(job_id)`, `Cancel(job_id)`, with persisted stages `requested → package_verified → installed → enrolled → connected → preflight_passed → ready`.

### Optional cloud Linux: Compute Engine

**API versions.** Compute Engine REST, `v1` channel, base `https://compute.googleapis.com/compute/v1/`. Google's reference now also publishes date-named interface versions (the current stable reference is labelled `2026-09-01`); pin the `v1` channel, which Google documents as long-lived with in-place updates, and record the exact discovery revision the adapter was generated against. Secret Manager REST `v1`, base `https://secretmanager.googleapis.com/v1/`. Metadata server `http://metadata.google.internal/computeMetadata/v1/` (header `Metadata-Flavor: Google`). cloud-init GCE datasource reads the instance metadata keys `user-data` and `user-data-encoding`.

**Hub-side credential.** One hub service account (or ADC on the hub host) restricted to the fleet project with: `roles/compute.instanceAdmin.v1` (create/get/getGuestAttributes/setMetadata/delete), `roles/iam.serviceAccountUser` on the single per-fleet _instance_ service account, and `roles/secretmanager.admin` scoped to secrets under a fixed name prefix (`boss-enroll-*`). Nothing else. The instance service account itself gets only `roles/secretmanager.secretAccessor` on its own job's secret, granted per job.

| Contract operation                                  | Compute Engine realization                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      | Identity / idempotency                                                                                                                                                                                                                                                      |
| --------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `CreateHost(linux, selector, release, boss-worker)` | `POST .../zones/{zone}/instances` (`instances.insert`) with `requestId` = job UUID; metadata items `enable-guest-attributes=TRUE`, `enable-oslogin=TRUE`, `user-data` = versioned cloud-config (below), `boss-job-id`, `boss-hub-url`, `boss-release-id`; `serviceAccounts[0].email` = the fleet instance SA with `cloud-platform` scope; labels `boss-host-id`, `boss-job-id`. `selector` is `{project, zone, machine_type, image_family, image_project, boot_disk_gb}` for create, or `{project, zone, instance_id}` to adopt an existing stopped instance. Returns an `Operation`; `targetId` is the instance `id` (uint64). | Compute requires `requestId` to be a non-zero UUID and ignores a repeated request with the same id, so a retried insert cannot create a second VM. The job row stores `(project_id, zone, instance_id)` from `targetId` / `instances.get` as the provider machine identity. |
| `DeliverSecret(job_id, expiry, secret)`             | `POST .../secrets` (`secretId` = `boss-enroll-{job_id}`, automatic replication, `expireTime` = expiry) then `POST .../secrets/{id}:addVersion` with `payload.data` base64 and `payload.dataCrc32c`; then `setIamPolicy` on that secret binding `roles/secretmanager.secretAccessor` to the instance SA. The instance reads it with `GET .../versions/latest:access` using the metadata-server access token. The installer writes it to an owner-only temp file for `boss-worker`, enrolls, deletes the file, and the hub destroys the version (`:destroy`) on `enrolled`.                                                       | The secret name embeds the job id; adding a version to an existing secret is idempotent for the job. `expireTime` removes it even if the hub dies. The 64 KiB payload limit is irrelevant for a 256-bit token.                                                              |
| `Inspect(job_id)`                                   | `GET .../instances/{name}` for `status` (`PROVISIONING`, `STAGING`, `RUNNING`, `STOPPING`, `TERMINATED`) plus `GET .../instances/{name}/getGuestAttributes?queryPath=boss/` for installer-written stage keys (`stage`, `error_code`, `error_text`, `spoke_version`, `release_id`, `updated_at`). Spoke-side stages (`connected`, `preflight_passed`, `ready`) come from the hub's own stream manager, not the provider. `instances.getSerialPortOutput` is attached to a failure report for diagnosis only.                                                                                                                     | Guest attributes are readable by anyone with `compute.instances.getGuestAttributes`; the installer writes only non-secret stage data. Query cap is 10/minute/VM, so the hub polls at ≥ 10 s intervals (15 s recommended).                                                   |
| `Retry(job_id)`                                     | Stage-resume: `requested` → re-issue insert with the same `requestId`; `package_verified` / `installed` failures → `instances.reset` (cloud-init re-runs the bootstrap on the same instance identity); `enrolled` failures → re-deliver a new secret version (old version destroyed) without touching the VM. Never a new `instances.insert` with a fresh `requestId` while the old instance exists.                                                                                                                                                                                                                            | Identity cannot change on retry: the stored `instance_id` must match the identity token presented at enrollment.                                                                                                                                                            |
| `Cancel(job_id)`                                    | `instances.delete` with `requestId` = job UUID + `-cancel`; destroy all secret versions; mark the pending `hosts` row cancelled.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                | Delete is idempotent on the same `requestId`; a 404 is terminal success.                                                                                                                                                                                                    |

**Machine identity at enrollment.** The installer requests `GET /computeMetadata/v1/instance/service-accounts/default/identity?audience={hub_enroll_url}&format=full` and sends the JWT with the enrollment request. The hub verifies RS256 against `https://www.googleapis.com/oauth2/v3/certs`, `iss == https://accounts.google.com`, `aud == hub_enroll_url`, `exp` (tokens live under an hour), and that `google.compute_engine.{project_id, zone, instance_id}` equals the tuple stored by `CreateHost`. Google's docs are explicit that `instance_name` can repeat across zones, so the tuple, not the name, is the identity. This is a stronger binding than the design's minimum (single-use token only) and costs one metadata call.

**Secret-delivery invariants.** The enrollment token is in Secret Manager only; `user-data` contains the secret _reference_ (`projects/{p}/secrets/boss-enroll-{job}/versions/latest`), hub URL, hub CA fingerprint, release manifest URL, Ed25519 release verification key and job id. `user-data` is persistent, readable by any process on the VM via the metadata server and by project viewers via the API, which is why the design forbids secrets in it and why the token is also single-use and 15-minute-bounded.

**Readiness and failure mapping.**

| Observed provider state                                                                                                                                  | Job stage / typed error                                                                             |
| -------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| `instances.insert` Operation `error.errors[].code` = `ZONE_RESOURCE_POOL_EXHAUSTED`, `QUOTA_EXCEEDED`, `RESOURCE_ALREADY_EXISTS` (different `requestId`) | `requested` → `ProviderCapacity` / `ProviderQuota` / `IdentityConflict` (terminal; operator action) |
| Operation `DONE`, instance `RUNNING`, no `boss/stage` within 5 min                                                                                       | `requested` → `BootstrapNotStarted` (retryable via `instances.reset`; attach serial output)         |
| `boss/stage=package_verify_failed`, `error_code` ∈ `{manifest_sig, digest_mismatch, platform_mismatch, version_not_approved, origin_not_https}`          | `requested` → `PackageVerification` (terminal until a new release is offered)                       |
| `boss/stage=installed`, then no `enrolled` within token expiry                                                                                           | `installed` → `EnrollmentExpired` (retry re-delivers a secret version)                              |
| Secret access `403` surfaced as `boss/stage=secret_unreadable`                                                                                           | `installed` → `SecretDelivery` (retry re-applies IAM then re-delivers)                              |
| Identity JWT fails verification at the hub                                                                                                               | `installed` → `IdentityMismatch` (terminal; `Cancel` is the only safe path)                         |
| Instance `TERMINATED` / `STOPPING` unexpectedly                                                                                                          | any → `ProviderStopped` (retryable: `instances.start`)                                              |
| Spoke `Hello` received, preflight reports `failed_preflights`                                                                                            | `connected` → `PreflightFailed{names}` (host visible, unschedulable)                                |

**cloud-init payload shape (versioned, no secrets; `runcmd` is pseudocode, not executable shell).** The ordered operations specify the verifier the payload must carry; they are not names of existing installed commands.

```yaml
#cloud-config
# boss-bootstrap-payload/1
users:
  - name: boss-worker
    system: true
    shell: /bin/bash
    homedir: /var/lib/boss-worker
    lock_passwd: true
write_files:
  - path: /etc/boss-spoke/bootstrap.json
    permissions: "0644"
    content: |
      {"payload_version":1,
       "job_id":"job_01J9…",
       "host_id":"host_01J9…",
       "hub_url":"https://hub.example.invalid:8443",
       "hub_ca_sha256":"3f…",
       "release_manifest_url":"https://github.com/spinyfin/mono/releases/download/release-v1.2.3/boss-spoke-linux-amd64.manifest.json",
       "release_verify_key_ed25519":"MCowBQYDK2VwAyEA…",
       "enroll_secret_ref":"projects/example-project/secrets/boss-enroll-job_01J9…/versions/latest",
       "guest_attr_namespace":"boss"}
runcmd:
  - |
    PSEUDOCODE (abort on any failed requirement):
    config := read /etc/boss-spoke/bootstrap.json
    manifest_bytes, detached_signature := fetch over HTTPS from approved release origin
    require Ed25519 signature valid over exact manifest_bytes using config.release_verify_key_ed25519
    manifest := parse verified manifest_bytes
    require manifest release == approved release and platform == this host's OS/architecture
    bootstrap_bytes := fetch manifest's bootstrap asset over HTTPS from approved origin
    require SHA256(bootstrap_bytes) == bootstrap digest in verified manifest
    atomically install those verified bytes at /usr/local/sbin/boss-bootstrap, root-owned mode 0755
    execute /usr/local/sbin/boss-bootstrap --config /etc/boss-spoke/bootstrap.json
```

The bootstrap script itself is a release asset covered by the signed manifest. Signature verification precedes manifest parsing, and digest verification precedes installing or executing the downloaded bytes. The verifier and pinned key must arrive through the authenticated provisioning payload or trusted base image, never from the unverified bootstrap. Python PTY support (`fcntl`, `pty`, `termios`) is preflighted by the installer as `boss-worker`, per the design.

### Initial Linux and macOS: Fleet Free

**API versions.** Fleet REST API path version `/api/v1/fleet/` with `Authorization: Bearer <token>`; the server has no separate API version header, so the adapter pins a minimum **Fleet server version** and verifies it at `GET /api/v1/fleet/version` on every hub start. The secret-variables endpoint (`PUT /api/v1/fleet/spec/secret_variables`) is documented in Fleet's contributor API reference, which Fleet warns "frequently change[s]"; its request shape is a contract fixture below and part of the acceptance run so a drift is caught before it reaches production. `fleetd` on the Mac self-updates from Fleet's TUF repository (`tuf.fleetctl.com`) on its configured channel; pin the `stable` channels.

**Hub-side credential.** A dedicated Fleet API-only credential in the hub secret facility. Verify the permissions needed for script and secret-variable calls on Free; do not assume Premium RBAC or a Maintainer role provides a Free least-privilege boundary.

**Precondition established once by the fleet administrator:** a self-hosted Fleet server reachable over HTTPS and enrolled `fleetd` agents with scripts enabled. This report neither enrolls machines nor changes operator configuration. macOS uses a signed/notarized agent package; Linux uses the supported Linux agent package. Apple MDM/ABM is not required by the script path. No assumption is made about the personal Macs' ABM eligibility.

**Provider selection.** Store provider kind and provider-scoped machine identity on each job. OS and provider are separate choices: Linux may use Fleet or GCE. Keep cloud credentials/identity logic inside the cloud adapter. The shared job stages and Boss certificate lifecycle must work without any cloud service.

**Linux mapping.** Apply the table below to `CreateHost(linux, ...)` using the Fleet inventory's Linux platform value for the selected distribution (for example `ubuntu`), stored host id/UUID, and an enrolled agent with scripts enabled. A root bootstrap hands off to systemd, creates `boss-worker` with home `/var/lib/boss-worker`, verifies the Linux package and runs enrollment/preflight as that user. Probe via the same execution-id result API; cancel stops only this job's bootstrap and Boss service, never deletes or resets the physical host. Preserve worker data and existing journals on recovery. Retry reuses the selected machine and pending host identity. No GCE metadata token, cloud-init or VM operation is involved.

Both platforms use the same job-bound secret delivery and run/result protocol. Persist `expected_platform` from the requested, supported OS/distribution: `darwin` for macOS and `ubuntu` for the initial Ubuntu Linux fixture. Other Linux distributions require an explicit supported Fleet platform mapping; do not accept any non-`darwin` value as Linux. Linux hands off to systemd; macOS uses launchd.

| Contract operation                                     | Fleet realization                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | Identity / idempotency                                                                                                                                                                                                               |
| ------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `CreateHost(platform, selector, release, boss-worker)` | Resolve `selector` (`{hardware_uuid}` or `{hardware_serial}`) via `GET /api/v1/fleet/hosts/identifier/{identifier}`, or `{fleet_host_id}` via `GET /api/v1/fleet/hosts/{id}`; require `platform == expected_platform`, `status == "online"`, `scripts_enabled == true`, `orbit_version` non-null. Store `(fleet_host_id, uuid, hardware_serial)` as provider identity. Submit the bootstrap as `POST /api/v1/fleet/scripts/run` with `host_id` and `script_contents` (< 10,000 characters: a stub that fetches and verifies the signed installer, using the same signed-release policy as the Linux installer). Response `execution_id` is the provider job handle. | A job stores the first `execution_id`; a retried `CreateHost` for the same job never issues a second run while the first is `exit_code: null` and `host_timeout: false`.                                                             |
| `DeliverSecret(job_id, expiry, secret)`                | `PUT /api/v1/fleet/spec/secret_variables` with `{"secrets":[{"name":"FLEET_SECRET_BOSS_ENROLL_{JOB}","value":"<token>"}]}` **before** the run; the script references `$FLEET_SECRET_BOSS_ENROLL_{JOB}` and Fleet expands it at delivery to `fleetd`, which executes as root; the script writes it to an owner-only file for `boss-worker` and `boss-spoke enroll` deletes it. After `enrolled` or expiry, the hub overwrites the variable's value with a random placeholder (Fleet refuses to delete a variable still referenced by a stored script, so overwrite is the reliable invalidation).                                                                    | Fleet masks `$FLEET_SECRET_*` in UI/API views and stores values encrypted; the enrollment token is additionally single-use and 15-minute-bounded. The variable name embeds the job id, so a retry overwrites rather than duplicates. |
| `Inspect(job_id)`                                      | `GET /api/v1/fleet/scripts/results/{execution_id}` → `exit_code` (`null` = pending), `output`, `host_timeout`, `runtime`, `message`. The installer prints a final machine-readable line `BOSS_STAGE=<stage> BOSS_ERROR=<code>` and exits with a stage-specific code. For post-install stages the hub runs a second, read-only probe script (`boss-spoke status --json`) and reads its `output`; `connected` onward comes from the hub stream manager. Host liveness: `GET /hosts/{id}` `status` / `seen_time`.                                                                                                                                                      | Results are addressed by `execution_id`, never by "latest activity". Output is bounded by Fleet, so the installer keeps its structured line last.                                                                                    |
| `Retry(job_id)`                                        | Stage-resume by re-running the idempotent installer on the **same** `fleet_host_id`; a new `execution_id` is recorded against the same job. `enrolled` failures re-deliver by overwriting the same variable. Fleet's default script timeout (5 min) means the stub must hand off long work (package download, install, enrollment) to a job-specific systemd unit (Linux) or `launchd` job (macOS) and return; Fleet's `script_execution_timeout` agent option can raise the limit, but the adapter must not depend on it.                                                                                                                                          | Retry never changes `host_id`; the hub compares the enrolment's reported hardware UUID with the stored one.                                                                                                                          |
| `Cancel(job_id)`                                       | Overwrite the secret variable; run a short cleanup script (stop the job-specific systemd bootstrap unit and `boss-spoke.service` on Linux, or boot out the corresponding launchd jobs on macOS; remove only that job's temporary token, preserving journals and worker data) with its own `execution_id`; mark the pending `hosts` row cancelled. Fleet offers no "cancel a pending script", so a cancelled job that later reports `exit_code` is ignored by stage machine design.                                                                                                                                                                                  | Cleanup is idempotent on the host.                                                                                                                                                                                                   |

**Machine identity at enrollment.** Bind the job to the Fleet host id and UUID captured from inventory. A reported hardware UUID is a consistency check, not cryptographic proof: a root process can spoof it. Authorization depends on the authenticated management channel delivering a single-use job-bound token to the selected host, and on the hub atomically binding its consumption to that host's generated key. Do not claim the UUID prevents use of a stolen token.

**Linux identity source.** On the initial x86 NUC/Beelink hosts, the root installer reads SMBIOS/DMI `/sys/class/dmi/id/product_uuid`, trims whitespace and normalizes UUID case before passing the captured value in the enrollment request for comparison with the stored Fleet `host.uuid`. Fleet populates that field from osquery's `system_info.uuid`; osquery normally obtains it from DMI on Linux but can retain or generate an identifier when the hardware value is unavailable or a placeholder. Therefore verify the three-way DMI/osquery/Fleet match during qualification; reject missing, placeholder, duplicated or mismatched values as `IdentityMismatch`, rather than substituting hostname, serial, `/etc/machine-id` or a new UUID silently. Other identity sources require an explicit provider mapping. DMI is spoofable by root/firmware or a VM administrator, and is not an attestation or protection against token theft. [Fleet inventory ingestion](https://github.com/fleetdm/fleet/blob/main/server/service/osquery_utils/queries.go), [osquery system_info](https://github.com/osquery/osquery/blob/master/osquery/tables/system/linux/system_info.cpp), [osquery UUID selection](https://github.com/osquery/osquery/blob/master/osquery/core/system.cpp).

**Secret-delivery acceptance gate.** Prove secret-variable API access on Free, substitution into an owner-only file, redacted script/result/UI views, and invalidation after consumption, cancellation or expiry. Root on the selected host can access the value, within the installer trust model. If any gate fails, this mapping is not qualified. A UUID and job id sent over server-authenticated TLS do not authenticate a client; the previously suggested one-time-fetch fallback is therefore not a safe substitute. An alternative needs an independently authenticated delivery channel and review, not a config switch.

**Readiness and failure mapping.**

| Observed provider state                                                                                                                                     | Job stage / typed error                                                                                  |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| Host lookup 404, or `platform != expected_platform` (`darwin` for macOS, `ubuntu` for initial Linux)                                                        | `requested` → `MachineNotFound` / `WrongPlatform` (terminal)                                             |
| `scripts_enabled` false/null, or `orbit_version` null                                                                                                       | `requested` → `ManagementAgentNotReady` (operator action: rebuild `fleetd` package with scripts enabled) |
| `status` `offline` / `mia` / `missing`                                                                                                                      | `requested` or `installed` → `HostOffline` (retryable; Fleet runs the script at next check-in)           |
| `scripts/run` 4xx: variable missing in `script_contents` (Fleet validates references)                                                                       | `requested` → `SecretDelivery` (retry re-PUTs the variable)                                              |
| Result `host_timeout: true`                                                                                                                                 | stage of the last `BOSS_STAGE` line, error `BootstrapTimeout` (retry; stub must have handed off)         |
| Result `exit_code` ∈ installer stage codes with `BOSS_ERROR` ∈ `{manifest_sig, digest_mismatch, platform_mismatch, version_not_approved, origin_not_https}` | `requested` → `PackageVerification` (terminal)                                                           |
| `BOSS_STAGE=installed`, no `enrolled` before token expiry                                                                                                   | `installed` → `EnrollmentExpired` (retry re-delivers)                                                    |
| Missing/unsupported machine UUID source, ambiguous identity, or normalized enrollment UUID ≠ stored Fleet `uuid`                                            | `installed` → `IdentityMismatch` (terminal; `Cancel`)                                                    |
| Spoke `Hello`, preflight `failed_preflights` non-empty                                                                                                      | `connected` → `PreflightFailed{names}`                                                                   |

**Non-selected MDM-command comparison (not required or assumed free).** Where a Mac is also Apple-MDM-enrolled in Fleet, `POST /api/v1/fleet/commands/run` with a base64 `InstallEnterpriseApplication` plist (`ManifestURL` on the approved HTTPS origin, `PinningRevocationCheckRequired` true, pinning certs set) can replace the stub's download step. Because macOS acknowledges before installing and reports no install errors over MDM, the stage machine must still obtain `installed` from the probe script, never from `commands/results` `status: Acknowledged`.

## Sanitized contract fixtures

These are illustrative provider contract shapes for later Bazel tests, not captured responses or proof of Free entitlement. GCE and MDM examples are optional comparisons; the initial Fleet script adapter must pass the Free acceptance gate. Identifiers, projects, hostnames and tokens are placeholders; field names and nesting are as documented by the providers on 2026-10-22. The implementation entry commits them as files next to the provider crates; this report is their reviewed source.

### Compute Engine: `instances.insert` request (abbreviated) and Operation response

```json
{
  "request": {
    "method": "POST",
    "url": "https://compute.googleapis.com/compute/v1/projects/example-project/zones/us-central1-a/instances?requestId=7d1c7f58-6b7a-4a3e-9c2d-0e1f2a3b4c5d",
    "body": {
      "name": "boss-spoke-01j9x",
      "machineType": "zones/us-central1-a/machineTypes/n2-standard-16",
      "labels": {
        "boss-host-id": "host_01j9x",
        "boss-job-id": "job_01j9x",
        "purpose": "boss-spoke"
      },
      "disks": [
        {
          "boot": true,
          "autoDelete": true,
          "initializeParams": {
            "sourceImage": "projects/ubuntu-os-cloud/global/images/family/ubuntu-2604-lts-amd64",
            "diskSizeGb": "500",
            "diskType": "zones/us-central1-a/diskTypes/pd-balanced"
          }
        }
      ],
      "networkInterfaces": [
        {
          "network": "global/networks/default",
          "accessConfigs": [{ "type": "ONE_TO_ONE_NAT", "name": "External NAT" }]
        }
      ],
      "serviceAccounts": [
        {
          "email": "boss-spoke-instance@example-project.iam.gserviceaccount.com",
          "scopes": ["https://www.googleapis.com/auth/cloud-platform"]
        }
      ],
      "shieldedInstanceConfig": {
        "enableSecureBoot": true,
        "enableVtpm": true,
        "enableIntegrityMonitoring": true
      },
      "metadata": {
        "items": [
          { "key": "enable-guest-attributes", "value": "TRUE" },
          { "key": "enable-oslogin", "value": "TRUE" },
          {
            "key": "boss-bootstrap-url",
            "value": "https://github.com/spinyfin/mono/releases/download/release-v1.2.3/boss-bootstrap-linux"
          },
          { "key": "user-data", "value": "#cloud-config\n# boss-bootstrap-payload/1\n…" }
        ]
      }
    }
  },
  "response": {
    "kind": "compute#operation",
    "id": "1234567890123456789",
    "name": "operation-1761100000000-abcdef",
    "operationType": "insert",
    "targetLink": "https://www.googleapis.com/compute/v1/projects/example-project/zones/us-central1-a/instances/boss-spoke-01j9x",
    "targetId": "9876543210987654321",
    "status": "RUNNING",
    "progress": 0,
    "insertTime": "2026-10-22T10:00:00.000-07:00",
    "selfLink": "https://www.googleapis.com/compute/v1/projects/example-project/zones/us-central1-a/operations/operation-1761100000000-abcdef"
  },
  "error_variant": {
    "kind": "compute#operation",
    "status": "DONE",
    "error": {
      "errors": [
        {
          "code": "ZONE_RESOURCE_POOL_EXHAUSTED",
          "message": "The zone 'projects/example-project/zones/us-central1-a' does not have enough resources available to fulfill the request."
        }
      ]
    },
    "httpErrorStatusCode": 503,
    "httpErrorMessage": "SERVICE UNAVAILABLE"
  }
}
```

### Secret Manager: `secrets.versions.access` response (read from the instance)

```json
{
  "request": {
    "method": "GET",
    "url": "https://secretmanager.googleapis.com/v1/projects/example-project/secrets/boss-enroll-job_01j9x/versions/latest:access",
    "headers": {
      "Authorization": "Bearer <access token from http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token>"
    }
  },
  "response": {
    "name": "projects/123456789012/secrets/boss-enroll-job_01j9x/versions/1",
    "payload": { "data": "<base64 of the 256-bit enrollment token>", "dataCrc32c": "3027405460" }
  },
  "iam_binding_applied_by_hub": {
    "resource": "projects/example-project/secrets/boss-enroll-job_01j9x",
    "bindings": [
      {
        "role": "roles/secretmanager.secretAccessor",
        "members": ["serviceAccount:boss-spoke-instance@example-project.iam.gserviceaccount.com"]
      }
    ]
  }
}
```

### Compute Engine: identity token claims presented at enrollment (`format=full`)

```json
{
  "iss": "https://accounts.google.com",
  "aud": "https://hub.example.invalid:8443/enroll",
  "sub": "112233445566778899001",
  "azp": "112233445566778899001",
  "email": "boss-spoke-instance@example-project.iam.gserviceaccount.com",
  "email_verified": true,
  "iat": 1761100300,
  "exp": 1761103900,
  "google": {
    "compute_engine": {
      "project_id": "example-project",
      "project_number": 123456789012,
      "zone": "us-central1-a",
      "instance_id": "9876543210987654321",
      "instance_name": "boss-spoke-01j9x",
      "instance_creation_timestamp": 1761100060
    }
  }
}
```

Verifier inputs: JWKS `https://www.googleapis.com/oauth2/v3/certs`, `alg` RS256, `kid` from the header. The hub matches `project_id`, `zone`, `instance_id` to the stored tuple.

### Compute Engine: `instances.getGuestAttributes` readiness read

```json
{
  "request": {
    "method": "GET",
    "url": "https://compute.googleapis.com/compute/v1/projects/example-project/zones/us-central1-a/instances/boss-spoke-01j9x/getGuestAttributes?queryPath=boss/"
  },
  "response": {
    "kind": "compute#guestAttributes",
    "queryPath": "boss/",
    "queryValue": {
      "items": [
        { "namespace": "boss", "key": "stage", "value": "installed" },
        { "namespace": "boss", "key": "release_id", "value": "release-v1.2.3" },
        { "namespace": "boss", "key": "spoke_version", "value": "1.2.3" },
        { "namespace": "boss", "key": "updated_at", "value": "2026-10-22T17:04:11Z" }
      ]
    },
    "selfLink": "https://www.googleapis.com/compute/v1/projects/example-project/zones/us-central1-a/instances/boss-spoke-01j9x"
  },
  "failure_variant_items": [
    { "namespace": "boss", "key": "stage", "value": "package_verify_failed" },
    { "namespace": "boss", "key": "error_code", "value": "digest_mismatch" },
    {
      "namespace": "boss",
      "key": "error_text",
      "value": "boss-spoke-linux-amd64 sha256 mismatch against signed manifest"
    }
  ],
  "installer_write": "curl -X PUT -H 'Metadata-Flavor: Google' --data 'installed' http://metadata.google.internal/computeMetadata/v1/instance/guest-attributes/boss/stage"
}
```

### Fleet macOS: host identity resolution

```json
{
  "request": {
    "method": "GET",
    "url": "https://fleet.example.invalid/api/v1/fleet/hosts/identifier/392547DC-0000-0000-A87A-D701FF75BC65",
    "headers": { "Authorization": "Bearer <api-only user token>" }
  },
  "response_subset": {
    "host": {
      "id": 33,
      "uuid": "392547dc-0000-0000-a87a-d701ff75bc65",
      "hardware_serial": "C02XXXXXXXXX",
      "hardware_model": "Mac14,12",
      "platform": "darwin",
      "os_version": "macOS 26.0.1",
      "status": "online",
      "seen_time": "2026-10-22T17:00:41Z",
      "last_enrolled_at": "2026-10-20T02:29:13Z",
      "orbit_version": "1.52.0",
      "osquery_version": "5.20.0",
      "scripts_enabled": true,
      "mdm": { "enrollment_status": null, "name": null, "server_url": null }
    }
  }
}
```

### Fleet macOS: secret delivery, job submission and inspection

```json
{
  "deliver_secret": {
    "method": "PUT",
    "url": "https://fleet.example.invalid/api/v1/fleet/spec/secret_variables",
    "body": {
      "secrets": [
        { "name": "FLEET_SECRET_BOSS_ENROLL_JOB_01J9X", "value": "<256-bit enrollment token>" }
      ]
    },
    "response_status": 200
  },
  "create_host_run": {
    "method": "POST",
    "url": "https://fleet.example.invalid/api/v1/fleet/scripts/run",
    "body": {
      "host_id": 33,
      "script_contents": "#!/bin/bash\n# boss-bootstrap-stub/1 job_01j9x\nset -euo pipefail\numask 077\nMANIFEST_URL='https://github.com/spinyfin/mono/releases/download/release-v1.2.3/boss-spoke-macos-arm64.manifest.json'\nVERIFY_KEY='MCowBQYDK2VwAyEA…'\nHUB_URL='https://hub.example.invalid:8443'\nHUB_CA_SHA256='3f…'\ninstall -d -m 0700 /var/root/boss-bootstrap\nprintf '%s' \"$FLEET_SECRET_BOSS_ENROLL_JOB_01J9X\" > /var/root/boss-bootstrap/enroll.token\n# fetch + verify signed manifest and bootstrap binary (digest checked before exec), then hand off:\n… \nlaunchctl bootstrap system /Library/LaunchDaemons/com.spinyfin.boss-bootstrap.plist\necho 'BOSS_STAGE=requested BOSS_ERROR=none BOSS_HANDOFF=launchd'\nexit 0\n"
    },
    "response": { "host_id": 33, "execution_id": "e797d6c6-3aae-11ee-be56-0242ac120002" },
    "response_status": 202
  },
  "inspect_pending": {
    "method": "GET",
    "url": "https://fleet.example.invalid/api/v1/fleet/scripts/results/e797d6c6-3aae-11ee-be56-0242ac120002",
    "response": {
      "script_contents": "#!/bin/bash\n# boss-bootstrap-stub/1 job_01j9x\n…$FLEET_SECRET_BOSS_ENROLL_JOB_01J9X…",
      "exit_code": null,
      "output": "",
      "message": "",
      "hostname": "anaplian",
      "host_timeout": false,
      "host_id": 33,
      "execution_id": "e797d6c6-3aae-11ee-be56-0242ac120002",
      "runtime": 0,
      "created_at": "2026-10-22T17:01:02Z"
    }
  },
  "inspect_done": {
    "response": {
      "exit_code": 0,
      "output": "BOSS_STAGE=requested BOSS_ERROR=none BOSS_HANDOFF=launchd",
      "host_timeout": false,
      "runtime": 7,
      "execution_id": "e797d6c6-3aae-11ee-be56-0242ac120002",
      "host_id": 33,
      "hostname": "anaplian",
      "message": "",
      "created_at": "2026-10-22T17:01:02Z"
    }
  },
  "inspect_failed": {
    "response": {
      "exit_code": 12,
      "output": "verifying manifest signature… FAIL\nBOSS_STAGE=requested BOSS_ERROR=manifest_sig",
      "host_timeout": false,
      "runtime": 3,
      "execution_id": "0b8f2a9e-…",
      "host_id": 33,
      "hostname": "anaplian",
      "message": "",
      "created_at": "2026-10-22T17:09:40Z"
    }
  },
  "inspect_timeout": {
    "response": {
      "exit_code": -1,
      "output": "",
      "host_timeout": true,
      "runtime": 300,
      "execution_id": "5e4d3c2b-…",
      "host_id": 33,
      "hostname": "anaplian",
      "message": "Timeout. Fleet stopped the script after 5 minutes to protect host performance.",
      "created_at": "2026-10-22T17:15:00Z"
    }
  },
  "probe_run": {
    "method": "POST",
    "url": "https://fleet.example.invalid/api/v1/fleet/scripts/run",
    "body": {
      "host_id": 33,
      "script_contents": "#!/bin/sh\nexec sudo -u boss-worker /usr/local/bin/boss-spoke status --json\n"
    },
    "expected_output": "{\"stage\":\"enrolled\",\"host_id\":\"host_01j9x\",\"spoke_version\":\"1.2.3\",\"cert_not_after\":\"2026-11-21T17:03:00Z\"}"
  },
  "invalidate_secret_after_enrolled": {
    "method": "PUT",
    "url": "https://fleet.example.invalid/api/v1/fleet/spec/secret_variables",
    "body": {
      "secrets": [{ "name": "FLEET_SECRET_BOSS_ENROLL_JOB_01J9X", "value": "consumed-<random>" }]
    }
  }
}
```

### Fleet Linux: host identity resolution

This Ubuntu/x86 fixture selects an already enrolled physical host. The root-read DMI value `A2064CEF-1234-4567-AFB9-283E3C1D487E` and osquery `system_info.uuid` must normalize to the returned UUID; the hardware model and serial are illustrative, not a claim about a particular operator machine.

```json
{
  "request": {
    "method": "GET",
    "url": "https://fleet.example.invalid/api/v1/fleet/hosts/identifier/a2064cef-1234-4567-afb9-283e3c1d487e",
    "headers": { "Authorization": "Bearer <api-only user token>" }
  },
  "response_subset": {
    "host": {
      "id": 44,
      "uuid": "a2064cef-1234-4567-afb9-283e3c1d487e",
      "hardware_serial": "NUC-SANITIZED-01",
      "hardware_model": "NUC11PAHi7",
      "platform": "ubuntu",
      "os_version": "Ubuntu 24.04.3 LTS",
      "status": "online",
      "seen_time": "2026-10-22T17:00:41Z",
      "orbit_version": "1.52.0",
      "osquery_version": "5.20.0",
      "scripts_enabled": true
    }
  },
  "expected_job_identity": {
    "provider": "fleet",
    "fleet_host_id": 44,
    "uuid": "a2064cef-1234-4567-afb9-283e3c1d487e",
    "expected_platform": "ubuntu"
  },
  "negative_variants": [
    { "platform": "darwin", "expected_error": "WrongPlatform" },
    { "scripts_enabled": false, "expected_error": "ManagementAgentNotReady" },
    { "status": "offline", "expected_error": "HostOffline" },
    {
      "dmi_product_uuid": "00000000-0000-0000-0000-000000000000",
      "expected_error": "IdentityMismatch"
    }
  ]
}
```

### Fleet Linux: secret delivery, systemd handoff and inspection

The JSON is a request/response fixture; `script_contents` is **non-executable installer pseudocode** at the uppercase `FETCH`, `VERIFY` and `INSTALL` steps. These steps run in order and fail closed; they do not assume an existing Boss installer on the fresh host. The verifier comes from the authenticated management payload, not the unverified download. `INSTALL` creates the dedicated account/home and root-owned bootstrap/config; the systemd bootstrap downloads and verifies the larger spoke package under the same signature/digest policy, installs it and enrolls as `boss-worker`, removes the token and starts `boss-spoke.service` with `User=boss-worker`. Its unit uses the same private home/state and service environment for preflight.

The hub renders enrollment tokens as exactly 64 hexadecimal characters and validates that encoding before secret submission, so Fleet's textual substitution cannot introduce shell syntax. The root stub uses a shell builtin to write the substituted value with `umask 077`, then transfers the `0600` file to `boss-worker`; no token enters a child process's argv or the systemd unit definition. The unit/config contain only file paths and public job/trust metadata. Both the script/result masking and host-side file handling still require the Free-edition acceptance test.

```json
{
  "deliver_secret": {
    "method": "PUT",
    "url": "https://fleet.example.invalid/api/v1/fleet/spec/secret_variables",
    "body": {
      "secrets": [
        {
          "name": "FLEET_SECRET_BOSS_ENROLL_JOB_01J9L",
          "value": "<64 hexadecimal characters; sanitized>"
        }
      ]
    },
    "response_status": 200
  },
  "create_host_run": {
    "method": "POST",
    "url": "https://fleet.example.invalid/api/v1/fleet/scripts/run",
    "body": {
      "host_id": 44,
      "script_contents": "#!/bin/bash\n# boss-bootstrap-stub/1 job_01j9l; NON-EXECUTABLE PSEUDOCODE at uppercase steps\nset -euo pipefail\numask 077\nMANIFEST_URL='https://github.com/spinyfin/mono/releases/download/release-v1.2.3/boss-spoke-linux-amd64.manifest.json'\nVERIFY_KEY='MCowBQYDK2VwAyEA…'\nFETCH manifest bytes and detached signature from MANIFEST_URL over approved HTTPS using payload-pinned VERIFY_KEY\nVERIFY Ed25519 signature over exact bytes BEFORE parsing manifest\nVERIFY approved release and linux-amd64 platform\nFETCH bootstrap from approved HTTPS origin\nVERIFY bootstrap SHA256 against verified manifest BEFORE execution\nINSTALL verified bootstrap; create boss-worker with home /var/lib/boss-worker and private state; write public job/trust config to /etc/boss-spoke/bootstrap-job_01j9l.json\ninstall -d -o boss-worker -g boss-worker -m 0700 /var/lib/boss-worker/.boss-spoke/bootstrap/job_01j9l\nprintf '%s' '$FLEET_SECRET_BOSS_ENROLL_JOB_01J9L' > /var/lib/boss-worker/.boss-spoke/bootstrap/job_01j9l/enroll.token\nchown boss-worker:boss-worker /var/lib/boss-worker/.boss-spoke/bootstrap/job_01j9l/enroll.token\nchmod 0600 /var/lib/boss-worker/.boss-spoke/bootstrap/job_01j9l/enroll.token\nsystemd-run --unit=boss-bootstrap-job_01j9l --property=Type=exec --property=UMask=0077 /usr/local/sbin/boss-bootstrap --config /etc/boss-spoke/bootstrap-job_01j9l.json --worker-user boss-worker --enrollment-token-file /var/lib/boss-worker/.boss-spoke/bootstrap/job_01j9l/enroll.token\necho 'BOSS_STAGE=requested BOSS_ERROR=none BOSS_HANDOFF=systemd'\n"
    },
    "response": {
      "host_id": 44,
      "execution_id": "e797d6c6-3aae-41ee-be56-0242ac120044"
    },
    "response_status": 202
  },
  "inspect_pending": {
    "method": "GET",
    "url": "https://fleet.example.invalid/api/v1/fleet/scripts/results/e797d6c6-3aae-41ee-be56-0242ac120044",
    "response_subset": {
      "host_id": 44,
      "execution_id": "e797d6c6-3aae-41ee-be56-0242ac120044",
      "exit_code": null,
      "output": "",
      "host_timeout": false
    }
  },
  "inspect_handoff": {
    "method": "GET",
    "url": "https://fleet.example.invalid/api/v1/fleet/scripts/results/e797d6c6-3aae-41ee-be56-0242ac120044",
    "response_subset": {
      "host_id": 44,
      "execution_id": "e797d6c6-3aae-41ee-be56-0242ac120044",
      "exit_code": 0,
      "output": "BOSS_STAGE=requested BOSS_ERROR=none BOSS_HANDOFF=systemd",
      "host_timeout": false,
      "runtime": 9
    }
  },
  "inspect_failure_variants": [
    {
      "exit_code": 12,
      "output": "BOSS_STAGE=requested BOSS_ERROR=manifest_sig",
      "host_timeout": false
    },
    {
      "exit_code": 13,
      "output": "BOSS_STAGE=requested BOSS_ERROR=digest_mismatch",
      "host_timeout": false
    },
    {
      "exit_code": -1,
      "output": "",
      "host_timeout": true
    }
  ],
  "probe_run": {
    "method": "POST",
    "url": "https://fleet.example.invalid/api/v1/fleet/scripts/run",
    "body": {
      "host_id": 44,
      "script_contents": "#!/bin/sh\nset -eu\nsystemctl show boss-spoke.service --property=User,Group,ActiveState\nexec runuser -u boss-worker -- env HOME=/var/lib/boss-worker /usr/local/bin/boss-spoke status --json\n"
    },
    "response": {
      "host_id": 44,
      "execution_id": "f797d6c6-3aae-41ee-be56-0242ac120044"
    },
    "response_status": 202
  },
  "inspect_probe": {
    "method": "GET",
    "url": "https://fleet.example.invalid/api/v1/fleet/scripts/results/f797d6c6-3aae-41ee-be56-0242ac120044",
    "response_subset": {
      "host_id": 44,
      "execution_id": "f797d6c6-3aae-41ee-be56-0242ac120044",
      "exit_code": 0,
      "host_timeout": false,
      "output": "User=boss-worker\nGroup=boss-worker\nActiveState=active\n{\"stage\":\"enrolled\",\"host_id\":\"host_01j9l\",\"spoke_version\":\"1.2.3\",\"cert_not_after\":\"2026-11-21T17:03:00Z\"}"
    }
  },
  "invalidate_secret_after_enrolled": {
    "method": "PUT",
    "url": "https://fleet.example.invalid/api/v1/fleet/spec/secret_variables",
    "body": {
      "secrets": [
        {
          "name": "FLEET_SECRET_BOSS_ENROLL_JOB_01J9L",
          "value": "consumed-<random>"
        }
      ]
    }
  }
}
```

Handoff exit zero only confirms submission to systemd; it does not establish installation/enrollment/readiness. The probe's JSON is the proposed Boss status contract, separate from Fleet's result envelope. Assert `installed` then `enrolled` via successive probes; the hub alone records `connected → preflight_passed → ready` from authenticated stream/preflight observations. A connected Linux host reporting `failed_preflights: ["python3_pty"]` must stay unschedulable with `PreflightFailed{python3_pty}` even if the Fleet script succeeded.

### Fleet: non-selected MDM install command comparison

```json
{
  "run": {
    "method": "POST",
    "url": "https://fleet.example.invalid/api/v1/fleet/commands/run",
    "body": {
      "command": "<base64 plist: RequestType=InstallEnterpriseApplication, ManifestURL=https://github.com/spinyfin/mono/releases/download/release-v1.2.3/boss-spoke-macos-arm64.manifest.plist, PinningRevocationCheckRequired=true, ManifestURLPinningCerts=[…]>",
      "host_uuids": ["392547dc-0000-0000-a87a-d701ff75bc65"]
    },
    "response": {
      "command_uuid": "a2064cef-0000-1234-afb9-283e3c1d487e",
      "request_type": "InstallEnterpriseApplication"
    }
  },
  "results": {
    "method": "GET",
    "url": "https://fleet.example.invalid/api/v1/fleet/commands/results?command_uuid=a2064cef-0000-1234-afb9-283e3c1d487e",
    "response": {
      "results": [
        {
          "host_uuid": "392547dc-0000-0000-a87a-d701ff75bc65",
          "command_uuid": "a2064cef-0000-1234-afb9-283e3c1d487e",
          "status": "Acknowledged",
          "updated_at": "2026-10-22T17:02:10Z",
          "request_type": "InstallEnterpriseApplication",
          "hostname": "anaplian",
          "payload": "<base64>",
          "result": "<base64 plist>"
        }
      ]
    },
    "note": "Acknowledged means parameters validated only; installed must come from the probe script."
  }
}
```

## Acceptance procedure

The initial Fleet procedure and optional GCE procedure are to be executed by the platform-adapter entries against real providers and recorded in the repo-owned fresh-host acceptance report format those entries define. Nothing here requires SSH. Every step names the artifact that proves it.

### Optional cloud Linux (Compute Engine)

1. **Preconditions recorded:** fleet project id, zone, instance service-account email, hub service-account roles (exactly the three listed above), approved image family, and the project-wide `boss-enroll-*` secret prefix. Record `gcloud --version` and the Compute discovery revision used to generate the client.
2. **Create:** `CreateHost(linux, …)` → assert Operation `targetId` is stored as `instance_id`; assert the instance metadata contains no secret (dump `instances.get` `metadata.items` into the report with values hashed).
3. **Idempotency:** re-issue `CreateHost` for the same job within 10 minutes → assert the same Operation/instance (same `targetId`), no second VM in `instances.list` filtered by `labels.boss-job-id`.
4. **Secret delivery:** `DeliverSecret` → assert a project viewer _without_ `secretAccessor` gets 403 on `:access`; assert the VM's guest attribute `boss/stage` reaches `installed` and `enrolled`; assert the secret version is `DESTROYED` after `enrolled`.
5. **Identity:** capture the identity JWT presented at enrollment (hub audit log) → assert `iss`, `aud`, `exp`, signature, and the `(project_id, zone, instance_id)` match. Negative: enroll from a second VM with a copied `bootstrap.json` → `IdentityMismatch`, no certificate issued.
6. **Package verification negatives:** offer a manifest with a bad signature, then a package with a wrong digest → `boss/stage=package_verify_failed` with the right `error_code`, no service installed (`systemctl status boss-spoke` absent, proven via a later successful run's installer log).
7. **Retry:** power off the VM mid-install (`instances.stop`) → `Retry` resumes via `instances.reset`; the same instance id enrols; no new VM.
8. **Expiry:** let a token expire unused → `EnrollmentExpired`; `Retry` delivers a new version and succeeds.
9. **Service user:** `boss-spoke status --json` run by the installer as `boss-worker` shows owner-only state dirs, python PTY preflight passed, `gh auth status` succeeded for the provisioned credential, no secret in `journalctl -u boss-spoke`.
10. **Ready end-to-end:** the hub shows `ready`; one real task runs on the spoke and publishes a PR (this is the design's remote-capacity acceptance, referenced not duplicated).
11. **Cancel:** `Cancel` → instance deleted, secret destroyed, `hosts` row cancelled; repeat `Cancel` → no error.
12. **Poll budget:** report shows guest-attribute poll interval ≥ 10 s and no `429` from the metadata server across the run.

### Initial self-hosted Linux and macOS (Fleet Free)

1. **Preconditions recorded:** an unlicensed Free server outside any Premium trial, Linux and macOS agent enrollment, Tailscale/private hub and management-server reachability, Fleet server version (`GET /api/v1/fleet/version`), tier, API-only user role, `fleetd` package build flags (signed, notarized, scripts enabled), and for each platform the `hosts/identifier` subset fixture (uuid, serial, expected platform, `scripts_enabled: true`). Signing/notarization applies to the Mac package; record the Linux package version and systemd environment separately. Record whether Apple MDM is enrolled; it must not be required for the pass.
2. **Secret handling proof (gate):** deliver a known test variable, run a script that writes it to a `0600` file and prints `ok` → assert `GET /scripts/results` returns `script_contents` with the variable _name_ and `output` without the value; assert the Fleet UI script-result view shows no value. Include secret creation and invalidation API calls; if any operation needs Premium or exposes the value, acceptance fails. Do not enable the adapter or substitute a UUID-only fetch.
3. **Create:** `CreateHost(macos, {hardware_uuid})` → `execution_id` stored; result `exit_code 0` with `BOSS_HANDOFF=launchd` within the 5-minute Fleet timeout; `launchctl print system/com.spinyfin.boss-bootstrap` exists (reported through the probe script).
4. **Idempotency:** second `CreateHost` while the first is pending → no second `scripts/run` call (hub audit log).
5. **Staged results:** probe script output moves `installed` → `enrolled`; hub shows `connected` → `preflight_passed` → `ready`. `boss-worker` exists (`dscl . -read /Users/boss-worker`), state dirs are owner-only, the enrollment token file is gone.
6. **Identity:** enrollment request's hardware UUID equals the Fleet `uuid`; negative: replay the same job's bootstrap on the other Mac → `IdentityMismatch`.
7. **Package verification negatives:** bad signature and bad digest manifests → `BOSS_ERROR=manifest_sig` / `digest_mismatch`, exit codes mapped, no `LaunchDaemon` installed.
8. **Timeout path:** force the stub to block → `host_timeout: true` maps to `BootstrapTimeout`; `Retry` succeeds on the same host.
9. **Offline host:** close the lid; `hosts/{id}` `status` leaves `online`; job shows `HostOffline`; reopening completes the run at next check-in without operator action.
10. **Secret invalidation:** after `enrolled`, the variable's value is overwritten; a replayed stub fails enrollment with the consumed token.
11. **Ready end-to-end:** one real task runs on the Mac spoke and publishes a PR.
12. **Optional MDM comparison (separate entitlement check, not a Free dependency):** `commands/run` `InstallEnterpriseApplication` returns `Acknowledged`; the report must show `installed` came from the probe, not the command status.

13. **Linux parity:** use the [Fleet Linux identity fixture](#fleet-linux-host-identity-resolution) and [secret/handoff/inspection fixtures](#fleet-linux-secret-delivery-systemd-handoff-and-inspection) on a managed home Ubuntu host over Tailscale. Record the DMI, osquery and Fleet UUID comparison, `platform: ubuntu`, `scripts_enabled: true`, and rejection of a wrong-platform host and a mismatched/placeholder UUID. Repeat the Free secret gate on Linux, checking the substituted file's `boss-worker` ownership and `0600` mode without printing its contents, result/UI masking and deletion/invalidation. Record the systemd handoff execution id separately from the probe execution id; handoff success alone must leave the host unschedulable. Prove `installed → enrolled` by probes and `connected → preflight_passed → ready` at the hub; inject a missing Python PTY module or worker credential and require `PreflightFailed`. Repeat signature/digest rejection, expired-token replacement, interrupted-install retry, cancellation and a real worker task publishing a PR. Assert systemd service ownership, private state, as-user preflight in the installed service environment and no cloud API calls. With the spoke stopped, recover through Fleet alone. Cancellation must stop only the selected job's bootstrap/Boss service and remove its token, never delete the host, wipe journals or touch unrelated services. Save sanitized requests/results and hub stage history alongside the report.
14. **Provider independence:** contract tests select Fleet/Linux and Fleet/macOS without GCE credentials. A fake cloud provider uses the same stages while retaining its own identity. Do not require a paid cloud deployment to pass initial local-host acceptance.

## Risks and evidence limits

- **Free secret delivery is unverified.** Published script support does not establish the complete secret lifecycle. The versioned Free acceptance report is a rollout gate; no live provider test was run for this investigation.
- **Fleet contributor API stability.** The secret-write endpoint lives in a contributor reference. Pin/test its contract; fail closed on incompatibility.
- **Physical availability.** Management agents cannot recover a powered-off NUC without out-of-band hardware. Fiber/power loss may disconnect multiple hosts; spoke journal/replay behavior remains investigation §§2.3 and 2.5's responsibility.
- **Management privilege.** Root-running agents can control their hosts; dedicated `boss-worker` separation protects the operator account from workers, not from a compromised management server.
- **Cloud comparison limits.** GCE fixtures are not initial provisioning requirements or measured cost evidence. Confirm API version and retry semantics against a real provider before adopting that adapter; guest attributes must contain no secrets.
- **No deployment effect.** This report installs nothing. The fleet administrator must configure management, enroll agents and qualify Free delivery; implementation must pass signed-package and fresh-host acceptance before scheduling workers.

## Inputs to the existing implementation breakdown

These are changes to the design's existing implementation entries, not claims that this investigation implements or deploys them.

1. **Hub managed provisioning jobs:** provider selection independent of OS; provider-scoped identity, shared stages and typed errors; tests for existing-machine and cloud-machine semantics. Reuse repository HTTP/retry infrastructure.
2. **Managed Linux host provisioning:** target managed self-hosted Linux first using Fleet script run/results, systemd bootstrap and as-user preflight over the private network. Treat GCE as an optional adapter, not a dependency. The parent design's contract and implementation entry now use this self-hosting-first wording.
3. **Managed macOS host provisioning:** Free script path, launchd hand-off, result probes and explicit free-edition secret/permission tests; no Premium package deployment or UUID-only fetch fallback.
4. **Signed spoke packages and services:** publish both platform installers with the signed manifests already required by the design. Shared Fleet job logic stays independent of platform-specific service setup.

## Sources

Repository sources (primary investigation read from `main@origin`; other documents from this checkout): `.buildkite/README.md`, `.buildkite/gce-agent-runbook.md`, `.buildkite/linux-agents-runbook.md`, `tools/release/release.toml`, `tools/boss/engine/core/src/host_provisioning.rs`, `tools/boss/docs/designs/hub-and-spoke-distributed-engine.md`, `tools/boss/docs/investigations/hub-and-spoke-distributed-engine-2026-10-21.md`.

Provider documentation consulted on 2026-10-22:

- Compute Engine: [Verifying the identity of instances](https://docs.cloud.google.com/compute/docs/instances/verifying-instance-identity), [Manage guest attributes](https://docs.cloud.google.com/compute/docs/metadata/manage-guest-attributes), [`instances.getGuestAttributes`](https://docs.cloud.google.com/compute/docs/reference/rest/v1/instances/getGuestAttributes), [`instances.insert`](https://docs.cloud.google.com/compute/docs/reference/rest/v1/instances/insert), [API versioning guide](https://docs.cloud.google.com/compute/docs/api/how-tos/api-versioning-guide)
- Secret Manager: [`projects.secrets.versions.access`](https://docs.cloud.google.com/secret-manager/docs/reference/rest/v1/projects.secrets.versions/access), [`SecretPayload`](https://docs.cloud.google.com/secret-manager/docs/reference/rest/v1/SecretPayload)
- cloud-init: [Google Compute Engine datasource](https://docs.cloud-init.io/en/25.2/reference/datasources/gce.html)
- Fleet: [REST API](https://fleetdm.com/docs/rest-api/rest-api) (scripts, hosts, software, commands), [Scripts guide](https://fleetdm.com/guides/scripts), [Secrets in scripts and configuration profiles](https://fleetdm.com/guides/secrets-in-scripts-and-configuration-profiles), [fleetd authentication](https://fleetdm.com/guides/fleetd-authentication), [fleetd](https://fleetdm.com/docs/using-fleet/fleetd), [Enroll hosts](https://fleetdm.com/docs/using-fleet/enroll-hosts), [Pricing](https://fleetdm.com/pricing), [contributor API reference](https://github.com/fleetdm/fleet/blob/main/docs/Contributing/reference/api-for-contributors.md) (`secret_variables`), [`server/datastore/mysql/secret_variables.go`](https://github.com/fleetdm/fleet/blob/main/server/datastore/mysql/secret_variables.go)
- Apple: [Install Enterprise Application command](https://developer.apple.com/documentation/devicemanagement/install-enterprise-application-command)
- Endpoint Central: [edition matrix](https://www.manageengine.com/products/desktop-central/edition-comparison-matrix.html), [server requirements](https://www.manageengine.com/products/desktop-central/help/server/system-requirements.html), [Cloud API index](https://www.manageengine.com/products/desktop-central/help/api/cloud/api-index.html).
