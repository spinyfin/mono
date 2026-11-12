# Keep-both conflict resolution and per-path conflict rules

When a PR in review conflicts with its base branch, the engine first rebases the branch itself and then hands every file the rebase left conflicted to a set of deterministic resolvers. If every file resolves, the result is pushed and no agent is started. This page covers two of those resolvers: the built-in keep-both resolver, and the per-path rules an operator can add. Lockfile regeneration is described in the [conflict reduction design](designs/merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md).

## What keep-both resolves

The most common mechanical conflict is two branches each adding something new at the same place: two tests at the end of one module, two `mod` lines, two names in one wrapped `use` list. Neither side touched what the other wrote. The keep-both resolver writes both insertions, the base branch's first, runs `rustfmt` on the file and leaves the verdict to the checks every pushed resolution already passes.

It applies to every Rust source file, and resolves a file only when **every** conflict in it is one of:

| Conflict                                                                                                                                             | Result                                                         |
| ---------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------- |
| Both sides inserted whole items at the same spot (functions, tests, types, impls, constants, inline modules), with their doc comments and attributes | Both, separated the way the sides separate their own items     |
| Both sides inserted `use`, `mod name;` or `extern crate` declarations at the same spot                                                               | Both; `rustfmt` sorts them                                     |
| Both sides added names to the same brace-wrapped `use` list, including when `rustfmt` repacked the lines so the text of existing lines changed       | The base's names plus both sides' additions; `rustfmt` rewraps |

It declines, leaving the file untouched for an agent, when:

- a conflict edits lines that were already there, or one side removed or renamed something;
- an insertion is not self-contained: unbalanced brackets, or it ends inside a string or comment;
- an insertion contains anything that is not a whole item: a statement, a match arm, a struct field, an enum variant, a list entry, a comment that belongs to no item;
- **both sides added the same thing** anywhere in the file: an item with the same name, the same import (whether each side wrote it as a declaration of its own, inside a group, or as a name added to an existing list), or one definition under two different names. Keeping both would at best fail to compile and at worst duplicate behaviour silently;
- the conflict has more than two sides;
- `rustfmt` rejects the combined file, which means the two insertions do not form valid Rust together.

The result is never one side's version of the file. There is no fallback to "ours" or "theirs".

`rustfmt` is the one on the engine's `PATH`, run at the workspace root on the resolved file only. It is pointed at the nearest `rustfmt.toml` or `.rustfmt.toml` between the file and the repository root, so a user-level rustfmt configuration on the host cannot change the result. A repository with no rustfmt configuration is formatted with the owning crate's edition from its `Cargo.toml`, as `cargo fmt` would. If `rustfmt` cannot be started at all, the resolver reports an operational failure rather than a decline, and the trace says so.

Statements and list entries are deliberately excluded from the built-in. Their order can matter (two schema steps, two entries in an ordered list) and two unrelated branches can add the same call without either being wrong. A [per-path rule](#per-path-rules) is the way to allow them for a file known to be an append-only list.

## What still checks the result

Keeping both does not prove the merged code is right. A new test may call a function whose signature the other side changed. The decision is left to the gates that apply to every mechanical resolution:

1. **The push gate.** The resolution is pushed with `cube workspace push`, which runs `checkleft run` first. For Rust that includes the format check and clippy on the crates that own the changed files, so a combination that does not compile is refused before anything reaches GitHub. A refused push falls through to an agent, exactly as before.
2. **The deletion tripwire.** Every pushed mechanical resolution is compared with both merge parents. A resolution that drops something a parent added halts the attempt for human sign-off. Keep-both resolutions go through it like any other.
3. **CI on the PR.** A pushed resolution that fails its tests is handled by the ordinary CI-fix lifecycle.

## Per-path rules

Rules live in `conflict-recipes.toml` in the Boss state root, next to the work database. The file is Boss-side configuration. It is never read from a product repository, so a PR cannot change what the engine runs on its own conflict. The file is read at the start of every conflict attempt; edits need no restart. Without the file, no rule is active.

Two kinds of rule are supported.

```toml
# Keep both sides of same-spot insertions for these paths, including
# statements and list entries.
[[keep_both]]
name = "migration_lists"
glob = "**/schema_migration_registry.rs"

[[keep_both]]
name = "build_deps"
glob = "**/BUILD.bazel"
# Optional. Run at the workspace root after both insertions are written.
# {file} is the path of the resolved file. A non-zero exit declines.
format_command = ["buildifier", "{file}"]

# Discard the conflicted file and regenerate it.
[[recipe]]
name = "generated_schema"
glob = "**/schema.generated.json"
resolve_command = ["make", "regen-schema"]
verify_command = ["make", "validate-schema"] # optional
workdir = "." # optional; default is the file's directory
```

A `keep_both` rule accepts any self-contained run of whole lines. It still declines when a conflict edits existing lines, when an insertion's brackets do not balance, and when both sides added the same line. Without a `format_command`, Rust files are formatted with `rustfmt` and other files are left as written. Use a rule only where two additions are independent of each other and of their order.

**Order of precedence.** The built-in lockfile resolvers are asked first, then every `[[recipe]]` in file order, then every `[[keep_both]]` rule in file order, then the built-in keep-both resolver. The first resolver whose pattern matches a path handles it, and a decline does not pass the file to the next one. So a rule for a Rust path replaces the built-in keep-both resolver for that path, and no rule can displace a lockfile resolver.

**A broken file applies nothing.** If the file does not parse, or any rule has an empty name, an invalid glob or an empty command, none of its rules are used and the engine logs `conflict rules file is invalid`. The built-in resolvers keep working.

## Reading the trace

Each resolver run logs one line with `conflict_class` (`keep_both_union` for the built-in, `recipe` for a per-path rule), the outcome and, for a decline, the reason:

```text
deterministic_resolvers: resolver resolved conflicted file path=… conflict_class="keep_both_union" summary="kept both sides' insertions in … (2 conflict(s): imports, items)"
deterministic_resolvers: resolver declined conflicted file path=… conflict_class="keep_both_union" reason="conflict at line 412: side #2 adds something that is not a whole item (`registry.register(…);`)"
```

`conflict_ladder: loaded per-path conflict rules for rung 0` confirms the rules file was read and how many rules it holds.

## Measuring a change to the rules

`tools/boss/scripts/conflict-replay` rebuilds the corpus of real conflicts from GitHub force-push history and runs the shipped resolver over it. See its [README](../scripts/conflict-replay/README.md) for the method and the results recorded when keep-both was extended beyond `mod.rs` and `lib.rs`.
