//! `djogi::primary_key!` declarative-style macro.
//!
//! Adopters declare a custom PK type in ~4 lines. The macro emits:
//!
//! - the `pub struct <Name>(<Inner>);` newtype with a standard derive set
//!   (`Debug`, `Clone`, `Copy`, `PartialEq`, `Eq`, `Hash`);
//! - `impl ::djogi::primary_key::PrimaryKey for <Name>` with
//!   `KIND = PkType::Custom(CustomPrimaryKeyKind { .. })`, `SQL_TYPE`, and
//!   `DEFAULT_SQL` populated from the declaration attributes;
//! - `ToSql` / `FromSql` delegation to the inner type — the newtype
//!   encodes on the wire exactly as `<Inner>` does;
//! - `impl PrimaryKeyDbGen for <Name>` when `bulk_sql = "..."` is present
//!   — `generate_many` executes `bulk_sql` with the batch count as `$1`
//!   and decodes each row's first column as the inner type;
//! - `impl PrimaryKeyClientGen for <Name>` when `generate = |...| expr`
//!   is present — the emitted body calls the closure expression once per
//!   invocation and wraps the result in the newtype.
//!
//! # Grammar
//!
//! ```ignore
//! djogi::primary_key! {
//!     pub struct MyAppId(i64);
//!     sql_type = "BIGINT";
//!     default_sql = "my_app_id_next()";
//!     bulk_sql = "SELECT id FROM my_app_id_next_many($1)";
//!     // Optional — when present, emits `PrimaryKeyClientGen`:
//!     // generate = || some_client_side_id_generator();
//! }
//! ```
//!
//! `sql_type` and `default_sql` are required. `bulk_sql` is required for
//! DB-backed generators (it is what `generate_many` calls). `generate`
//! opts the type into client-side generation.
//!
//! See `docs/guide/primary-keys.md#custom-pk-types` for the user-facing
//! prose and the "when do I reach for this?" decision tree.

use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Expr, Ident, LitStr, Token, Type, Visibility};

/// Parsed `djogi::primary_key! { ... }` invocation.
///
/// The declaration is semicolon-separated — `struct Name(Inner);` first,
/// then `key = value;` attribute pairs in any order. Each attribute key
/// may appear at most once.
pub struct PrimaryKeyDecl {
    vis: Visibility,
    name: Ident,
    inner: Type,
    sql_type: LitStr,
    default_sql: LitStr,
    bulk_sql: Option<LitStr>,
    generate: Option<Expr>,
}

impl Parse for PrimaryKeyDecl {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let vis: Visibility = input.parse()?;
        input.parse::<Token![struct]>()?;
        let name: Ident = input.parse()?;
        let inner_group;
        syn::parenthesized!(inner_group in input);
        let inner: Type = inner_group.parse()?;
        input.parse::<Token![;]>()?;

        let mut sql_type: Option<LitStr> = None;
        let mut default_sql: Option<LitStr> = None;
        let mut bulk_sql: Option<LitStr> = None;
        let mut generate: Option<Expr> = None;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            if key == "sql_type" {
                if sql_type.is_some() {
                    return Err(syn::Error::new_spanned(
                        &key,
                        "duplicate `sql_type` in djogi::primary_key!",
                    ));
                }
                sql_type = Some(input.parse()?);
            } else if key == "default_sql" {
                if default_sql.is_some() {
                    return Err(syn::Error::new_spanned(
                        &key,
                        "duplicate `default_sql` in djogi::primary_key!",
                    ));
                }
                default_sql = Some(input.parse()?);
            } else if key == "bulk_sql" {
                if bulk_sql.is_some() {
                    return Err(syn::Error::new_spanned(
                        &key,
                        "duplicate `bulk_sql` in djogi::primary_key!",
                    ));
                }
                bulk_sql = Some(input.parse()?);
            } else if key == "generate" {
                if generate.is_some() {
                    return Err(syn::Error::new_spanned(
                        &key,
                        "duplicate `generate` in djogi::primary_key!",
                    ));
                }
                generate = Some(input.parse()?);
            } else {
                return Err(syn::Error::new_spanned(
                    &key,
                    format!(
                        "unknown djogi::primary_key! key `{key}`; \
                             expected one of sql_type / default_sql / bulk_sql / generate"
                    ),
                ));
            }
            // Separator after every attribute. Trailing semicolon is
            // tolerated by the while-loop exit condition.
            input.parse::<Token![;]>()?;
        }

        let sql_type = sql_type.ok_or_else(|| {
            syn::Error::new(
                input.span(),
                "djogi::primary_key! requires `sql_type = \"...\"`",
            )
        })?;
        let default_sql = default_sql.ok_or_else(|| {
            syn::Error::new(
                input.span(),
                "djogi::primary_key! requires `default_sql = \"...\"`",
            )
        })?;

        Ok(Self {
            vis,
            name,
            inner,
            sql_type,
            default_sql,
            bulk_sql,
            generate,
        })
    }
}

