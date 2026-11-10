# Optional spoke hub listener

The engine installs its spoke host provider when `BOSS_SPOKE_HUB_CONFIG` names a JSON configuration file. Without that setting it uses the existing local and SSH providers. If the work database already records spoke history or a ready spoke host, missing configuration stops startup instead of routing a spoke-owned workspace through SSH.

The listener is **off by default**. An operator supplies the server identity and configuration; enrollment must authorize the host, and the host transport must reach `spoke_ready` before new executions use it. Loading this file does not enroll hosts, change readiness, or migrate local workers. Existing SSH executions retain their recorded transport.

```json
{
  "listen": "192.0.2.10:7443",
  "hub_id": "hub",
  "authority_generation": "authority-generation",
  "credentials_dir": "/private/boss/host-credentials",
  "certificate": "/private/boss/server.pem",
  "private_key": "/private/boss/server.key",
  "hosts": {
    "worker-host": {
      "events_socket": "/private/boss-spoke/events.sock",
      "boss_event": "/opt/boss/bin/boss-event",
      "bazel_workspace": true
    }
  }
}
```

The server certificate identifies the hub endpoint; it is separate from the host enrollment CA. Credential state uses the enrollment crate's owner-only directories. Host paths name files on that host and are used to render worker hooks; the hub does not open them. `bazel_workspace` supplies the remote repository's build-system policy to the shared prompt composer. Keep the hub identity and authority generation stable across ordinary restarts.

The listener mounts enrollment and authenticated spoke streams together. Dispatch uses the same stream manager and durable command store. Invalid configuration, invalid TLS material, and a failed bind prevent startup. A later listener failure is logged; stored commands and ownership remain in the work database.

Lease and spawn timeouts retain execution ownership and reconcile the stored command every five seconds. They do not count as pre-start failures or release the worker's lease or persona. Probes use the host adapter even without a local pane slot. A delivered probe is buffered until its reply; an uncertain delivery remains in flight and is not automatically duplicated. A release-time Stop waits at most one second for a reply, while subsequent attempts reuse its durable command identity.

The composer refuses worker kinds requiring structured-output custody that the spoke adapter cannot collect. This fails before spawning rather than embedding a hub-local artifact path in a remote prompt. End-to-end availability also requires the provisioned host's event-ingress service and readiness checks; the listener setting alone is not a readiness assertion.
