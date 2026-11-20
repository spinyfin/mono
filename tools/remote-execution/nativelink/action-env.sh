#!/bin/bash
# Per-action wrapper for a NativeLink worker on a Mac. It is the worker's
# `entrypoint` in its NativeLink config, so every remote action runs as
# `action-env.sh <the action's command line>`.
#
# When Bazel runs an Apple action locally it fills in DEVELOPER_DIR and SDKROOT
# itself, from the Xcode and SDK the action asks for. With remote execution
# that is the worker's job. This script does it and then runs the action
# unchanged, so callers do not have to pass either variable.
#
# Inputs, set by Bazel on Apple actions:
#   XCODE_VERSION_OVERRIDE      for example 26.5.0.17F42
#   APPLE_SDK_PLATFORM          for example MacOSX, iPhoneOS, iPhoneSimulator
#   APPLE_SDK_VERSION_OVERRIDE  for example 26.5
#
# Behaviour:
#   - What the action asks for wins over a DEVELOPER_DIR or SDKROOT the caller
#     passed. A caller cannot know this worker's paths: the same path can be a
#     different Xcode here. A caller's value is kept only when the action does
#     not say which Xcode or SDK it wants.
#   - Actions that carry none of the inputs run untouched.
#   - The requested Xcode is used when it is installed in /Applications, and
#     the requested SDK when that Xcode has it.
#   - Otherwise the default Xcode (or that platform's default SDK) is used and
#     the mismatch is recorded in $LOG. Create the file $STRICT_MARKER to make
#     such actions fail with a clear error instead.
#   - Lookups are cached in $CACHE. An entry is recomputed once /Applications
#     changes (an Xcode added, removed or renamed). An Xcode updated in place
#     is not detected: delete $CACHE after one.
#
# The action environment is cleared by the worker, so nothing here can rely on
# HOME or PATH. The NL_XCODE_ENV_* overrides exist only for testing by hand.
set -u

# The worker invokes this script by absolute path, so its own directory is a
# stable place for per-host state. The same script is installed on every worker.
NL_DIR="${0%/*}"
CACHE="${NL_XCODE_ENV_CACHE:-$NL_DIR/xcode-env-cache}"
# The script is installed in ~/nativelink, so the account's log directory is
# one level up. HOME is not available to say so (see above).
LOG="${NL_XCODE_ENV_LOG:-${NL_DIR%/*}/Library/Logs/nativelink/xcode-env.log}"
STRICT_MARKER="${NL_XCODE_ENV_STRICT_MARKER:-$NL_DIR/xcode-strict}"
DEFAULT_DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer

KIND=""
VALUE=""

note() {
  printf '%s %s\n' "$(/bin/date '+%Y-%m-%d %H:%M:%S')" "$*" >> "$LOG" 2>/dev/null
}

die() {
  printf 'remote worker %s: %s\n' "$(/bin/hostname -s)" "$*" >&2
  exit 1
}

# Cache entries are one line: "<exact|fallback> <path>".
cache_get() { # $1 = key; sets KIND and VALUE
  local file="$CACHE/$1"
  [ -f "$file" ] && [ "$file" -nt /Applications ] || return 1
  read -r KIND VALUE < "$file" || return 1
  [ -n "$VALUE" ] && [ -e "$VALUE" ]
}

cache_put() { # $1 = key, $2 = kind, $3 = path
  /bin/mkdir -p "$CACHE" 2>/dev/null || return 0
  local tmp="$CACHE/.$1.$$"
  printf '%s %s\n' "$2" "$3" > "$tmp" 2>/dev/null && /bin/mv -f "$tmp" "$CACHE/$1" 2>/dev/null
  return 0
}

# Prints "<version> <build> <developer dir>" for each Xcode in /Applications.
installed_xcodes() {
  local app plist version build
  for app in /Applications/Xcode*.app; do
    [ -L "$app" ] && continue # a symlink to a bundle that is listed under its own name
    plist="$app/Contents/version.plist"
    [ -f "$plist" ] || continue
    version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$plist" 2>/dev/null) || continue
    build=$(/usr/libexec/PlistBuddy -c 'Print :ProductBuildVersion' "$plist" 2>/dev/null) || continue
    printf '%s %s %s\n' "$version" "$build" "$app/Contents/Developer"
  done
}

