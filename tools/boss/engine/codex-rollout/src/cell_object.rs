//! Decode static cell argument objects without evaluating JavaScript.
//! Accept JSON plus bare property names, single-quoted strings, trailing commas,
//! and JavaScript string escapes (`\u0041`, `\u{42}`, `\x43`). Dynamic
//! expressions, templates, spreads and comments remain unsupported.

use std::iter::Peekable;

use serde_json::Value;

pub(super) fn parse_literal_object(source: &str) -> Option<Value> {
    let mut chars = source.chars().peekable();
    let mut json = String::new();
    while let Some(ch) = chars.next() {
        match ch {
            '"' | '\'' => {
                let value = literal_string(&mut chars, ch)?;
                json.push_str(&serde_json::to_string(&value).ok()?);
            }
            ch if ch.is_ascii_alphabetic() || matches!(ch, '_' | '$') => {
                let mut token = String::from(ch);
                while let Some(&next) = chars.peek() {
                    if !(next.is_ascii_alphanumeric() || matches!(next, '_' | '$')) {
                        break;
                    }
                    token.push(chars.next()?);
                }
                // Only property names are quoted. An identifier used as a
                // value must still fail JSON parsing (except true/false/null).
                if chars.clone().find(|ch| !ch.is_whitespace()) == Some(':') {
                    json.push_str(&serde_json::to_string(&token).ok()?);
                } else {
                    json.push_str(&token);
                }
            }
            ',' if matches!(chars.clone().find(|ch| !ch.is_whitespace()), Some('}' | ']')) => {
                // An array hole is not an ordinary trailing comma.
                if json.trim_end().ends_with(['[', '{', ',']) {
                    return None;
                }
            }
            ch => json.push(ch),
        }
    }
    serde_json::from_str(&json).ok()
}

fn literal_string(chars: &mut Peekable<impl Iterator<Item = char>>, quote: char) -> Option<String> {
    let mut out = String::new();
    loop {
        let ch = chars.next()?;
        if ch == quote {
            return Some(out);
        }
        if ch == '\n' || ch == '\r' {
            return None;
        }
        if ch == '\\' {
            decode_js_escape(chars, &mut out)?;
            continue;
        }
        out.push(ch);
    }
}

/// Decode one JS string escape whose backslash has already been consumed.
/// Unrecognised escapes return `None` so the caller can fall back to the
/// whole script instead of copying `\u0041` as `u0041`.
fn decode_js_escape(chars: &mut Peekable<impl Iterator<Item = char>>, out: &mut String) -> Option<()> {
    let ch = chars.next()?;
    match ch {
        'n' => out.push('\n'),
        't' => out.push('\t'),
        'r' => out.push('\r'),
        'b' => out.push('\u{0008}'),
        'f' => out.push('\u{000c}'),
        'v' => out.push('\u{000b}'),
        '0' => out.push('\0'),
        '"' | '\'' | '\\' | '/' => out.push(ch),
        '\n' => {}
        '\r' => {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
        }
        'x' => {
            let hex: String = [chars.next()?, chars.next()?].into_iter().collect();
            if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
        }
        'u' => decode_js_unicode_escape(chars, out)?,
        _ => return None,
    }
    Some(())
}

fn decode_js_unicode_escape(chars: &mut Peekable<impl Iterator<Item = char>>, out: &mut String) -> Option<()> {
    if chars.peek() == Some(&'{') {
        chars.next();
        let mut hex = String::new();
        loop {
            let next = chars.next()?;
            if next == '}' {
                break;
            }
            hex.push(next);
        }
        if hex.is_empty() || hex.len() > 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
        return Some(());
    }
    let hex: String = [chars.next()?, chars.next()?, chars.next()?, chars.next()?]
        .into_iter()
        .collect();
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_literal_keys_and_strings_without_changing_shell_quotes() {
        let parsed =
            parse_literal_object(r#"{cmd: '\"$BOSS_BIN\" propose done --summary \'shipped\'', yield_time_ms: 1000,}"#)
                .unwrap();
        assert_eq!(parsed["cmd"], "\"$BOSS_BIN\" propose done --summary 'shipped'");
        assert_eq!(parsed["yield_time_ms"], 1000);
    }

    #[test]
    fn decodes_javascript_unicode_and_hex_string_escapes() {
        let parsed = parse_literal_object(r#"{cmd:"echo \u0041\u{42}\x43"}"#).unwrap();
        assert_eq!(parsed["cmd"], "echo ABC");
        let parsed = parse_literal_object(r#"{cmd:'\x24BOSS_BIN propose done'}"#).unwrap();
        assert_eq!(parsed["cmd"], "$BOSS_BIN propose done");
    }

    #[test]
    fn rejects_expressions_instead_of_guessing_the_command() {
        for source in [
            "{cmd: command}",
            "{cmd: 'boss ' + suffix}",
            "{cmd: `boss ${verb}`}",
            "{...options, cmd: 'boss propose done'}",
            "{cmd: '\\q'}",
            "{cmd: 'unfinished}",
            "{cmd: ['boss',,]}",
            "{cmd: [,]}",
        ] {
            assert!(parse_literal_object(source).is_none(), "{source}");
        }
    }
}
