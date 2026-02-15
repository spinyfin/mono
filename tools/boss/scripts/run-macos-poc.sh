#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: run-macos-poc.sh [--skip-install]

Starts the Boss macOS PoC app and auto-launches the engine.

Required environment variables:
  ANTHROPIC_API_KEY   API key for Claude ACP.

Optional environment variables:
  BOSS_ACP_CMD        ACP adapter command override.
  BOSS_ENGINE_LOG_PATH
  BOSS_SOCKET_PATH    Unix socket path (default /tmp/boss-engine.sock).
  BOSS_ENGINE_AUTOSTART
  BOSS_ENGINE_CMD
  RUST_LOG
  BOSS_SKIP_INSTALL   Set to 1 to skip pnpm install (same effect as --skip-install).
EOF
}

skip_install=0
while (($# > 0)); do
  case "$1" in
    --skip-install)
      skip_install=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
  shift
done

if [[ "${BOSS_SKIP_INSTALL:-0}" == "1" ]]; then
  skip_install=1
fi

if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
  echo "ANTHROPIC_API_KEY is required." >&2
  echo "Example: export ANTHROPIC_API_KEY=... && $0" >&2
  exit 1
fi

for cmd in bazel pnpm swift node; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "Missing required command: $cmd" >&2
    exit 1
  fi
done

node_major="$(node -p 'Number(process.versions.node.split(".")[0])')"
if (( node_major < 22 )); then
  latest_nvm_bin="$(ls -1d "$HOME"/.nvm/versions/node/v*/bin 2>/dev/null | sort -V | tail -1 || true)"
  if [[ -n "${latest_nvm_bin}" ]]; then
    export PATH="${latest_nvm_bin}:$PATH"
    node_major="$(node -p 'Number(process.versions.node.split(".")[0])')"
  fi
fi

if (( node_major < 22 )); then
  echo "Node >=22 is required for @zed-industries/claude-code-acp (found $(node -v))." >&2
  echo "Install/switch Node (e.g. nvm use 24) and retry." >&2
  exit 1
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../../.." && pwd)"

if (( skip_install == 0 )); then
  (
    cd "$repo_root"
    pnpm install --frozen-lockfile
  )
fi

export BOSS_ACP_CMD="${BOSS_ACP_CMD:-pnpm --filter @mono/claude-code-acp exec claude-code-acp}"
export BOSS_ENGINE_LOG_PATH="${BOSS_ENGINE_LOG_PATH:-/tmp/boss-engine.log}"
export RUST_LOG="${RUST_LOG:-info,acp_stderr=debug}"

echo "Launching BossMacApp..."
echo "Repo: $repo_root"
echo "Node: $(node -v)"
echo "BOSS_ACP_CMD: $BOSS_ACP_CMD"
echo "BOSS_ENGINE_LOG_PATH: $BOSS_ENGINE_LOG_PATH"
echo "RUST_LOG: $RUST_LOG"

exec swift run --package-path "$repo_root/tools/boss/app-macos" BossMacApp
