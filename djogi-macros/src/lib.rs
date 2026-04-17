//! Proc macros for the Djogi framework.
//!
//! Provides `#[model(table = "...")]` — the attribute macro that does field
//! injection and derives all Model impls. `#[derive(Model)]` is a no-op stub
//! kept for potential future use.

mod model;

use proc_macro::TokenStream;

/// The primary Djogi macro. Annotate any struct with `#[model(table = "...")]`
/// to inject framework fields and derive CRUD, `FromRow`, and model descriptor.
///
/// ```rust,ignore
/// use djogi::prelude::*;
///
/// #[model(table = "posts")]
/// #[derive(Debug, Clone)]
/// pub struct Post {
///     pub title: String,
///     pub published: bool,
/// }
/// ```
#[proc_macro_attribute]
pub fn model(attr: TokenStream, item: TokenStream) -> TokenStream {
    model::expand(attr.into(), item.into()).into()
}

/// No-op stub — field injection requires `#[model]` (attribute macro).
/// Kept as a placeholder for future derive-based extensions.
///
/// NOTE: Only `field` is listed as a helper attribute here, not `model`.
/// Listing `model` as a helper would shadow the `#[model]` proc_macro_attribute
/// and cause ambiguous resolution (Post-Review Fix #4).
#[proc_macro_derive(Model, attributes(field))]
pub fn derive_model(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}