/// Expand a `djogi::primary_key! { ... }` invocation to the newtype plus
/// trait impls. The caller (in `lib.rs`) is responsible for converting
/// to/from `proc_macro::TokenStream`.
pub fn expand(input: TokenStream) -> TokenStream {
    let decl = match syn::parse2::<PrimaryKeyDecl>(input) {
        Ok(d) => d,
        Err(e) => return e.to_compile_error(),
    };
    let PrimaryKeyDecl {
        vis,
        name,
        inner,
        sql_type,
        default_sql,
        bulk_sql,
        generate,
    } = decl;
    let name_str = name.to_string();

    // `bulk_create` binds every non-Serial custom PK on
    // `PrimaryKeyDbGen`, so every flavor of `primary_key!` must produce
    // that impl. The body picks a per-batch allocation strategy from the
    // attrs that were actually supplied:
    //
    //   * `bulk_sql` present  → run the user's SQL, bind `$1 = count::i32`,
    //                           length-check the result.
    //   * `generate` present  → loop `PrimaryKeyClientGen::generate_client`
    //                           once per row; zero DB round-trips.
    //   * otherwise           → synthesise
    //                           `SELECT <default_sql> FROM generate_series(1, $1)`
    //                           so the column DEFAULT's generator runs N
    //                           times in one query. Adopters whose
    //                           `default_sql` is a constant literal will
    //                           get N duplicate ids at runtime; they need
    //                           to supply `bulk_sql` or `generate` for
    //                           real `bulk_create` traffic.
    //
    // `generate(ctx)` funnels through `generate_many(ctx, 1)` in all three
    // shapes so there is one code path to audit per flavor, not two.
    let bulk_sql_body = if let Some(sql) = bulk_sql.as_ref() {
        quote! {
            if n == 0 {
                return ::std::result::Result::Ok(::std::vec::Vec::new());
            }
            let count = ::djogi::primary_key::checked_count(n)?;
            let rows = ctx
                .__query_all_for_macros(#sql, &[&count])
                .await?;
            let out: ::std::result::Result<::std::vec::Vec<Self>, ::djogi::DjogiError> = rows
                .into_iter()
                .map(|row| {
                    ::djogi::try_get_scalar::<#inner>(&row, 0).map(Self)
                })
                .collect();
            let out = out?;
            if out.len() != n {
                return ::std::result::Result::Err(
                    ::djogi::primary_key::bulk_row_count_mismatch_err(out.len(), n, "bulk_sql"),
                );
            }
            ::std::result::Result::Ok(out)
        }
    } else if generate.is_some() {
        quote! {
            // Client-gen loop: `generate_client()` wraps the `generate = |...|`
            // expression and produces a single value per call. No DB traffic
            // from the helper macro's side.
            let mut out: ::std::vec::Vec<Self> = ::std::vec::Vec::with_capacity(n);
            for _ in 0..n {
                out.push(<Self as ::djogi::primary_key::PrimaryKeyClientGen>::generate_client());
            }
            ::std::result::Result::Ok(out)
        }
    } else {
        // Default-SQL-only path. `generate_series(1, $1)` yields N rows and
        // the scalar subquery re-evaluates the adopter's `default_sql` per
        // row — the same semantics Postgres applies to a column DEFAULT, just
        // reached via an explicit query so the macro can bind N values.
        let synthesised_sql = format!(
            "SELECT ({default_sql}) AS id FROM generate_series(1, $1)",
            default_sql = default_sql.value(),
        );
        quote! {
            if n == 0 {
                return ::std::result::Result::Ok(::std::vec::Vec::new());
            }
            let count = ::djogi::primary_key::checked_count(n)?;
            let rows = ctx
                .__query_all_for_macros(#synthesised_sql, &[&count])
                .await?;
            let out: ::std::result::Result<::std::vec::Vec<Self>, ::djogi::DjogiError> = rows
                .into_iter()
                .map(|row| {
                    ::djogi::try_get_scalar::<#inner>(&row, 0).map(Self)
                })
                .collect();
            let out = out?;
            if out.len() != n {
                return ::std::result::Result::Err(
                    ::djogi::primary_key::bulk_row_count_mismatch_err(
                        out.len(),
                        n,
                        "synthesised default_sql batch",
                    ),
                );
            }
            ::std::result::Result::Ok(out)
        }
    };
    let generate_many_ctx = if bulk_sql.is_none() && generate.is_some() {
        quote! { _ctx }
    } else {
        quote! { ctx }
    };
    let db_gen_impl = quote! {
        impl ::djogi::primary_key::PrimaryKeyDbGen for #name {
            async fn generate(
                ctx: &mut ::djogi::DjogiContext,
            ) -> ::std::result::Result<Self, ::djogi::DjogiError> {
                let mut batch = <Self as ::djogi::primary_key::PrimaryKeyDbGen>::generate_many(ctx, 1).await?;
                batch
                    .pop()
                    .ok_or_else(|| ::djogi::DjogiError::Db(
                        ::djogi::DbError::other(
                            "djogi::primary_key!: generate_many returned zero rows for n=1",
                        ),
                    ))
            }

            async fn generate_many(
                #generate_many_ctx: &mut ::djogi::DjogiContext,
                n: usize,
            ) -> ::std::result::Result<::std::vec::Vec<Self>, ::djogi::DjogiError> {
                #bulk_sql_body
            }
        }
    };

    // Client-backed generator. The `generate = |…| expr` attribute carries
    // a callable expression — typically a closure or a fn item. We call it
    // once per `generate_client()` invocation and wrap the result.
    let client_gen_impl = generate.as_ref().map(|expr| {
        quote! {
            impl ::djogi::primary_key::PrimaryKeyClientGen for #name {
                fn generate_client() -> Self {
                    Self((#expr)())
                }
            }
        }
    });

    quote! {
        #[derive(
            ::std::fmt::Debug,
            ::std::clone::Clone,
            ::std::marker::Copy,
            ::std::cmp::PartialEq,
            ::std::cmp::Eq,
            // Cluster 8δ T7.2 — `Cacheable::Id: Hash + Eq + Clone + Ord + Send +
            // Sync + 'static` (`sassi-reference/sassi/src/cacheable.rs:60`). The
            // auto-emitted `impl Cacheable for {Model}` from `#[derive(Model)]`
            // sets `type Id = <user PK type>`, so the PK must satisfy `Ord` for
            // the impl to type-check. `primary_key!` always wraps a primitive-
            // integer or UUID inner type (`BIGINT` / `INTEGER` / `UUID` per
            // `sql_type` validation at parse time) — every accepted inner type
            // already implements `Ord`, so the derive cost is zero. Adding
            // these here keeps `Cacheable` auto-emission compatible with
            // adopter-defined custom PKs without forcing them to write their
            // own derives. The built-in PK types (HeerId, HeerIdDesc, RanjId,
            // RanjIdDesc, Serial) already implement `Ord` upstream
            // (heeranjid + std).
            ::std::cmp::PartialOrd,
            ::std::cmp::Ord,
            ::std::hash::Hash,
            ::djogi::__private::serde::Serialize,
            ::djogi::__private::serde::Deserialize,
        )]
        // Route serde through `::djogi::__private::serde` so adopters never
        // need a direct `serde` dependency — matches the other macro-
        // emitted derive paths across the crate (`feedback_macro_path_routing`).
        #[serde(crate = "::djogi::__private::serde")]
        #[serde(transparent)]
        #vis struct #name(pub #inner);

        impl ::djogi::primary_key::PrimaryKey for #name {
            const __DJOGI_PK_SEAL: ::djogi::primary_key::PkSealToken =
                ::djogi::__private::pk_seal::TOKEN;
            const KIND: ::djogi::PkType = ::djogi::PkType::Custom(
                ::djogi::descriptor::CustomPrimaryKeyKind {
                    type_name: #name_str,
                    sql_type: #sql_type,
                    default_sql: #default_sql,
                }
            );
            const SQL_TYPE: &'static str = #sql_type;
            const DEFAULT_SQL: ::std::option::Option<&'static str> =
                ::std::option::Option::Some(#default_sql);

            fn sentinel() -> Self {
                // Defer to the inner type's `Default` — `i64::default() == 0`,
                // `uuid::Uuid::default() == nil`. Matches the "zero value"
                // contract the built-in `PrimaryKey::sentinel` impls uphold.
                Self(<#inner as ::std::default::Default>::default())
            }
        }

        // `impl Default` lets adopter code use the custom PK type as an
        // ambient field on a `#[model]` struct. The macro-emitted model
        // `Default` impl assigns `Default::default()` to every user field;
        // custom PKs must honour that contract.
        impl ::std::default::Default for #name {
            fn default() -> Self {
                <Self as ::djogi::primary_key::PrimaryKey>::sentinel()
            }
        }

        impl ::djogi::__private::postgres_types::ToSql for #name {
            fn to_sql(
                &self,
                ty: &::djogi::__private::postgres_types::Type,
                out: &mut ::djogi::__private::bytes::BytesMut,
            ) -> ::std::result::Result<
                ::djogi::__private::postgres_types::IsNull,
                ::std::boxed::Box<
                    dyn ::std::error::Error + ::std::marker::Sync + ::std::marker::Send,
                >,
            > {
                <#inner as ::djogi::__private::postgres_types::ToSql>::to_sql(&self.0, ty, out)
            }

            fn accepts(ty: &::djogi::__private::postgres_types::Type) -> bool {
                <#inner as ::djogi::__private::postgres_types::ToSql>::accepts(ty)
            }

            ::djogi::__private::postgres_types::to_sql_checked!();
        }

        // Delegate `IntoFilterValue` to the inner type so `FieldRef<_, Self>::in_list`,
        // `::eq`, etc. reuse the inner's discriminant. Built-in inners like
        // `i64`, `uuid::Uuid`, `HeerId`, and `RanjId` already implement it;
        // adopter inners without an impl surface a clean bound error at the
        // filter call site.
        impl ::djogi::IntoFilterValue for #name {
            fn into_filter_value(self) -> ::djogi::query::internal::FilterValue {
                <#inner as ::djogi::IntoFilterValue>::into_filter_value(self.0)
            }
        }

        impl<'a> ::djogi::__private::postgres_types::FromSql<'a> for #name {
            fn from_sql(
                ty: &::djogi::__private::postgres_types::Type,
                raw: &'a [u8],
            ) -> ::std::result::Result<
                Self,
                ::std::boxed::Box<
                    dyn ::std::error::Error + ::std::marker::Sync + ::std::marker::Send,
                >,
            > {
                <#inner as ::djogi::__private::postgres_types::FromSql<'a>>::from_sql(ty, raw).map(Self)
            }

            fn accepts(ty: &::djogi::__private::postgres_types::Type) -> bool {
                <#inner as ::djogi::__private::postgres_types::FromSql<'a>>::accepts(ty)
            }
        }

        #db_gen_impl
        #client_gen_impl
    }
}
