//! The backward-compatibility rules, applied to an old and a new [`Schema`] of
//! the same file.
//!
//! Each rule produces a [`Violation`] anchored to a line of the *new* file.
//! Something that no longer exists in the new file (a removed field, value,
//! message or RPC) is reported at the line of the definition that used to
//! contain it: the enclosing message, enum or service, or the package
//! statement for a top-level definition.

use std::collections::BTreeSet;

use crate::schema::{Enum, Message, Schema, Service};

/// One incompatibility between the old and new schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// 1-based line in the new file.
    pub line: u32,
    pub message: String,
    pub remediation: String,
}

const RESERVE_REMEDIATION: &str = "Keep the definition, or delete it and add both its number and its name to a `reserved` statement so neither can be reused.";
const NEW_FIELD_REMEDIATION: &str = "A changed meaning is a new field or a new message: add one under a fresh number and leave the existing one as it was.";
const NEW_PACKAGE_REMEDIATION: &str = "Peers are routinely different builds; an incompatible shape belongs in a new package version (for example `v2`), not in an edit to the released one.";

/// Compare `old` with `new` and return every violation, ordered by line.
pub fn compare(old: &Schema, new: &Schema) -> Vec<Violation> {
    let mut out = Vec::new();

    if old.syntax != new.syntax {
        out.push(Violation {
            line: new.syntax_line,
            message: format!("syntax changed from `{}` to `{}`", old.syntax, new.syntax),
            remediation: NEW_PACKAGE_REMEDIATION.to_owned(),
        });
    }
    if old.package != new.package {
        out.push(Violation {
            line: new.package_line,
            message: format!(
                "package changed from `{}` to `{}`; every message and service in the file is renamed on the wire",
                display_package(old.package.as_deref()),
                display_package(new.package.as_deref())
            ),
            remediation: NEW_PACKAGE_REMEDIATION.to_owned(),
        });
    }

    for (name, old_message) in &old.messages {
        match new.messages.get(name) {
            Some(new_message) => compare_message(name, old_message, new_message, &mut out),
            None if old_message.map_entry => {}
            None => out.push(Violation {
                line: line_of_container(new, name),
                message: format!("message `{name}` was removed or renamed"),
                remediation: "Keep the message; a shape that is no longer needed stays in the schema (optionally marked `deprecated`) so old peers can still name it.".to_owned(),
            }),
        }
    }
    for (name, old_enum) in &old.enums {
        match new.enums.get(name) {
            Some(new_enum) => compare_enum(name, old_enum, new_enum, &mut out),
            None => out.push(Violation {
                line: line_of_container(new, name),
                message: format!("enum `{name}` was removed or renamed"),
                remediation: "Keep the enum; a shape that is no longer needed stays in the schema (optionally marked `deprecated`) so old peers can still name it.".to_owned(),
            }),
        }
    }
    for (name, old_service) in &old.services {
        match new.services.get(name) {
            Some(new_service) => compare_service(name, old_service, new_service, &mut out),
            None => out.push(Violation {
                line: new.package_line,
                message: format!("service `{name}` was removed or renamed"),
                remediation: "Keep the service; retire RPCs by leaving them in place and marking them `deprecated`."
                    .to_owned(),
            }),
        }
    }

    out.sort_by(|a, b| a.line.cmp(&b.line).then_with(|| a.message.cmp(&b.message)));
    out
}

fn display_package(package: Option<&str>) -> String {
    package.map(str::to_owned).unwrap_or_else(|| "(none)".to_owned())
}

/// The line in `new` of the definition that contained `name` in the old file:
/// the parent message when `name` is nested and the parent still exists,
/// otherwise the package statement.
fn line_of_container(new: &Schema, name: &str) -> u32 {
    match name.rsplit_once('.') {
        Some((parent, _)) => new.messages.get(parent).map(|m| m.line).unwrap_or(new.package_line),
        None => new.package_line,
    }
}

