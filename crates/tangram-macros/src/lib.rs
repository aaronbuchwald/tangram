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
/// Public `async fn` items become **async actions**: they take a
/// `tangram::Ctx<Self>` as their first parameter instead of `self`, run
/// OUTSIDE the store lock, and may perform I/O (network lookups, etc.). They
/// read state via `Ctx::state` and commit attributed mutations via
/// `Ctx::mutate`; the remaining parameters become the argument object exactly
/// as for sync actions, so every surface sees one identical contract.
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
        // `build_action_tokens` returns `None` for associated functions (no
        // self/ctx receiver) — those are not actions, so we skip them.
        if let Some((arg_struct, def)) = build_action_tokens(self_ty, method)? {
            arg_structs.push(arg_struct);
            defs.push(def);
        }
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

/// Build the arg struct and `ActionDef` tokens for one public method.
///
/// Returns `None` for associated functions (no self/ctx receiver), which are
/// not actions and should be skipped by the caller.
fn build_action_tokens(
    self_ty: &syn::Type,
    method: &syn::ImplItemFn,
) -> syn::Result<Option<(TokenStream2, TokenStream2)>> {
    let sig = &method.sig;
    let is_async = sig.asyncness.is_some();
    let Some(mutates) = check_mutates(sig, is_async)? else {
        return Ok(None);
    };

    let fn_ident = &sig.ident;
    let name_str = fn_ident.to_string();
    let description = doc_comment(&method.attrs);
    let args_ident = format_ident!("__TangramArgs_{}", fn_ident);

    let (fields, field_idents) = collect_method_params(sig)?;

    let arg_struct = build_arg_struct(&args_ident, &fields);
    let def = build_action_def(
        self_ty,
        fn_ident,
        &name_str,
        &description,
        mutates,
        is_async,
        &args_ident,
        &field_idents,
        &sig.output,
    );

    Ok(Some((arg_struct, def)))
}

/// Determine whether an action mutates state and validate the receiver.
///
/// Returns `None` for associated functions (no self/ctx receiver) — those are
/// not actions. Async actions always mutate (context-based); sync actions
/// mutate iff the receiver is `&mut self`.
fn check_mutates(sig: &syn::Signature, is_async: bool) -> syn::Result<Option<bool>> {
    if is_async {
        match sig.inputs.first() {
            Some(FnArg::Typed(_)) => Ok(Some(true)),
            _ => Err(syn::Error::new(
                sig.span(),
                "async tangram actions take a context handle as their first parameter \
                 (e.g. `ctx: Ctx<Self>`) instead of `self`",
            )),
        }
    } else {
        let Some(FnArg::Receiver(receiver)) = sig.inputs.first() else {
            return Ok(None); // associated fn — not an action
        };
        Ok(Some(receiver.mutability.is_some()))
    }
}

/// Collect `(field token, ident)` pairs for every non-self parameter.
fn collect_method_params(
    sig: &syn::Signature,
) -> syn::Result<(Vec<TokenStream2>, Vec<syn::Ident>)> {
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
    Ok((fields, field_idents))
}

/// Emit the `#[derive(Deserialize, JsonSchema)]` argument struct.
fn build_arg_struct(args_ident: &syn::Ident, fields: &[TokenStream2]) -> TokenStream2 {
    quote! {
        #[derive(::serde::Deserialize, ::schemars::JsonSchema)]
        #[allow(non_camel_case_types)]
        struct #args_ident {
            #(#fields,)*
        }
    }
}

/// Emit the `ActionDef { … }` literal for one action.
#[allow(clippy::too_many_arguments)]
fn build_action_def(
    self_ty: &syn::Type,
    fn_ident: &syn::Ident,
    name_str: &str,
    description: &str,
    mutates: bool,
    is_async: bool,
    args_ident: &syn::Ident,
    field_idents: &[syn::Ident],
    output: &ReturnType,
) -> TokenStream2 {
    let call = if is_async {
        quote! { #self_ty::#fn_ident(ctx, #(args.#field_idents),*).await }
    } else {
        quote! { model.#fn_ident(#(args.#field_idents),*) }
    };
    // Result-returning actions propagate Err as an action failure.
    let invoke = if returns_result(output) {
        quote! {
            let out = #call.map_err(|e| ::tangram::ActionError::failed(e.to_string()))?;
        }
    } else {
        quote! { let out = #call; }
    };

    let handler = if is_async {
        quote! {
            ::tangram::ActionHandler::Async(|ctx, raw| ::std::boxed::Box::pin(async move {
                let args: #args_ident = ::tangram::__private::serde_json::from_value(raw)
                    .map_err(::tangram::ActionError::bad_args)?;
                #invoke
                ::tangram::__private::serde_json::to_value(out)
                    .map_err(::tangram::ActionError::internal)
            }))
        }
    } else {
        quote! {
            ::tangram::ActionHandler::Sync(|model, raw| {
                let args: #args_ident = ::tangram::__private::serde_json::from_value(raw)
                    .map_err(::tangram::ActionError::bad_args)?;
                #invoke
                ::tangram::__private::serde_json::to_value(out)
                    .map_err(::tangram::ActionError::internal)
            })
        }
    };

    quote! {
        ::tangram::ActionDef {
            name: #name_str,
            description: #description,
            mutates: #mutates,
            input_schema: || {
                ::tangram::__private::serde_json::to_value(::schemars::schema_for!(#args_ident))
                    .expect("action arg schema serializes")
            },
            handler: #handler,
        }
    }
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
