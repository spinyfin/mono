//! Recognises a run of whole Rust items — functions, types, impls,
//! modules, constants, `use` and `mod` declarations — in a fragment of
//! inserted text.
//!
//! A fragment qualifies only when it is nothing but complete items, each
//! with its own doc comments and attributes. A statement, an expression, a
//! match arm, a struct field, an enum variant or a stray comment makes the
//! whole fragment fail: those can depend on their position or on each
//! other in ways that keeping both sides would not preserve.

use super::lexer::{Syntax, check_balanced, scan};

/// One whole item found in an inserted fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    /// A `use`, `mod name;` or `extern crate` declaration rather than a
    /// definition with a body.
    pub(crate) is_import: bool,
    /// Identity within its namespace (`value:parse`, `type:Config`,
    /// `use:crate::a::B`), for spotting both sides adding the same item.
    /// `None` for items that may legitimately repeat (`const _`).
    pub(crate) key: Option<String>,
    /// The definition with comments dropped, whitespace collapsed and its
    /// own name blanked: equal shapes are one definition added under two
    /// names. `None` for declarations, which have no body to compare.
    pub(crate) shape: Option<String>,
    /// Whether the item, with its doc comments and attributes, fits on
    /// one line.
    pub(crate) single_line: bool,
}

/// Why a fragment is not a run of whole items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// It is a slice out of the middle of something: its brackets do not
    /// balance, or it ends inside a string or comment. No reading of such
    /// a fragment can be kept.
    NotSelfContained(String),
    /// It is self-contained but holds something other than whole items.
    NotItems(String),
}

impl Refusal {
    pub(crate) fn reason(&self) -> &str {
        match self {
            Refusal::NotSelfContained(reason) | Refusal::NotItems(reason) => reason,
        }
    }
}

/// Splits `lines` into whole items, or says why they are not that.
pub(crate) fn split_items(lines: &[String]) -> Result<Vec<Item>, Refusal> {
    let text = lines.join("\n");
    let scanned = scan(&text, Syntax::Rust).map_err(Refusal::NotSelfContained)?;
    let code_text = scanned.code();
    check_balanced(&code_text).map_err(Refusal::NotSelfContained)?;
    let code: Vec<char> = code_text.chars().collect();
    let plain: Vec<char> = scanned.without_comments().chars().collect();
    let original: Vec<char> = text.chars().collect();
    items_in(&code, &plain, &original).map_err(Refusal::NotItems)
}

