use crate::bazel_support::{logical_bazelrc_lines, shell_split};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

use crate::input::SourceTree;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BazelrcEntryKind {
    Flag,
    Import,
    TryImport,
}

#[derive(Debug, Clone)]
pub(crate) struct BazelrcEntry {
    pub(crate) source_path: PathBuf,
    pub(crate) kind: BazelrcEntryKind,
    pub(crate) line: u32,
    pub(crate) column: u32,
    pub(crate) command: Option<String>,
    pub(crate) config_name: Option<String>,
    pub(crate) flag: Option<String>,
    pub(crate) value: Option<String>,
    pub(crate) import_path: Option<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct ParsedBazelrcClosure {
    pub(crate) entries: Vec<BazelrcEntry>,
}

pub(crate) fn is_bazelrc_root_candidate(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name == ".bazelrc" || name.ends_with(".bazelrc")
}

pub(crate) fn parse_bazelrc_closure(path: &Path, tree: &dyn SourceTree) -> Result<ParsedBazelrcClosure> {
    let mut entries = Vec::new();
    let mut visited = HashSet::new();
    let mut pending = vec![path.to_path_buf()];

    while let Some(next_path) = pending.pop() {
        if !visited.insert(next_path.clone()) {
            continue;
        }
        let contents = tree
            .read_file(&next_path)
            .with_context(|| format!("failed to read bazelrc file {}", next_path.display()))?;
        let contents = String::from_utf8(contents)
            .with_context(|| format!("bazelrc file is not utf-8: {}", next_path.display()))?;

        let parsed_entries = parse_bazelrc_file(&next_path, &contents)?;
        for entry in &parsed_entries {
            if matches!(entry.kind, BazelrcEntryKind::Import | BazelrcEntryKind::TryImport)
                && let Some(import_path) = &entry.import_path
                && tree.exists(import_path)
            {
                pending.push(import_path.clone());
            }
        }
        entries.extend(parsed_entries);
    }

    Ok(ParsedBazelrcClosure { entries })
}

fn parse_bazelrc_file(path: &Path, contents: &str) -> Result<Vec<BazelrcEntry>> {
    let logical_lines = logical_bazelrc_lines(contents)?;
    let mut entries = Vec::new();
    for (line_number, line) in logical_lines {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let tokens = shell_split(trimmed)
            .with_context(|| format!("failed to tokenize bazelrc line {} in {}", line_number, path.display()))?;
        if tokens.is_empty() {
            continue;
        }

        match tokens[0].as_str() {
            "import" | "try-import" => {
                if tokens.len() < 2 {
                    continue;
                }
                let import_path = resolve_import_path(path, &tokens[1]);
                entries.push(BazelrcEntry {
                    source_path: path.to_path_buf(),
                    kind: if tokens[0] == "import" {
                        BazelrcEntryKind::Import
                    } else {
                        BazelrcEntryKind::TryImport
                    },
                    line: line_number,
                    column: 1,
                    command: None,
                    config_name: None,
                    flag: None,
                    value: None,
                    import_path,
                });
            }
            command_token => {
                let Some((command, config_name)) = parse_command_token(command_token) else {
                    continue;
                };
                entries.extend(extract_flag_entries(
                    path,
                    line_number,
                    trimmed,
                    &tokens[1..],
                    command,
                    config_name,
                ));
            }
        }
    }

    Ok(entries)
}

fn resolve_import_path(current_path: &Path, raw_import: &str) -> Option<PathBuf> {
    if let Some(rest) = raw_import.strip_prefix("%workspace%/") {
        return Some(PathBuf::from(rest));
    }

    let candidate = Path::new(raw_import);
    if candidate.is_absolute() {
        return None;
    }

    let parent = current_path.parent().unwrap_or(Path::new(""));
    Some(normalize_relative_path(&parent.join(candidate)))
}

fn normalize_relative_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            _ => {}
        }
    }
    normalized
}

fn parse_command_token(token: &str) -> Option<(String, Option<String>)> {
    let (command, config_name) = match token.split_once(':') {
        Some((command, config_name)) => (command, Some(config_name)),
        None => (token, None),
    };

    let command = command.trim().to_ascii_lowercase();
    if command.is_empty() {
        return None;
    }
    let config_name = config_name.and_then(|name| {
        let trimmed = name.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    });
    Some((command, config_name))
}

fn extract_flag_entries(
    path: &Path,
    line_number: u32,
    line: &str,
    tokens: &[String],
    command: String,
    config_name: Option<String>,
) -> Vec<BazelrcEntry> {
    let mut entries = Vec::new();
    let mut index = 0usize;

    while index < tokens.len() {
        let token = &tokens[index];
        if !token.starts_with('-') {
            index += 1;
            continue;
        }

        let (flag_name, inline_value) = match token.split_once('=') {
            Some((name, value)) => (name.trim_start_matches('-'), Some(value.to_owned())),
            None => (token.trim_start_matches('-'), None),
        };
        if flag_name.is_empty() {
            index += 1;
            continue;
        }

        let mut value = inline_value;
        if value.is_none()
            && let Some(next) = tokens.get(index + 1)
            && !next.starts_with('-')
        {
            value = Some(next.clone());
            index += 1;
        }

        let column = line.find(token).map(|col| (col + 1) as u32).unwrap_or(1);
        entries.push(BazelrcEntry {
            source_path: path.to_path_buf(),
            kind: BazelrcEntryKind::Flag,
            line: line_number,
            column,
            command: Some(command.clone()),
            config_name: config_name.clone(),
            flag: Some(flag_name.to_owned()),
            value,
            import_path: None,
        });
        index += 1;
    }

    entries
}
