#!/bin/bash
#
# Guard: the bundled macOS @host_tmux//:tmux binary and its lib/*.dylib
# runfiles must be self-contained: every Mach-O load command must point at
# /usr/lib/, /System/, or an @loader_path/ target that exists in runfiles.

set -euo pipefail

if [[ "$#" -ne 1 ]]; then
  printf 'usage: %s <tmux-runfile>\n' "$0" >&2
  exit 2
fi

tmux_entry="$1"
if [[ ! -e "${tmux_entry}" ]]; then
  printf 'FAIL: %s does not exist\n' "${tmux_entry}" >&2
  exit 1
fi

# Resolve the symlink to tmux.bin inside the @host_tmux runfiles directory.
tmux_dir="$(cd "$(dirname "${tmux_entry}")" && pwd)"
tmux_name="$(basename "${tmux_entry}")"
if [[ -L "${tmux_entry}" ]]; then
  link_target="$(readlink "${tmux_entry}")"
  if [[ "${link_target}" == /* ]]; then
    tmux_bin="${link_target}"
  else
    tmux_bin="$(cd "${tmux_dir}/$(dirname "${link_target}")" && pwd)/$(basename "${link_target}")"
  fi
else
  tmux_bin="${tmux_dir}/${tmux_name}"
fi
bundle_dir="$(dirname "${tmux_bin}")"

check_macho() {
  local macho="$1"
  local is_dylib="$2"
  local macho_dir
  macho_dir="$(dirname "${macho}")"

  local line_num=0
  while IFS= read -r line; do
    line_num=$((line_num + 1))
    # Line 1 is the Mach-O header ("<path>:").
    if [[ "${line_num}" -eq 1 ]]; then
      continue
    fi
    # Line 2 of a dylib is its own LC_ID_DYLIB.
    if [[ "${is_dylib}" -eq 1 && "${line_num}" -eq 2 ]]; then
      continue
    fi
    local dep="${line#"${line%%[![:space:]]*}"}"
    dep="${dep%% (compatibility version*}"
    if [[ -z "${dep}" ]]; then
      continue
    fi
    case "${dep}" in
      /usr/lib/*|/System/*)
        ;;
      @loader_path/*)
        local rel="${dep#@loader_path/}"
        if [[ ! -e "${macho_dir}/${rel}" ]]; then
          printf 'FAIL: %s references %s, but %s does not exist in runfiles\n' "${macho}" "${dep}" "${macho_dir}/${rel}" >&2
          exit 1
        fi
        ;;
      *)
        printf 'FAIL: %s has non-self-contained dependency %s\n' "${macho}" "${dep}" >&2
        exit 1
        ;;
    esac
  done < <(/usr/bin/otool -L "${macho}")
}

check_macho "${tmux_bin}" 0
dylib_count=0
if [[ -d "${bundle_dir}/lib" ]]; then
  for dylib in "${bundle_dir}/lib/"*.dylib; do
    if [[ -e "${dylib}" ]]; then
      dylib_count=$((dylib_count + 1))
      check_macho "${dylib}" 1
    fi
  done
fi

printf 'PASS: %s and %d bundled dylib(s) have self-contained load commands\n' "${tmux_bin}" "${dylib_count}"
