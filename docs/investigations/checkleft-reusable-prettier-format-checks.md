# Reusable per-language prettier `format/*` checks in checkleft

Status: investigation (writeup only — no code/config changes in this PR).
Date: 2026-06-20.

## Summary

**Goal.** Define ONE checkleft check that runs `npx --yes prettier --check <files>` and reuse it across `format/ts`, `format/js`, `format/md`, `format/html`, where each id differs only in the file glob it targets — with the command and most config defined in one place.

**Finding.** This four-ids-share-one-command pattern is **not expressible today** without either (a) duplicating the command across four definition files, or (b) collapsing to a single id with a combined glob. The reason is concrete and grounded in the source: for a `declarative` check the file glob (`applies_to`) is part of the *check definition*, and a per-instance `CHECKS.yaml` entry **cannot override it**. The only things a `CHECKS.yaml` entry can vary on a declarative check are the binary binding (`needs.<name>.path|bazel`) and `policy.severity` — not the glob, not the args.

**Recommendation.**

- If you need the four separate ids **today** with no checkleft change: write four tiny declarative definition files (one per language) under the repo's on-disk `check_definitions.exec_paths` and reference them from `CHECKS.yaml`. The command line is duplicated four times; there is no anchor/include mechanism to DRY it across files. Because mono already runs definitions from disk (`exec_paths` + `allow_override_bundled: true`), this needs **no checkleft release**. Exact config in §4.
- If you don't actually need four ids: one definition `format/prettier` with a combined glob is fully expressible today, zero duplication — but you lose per-language enable/severity/bypass granularity.
- The clean "define-once, four ids, per-id glob" outcome requires a **small checkleft change**: add an optional per-instance `applies_to` override to the `CHECKS.yaml` `checks` entry (§3, Option 1). This is additive and the smallest delta, but it changes the binary and therefore needs a checkleft release + a version bump in downstream consumers (e.g. `checkleft-sandbox`) before they can use it. It also carries a silent-downgrade hazard on older binaries (§3).

Every claim below cites the checkleft source in this repo (`tools/checkleft/...`).

---

## 1. checkleft's declarative-check model

### 1.1 Two layers: the `CHECKS.yaml` *instance* vs the check *definition*

