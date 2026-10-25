//! Checkleft check: fail a change whose `.proto` files are not backward
//! compatible with the same files at the change's base revision.
//!
//! Registered under the canonical id `proto/breaking`. Runs inside the
//! checkleft wasm host. For every `.proto` file the framework schedules it on,
//! the check parses the file as it is now and as it was at the base revision
//! (the base content the host already supplies through
//! [`ChangeSet::base_files`](checkleft_check_sdk::ChangeSet::base_files)), and
//! applies the protocol evolution rules a fleet of independently upgraded
//! peers depends on:
//!
//! - field numbers are permanent: no number reuse, no type, cardinality,
//!   name, oneof or `json_name` change on an existing number;
//! - a removed field or enum value must `reserved` both its number and its
//!   name, and a reservation is never dropped;
//! - no enum value renumbering or renaming;
//! - no message, enum, service or RPC removal or rename;
//! - no package or syntax change;
//! - no change to an existing RPC's request type, response type or streaming.
//!
//! Each violation is one finding at the line in the new file (for a removed
//! definition, at the line of the definition that used to contain it).
//!
//! Parsing is done in-process by `protox-parse`, a pure-Rust protobuf parser
//! that produces a `FileDescriptorProto` with source spans, so the check needs
//! no `protoc`, no `buf` and no imports: every rule above is a property of a
//! single file, and type references are compared as written (a differently
//! qualified reference to the same type counts as a change, which fails
//! closed).
//!
//! ## Which files
//!
//! The check carries no file filter of its own. A `CHECKS.yaml` instance
//! scopes it with the framework's `include` globs to the protocol directories
//! that declare a compatibility contract:
//!
//! ```yaml
//! - id: proto/breaking
//!   include:
//!     - "tools/boss/protocol-spoke/proto/**/*.proto"
//!   policy:
//!     severity: error
//! ```
//!
//! ## Base revision
//!
//! - A file that is **new** in the change has nothing to be compatible with
//!   and passes.
//! - A **modified** file is compared with its own base-revision content; a
//!   **renamed** file with the content of its old path.
//! - A modified or renamed file whose base content is **unavailable** on a
//!   change run is an error finding, never a silent pass: checkleft deepens
//!   shallow checkouts to reach the merge base itself, so a missing base means
//!   the comparison could not be made and the gate must say so.
//! - Under `checkleft run --all` there is no base revision (every tracked file
//!   is listed as modified against nothing), so the check is a documented
//!   no-op, like `change/file-count`.
//! - A deleted `.proto` file never reaches a file-scoped check (the scheduler
//!   skips deletions); deleting a whole schema file is a review matter.
//!
//! The check has no configuration.

mod compare;
#[cfg(test)]
mod fixtures;
mod schema;
#[cfg(test)]
mod tests;

use checkleft_check_sdk::{ChangeKind, ChangedFile, CheckInput, Finding, check};

pub use compare::{Violation, compare};
pub use schema::{ParseFailure, Schema, parse};

#[check(
    name = "proto/breaking",
    description = "fails a change whose .proto files are not backward compatible with the same files at the change's base revision",
    severity = error
)]
pub fn proto_breaking_check(input: CheckInput) -> Vec<Finding> {
    // `--all` lists every tracked file as modified with no base revision; there
    // is nothing to compare against, so this is a documented no-op.
    if input.changeset.whole_repo {
        return Vec::new();
    }

    let mut findings = Vec::new();
    for file in &input.changeset.changed_files {
        match file.kind {
            // Nothing to be compatible with.
            ChangeKind::Added | ChangeKind::Deleted => continue,
            ChangeKind::Modified | ChangeKind::Renamed => {}
        }
        let Some(base) = input.changeset.base_content_for(file) else {
            findings.push(base_unavailable(file));
            continue;
        };
        let Ok(new_source) = std::fs::read_to_string(&file.path) else {
            findings.push(
                Finding::error(format!("`{}` could not be read from the sandbox", file.path))
                    .at(&file.path, 1)
                    .with_remediation("Report this: the host scheduled the check on a file it did not stage."),
            );
            continue;
        };
        findings.extend(check_file(file, base, &new_source));
    }
    findings
}

/// Compare one file's base-revision text with its current text and turn each
/// violation into a finding on the new file.
pub fn check_file(file: &ChangedFile, base_source: &str, new_source: &str) -> Vec<Finding> {
    let base_label = file.old_path.as_deref().unwrap_or(&file.path);
    let old = match schema::parse(base_label, base_source) {
        Ok(schema) => schema,
        Err(failure) => {
            return vec![
                Finding::error(format!(
                    "base revision of `{base_label}` does not parse, so compatibility cannot be checked: {}",
                    failure.message
                ))
                .at(&file.path, 1)
                .with_remediation(
                    "The base revision is what peers were built from. If it is genuinely unparseable, fix it in its own change first.",
                ),
            ];
        }
    };
    let new = match schema::parse(&file.path, new_source) {
        Ok(schema) => schema,
        Err(failure) => {
            return vec![
                Finding::error(format!("`{}` does not parse: {}", file.path, failure.message))
                    .at(&file.path, failure.line.unwrap_or(1))
                    .with_remediation(
                        "Fix the syntax error; the compatibility rules are checked once the file parses.",
                    ),
            ];
        }
    };

    compare::compare(&old, &new)
        .into_iter()
        .map(|violation| {
            Finding::error(format!("backward-incompatible schema change: {}", violation.message))
                .at(&file.path, violation.line)
                .with_remediation(violation.remediation)
        })
        .collect()
}

fn base_unavailable(file: &ChangedFile) -> Finding {
    let base_label = file.old_path.as_deref().unwrap_or(&file.path);
    Finding::error(format!(
        "base-revision content of `{base_label}` is unavailable, so its backward compatibility cannot be checked"
    ))
    .at(&file.path, 1)
    .with_remediation(
        "The check compares the file with the change's base revision and refuses to pass without it. Make sure the base revision is reachable in this checkout (checkleft deepens shallow clones itself); if the file is new in this change, record it as added rather than modified.",
    )
}

// NOTE: this crate is an rlib, NOT a standalone wasm component. The component
// ABI (`export_checks!` → `list-checks`/`run-check`) is wired ONCE in the
// aggregating `checkleft-preinstalled-bundle` crate, which links this check
// into a single multiplexed component.