fn compare_message(name: &str, old: &Message, new: &Message, out: &mut Vec<Violation>) {
    for (number, old_field) in &old.fields {
        match new.fields.get(number) {
            None => {
                let number_reserved = contains(&new.reserved_numbers, *number);
                let name_reserved = new.reserved_names.contains(&old_field.name);
                if !number_reserved || !name_reserved {
                    let missing = match (number_reserved, name_reserved) {
                        (false, false) => "its number and its name",
                        (false, true) => "its number",
                        (true, false) => "its name",
                        (true, true) => unreachable!(),
                    };
                    out.push(Violation {
                        line: new.line,
                        message: format!(
                            "field `{}` ({number}) of message `{name}` was removed without reserving {missing}",
                            old_field.name
                        ),
                        remediation: RESERVE_REMEDIATION.to_owned(),
                    });
                }
            }
            Some(new_field) => {
                if old_field.name != new_field.name {
                    out.push(Violation {
                        line: new_field.line,
                        message: format!(
                            "field {number} of message `{name}` was renamed from `{}` to `{}`",
                            old_field.name, new_field.name
                        ),
                        remediation: NEW_FIELD_REMEDIATION.to_owned(),
                    });
                }
                if old_field.type_text != new_field.type_text {
                    out.push(Violation {
                        line: new_field.line,
                        message: format!(
                            "field `{}` ({number}) of message `{name}` changed type from `{}` to `{}`",
                            new_field.name, old_field.type_text, new_field.type_text
                        ),
                        remediation: NEW_FIELD_REMEDIATION.to_owned(),
                    });
                }
                if old_field.cardinality != new_field.cardinality {
                    out.push(Violation {
                        line: new_field.line,
                        message: format!(
                            "field `{}` ({number}) of message `{name}` changed from {} to {}",
                            new_field.name,
                            old_field.cardinality.as_str(),
                            new_field.cardinality.as_str()
                        ),
                        remediation: NEW_FIELD_REMEDIATION.to_owned(),
                    });
                }
                if old_field.oneof != new_field.oneof {
                    out.push(Violation {
                        line: new_field.line,
                        message: format!(
                            "field `{}` ({number}) of message `{name}` moved {} {}",
                            new_field.name,
                            match &old_field.oneof {
                                Some(oneof) => format!("out of oneof `{oneof}`"),
                                None => "from outside any oneof".to_owned(),
                            },
                            match &new_field.oneof {
                                Some(oneof) => format!("into oneof `{oneof}`"),
                                None => "to outside any oneof".to_owned(),
                            }
                        ),
                        remediation: NEW_FIELD_REMEDIATION.to_owned(),
                    });
                }
                if old_field.json_name.is_some() || new_field.json_name.is_some() {
                    let old_json = old_field.json_name.as_deref().unwrap_or("(default)");
                    let new_json = new_field.json_name.as_deref().unwrap_or("(default)");
                    if old_json != new_json {
                        out.push(Violation {
                            line: new_field.line,
                            message: format!(
                                "field `{}` ({number}) of message `{name}` changed json_name from `{old_json}` to `{new_json}`",
                                new_field.name
                            ),
                            remediation: NEW_FIELD_REMEDIATION.to_owned(),
                        });
                    }
                }
            }
        }
    }

    for (number, new_field) in &new.fields {
        if old.fields.contains_key(number) {
            continue;
        }
        if contains(&old.reserved_numbers, *number) {
            out.push(Violation {
                line: new_field.line,
                message: format!(
                    "field `{}` ({number}) of message `{name}` reuses reserved number {number}",
                    new_field.name
                ),
                remediation: "Field numbers are permanent. Pick a number that has never been used in this message."
                    .to_owned(),
            });
        }
        if old.reserved_names.contains(&new_field.name) {
            out.push(Violation {
                line: new_field.line,
                message: format!(
                    "field `{}` ({number}) of message `{name}` reuses reserved name `{}`",
                    new_field.name, new_field.name
                ),
                remediation:
                    "Field names are permanent once reserved. Pick a name that has never been used in this message."
                        .to_owned(),
            });
        }
    }

    report_lost_reservations(
        &format!("message `{name}`"),
        new.line,
        &old.reserved_numbers,
        &new.reserved_numbers,
        &old.reserved_names,
        &new.reserved_names,
        out,
    );
}

