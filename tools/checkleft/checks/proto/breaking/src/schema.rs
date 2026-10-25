//! A `.proto` file reduced to the shape the compatibility rules compare.
//!
//! [`parse`] runs `protox-parse` over one file's text and flattens the
//! resulting `FileDescriptorProto` into [`Schema`]: every message, enum and
//! service keyed by its name within the file, every field and enum value keyed
//! by number, and a 1-based line for each definition taken from the parser's
//! `source_code_info` spans. Named references resolve in their declaration
//! scope to canonical same-file names; unresolved references fail explicitly.

use std::collections::{BTreeMap, BTreeSet};

use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FileDescriptorProto};

/// One parsed `.proto` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    /// `syntax = "..."`; `proto2` when the statement is absent.
    pub syntax: String,
    /// Line of the `syntax` statement, or 1.
    pub syntax_line: u32,
    /// `package ...;` if declared.
    pub package: Option<String>,
    /// Line of the `package` statement, or 1.
    pub package_line: u32,
    /// Messages, including nested ones, keyed by dotted name within the file
    /// (`Outer.Inner`).
    pub messages: BTreeMap<String, Message>,
    /// Enums, including nested ones, keyed by dotted name within the file.
    pub enums: BTreeMap<String, Enum>,
    /// Services keyed by name.
    pub services: BTreeMap<String, Service>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub line: u32,
    pub map_entry: bool,
    pub fields: BTreeMap<i32, Field>,
    /// Inclusive reserved number ranges.
    pub reserved_numbers: Vec<(i32, i32)>,
    pub reserved_names: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub number: i32,
    pub line: u32,
    /// The scalar type (`int32`) or canonical named type (`.pkg.Foo`).
    pub type_text: String,
    pub named_type: bool,
    pub cardinality: Cardinality,
    /// The oneof this field belongs to, if any. Synthetic oneofs created by
    /// proto3 `optional` are not oneofs here.
    pub oneof: Option<String>,
    /// An explicit `json_name` option, when set.
    pub json_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality {
    /// `repeated` (including map fields).
    Repeated,
    /// proto2 `required`.
    Required,
    /// Explicit presence: proto2 `optional` or proto3 `optional`.
    Optional,
    /// proto3 singular field without `optional`.
    Implicit,
}

impl Cardinality {
    pub fn as_str(self) -> &'static str {
        match self {
            Cardinality::Repeated => "repeated",
            Cardinality::Required => "required",
            Cardinality::Optional => "optional",
            Cardinality::Implicit => "singular (implicit presence)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enum {
    pub line: u32,
    /// Values in declaration order. Two values may share a number when the
    /// enum sets `allow_alias`.
    pub values: Vec<EnumValue>,
    /// Inclusive reserved number ranges.
    pub reserved_numbers: Vec<(i32, i32)>,
    pub reserved_names: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumValue {
    pub name: String,
    pub number: i32,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub line: u32,
    pub methods: BTreeMap<String, Method>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    pub line: u32,
    /// Canonical request type.
    pub input: String,
    /// Canonical response type.
    pub output: String,
    pub client_streaming: bool,
    pub server_streaming: bool,
}

/// A syntax error in the file, with the 1-based line it was found on when the
/// parser could attribute it to one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseFailure {
    pub message: String,
    pub line: Option<u32>,
}

/// Parse one `.proto` file's text. `name` only labels diagnostics.
pub fn parse(name: &str, source: &str) -> Result<Schema, ParseFailure> {
    let descriptor = protox_parse::parse(name, source).map_err(|err| ParseFailure {
        message: err.to_string(),
        line: err.span().map(|span| line_of_byte_offset(source, span.start)),
    })?;
    let mut schema = Schema::from_descriptor(&descriptor);
    super::resolve::resolve(&mut schema)?;
    Ok(schema)
}

fn line_of_byte_offset(source: &str, offset: usize) -> u32 {
    let offset = offset.min(source.len());
    let newlines = source.as_bytes()[..offset].iter().filter(|b| **b == b'\n').count();
    u32::try_from(newlines + 1).unwrap_or(u32::MAX)
}

// FileDescriptorProto / DescriptorProto / ... field numbers, as used in
// `SourceCodeInfo.Location.path`.
const FILE_PACKAGE: i32 = 2;
const FILE_MESSAGE_TYPE: i32 = 4;
const FILE_ENUM_TYPE: i32 = 5;
const FILE_SERVICE: i32 = 6;
const FILE_SYNTAX: i32 = 12;
const MESSAGE_FIELD: i32 = 2;
const MESSAGE_NESTED_TYPE: i32 = 3;
const MESSAGE_ENUM_TYPE: i32 = 4;
const ENUM_VALUE: i32 = 2;
const SERVICE_METHOD: i32 = 2;

/// Lines of every definition, keyed by its `SourceCodeInfo` path.
struct Lines(BTreeMap<Vec<i32>, u32>);

impl Lines {
    fn from_descriptor(file: &FileDescriptorProto) -> Self {
        let mut lines = BTreeMap::new();
        if let Some(info) = &file.source_code_info {
            for location in &info.location {
                // span is [start_line, start_col, end_col] or
                // [start_line, start_col, end_line, end_col], zero-based.
                if let Some(start_line) = location.span.first() {
                    let line = u32::try_from(*start_line).unwrap_or(0).saturating_add(1);
                    lines.entry(location.path.clone()).or_insert(line);
                }
            }
        }
        Lines(lines)
    }

