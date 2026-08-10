## Summary

Grok worker preflight now coordinates with Grok's observed OAuth refresh pidfile protocol and hardens host Claude-settings detection.

### OAuth refresh coordination

Grok's `auth.json.lock` is a foreign `pid:timestamp` pidfile. Before `grok models`, Boss reads it without creating, changing, or locking it; a live recorded process is allowed up to 30 seconds to clear the file, while a dead pid is treated as stale. Boss then validates that `auth.json` is a non-empty JSON object. Malformed lock data and unavailable credentials fail closed.

The `grok models` process is separately bounded to 30 seconds. If a refresh starts after the pidfile check and causes the probe to wait, Boss kills the probe and reports the timeout instead of hanging a worker slot. This is coordination with Grok's pidfile protocol, not a retry-until-pass loop and not an assumption that Grok uses flock.

### HOME scoping

Permission-source checks strip the ` (settings)` display suffix and compare canonical paths. Host `~/.claude/settings*` continues to fail closed, including equivalent path spellings, while workspace settings beneath the operator home and settings beneath the scoped process home remain allowed.

## Validation

- `bazel build //tools/boss/engine/driver:driver`
- `bazel test //tools/boss/engine/driver:driver_test`
- `checkleft run`
- Unit coverage for absent, stale, and malformed Grok OAuth pidfiles and host-home versus workspace settings classification.
