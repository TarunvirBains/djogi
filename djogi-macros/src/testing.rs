//! Implementation of the `#[djogi_test]` attribute proc-macro.
//!
//! Transforms an `async fn my_test(ctx: DjogiContext)` into a
//! `#[tokio::test]`-runnable by wrapping it with per-test database lifecycle:
//!
//! 1. `CREATE DATABASE djogi_test_<uuid>`.
//! 2. HeeRanjID schema + default node installed in the fresh DB.
//! 3. Optional Postgres extensions (e.g. `postgis`) auto-provisioned via
//!    `CREATE EXTENSION IF NOT EXISTS` — the list comes from the
//!    `extensions = [...]` attribute argument.
//! 4. `DjogiContext` constructed from a pool pointed at the new DB.
//! 5. `DROP DATABASE` on guard drop — runs even if the test panics.
//!
//! # Internals
//!
//! This macro generates code that calls
//! `::djogi::testing::setup_test_db_with_extensions(&["postgis", ...])`,
//! which uses `tokio_postgres` directly for bootstrap (no sqlx), calls
//! `heeranjid::postgres_schema::install_schema` + `seed_default_node` from
//! heeranjid 0.2.1, and then loops over the extension list issuing one
//! `CREATE EXTENSION IF NOT EXISTS "<name>"` per entry.
//!
//! # Usage
//!
//! ```rust,ignore
//! use djogi_macros::djogi_test;
//! use djogi::DjogiContext;
//!
//! // No extensions — same as pre-Task-13 behavior.
//! #[djogi_test]
//! async fn my_test(mut ctx: DjogiContext) { /* ... */ }
//!
//! // Auto-provision PostGIS on the per-test DB.
//! #[djogi_test(extensions = ["postgis"])]
//! async fn geo_test(mut ctx: DjogiContext) { /* ... */ }
//! ```
//!
//! # Attribute grammar
//!
//! The attribute accepts zero or more comma-separated `key = value` entries.
//! The only recognized key in Phase 6.5 is:
//!
//! - `extensions = [ "name1", "name2", ... ]` — array of string literals;
//!   each element names a Postgres extension to provision. Extension names
//!   are validated at runtime against a strict allowlist (ASCII letters /
//!   digits / underscores, 1..=63 bytes).
//!
//! Any other key produces a span-precise `syn::Error`. Wrong value shape
//! (scalar instead of array, non-string array element, etc.) also errors
//! with a helpful message pointing at the offending token.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{Expr, ExprArray, ExprLit, FnArg, ItemFn, Lit, Meta, Pat, Signature, Token, parse2};

/// Expand `#[djogi_test]` on an `async fn` with one `DjogiContext` parameter.
///
/// The generated code wraps the test body in a `#[tokio::test]` harness that:
///
/// 1. Calls `::djogi::testing::setup_test_db_with_extensions(&[...]).await`
///    to create the per-test DB, install HeeRanjID, auto-provision any
///    requested Postgres extensions, and return a
///    `(TestDbCleanup, DjogiContext)`.
/// 2. Runs the original test body with the `DjogiContext`.
/// 3. Calls `::djogi::testing::teardown_test_db(cleanup).await` explicitly after
///    the body returns — whether it returns normally or panics.
///
/// Panics from the test body are caught via `::futures::FutureExt::catch_unwind`
/// so teardown can run before the panic is resumed via `::std::panic::resume_unwind`.
///
/// # Parsed attribute arguments
///
/// - `extensions = [ "postgis", "pg_trgm", ... ]` — optional array of
///   Postgres extension names to provision on the per-test DB via
///   `CREATE EXTENSION IF NOT EXISTS`. See the module docs for the exact
///   grammar and validation rules.
pub fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    // Parse the attribute arguments into a typed `Args` struct.
    let args = match parse_args(attr) {
        Ok(a) => a,
        Err(e) => return e.to_compile_error(),
    };

    // Parse the annotated function.
    let func: ItemFn = match parse2(item) {
        Ok(f) => f,
        Err(e) => return e.to_compile_error(),
    };

    // Validate: must be async.
    if func.sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            func.sig.fn_token,
            "#[djogi_test] can only be applied to an async fn",
        )
        .to_compile_error();
    }

    // Validate: exactly one argument whose name we'll use as the ctx binding.
    let ctx_arg_name = match extract_ctx_arg_name(&func.sig) {
        Ok(name) => name,
        Err(e) => return e.to_compile_error(),
    };

    let fn_name = &func.sig.ident;
    let fn_body = &func.block;
    let fn_vis = &func.vis;
    // Propagate outer attributes (e.g. `#[ignore]`, `#[should_panic]`,
    // rustdoc comments) onto the generated `#[::tokio::test]` wrapper so
    // user-space test modifiers behave as they would on a plain tokio test.
    // Without this forwarding the macro silently drops `#[ignore]`, causing
    // blocked tests to run and fail instead of being skipped.
    let fn_attrs = &func.attrs;

    // Generate a private inner async fn containing the original test body.
    // The wrapper calls this inner fn via catch_unwind so teardown always runs.
    let inner_name = format_ident!("__djogi_test_inner_{fn_name}");

    // Emit the extension list as a slice of string literals. An empty list
    // still produces a valid `&[&str]` expression — the runtime loop simply
    // executes zero iterations in that case.
    let extensions = &args.extensions;
    let extensions_slice = quote! { &[ #( #extensions ),* ] as &[&str] };

    quote! {
        #( #fn_attrs )*
        #[::tokio::test]
        #fn_vis async fn #fn_name() {
            use ::djogi::__private::futures::FutureExt as _;
            use ::std::panic::AssertUnwindSafe;

            // Inner async fn holds the original test body, called with the
            // DjogiContext from setup_test_db_with_extensions. The parameter
            // is always `mut` so the test body can call `&mut self` methods
            // on the context.
            async fn #inner_name(mut #ctx_arg_name: ::djogi::DjogiContext) {
                #fn_body
            }

            // Set up the per-test database. Panics here (e.g., DATABASE_URL not
            // set, unknown extension name) propagate directly — there is
            // nothing to clean up yet if setup itself failed.
            let (cleanup, ctx) = ::djogi::testing::setup_test_db_with_extensions(
                #extensions_slice,
            )
                .await
                .expect("djogi_test: failed to set up per-test database");

            // Run the test body, catching any panics so teardown always runs.
            let result = AssertUnwindSafe(#inner_name(ctx)).catch_unwind().await;

            // Teardown: drop the per-test database regardless of test outcome.
            // This is async, so it runs cleanly inside the Tokio test runtime
            // without the block_on-in-async-context problem that a Drop impl
            // would face.
            ::djogi::testing::teardown_test_db(cleanup).await;

            // Propagate any panic from the test body now that cleanup is done.
            if let Err(panic_payload) = result {
                ::std::panic::resume_unwind(panic_payload);
            }
        }
    }
}

