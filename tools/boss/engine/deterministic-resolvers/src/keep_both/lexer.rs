//! A small lexical scanner that tells code apart from comments and
//! string literals, so bracket counting and item detection are not
//! fooled by a `{` inside a string or a `;` inside a comment.
//!
//! It is deliberately not a parser. It classifies every character of a
//! text fragment and reports a fragment that ends inside a string or a
//! block comment as not self-contained.

/// Which lexical conventions a file follows, chosen from its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Syntax {
    /// Rust: `//` and nested `/* */` comments, `"…"`, raw and byte
    /// strings, char literals, and lifetimes written with a lone `'`.
    Rust,
    /// `//` and `/* */` comments with `"…"` strings (Swift, protobuf,
    /// JavaScript and similar).
    Slash,
    /// `#` comments with `"…"` strings (Starlark, TOML, YAML, shell,
    /// Python and similar).
    Hash,
}

impl Syntax {
    pub(crate) fn for_path(path: &str) -> Syntax {
        let name = path.rsplit('/').next().unwrap_or(path);
        let extension = name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
        match extension {
            "rs" => Syntax::Rust,
            "swift" | "proto" | "js" | "jsx" | "ts" | "tsx" | "c" | "h" | "cc" | "cpp" | "java" | "kt" | "go" => {
                Syntax::Slash
            }
            _ => Syntax::Hash,
        }
    }
}

/// What one character of a scanned fragment is part of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CharClass {
    Code,
    Comment,
    /// A string or char literal, quotes included.
    Literal,
}

/// A text fragment with every character classified.
pub(crate) struct Scanned {
    chars: Vec<char>,
    classes: Vec<CharClass>,
}

impl Scanned {
    /// The fragment with comments and literal contents blanked out, line
    /// structure preserved: what is left is only the code that brackets,
    /// keywords and terminators can be read from.
    pub(crate) fn code(&self) -> String {
        self.project(|class| class == CharClass::Code)
    }

    /// The fragment with comments blanked out but literals kept, for
    /// comparing two definitions without their commentary.
    pub(crate) fn without_comments(&self) -> String {
        self.project(|class| class != CharClass::Comment)
    }

    fn project(&self, keep: impl Fn(CharClass) -> bool) -> String {
        self.chars
            .iter()
            .zip(&self.classes)
            .map(|(&ch, &class)| if ch == '\n' || keep(class) { ch } else { ' ' })
            .collect()
    }
}

/// Classifies every character of `text`. Fails when the fragment ends
/// inside a string literal or a block comment: such a fragment is a slice
/// out of the middle of something and cannot be reasoned about alone.
pub(crate) fn scan(text: &str, syntax: Syntax) -> Result<Scanned, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut classes = vec![CharClass::Code; chars.len()];
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        let next = chars.get(i + 1).copied();
        let end = match (syntax, ch) {
            (Syntax::Rust | Syntax::Slash, '/') if next == Some('/') => line_end(&chars, i),
            (Syntax::Rust | Syntax::Slash, '/') if next == Some('*') => block_comment_end(&chars, i)?,
            (Syntax::Hash, '#') => line_end(&chars, i),
            (Syntax::Rust, 'r') if raw_string_hashes(&chars, i).is_some() => {
                let hashes = raw_string_hashes(&chars, i).unwrap_or(0);
                let end = raw_string_end(&chars, i + 1 + hashes + 1, hashes)?;
                mark(&mut classes, i, end, CharClass::Literal);
                i = end;
                continue;
            }
            (Syntax::Rust, '\'') => match char_literal_end(&chars, i) {
                Some(end) => {
                    mark(&mut classes, i, end, CharClass::Literal);
                    i = end;
                    continue;
                }
                // A lifetime or loop label: the quote is ordinary code.
                None => {
                    i += 1;
                    continue;
                }
            },
            (_, '"') => {
                let end = string_end(&chars, i, syntax)?;
                mark(&mut classes, i, end, CharClass::Literal);
                i = end;
                continue;
            }
            (Syntax::Slash | Syntax::Hash, '\'') => match same_line_quote_end(&chars, i) {
                Some(end) => {
                    mark(&mut classes, i, end, CharClass::Literal);
                    i = end;
                    continue;
                }
                None => {
                    i += 1;
                    continue;
                }
            },
            _ => {
                i += 1;
                continue;
            }
        };
        mark(&mut classes, i, end, CharClass::Comment);
        i = end;
    }
    Ok(Scanned { chars, classes })
}

