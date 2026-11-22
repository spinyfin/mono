#!/usr/bin/env bash
# Idempotently install and configure prometheus-node-exporter on Ubuntu.
#   --mode hub     Bind to 127.0.0.1:9100 (for headcrash where Prometheus runs)
#   --mode worker  Bind to the host's Tailscale IPv4 on port 9100
set -euo pipefail

mode=""
while (($# > 0)); do
  case "$1" in
    --mode)
      mode="${2:-}"
      shift 2
      ;;
    *)
      echo "usage: setup-ubuntu.sh --mode <hub|worker>" >&2
      exit 1
      ;;
  esac
done

case "${mode}" in
  hub)
    listen_ip="127.0.0.1"
    ;;
  worker)
    listen_ip="$(tailscale ip -4 | head -n1)"
    if [[ -z "${listen_ip}" ]]; then
      echo "Tailscale IPv4 address not found; bring up Tailscale first" >&2
      exit 1
    fi
    ;;
  *)
    echo "usage: setup-ubuntu.sh --mode <hub|worker>" >&2
    exit 1
    ;;
esac

if ! dpkg -s prometheus-node-exporter >/dev/null 2>&1; then
  sudo apt-get update
  sudo apt-get install -y prometheus-node-exporter
fi

desired="ARGS=\"--web.listen-address=${listen_ip}:9100\""
defaults_file="/etc/default/prometheus-node-exporter"
changed=0
if [[ ! -f "${defaults_file}" ]] || [[ "$(cat "${defaults_file}")" != "${desired}" ]]; then
  printf '%s\n' "${desired}" | sudo tee "${defaults_file}" >/dev/null
  changed=1
fi

sudo systemctl enable --now prometheus-node-exporter.service
if ((changed == 1)); then
  sudo systemctl restart prometheus-node-exporter.service
fi
sudo systemctl is-active --quiet prometheus-node-exporter.service
echo "Configured prometheus-node-exporter bound to ${listen_ip}:9100"