    fn get(&self, path: &[i32], fallback: u32) -> u32 {
        self.0.get(path).copied().unwrap_or(fallback)
    }
}

impl Schema {
    fn from_descriptor(file: &FileDescriptorProto) -> Self {
        let lines = Lines::from_descriptor(file);
        let syntax = file
            .syntax
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "proto2".to_owned());
        let syntax_line = lines.get(&[FILE_SYNTAX], 1);
        let package = file.package.clone().filter(|p| !p.is_empty());
        let package_line = lines.get(&[FILE_PACKAGE], 1);

        let mut schema = Schema {
            syntax,
            syntax_line,
            package,
            package_line,
            messages: BTreeMap::new(),
            enums: BTreeMap::new(),
            services: BTreeMap::new(),
        };

        for (index, message) in file.message_type.iter().enumerate() {
            let path = vec![FILE_MESSAGE_TYPE, index as i32];
            schema.add_message(message, &path, "", package_line, &lines);
        }
        for (index, enum_type) in file.enum_type.iter().enumerate() {
            let path = vec![FILE_ENUM_TYPE, index as i32];
            schema.add_enum(enum_type, &path, "", package_line, &lines);
        }
        for (index, service) in file.service.iter().enumerate() {
            let path = vec![FILE_SERVICE, index as i32];
            let service_line = lines.get(&path, package_line);
            let mut methods = BTreeMap::new();
            for (method_index, method) in service.method.iter().enumerate() {
                let mut method_path = path.clone();
                method_path.extend([SERVICE_METHOD, method_index as i32]);
                methods.insert(
                    method.name.clone().unwrap_or_default(),
                    Method {
                        line: lines.get(&method_path, service_line),
                        input: method.input_type.clone().unwrap_or_default(),
                        output: method.output_type.clone().unwrap_or_default(),
                        client_streaming: method.client_streaming.unwrap_or(false),
                        server_streaming: method.server_streaming.unwrap_or(false),
                    },
                );
            }
            schema.services.insert(
                service.name.clone().unwrap_or_default(),
                Service {
                    line: service_line,
                    methods,
                },
            );
        }
        schema
    }

