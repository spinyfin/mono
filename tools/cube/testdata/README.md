# Cube GC store

`gc_store.json` pins one platform-independent jj store for the GC concurrency
test. The store retains all eight writers and 16,800 bookmark operations from
`gc_store.sh`. The test installs a private copy and repairs its workspace and
remote paths before running real collection and concurrent leases.
The repository rule extracts the archive once per workspace. Tests receive the
tree as runfiles and reuse cube's parallel fixture-copy helper.

## Regenerate

Use a new versioned release for every regeneration; do not replace a published
asset. From the checkout, run:

```sh
python3 tools/cube/testdata/gc_store_archive.py \
  --output-dir /tmp/cube-gc-v2 --release-tag cube-test-data-gc-store-v2
```

The script resolves the pinned jj through Bazel, runs the original generator,
and writes `gc-store.tar.gz` and `gc_store.json` beside the retained store. It
normalizes archive order, timestamps, ownership, permissions, and gzip headers.
PAX supports jj's 128-character object names. Store contents retain real jj
timestamps and concurrent history; regeneration is not byte-identical, but
packing a given store is.

Publish both files on `spinyfin/mono` with a dedicated `cube-test-data-*` tag and
a title beginning `Cube test data`. Set `gh release create --latest=false` so
this data does not replace the latest Boss release. Copy the generated manifest
to this directory and run:

```sh
bazel build //tools/cube:cube_lib_test //tools/cube:gc_store_archive_test
bazel test //tools/cube:cube_lib_test //tools/cube:gc_store_archive_test
```

The fast archive test checks the generator checksum, jj version, and all jj
platform archive checksums. Drift fails with regeneration instructions. It also
checks metadata normalization, including long object names. Commit the manifest
and source changes, never the store or tarball.

## Download cache

Bazel's SHA-256 repository cache is separate from its build action disk cache.
`bazel info repository_cache` shows the resolved location. On macOS the default
is `~/Library/Caches/bazel/_bazel_<user>/cache/repos/v1`, shared across workspaces.
Linux CI explicitly uses `/mnt/ssd/bazel/repo_cache` in `.ci.bazelrc`, separate
from `/mnt/ssd/bazel/disk_cache`; macOS CI uses `/Volumes/ssd/bazel/repo_cache`.
The default developer action cache is `~/.cache/bazelcache`. Evicting action
cache entries does not evict the downloaded archive.

To verify reuse without deleting caches, declare the same URL and SHA-256 with
`http_file` in a fresh minimal Bazel workspace. Build it with
`--repository_disable_download --experimental_workspace_rules_log_file=<path>`.
Use the original workspace's repository-cache path, passing
`--repository_cache=<cache>` if the fresh workspace's default differs.
A successful first fetch with downloads forbidden proves a repository-cache
hit; the rule log records the URL and digest. This also works when that fresh
workspace has no disk-cache configuration or prior build outputs.
