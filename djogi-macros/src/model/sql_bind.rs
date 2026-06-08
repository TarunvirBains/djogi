//! Bind and decode helpers for narrow / unsigned integer fields.
//! # What
//! `tokio-postgres` (via `postgres-types`) does not implement `ToSql` /
//! `FromSql` for `u8`, `u16`, `u64`. For `i8` and `u32`, it does have
//! impls, but they map to the *wrong* Postgres types for djogi's column
//! definitions (`i8 → postgres "char"`, `u32 → OID`). djogi models these
//! five types with different SQL carriers:
//! | Rust type | SQL carrier | Wire type (bind) | Wire type (decode) |
//! |-----------|---------------------|------------------|--------------------|
//! | `i8` | `SMALLINT` | `i16` | `i16 → i8` |
//! | `u8` | `SMALLINT` | `i16` | `i16 → u8` |
//! | `u16` | `INTEGER` | `i32` | `i32 → u16` |
//! | `u32` | `BIGINT` | `i64` | `i64 → u32` |
//! | `u64` | `NUMERIC` | `Decimal` | `Decimal → u64` |
//! This module provides token-stream helpers consumed by `crud.rs` and
//! `from_row.rs` to emit these shims without duplicating the logic at
//! every bind / decode site.

use proc_macro2::TokenStream;
use quote::quote;
use syn::Type;

use super::attrs::unwrap_schema_type;

/// How a Rust field type binds to tokio-postgres for CRUD operations.
/// Determined by [`bind_kind`] from the raw (possibly `Option<>`/`Tracked<>`
/// -wrapped) field type. The variants carry the widening conversion needed
/// before a bind, or signal that no conversion is required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindKind {
    /// The Rust type implements `ToSql` for the correct Postgres type directly.
    /// All standard types (`i16`, `i32`, `i64`, `f32`, `f64`, `bool`,
    /// `String`, `DateTime`, `Decimal`, `Uuid`, `HeerId`, etc.) and their
    /// `Vec<T>` / `Option<T>` / `Tracked<T>` combinations land here.
    Direct,
    /// `i8` or `u8` → widen to `i16` before binding.
    WidenToI16,
    /// `u16` → widen to `i32` before binding.
    WidenToI32,
    /// `u32` → widen to `i64` before binding.
    WidenToI64,
    /// `u64` → widen to `rust_decimal::Decimal` before binding.
    WidenToDecimal,
}

