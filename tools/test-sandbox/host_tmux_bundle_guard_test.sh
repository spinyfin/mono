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

# In the runfiles tree, tmux, tmux.bin, and lib/*.dylib are siblings.
bundle_dir="$(cd "$(dirname "${tmux_entry}")" && pwd)"
tmux_bin="${bundle_dir}/tmux.bin"
if [[ ! -e "${tmux_bin}" ]]; then
  printf 'FAIL: bundled binary %s does not exist beside %s\n' "${tmux_bin}" "${tmux_entry}" >&2
  exit 1
fi

otool_cmd=()
if [[ -n "${DEVELOPER_DIR:-}" && -x "${DEVELOPER_DIR}/Toolchains/XcodeDefault.xctoolchain/usr/bin/otool" ]]; then
  otool_cmd=("${DEVELOPER_DIR}/Toolchains/XcodeDefault.xctoolchain/usr/bin/otool")
elif [[ -n "${DEVELOPER_DIR:-}" && -x "${DEVELOPER_DIR}/usr/bin/otool" ]]; then
  otool_cmd=("${DEVELOPER_DIR}/usr/bin/otool")
else
  otool_cmd=("/usr/bin/xcrun" "--no-cache" "otool")
fi

check_macho() {
  local macho="$1"
  local is_dylib="$2"
  local macho_dir
  macho_dir="$(dirname "${macho}")"

  local otool_file="${TEST_TMPDIR:?}/otool.out"
  if ! "${otool_cmd[@]}" -L "${macho}" > "${otool_file}"; then
    printf 'FAIL: %s -L failed for %s\n' "${otool_cmd[*]}" "${macho}" >&2
    return 1
  fi

  local line_num=0
  local dep_count=0
  local saw_libsystem=0
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
    dep_count=$((dep_count + 1))
    if [[ "${dep}" == "/usr/lib/libSystem.B.dylib" ]]; then
      saw_libsystem=1
    fi
    case "${dep}" in
      /usr/lib/*|/System/*)
        ;;
      @loader_path/*)
        local rel="${dep#@loader_path/}"
        if [[ ! -e "${macho_dir}/${rel}" ]]; then
          printf 'FAIL: %s references %s, but %s does not exist in runfiles\n' "${macho}" "${dep}" "${macho_dir}/${rel}" >&2
          return 1
        fi
        ;;
      *)
        printf 'FAIL: %s has non-self-contained dependency %s\n' "${macho}" "${dep}" >&2
        return 1
        ;;
    esac
  done < "${otool_file}"

  if [[ "${dep_count}" -eq 0 ]]; then
    printf 'FAIL: no Mach-O load commands parsed for %s\n' "${macho}" >&2
    return 1
  fi
  if [[ "${is_dylib}" -eq 0 && "${saw_libsystem}" -ne 1 ]]; then
    printf 'FAIL: %s did not report /usr/lib/libSystem.B.dylib in otool -L output\n' "${macho}" >&2
    return 1
  fi
}

# Negative control: non-Mach-O input must fail inspection rather than passing
# vacuously.
non_macho="${TEST_TMPDIR:?}/not-a-macho"
printf 'not a mach-o binary\n' > "${non_macho}"
if check_macho "${non_macho}" 0 >/dev/null 2>&1; then
  printf 'FAIL: check_macho unexpectedly passed on non-Mach-O input %s\n' "${non_macho}" >&2
  exit 1
fi

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
