//! Proc macros for the Djogi framework.
//!
//! Provides `#[derive(Model)]` and related attribute macros.

use proc_macro::TokenStream;

/// Derive the `Model` trait for a struct.
///
/// Generates CRUD operations, `FromRow`, field accessors, and model
/// descriptor registration.
#[proc_macro_derive(Model, attributes(model, field))]
pub fn derive_model(_input: TokenStream) -> TokenStream {
    // Phase 1 will implement this
    TokenStream::new()
}