fn items_in(code: &[char], plain: &[char], original: &[char]) -> Result<Vec<Item>, String> {
    let mut items = Vec::new();
    // Where the next item's leading comments and attributes may begin.
    let mut chunk_start = 0;
    let mut pos = skip_whitespace(code, 0);
    while pos < code.len() {
        let item_start = skip_attributes(code, pos)?;
        let header = parse_header(code, item_start)?;
        let end = item_end(code, &header)?;
        let line_end = code[end..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(code.len(), |n| end + n);
        if code[end + 1..line_end].iter().any(|c| !c.is_whitespace()) {
            return Err("has more code after an item on the same line".to_owned());
        }
        let first_text = (chunk_start..line_end)
            .find(|&i| !original[i].is_whitespace())
            .unwrap_or(item_start);
        let single_line = !original[first_text..end].contains(&'\n');
        items.push(Item {
            is_import: header.kind.is_import(code, end),
            key: header.key(code, end),
            shape: header.shape(plain, item_start, end, code),
            single_line,
        });
        chunk_start = line_end;
        pos = skip_whitespace(code, line_end);
    }
    if original[chunk_start..].iter().any(|c| !c.is_whitespace()) {
        return Err("ends with a comment that belongs to no item".to_owned());
    }
    if items.is_empty() {
        return Err("adds no item".to_owned());
    }
    Ok(items)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Fn,
    /// `struct`, `enum`, `union`, `trait`.
    TypeDef,
    TypeAlias,
    Const,
    Static,
    Impl,
    Mod,
    Use,
    ExternCrate,
    ForeignBlock,
    MacroRules,
}

impl Kind {
    /// Whether the item ends at its first top-level `;` and never at a
    /// closing brace (a `const` may end `= Foo { .. };`).
    fn ends_only_at_semicolon(self) -> bool {
        matches!(
            self,
            Kind::TypeAlias | Kind::Const | Kind::Static | Kind::Use | Kind::ExternCrate
        )
    }

    fn is_import(self, code: &[char], end: usize) -> bool {
        match self {
            Kind::Use | Kind::ExternCrate => true,
            // `mod name;` declares a module file; `mod name { .. }` defines one.
            Kind::Mod => code[end] == ';',
            _ => false,
        }
    }
}

struct Header {
    kind: Kind,
    name: Option<String>,
    /// Index of the kind keyword's first character.
    keyword_at: usize,
    /// Index just past the name (or past the keyword when there is none).
    after_name: usize,
}

impl Header {
    fn key(&self, code: &[char], end: usize) -> Option<String> {
        let name = self.name.as_deref();
        match self.kind {
            Kind::Fn | Kind::Const | Kind::Static => name.filter(|n| *n != "_").map(|n| format!("value:{n}")),
            Kind::TypeDef | Kind::TypeAlias | Kind::Mod => name.map(|n| format!("type:{n}")),
            Kind::MacroRules => name.map(|n| format!("macro:{n}")),
            Kind::ExternCrate => name.map(|n| format!("crate:{n}")),
            Kind::Use => Some(format!("use:{}", squeeze(&code[self.after_name..end], ""))),
            Kind::Impl => {
                let header_end = (self.keyword_at..end).find(|&i| code[i] == '{').unwrap_or(end);
                Some(squeeze(&code[self.keyword_at..header_end], " "))
            }
            Kind::ForeignBlock => None,
        }
    }

    fn shape(&self, plain: &[char], item_start: usize, end: usize, code: &[char]) -> Option<String> {
        if self.kind.is_import(code, end) || self.kind == Kind::Impl {
            return None;
        }
        let name = self.name.as_deref()?;
        let name_at = self.after_name - name.chars().count();
        let mut shape = squeeze(&plain[item_start..name_at], " ");
        shape.push_str(" _ ");
        shape.push_str(&squeeze(&plain[self.after_name..=end], " "));
        Some(shape)
    }
}

/// Collapses every run of whitespace in `chars` to `joiner`.
fn squeeze(chars: &[char], joiner: &str) -> String {
    let text: String = chars.iter().collect();
    text.split_whitespace().collect::<Vec<_>>().join(joiner)
}

fn skip_whitespace(code: &[char], mut pos: usize) -> usize {
    while pos < code.len() && code[pos].is_whitespace() {
        pos += 1;
    }
    pos
}

fn word_at(code: &[char], pos: usize) -> String {
    code[pos.min(code.len())..]
        .iter()
        .take_while(|c| c.is_alphanumeric() || **c == '_')
        .collect()
}

/// Index of the bracket that closes the one at `open`. The fragment is
/// already known to be balanced.
fn matching_close(code: &[char], open: usize) -> Result<usize, String> {
    let mut depth = 0usize;
    for (offset, ch) in code[open..].iter().enumerate() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(open + offset);
                }
            }
            _ => {}
        }
    }
    Err("leaves a bracket open".to_owned())
}

/// Skips the `#[..]` attributes in front of an item.
fn skip_attributes(code: &[char], mut pos: usize) -> Result<usize, String> {
    while code.get(pos) == Some(&'#') {
        match code.get(pos + 1) {
            Some('[') => {}
            Some('!') => {
                return Err(
                    "adds an inner attribute, which applies to the enclosing module rather than an item".to_owned(),
                );
            }
            _ => return Err("starts with `#`, which is not an attribute".to_owned()),
        }
        pos = skip_whitespace(code, matching_close(code, pos + 1)? + 1);
        if pos >= code.len() {
            return Err("ends with an attribute that is attached to no item".to_owned());
        }
    }
    Ok(pos)
}

