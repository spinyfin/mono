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
# Skip/require lists are repo-specific. This checkout's defaults live in
# `workspace-shim-policy` next to this script (the lease, AGENTS.md and the
# push-guard repair all invoke this script and pick those defaults up). A
# checkout that copies the installer without a policy file, or that unsets
# the lists, shims every declared tool and requires nothing. Environment
# variables override the policy file when set:
#   * REPOBIN_SHIM_SKIP — space-separated tool names to never shim (e.g. a
#     repo whose engine owns its own launchers for some names declared in
#     REPOBIN.toml).
#   * REPOBIN_SHIM_REQUIRE — space-separated tool names that must end up
#     executable in bin/ after install, or the script fails loudly.
#
# Behaviour:
#   * Cheap and idempotent: only symlinks are written, so `run_when: always`
#     costs nothing and self-heals a wiped bin/.
#   * Never clobbers a real `repobin install` (bin/repobin is a regular
#     file): that is strictly better than the shim, so bin/ is left alone.
#   * Never overwrites a regular file or directory at a tool's name; warns
#     and leaves it.
#   * Removes an existing symlink for a skipped name when it resolves to
#     this installer's repobin-shim.sh, so a later lease converges after an
#     unscoped install wrote engine-owned names.
#   * Exits non-zero if, after installing, any required name is not
#     executable in bin/.
#
# Tested by //tools/repobin/shim:repobin_shim_test.
set -euo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
root="$(cd -- "$here/../../.." && pwd -P)"
config="$root/REPOBIN.toml"
bin="$root/bin"
shim="$here/repobin-shim.sh"
shim_rel="../tools/repobin/shim/repobin-shim.sh"

die() {
  printf 'install-workspace-shims: %s\n' "$@" >&2
  exit 1
}

# Policy file next to this script supplies this checkout's defaults. Env
# vars win when set (including to empty), so tests can still force the
# generic "nothing skipped, nothing required" behaviour.
policy_skip=""
policy_require=""
policy="$here/workspace-shim-policy"
if [[ -f "$policy" ]]; then
  while IFS= read -r line || [[ -n "$line" ]]; do
    case "$line" in
      '#'* | '') continue ;;
      SKIP=*) policy_skip="${line#SKIP=}" ;;
      REQUIRE=*) policy_require="${line#REQUIRE=}" ;;
    esac
  done < "$policy"
fi
if [[ -n "${REPOBIN_SHIM_SKIP+x}" ]]; then
  skip_names="$REPOBIN_SHIM_SKIP"
else
  skip_names="$policy_skip"
fi
if [[ -n "${REPOBIN_SHIM_REQUIRE+x}" ]]; then
  require_names="$REPOBIN_SHIM_REQUIRE"
else
  require_names="$policy_require"
fi

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

# Drop leftover shims for skipped names (declared or not) so a workspace
# that once ran this installer without the skip list converges on the next
# lease. Regular files and unrelated symlinks are left alone.
for name in $skip_names; do
  entry="$bin/$name"
  if [[ -L "$entry" && "$entry" -ef "$shim" ]]; then
    rm -f "$entry"
  fi
done

# The property REPOBIN_SHIM_REQUIRE exists for (this checkout requires
# `checkleft`): fail the lease rather than hand out a workspace missing a
# tool the caller declared load-bearing.
for name in $require_names; do
  [[ -x "$bin/$name" ]] || die "$bin/$name is not executable after install; REPOBIN_SHIM_REQUIRE named it as required"
done

echo "install-workspace-shims: bin/ ready ($installed installed, $kept already present) -> $shim_rel for: repobin $(printf '%s ' $names)"