fn mark(classes: &mut [CharClass], start: usize, end: usize, class: CharClass) {
    for slot in &mut classes[start..end] {
        *slot = class;
    }
}

fn line_end(chars: &[char], start: usize) -> usize {
    chars[start..]
        .iter()
        .position(|&c| c == '\n')
        .map_or(chars.len(), |offset| start + offset)
}

fn block_comment_end(chars: &[char], start: usize) -> Result<usize, String> {
    let mut depth = 0usize;
    let mut i = start;
    while i < chars.len() {
        if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
            depth += 1;
            i += 2;
        } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return Ok(i);
            }
        } else {
            i += 1;
        }
    }
    Err("ends inside a block comment".to_owned())
}

fn is_ident_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// When `chars[at]` is the `r` that opens a raw string (`r"…"`, `r#"…"#`,
/// also behind a `b` or `c` prefix), the number of `#` it uses.
fn raw_string_hashes(chars: &[char], at: usize) -> Option<usize> {
    let before = at.checked_sub(1).map(|i| chars[i]);
    let standalone = match before {
        None => true,
        Some('b' | 'c') => at < 2 || !is_ident_char(chars[at - 2]),
        Some(prev) => !is_ident_char(prev),
    };
    if !standalone {
        return None;
    }
    let hashes = chars[at + 1..].iter().take_while(|&&c| c == '#').count();
    (chars.get(at + 1 + hashes) == Some(&'"')).then_some(hashes)
}

fn raw_string_end(chars: &[char], body_start: usize, hashes: usize) -> Result<usize, String> {
    let mut i = body_start;
    while i < chars.len() {
        if chars[i] == '"' && chars[i + 1..].iter().take(hashes).filter(|&&c| c == '#').count() == hashes {
            return Ok(i + 1 + hashes);
        }
        i += 1;
    }
    Err("ends inside a raw string literal".to_owned())
}

fn string_end(chars: &[char], start: usize, syntax: Syntax) -> Result<usize, String> {
    // Swift, Python, Starlark and TOML all write multi-line strings with
    // a tripled quote.
    let triple = syntax != Syntax::Rust && chars[start..].starts_with(&['"', '"', '"']);
    let mut i = start + if triple { 3 } else { 1 };
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '"' if !triple => return Ok(i + 1),
            '"' if chars[i..].starts_with(&['"', '"', '"']) => return Ok(i + 3),
            _ => i += 1,
        }
    }
    Err("ends inside a string literal".to_owned())
}

/// The end of the char literal opening at `start`, or `None` when the
/// quote starts a lifetime (`'a`) or a loop label (`'outer:`).
fn char_literal_end(chars: &[char], start: usize) -> Option<usize> {
    match chars.get(start + 1)? {
        '\\' => {
            let close = chars[start + 2..].iter().take(12).position(|&c| c == '\'')?;
            // `'\''`: the first quote found is the escaped one.
            let close = if close == 0 { 1 } else { close };
            (chars.get(start + 2 + close) == Some(&'\'')).then_some(start + 3 + close)
        }
        '\'' => None,
        _ => (chars.get(start + 2) == Some(&'\'')).then_some(start + 3),
    }
}

