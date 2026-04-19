//! Function-like macros for reverse-relation accessors.
//!
//! # What
//!
//! This module hosts the expansion logic for the two Phase 3 Task 7
//! reverse-accessor macros:
//!
//! - [`reverse_one_to_many`] — reverse of a forward `ForeignKey<Target>`,
//!   returns `Vec<Source>`.
//! - [`reverse_one_to_one`] — reverse of a forward `OneToOneField<Target>`
//!   (or a `ForeignKey<Target>` + `UNIQUE` pair), returns `Option<Source>`.
//!
//! The third Task 7 macro — `many_to_many!` — is **not** implemented
//! here; it ships in a later commit once the `ManyToMany<Target>` trait
//! (Task 6) is finalized.
//!
//! # Why function-like and not derive
//!
//! A reverse accessor lives on the **opposite** side of the relation
//! from where the FK column is declared. A `#[derive(Model)]` on
//! `Vehicle` (the FK source) has no way to emit `impl Owner { fn
//! cars() }`: attribute macros can only generate items adjacent to
//! their input, not items attached to a foreign type. A function-like
//! macro at the module level reads both type names and emits the
//! `impl Target { ... }` block directly, regardless of which crate
//! defined `Target`.
//!
//! Invocation form is declarative — one line per reverse direction:
//!
//! ```ignore
//! djogi::reverse_one_to_many!(Owner, cars -> Vehicle by owner_id);
//! djogi::reverse_one_to_one!(User, profile -> Profile by user_id);
//! ```
//!
//! # How (emitted shape)
//!
//! For `reverse_one_to_many!(Target, method -> Source by via_column)`:
//!
//! ```ignore
//! impl Target {
//!     pub fn method<'a, E>(&'a self, executor: E)
//!         -> impl Future<Output = Result<Vec<Source>, DjogiError>> + Send + 'a
//!     where
//!         E: sqlx::Executor<'a, Database = sqlx::Postgres> + Send + 'a,
//!     {
//!         let pk = <Self as Model>::pk_value(self).clone();
//!         async move {
//!             Source::objects()
//!                 .filter(move |f| f.via_column().eq(ForeignKey::new(pk)))
//!                 .fetch_all(executor).await
//!         }
//!     }
//! }
//!
//! inventory::submit! {
//!     ReverseRelationMarker {
//!         kind: RelationKind::FK,
//!         source: "Target",  // i.e. the model the method is attached to
//!         name: "method",
//!         target: "Source",
//!         via: "via_column",
//!     }
//! }
//! ```
//!
//! `reverse_one_to_one!` emits an almost-identical shape with two
//! differences: return type is `Result<Option<Source>, DjogiError>` and
//! the terminal is `.first(executor)` instead of `.fetch_all(executor)`.
//!
//! # Terminology note (source vs target)
//!
//! The macro invocation reads `ReceivingType, method -> ReturnedType by
//! via_column`. In this module:
//!
//! - `receiver_type` — the type the accessor method is attached to.
//!   Corresponds to the first positional argument in the invocation
//!   and to the `source` field in the `ReverseRelationMarker` (because
//!   Phase 4.5 reads "this model is the source of the reverse
//!   accessor").
//! - `returned_type` — the model the accessor queries. Corresponds to
//!   the arrow's right-hand side and to the `target` field in the
//!   `ReverseRelationMarker`.
//!
//! The `source` / `target` field names in `ReverseRelationMarker`
//! match Phase 4.5's projection-generator vocabulary, not the
//! forward-FK vocabulary where "source" means the FK-carrying row.
//! Keep the two terminologies distinct when reading.
//!
//! # Path routing
//!
//! All emitted type references route through `::djogi::*` rather than
//! reaching into `sqlx` / `heeranjid` / `time` / `uuid` directly. Macro
//! output compiles in the user's crate, which depends only on `djogi`;
//! the re-exports in `djogi/src/lib.rs` and `djogi/src/prelude` mean a
//! single dep is sufficient.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{Ident, Result, Token};

/// Parsed form of `reverse_one_to_many!(Receiver, method -> Returned by via)`.
///
/// Shared between both reverse-accessor macros; the only difference is
/// the terminal (`.fetch_all` vs `.first`) and the return type
/// (`Vec<Returned>` vs `Option<Returned>`), so parsing is identical.
pub struct ReverseRelationInput {
    /// The type the accessor method is attached to (e.g. `Owner`).
    pub receiver_type: Ident,
    /// The method name emitted on the receiver (e.g. `cars`).
    pub method: Ident,
    /// The model the accessor queries (e.g. `Vehicle`).
    pub returned_type: Ident,
    /// The column on `returned_type` that carries the FK pointing
    /// back at `receiver_type` (e.g. `owner_id`).
    pub via_column: Ident,
}

