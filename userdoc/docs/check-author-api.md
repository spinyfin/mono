# Writing checks

Checks are implemented in Rust and registered as built-ins.

For the high-level execution contract (what checks receive and return, hermetic expectations, and change-scoped behavior), see [Concepts](concepts.md).

## Core trait

Each check implements:

```rust
#[async_trait]
pub trait Check: Send + Sync {
    fn id(&self) -> &str;
    fn description(&self) -> &str;

    async fn run(
        &self,
        changeset: &ChangeSet,
        tree: &dyn SourceTree,
        config: &toml::Value,
    ) -> Result<CheckResult>;
}
```

## Inputs

- `changeset`: changed files, kinds, and optional description metadata.
- `tree`: safe source-tree access (`read_file`, `exists`, `list_dir`, `glob`).
- `config`: the resolved `[checks.<id>.config]` table as TOML.

## Output contract

Return `CheckResult` with:

- `check_id`
- `findings[]`

Each finding supports:

- `severity`: `error`, `warning`, or `info`
- `message`
- `location` (`path`, optional `line`, optional `column`)
- `remediation` (optional)
- `suggested_fix` (optional)

## Authoring steps

1. Add a new module in `cli/checkleft/src/checks/`.
2. Implement `Check`.
3. Register it in `cli/checkleft/src/checks/mod.rs`.
4. Add/update `CHECKS.toml` entries to configure an instance.
5. Add tests for:
   - happy path
   - invalid config
   - non-target files
   - edge cases for path/content parsing

## Best practices

- Skip deleted files unless your check explicitly needs them.
- Parse config once per run.
- Keep findings stable and actionable.
- Use `warning` when guidance should not fail builds.
- Use `error` for policy that must block merges.

## Minimal skeleton

```rust
#[derive(Debug, Default)]
pub struct ExampleCheck;

#[async_trait]
impl Check for ExampleCheck {
    fn id(&self) -> &str { "example" }
    fn description(&self) -> &str { "validates example policy" }

    async fn run(
        &self,
        changeset: &ChangeSet,
        tree: &dyn SourceTree,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        let mut findings = Vec::new();

        for changed in &changeset.changed_files {
            if matches!(changed.kind, ChangeKind::Deleted) {
                continue;
            }

            // read file and evaluate policy
            let _contents = tree.read_file(&changed.path)?;

            // push findings as needed
        }

        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings,
        })
    }
}
```
