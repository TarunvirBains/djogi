//! Generates `impl FromPgRow for T` — canonical row-decode.
//! # What
//! Emits one `impl` block per `#[model]`-annotated struct:
//! - `const COLUMNS: &'static [&'static str]` — column names in the
//!   canonical SELECT order (`id`, `created_at`, `updated_at`, then
//!   user fields in declaration order).
//! - `const COLUMN_LIST: &'static str` — the same names joined with
//!   `", "`, ready to interpolate into `SELECT {COLUMN_LIST} FROM t`
//!   and `RETURNING {COLUMN_LIST}` SQL text.
//! - `fn from_pg_row(row: &tokio_postgres::Row) -> Result<Self,
//! DjogiError>` — positional (ordinal) decode via `row.try_get(0)`,
//!   `row.try_get(1)`, … matching the `COLUMNS` order.
//! # Why ordinal, not name-based
//! Ordinal decode is O(N) per row (one `try_get(i)` per column);
//! name-based decode is O(N^2) (each `try_get(name)` does a linear
//! scan through `row.columns()`). For the typical `#[model]`
//! deriver — three framework fields plus a handful of user fields
//! the quadratic term is small but real, and CRUD call paths are
//! hot. The CRUD and QuerySet terminals now bake
//! `{Self::COLUMN_LIST}` into `SELECT` / `RETURNING` clauses, so the
//! wire order is always struct-field order and ordinal decode is
//! sound.
//! # Debug-build drift guard
//! Every `try_get(i)` is preceded by
//! `debug_assert_eq!(row.columns()[i].name(), Self::COLUMNS[i])`. If
//! a caller hand-rolls a SELECT that doesn't match `COLUMN_LIST`, or
//! if a future refactor reshapes the builder, the assert fires in
//! `cargo test` (which runs in debug mode by default). Release builds
//! drop the assert to keep decode a single `try_get(i)` with no
//! per-row overhead.
//! Column names come from field idents (snake_case Rust names match
//! the SQL columns the migration emits). Raw-identifier fields
//! (`r#type`) strip the `r#` prefix — matching the descriptor's
//! convention.
//! # Where
//! Called from `mod.rs` after `inject::expand` has mutated the
//! struct, so the iterator includes `id`, `created_at`, and
//! `updated_at` automatically at the front.

use proc_macro2::TokenStream;
use quote::quote;
use syn::ItemStruct;

use super::attrs::{FieldAttrs, ModelAttrs, PkStrategy};
use super::sql_bind::{bind_kind, decode_field_tokens, is_nullable, is_tracked_inner};

/// Emit `impl FromPgRow for <Struct>` — the canonical row-decode
/// contract used by every CRUD terminal and QuerySet terminal.
/// `model_attrs` provides pk strategy info so we can determine how many
/// framework fields precede user fields. `field_attrs` carries per-field
/// metadata including codec information for protected fields with encryption.
pub fn expand(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    field_attrs: &[FieldAttrs],
) -> TokenStream {
    let name = &struct_item.ident;
    let (impl_generics, ty_generics, where_clause) = struct_item.generics.split_for_impl();

    // Column names — field ident -> SQL column (strip raw-identifier
    // prefix `r#` so `r#type` serializes to `"type"`). Matches the
    // convention `stubs.rs` / `descriptor.rs` already use.
    let col_names: Vec<String> = struct_item
        .fields
        .iter()
        .map(crate::syn_util::column_name_from_field)
        .collect();

    let n_cols = col_names.len();
    // COLUMN_LIST is the comma-joined form, baked at macro time. A
    // single `", ".join(...)` walk here avoids per-call allocation at
    // runtime.
    let column_list: String = col_names.join(", ");

    // Per-column decode token: delegates to `decode_at` for direct types
    // and to the appropriate `decode_narrowed` / `decode_u64_from_decimal`
    // variant for widened types (i8/u8 → i16, u16 → i32, u32 → i64,
    // u64 → Decimal). The `sql_bind` module determines the bind kind from
    // the field's Rust type; the debug-build column-name guard runs on the
    // wide read inside each helper.
    //
    // Build a codec vector aligned with ALL struct fields (None for framework,
    // Some/None for users). `field_attrs` is user-only but `struct_item.fields`
    // includes framework fields at the front, so we can't zip them directly.
    let n_framework = match model_attrs.pk {
        PkStrategy::None => 2, // created_at, updated_at only
        _ => 3,                // id, created_at, updated_at
    };
    let codec_vec: Vec<Option<(String, String, String)>> = struct_item
        .fields
        .iter()
        .enumerate()
        .map(|(idx, field)| {
            if idx < n_framework {
                None // Framework fields have no codec.
            } else {
                let fa_idx = idx - n_framework;
                field_attrs
                    .get(fa_idx)
                    .and_then(|fa| fa.protected.as_ref().and_then(|p| p.codec.clone()))
                    .map(|codec_id| {
                        let col_name = crate::syn_util::column_name_from_field(field);
                        (codec_id, name.to_string(), col_name)
                    })
            }
        })
        .collect();

    let field_assignments: Vec<TokenStream> = struct_item
        .fields
        .iter()
        .zip(col_names.iter())
        .enumerate()
        .map(|(i, (field, col_name))| {
            let fname = field.ident.as_ref().expect("only named structs supported");
            let kind = bind_kind(&field.ty);
            let nullable = is_nullable(&field.ty);
            let tracked = is_tracked_inner(&field.ty);
            let codec = codec_vec[i].clone();
            let decode_expr = decode_field_tokens(&kind, nullable, tracked, i, col_name, codec);
            quote! {
                #fname: #decode_expr
            }
        })
        .collect();

    // `COLUMNS` needs a typed slice literal for the const. Build it
    // from the collected names.
    let columns_lit = col_names.iter().map(|n| quote! { #n });

    quote! {
        impl #impl_generics ::djogi::__private::pg::FromPgRow
        for #name #ty_generics #where_clause
        {
            const COLUMNS: &'static [&'static str] = &[
                #(#columns_lit,)*
            ];

            const COLUMN_LIST: &'static str = #column_list;

            fn from_pg_row(
                row: &::djogi::__private::tokio_postgres::Row,
            ) -> ::std::result::Result<Self, ::djogi::DjogiError> {
                // Row must have at least `N_COLS` columns in canonical
                // order at positions 0..N_COLS. Callers that add extra
                // trailing columns (e.g. `annotate()` appending aggregate
                // aliases, or `select_related` appending aliased joined
                // columns) are allowed — the trailing columns are simply
                // ignored by this decoder and fielded by their own decode
                // paths (`AggregateTuple`, `FromJoinedPgRow`).
                // Release builds skip the asserts entirely; the
                // `try_get(i)` at each ordinal position still errors
                // with a typed decode error if the wire type doesn't
                // match, so misuse does not corrupt data — it just
                // loses the up-front panic diagnostics.
                ::std::debug_assert!(
                    ::djogi::__private::tokio_postgres::Row::columns(row).len() >= #n_cols,
                    "FromPgRow column-count mismatch: expected at least {}, got {}",
                    #n_cols,
                    ::djogi::__private::tokio_postgres::Row::columns(row).len(),
                );
                ::std::result::Result::Ok(Self {
                    #(#field_assignments,)*
                })
            }
        }
    }
}