fn compare_enum(name: &str, old: &Enum, new: &Enum, out: &mut Vec<Violation>) {
    for old_value in &old.values {
        let same_number: Vec<_> = new.values.iter().filter(|v| v.number == old_value.number).collect();
        if same_number.iter().any(|v| v.name == old_value.name) {
            continue;
        }
        if let Some(renamed) = same_number.first() {
            out.push(Violation {
                line: renamed.line,
                message: format!(
                    "enum value {} of `{name}` was renamed from `{}` to `{}`",
                    old_value.number, old_value.name, renamed.name
                ),
                remediation: "Enum value names are part of the contract (JSON and generated code). Keep the old name; add a new value for a new meaning.".to_owned(),
            });
            continue;
        }
        let number_reserved = contains(&new.reserved_numbers, old_value.number);
        let name_reserved = new.reserved_names.contains(&old_value.name);
        if !number_reserved || !name_reserved {
            let missing = match (number_reserved, name_reserved) {
                (false, false) => "its number and its name",
                (false, true) => "its number",
                (true, false) => "its name",
                (true, true) => unreachable!(),
            };
            out.push(Violation {
                line: new.line,
                message: format!(
                    "enum value `{}` ({}) of `{name}` was removed without reserving {missing}",
                    old_value.name, old_value.number
                ),
                remediation: RESERVE_REMEDIATION.to_owned(),
            });
        }
    }

    let old_numbers: BTreeSet<i32> = old.values.iter().map(|v| v.number).collect();
    for new_value in &new.values {
        if old_numbers.contains(&new_value.number) {
            continue;
        }
        if contains(&old.reserved_numbers, new_value.number) {
            out.push(Violation {
                line: new_value.line,
                message: format!(
                    "enum value `{}` ({}) of `{name}` reuses reserved number {}",
                    new_value.name, new_value.number, new_value.number
                ),
                remediation: "Enum numbers are permanent. Pick a number that has never been used in this enum."
                    .to_owned(),
            });
        }
        if old.reserved_names.contains(&new_value.name) {
            out.push(Violation {
                line: new_value.line,
                message: format!(
                    "enum value `{}` ({}) of `{name}` reuses reserved name `{}`",
                    new_value.name, new_value.number, new_value.name
                ),
                remediation:
                    "Enum value names are permanent once reserved. Pick a name that has never been used in this enum."
                        .to_owned(),
            });
        }
    }

    report_lost_reservations(
        &format!("enum `{name}`"),
        new.line,
        &old.reserved_numbers,
        &new.reserved_numbers,
        &old.reserved_names,
        &new.reserved_names,
        out,
    );
}

fn compare_service(name: &str, old: &Service, new: &Service, out: &mut Vec<Violation>) {
    for (method_name, old_method) in &old.methods {
        let Some(new_method) = new.methods.get(method_name) else {
            out.push(Violation {
                line: new.line,
                message: format!("rpc `{method_name}` of service `{name}` was removed or renamed"),
                remediation:
                    "Keep the RPC; retire it by marking it `deprecated` so old peers still get a typed answer."
                        .to_owned(),
            });
            continue;
        };
        if old_method.input != new_method.input {
            out.push(Violation {
                line: new_method.line,
                message: format!(
                    "rpc `{method_name}` of service `{name}` changed its request type from `{}` to `{}`",
                    old_method.input, new_method.input
                ),
                remediation: "Add fields to the existing request message instead, or add a new RPC.".to_owned(),
            });
        }
        if old_method.output != new_method.output {
            out.push(Violation {
                line: new_method.line,
                message: format!(
                    "rpc `{method_name}` of service `{name}` changed its response type from `{}` to `{}`",
                    old_method.output, new_method.output
                ),
                remediation: "Add fields to the existing response message instead, or add a new RPC.".to_owned(),
            });
        }
        if old_method.client_streaming != new_method.client_streaming
            || old_method.server_streaming != new_method.server_streaming
        {
            out.push(Violation {
                line: new_method.line,
                message: format!(
                    "rpc `{method_name}` of service `{name}` changed streaming from {} to {}",
                    streaming_text(old_method.client_streaming, old_method.server_streaming),
                    streaming_text(new_method.client_streaming, new_method.server_streaming)
                ),
                remediation: "Streaming is part of the RPC's wire contract. Add a new RPC with the new shape."
                    .to_owned(),
            });
        }
    }
}

