//! Deterministic validation of the footprint declared by a design author.

#[derive(Debug, PartialEq, Eq)]
pub struct TaskFootprint {
    pub deliverables: Vec<String>,
    pub likely_changed_files: Vec<String>,
    pub files: usize,
    pub changed_lines: usize,
}

impl TaskFootprint {
    pub fn brief(&self) -> String {
        let files = self.likely_changed_files.join("\n- ");
        format!(
            "Likely changed files:\n- {files}\n\nEstimated size: ~{} files / ~{} changed lines",
            self.files, self.changed_lines
        )
    }
}

/// Accept the documented field labels, including Markdown bold/bullet styling.
/// Fenced examples cannot satisfy a required field. Reject duplicate fields so
/// a second, larger estimate cannot be hidden behind the first.
pub fn parse_task_footprint(body: &str) -> Result<TaskFootprint, String> {
    let mut deliverables = Vec::new();
    let mut files = Vec::new();
    let mut estimate = None;
    let mut seen = std::collections::HashSet::new();
    let mut active = "";
    let mut in_fence = false;
    let mut deliverable_indent = None;
    for line in body.lines() {
        let indent = line.len() - line.trim_start().len();
        let line = line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || line.is_empty() {
            continue;
        }
        let plain = line.trim_start_matches("- ");
        if !line.starts_with("- `")
            && !line.starts_with("* `")
            && !line.starts_with(|c: char| c.is_ascii_digit())
            && let Some((label, value)) = plain.split_once(':')
            && !label.contains(['/', '.', '`'])
        {
            let label = label.trim().trim_matches('*').to_ascii_lowercase();
            let value = value.trim_start_matches("**").trim();
            active = match label.as_str() {
                "major deliverables" | "deliverables" => "deliverables",
                "likely changed files" | "likely-changed files" => "files",
                "estimated size" => "estimate",
                "scope" | "effort" | "effort hint" | "dependencies" | "depends on" => "",
                _ => continue,
            };
            if !active.is_empty() {
                if !seen.insert(active) {
                    return Err(format!("duplicate {label} field"));
                }
                match active {
                    "estimate" => estimate = Some(parse_estimate(value.trim_end_matches("**"))?),
                    "files" if !value.trim().is_empty() => files.push(value.trim().to_owned()),
                    _ => {}
                }
                continue;
            }
        }
        match active {
            "deliverables" => {
                let marker_end = line.find(['.', ')']);
                if let Some(end) = marker_end
                    && line[..end].chars().all(|c| c.is_ascii_digit())
                    && end > 0
                    && line[end + 1..].starts_with(' ')
                    && !line[end + 1..].trim().is_empty()
                {
                    let top = *deliverable_indent.get_or_insert(indent);
                    if indent <= top {
                        if indent < top {
                            deliverables.clear();
                            deliverable_indent = Some(indent);
                        }
                        deliverables.push(line[end + 1..].trim().to_owned());
                    }
                }
            }
            "files" => {
                if let Some(path) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
                    files.push(path.to_owned());
                }
            }
            _ => {}
        }
    }
    if deliverables.is_empty() {
        return Err("missing numbered Major deliverables list".to_owned());
    }
    if deliverables.len() > 3 {
        return Err(format!("{} major deliverables exceeds limit of 3", deliverables.len()));
    }
    if files.is_empty() {
        return Err("missing Likely changed files list".to_owned());
    }
    let mut listed_files = 0usize;
    for item in &files {
        let (path, rest) = if let Some(quoted) = item.strip_prefix('`') {
            quoted.split_once('`').ok_or("unclosed backtick in file path")?
        } else {
            item.split_once(' ').unwrap_or((item, ""))
        };
        if path.is_empty()
            || path.chars().any(char::is_whitespace)
            || (!item.starts_with('`') && !path.contains('/') && !path.contains('.'))
        {
            return Err(format!("expected explicit file path or glob, got {item}"));
        }
        let count = if path.contains(['*', '?', '[']) {
            let count = rest.trim().trim_start_matches('(').split(')').next().unwrap_or("");
            parse_count(count.strip_suffix(" files").ok_or("file glob needs (~N files)")?)?
        } else {
            1
        };
        listed_files = listed_files.checked_add(count).ok_or("file count overflow")?;
    }
    let (file_count, changed_lines) = estimate.ok_or("missing Estimated size: ~N files / ~M changed lines")?;
    if file_count > 25 || changed_lines > 1_500 {
        return Err(format!(
            "estimated {file_count} files / {changed_lines} changed lines exceeds limit of 25 files / 1,500 changed lines"
        ));
    }
    if listed_files > file_count {
        return Err(format!(
            "file list covers ~{listed_files} files but estimate states ~{file_count}"
        ));
    }
    Ok(TaskFootprint {
        deliverables,
        likely_changed_files: files,
        files: file_count,
        changed_lines,
    })
}

fn parse_estimate(value: &str) -> Result<(usize, usize), String> {
    let (files, lines) = value
        .trim()
        .split_once(" files / ")
        .ok_or("expected Estimated size: ~N files / ~M changed lines")?;
    let lines = lines
        .strip_suffix(" changed lines")
        .ok_or("estimate must include changed lines")?;
    Ok((parse_count(files)?, parse_count(lines)?))
}

fn parse_count(value: &str) -> Result<usize, String> {
    let value = value.trim().trim_start_matches('~').replace(',', "");
    let count = value
        .parse::<usize>()
        .map_err(|_| "estimate counts must be positive integers")?;
    if count == 0 {
        return Err("estimate counts must be positive integers".to_owned());
    }
    Ok(count)
}

#[cfg(test)]
mod tests;