/// Determine the bind kind for a field type (including `Option<T>` / `Tracked<T>` wrappers).
/// Strips `Option<T>` and `Tracked<T>` wrappers via [`unwrap_schema_type`]
/// before checking the inner type. The nullability information from `Option`
/// is NOT carried in `BindKind`; callers that need it should call
/// [`unwrap_schema_type`] themselves.
pub fn bind_kind(ty: &Type) -> BindKind {
    // Strip Option<T> and Tracked<T> to get the innermost type.
    let (inner, _nullable) = unwrap_schema_type(ty);
    // Normalise whitespace and strip an optional leading `::`.
    let s = quote::quote!(#inner).to_string().replace(' ', "");
    let s = s.strip_prefix("::").unwrap_or(&s);
    match s {
        "i8" | "u8" => BindKind::WidenToI16,
        "u16" => BindKind::WidenToI32,
        "u32" => BindKind::WidenToI64,
        "u64" => BindKind::WidenToDecimal,
        _ => BindKind::Direct,
    }
}

/// Whether a field type is `Option<T>` at the outermost level (after stripping `Tracked`).
/// Used to pick the correct widening expression for nullable widened fields.
pub fn is_nullable(ty: &Type) -> bool {
    let (_inner, nullable) = unwrap_schema_type(ty);
    nullable
}

/// Whether a field type is `Tracked<T>` at the outermost (or Option-inner) level.
pub fn is_tracked_inner(ty: &Type) -> bool {
    // Check both `Tracked<T>` and `Option<Tracked<T>>` shapes.
    fn inner_is_tracked(ty: &Type) -> bool {
        let syn::Type::Path(syn::TypePath { path, .. }) = ty else {
            return false;
        };
        path.segments
            .last()
            .map(|seg| seg.ident == "Tracked")
            .unwrap_or(false)
    }

    // If option-wrapped, inspect the inner type.
    use super::attrs::unwrap_option;
    let (after_option, _was_option) = unwrap_option(ty);
    inner_is_tracked(&after_option)
}

/// Map a codec ID string to the fully-qualified type path of the
/// corresponding [`FieldCodec`](::djogi::field_codec::FieldCodec) impl.
///
/// This is the macro-side mirror of `::djogi::field_codec::REGISTRY`.
/// The returned path routes through `::djogi::__private::field_codec_aes`
/// so it resolves from the adopter crate (the `aes` submodule is
/// `pub(crate)` and invisible to external crates). Always prefixed with
/// `::djogi` so it resolves without a `use` statement.
pub fn codec_id_to_type_path(codec_id: &str) -> TokenStream {
    match codec_id {
        "aes256_gcm_v1" => quote! { ::djogi::__private::field_codec_aes::Aes256GcmV1 },
        _ => unreachable!(
            "codec_id_to_type_path called with unknown codec \"{codec_id}\"; \
             the macro should have validated this against KNOWN_CODEC_IDS at parse time"
        ),
    }
}

/// Emit `push_bind(...)` tokens for a `SqlAccumulator` bind site.
/// `field_expr` is the token stream that evaluates to the field's Rust value
/// an **owned** value of the field's declared Rust type (e.g. `row.count`
/// in bulk paths, `self.count.clone()` in the save path, etc.).
/// `tracked` indicates whether the field type is `Tracked<T>` (or
/// `Option<Tracked<T>>`). For widened types inside `Tracked`, the emitted
/// code extracts the inner value via `(*field_expr).clone()` before widening.
/// For direct types, `Tracked<T>` implements `ToSql where T: ToSql`, so no
/// extraction is needed.
///
/// When `codec` is `Some((codec_id, model_name, field_name))`, the field has a
/// `#[field(protected(codec = "..."))]` annotation. The emitted code calls
/// `CodecType::encode(model_name, field_name, &value)?` to produce an encoded
/// at-rest representation before binding. The model and field names supply
/// AAD (additional authenticated data) for AEAD codecs, preventing ciphertext
/// replay across different fields or models.
///
/// For widened types the emitted tokens perform the widening conversion before
/// calling `push_bind`. For direct types the field expression is passed
/// through unchanged (unless a codec intercepts it).
pub fn push_bind_tokens(
    kind: &BindKind,
    nullable: bool,
    tracked: bool,
    field_expr: TokenStream,
    codec: Option<(String, String, String)>,
) -> TokenStream {
    // Codec path: encode before binding. The codec output type is Vec<u8>
    // (or Option<Vec<u8>> for nullable), which is BindKind::Direct, so widening
    // logic is irrelevant when a codec is present.
    // Tuple is (codec_id, model_name, field_name) — all owned Strings.
    if let Some((codec_id, model_name, field_name)) = codec {
        let codec_ty = codec_id_to_type_path(&codec_id);
        let model_lit = syn::LitStr::new(&model_name, proc_macro2::Span::call_site());
        let field_lit = syn::LitStr::new(&field_name, proc_macro2::Span::call_site());

        if nullable {
            // Option<T> with codec: map through the Option, encode each Some.
            return quote! {
                __acc.push_bind(
                    #field_expr.map(|__v| {
                        <#codec_ty as ::djogi::field_codec::FieldCodec>::encode(
                            #model_lit,
                            #field_lit,
                            &__v,
                        )
                        .map_err(|__e| {
                            ::djogi::DjogiError::field_codec_encode(
                                #model_lit,
                                #field_lit,
                                <#codec_ty as ::djogi::field_codec::FieldCodec>::ID,
                                __e,
                            )
                        })?
                    })
                )
            };
        } else {
            // Non-nullable with codec: encode then bind.
            return quote! {
                __acc.push_bind({
                    <#codec_ty as ::djogi::field_codec::FieldCodec>::encode(
                        #model_lit,
                        #field_lit,
                        &#field_expr,
                    )
                    .map_err(|__e| {
                        ::djogi::DjogiError::field_codec_encode(
                            #model_lit,
                            #field_lit,
                            <#codec_ty as ::djogi::field_codec::FieldCodec>::ID,
                            __e,
                        )
                    })?
                })
            };
        }
    }

    // For direct types (no codec), Tracked<T>: ToSql handles the wrapping automatically.
    if matches!(kind, BindKind::Direct) {
        return quote! { __acc.push_bind(#field_expr) };
    }

    // For widened types, extract the inner value from Tracked first.
    let effective = if tracked && !nullable {
        // Tracked<T>: `(*field_expr).clone()` applies `Tracked`'s Deref impl
        // (`Tracked<T>: Deref<Target=T>`) to reach the inner `T`, then clones it.
        // `field_expr` is an owned `Tracked<T>`, so one deref suffices.
        quote! { (*#field_expr).clone() }
    } else if tracked && nullable {
        // Option<Tracked<T>>: extract `Option<T>` by mapping through two derefs.
        // `field_expr.as_ref()` → `Option<&Tracked<T>>`.
        // In the closure `__t: &Tracked<T>`:
        // - `*__t` → `Tracked<T>` (deref the `&` reference)
        // - `**__t` → `T` (deref via `Tracked<T>: Deref<Target=T>`)
        // Using `(*__t).clone()` would clone `Tracked<T>` (wrong);
        // `(**__t).clone()` clones the inner `T` (correct).
        quote! { #field_expr.as_ref().map(|__t| (**__t).clone()) }
    } else {
        field_expr
    };

    match (kind, nullable) {
        (BindKind::Direct, _) => unreachable!("handled above"),

        (BindKind::WidenToI16, false) => {
            quote! { __acc.push_bind(i16::from(#effective)) }
        }
        (BindKind::WidenToI16, true) => {
            quote! { __acc.push_bind(#effective.map(i16::from)) }
        }

        (BindKind::WidenToI32, false) => {
            quote! { __acc.push_bind(i32::from(#effective)) }
        }
        (BindKind::WidenToI32, true) => {
            quote! { __acc.push_bind(#effective.map(i32::from)) }
        }

        (BindKind::WidenToI64, false) => {
            quote! { __acc.push_bind(i64::from(#effective)) }
        }
        (BindKind::WidenToI64, true) => {
            quote! { __acc.push_bind(#effective.map(i64::from)) }
        }

        (BindKind::WidenToDecimal, false) => {
            quote! {
                __acc.push_bind(
                    ::djogi::__private::rust_decimal::Decimal::from(#effective)
                )
            }
        }
        (BindKind::WidenToDecimal, true) => {
            quote! {
                __acc.push_bind(
                    #effective.map(::djogi::__private::rust_decimal::Decimal::from)
                )
            }
        }
    }
}

/// Emit a widened-temporary declaration + slice-entry token pair for the
/// `create` / `create_with_id` `&[&(dyn ToSql + Sync)]` params slice.
/// The `create` path builds a `&[&(dyn ToSql + Sync)]` slice before the
/// INSERT. Because borrows in slice items must outlive the slice expression
/// itself, widened temporaries must be declared as named `let` bindings
/// **before** the slice literal.
/// Returns `(pre_decl, entry)` where:
/// - `pre_decl` — a `let __bind_<slot>: WideType = widen(val);` statement,
///   or the empty token stream for direct types.
/// - `entry` — `&__bind_<slot> as &(dyn ToSql + Sync)` for widened types,
///   or `&(val_expr) as &(dyn ToSql + Sync)` for direct types.
///   `slot` is the zero-based index of the field in the user-field list,
///   used to generate a unique local binding name that does not clash across
///   fields.
///   `val_expr` evaluates to an **owned** copy of the field value (e.g.
///   `value.count` or `value.count.clone()`).
pub fn create_param_tokens(
    kind: &BindKind,
    nullable: bool,
    tracked: bool,
    val_expr: TokenStream,
    slot: usize,
) -> (TokenStream, TokenStream) {
    let bind_name_str = format!("__bind_{slot}");
    let bind_name: syn::Ident = syn::Ident::new(&bind_name_str, proc_macro2::Span::call_site());

    // Expr to extract the field value, unwrapping Tracked<T> if needed.
    // `tracked=true, nullable=false` — `Tracked<T>`: one deref via Tracked's
    // `Deref<Target=T>` impl gives the inner `T`. `(*val_expr).clone()` works
    // because `val_expr` is an owned `Tracked<T>` value, not a reference.
    // `tracked=true, nullable=true` — `Option<Tracked<T>>`: two derefs are needed.
    // `.as_ref()` gives `Option<&Tracked<T>>`; in the closure `__t: &Tracked<T>`:
    // - `*__t` = `Tracked<T>` (deref the `&` reference — first deref)
    // - `**__t` = `T` (via `Tracked<T>: Deref<Target=T>` — second deref)
    // `(*__t).clone()` would clone `Tracked<T>` (wrong — the widening
    // conversion then fails: e.g. `i32::from(Tracked<u16>)` is not implemented);
    // `(**__t).clone()` clones the inner `T` (correct).
    let extract = if tracked && !nullable {
        // Tracked<T>: (*value.field).clone() → T
        quote! { (*#val_expr).clone() }
    } else if tracked && nullable {
        // Option<Tracked<T>>: map through to get Option<T>.
        quote! { #val_expr.as_ref().map(|__t| (**__t).clone()) }
    } else {
        val_expr.clone()
    };

    match (kind, nullable) {
        (BindKind::Direct, _) => {
            // No temporary needed — bind the field directly.
            let entry = quote! {
                &#val_expr as &(dyn ::djogi::__private::postgres_types::ToSql + Sync)
            };
            (TokenStream::new(), entry)
        }

        (BindKind::WidenToI16, false) => {
            let pre = quote! { let #bind_name: i16 = i16::from(#extract); };
            let entry =
                quote! { &#bind_name as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) };
            (pre, entry)
        }
        (BindKind::WidenToI16, true) => {
            let pre = quote! { let #bind_name: Option<i16> = #extract.map(i16::from); };
            let entry =
                quote! { &#bind_name as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) };
            (pre, entry)
        }

        (BindKind::WidenToI32, false) => {
            let pre = quote! { let #bind_name: i32 = i32::from(#extract); };
            let entry =
                quote! { &#bind_name as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) };
            (pre, entry)
        }
        (BindKind::WidenToI32, true) => {
            let pre = quote! { let #bind_name: Option<i32> = #extract.map(i32::from); };
            let entry =
                quote! { &#bind_name as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) };
            (pre, entry)
        }

        (BindKind::WidenToI64, false) => {
            let pre = quote! { let #bind_name: i64 = i64::from(#extract); };
            let entry =
                quote! { &#bind_name as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) };
            (pre, entry)
        }
        (BindKind::WidenToI64, true) => {
            let pre = quote! { let #bind_name: Option<i64> = #extract.map(i64::from); };
            let entry =
                quote! { &#bind_name as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) };
            (pre, entry)
        }

        (BindKind::WidenToDecimal, false) => {
            let pre = quote! {
                let #bind_name: ::djogi::__private::rust_decimal::Decimal =
                    ::djogi::__private::rust_decimal::Decimal::from(#extract);
            };
            let entry =
                quote! { &#bind_name as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) };
            (pre, entry)
        }
        (BindKind::WidenToDecimal, true) => {
            let pre = quote! {
                let #bind_name: Option<::djogi::__private::rust_decimal::Decimal> =
                    #extract.map(::djogi::__private::rust_decimal::Decimal::from);
            };
            let entry =
                quote! { &#bind_name as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) };
            (pre, entry)
        }
    }
}

/// Emit the decode tokens for a single field in `FromPgRow::from_pg_row`.
/// For direct types without a codec, delegates to
/// `::djogi::__private::pg::decode_at::<_>` (the existing helper).
/// For widened types without a codec, delegates to the appropriate
/// `decode_narrowed` / `decode_u64_from_decimal` variant — all live in
/// `::djogi::__private::pg`.
/// When `codec` is `Some((codec_id, model_name, field_name))`, the field has
/// a `#[field(protected(codec = "..."))]` annotation. The emitted code reads
/// the stored bytes (`Vec<u8>`) from Postgres and calls
/// `CodecType::decode(model_name, field_name, &bytes)?` to produce the
/// decoded in-memory value. The codec's `Encoded` type is always `Vec<u8>`;
/// widening logic is irrelevant when a codec is present (codec overrides
/// the bind kind).
/// The `tracked` flag controls whether the decoded value is wrapped in
/// `::djogi::Tracked::new(...)`:
/// - `tracked=false, nullable=false` → value directly (e.g. `u8`)
/// - `tracked=false, nullable=true` → `Option<T>` (e.g. `Option<u8>`)
/// - `tracked=true, nullable=false` → `Tracked::new(value)` (e.g. `Tracked<u8>`)
/// - `tracked=true, nullable=true` → `option.map(Tracked::new)` (e.g. `Option<Tracked<u8>>`)
///   `col_name` is a `&'static str` literal baked at macro time;
///   `col_idx` is the ordinal position in the SELECT column list.
pub fn decode_field_tokens(
    kind: &BindKind,
    nullable: bool,
    tracked: bool,
    col_idx: usize,
    col_name: &str,
    codec: Option<(String, String, String)>,
) -> TokenStream {
    // Codec path: read stored bytes and decode via the codec.
    // The codec's `Encoded` type is always `Vec<u8>` (BYTEA in Postgres),
    // so widening logic is irrelevant — codec always reads as BYTEA regardless
    // of the original Rust field type.
    if let Some((codec_id, model_name, field_name)) = codec {
        let codec_ty = codec_id_to_type_path(&codec_id);
        let model_lit = syn::LitStr::new(&model_name, proc_macro2::Span::call_site());
        let field_lit = syn::LitStr::new(&field_name, proc_macro2::Span::call_site());
        let col_name_lit = syn::LitStr::new(col_name, proc_macro2::Span::call_site());

        // Helper to wrap raw postgres decode errors into DjogiError::Decode.
        let decode_err_wrap = quote! {
            |__e| ::djogi::DjogiError::Decode(
                ::std::format!("column `{}`: {}", #col_name_lit, __e)
            )
        };

        let decoded = if nullable {
            // Option<T> with codec: read Option<Vec<u8>>, map through decode.
            quote! {
                row.try_get::<_, ::std::option::Option<Vec<u8>>>(#col_idx)
                    .map_err(#decode_err_wrap)?
                    .map(|__bytes| {
                        <#codec_ty as ::djogi::field_codec::FieldCodec>::decode(
                            #model_lit,
                            #field_lit,
                            &__bytes,
                        )
                    })
                    .transpose()
                    .map_err(|__e| {
                        ::djogi::DjogiError::field_codec_decode(
                            #model_lit,
                            #field_lit,
                            <#codec_ty as ::djogi::field_codec::FieldCodec>::ID,
                            __e.to_string(),
                        )
                    })?
            }
        } else {
            // Non-nullable with codec: read Vec<u8>, decode.
            quote! {
                <#codec_ty as ::djogi::field_codec::FieldCodec>::decode(
                    #model_lit,
                    #field_lit,
                    &row.try_get::<_, Vec<u8>>(#col_idx)
                        .map_err(#decode_err_wrap)?,
                )
                .map_err(|__e| {
                    ::djogi::DjogiError::field_codec_decode(
                        #model_lit,
                        #field_lit,
                        <#codec_ty as ::djogi::field_codec::FieldCodec>::ID,
                        __e.to_string(),
                    )
                })?
            }
        };

        // Codec decode always returns `Codec::Decoded` (not Tracked<Decoded>).
        // Wrap in Tracked::new when the field is tracked.
        if tracked {
            if nullable {
                return quote! {
                    { let __v = #decoded; __v.map(::djogi::Tracked::new) }
                };
            } else {
                return quote! { ::djogi::Tracked::new(#decoded) };
            }
        }
        return decoded;
    }

    // No codec — emit the standard widened/direct decode path.
    let col_name_lit = syn::LitStr::new(col_name, proc_macro2::Span::call_site());

    // Emit the raw decode expression (without Tracked wrapping).
    let raw = match (kind, nullable) {
        (BindKind::Direct, _) => {
            quote! {
                ::djogi::__private::pg::decode_at::<_>(row, #col_idx, #col_name_lit)?
            }
        }

        (BindKind::WidenToI16, false) => quote! {
            ::djogi::__private::pg::decode_narrowed::<i16, _>(row, #col_idx, #col_name_lit)?
        },
        (BindKind::WidenToI16, true) => quote! {
            ::djogi::__private::pg::decode_narrowed_opt::<i16, _>(row, #col_idx, #col_name_lit)?
        },

        (BindKind::WidenToI32, false) => quote! {
            ::djogi::__private::pg::decode_narrowed::<i32, _>(row, #col_idx, #col_name_lit)?
        },
        (BindKind::WidenToI32, true) => quote! {
            ::djogi::__private::pg::decode_narrowed_opt::<i32, _>(row, #col_idx, #col_name_lit)?
        },

        (BindKind::WidenToI64, false) => quote! {
            ::djogi::__private::pg::decode_narrowed::<i64, _>(row, #col_idx, #col_name_lit)?
        },
        (BindKind::WidenToI64, true) => quote! {
            ::djogi::__private::pg::decode_narrowed_opt::<i64, _>(row, #col_idx, #col_name_lit)?
        },

        (BindKind::WidenToDecimal, false) => quote! {
            ::djogi::__private::pg::decode_u64_from_decimal(row, #col_idx, #col_name_lit)?
        },
        (BindKind::WidenToDecimal, true) => quote! {
            ::djogi::__private::pg::decode_opt_u64_from_decimal(row, #col_idx, #col_name_lit)?
        },
    };

    // For direct types, `decode_at::<Tracked<T>>` already handles
    // the `Tracked` wrapping (because `Tracked<T>: FromSql where T: FromSql`).
    // Only widened types need explicit `Tracked::new(...)` wrapping after decode.
    if matches!(kind, BindKind::Direct) {
        return raw;
    }

    // Widened type: the decode helper returns the narrow type `N` (not
    // `Tracked<N>`). Wrap in `Tracked::new` when the field is Tracked.
    match (tracked, nullable) {
        (false, _) => {
            // Not Tracked: return decoded value directly.
            raw
        }
        (true, false) => {
            // Tracked<N>: decode N, then wrap.
            quote! { ::djogi::Tracked::new(#raw) }
        }
        (true, true) => {
            // Option<Tracked<N>>: decode Option<N>, then map each Some to
            // Tracked::new.
            quote! { { let __v = #raw; __v.map(::djogi::Tracked::new) } }
        }
    }
}

/// Emit decode tokens for the `FromJoinedPgRow` name-based path.
/// `FromJoinedPgRow` decodes columns by `"{prefix}{col_name}"` strings rather
/// than by ordinal index. For widened types, delegates to the appropriate
/// `decode_narrowed_by_name` / `decode_u64_from_decimal_by_name` variant.
/// For direct types, uses `row.try_get::<_, _>(col_name_expr)` as before.
/// When `codec` is `Some((codec_id, model_name, field_name))`, reads the
/// stored bytes and calls `CodecType::decode()` — codec always overrides
/// the bind kind (codec output is Vec<u8> / BYTEA).
/// `col_name_expr` is a token stream that evaluates to `&str` — typically
/// `&format!("{}{}", prefix, "col_name")` or similar.
pub fn decode_joined_field_tokens(
    kind: &BindKind,
    nullable: bool,
    tracked: bool,
    col_name_expr: TokenStream,
    codec: Option<(String, String, String)>,
) -> TokenStream {
    // Codec path: read stored bytes and decode via the codec.
    if let Some((codec_id, model_name, field_name)) = codec {
        let codec_ty = codec_id_to_type_path(&codec_id);
        let model_lit = syn::LitStr::new(&model_name, proc_macro2::Span::call_site());
        let field_lit = syn::LitStr::new(&field_name, proc_macro2::Span::call_site());

        let decoded = if nullable {
            quote! {
                row.try_get::<_, ::std::option::Option<Vec<u8>>>(#col_name_expr)
                    .map_err(|__e| ::djogi::DjogiError::Decode(
                        ::std::format!("column `{}: {}", #col_name_expr, __e)
                    ))?
                    .map(|__bytes| {
                        <#codec_ty as ::djogi::field_codec::FieldCodec>::decode(
                            #model_lit,
                            #field_lit,
                            &__bytes,
                        )
                    })
                    .transpose()
                    .map_err(|__e| {
                        ::djogi::DjogiError::field_codec_decode(
                            #model_lit,
                            #field_lit,
                            <#codec_ty as ::djogi::field_codec::FieldCodec>::ID,
                            __e.to_string(),
                        )
                    })?
            }
        } else {
            quote! {
                <#codec_ty as ::djogi::field_codec::FieldCodec>::decode(
                    #model_lit,
                    #field_lit,
                    &row.try_get::<_, Vec<u8>>(#col_name_expr)
                        .map_err(|__e| ::djogi::DjogiError::Decode(
                            ::std::format!("column `{}: {}", #col_name_expr, __e)
                        ))?,
                )
                .map_err(|__e| {
                    ::djogi::DjogiError::field_codec_decode(
                        #model_lit,
                        #field_lit,
                        <#codec_ty as ::djogi::field_codec::FieldCodec>::ID,
                        __e.to_string(),
                    )
                })?
            }
        };

        if tracked {
            if nullable {
                return quote! {
                    { let __v = #decoded; __v.map(::djogi::Tracked::new) }
                };
            } else {
                return quote! { ::djogi::Tracked::new(#decoded) };
            }
        }
        return decoded;
    }

    let raw = match (kind, nullable) {
        (BindKind::Direct, _) => {
            quote! {
                row.try_get::<_, _>(#col_name_expr)
                    .map_err(|__e| ::djogi::DjogiError::Decode(
                        ::std::format!("column `{}: {}", #col_name_expr, __e)
                    ))?
            }
        }

        (BindKind::WidenToI16, false) => quote! {
            ::djogi::__private::pg::decode_narrowed_by_name::<i16, _>(row, #col_name_expr)?
        },
        (BindKind::WidenToI16, true) => quote! {
            ::djogi::__private::pg::decode_narrowed_opt_by_name::<i16, _>(row, #col_name_expr)?
        },

        (BindKind::WidenToI32, false) => quote! {
            ::djogi::__private::pg::decode_narrowed_by_name::<i32, _>(row, #col_name_expr)?
        },
        (BindKind::WidenToI32, true) => quote! {
            ::djogi::__private::pg::decode_narrowed_opt_by_name::<i32, _>(row, #col_name_expr)?
        },

        (BindKind::WidenToI64, false) => quote! {
            ::djogi::__private::pg::decode_narrowed_by_name::<i64, _>(row, #col_name_expr)?
        },
        (BindKind::WidenToI64, true) => quote! {
            ::djogi::__private::pg::decode_narrowed_opt_by_name::<i64, _>(row, #col_name_expr)?
        },

        (BindKind::WidenToDecimal, false) => quote! {
            ::djogi::__private::pg::decode_u64_from_decimal_by_name(row, #col_name_expr)?
        },
        (BindKind::WidenToDecimal, true) => quote! {
            ::djogi::__private::pg::decode_opt_u64_from_decimal_by_name(row, #col_name_expr)?
        },
    };

    // For direct types, no Tracked wrapping needed (Tracked<T>: FromSql handles it).
    if matches!(kind, BindKind::Direct) {
        return raw;
    }

    // Widened types need explicit Tracked wrapping.
    match (tracked, nullable) {
        (false, _) => raw,
        (true, false) => quote! { ::djogi::Tracked::new(#raw) },
        (true, true) => quote! { { let __v = #raw; __v.map(::djogi::Tracked::new) } },
    }
}

/// Emit the `rust_source_type` token for a specific Rust type string.
/// Unlike [`rust_source_type_tokens`], this function discriminates between
/// `i8` and `u8` (both widen to `i16` but carry different `RustSourceType`
/// variants for the CHECK projection).
/// `Decimal` / `rust_decimal::Decimal` returns `Some(RustSourceType::Decimal)`
/// even though the bind/decode path is `BindKind::Direct` (no shim)
/// the discriminator is read by the projection layer to emit a
/// structural CHECK enforcing rust_decimal's 96-bit-mantissa / scale-≤-28
/// representable range. .
pub fn rust_source_type_tokens_for_type(ty: &Type) -> TokenStream {
    let (inner, _nullable) = unwrap_schema_type(ty);
    let s = quote::quote!(#inner).to_string().replace(' ', "");
    let s = s.strip_prefix("::").unwrap_or(&s);
    match s {
        "i8" => quote! { ::std::option::Option::Some(::djogi::RustSourceType::I8) },
        "u8" => quote! { ::std::option::Option::Some(::djogi::RustSourceType::U8) },
        "u16" => quote! { ::std::option::Option::Some(::djogi::RustSourceType::U16) },
        "u32" => quote! { ::std::option::Option::Some(::djogi::RustSourceType::U32) },
        "u64" => quote! { ::std::option::Option::Some(::djogi::RustSourceType::U64) },
        // adopter `Decimal` columns project a structural CHECK
        // bounded by `rust_decimal::Decimal`'s 96-bit mantissa + scale ≤ 28.
        // No shim is emitted (rust_decimal has a native postgres-types impl);
        // the discriminator only steers the projection layer.
        "Decimal" | "rust_decimal::Decimal" => {
            quote! { ::std::option::Option::Some(::djogi::RustSourceType::Decimal) }
        }
        _ => quote! { ::std::option::Option::None },
    }
}
