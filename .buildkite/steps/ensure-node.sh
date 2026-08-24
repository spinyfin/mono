#!/usr/bin/env bash
# ensure-node.sh — source from checkleft CI steps so Node >= 22 is on PATH.
#
# checkleft's format/oxc and lint/oxc provision oxfmt/oxlint via
# `npx --yes <package>@<version>`. That needs a Node runtime on PATH; npx is
# not a Bazel-provided toolchain. Linux bazel-any agents (including GCE) do
# not install Node as a host package, so a checks job that touches files
# format/oxc applies to otherwise dies with:
#   failed to spawn subprocess ... binary `oxfmt`: No such file or directory
#
# When `npx` is missing or Node is older than 22, this downloads the official
# Node tarball for the host OS/arch, verifies its sha256, and prepends its
# bin/ to PATH. Cached under $HOME/.cache/mono-ci-node so later jobs on the
# same agent reuse it.
#
# Must be sourced (not executed) so the PATH export reaches the caller.

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
  echo "ensure-node.sh must be sourced, not executed" >&2
  exit 1
fi

# Pin matches the runtime empirically verified against oxfmt's ESM bin
# (see MIN_NODE_MAJOR_VERSION in tools/checkleft/src/external/declarative/resolve.rs).
_MONO_NODE_VERSION="24.8.0"
_MONO_NODE_MIN_MAJOR=22

_mono_node_major() {
  if ! command -v node >/dev/null 2>&1; then
    echo 0
    return
  fi
  node -p "parseInt(process.versions.node, 10)" 2>/dev/null || echo 0
}

if command -v npx >/dev/null 2>&1 && [[ "$(_mono_node_major)" -ge "${_MONO_NODE_MIN_MAJOR}" ]]; then
  echo "--- [ensure-node] using host Node $(node --version) ($(command -v npx))"
  unset -f _mono_node_major
  unset _MONO_NODE_VERSION _MONO_NODE_MIN_MAJOR
  return 0
fi

_mono_os="$(uname -s)"
_mono_arch="$(uname -m)"
case "${_mono_os}-${_mono_arch}" in
  Linux-x86_64) _mono_platform="linux-x64" ;;
  Linux-aarch64) _mono_platform="linux-arm64" ;;
  Darwin-arm64) _mono_platform="darwin-arm64" ;;
  Darwin-x86_64) _mono_platform="darwin-x64" ;;
  *)
    echo "ensure-node: unsupported platform ${_mono_os}-${_mono_arch}; install Node >= ${_MONO_NODE_MIN_MAJOR}" >&2
    unset -f _mono_node_major
    unset _MONO_NODE_VERSION _MONO_NODE_MIN_MAJOR _mono_os _mono_arch
    return 1
    ;;
esac

# Official sha256 of node-v${_MONO_NODE_VERSION}-${platform}.tar.gz from
# https://nodejs.org/dist/v${_MONO_NODE_VERSION}/SHASUMS256.txt
case "${_mono_platform}" in
  linux-x64) _mono_sha256="daf68404b478b4c3616666580d02500a24148c0f439e4d0134d65ce70e90e655" ;;
  linux-arm64) _mono_sha256="5eb16b14af5a5f494ed54770822144e847c744fe590f8df093ad4927cf3dd7fd" ;;
  darwin-arm64) _mono_sha256="d81191a1866760eb918caa976c023036bc1fc7405ea31b148905211522045767" ;;
  darwin-x64) _mono_sha256="6fd8496b59baa8f86a24e3eb03308b763091716ffc6b6e1094d1a5e5696dd6dd" ;;
esac

_mono_tarball="node-v${_MONO_NODE_VERSION}-${_mono_platform}.tar.gz"
_mono_cache_root="${XDG_CACHE_HOME:-${HOME}/.cache}/mono-ci-node/v${_MONO_NODE_VERSION}"
_mono_prefix="${_mono_cache_root}/node-v${_MONO_NODE_VERSION}-${_mono_platform}"

if [[ ! -x "${_mono_prefix}/bin/npx" ]]; then
  echo "--- [ensure-node] installing Node v${_MONO_NODE_VERSION} (${_mono_platform})"
  mkdir -p "${_mono_cache_root}"
  _mono_tmp="$(mktemp -d "${_mono_cache_root}/download.XXXXXX")"
  if ! curl -fsSL --retry 3 --retry-delay 2 \
    "https://nodejs.org/dist/v${_MONO_NODE_VERSION}/${_mono_tarball}" \
    -o "${_mono_tmp}/${_mono_tarball}"; then
    echo "ensure-node: failed to download ${_mono_tarball}" >&2
    rm -rf "${_mono_tmp}"
    unset -f _mono_node_major
    unset _MONO_NODE_VERSION _MONO_NODE_MIN_MAJOR _mono_os _mono_arch _mono_platform _mono_sha256 _mono_tarball _mono_cache_root _mono_prefix _mono_tmp
    return 1
  fi
  _mono_actual=""
  if command -v sha256sum >/dev/null 2>&1; then
    _mono_actual="$(sha256sum "${_mono_tmp}/${_mono_tarball}" | awk '{print $1}')"
  else
    _mono_actual="$(shasum -a 256 "${_mono_tmp}/${_mono_tarball}" | awk '{print $1}')"
  fi
  if [[ "${_mono_actual}" != "${_mono_sha256}" ]]; then
    echo "ensure-node: sha256 mismatch for ${_mono_tarball}" >&2
    echo "  expected ${_mono_sha256}" >&2
    echo "  actual   ${_mono_actual}" >&2
    rm -rf "${_mono_tmp}"
    unset -f _mono_node_major
    unset _MONO_NODE_VERSION _MONO_NODE_MIN_MAJOR _mono_os _mono_arch _mono_platform _mono_sha256 _mono_tarball _mono_cache_root _mono_prefix _mono_tmp _mono_actual
    return 1
  fi
  tar -xzf "${_mono_tmp}/${_mono_tarball}" -C "${_mono_cache_root}"
  rm -rf "${_mono_tmp}"
fi

export PATH="${_mono_prefix}/bin:${PATH}"
echo "--- [ensure-node] Node $(node --version) via ${_mono_prefix}/bin"

unset -f _mono_node_major
unset _MONO_NODE_VERSION _MONO_NODE_MIN_MAJOR _mono_os _mono_arch _mono_platform _mono_sha256 _mono_tarball _mono_cache_root _mono_prefix _mono_tmp _mono_actual
