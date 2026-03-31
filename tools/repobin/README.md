# repobin

Status: experimental / under active development. The CLI behavior and config
format may change without notice.

`repobin` installs lightweight commands onto your `PATH` that dispatch to
repo-defined Bazel binaries in the current working directory.

## Install

```bash
cargo install repobin
```

## Usage

Check in a repo-root `REPOBIN.toml`:

```toml
version = 1

[tools.boss]
target = "//tools/boss/cli:boss"

[tools.cube]
target = "//tools/cube:cube"
```

Then install the `repobin` binary plus tool symlinks:

```bash
bazel run //tools/repobin:repobin -- install
repobin install
repobin install --bin-dir ~/.local/bin
```

Once installed, invoking a configured tool from inside that repo will:

1. find the nearest `REPOBIN.toml`,
2. build the configured Bazel target,
3. resolve the runnable executable from Bazel metadata,
4. replace the current process with the built binary.

Examples:

```bash
boss task list
cube workspace lease mono --task "prepare repobin publish"
repobin doctor
repobin list
repobin exec boss -- task list
```

## Notes

- `repobin` currently supports Bazel-backed tools only.
- It expects a working `bazel` entry point on `PATH`.
- `repobin install` defaults to `~/bin` and warns if the chosen directory is
  not on `PATH`.
