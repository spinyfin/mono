//! Decode static cell argument objects without evaluating JavaScript.
//! Accept JSON plus bare property names, single-quoted strings and trailing commas. Dynamic
//! expressions, templates, spreads and comments remain unsupported.

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

fn literal_string(chars: &mut impl Iterator<Item = char>, quote: char) -> Option<String> {
    let mut json = String::from("\"");
    while let Some(ch) = chars.next() {
        match ch {
            ch if ch == quote => {
                json.push('"');
                return serde_json::from_str(&json).ok();
            }
            '\\' => {
                let escaped = chars.next()?;
                if quote == '\'' && escaped == '\'' {
                    json.push('\'');
                } else {
                    // JSON validates escapes; unsupported JavaScript escapes
                    // fail closed instead of silently changing the command.
                    json.push('\\');
                    json.push(escaped);
                }
            }
            '"' => json.push_str("\\\""),
            ch => json.push(ch),
        }
    }
    None
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
    fn rejects_expressions_instead_of_guessing_the_command() {
        for source in [
            "{cmd: command}",
            "{cmd: 'boss ' + suffix}",
            "{cmd: `boss ${verb}`}",
            "{...options, cmd: 'boss propose done'}",
            "{cmd: '\\x24BOSS_BIN propose done'}",
            "{cmd: 'unfinished}",
            "{cmd: ['boss',,]}",
            "{cmd: [,]}",
        ] {
            assert!(parse_literal_object(source).is_none(), "{source}");
        }
    }
}
