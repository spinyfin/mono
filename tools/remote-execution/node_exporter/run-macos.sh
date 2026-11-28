#!/usr/bin/env bash
set -euo pipefail

# Resolve the Tailscale IPv4 address on each launch. If Tailscale is not yet
# up at login, exit non-zero so launchd (KeepAlive + ThrottleInterval) retries
# without ever falling back to 0.0.0.0.
tailscale_bin="$(command -v tailscale || true)"
if [[ -z "${tailscale_bin}" && -x /Applications/Tailscale.app/Contents/MacOS/Tailscale ]]; then
  tailscale_bin="/Applications/Tailscale.app/Contents/MacOS/Tailscale"
fi
if [[ -z "${tailscale_bin}" ]]; then
  echo "tailscale CLI not found; exiting for launchd retry" >&2
  exit 1
fi

tailscale_ip="$("${tailscale_bin}" ip -4 2>/dev/null | head -n1 || true)"
if [[ -z "${tailscale_ip}" ]]; then
  echo "Tailscale IPv4 address not yet available; exiting for launchd retry" >&2
  exit 1
fi

node_exporter_bin="$(command -v node_exporter || true)"
if [[ -z "${node_exporter_bin}" && -x /opt/homebrew/bin/node_exporter ]]; then
  node_exporter_bin="/opt/homebrew/bin/node_exporter"
fi
if [[ -z "${node_exporter_bin}" ]]; then
  echo "node_exporter binary not found" >&2
  exit 1
fi

exec "${node_exporter_bin}" --web.listen-address="${tailscale_ip}:9100"