impl Parse for ReverseRelationInput {
    fn parse(input: ParseStream) -> Result<Self> {
        let receiver_type: Ident = input.parse()?;
        input.parse::<Token![,]>()?;
        let method: Ident = input.parse()?;
        // `->` is a two-character punctuation token (`Token![->]`). A
        // single `-` followed by `>` would be two separate tokens and
        // fail to parse here — the intent is that users always write
        // the arrow with no space between the dash and the gt.
        input.parse::<Token![->]>()?;
        let returned_type: Ident = input.parse()?;
        // `by` is a contextual keyword — we parse it as an ident and
        // verify the spelling. Using a named literal (vs another
        // punctuation token) keeps the invocation readable and matches
        // the prose in module docs.
        let by_kw: Ident = input.parse()?;
        if by_kw != "by" {
            return Err(syn::Error::new(
                by_kw.span(),
                "expected `by` after the returned type in \
                 `reverse_one_to_many!(Receiver, method -> Returned by via_column)`",
            ));
        }
        let via_column: Ident = input.parse()?;
        Ok(ReverseRelationInput {
            receiver_type,
            method,
            returned_type,
            via_column,
        })
    }
}

/// Kind of reverse accessor being emitted. Drives the return-type
/// shape, the terminal method on the inner QuerySet, and the
/// `RelationKind` marker discriminator.
#[derive(Clone, Copy)]
enum AccessorKind {
    /// `reverse_one_to_many!` — `.fetch_all()` → `Vec<Returned>`, marker
    /// kind `FK`.
    OneToMany,
    /// `reverse_one_to_one!` — `.first()` → `Option<Returned>`, marker
    /// kind `O2O`.
    OneToOne,
}

/// Shared expansion for both reverse-accessor macros.
///
/// Splitting the kind into a parameter keeps the parsed-input handling
/// DRY and lets a reader compare the two macros by diffing their thin
/// `reverse_one_to_many`/`reverse_one_to_one` wrappers in `lib.rs`.
pub fn expand(input: TokenStream, kind: AccessorKindOpaque) -> TokenStream {
    let parsed: ReverseRelationInput = match syn::parse2(input) {
        Ok(p) => p,
        Err(e) => return e.to_compile_error(),
    };
    expand_parsed(parsed, kind.0)
}

/// Opaque newtype wrapping [`AccessorKind`] so the `lib.rs` entry
/// points can parameterize the expansion without needing to import
/// the private enum.
///
/// `lib.rs` constructs values of this type via the associated helper
/// constants [`AccessorKindOpaque::ONE_TO_MANY`] /
/// [`AccessorKindOpaque::ONE_TO_ONE`] — those are the only supported
/// constructors, keeping the enum itself private to this module.
#[derive(Clone, Copy)]
pub struct AccessorKindOpaque(AccessorKind);

impl AccessorKindOpaque {
    /// `reverse_one_to_many!` expansion flavor.
    pub const ONE_TO_MANY: Self = Self(AccessorKind::OneToMany);
    /// `reverse_one_to_one!` expansion flavor.
    pub const ONE_TO_ONE: Self = Self(AccessorKind::OneToOne);
}

