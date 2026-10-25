//! Resolve names using protobuf's innermost-to-outermost declaration scopes.

use std::collections::BTreeSet;

use crate::schema::{ParseFailure, Schema};

pub fn resolve(schema: &mut Schema) -> Result<(), ParseFailure> {
    let package = schema.package.as_deref().unwrap_or("");
    let symbols: BTreeSet<String> = schema
        .messages
        .keys()
        .chain(schema.enums.keys())
        .map(|name| qualify(package, name))
        .collect();
    // Include namespace prefixes: once a leading component matches a scope,
    // protobuf does not fall back to an outer scope if its suffix is missing.
    let mut namespaces = symbols.clone();
    for symbol in &symbols {
        let mut prefix = symbol.as_str();
        while let Some((parent, _)) = prefix.rsplit_once('.') {
            namespaces.insert(parent.to_owned());
            prefix = parent;
        }
    }
    for (name, message) in &mut schema.messages {
        let scope = qualify(package, name);
        for field in message.fields.values_mut() {
            if field.named_type {
                field.type_text = resolve_name(&field.type_text, &scope, &symbols, &namespaces, field.line)?;
            }
        }
    }
    for (name, service) in &mut schema.services {
        let scope = qualify(package, name);
        for method in service.methods.values_mut() {
            method.input = resolve_name(&method.input, &scope, &symbols, &namespaces, method.line)?;
            method.output = resolve_name(&method.output, &scope, &symbols, &namespaces, method.line)?;
        }
    }
    Ok(())
}

fn qualify(scope: &str, name: &str) -> String {
    if scope.is_empty() {
        name.to_owned()
    } else {
        format!("{scope}.{name}")
    }
}

fn resolve_name(
    name: &str,
    scope: &str,
    symbols: &BTreeSet<String>,
    namespaces: &BTreeSet<String>,
    line: u32,
) -> Result<String, ParseFailure> {
    if let Some(absolute) = name.strip_prefix('.') {
        if symbols.contains(absolute) {
            return Ok(format!(".{absolute}"));
        }
    } else {
        let first = name.split('.').next().unwrap_or(name);
        let mut scope = scope;
        loop {
            if namespaces.contains(&qualify(scope, first)) {
                let candidate = qualify(scope, name);
                if symbols.contains(&candidate) {
                    return Ok(format!(".{candidate}"));
                }
                break;
            }
            if scope.is_empty() {
                break;
            }
            scope = scope.rsplit_once('.').map_or("", |(parent, _)| parent);
        }
    }
    Err(ParseFailure {
        message: format!(
            "cannot resolve type `{name}` in scope `{scope}` to a same-file declaration; compatibility cannot be established"
        ),
        line: Some(line),
    })
}
