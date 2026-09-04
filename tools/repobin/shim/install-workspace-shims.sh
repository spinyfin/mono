#!/usr/bin/env bash
# install-workspace-shims.sh — populate <workspace>/bin/ with repobin shims.
#
# Generic repobin machinery, not mono/Boss-specific: any repobin-using
# checkout can run this by hand from anywhere inside its tree. It makes the
# gitignored bin/ look like CI's: one entry per eligible tool declared in
# REPOBIN.toml plus `repobin` itself. CI's entries are real `repobin install`
# symlinks to a bazel-built repobin binary; here each entry is a symlink to
# repobin-shim.sh, which builds that same binary on first use and dispatches
# through it (see the shim's header for why the build is deferred rather than
# done at lease time).
#
# Two things are repo-specific and are read from the environment rather than
# hardcoded, so this script has no opinion about who owns which tool names:
#   * REPOBIN_SHIM_SKIP — space-separated tool names to never shim (e.g. a
#     repo whose engine owns its own launchers for some names declared in
#     REPOBIN.toml). Defaults to empty: nothing is skipped.
#   * REPOBIN_SHIM_REQUIRE — space-separated tool names that must end up
#     executable in bin/ after install, or the script fails loudly. Defaults
#     to empty: nothing is required. mono runs this (`.cube/setup.yaml`, step
#     `repobin-tool-shims`) with REPOBIN_SHIM_SKIP='boss boss-event bossctl
#     cube' (those four are engine-owned launchers) and
#     REPOBIN_SHIM_REQUIRE=checkleft (a workspace without a usable checkleft
#     gate must fail its lease rather than let a worker discover it later).
#
# Behaviour:
#   * Cheap and idempotent: only symlinks are written, so `run_when: always`
#     costs nothing and self-heals a wiped bin/.
#   * Never clobbers a real `repobin install` (bin/repobin is a regular
#     file): that is strictly better than the shim, so bin/ is left alone.
#   * Never overwrites a regular file or directory at a tool's name; warns
#     and leaves it.
#   * Exits non-zero if, after installing, any REPOBIN_SHIM_REQUIRE name is
#     not executable in bin/.
#
# Tested by //tools/repobin/shim:repobin_shim_test.
set -euo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
root="$(cd -- "$here/../../.." && pwd -P)"
config="$root/REPOBIN.toml"
bin="$root/bin"
shim="$here/repobin-shim.sh"
shim_rel="../tools/repobin/shim/repobin-shim.sh"
skip_names="${REPOBIN_SHIM_SKIP:-}"
require_names="${REPOBIN_SHIM_REQUIRE:-}"

die() {
  printf 'install-workspace-shims: %s\n' "$@" >&2
  exit 1
}

[[ -f "$config" ]] || die "no REPOBIN.toml at $root"
[[ -x "$shim" ]] || die "$shim is missing or not executable"

if [[ -f "$bin/repobin" && ! -L "$bin/repobin" ]]; then
  echo "install-workspace-shims: $bin/repobin is a real \`repobin install\`; leaving bin/ untouched"
  exit 0
fi

mkdir -p "$bin"

# Tool names: every `[tools.<name>]` and `[pins.<name>]` table header, with
# leading/trailing whitespace ignored just like the shim and worker launcher.
names="$(sed -n -E 's/^[[:space:]]*\[(tools|pins)\.([^]]+)\][[:space:]]*$/\2/p' "$config")"
[[ -n "$names" ]] || die "REPOBIN.toml at $root declares no [tools.*] or [pins.*] entries"

is_skipped() {
  local candidate="$1" skip
  for skip in $skip_names; do
    [[ "$candidate" == "$skip" ]] && return 0
  done
  return 1
}

installed=0
kept=0
for name in repobin $names; do
  is_skipped "$name" && continue
  entry="$bin/$name"
  if [[ -L "$entry" ]]; then
    # Already our shim? (`-ef` compares inodes through the symlink; no
    # `readlink`, which the audited test toolset does not provide.)
    if [[ "$entry" -ef "$shim" ]]; then
      kept=$((kept + 1))
      continue
    fi
  elif [[ -e "$entry" ]]; then
    echo "install-workspace-shims: warning: $entry exists and is not a symlink; leaving it alone" >&2
    continue
  fi
  tmp="$bin/.$name.$$.tmp"
  ln -s "$shim_rel" "$tmp"
  mv -f "$tmp" "$entry"
  installed=$((installed + 1))
done

# The property REPOBIN_SHIM_REQUIRE exists for (mono requires `checkleft`):
# fail the lease rather than hand out a workspace missing a tool the caller
# declared load-bearing.
for name in $require_names; do
  [[ -x "$bin/$name" ]] || die "$bin/$name is not executable after install; REPOBIN_SHIM_REQUIRE named it as required"
done

echo "install-workspace-shims: bin/ ready ($installed installed, $kept already present) -> $shim_rel for: repobin $(printf '%s ' $names)"
