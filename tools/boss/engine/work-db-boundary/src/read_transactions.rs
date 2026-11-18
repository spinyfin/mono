//! WorkDb helpers must choose transaction behavior explicitly rather than inherit
//! the writable connection's IMMEDIATE default.
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens;

pub(crate) fn validate(implementation: &syn::ItemImpl) -> syn::Result<()> {
    for item in &implementation.items {
        let syn::ImplItem::Fn(method) = item else { continue };
        reject_implicit_transactions(method.block.to_token_stream())?;
    }
    Ok(())
}

fn reject_implicit_transactions(tokens: TokenStream) -> syn::Result<()> {
    let tokens: Vec<_> = tokens.into_iter().collect();
    for window in tokens.windows(3) {
        if matches!(&window[0], TokenTree::Punct(p) if p.as_char() == '.')
            && matches!(&window[1], TokenTree::Ident(i) if i == "transaction" || i == "unchecked_transaction")
            && matches!(&window[2], TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis && g.stream().is_empty())
        {
            return Err(syn::Error::new(
                window[1].span(),
                "WorkDb helpers must choose transaction behavior explicitly; use Deferred for reads and Immediate for writes",
            ));
        }
    }
    for token in tokens {
        if let TokenTree::Group(group) = token {
            reject_implicit_transactions(group.stream())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_both_implicit_apis_regardless_of_method_name() {
        for name in [
            "get_snapshot",
            "comments_banner_state",
            "can_admit_review_batch_for_pool",
            "update_rows",
        ] {
            for method in ["transaction", "unchecked_transaction"] {
                let input = format!("impl WorkDb {{ fn {name}(&self) {{ if true {{ conn.{method}()?; }} }} }}");
                assert!(validate(&syn::parse_str(&input).unwrap()).is_err(), "{name}: {method}");
            }
        }
    }

    #[test]
    fn accepts_explicit_read_and_write_behavior() {
        let input = syn::parse_quote! {
            impl WorkDb {
                fn get_snapshot(&self) {
                    conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
                    let message = "conn.transaction()";
                }
                fn update_rows(&self) { conn.transaction_with_behavior(TransactionBehavior::Immediate)?; }
            }
        };
        assert!(validate(&input).is_ok());
    }
}
