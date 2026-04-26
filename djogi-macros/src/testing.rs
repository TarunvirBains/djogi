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
//!
//! // Auto-create tables for the listed models on the per-test DB
//! // (Phase 7 T10 — closes #18). Removes hand-written CREATE TABLE
//! // boilerplate from integration tests; DDL is driven through the
//! // same migration engine that production uses, so model-shape
//! // changes propagate automatically.
//! #[djogi_test(sync_models = [Widget, Category])]
//! async fn widget_test(mut ctx: DjogiContext) { /* ... */ }
//!
//! // Both keys may appear together — extensions provision first so
//! // PostGIS-dependent columns resolve before tables are created.
//! #[djogi_test(extensions = ["postgis"], sync_models = [Place])]
//! async fn place_test(mut ctx: DjogiContext) { /* ... */ }
//! ```
//!
//! # Attribute grammar
//!
//! The attribute accepts zero or more comma-separated `key = value` entries.
//! Recognized keys:
//!
//! - `extensions = [ "name1", "name2", ... ]` — array of string literals;
//!   each element names a Postgres extension to provision. Extension names
//!   are validated at runtime against a strict allowlist (ASCII letters /
//!   digits / underscores, 1..=63 bytes).
//! - `sync_models = [ Type1, Type2, ... ]` — array of bare type paths,
//!   each implementing the `djogi::Model` trait. Resolved at runtime
//!   via `<Type as ::djogi::model::Model>::descriptor()` (the canonical
//!   absolute path used by the rest of the macro substrate). Empty
//!   array `[]` is accepted as a zero-DDL no-op. Module-qualified
//!   paths (`crate::models::Widget`, `super::Other`) are accepted.
//!   The macro-generated wrapper calls
//!   `::djogi::testing::sync_models` AFTER
//!   `::djogi::testing::setup_test_db_with_extensions`, so any
//!   PostGIS-dependent columns resolve cleanly.
//!
//! Any other key produces a span-precise `syn::Error`. Wrong value shape
//! (scalar instead of array, non-path / non-string array element, etc.)
//! also errors with a helpful message pointing at the offending token.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{
    Expr, ExprArray, ExprLit, ExprPath, FnArg, ItemFn, Lit, Meta, Pat, Path, Signature, Token,
    parse2,
};

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

    // Phase 7 T10 — emit a call to `::djogi::testing::sync_models` when
    // the attribute carried `sync_models = [...]` with at least one
    // entry. Empty `sync_models = []` and an absent keyword both
    // suppress the call entirely (zero-DDL no-op) so the generated
    // wrapper is byte-identical with the pre-T10 shape when the user
    // does not opt in. The slice is emitted as a `&[&'static
    // ModelDescriptor]` so the runtime helper does not own the
    // descriptors. The `&mut` receiver matches the runtime helper's
    // signature; the call lives BETWEEN setup_test_db_with_extensions
    // and the user's test body so any extension-dependent column types
    // (e.g. PostGIS `geography(Point)`) resolve cleanly.
    //
    // The `ctx` binding's `mut` is conditional — emitting `let mut
    // ctx = ...` when no later code takes `&mut ctx` would trigger
    // `unused_mut`. Since CI builds with `-D warnings`, we keep the
    // pre-T10 immutable binding for the no-sync-models path.
    let (ctx_mut_kw, sync_models_call) = match &args.sync_models {
        Some(paths) if !paths.is_empty() => {
            let descriptor_exprs = paths.iter().map(|p| {
                quote! { <#p as ::djogi::model::Model>::descriptor() }
            });
            (
                quote! { mut },
                quote! {
                    ::djogi::testing::sync_models(
                        &mut ctx,
                        &[ #( #descriptor_exprs ),* ],
                    )
                        .await
                        .expect("djogi_test: failed to sync_models on per-test database");
                },
            )
        }
        _ => (quote! {}, quote! {}),
    };

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
            //
            // Wrapper ordering is deliberate and observable:
            //   1. extensions provisioned first (CREATE EXTENSION),
            //   2. sync_models runs (CREATE TABLE / indexes / FKs),
            //   3. user's test body runs.
            // Step 2 is omitted when `sync_models` is absent or `[]`.
            let (cleanup, #ctx_mut_kw ctx) = ::djogi::testing::setup_test_db_with_extensions(
                #extensions_slice,
            )
                .await
                .expect("djogi_test: failed to set up per-test database");

            // Phase 7 T10 — auto-create tables for the listed models on
            // the per-test database. Skipped entirely when the
            // attribute did not carry `sync_models = [...]` or carried
            // an empty list.
            #sync_models_call

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
/// Recognized keys: `extensions = [ ... ]` (Phase 6.5) and
/// `sync_models = [ ... ]` (Phase 7 T10). Any other key produces a
/// compile error at [`parse_args`].
///
/// `#[derive(Debug)]` is for the unit tests' `Result::unwrap_err`
/// path — `syn::Error` already implements `Display`, but `unwrap_err`
/// also requires the `Ok` half implement `Debug`.
#[derive(Debug, Default)]
struct Args {
    /// Postgres extensions to provision via `CREATE EXTENSION IF NOT EXISTS`.
    /// Stored as the original string literals so we can re-emit them with
    /// their original spans inside the generated slice expression.
    extensions: Vec<syn::LitStr>,
    /// Models to materialise on the per-test database via
    /// [`djogi::testing::sync_models`] (Phase 7 T10). Each entry is
    /// the bare type path the user wrote — re-emitted with original
    /// span inside the generated slice. `None` means the
    /// `sync_models` keyword was absent (preserves the pre-T10
    /// "no auto DDL" behaviour); `Some(empty)` means it was present
    /// with an empty array (explicit zero-DDL no-op — still no
    /// `sync_models` call is emitted).
    sync_models: Option<Vec<Path>>,
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
    let mut saw_sync_models = false;

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
        } else if meta.path().is_ident("sync_models") {
            if saw_sync_models {
                return Err(syn::Error::new_spanned(
                    meta.path(),
                    "#[djogi_test] `sync_models` specified more than once",
                ));
            }
            saw_sync_models = true;
            args.sync_models = Some(parse_sync_models_value(&meta)?);
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
                     supported arguments: `extensions = [...]`, `sync_models = [...]`",
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

/// Parse a `sync_models = [ Type1, Type2, ... ]` entry into a vec of
/// [`syn::Path`].
///
/// Validates that:
/// - The entry is a `Meta::NameValue` (not a bare `sync_models` or
///   `sync_models(...)` list form).
/// - The value is an array expression.
/// - Every element is a bare type path. String literals, integer
///   literals, function-call expressions, etc. are rejected with
///   span-precise errors.
///
/// Empty arrays are accepted — they round-trip as a no-op (no
/// `sync_models` runtime call is emitted at all). Module-qualified
/// paths (`crate::models::Widget`, `super::Other`) are accepted: the
/// parser keeps the entire [`Path`] verbatim, so the generated code
/// resolves them from the user's call site.
fn parse_sync_models_value(meta: &Meta) -> Result<Vec<Path>, syn::Error> {
    let Meta::NameValue(nv) = meta else {
        return Err(syn::Error::new_spanned(
            meta,
            "#[djogi_test] `sync_models` must use the form `sync_models = [Type1, Type2, ...]`",
        ));
    };

    let Expr::Array(ExprArray { elems, .. }) = &nv.value else {
        return Err(syn::Error::new_spanned(
            &nv.value,
            "#[djogi_test] `sync_models` expects an array literal of type paths, \
             e.g. `sync_models = [Widget, Category]`",
        ));
    };

    let mut out = Vec::with_capacity(elems.len());
    for elem in elems {
        let Expr::Path(ExprPath {
            qself: None,
            path,
            attrs,
        }) = elem
        else {
            return Err(syn::Error::new_spanned(
                elem,
                "#[djogi_test] `sync_models` array elements must be bare type paths, \
                 e.g. `Widget` or `crate::models::Widget`",
            ));
        };
        if !attrs.is_empty() {
            return Err(syn::Error::new_spanned(
                elem,
                "#[djogi_test] `sync_models` array elements must be bare type paths \
                 without attributes",
            ));
        }
        out.push(path.clone());
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

#[cfg(test)]
mod tests {
    //! Unit tests for the `#[djogi_test]` attribute parser.
    //!
    //! These tests exercise the parsing layer without touching the
    //! emission half — every assertion runs against
    //! [`parse_args`] / [`parse_sync_models_value`]. Compile-fail
    //! coverage of end-to-end macro rejection lives in the trybuild
    //! fixtures under `djogi-macros/tests/compile_fail/`.
    //!
    //! Token-stream walk tests for emission ordering live in
    //! [`emission_order_tests`] below — they exercise [`expand`]
    //! directly.
    use super::*;

    /// Parse the body of a fictitious `#[djogi_test( ... )]` invocation.
    ///
    /// Wraps [`parse_args`] so each test can write the human-friendly
    /// inside-the-parens shape and not worry about `proc_macro2`
    /// boilerplate. Empty input is allowed (matches the bare
    /// `#[djogi_test]` case).
    fn parse(body: &str) -> Result<Args, syn::Error> {
        let ts: TokenStream = body.parse().expect("parse attribute body as TokenStream");
        parse_args(ts)
    }

    /// Render the full macro expansion for the given attribute body
    /// against a fixed `async fn t(mut ctx: DjogiContext) {}` stub.
    ///
    /// Used by the emission-order tests so the assertion can scan the
    /// generated wrapper for substrings like `setup_test_db_with_extensions`
    /// and `sync_models` and assert their relative position. The stub
    /// body is intentionally trivial — these tests only care about the
    /// wrapper shape, not the user's test body.
    fn render_expansion(attr_body: &str) -> String {
        let attr: TokenStream = attr_body.parse().expect("parse attr body as TokenStream");
        let item: TokenStream = "async fn t(mut ctx: DjogiContext) {}"
            .parse()
            .expect("parse item TokenStream");
        super::expand(attr, item).to_string()
    }

    // ── empty / single key happy paths ──────────────────────────────

    #[test]
    fn empty_attribute_parses_to_default() {
        let args = parse("").unwrap();
        assert!(args.extensions.is_empty());
        assert!(args.sync_models.is_none());
    }

    #[test]
    fn extensions_only_parses() {
        let args = parse(r#"extensions = ["postgis", "pg_trgm"]"#).unwrap();
        assert_eq!(args.extensions.len(), 2);
        assert!(args.sync_models.is_none());
    }

    // ── sync_models — happy paths ──────────────────────────────────

    #[test]
    fn sync_models_empty_array_parses_to_empty_vec() {
        let args = parse("sync_models = []").unwrap();
        assert_eq!(args.sync_models.unwrap().len(), 0);
    }

    #[test]
    fn sync_models_single_path_parses() {
        let args = parse("sync_models = [Widget]").unwrap();
        let paths = args.sync_models.unwrap();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].is_ident("Widget"));
    }

    #[test]
    fn sync_models_multiple_paths_parses() {
        let args = parse("sync_models = [Widget, Category, Tag]").unwrap();
        let paths = args.sync_models.unwrap();
        assert_eq!(paths.len(), 3);
        assert!(paths[0].is_ident("Widget"));
        assert!(paths[1].is_ident("Category"));
        assert!(paths[2].is_ident("Tag"));
    }

    #[test]
    fn sync_models_accepts_module_qualified_paths() {
        // Both `crate::module::Type` and `super::Other` should parse —
        // the runtime helper resolves them in the user's call-site
        // namespace via `<Type as ::djogi::Model>::descriptor()`.
        let args = parse("sync_models = [crate::models::Widget, super::Other]").unwrap();
        let paths = args.sync_models.unwrap();
        assert_eq!(paths.len(), 2);
        // First path: 3 segments — `crate`, `models`, `Widget`.
        assert_eq!(paths[0].segments.len(), 3);
        assert_eq!(paths[0].segments[2].ident, "Widget");
        // Second path: 2 segments — `super`, `Other`.
        assert_eq!(paths[1].segments.len(), 2);
        assert_eq!(paths[1].segments[1].ident, "Other");
    }

    #[test]
    fn sync_models_with_extensions_both_parse() {
        let args = parse(r#"extensions = ["postgis"], sync_models = [Place]"#).unwrap();
        assert_eq!(args.extensions.len(), 1);
        assert_eq!(args.sync_models.unwrap().len(), 1);
    }

    // ── sync_models — error paths ──────────────────────────────────

    #[test]
    fn sync_models_keyword_without_value_errors() {
        // `sync_models` alone (no `= [...]`) is parsed as `Meta::Path`
        // by syn, which fails the `Meta::NameValue` match in
        // `parse_sync_models_value`.
        let err = parse("sync_models").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("sync_models"),
            "error must mention `sync_models`: {msg}"
        );
        assert!(
            msg.contains("sync_models = [Type1, Type2, ...]"),
            "error must point at the correct grammar: {msg}"
        );
    }

    #[test]
    fn sync_models_non_array_value_errors() {
        let err = parse("sync_models = Widget").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("array literal of type paths"),
            "error must point at the array shape: {msg}"
        );
    }

    #[test]
    fn sync_models_string_element_errors() {
        // String literals where bare paths are expected — common
        // copy-paste mistake from `extensions = ["postgis"]`.
        let err = parse(r#"sync_models = ["Widget"]"#).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bare type paths"),
            "error must call out the type-path requirement: {msg}"
        );
    }

    #[test]
    fn sync_models_integer_element_errors() {
        let err = parse("sync_models = [42]").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bare type paths"),
            "error must call out the type-path requirement: {msg}"
        );
    }

    #[test]
    fn sync_models_call_expression_element_errors() {
        let err = parse("sync_models = [some_fn()]").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bare type paths"),
            "error must call out the type-path requirement: {msg}"
        );
    }

    #[test]
    fn sync_models_duplicate_keyword_errors() {
        let err = parse("sync_models = [], sync_models = [Widget]").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("specified more than once"),
            "duplicate-keyword error must call out the repetition: {msg}"
        );
    }

    #[test]
    fn unknown_argument_lists_supported_keys() {
        let err = parse("foo = \"bar\"").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("`extensions = [...]`"),
            "unknown-arg error must mention extensions: {msg}"
        );
        assert!(
            msg.contains("`sync_models = [...]`"),
            "unknown-arg error must mention sync_models: {msg}"
        );
    }

    // ── emission order ──────────────────────────────────────────────

    #[test]
    fn sync_models_empty_array_emits_no_call() {
        // Empty `sync_models = []` round-trips as a zero-DDL no-op:
        // the generated wrapper must NOT contain a `sync_models` call.
        let expanded = render_expansion("sync_models = []");
        assert!(
            !expanded.contains("sync_models"),
            "empty `sync_models = []` must not emit a `sync_models` call; got:\n{expanded}"
        );
        // The setup helper must still be there.
        assert!(
            expanded.contains("setup_test_db_with_extensions"),
            "setup helper must always be emitted: {expanded}"
        );
    }

    #[test]
    fn sync_models_absent_keyword_emits_no_call() {
        // No `sync_models` keyword at all — preserves pre-T10
        // wrapper shape exactly.
        let expanded = render_expansion("");
        assert!(
            !expanded.contains("sync_models"),
            "no `sync_models` keyword must not emit a `sync_models` call: {expanded}"
        );
    }

    #[test]
    fn sync_models_with_extensions_emission_order() {
        // Both keys present — the wrapper must call
        // `setup_test_db_with_extensions` BEFORE `sync_models` so
        // PostGIS-dependent column types resolve. We assert byte
        // ordering on the rendered TokenStream.
        let expanded = render_expansion(r#"extensions = ["postgis"], sync_models = [Widget]"#);
        let setup_pos = expanded
            .find("setup_test_db_with_extensions")
            .expect("expanded code must contain setup helper");
        let sync_pos = expanded
            .find("sync_models")
            .expect("expanded code must contain sync_models call");
        assert!(
            setup_pos < sync_pos,
            "setup_test_db_with_extensions must precede sync_models in the emitted wrapper:\n\
             setup_pos = {setup_pos}, sync_pos = {sync_pos}\n\
             expanded =\n{expanded}",
        );
    }

    #[test]
    fn sync_models_emits_descriptor_call_per_path() {
        // Each entry in `sync_models = [A, B, C]` becomes one
        // `<Path as ::djogi::Model>::descriptor()` expression in the
        // generated slice. The `descriptor` literal repeating once
        // per path is the simplest stable signal we can assert on.
        let expanded = render_expansion("sync_models = [Widget, Category, Tag]");
        let count = expanded.matches("descriptor").count();
        // Each path emits exactly one `descriptor()` call.
        assert!(
            count >= 3,
            "expected at least 3 `descriptor()` calls (one per path), got {count}: {expanded}"
        );
    }

    #[test]
    fn sync_models_uses_mut_ctx_binding() {
        // When `sync_models` is non-empty, the wrapper must bind the
        // setup return as `let (cleanup, mut ctx)` so the runtime
        // helper can take `&mut ctx`. Without `sync_models`, the
        // pre-T10 immutable `let (cleanup, ctx)` is preserved so
        // `unused_mut` does not fire under `-D warnings`.
        //
        // Note that the inner async fn's parameter is always
        // `mut ctx: DjogiContext` (forwarded verbatim from the
        // user's test fn signature in our render fixture), so a
        // global "contains mut ctx" check is too coarse. We assert
        // on the specific destructure pattern instead.
        let with_sync = render_expansion("sync_models = [Widget]");
        let without_sync = render_expansion("");
        assert!(
            with_sync.contains("(cleanup , mut ctx)"),
            "sync_models path must bind ctx as mut in the setup destructure: {with_sync}"
        );
        assert!(
            !without_sync.contains("(cleanup , mut ctx)"),
            "no-sync_models path must keep the setup-destructure ctx immutable \
             to avoid unused_mut: {without_sync}"
        );
        assert!(
            without_sync.contains("(cleanup , ctx)"),
            "no-sync_models path must still destructure (cleanup, ctx): {without_sync}"
        );
    }
}
