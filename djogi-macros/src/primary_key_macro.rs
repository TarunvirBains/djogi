//! `djogi::primary_key!` declarative-style macro (Phase 7-Zero-2 T3).
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
            match key.to_string().as_str() {
                "sql_type" => {
                    if sql_type.is_some() {
                        return Err(syn::Error::new_spanned(
                            &key,
                            "duplicate `sql_type` in djogi::primary_key!",
                        ));
                    }
                    sql_type = Some(input.parse()?);
                }
                "default_sql" => {
                    if default_sql.is_some() {
                        return Err(syn::Error::new_spanned(
                            &key,
                            "duplicate `default_sql` in djogi::primary_key!",
                        ));
                    }
                    default_sql = Some(input.parse()?);
                }
                "bulk_sql" => {
                    if bulk_sql.is_some() {
                        return Err(syn::Error::new_spanned(
                            &key,
                            "duplicate `bulk_sql` in djogi::primary_key!",
                        ));
                    }
                    bulk_sql = Some(input.parse()?);
                }
                "generate" => {
                    if generate.is_some() {
                        return Err(syn::Error::new_spanned(
                            &key,
                            "duplicate `generate` in djogi::primary_key!",
                        ));
                    }
                    generate = Some(input.parse()?);
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        &key,
                        format!(
                            "unknown djogi::primary_key! key `{other}`; \
                             expected one of sql_type / default_sql / bulk_sql / generate"
                        ),
                    ));
                }
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

    // DB-backed generator. `generate_many` runs `bulk_sql` with `$1` bound
    // to the count as `i32` — matches the built-in `generate_ids` /
    // `generate_ranjids` call shape. `generate` dispatches to
    // `generate_many(ctx, 1)` and takes the first element; one code path,
    // one query shape to audit.
    let db_gen_impl = bulk_sql.as_ref().map(|sql| {
        quote! {
            impl ::djogi::primary_key::PrimaryKeyDbGen for #name {
                async fn generate(
                    ctx: &mut ::djogi::DjogiContext,
                ) -> ::std::result::Result<Self, ::djogi::DjogiError> {
                    let mut batch = <Self as ::djogi::primary_key::PrimaryKeyDbGen>::generate_many(ctx, 1).await?;
                    batch
                        .pop()
                        .ok_or_else(|| ::djogi::DjogiError::Db(
                            ::djogi::DbError::other(
                                "djogi::primary_key!: bulk_sql returned zero rows for n=1",
                            ),
                        ))
                }

                async fn generate_many(
                    ctx: &mut ::djogi::DjogiContext,
                    n: usize,
                ) -> ::std::result::Result<::std::vec::Vec<Self>, ::djogi::DjogiError> {
                    if n == 0 {
                        return ::std::result::Result::Ok(::std::vec::Vec::new());
                    }
                    let count: i32 = ::std::convert::TryFrom::try_from(n).map_err(|_| {
                        ::djogi::DjogiError::Db(::djogi::DbError::other(
                            ::std::format!(
                                "djogi::primary_key!: bulk generate rejected — count {} exceeds i32::MAX",
                                n
                            ),
                        ))
                    })?;
                    let rows = ctx
                        .__query_all_for_macros(#sql, &[&count])
                        .await?;
                    rows.into_iter()
                        .map(|row| {
                            ::djogi::try_get_scalar::<#inner>(&row, 0).map(Self)
                        })
                        .collect()
                }
            }
        }
    });

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