# 26.5.0 and 26.5 name the same release.
strip_trailing_zeros() {
  local v="$1"
  while [ "${v%.0}" != "$v" ]; do v="${v%.0}"; done
  printf '%s' "$v"
}

resolve_developer_dir() { # $1 = requested Xcode version; exports DEVELOPER_DIR
  local want="$1"
  local key="xcode-${want//[^A-Za-z0-9._-]/_}"
  if ! cache_get "$key"; then
    # A trailing component with a letter is a build id (17F42); match on it
    # exactly. Without one, match on the version number.
    local want_build="${want##*.}" want_version version build dir found=""
    case "$want_build" in
      *[A-Za-z]*) want_version=$(strip_trailing_zeros "${want%.*}") ;;
      *) want_build="" want_version=$(strip_trailing_zeros "$want") ;;
    esac
    while read -r version build dir; do
      if [ -n "$want_build" ]; then
        if [ "$build" = "$want_build" ]; then found="$dir"; break; fi
      elif [ "$(strip_trailing_zeros "$version")" = "$want_version" ]; then
        found="$dir"
        break
      fi
    done < <(installed_xcodes)
    if [ -n "$found" ]; then
      KIND=exact
      VALUE="$found"
    else
      KIND=fallback
      VALUE="$DEFAULT_DEVELOPER_DIR"
      [ -d "$VALUE" ] || VALUE=$(/usr/bin/xcode-select -p 2>/dev/null)
      [ -d "$VALUE" ] || die "no Xcode found on this worker"
      note "Xcode $want requested but not installed (installed: $(installed_xcodes | /usr/bin/awk '{printf "%s build %s; ", $1, $2}')). Using $VALUE."
    fi
    cache_put "$key" "$KIND" "$VALUE"
  fi
  if [ "$KIND" = fallback ] && [ -e "$STRICT_MARKER" ]; then
    die "this action asks for Xcode $want, which is not installed here"
  fi
  export DEVELOPER_DIR="$VALUE"
}

resolve_sdkroot() { # $1 = platform, $2 = SDK version; exports SDKROOT
  local platform="$1" version="$2" developer_dir="${DEVELOPER_DIR:-}"
  local key="sdk-${developer_dir//[^A-Za-z0-9._-]/_}-${platform//[^A-Za-z0-9._-]/_}-${version//[^A-Za-z0-9._-]/_}"
  if ! cache_get "$key"; then
    local sdk path
    sdk=$(printf '%s' "$platform" | /usr/bin/tr '[:upper:]' '[:lower:]')
    if path=$(/usr/bin/xcrun --sdk "${sdk}${version}" --show-sdk-path 2>/dev/null) && [ -d "$path" ]; then
      KIND=exact
      VALUE="$path"
    elif path=$(/usr/bin/xcrun --sdk "$sdk" --show-sdk-path 2>/dev/null) && [ -d "$path" ]; then
      KIND=fallback
      VALUE="$path"
      note "SDK ${sdk}${version} requested but not available in ${developer_dir:-the default Xcode}. Using $path."
    else
      die "no $platform SDK found in ${developer_dir:-the default Xcode}"
    fi
    cache_put "$key" "$KIND" "$VALUE"
  fi
  if [ "$KIND" = fallback ] && [ -e "$STRICT_MARKER" ]; then
    die "this action asks for the $platform $version SDK, which is not installed here"
  fi
  export SDKROOT="$VALUE"
}

if [ -n "${XCODE_VERSION_OVERRIDE:-}" ]; then
  resolve_developer_dir "$XCODE_VERSION_OVERRIDE"
fi

if [ -n "${APPLE_SDK_PLATFORM:-}" ] && [ -n "${APPLE_SDK_VERSION_OVERRIDE:-}" ]; then
  resolve_sdkroot "$APPLE_SDK_PLATFORM" "$APPLE_SDK_VERSION_OVERRIDE"
fi

exec "$@"
