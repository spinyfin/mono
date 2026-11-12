# Optional spoke hub listener

The engine installs its spoke host provider on the shared hub listener configured with `BOSS_HUB_LISTEN`, `BOSS_HUB_CREDENTIALS_DIR`, `BOSS_HUB_TLS_CERT`, and `BOSS_HUB_TLS_KEY`. `BOSS_SPOKE_HUB_CONFIG` names a JSON file supplying remote launch paths. Without listener configuration it uses the existing local and SSH providers. If the work database already records spoke history or a ready spoke host, missing configuration stops startup instead of routing a spoke-owned workspace through SSH.

The listener is **off by default**. An operator supplies the server identity and configuration; enrollment must authorize the host, and the host transport must reach `spoke_ready` before new executions use it. Loading this file does not enroll hosts, change readiness, or migrate local workers. Existing SSH executions retain their recorded transport.

```json
{
  "hosts": {
    "worker-host": {
      "events_socket": "/private/boss-spoke/events.sock",
      "bazel_workspace": true
    }
  }
}
```

The server certificate identifies the hub endpoint; it is separate from the host enrollment CA. Credential state uses the enrollment crate's owner-only directories. Host paths name files on that host and are used to render worker hooks; the hub does not open them. `bazel_workspace` supplies the remote repository's build-system policy to the shared prompt composer. Keep the hub identity and authority generation stable across ordinary restarts.

The listener mounts enrollment and authenticated spoke streams together. Dispatch uses the same stream manager and durable command store. Invalid configuration, invalid TLS material, and a failed bind prevent startup. A later listener failure is logged; stored commands and ownership remain in the work database.

Lease and spawn timeouts retain execution ownership and reconcile the stored command every five seconds. They do not count as pre-start failures or release the worker's lease or persona. Probes use the host adapter even without a local pane slot. A delivered probe is buffered until its reply; an uncertain delivery remains in flight and is not automatically duplicated. A release-time Stop waits at most one second for a reply, while subsequent attempts reuse its durable command identity.

The shared remote composer supplies effort, environment and host-side output paths and requires the corresponding launch-input protocol feature. It never embeds hub-local artifact paths in remote prompts. End-to-end availability also requires the provisioned host's event-ingress service and readiness checks; the listener setting alone is not a readiness assertion.

A configured listener also applies what spokes report. The hub stores each journaled record before acknowledging it, and a separate projector then applies the stored records in order: hook events take the ordinary worker-event fan-out, queued proposals go through the `propose` validators and apply pipeline, and pane observations are reflected in live state. Receipt and application have independent cursors, so a crash between them replays locally and never makes the spoke resend. See [spoke observation projection](spoke-observation-projection.md).
