//! Event labels and explicitly selected, typed worker traversal.
//! Policy lives in the visitor implementation, never in this derive.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use std::collections::HashSet;
use syn::{Data, DeriveInput, Fields, Ident, Variant, parse_macro_input};

/// Generate `variant_label`, `map_for_worker`, and an enum-specific
/// `WorkerVisitor` trait. Every variant must select `#[worker(passthrough)]`
/// or `#[worker(transform = hook_name)]`. Hooks receive and return a tuple
/// of the variant's fields, in declaration order, with no default method.
#[proc_macro_derive(EventTraversal, attributes(worker))]
pub fn event_traversal(input: TokenStream) -> TokenStream {
    expand(parse_macro_input!(input as DeriveInput))
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn selection(variant: &Variant) -> syn::Result<Option<Ident>> {
    let mut attrs = variant.attrs.iter().filter(|attr| attr.path().is_ident("worker"));
    let attr = attrs.next().ok_or_else(|| {
        syn::Error::new_spanned(
            variant,
            "every event must select worker(passthrough) or worker(transform = hook)",
        )
    })?;
    if let Some(duplicate) = attrs.next() {
        return Err(syn::Error::new_spanned(duplicate, "duplicate worker selection"));
    }
    let mut selected = false;
    let mut hook = None;
    attr.parse_nested_meta(|meta| {
        if selected {
            return Err(meta.error("duplicate worker selection"));
        }
        selected = true;
        if meta.path.is_ident("passthrough") {
            Ok(())
        } else if meta.path.is_ident("transform") {
            hook = Some(meta.value()?.parse::<Ident>()?);
            Ok(())
        } else {
            Err(meta.error("expected passthrough or transform = hook"))
        }
    })?;
    if !selected {
        return Err(syn::Error::new_spanned(attr, "empty worker selection"));
    }
    Ok(hook)
}

fn expand(input: DeriveInput) -> syn::Result<TokenStream2> {
    if !input.generics.params.is_empty() || input.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "event traversal does not support generics",
        ));
    }
    if let Some(attr) = input.attrs.iter().find(|attr| attr.path().is_ident("worker")) {
        return Err(syn::Error::new_spanned(attr, "worker selection belongs on a variant"));
    }
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(&input, "event traversal requires an enum"));
    };
    let name = &input.ident;
    let visibility = &input.vis;
    let visitor = format_ident!("{name}WorkerVisitor");
    let mut labels = Vec::new();
    let mut traversal = Vec::new();
    let mut methods = Vec::new();
    let mut hooks = HashSet::new();
    for variant in &data.variants {
        let hook = selection(variant)?;
        for field in &variant.fields {
            if let Some(attr) = field.attrs.iter().find(|attr| attr.path().is_ident("worker")) {
                return Err(syn::Error::new_spanned(attr, "worker selection belongs on a variant"));
            }
        }
        let ident = &variant.ident;
        let label = ident.to_string();
        let names: Vec<_> = variant
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| field.ident.clone().unwrap_or_else(|| format_ident!("field_{index}")))
            .collect();
        let (pattern, ignored) = match &variant.fields {
            Fields::Named(_) => (quote! { Self::#ident { #(#names),* } }, quote! { Self::#ident { .. } }),
            Fields::Unnamed(_) => (quote! { Self::#ident(#(#names),*) }, quote! { Self::#ident(..) }),
            Fields::Unit => (quote! { Self::#ident }, quote! { Self::#ident }),
        };
        labels.push(quote! { #ignored => #label });
        if let Some(hook) = hook {
            if !hooks.insert(hook.to_string()) {
                return Err(syn::Error::new_spanned(hook, "duplicate transformation hook"));
            }
            let types: Vec<_> = variant.fields.iter().map(|field| &field.ty).collect();
            methods.push(quote! { fn #hook(&mut self, fields: (#(#types,)*)) -> (#(#types,)*); });
            // A name independent of payload bindings avoids shadowing the visitor
            // when a variant contains a field named `visitor` (or any other name).
            let mut binding = "__event_visitor".to_owned();
            while names.iter().any(|name| name == binding.as_str()) {
                binding.push('_');
            }
            let binding = format_ident!("{binding}");
            traversal.push(quote! {
                (#pattern, #binding) => {
                    let (#(#names,)*) = #binding.#hook((#(#names,)*));
                    #pattern
                }
            });
        } else {
            traversal.push(quote! { (event @ #ignored, _) => event });
        }
    }
    Ok(quote! {
        /// Required transformations selected by the event declarations.
        #visibility trait #visitor {
            #(#methods)*
        }

        impl #name {
            /// Stable Rust variant name for diagnostics; independent of serde tags.
            #visibility fn variant_label(&self) -> &'static str {
                match self { #(#labels,)* }
            }

            /// Traverse explicitly selected payloads without deciding policy.
            #visibility fn map_for_worker(self, visitor: &mut impl #visitor) -> Self {
                match (self, visitor) { #(#traversal,)* }
            }
        }
    })
}

#[cfg(test)]
mod tests;
