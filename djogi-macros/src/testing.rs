//! Implementation of the `#[djogi_test]` attribute proc-macro.
//!
//! Transforms an `async fn my_test(ctx: DjogiContext)` into a
//! `#[tokio::test]`-runnable by wrapping it with per-test database lifecycle:
//!
//! 1. `CREATE DATABASE djogi_test_<uuid>`.
//! 2. HeeRanjID schema + default node installed in the fresh DB.
//! 3. `DjogiContext` constructed from a pool pointed at the new DB.
//! 4. `DROP DATABASE` on guard drop — runs even if the test panics.
//!
//! # Internals through T9
//!
//! This macro generates code that calls
//! `::djogi::testing::setup_test_db()`, which internally rides on sqlx
//! machinery as allowed by Phase 5-Zero plan RQ-1: keeping the sqlx dev-dep
//! through T9 avoids inflating T1's surface area. T10 rewrites
//! `::djogi::testing::setup_test_db` to tokio-postgres + deadpool and
//! removes sqlx from dev-dependencies.
//!
//! # Usage
//!
//! ```rust,ignore
//! use djogi_macros::djogi_test;
//! use djogi::DjogiContext;
//!
//! #[djogi_test]
//! async fn my_test(ctx: DjogiContext) {
//!     // ctx is a DjogiContext backed by a fresh, isolated per-test DB.
//!     // HeeRanjID is installed and the default node is seeded.
//!     // The database is dropped automatically when this function returns,
//!     // whether it returns normally or panics.
//!     let n: i64 = sqlx::query_scalar("SELECT 1::bigint")
//!         .fetch_one(ctx.pool().unwrap())
//!         .await
//!         .unwrap();
//!     assert_eq!(n, 1);
//! }
//! ```

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{FnArg, ItemFn, Pat, Signature, parse2};

/// Expand `#[djogi_test]` on an `async fn` with one `DjogiContext` parameter.
///
/// The generated code wraps the test body in a `#[tokio::test]` harness that:
///
/// 1. Calls `::djogi::testing::setup_test_db().await` to create the per-test DB
///    and get a `(TestDbCleanup, DjogiContext)`.
/// 2. Runs the original test body with the `DjogiContext`.
/// 3. Calls `::djogi::testing::teardown_test_db(cleanup).await` explicitly after
///    the body returns — whether it returns normally or panics.
///
/// Panics from the test body are caught via `::futures::FutureExt::catch_unwind`
/// so teardown can run before the panic is resumed via `::std::panic::resume_unwind`.
pub fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    // Reject any attribute arguments — #[djogi_test] takes no args in v1.
    if !attr.is_empty() {
        return syn::Error::new_spanned(attr, "#[djogi_test] takes no arguments in v1")
            .to_compile_error();
    }

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

    // Generate a private inner async fn containing the original test body.
    // The wrapper calls this inner fn via catch_unwind so teardown always runs.
    let inner_name = format_ident!("__djogi_test_inner_{fn_name}");

    quote! {
        #[::tokio::test]
        #fn_vis async fn #fn_name() {
            use ::djogi::__private::futures::FutureExt as _;
            use ::std::panic::AssertUnwindSafe;

            // Inner async fn holds the original test body, called with the
            // DjogiContext from setup_test_db. The parameter is always `mut`
            // so the test body can call `&mut self` methods on the context.
            async fn #inner_name(mut #ctx_arg_name: ::djogi::DjogiContext) {
                #fn_body
            }

            // Set up the per-test database. Panics here (e.g., DATABASE_URL not
            // set) propagate directly — there is nothing to clean up yet.
            let (cleanup, ctx) = ::djogi::testing::setup_test_db()
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
