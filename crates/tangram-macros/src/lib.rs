//! Proc macros for the Tangram SDK: `#[model]` and `#[actions]`.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    Expr, FnArg, ImplItem, ItemImpl, Lit, Meta, Pat, ReturnType, Type, parse_macro_input,
    spanned::Spanned,
};

/// Marks a struct as a Tangram model: a plain Rust type that the SDK can store
/// in a replicated CRDT document, serialize to clients, and describe with a
/// JSON schema.
///
/// Expands to the full derive set the SDK needs (serde, schemars, autosurgeon)
/// so app code only writes `#[model]`. The app crate must depend on `serde`,
/// `schemars`, and `autosurgeon` (the derives reference those crates).
#[proc_macro_attribute]
pub fn model(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = TokenStream2::from(item);
    quote! {
        #[derive(
            Debug,
            Clone,
            ::serde::Serialize,
            ::serde::Deserialize,
            ::schemars::JsonSchema,
            ::autosurgeon::Reconcile,
            ::autosurgeon::Hydrate,
        )]
        #item
    }
    .into()
}

/// Turns the public methods of an impl block into Tangram actions.
///
/// Every public method taking `&mut self` (a mutating action) or `&self` (a
/// read-only action) becomes an entry in the model's action registry. The SDK
/// exposes each action as an MCP tool and an HTTP endpoint; doc comments
/// become tool descriptions and the remaining parameters become a JSON-schema
/// described argument object.
///
/// Methods returning `Result<T, E>` surface `Err` as an action failure
/// (requires `E: Display`); any other return type is serialized as the result.
#[proc_macro_attribute]
pub fn actions(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let impl_block = parse_macro_input!(item as ItemImpl);
    match expand_actions(&impl_block) {
        Ok(generated) => quote! { #impl_block #generated }.into(),
        Err(err) => {
            let err = err.to_compile_error();
            quote! { #impl_block #err }.into()
        }
    }
}

fn expand_actions(impl_block: &ItemImpl) -> syn::Result<TokenStream2> {
    let self_ty = &*impl_block.self_ty;

    let mut arg_structs = Vec::new();
    let mut defs = Vec::new();

    for item in &impl_block.items {
        let ImplItem::Fn(method) = item else { continue };
        if !matches!(method.vis, syn::Visibility::Public(_)) {
            continue;
        }
        let sig = &method.sig;
        if sig.asyncness.is_some() {
            return Err(syn::Error::new(
                sig.span(),
                "tangram actions must be synchronous (they run inside a state transaction)",
            ));
        }

        // Receiver decides whether the action mutates the document.
        let Some(FnArg::Receiver(receiver)) = sig.inputs.first() else {
            continue; // associated fn (no self) — not an action
        };
        let mutates = receiver.mutability.is_some();

        let fn_ident = &sig.ident;
        let name_str = fn_ident.to_string();
        let description = doc_comment(&method.attrs);
        let args_ident = format_ident!("__TangramArgs_{}", fn_ident);

        // Collect (ident, type) for every non-self parameter.
        let mut fields = Vec::new();
        let mut field_idents = Vec::new();
        for arg in sig.inputs.iter().skip(1) {
            let FnArg::Typed(pat_ty) = arg else { continue };
            let Pat::Ident(pat_ident) = &*pat_ty.pat else {
                return Err(syn::Error::new(
                    pat_ty.span(),
                    "tangram action parameters must be simple identifiers",
                ));
            };
            let ident = &pat_ident.ident;
            let ty = &*pat_ty.ty;
            fields.push(quote! { pub #ident: #ty });
            field_idents.push(ident.clone());
        }

        arg_structs.push(quote! {
            #[derive(::serde::Deserialize, ::schemars::JsonSchema)]
            #[allow(non_camel_case_types)]
            struct #args_ident {
                #(#fields,)*
            }
        });

        let call = quote! { model.#fn_ident(#(args.#field_idents),*) };
        // Result-returning actions propagate Err as an action failure.
        let invoke = if returns_result(&sig.output) {
            quote! {
                let out = #call.map_err(|e| ::tangram::ActionError::failed(e.to_string()))?;
            }
        } else {
            quote! { let out = #call; }
        };

        defs.push(quote! {
            ::tangram::ActionDef {
                name: #name_str,
                description: #description,
                mutates: #mutates,
                input_schema: || {
                    ::tangram::__private::serde_json::to_value(::schemars::schema_for!(#args_ident))
                        .expect("action arg schema serializes")
                },
                handler: |model, raw| {
                    let args: #args_ident = ::tangram::__private::serde_json::from_value(raw)
                        .map_err(::tangram::ActionError::bad_args)?;
                    #invoke
                    ::tangram::__private::serde_json::to_value(out)
                        .map_err(::tangram::ActionError::internal)
                },
            }
        });
    }

    Ok(quote! {
        const _: () = {
            #(#arg_structs)*

            impl ::tangram::Actions for #self_ty {
                fn actions() -> ::std::vec::Vec<::tangram::ActionDef<Self>> {
                    ::std::vec![#(#defs),*]
                }
            }
        };
    })
}

/// Extract a description from `#[doc = "..."]` attributes (doc comments).
fn doc_comment(attrs: &[syn::Attribute]) -> String {
    let mut lines = Vec::new();
    for attr in attrs {
        if let Meta::NameValue(nv) = &attr.meta
            && nv.path.is_ident("doc")
            && let Expr::Lit(expr_lit) = &nv.value
            && let Lit::Str(s) = &expr_lit.lit
        {
            lines.push(s.value().trim().to_string());
        }
    }
    lines.join("\n")
}

/// Best-effort detection of a `Result<..>` return type by its final path segment.
fn returns_result(output: &ReturnType) -> bool {
    let ReturnType::Type(_, ty) = output else {
        return false;
    };
    let Type::Path(type_path) = &**ty else {
        return false;
    };
    type_path
        .path
        .segments
        .last()
        .is_some_and(|seg| seg.ident == "Result")
}
