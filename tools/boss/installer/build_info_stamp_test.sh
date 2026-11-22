#!/usr/bin/env bash
# Guard: non-release builds compile a constant placeholder version, and
# stamped builds embed the tag-derived version from workspace-status.sh.
#
# build_info_rs is compiled into engine_lib, boss, and bossctl. If the
# unstamped output read stable-status.txt, every new boss-v* tag would
# change that file's action key and rebuild 40–80 downstream test
# targets. Unstamped output must therefore be a byte-exact constant
# (no stable-status.txt input). stamp=1 (always stamp) is the positive
# control that --stamp still embeds STABLE_BOSS_BASE_VERSION.
#
# Args:
#   $1 — unstamped build_info_rs (stamp = -1, this test runs --nostamp)
#   $2 — always-stamped build_info_rs (stamp = 1)
#   $3 — unstamped version plist (stamp = 0)
#   $4 — always-stamped version plist (stamp = 1)
#   $5 — //tools/boss/cli:boss
#   $6 — //tools/boss/bossctl
set -euo pipefail

unstamped_rs="$1"
stamped_rs="$2"
unstamped_plist="$3"
stamped_plist="$4"
boss_bin="$5"
bossctl_bin="$6"

expect_line() {
  local file="$1" line="$2"
  if ! grep -qxF "$line" "$file"; then
    echo "ERROR: $file is missing line: $line" >&2
    echo "contents:" >&2
    cat -v "$file" >&2
    echo >&2
    exit 1
  fi
}

expect_line "$unstamped_rs" 'pub const BOSS_VERSION: &str = "0.0.0-dev";'
expect_line "$unstamped_rs" 'pub const BOSS_GIT_SHA: &str = "unknown";'
expect_line "$unstamped_rs" 'pub const BOSS_BUILD_TIME: &str = "unknown";'
if grep -q '1\.0\.' "$unstamped_rs"; then
  echo "ERROR: unstamped build_info_rs contains a tag-derived 1.0.* version:" >&2
  cat -v "$unstamped_rs" >&2
  echo >&2
  echo "An unstamped build must not read STABLE_BOSS_BASE_VERSION; a new" >&2
  echo "boss-v* tag would otherwise bust the action cache of engine_lib and tests." >&2
  exit 1
fi

stamped_line=""
while IFS= read -r line; do
  case "$line" in
    'pub const BOSS_VERSION: &str = "'*)
      stamped_line="$line"
      break
      ;;
  esac
done <"$stamped_rs"
stamped_version="${stamped_line#*\"}"
stamped_version="${stamped_version%%\"*}"
if [[ -z "$stamped_version" ]]; then
  echo "ERROR: always-stamped build_info_rs has no BOSS_VERSION: $(<"$stamped_rs")" >&2
  exit 1
fi
if [[ "$stamped_version" == "0.0.0-dev" ]]; then
  echo "ERROR: always-stamped build_info_rs still has the unstamped placeholder 0.0.0-dev" >&2
  echo "A --stamp / stamp=1 build must embed STABLE_BOSS_BASE_VERSION." >&2
  echo "contents: $(<"$stamped_rs")" >&2
  exit 1
fi

if ! grep -q '<string>0.0.0</string>' "$unstamped_plist"; then
  echo "ERROR: unstamped plist is missing CFBundle version 0.0.0:" >&2
  cat "$unstamped_plist" >&2
  exit 1
fi
if ! grep -q '<string>0.0.0-dev-unstamped</string>' "$unstamped_plist"; then
  echo "ERROR: unstamped plist is missing BossFullVersion 0.0.0-dev-unstamped:" >&2
  cat "$unstamped_plist" >&2
  exit 1
fi

if grep -q '<string>0.0.0-dev-unstamped</string>' "$stamped_plist"; then
  echo "ERROR: always-stamped plist still has the unstamped BossFullVersion placeholder:" >&2
  cat "$stamped_plist" >&2
  exit 1
fi

boss_version="$("$boss_bin" --version)"
if [[ "$boss_version" != "boss 0.0.0-dev" ]]; then
  echo "ERROR: unstamped boss --version was '${boss_version}', expected 'boss 0.0.0-dev'" >&2
  exit 1
fi

bossctl_version="$("$bossctl_bin" --version)"
if [[ "$bossctl_version" != "bossctl 0.0.0-dev" ]]; then
  echo "ERROR: unstamped bossctl --version was '${bossctl_version}', expected 'bossctl 0.0.0-dev'" >&2
  exit 1
fi

echo "OK: unstamped builds report 0.0.0-dev; stamped build_info_rs embeds ${stamped_version}."