fn same_line_quote_end(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < chars.len() && chars[i] != '\n' {
        match chars[i] {
            '\\' => i += 2,
            '\'' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Checks that every bracket in `code` (already stripped of comments and
/// literals) is closed by the matching bracket, and that no bracket is
/// closed before it was opened.
pub(crate) fn check_balanced(code: &str) -> Result<(), String> {
    let mut stack: Vec<char> = Vec::new();
    for ch in code.chars() {
        match ch {
            '(' | '[' | '{' => stack.push(ch),
            ')' | ']' | '}' => {
                let open = stack.pop().ok_or_else(|| format!("closes a `{ch}` it did not open"))?;
                if closer(open) != ch {
                    return Err(format!("closes `{open}` with `{ch}`"));
                }
            }
            _ => {}
        }
    }
    match stack.last() {
        Some(open) => Err(format!("leaves a `{open}` open")),
        None => Ok(()),
    }
}

fn closer(open: char) -> char {
    match open {
        '(' => ')',
        '[' => ']',
        _ => '}',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(text: &str) -> String {
        scan(text, Syntax::Rust).unwrap().code()
    }

    #[test]
    fn blanks_comments_and_string_contents_but_keeps_line_structure() {
        let text = "let a = \"{ not code }\"; // } nor this\nlet b = 1; /* { */";
        let masked = code(text);
        assert_eq!(masked.lines().count(), 2);
        assert_eq!(masked.len(), text.len());
        assert!(!masked.contains('{'), "{masked:?}");
        assert!(masked.starts_with("let a = "));
        assert!(masked.contains("let b = 1;"));
    }

    #[test]
    fn handles_raw_strings_char_literals_and_lifetimes() {
        assert!(!code("let s = r#\"a \" { b\"#;").contains('{'));
        assert!(!code("let s = br##\"{\"##;").contains('{'));
        assert!(!code("let c = '{';").contains('{'));
        assert!(!code("let c = '\\'';  let d = '{';").contains('{'));
        // A lifetime's quote must not swallow the code after it.
        assert!(code("fn f<'a>(x: &'a str) -> &'a str { x }").contains('{'));
        // `r` at the end of an identifier does not open a raw string.
        assert!(code("let var = \"x\"; let y = { 1 };").contains('{'));
    }

    #[test]
    fn nested_block_comments_end_where_the_outermost_one_closes() {
        assert_eq!(code("/* a /* b */ { */ fn x() {}").trim(), "fn x() {}");
    }

    #[test]
    fn a_fragment_ending_inside_a_string_or_comment_is_rejected() {
        assert!(scan("let s = \"unterminated", Syntax::Rust).is_err());
        assert!(scan("/* never closed", Syntax::Rust).is_err());
        assert!(scan("let s = r#\"open", Syntax::Rust).is_err());
    }

    #[test]
    fn without_comments_keeps_literals() {
        let scanned = scan("const A: &str = \"x\"; // why", Syntax::Rust).unwrap();
        assert_eq!(scanned.without_comments().trim_end(), "const A: &str = \"x\";");
    }

    #[test]
    fn hash_and_slash_syntaxes_use_their_own_comment_markers() {
        let starlark = scan("deps = [\"//a:b\"],  # }", Syntax::Hash).unwrap().code();
        assert!(!starlark.contains('}'));
        assert!(starlark.contains('['));
        let swift = scan("let a = \"}\" // }\nfunc f() {}", Syntax::Slash).unwrap().code();
        assert_eq!(swift.matches('}').count(), 1);
    }

    #[test]
    fn syntax_is_chosen_from_the_file_extension() {
        assert_eq!(Syntax::for_path("tools/a/src/lib.rs"), Syntax::Rust);
        assert_eq!(Syntax::for_path("app/Sources/View.swift"), Syntax::Slash);
        assert_eq!(Syntax::for_path("tools/a/BUILD.bazel"), Syntax::Hash);
        assert_eq!(Syntax::for_path("Cargo.toml"), Syntax::Hash);
    }

    #[test]
    fn balance_check_reports_what_is_wrong() {
        assert!(check_balanced("fn a() { [1, (2)] }").is_ok());
        assert!(check_balanced("}\n\nfn b() {").unwrap_err().contains("did not open"));
        assert!(check_balanced("fn b() {").unwrap_err().contains("open"));
        assert!(check_balanced("( ]").unwrap_err().contains("closes `(` with `]`"));
    }
}
