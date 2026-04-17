//! Generates `{Model}Fields` and `{Model}Filter` — empty stubs for Phase 2.
//!
//! Phase 2 fills these in as the entry points to the QuerySet filter API
//! (`{Model}Fields` for typed closure-based filtering; `{Model}Filter` for
//! programmatic / shell use). Phase 1 emits them as empty unit structs so
//! the names exist in scope and can be referenced by the macro's future
//! expansion without a crate-wide refactor.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::ItemStruct;

pub fn expand(struct_item: &ItemStruct) -> TokenStream {
    let name = &struct_item.ident;
    let fields_name = format_ident!("{}Fields", name);
    let filter_name = format_ident!("{}Filter", name);

    quote! {
        /// Typed field accessors for QuerySet filter closures.
        /// Fully implemented in Phase 2.
        #[derive(Debug, Clone, Copy)]
        pub struct #fields_name;

        /// Programmatic filter builder for dynamic/shell use.
        /// Fully implemented in Phase 2.
        #[derive(Debug, Clone, Default)]
        pub struct #filter_name;
    }
}
