//! Markdown destinations for the corpus's human navigation checklist.

use std::collections::HashMap;

pub(crate) fn definitions(markdown: &str) -> HashMap<String, String> {
    let pattern = regex::Regex::new(r"(?m)^ {0,3}\[([^\]]+)\]:\s*(.+)$").expect("static regex");
    pattern
        .captures_iter(markdown)
        .filter_map(|capture| destination(&capture[2]).map(|target| (capture[1].to_lowercase(), target)))
        .collect()
}

fn destination(text: &str) -> Option<String> {
    let text = text.trim_start();
    if let Some(angled) = text.strip_prefix('<') {
        return angled.split_once('>').map(|(target, _)| target.to_owned());
    }
    let mut depth = 0;
    let mut escaped = false;
    let mut end = text.len();
    for (index, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            ')' => {
                end = index;
                break;
            }
            ch if ch.is_whitespace() => {
                end = index;
                break;
            }
            _ => {}
        }
    }
    (end > 0).then(|| text[..end].to_owned())
}

pub(crate) fn targets(line: &str, definitions: &HashMap<String, String>) -> Vec<String> {
    let mut targets = Vec::new();
    let labels = regex::Regex::new(r"\[([^\]]*)\](?:\[([^\]]*)\])?").expect("static regex");
    for capture in labels.captures_iter(line) {
        let matched = capture.get(0).expect("whole match");
        let rest = &line[matched.end()..];
        if let Some(inline) = rest.strip_prefix('(') {
            if let Some(target) = destination(inline) {
                targets.push(target);
            }
        } else {
            let label = capture
                .get(2)
                .filter(|value| !value.as_str().is_empty())
                .map_or(&capture[1], |value| value.as_str());
            if let Some(target) = definitions.get(&label.to_lowercase()) {
                targets.push(target.clone());
            }
        }
    }
    let autolinks = regex::Regex::new(r"<([a-zA-Z][a-zA-Z0-9+.-]*:[^<>]+)>").expect("static regex");
    targets.extend(autolinks.captures_iter(line).map(|capture| capture[1].to_owned()));
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_destinations_across_schemes_and_reference_styles() {
        let definitions = definitions("[code]: /local/file.rs:19\n");
        assert_eq!(
            targets(
                "[code] [label][code] [link](https://example.com/a(b)) [file](</path with spaces>) <mailto:a@example.com>",
                &definitions
            ),
            vec![
                "/local/file.rs:19",
                "/local/file.rs:19",
                "https://example.com/a(b)",
                "/path with spaces",
                "mailto:a@example.com"
            ]
        );
    }
}