fn expand_parsed(parsed: ReverseRelationInput, kind: AccessorKind) -> TokenStream {
    let ReverseRelationInput {
        receiver_type,
        method,
        returned_type,
        via_column,
    } = parsed;

    // Literals for the inventory marker. Stringify the idents to feed
    // `&'static str` slots — the `ReverseRelationMarker` fields are all
    // `&'static str` so records can live in a const-initialised static.
    let receiver_lit = receiver_type.to_string();
    let method_lit = method.to_string();
    let returned_lit = returned_type.to_string();
    let via_lit = via_column.to_string();

    // `{Returned}Fields::{via_column}()` is the typed field handle the
    // emitted closure invokes. `format_ident!` with the raw string
    // preserves raw-ident (`r#type`) prefixes if present; the user's
    // macro invocation sees the exact identifier they wrote.
    let filter_method = format_ident!("{}", via_column);

    // Per-kind variations: terminal, return-type inner shape,
    // RelationKind marker variant, and the relation-wrapper
    // constructor used inside the filter closure.
    //
    // `wrapper_ctor` names the exact type the `FieldRef::eq(value)`
    // closure receives. For reverse-FK, the forward column is
    // `ForeignKey<Receiver>`; for reverse-O2O, the forward column is
    // `OneToOneField<Receiver>`. The field-handle's `V` generic is
    // bound to the declared field type, so the value we bind to
    // `.eq(...)` must match that wrapper exactly — the
    // `IntoFilterValue` impls on both wrappers project through the
    // inner PK, so the resulting SQL is identical, but the type the
    // closure body constructs is different by kind.
    let (return_inner_ty, terminal_call, relation_kind_variant, wrapper_ctor): (
        TokenStream,
        TokenStream,
        TokenStream,
        TokenStream,
    ) = match kind {
        AccessorKind::OneToMany => (
            quote! { ::std::vec::Vec<#returned_type> },
            quote! { fetch_all(executor) },
            quote! { ::djogi::relation::registry::RelationKind::FK },
            quote! { ::djogi::relation::ForeignKey::<#receiver_type>::new(pk) },
        ),
        AccessorKind::OneToOne => (
            quote! { ::std::option::Option<#returned_type> },
            quote! { first(executor) },
            quote! { ::djogi::relation::registry::RelationKind::O2O },
            quote! { ::djogi::relation::OneToOneField::<#receiver_type>::new(pk) },
        ),
    };

    // Doc strings assembled once so the emitted `impl` has human-readable
    // documentation on the accessor method. Phase 4.5 and the admin UI
    // render model method docs; keeping them informative here pays off
    // at every downstream surface.
    let method_doc = match kind {
        AccessorKind::OneToMany => format!(
            "Reverse one-to-many accessor — returns every `{returned}` row whose \
             `{via}` column points at this `{receiver}`. Declared with \
             `djogi::reverse_one_to_many!({receiver}, {m} -> {returned} by {via});`.",
            receiver = receiver_lit,
            returned = returned_lit,
            via = via_lit,
            m = method_lit,
        ),
        AccessorKind::OneToOne => format!(
            "Reverse one-to-one accessor — returns `Some(row)` when exactly one \
             `{returned}` row has its `{via}` column pointing at this `{receiver}`, \
             and `None` when none matches. Declared with \
             `djogi::reverse_one_to_one!({receiver}, {m} -> {returned} by {via});`.",
            receiver = receiver_lit,
            returned = returned_lit,
            via = via_lit,
            m = method_lit,
        ),
    };

    // The generated impl:
    //
    // * `'a` scopes the executor's borrow. `&'a self` gives the returned
    //   future a borrow of the receiver so the pk extraction can happen
    //   once outside the `async move`; cloning the pk there (rather than
    //   re-borrowing inside the future) keeps the `Send` bound on the
    //   returned future cheap to satisfy.
    // * The emitted executor bound omits a redundant `+ Send` —
    //   `sqlx::Executor` already declares `Send` as a supertrait on its
    //   own definition, so repeating it here would be dead syntax.
    // * The `<Self as Model>::Pk` where-clause is dropped entirely.
    //   `Model::Pk` already carries `Clone + Send + Sync + 'static` on
    //   the trait itself (see `djogi/src/model.rs`); `'static: 'a` so
    //   the outlive requirement is implied, `Clone` is the only extra
    //   capability the closure uses and it is already satisfied for
    //   every `Model` implementer. Repeating the bounds here would be
    //   dead syntax and invited the over-eager review flag that
    //   prompted this fixup.
    // * The `+ Send` on the returned `impl Future` IS necessary — the
    //   auto-trait bound on an opaque return type is not inherited
    //   from the inner `async move` block, so callers that need to
    //   `.await` the future on a multi-threaded executor still require
    //   the explicit annotation.
    let expanded = quote! {
        #[automatically_derived]
        impl #receiver_type {
            #[doc = #method_doc]
            #[inline]
            pub fn #method<'a, E>(
                &'a self,
                executor: E,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<#return_inner_ty, ::djogi::DjogiError>,
            > + ::std::marker::Send + 'a
            where
                E: ::djogi::__private::sqlx::Executor<
                        'a,
                        Database = ::djogi::__private::sqlx::Postgres,
                    >
                    + 'a,
            {
                // Capture the pk by value outside the async block so
                // the future does not borrow `self` beyond the outer
                // `'a`. The closure passed to `.filter` needs `move`
                // so the owned pk survives past the closure's return.
                let pk = <Self as ::djogi::model::Model>::pk_value(self).clone();
                async move {
                    <#returned_type as ::djogi::model::Model>::objects()
                        .filter(move |f| f.#filter_method().eq(#wrapper_ctor))
                        .#terminal_call
                        .await
                }
            }
        }

        // Inventory marker — Phase 4.5's projection generator walks
        // these records to discover every registered reverse accessor.
        // The marker's `source` field is the receiver (the model the
        // method lives on); `target` is the model the accessor queries.
        // Construction routes through the sealed
        // `__make_reverse_relation_marker` constructor so `name` and
        // `via` are validated against
        // `crate::ident::const_assert_plain_ident` at const-eval time
        // — a downstream crate cannot submit a fabricated marker
        // carrying SQL metacharacters through the inventory slice.
        ::djogi::__private::inventory::submit! {
            ::djogi::relation::registry::__macro_support::__make_reverse_relation_marker(
                #relation_kind_variant,
                #receiver_lit,
                #method_lit,
                #returned_lit,
                #via_lit,
            )
        }
    };

    expanded
}