checkleft separates **which checks run** (the consuming repo's `CHECKS.yaml`) from **what a check is** (a check *definition*). These are different files with different schemas.

**Layer A — `CHECKS.yaml` entry (the instance).** Parsed by `ParsedCheckConfig` in `tools/checkleft/src/config.rs:421`:

```rust
#[derive(Debug, Clone, Deserialize)]
struct ParsedCheckConfig {
    id: String,
    #[serde(default)]
    check: Option<String>,
    /// Explicit implementation reference (`generated:<id>`, `bundled:<name>`,
    /// or a repo-relative manifest path). When absent, the check name is resolved
    /// automatically against the bundled set and configured exec_paths.
    #[serde(default)]
    implementation: Option<String>,
    #[serde(default = "enabled_default")]
    enabled: bool,
    #[serde(default)]
    policy: ParsedCheckPolicyConfig,
    #[serde(default = "empty_toml_table")]
    config: toml::Value,
}
```

So a `CHECKS.yaml` entry carries only: `id`, `check` (the *definition name*, defaulting to `id` — see `config.rs:339`), an optional explicit `implementation`, `enabled`, `policy` (severity/bypass), and a free-form `config` table. It does **not** carry the command, the globs, or the args — those live in the definition. (Note: `ParsedCheckConfig` is **not** `#[serde(deny_unknown_fields)]`, which matters for the downgrade hazard in §3.)

**Layer B — the check definition (declarative manifest).** This is where the command, globs, and transforms live. The validated model is `ExternalCheckDeclarativePackage` in `tools/checkleft/src/external/declarative/mod.rs:55`:

```rust
pub struct ExternalCheckDeclarativePackage {
    /// Declared binary requirements ("named holes"), keyed by name.
    pub needs: BTreeMap<String, BinaryRequirement>,
    /// File globs the check applies to. The framework selects matching changed
    /// files before running any invocation.
    pub applies_to: Vec<String>,
    /// Ordered, self-contained invocation specs.
    pub invocations: Vec<Invocation>,
}
```

The on-disk YAML manifest is parsed by `parse_declarative_check_manifest` (`mod.rs:216`, via `serde_yaml::from_str`) into a single `#[serde(deny_unknown_fields)]` struct whose top-level fields are (`tools/checkleft/src/external/mod.rs:223-233`): `id`, `runtime`, `api_version`, `mode`, `applies_to`, `needs`, `invocations`. The validator enforces (`mod.rs:242-249`):

- `mode` must be `declarative`;
- `runtime` must equal `EXTERNAL_CHECK_DECLARATIVE_RUNTIME_V1` = `"declarative-v1"` (`mod.rs:15`);
- `api_version` must equal `EXTERNAL_CHECK_API_V1` = `"v1"`.

A canonical example is the bundled rust formatter, `tools/checkleft/checks/format/rust.yaml`:

```yaml
id: format/rust
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to:
  - "**/*.rs"
needs:
  rustfmt:
    default:
      bazel: "@rules_rust//tools/upstream_wrapper:rustfmt"
    fallback:
      path: "rustfmt"
invocations:
  - id: format
    run: rustfmt
    mode: per_file
    args:
      - "--check"
      - "-l"
      - "--config-path={{repo_root}}"
      - "{{file}}"
    exit:
      "0": ok
      "1": findings
      default: error
    transform:
      kind: linelist
      message: "file needs rustfmt formatting"
      remediations:
        - "Run `cargo fmt` to reformat, or apply the expected diff shown by `rustfmt --check {{input.file}}`."
```

Key sub-fields, all grounded in `mod.rs`:

- **`needs`** (`BinaryRequirement`, `mod.rs:70`): named binary "holes" each with a `default` binding and optional `fallback`. A `BinaryBinding` is exactly one of `bazel: <label>` or `path: <path-or-PATH-name>` (`mod.rs:80`, `parse_binding` at `mod.rs:339`). A `fallback` is only allowed when `default` is a `bazel` binding (`validate_requirement`, `mod.rs:330`).
- **`invocations`** (`Invocation`, `mod.rs:91`): ordered list. Each has `id`, a `kind` (`tool` (default) or `bazel_aspect`), `exit` semantics, and a `transform`. A `tool` invocation (`ToolInvocation`, `mod.rs:117`) has `run` (a key into `needs`), `mode` (`batch` | `per_file`), and `args`.
- **`exit`** (`ExitSemantics`, `mod.rs:161`): an explicit map from exit code (and `default`) to one of `ok` / `findings` / `error`. A `default` outcome is **required** (`validate_exit`, `mod.rs:493`) so that a crashing tool surfaces as a check error rather than silently "clean".
- **`transform`** (`tools/checkleft/src/external/declarative/transform.rs`): how stdout becomes findings. Implemented kinds are `json` (jq-like `select` + a `finding` template map), `passthrough` (the binary already emits checkleft findings JSON), and `linelist` (one offending file path per stdout line → one file-level finding). `regex` and `sarif` are **reserved/unimplemented** and hard-error (`mod.rs:573`).

### 1.2 Where definitions live, and how an `id` maps to a command + file set

Resolution is in `resolve_check_implementation` (`config.rs:575`). For a `CHECKS.yaml` entry the *definition name* is `check` (falling back to `id`), and resolution proceeds:

1. If `implementation:` is set explicitly (`generated:`, `bundled:`, or a manifest path), use it verbatim.
2. Otherwise resolve the name against, in priority order: `exec_paths` (if `allow_override_bundled: true`) → bundled defs embedded in the binary → `exec_paths` (if `allow_override_bundled: false`) → `None` (falls through to the Rust built-in registry).

**Bundled definitions** are baked into the binary. They are a hardcoded array `BUNDLED_CHECK_DEFS` in `tools/checkleft/src/external/bundled.rs:88`, each row `include_str!`-embedding a manifest at compile time:

```rust
BundledCheckDef {
    check_names: &["format/rust"],
    kind: BundledCheckDefKind::Declarative {
        extension: "yaml",
        contents: include_str!("../../checks/format/rust.yaml"),
    },
    limits: None,
},
```

The currently bundled declarative checks are `format/bazel`, `format/rust`, `lint/rust`, `lint/bazel` (`bundled.rs:88-120`). Adding a new bundled name therefore means editing this array **and recompiling/releasing** the binary. There is no `prettier` (or `format/ts` etc.) bundled name today.

**Exec-path (on-disk) definitions** are resolved from directories named in `check_definitions.exec_paths` (`config.rs:412`, `find_in_exec_paths` at `config.rs:628`). Two layouts are tried per exec_path, **flat first**:

- Flat: `<exec_path>/<name>.yaml` / `.yml` / `.toml`
- Nested (legacy): `<exec_path>/<name>/check.yaml` / `check.toml`

Because the name can contain a slash, `check: format/rust` with `exec_paths: [tools/checkleft/checks]` resolves to `tools/checkleft/checks/format/rust.yaml` — exactly where the file lives. This is precisely how mono is wired today (`/CHECKS.yaml`):

```yaml
check_definitions:
  exec_paths:
    - tools/checkleft/checks
  allow_override_bundled: true
```

With `allow_override_bundled: true`, the on-disk copy wins over the bundled snapshot, so mono always runs its checked-in (head) definitions.

**Runtime mapping.** At run time `run_declarative_check` (`tools/checkleft/src/external/declarative/executor.rs:34`) ties it together:

```rust
let files = select_files(changeset, &package.applies_to)?;        // glob from the DEFINITION
...
resolve::resolve_all(repo_root, &package.needs, config)?          // config only overrides binaries
...
for invocation in &package.invocations { run_invocation(...) }    // args from the DEFINITION
```

`select_files` (`executor.rs:127`) builds its globset purely from `package.applies_to` — the definition's globs. The per-instance `config` is passed only to `resolve_all` (binary resolution) and `effective_severity`.

---

## 2. Reuse / parameterization: is "one command, four globbed ids" expressible today?

**No — not as a single shared definition.** Three independent mechanisms could in principle enable it; none does:

### 2.1 `check:` aliasing reuses the *implementation*, but not with a per-id glob

The documented "multiple instances of one implementation" pattern (`tools/checkleft/userdoc/docs/checks-config.md:132`) lets you point several ids at one definition via `check:`, e.g. `file/size` and `forbidden-paths` are instantiated multiple times. **But this only varies behavior for checks that read their `config` table.** For a *declarative* check, the glob is `applies_to` in the definition, and aliasing does not let an instance change it. Four ids aliased onto one prettier definition would all select the **same** files (the definition's `applies_to`), defeating the purpose.

### 2.2 Per-instance `config` can override only the binary binding, not the glob/args

For declarative checks the instance `config` table is consumed in exactly one place — `override_binding` (`tools/checkleft/src/external/declarative/resolve.rs:133`):

```rust
/// Read an optional binding override from `config` at `needs.<name>.{path|bazel}`.
fn override_binding(name: &str, config: &toml::Value) -> Option<Result<BinaryBinding>> {
    let entry = config.get("needs")?.get(name)?;
    let bazel = entry.get("bazel").and_then(toml::Value::as_str);
    let path = entry.get("path").and_then(toml::Value::as_str);
    ...
}
```

It reads `config.needs.<name>.{path|bazel}` and nothing else. There is no code path by which `config` influences `applies_to` (file selection) or `args`. So you cannot pass a per-id glob through `config`.

### 2.3 Templating cannot inject a per-id glob either

The arg templater recognizes a closed allowlist (`validate_arg_template_refs`, `mod.rs:454`):

```rust
match inner {
    "files" | "file" | "repo_root" => {}
    other => bail!(
        "invocation `{id}` arg contains unknown template ref `{{{{{other}}}}}` \
         (recognized in args: `{{{{files}}}}`, `{{{{file}}}}`, `{{{{repo_root}}}}`)"
    ),
}
```

Only `{{files}}`, `{{file}}`, `{{repo_root}}` are allowed in args; any other `{{...}}` is a hard error. The finding-template language (`template.rs`) similarly recognizes only `item.*`, `input.file`, `exit_code`. There is **no** `{{config.*}}` / `{{param.*}}` mechanism, and `applies_to` is not templated at all (it is consumed directly by `select_files`).

### 2.4 No YAML anchors/includes across definition files

Each definition file is parsed as a **single** package (`parse_declarative_check_manifest` → one `#[serde(deny_unknown_fields)]` struct; one bundled row per file). YAML anchors/aliases (`&a`/`*a`/`<<`) are resolved by the parser but only **within one file**, so they cannot share an invocation block across four separate definition files. There is no `include`/`extends` directive in the manifest schema.

### 2.5 What therefore IS expressible today

- **(A) One id, combined glob — zero duplication, no per-language granularity.** A single definition `format/prettier` whose `applies_to` lists every extension, referenced by one `CHECKS.yaml` entry. Command and config defined once. You get a single check id (`format/prettier`); you cannot enable/disable/severity/bypass per language, and per-directory child-`CHECKS.yaml` overrides act on the whole bundle, not per language.
- **(B) Four ids, four near-duplicate definition files — full granularity, duplicated command.** Four definitions `format/ts.yaml` … `format/html.yaml`, each with its own glob, each repeating the identical `needs`/`invocations` block, referenced by four `CHECKS.yaml` entries. This gives the four ids the task wants, at the cost of the duplication the task wants to avoid (and, per §2.4, there is no way to DRY it).

The specific target — **four ids, command defined once, each overriding only the glob** — is **not** achievable today.

---

## 3. Smallest checkleft change to enable define-once / reuse-many

Three options, smallest first; recommendation follows.

### Option 1 (recommended): per-instance `applies_to` override in `CHECKS.yaml`

**Mechanism.** Add an optional `applies_to: Vec<String>` to the `CHECKS.yaml` entry; when present on an instance, it replaces the definition's `applies_to` for that instance's file selection. This generalizes the existing `check:` aliasing (§2.1) so that the *one field that legitimately varies per language* becomes per-instance, while the command stays defined once in the shared definition.

**Touch points (mechanism, not full implementation):**

- `ParsedCheckConfig` (`config.rs:421`) — add `#[serde(default)] applies_to: Vec<String>`.
- `CheckConfig` (`config.rs:63`) — carry the override through resolution.
- The runner call site into `run_declarative_check` — pass the override.
- `run_declarative_check` / `select_files` (`executor.rs:34,127`) — prefer the instance override when non-empty, else `package.applies_to`.

Note the definition validator still requires a non-empty `applies_to` (`validate_declarative_implementation`, `mod.rs:293`), so the shared definition keeps a default glob (e.g. `**/*`); the instance override narrows it. Resulting config in §4.2.

**Pros:** smallest delta; additive and backward-compatible at the manifest layer (no manifest schema change, so `declarative-v1` / `api_version: v1` are untouched); keeps the command in one declarative file; composes with the existing exec_paths flow. **Cons:** introduces a second source of truth for "what files a check matches" (definition default vs instance override); see the downgrade hazard below.

### Option 2: config/param templating in the definition

**Mechanism.** Add a `{{config.*}}` (or a `params:` block) so a single definition reads `applies_to: ["{{config.globs}}"]` (and could parameterize args too) and each instance passes `config: { globs: [...] }`.

**Touch points:** extend the arg-ref allowlist (`mod.rs:454`) and the finding templater (`template.rs`), define list-valued template semantics for `applies_to` (it is a `Vec<String>`), and thread `config` into `select_files`. **Pros:** most general — parameterizes any field, not just the glob. **Cons:** materially larger surface (new template language + list semantics + selection plumbing + spec/tests); likely warrants a `declarative-v2` / `api_version: v2` bump because it changes manifest semantics.

### Option 3: a first-party config-driven `prettier`/`format` built-in

**Mechanism.** Implement a native (Rust or WASM-bundled) `format/prettier` check whose `config` carries `globs` (and maybe tool/args), instantiated N times via `check:` aliasing — exactly the `file/size` / `forbidden-paths` pattern, which already supports per-instance globs in `config` (e.g. `forbidden-paths.patterns`, `forbidden-imports-deps.include_globs`).

**Pros:** most idiomatic to the *existing* multi-instance pattern; richest validation. **Cons:** heaviest — it hand-rolls tool-spawning logic that the declarative runtime was built to generalize (see `mod.rs:9-15`), ships in the binary, and needs a release per behavior change. This re-introduces exactly the per-check code the declarative tier eliminates.

### Recommendation among changes

**Option 1.** It is the smallest change, it fills the single real gap (only the glob needs to vary), it preserves "command defined once in a declarative file", and it needs no manifest-schema/version bump.

### Version / release implications (applies to §2 and §3)

checkleft is published from mono releases and consumed downstream via `rules_multitool` (e.g. `checkleft-sandbox`). Two distinct axes:

- **Definitions vs binary.** Adding/maintaining *definition files* under mono's `exec_paths` (`tools/checkleft/checks/...`) needs **no checkleft release** — they are on-disk data resolved at run time (`allow_override_bundled: true`). This holds for both today's Option B and (after the change lands) Option 1's single definition. Downstream repos that rely on **bundled** defs would need a release to get new names baked in (`bundled.rs:88`), or they can adopt their own `exec_paths`.
- **Schema changes need a release + consumer bump.** Options 1, 2, and 3 all change the binary. Consumers must bump their `rules_multitool` checkleft pin before they can use the new field/built-in.
- **Silent-downgrade hazard (Option 1).** `ParsedCheckConfig` is **not** `#[serde(deny_unknown_fields)]` (`config.rs:421`), so an *older* checkleft binary reading a `CHECKS.yaml` that uses the new `applies_to:` instance field would **silently ignore it** — every id would fall back to the definition's default glob and over-match, with no error. This is a real foot-gun: gate it behind a version bump, and consider adding explicit validation/a version guard. By contrast, the declarative *manifest* structs **are** `#[serde(deny_unknown_fields)]` (`mod.rs:212,223,277`), so adding fields *there* (Option 2) fails loudly on old binaries — a safer, more visible failure mode.

---

## 4. Recommendation: concrete config

Two concrete answers, depending on whether you need four ids now.

### 4.1 Today (no checkleft change): four definition files + four `CHECKS.yaml` entries

mono already configures `check_definitions.exec_paths: [tools/checkleft/checks]` with `allow_override_bundled: true`, so just drop the definitions on disk and reference them. Create one file per language under `tools/checkleft/checks/format/`. They are identical except `id` and `applies_to`:

`tools/checkleft/checks/format/ts.yaml`:

```yaml
id: format/ts
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to:
  - "**/*.ts"
  - "**/*.tsx"
needs:
  npx:
    default:
      path: npx
invocations:
  - id: format
    run: npx
    mode: batch
    args:
      - "--yes"
      - "prettier"
      - "--list-different"
      - "{{files}}"
    exit:
      "0": ok          # all files already formatted
      "1": findings     # some files differ → printed one per line on stdout
      "2": error        # prettier internal/config/parse error
      default: error
    transform:
      kind: linelist
      message: "file needs prettier formatting"
      remediations:
        - "Run `npx --yes prettier --write {{input.file}}` to fix."
```

`format/js.yaml`, `format/md.yaml`, `format/html.yaml` are byte-for-byte identical except:

| file | `id` | `applies_to` |
|------|------|--------------|
| `format/js.yaml` | `format/js` | `**/*.js`, `**/*.jsx`, `**/*.cjs`, `**/*.mjs` |
| `format/md.yaml` | `format/md` | `**/*.md`, `**/*.markdown` |
| `format/html.yaml` | `format/html` | `**/*.html` |

Then in `/CHECKS.yaml`:

```yaml
checks:
  - id: format/ts
    policy:
      severity: error
  - id: format/js
    policy:
      severity: error
  - id: format/md
    policy:
      severity: warning
  - id: format/html
    policy:
      severity: error
```

This works today and gives four independently-controllable ids. The cost: the `needs`/`invocations` block is duplicated across the four files, and (per §2.4) there is no mechanism to share it.

**If four ids are not required**, collapse to one definition `tools/checkleft/checks/format/prettier.yaml` with `applies_to` listing every extension above, and a single `- id: format/prettier` entry — zero duplication, one id.

### 4.2 After Option 1 lands: one definition, four globbed instances

One shared definition `tools/checkleft/checks/format/prettier.yaml`:

```yaml
id: format/prettier
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to:
  - "**/*"            # default; overridden per instance
needs:
  npx:
    default:
      path: npx
invocations:
  - id: format
    run: npx
    mode: batch
    args:
      - "--yes"
      - "prettier"
      - "--list-different"
      - "{{files}}"
    exit:
      "0": ok
      "1": findings
      "2": error
      default: error
    transform:
      kind: linelist
      message: "file needs prettier formatting"
      remediations:
        - "Run `npx --yes prettier --write {{input.file}}` to fix."
```

`/CHECKS.yaml` — the command is defined once; each id overrides only the glob:

```yaml
checks:
  - id: format/ts
    check: format/prettier
    applies_to: ["**/*.ts", "**/*.tsx"]
  - id: format/js
    check: format/prettier
    applies_to: ["**/*.js", "**/*.jsx", "**/*.cjs", "**/*.mjs"]
  - id: format/md
    check: format/prettier
    applies_to: ["**/*.md", "**/*.markdown"]
  - id: format/html
    check: format/prettier
    applies_to: ["**/*.html"]
```

This is the target outcome. It requires the §3 Option 1 change in checkleft (released, with downstream version bumps) before it will work; on an older binary the `applies_to:` lines are silently ignored (§3).

### 4.3 Two grounded caveats on the prettier invocation

**Flag: `--list-different`, not `--check`.** The task framed the command as `npx --yes prettier --check <files>`. Grounded in how transforms actually parse output, `--check` is the wrong flag for the available transforms: `prettier --check` decorates stdout with `[warn] <file>` lines plus a trailing summary line ("Code style issues found…"). The `linelist` transform treats **each** stdout line as a file path (`transform.rs`; see the rustfmt precedent using `-l` in `format/rust.yaml`), so `--check` would yield findings whose path is literally `[warn] foo.ts` and a bogus finding from the summary line. `prettier --list-different` (alias `-l`) prints **one bare path per line** for files that differ and exits 1, which maps cleanly onto `linelist` — the exact shape rustfmt's `-l` relies on. The only implemented transforms are `json`, `passthrough`, `linelist`; `regex` (which could otherwise strip the `[warn]` prefix) is reserved and hard-errors (`mod.rs:573`). So `--list-different` realizes the same intent ("check formatting, list offenders, fail") in a way the runtime can actually parse. If you must keep `--check` verbatim, you would need the (unimplemented) `regex` transform first.

**Binary: `npx --yes` is non-hermetic.** The config uses `needs.npx.default.path: npx`, i.e. whatever `npx` is on `PATH`, and `--yes` lets npx fetch prettier on demand. This contradicts checkleft's hermetic-toolchain preference (the rust/bazel definitions use `bazel:` bindings with a `path:` fallback). Practical consequences: first run hits the network, and the prettier version floats. For reproducible CI, pin the version in args (`prettier@<x.y.z>` instead of bare `prettier`) and/or back `needs.npx` with a hermetic node toolchain via a `bazel:` binding once one is available, keeping `path: npx` only as a `fallback` (the `fallback` slot requires a `bazel` default — `mod.rs:330`).

---

## Open Questions

- **Is per-language id granularity actually required?** If `format/ts|js|md|html` exist only to format-check those files (same command, same severity), the one-id combined-glob definition (§2.5 A / §4.1 note) satisfies the goal today with zero duplication and zero checkleft change. The four-id requirement is only justified if you need per-language `enabled`/`severity`/`bypass` or per-directory child-`CHECKS.yaml` overrides. Confirm before investing in the §3 change.
- **Which downstream consumers need this, and via bundled or exec_paths?** If only mono needs it, exec_paths definitions need no release. If `checkleft-sandbox` (or other `rules_multitool` consumers) need it as a *bundled* zero-install check, that is a release + a `BUNDLED_CHECK_DEFS` row, and — for Option 1's `applies_to` override — a binary release plus a consumer version bump.
- **Hermetic prettier?** Is there an appetite for a hermetic node/prettier Bazel toolchain so the definition can use a `bazel:` binding (matching rust/bazel) instead of `npx --yes`? That decision is independent of the reuse question but affects determinism.
