//! Apply the returned-error boundary to fallible WorkDb methods.
use proc_macro::TokenStream;
use quote::quote;
use syn::{ImplItem, ItemImpl, ReturnType, Type, parse_macro_input};

mod read_transactions;

#[proc_macro_attribute]
pub fn instrument(_args: TokenStream, input: TokenStream) -> TokenStream {
    let mut implementation = parse_macro_input!(input as ItemImpl);
    if let Err(error) = read_transactions::validate(&implementation) {
        return error.into_compile_error().into();
    }
    for item in &mut implementation.items {
        let ImplItem::Fn(method) = item else { continue };
        let ReturnType::Type(_, ty) = &method.sig.output else {
            continue;
        };
        let Type::Path(path) = ty.as_ref() else { continue };
        if !path.path.segments.last().is_some_and(|s| s.ident == "Result") {
            continue;
        }
        let body = &method.block;
        let result = if method.sig.asyncness.is_some() {
            quote! { (async #body).await }
        } else {
            quote! { (|| #body)() }
        };
        method.block = syn::parse_quote!({
            crate::lock_stats::returned(#result)
        });
    }
    quote!(#implementation).into()
}