/// Parsed `#[djogi_test(...)]` attribute arguments.
///
/// Only `extensions = [ ... ]` is currently recognized. Any other key
/// produces a compile error at [`parse_args`].
#[derive(Default)]
struct Args {
    /// Postgres extensions to provision via `CREATE EXTENSION IF NOT EXISTS`.
    /// Stored as the original string literals so we can re-emit them with
    /// their original spans inside the generated slice expression.
    extensions: Vec<syn::LitStr>,
}

/// Parse the `TokenStream` inside the `#[djogi_test(...)]` parentheses.
///
/// Empty input returns [`Args::default()`] (preserves the v1 "no arguments"
/// behavior). Otherwise parses as a comma-separated list of `Meta` entries
/// and dispatches on the entry's path.
fn parse_args(attr: TokenStream) -> Result<Args, syn::Error> {
    if attr.is_empty() {
        return Ok(Args::default());
    }

    let metas: Punctuated<Meta, Token![,]> =
        Punctuated::parse_terminated.parse2(attr).map_err(|e| {
            syn::Error::new(
                e.span(),
                format!("#[djogi_test] could not parse attribute arguments: {e}"),
            )
        })?;

    let mut args = Args::default();
    let mut saw_extensions = false;

    for meta in metas {
        if meta.path().is_ident("extensions") {
            if saw_extensions {
                return Err(syn::Error::new_spanned(
                    meta.path(),
                    "#[djogi_test] `extensions` specified more than once",
                ));
            }
            saw_extensions = true;
            args.extensions = parse_extensions_value(&meta)?;
        } else {
            let path_display = meta
                .path()
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_else(|| "<unknown>".into());
            return Err(syn::Error::new_spanned(
                meta.path(),
                format!(
                    "#[djogi_test] unknown argument `{path_display}`; \
                     supported arguments: `extensions = [...]`",
                ),
            ));
        }
    }

    Ok(args)
}

/// Parse an `extensions = [ "a", "b" ]` entry into a vec of `LitStr`.
///
/// Validates that:
/// - The entry is a `Meta::NameValue` (not a bare `extensions` or
///   `extensions(...)` list form).
/// - The value is an array expression.
/// - Every element is a string literal.
///
/// Empty arrays are accepted — they round-trip through the generated slice
/// as `&[]` and the runtime loop runs zero iterations.
fn parse_extensions_value(meta: &Meta) -> Result<Vec<syn::LitStr>, syn::Error> {
    let Meta::NameValue(nv) = meta else {
        return Err(syn::Error::new_spanned(
            meta,
            "#[djogi_test] `extensions` must use the form `extensions = [\"name\", ...]`",
        ));
    };

    let Expr::Array(ExprArray { elems, .. }) = &nv.value else {
        return Err(syn::Error::new_spanned(
            &nv.value,
            "#[djogi_test] `extensions` expects an array literal, \
             e.g. `extensions = [\"postgis\"]`",
        ));
    };

    let mut out = Vec::with_capacity(elems.len());
    for elem in elems {
        let Expr::Lit(ExprLit {
            lit: Lit::Str(s), ..
        }) = elem
        else {
            return Err(syn::Error::new_spanned(
                elem,
                "#[djogi_test] `extensions` array elements must be string literals, \
                 e.g. `\"postgis\"`",
            ));
        };
        out.push(s.clone());
    }
    Ok(out)
}

/// Extract the identifier name of the first (and only) function argument.
///
/// Validates that:
/// - There is exactly one parameter.
/// - It is a named pattern (`ctx: DjogiContext`), not `self`, `_`, or a
///   destructure pattern.
///
/// Returns the `syn::Ident` to use as the binding name in the generated code.
fn extract_ctx_arg_name(sig: &Signature) -> Result<syn::Ident, syn::Error> {
    if sig.inputs.len() != 1 {
        return Err(syn::Error::new_spanned(
            &sig.inputs,
            "#[djogi_test] expects exactly one argument: `ctx: DjogiContext`",
        ));
    }

    let arg = sig.inputs.first().unwrap();
    match arg {
        FnArg::Typed(pat_type) => match &*pat_type.pat {
            Pat::Ident(pat_ident) => Ok(pat_ident.ident.clone()),
            _ => Err(syn::Error::new_spanned(
                arg,
                "#[djogi_test] argument must be a simple identifier pattern, \
                 e.g. `ctx: DjogiContext`",
            )),
        },
        FnArg::Receiver(_) => Err(syn::Error::new_spanned(
            arg,
            "#[djogi_test] cannot be applied to a method (got `self` argument)",
        )),
    }
}