fn streaming_text(client: bool, server: bool) -> &'static str {
    match (client, server) {
        (false, false) => "unary",
        (true, false) => "client streaming",
        (false, true) => "server streaming",
        (true, true) => "bidirectional streaming",
    }
}

/// Reserved numbers and names must stay reserved: a dropped reservation is the
/// first step of a reuse, and nothing else would catch it once the dropping
/// change has merged.
fn report_lost_reservations(
    what: &str,
    line: u32,
    old_numbers: &[(i32, i32)],
    new_numbers: &[(i32, i32)],
    old_names: &BTreeSet<String>,
    new_names: &BTreeSet<String>,
    out: &mut Vec<Violation>,
) {
    for (start, end) in uncovered(old_numbers, new_numbers) {
        let range = if start == end {
            start.to_string()
        } else {
            format!(
                "{start} to {}",
                if end == i32::MAX {
                    "max".to_owned()
                } else {
                    end.to_string()
                }
            )
        };
        out.push(Violation {
            line,
            message: format!(
                "{what} no longer reserves number{} {range}",
                if start == end { "" } else { "s" }
            ),
            remediation: "Reservations are permanent; put the `reserved` statement back.".to_owned(),
        });
    }
    for name in old_names.difference(new_names) {
        out.push(Violation {
            line,
            message: format!("{what} no longer reserves name `{name}`"),
            remediation: "Reservations are permanent; put the `reserved` statement back.".to_owned(),
        });
    }
}

/// Whether `number` falls in one of the inclusive `ranges`.
fn contains(ranges: &[(i32, i32)], number: i32) -> bool {
    ranges.iter().any(|(start, end)| *start <= number && number <= *end)
}

/// Merge inclusive ranges into a sorted, non-overlapping list. Bounds are
/// widened to i64 so `reserved 1000 to max` (`i32::MAX`) needs no overflow
/// handling in the arithmetic below.
fn normalize(ranges: &[(i32, i32)]) -> Vec<(i64, i64)> {
    let mut sorted: Vec<(i64, i64)> = ranges
        .iter()
        .map(|(s, e)| (i64::from(*s), i64::from(*e)))
        .filter(|(s, e)| s <= e)
        .collect();
    sorted.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::with_capacity(sorted.len());
    for (start, end) in sorted {
        match merged.last_mut() {
            Some((_, last_end)) if start <= *last_end + 1 => *last_end = (*last_end).max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// The parts of `old` (inclusive ranges) not covered by `new`.
fn uncovered(old: &[(i32, i32)], new: &[(i32, i32)]) -> Vec<(i32, i32)> {
    let new = normalize(new);
    let mut out = Vec::new();
    for (mut start, end) in normalize(old) {
        for (new_start, new_end) in &new {
            if *new_end < start {
                continue;
            }
            if *new_start > end {
                break;
            }
            if *new_start > start {
                out.push((start, new_start - 1));
            }
            start = new_end + 1;
            if start > end {
                break;
            }
        }
        if start <= end {
            out.push((start, end));
        }
    }
    out.into_iter()
        .map(|(s, e)| {
            (
                i32::try_from(s).unwrap_or(i32::MAX),
                i32::try_from(e).unwrap_or(i32::MAX),
            )
        })
        .collect()
}

#[cfg(test)]
mod range_tests {
    use super::*;

    #[test]
    fn uncovered_reports_gaps_only() {
        assert_eq!(uncovered(&[(1, 10)], &[(1, 10)]), Vec::<(i32, i32)>::new());
        assert_eq!(uncovered(&[(1, 10)], &[(3, 4)]), vec![(1, 2), (5, 10)]);
        assert_eq!(uncovered(&[(1, 10)], &[(1, 4), (5, 10)]), Vec::<(i32, i32)>::new());
        assert_eq!(uncovered(&[(1, 10)], &[]), vec![(1, 10)]);
        assert_eq!(uncovered(&[(5, 5)], &[(1, 4), (6, 9)]), vec![(5, 5)]);
        assert_eq!(
            uncovered(&[(1000, i32::MAX)], &[(1000, i32::MAX)]),
            Vec::<(i32, i32)>::new()
        );
    }
}
