#!/usr/bin/env bash
# Idempotently install/configure node_exporter on a macOS RBE worker under
# our own KeepAlive=true LaunchAgent, migrating off sh.brew.node_exporter.
set -euo pipefail

[[ "$(uname -s)" == "Darwin" ]] || {
  echo "setup-macos.sh must run on macOS" >&2
  exit 1
}

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
uid="$(id -u)"
support_dir="${HOME}/Library/Application Support/com.brianduff.node_exporter"
launch_agents_dir="${HOME}/Library/LaunchAgents"
logs_dir="${HOME}/Library/Logs/node_exporter"
plist_dest="${launch_agents_dir}/com.brianduff.node_exporter.plist"
runner_dest="${support_dir}/run.sh"

if ! brew list node_exporter >/dev/null 2>&1; then
  brew install node_exporter
fi

# Migrate off Homebrew's service (which has KeepAlive=false).
brew services stop node_exporter >/dev/null 2>&1 || true
launchctl bootout "gui/${uid}/sh.brew.node_exporter" >/dev/null 2>&1 || true
rm -f "${launch_agents_dir}/sh.brew.node_exporter.plist"

mkdir -p "${support_dir}" "${launch_agents_dir}" "${logs_dir}"
install -m 0755 "${here}/run-macos.sh" "${runner_dest}"

rendered_plist="$(mktemp)"
trap 'rm -f "${rendered_plist}"' EXIT
sed "s|USER_HOME|${HOME}|g" "${here}/com.brianduff.node_exporter.plist" > "${rendered_plist}"
install -m 0644 "${rendered_plist}" "${plist_dest}"

launchctl bootout "gui/${uid}/com.brianduff.node_exporter" >/dev/null 2>&1 || true
launchctl bootstrap "gui/${uid}" "${plist_dest}"
launchctl kickstart -k "gui/${uid}/com.brianduff.node_exporter" >/dev/null 2>&1 || true

tailscale_bin="$(command -v tailscale || true)"
if [[ -z "${tailscale_bin}" && -x /Applications/Tailscale.app/Contents/MacOS/Tailscale ]]; then
  tailscale_bin="/Applications/Tailscale.app/Contents/MacOS/Tailscale"
fi
tailscale_ip=""
if [[ -n "${tailscale_bin}" ]]; then
  tailscale_ip="$("${tailscale_bin}" ip -4 2>/dev/null | head -n1 || true)"
fi

if [[ -n "${tailscale_ip}" ]]; then
  echo "Installed com.brianduff.node_exporter bound to ${tailscale_ip}:9100"
else
  echo "Installed com.brianduff.node_exporter (will bind to Tailscale IPv4:9100 once Tailscale is up)"
fi
