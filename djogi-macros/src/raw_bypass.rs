//! Implementation of the raw SQL bypass attribute macro.
//! The macro deliberately does one thing: inject the hidden raw-access
//! extension traits into the decorated scope. The verbose public attribute
//! name lives in `lib.rs` because proc-macro entry points must be exported
//! from the crate root.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Item, parse_quote};

pub fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new_spanned(
            attr,
            "`#[djogi::deliberately_bypass_convention_with_raw_sql]` does not accept arguments",
        )
        .to_compile_error();
    }

    match try_expand(item) {
        Ok(tokens) => tokens,
        Err(err) => err.to_compile_error(),
    }
}

fn try_expand(item: TokenStream) -> syn::Result<TokenStream> {
    let item: Item = syn::parse2(item)?;

    let injected_access = raw_access_stmt();
    let injected_pool = raw_pool_stmt();

    match item {
        Item::Fn(mut function) => {
            function.block.stmts.insert(0, injected_access);
            function.block.stmts.insert(1, injected_pool);
            Ok(quote! { #function })
        }
        Item::Impl(mut impl_block) => {
            for impl_item in &mut impl_block.items {
                if let syn::ImplItem::Fn(method) = impl_item {
                    method.block.stmts.insert(0, injected_access.clone());
                    method.block.stmts.insert(1, injected_pool.clone());
                }
            }
            Ok(quote! { #impl_block })
        }
        Item::Mod(mut module) => {
            let Some((_, contents)) = module.content.as_mut() else {
                return Err(syn::Error::new_spanned(
                    &module,
                    "`#[djogi::deliberately_bypass_convention_with_raw_sql]` cannot decorate a \
      file-loaded module declaration (`mod foo;`). Either inline the module body \
      (`mod foo {... }`) and decorate that, or attach the attribute to specific \
      `fn` or `impl` items inside the module's source file.",
                ));
            };

            contents.push(raw_access_item());
            contents.push(raw_pool_item());
            Ok(quote! { #module })
        }
        other => Err(syn::Error::new_spanned(
            other,
            "`#[djogi::deliberately_bypass_convention_with_raw_sql]` may only decorate `fn`, \
    `impl`, or `mod` (with inline body) items.",
        )),
    }
}

fn raw_access_stmt() -> syn::Stmt {
    parse_quote!(
        #[allow(unused_imports)]
        use ::djogi::__bypass::RawAccessExt;
    )
}

fn raw_pool_stmt() -> syn::Stmt {
    parse_quote!(
        #[allow(unused_imports)]
        use ::djogi::__bypass::RawPoolAccessExt;
    )
}

fn raw_access_item() -> syn::Item {
    parse_quote!(
        #[allow(unused_imports)]
        use ::djogi::__bypass::RawAccessExt;
    )
}

fn raw_pool_item() -> syn::Item {
    parse_quote!(
        #[allow(unused_imports)]
        use ::djogi::__bypass::RawPoolAccessExt;
    )
}
