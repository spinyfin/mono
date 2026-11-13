use super::expand;

#[test]
fn rejects_incomplete_or_invalid_selections() {
    for (source, expected) in [
        ("enum Event { New }", "every event must select"),
        ("enum Event { #[worker()] New }", "empty worker selection"),
        (
            "enum Event { #[worker(unknown)] New }",
            "expected passthrough or transform",
        ),
        (
            "enum Event { #[worker(passthrough, transform = clean)] New }",
            "duplicate worker selection",
        ),
        (
            "enum Event { #[worker(passthrough)] #[worker(passthrough)] New }",
            "duplicate worker selection",
        ),
        ("enum Event { #[worker(passthrough = true)] New }", "expected `,`"),
        (
            "enum Event { #[worker(transform = \"clean\")] New }",
            "expected identifier",
        ),
        (
            "enum Event { #[worker(transform = clean)] A, #[worker(transform = clean)] B }",
            "duplicate transformation hook",
        ),
        ("#[worker(passthrough)] enum Event { A }", "belongs on a variant"),
        (
            "enum Event { #[worker(passthrough)] A { #[worker(passthrough)] value: u8 } }",
            "belongs on a variant",
        ),
        ("struct Event;", "requires an enum"),
        (
            "enum Event<T> { #[worker(passthrough)] A(T) }",
            "does not support generics",
        ),
    ] {
        let error = expand(syn::parse_str(source).unwrap()).unwrap_err().to_string();
        assert!(
            error.contains(expected),
            "{source}: expected {expected:?}, got {error:?}"
        );
    }
}

#[test]
fn supports_all_payload_shapes_without_changing_the_declaration() {
    let expanded = expand(syn::parse_quote! {
        pub enum Event {
            #[worker(passthrough)] Unit,
            #[worker(passthrough)] Named { payload: String },
            #[worker(passthrough)] Tuple(String),
            #[worker(transform = named)] Changed { value: Option<String> },
            #[worker(transform = tuple)] Pair(u64, Vec<String>),
            #[worker(transform = unit)] Empty,
        }
    })
    .unwrap();
    let file: syn::File = syn::parse2(expanded).unwrap();
    assert_eq!(file.items.len(), 2, "only the visitor and inherent impl are generated");
    let syn::Item::Trait(visitor) = &file.items[0] else {
        panic!("visitor trait")
    };
    assert_eq!(visitor.items.len(), 3);
    for method in &visitor.items {
        let syn::TraitItem::Fn(method) = method else {
            panic!("typed method")
        };
        assert!(method.default.is_none(), "policy cannot have a default transformation");
    }
}
