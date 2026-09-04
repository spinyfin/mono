#!/usr/bin/env bash
# install-workspace-shims.sh — populate <workspace>/bin/ with repobin shims.
#
# Run by cube on every lease of a mono workspace (`.cube/setup.yaml`, step
# `repobin-tool-shims`), and safe to run by hand from anywhere inside the
# checkout. It makes the gitignored bin/ look like CI's: one entry per tool
# declared in REPOBIN.toml plus `repobin` itself. CI's entries are real
# `repobin install` symlinks to a bazel-built repobin binary; here each entry
# is a symlink to repobin-shim.sh, which builds that same binary on first use
# and dispatches through it (see the shim's header for why the build is
# deferred rather than done at lease time).
#
# Behaviour:
#   * Cheap and idempotent: only symlinks are written, so `run_when: always`
#     costs nothing and self-heals a wiped bin/.
#   * Never clobbers a real `repobin install` (bin/repobin is a regular
#     file): that is strictly better than the shim, so bin/ is left alone.
#   * Never overwrites a regular file or directory at a tool's name; warns
#     and leaves it.
#   * Exits non-zero if, after installing, bin/checkleft is not executable —
#     a workspace without a usable checkleft gate must fail its lease loudly
#     rather than let a worker discover it (and improvise) later.
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

[[ -f "$config" ]] || die "no REPOBIN.toml at $root"
[[ -x "$shim" ]] || die "$shim is missing or not executable"

if [[ -f "$bin/repobin" && ! -L "$bin/repobin" ]]; then
  echo "install-workspace-shims: $bin/repobin is a real \`repobin install\`; leaving bin/ untouched"
  exit 0
fi

mkdir -p "$bin"

# Tool names: every `[tools.<name>]` and `[pins.<name>]` table header.
names="$(sed -n -E 's/^\[(tools|pins)\.([^]]+)\][[:space:]]*$/\2/p' "$config")"
[[ -n "$names" ]] || die "REPOBIN.toml at $root declares no [tools.*] or [pins.*] entries"

installed=0
kept=0
for name in repobin $names; do
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

# The property this whole mechanism exists for. Fail the lease rather than
# hand out a workspace whose checkleft gate a worker would have to improvise.
[[ -x "$bin/checkleft" ]] || die "$bin/checkleft is not executable after install; a workspace without a usable checkleft gate is not healthy"

echo "install-workspace-shims: bin/ ready ($installed installed, $kept already present) -> $shim_rel for: repobin $(printf '%s ' $names)"