fn parse_header(code: &[char], start: usize) -> Result<Header, String> {
    let not_an_item = || {
        let line: String = code[start..].iter().take_while(|&&c| c != '\n').collect();
        format!("adds something that is not a whole item (`{}`)", line.trim())
    };
    let mut pos = start;
    if word_at(code, pos) == "pub" {
        pos = skip_whitespace(code, pos + 3);
        if code.get(pos) == Some(&'(') {
            pos = matching_close(code, pos)? + 1;
        }
    }
    let mut after_extern = false;
    loop {
        pos = skip_whitespace(code, pos);
        let word = word_at(code, pos);
        let after_word = pos + word.chars().count();
        let next_word = word_at(code, skip_whitespace(code, after_word));
        let kind = match word.as_str() {
            "fn" => Kind::Fn,
            "struct" | "enum" | "trait" => Kind::TypeDef,
            // `union` is only a keyword in front of a type name.
            "union" if !next_word.is_empty() => Kind::TypeDef,
            "type" => Kind::TypeAlias,
            "static" => Kind::Static,
            "impl" => Kind::Impl,
            "mod" => Kind::Mod,
            "use" => Kind::Use,
            "crate" if after_extern => Kind::ExternCrate,
            "macro_rules" if code.get(after_word) == Some(&'!') => Kind::MacroRules,
            "const" if !matches!(next_word.as_str(), "fn" | "async" | "unsafe" | "extern") => Kind::Const,
            "const" | "default" | "async" | "unsafe" | "safe" | "auto" => {
                after_extern = false;
                pos = after_word;
                continue;
            }
            "extern" => {
                // The ABI string, if any, is blanked out of `code`.
                let next = skip_whitespace(code, after_word);
                if code.get(next) == Some(&'{') {
                    return Ok(Header {
                        kind: Kind::ForeignBlock,
                        name: None,
                        keyword_at: pos,
                        after_name: after_word,
                    });
                }
                after_extern = true;
                pos = after_word;
                continue;
            }
            _ => return Err(not_an_item()),
        };
        let mut name_at = skip_whitespace(code, after_word + usize::from(kind == Kind::MacroRules));
        if kind == Kind::Static && word_at(code, name_at) == "mut" {
            name_at = skip_whitespace(code, name_at + 3);
        }
        let name = match kind {
            Kind::Impl | Kind::Use => None,
            _ => Some(word_at(code, name_at)).filter(|name| !name.is_empty()),
        };
        if name.is_none() && !matches!(kind, Kind::Impl | Kind::Use) {
            return Err(not_an_item());
        }
        let after_name = match &name {
            Some(name) => name_at + name.chars().count(),
            None => after_word,
        };
        return Ok(Header {
            kind,
            name,
            keyword_at: pos,
            after_name,
        });
    }
}

