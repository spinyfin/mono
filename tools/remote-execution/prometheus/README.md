# NativeLink Prometheus config and node_exporter setup

`prometheus.yml` is the versioned `/etc/prometheus/prometheus.yml` for `headcrash`, covering Prometheus self-scraping, NativeLink OTLP metrics (`nativelink`), hub admin pool metrics (`nativelink-pool`), and per-host `node_exporter` targets (`node`).

## Publishing

`tools/remote-execution/grafana/publish.sh` publishes `prometheus.yml` in the same Buildkite step as the Grafana dashboards whenever `tools/remote-execution/prometheus/**` or `tools/remote-execution/grafana/**` changes on `main`.

The root-owned helper `install.py` validates the incoming config with both `validate.py` and `/usr/local/bin/promtool check config` on a temporary file before touching `/etc/prometheus/prometheus.yml`. If validation fails, it aborts without modifying the live config. When the config changes, it replaces `/etc/prometheus/prometheus.yml` atomically and applies it with `systemctl reload prometheus.service` (never `restart`).

### One-time enablement on headcrash

**Prometheus publishing is inactive until a host administrator installs the helper and sudoers rule on headcrash:**

1. From a reviewed `mono` checkout on `headcrash`, install `install.py` (`0755`) and `validate.py` (`0644`) into `/usr/local/libexec/nativelink-prometheus/`, owned by `root:root` in a `0755` `root:root` directory.
2. Confirm `/etc/prometheus` is `root:root` and not group/world writable, and `/usr/local/bin/promtool` exists.
3. Add this rule via `visudo` (empty argument string forbids arguments):

   ```sudoers
   nativelink-publisher ALL=(root) NOPASSWD: /usr/local/libexec/nativelink-prometheus/install.py ""
   ```

## Adding a worker host to monitoring (end to end)

1. **Install and start `node_exporter` on the host** (never bind to `0.0.0.0`):
   - **macOS worker:** run `bash tools/remote-execution/node_exporter/setup-macos.sh`. It installs `node_exporter` via Homebrew, removes Homebrew's `sh.brew.node_exporter` service, and installs `~/Library/LaunchAgents/com.brianduff.node_exporter.plist` (`KeepAlive=true`, `ThrottleInterval=10`), which runs `run-macos.sh` to bind `<tailscale-ip>:9100` and retry every 10s if Tailscale is not yet up at login.
   - **Ubuntu worker:** run `bash tools/remote-execution/node_exporter/setup-ubuntu.sh --mode worker` (or `--mode hub` on `headcrash` to bind `127.0.0.1:9100`).
2. **Verify the exporter is listening only on the expected IP:**

   ```sh
   curl -fsS "http://$(tailscale ip -4 | head -n1):9100/metrics" | head -n 5
   ```

3. **Register the host in `tools/remote-execution/prometheus/prometheus.yml`:** add a `static_configs` entry under the `node` job with its Tailscale IPv4 on port `9100` and labels (`instance`, `platform`, `pool`, plus `worker_id` for macOS workers).
4. **Validate locally and open a PR:**

   ```sh
   bazel test --remote_download_outputs=minimal //tools/remote-execution/prometheus:prometheus_test
   bin/checkleft run
   ```

   Merge to `main`; Buildkite validates with `promtool check config` and reloads Prometheus on `headcrash`. Confirm the new target shows `UP` in Prometheus (`http://127.0.0.1:9090/targets` on `headcrash`) and appears on the `NativeLink Workers` Grafana dashboard.

## Migrating the 6 existing macOS hosts off `sh.brew.node_exporter`

Homebrew's `sh.brew.node_exporter` plist sets `KeepAlive` to `false`, so `node_exporter` stays down if Tailscale is not ready when the LaunchAgent loads at login. Run `bash tools/remote-execution/node_exporter/setup-macos.sh` on each existing macOS worker (`anaplian`, `arbitrary`, `determinist`, `rascal`, `skaffen`, `zakalwe`). The script is idempotent: it stops and boots out `sh.brew.node_exporter`, removes `~/Library/LaunchAgents/sh.brew.node_exporter.plist`, and bootstraps `com.brianduff.node_exporter`.

Verify on each host:

```sh
launchctl print "gui/$(id -u)/com.brianduff.node_exporter" | grep -E 'state =|pid ='
lsof -nP -iTCP:9100 -sTCP:LISTEN
```
