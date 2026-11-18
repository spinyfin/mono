#!/usr/bin/env bash
set -euo pipefail

# Usage: gc_store.sh <jj-binary> <output-dir>. Relative paths resolve against the current directory.
case "$1" in /*) jj_binary="$1" ;; *) jj_binary="$PWD/$1" ;; esac
case "$2" in /*) fixture_dir="$2" ;; *) fixture_dir="$PWD/$2" ;; esac
root="$fixture_dir/root"
workspace="$fixture_dir/pool/mono-agent-001"
upstream="$fixture_dir/upstream"
mkdir -p "$fixture_dir/pool"
export JJ_CONFIG="$fixture_dir/jj-config.toml"
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
printf '[user]\nname = "Cube Test"\nemail = "cube@example.invalid"\n' > "$JJ_CONFIG"

jj_at() {
    local repo="$1"
    shift
    "$jj_binary" --quiet --no-pager -R "$repo" "$@"
}

"$jj_binary" --quiet git init --colocate "$root"
printf 'a\n' > "$root/a.txt"
jj_at "$root" describe -m base
jj_at "$root" bookmark create main -r @
jj_at "$root" new -m 'feature work'
printf 'b\n' > "$root/b.txt"
jj_at "$root" bookmark create feat -r @
jj_at "$root" new main
jj_at "$root" workspace add "$workspace" --name mono-agent-001 -r main
"$jj_binary" --quiet git clone --colocate "$root" "$upstream"
jj_at "$root" git remote add origin "$upstream"
jj_at "$workspace" new main
jj_at "$root" op log --no-graph --limit 1 -T id > "$fixture_dir/cut"

# Keep the real concurrent writers and full history; the published archive stores the result so tests never rebuild it.
pids=()
for worker in 0 1 2 3 4 5 6 7; do
    (
        for ((i = 0; i < 2100; i++)); do
            if ((i % 2 == 0)); then rev=main; else rev=feat; fi
            jj_at "$workspace" --ignore-working-copy bookmark set "gc-scale-$worker" --allow-backwards -r "$rev"
        done
    ) &
    pids+=("$!")
done
status=0
for pid in "${pids[@]}"; do wait "$pid" || status=1; done
if ((status != 0)); then exit "$status"; fi
jj_at "$root" --ignore-working-copy log -r main -T commit_id > /dev/null