/// Index of the character that ends the item: its top-level `;`, or the
/// `}` that closes its body.
fn item_end(code: &[char], header: &Header) -> Result<usize, String> {
    let mut pos = header.after_name;
    while pos < code.len() {
        match code[pos] {
            ';' => return Ok(pos),
            '(' | '[' | '{' => {
                let close = matching_close(code, pos)?;
                if code[close] == '}' && !header.kind.ends_only_at_semicolon() {
                    return Ok(close);
                }
                pos = close + 1;
            }
            _ => pos += 1,
        }
    }
    Err("ends in the middle of an item".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    fn keys(text: &str) -> Vec<Option<String>> {
        split_items(&lines(text))
            .unwrap()
            .into_iter()
            .map(|item| item.key)
            .collect()
    }

    fn refusal(text: &str) -> String {
        split_items(&lines(text)).unwrap_err().reason().to_owned()
    }

    #[test]
    fn splits_functions_with_their_docs_and_attributes() {
        let items = split_items(&lines(
            "\n/// Checks the thing.\n#[tokio::test]\nasync fn checks_the_thing() {\n    let a = vec![1, 2];\n    assert_eq!(a.len(), 2);\n}\n\n#[test]\nfn second() {}\n",
        ))
        .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].key.as_deref(), Some("value:checks_the_thing"));
        assert!(!items[0].single_line && !items[0].is_import);
        assert_eq!(items[1].key.as_deref(), Some("value:second"));
        assert!(!items[1].single_line, "an attribute line makes it a multi-line item");
    }

    #[test]
    fn recognises_every_kind_of_item() {
        assert_eq!(
            keys(
                "pub(crate) struct A { x: [u8; 4] }\npub struct B(u32);\nenum C { One, Two }\ntrait D { fn d(&self); }\ntype E = Vec<u8>;\npub const F: &str = \"f;\";\nstatic mut G: u8 = 0;\nconst H: Point = Point { x: 1 };\nunsafe extern \"C\" fn i() {}\npub const fn j() -> u8 { 1 }\nmacro_rules! k { () => {}; }\nunion L { a: u8 }\nextern crate m;\n"
            ),
            [
                "type:A", "type:B", "type:C", "type:D", "type:E", "value:F", "value:G", "value:H", "value:i",
                "value:j", "macro:k", "type:L", "crate:m"
            ]
            .map(|key| Some(key.to_owned()))
        );
    }

    #[test]
    fn imports_and_module_declarations_are_marked_as_imports() {
        let items = split_items(&lines(
            "use std::fmt;\npub mod alpha;\npub use crate::a::{B, C};\nmod inline { fn x() {} }\n",
        ))
        .unwrap();
        assert_eq!(
            items.iter().map(|item| item.is_import).collect::<Vec<_>>(),
            [true, true, true, false]
        );
        assert_eq!(items[0].key.as_deref(), Some("use:std::fmt"));
        assert_eq!(items[1].key.as_deref(), Some("type:alpha"));
        assert_eq!(items[2].key.as_deref(), Some("use:crate::a::{B,C}"));
        assert!(items[0].single_line);
    }

    #[test]
    fn impl_blocks_are_keyed_by_their_header() {
        assert_eq!(
            keys("impl  Default for Config {\n    fn default() -> Self { Self {} }\n}\nimpl<T: Clone> Wrapper<T> {}\n"),
            [
                Some("impl Default for Config".to_owned()),
                Some("impl<T: Clone> Wrapper<T>".to_owned())
            ]
        );
    }

    #[test]
    fn an_anonymous_const_has_no_identity() {
        assert_eq!(keys("const _: () = assert!(true);\n"), [None]);
    }

    #[test]
    fn shapes_ignore_the_name_and_comments_but_not_the_value() {
        let shape = |text: &str| split_items(&lines(text)).unwrap()[0].shape.clone();
        assert_eq!(
            shape("/// One.\npub const A_TAG: &str = \"tag-v1\";"),
            shape("// Two.\npub const B_TAG: &str = \"tag-v1\";")
        );
        assert_ne!(shape("pub const A: &str = \"x\";"), shape("pub const B: &str = \"y\";"));
        assert_eq!(shape("use a::B;"), None);
    }

    #[test]
    fn statements_and_fragments_are_not_items() {
        assert!(refusal("let a = 1;").contains("not a whole item"));
        assert!(refusal("registry.register(Box::new(x));").contains("not a whole item"));
        assert!(refusal("Stage::Paused => \"paused\",").contains("not a whole item"));
        assert!(refusal("    name: String,").contains("not a whole item"));
        assert!(refusal("if version < 49 {\n    run();\n}").contains("not a whole item"));
        assert!(refusal("lazy_static! { static ref X: u8 = 1; }").contains("not a whole item"));
    }

    #[test]
    fn a_fragment_cut_out_of_the_middle_of_an_item_is_refused() {
        // The classic misaligned diff: the previous item's closing brace
        // followed by a new item without its own.
        assert!(refusal("}\n\nfn b() {\n    one();").contains("did not open"));
        assert!(refusal("fn b() {\n    one();").contains("open"));
        assert!(refusal("fn a() {} fn b() {}").contains("same line"));
    }

    #[test]
    fn a_refusal_says_whether_the_fragment_was_self_contained() {
        let cut = split_items(&lines("fn b() {\n    one();")).unwrap_err();
        assert!(matches!(cut, Refusal::NotSelfContained(_)), "{cut:?}");
        let open_string = split_items(&lines("const A: &str = \"open;")).unwrap_err();
        assert!(matches!(open_string, Refusal::NotSelfContained(_)), "{open_string:?}");
        let statement = split_items(&lines("run();")).unwrap_err();
        assert!(matches!(statement, Refusal::NotItems(_)), "{statement:?}");
    }

    #[test]
    fn comments_and_attributes_must_belong_to_an_item() {
        assert!(refusal("fn a() {}\n// trailing note").contains("belongs to no item"));
        assert!(refusal("// only a note").contains("belongs to no item"));
        assert!(refusal("#[test]").contains("attached to no item"));
        assert!(refusal("#![allow(dead_code)]\nfn a() {}").contains("inner attribute"));
        assert!(refusal("\n\n").contains("adds no item"));
    }
}