    fn add_message(&mut self, message: &DescriptorProto, path: &[i32], prefix: &str, parent_line: u32, lines: &Lines) {
        let name = qualify(prefix, message.name.as_deref().unwrap_or_default());
        let line = lines.get(path, parent_line);
        let proto3 = self.syntax == "proto3";
        let oneof_names: Vec<&str> = message
            .oneof_decl
            .iter()
            .map(|oneof| oneof.name.as_deref().unwrap_or_default())
            .collect();

        let mut fields = BTreeMap::new();
        for (index, field) in message.field.iter().enumerate() {
            let mut field_path = path.to_vec();
            field_path.extend([MESSAGE_FIELD, index as i32]);
            let number = field.number.unwrap_or(0);
            fields.insert(
                number,
                convert_field(field, number, lines.get(&field_path, line), proto3, &oneof_names),
            );
        }

        // DescriptorProto reserved ranges are end-exclusive.
        let reserved_numbers = message
            .reserved_range
            .iter()
            .map(|range| (range.start.unwrap_or(0), range.end.unwrap_or(0).saturating_sub(1)))
            .collect();

        self.messages.insert(
            name.clone(),
            Message {
                line,
                map_entry: message.options.as_ref().and_then(|o| o.map_entry).unwrap_or(false),
                fields,
                reserved_numbers,
                reserved_names: message.reserved_name.iter().cloned().collect(),
            },
        );

        for (index, nested) in message.nested_type.iter().enumerate() {
            let mut nested_path = path.to_vec();
            nested_path.extend([MESSAGE_NESTED_TYPE, index as i32]);
            self.add_message(nested, &nested_path, &name, line, lines);
        }
        for (index, nested) in message.enum_type.iter().enumerate() {
            let mut nested_path = path.to_vec();
            nested_path.extend([MESSAGE_ENUM_TYPE, index as i32]);
            self.add_enum(nested, &nested_path, &name, line, lines);
        }
    }

    fn add_enum(
        &mut self,
        enum_type: &EnumDescriptorProto,
        path: &[i32],
        prefix: &str,
        parent_line: u32,
        lines: &Lines,
    ) {
        let name = qualify(prefix, enum_type.name.as_deref().unwrap_or_default());
        let line = lines.get(path, parent_line);
        let values = enum_type
            .value
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let mut value_path = path.to_vec();
                value_path.extend([ENUM_VALUE, index as i32]);
                EnumValue {
                    name: value.name.clone().unwrap_or_default(),
                    number: value.number.unwrap_or(0),
                    line: lines.get(&value_path, line),
                }
            })
            .collect();
        // EnumDescriptorProto reserved ranges are end-inclusive.
        let reserved_numbers = enum_type
            .reserved_range
            .iter()
            .map(|range| (range.start.unwrap_or(0), range.end.unwrap_or(0)))
            .collect();
        self.enums.insert(
            name,
            Enum {
                line,
                values,
                reserved_numbers,
                reserved_names: enum_type.reserved_name.iter().cloned().collect(),
            },
        );
    }
}

fn qualify(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}

fn convert_field(field: &FieldDescriptorProto, number: i32, line: u32, proto3: bool, oneof_names: &[&str]) -> Field {
    let label = field.label.and_then(|l| Label::try_from(l).ok());
    let proto3_optional = field.proto3_optional.unwrap_or(false);
    let cardinality = match label {
        Some(Label::Repeated) => Cardinality::Repeated,
        Some(Label::Required) => Cardinality::Required,
        _ if proto3_optional || !proto3 => Cardinality::Optional,
        _ => Cardinality::Implicit,
    };
    let type_text = match &field.type_name {
        Some(type_name) if !type_name.is_empty() => type_name.clone(),
        _ => field
            .r#type
            .and_then(|t| Type::try_from(t).ok())
            .map(scalar_name)
            .unwrap_or_else(|| "<unknown>".to_owned()),
    };
    // A proto3 `optional` field sits in a synthetic oneof the author never
    // wrote; it is not oneof membership.
    let oneof = if proto3_optional {
        None
    } else {
        field
            .oneof_index
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| oneof_names.get(index))
            .map(|name| (*name).to_owned())
    };
    Field {
        name: field.name.clone().unwrap_or_default(),
        number,
        line,
        type_text,
        named_type: field.type_name.as_ref().is_some_and(|n| !n.is_empty()),
        cardinality,
        oneof,
        json_name: field.json_name.clone(),
    }
}

fn scalar_name(scalar: Type) -> String {
    scalar.as_str_name().trim_start_matches("TYPE_").to_ascii_lowercase()
}
