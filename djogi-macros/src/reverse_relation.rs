//! Function-like macros for reverse-relation accessors.
//! # What
//! This module hosts the expansion logic for the two
//! reverse-accessor macros:
//! - [`reverse_one_to_many`] — reverse of a forward `ForeignKey<Target>`,
//! returns `Vec<Source>`.
//! - [`reverse_one_to_one`] — reverse of a forward `OneToOneField<Target>`
//! (or a `ForeignKey<Target>` + `UNIQUE` pair), returns `Option<Source>`.
//! The third Task 7 macro — `many_to_many!` — is **not** implemented
//! here; it ships in a later commit once the `ManyToMany<Target>` trait
//! (Task 6) is finalized.
//! # Why function-like and not derive
//! A reverse accessor lives on the **opposite** side of the relation
//! from where the FK column is declared. A `#[derive(Model)]` on
//! `Vehicle` (the FK source) has no way to emit a method on `Owner`:
//! attribute macros can only generate items adjacent to their input,
//! not items attached to a foreign type. A function-like macro at the
//! module level reads both type names and emits a per-relation trait
//! plus its impl, which Rust's coherence rule allows in either the
//! type-defining crate OR the trait-defining crate — letting
//! downstream FK-using crates declare reverse accessors against
//! upstream parent types.
//! Invocation form is declarative — one line per reverse direction:
//! ```ignore
//! djogi::reverse_one_to_many!(Owner, cars -> Vehicle by owner_id);
//! djogi::reverse_one_to_one!(User, profile -> Profile by user_id);
//! ```
//! # How (emitted shape)
//! For `reverse_one_to_many!(Target, method -> Source by via_column)`:
//! ```ignore
//! pub trait TargetMethodReverseRelation {
//!     fn method<'ctx>(
//!         &'ctx self,
//!         ctx: &'ctx mut DjogiContext,
//!     ) -> impl Future<Output = Result<Vec<Source>, DjogiError>> + Send + 'ctx;
//! }
//!
//! impl TargetMethodReverseRelation for Target {
//!     fn method<'ctx>(
//!         &'ctx self,
//!         ctx: &'ctx mut DjogiContext,
//!     ) -> impl Future<Output = Result<Vec<Source>, DjogiError>> + Send + 'ctx
//!     {
//!         let pk = <Self as Model>::pk_value(self).clone();
//!         async move {
//!             Source::objects()
//!                 .filter(move |f| f.via_column().eq(ForeignKey::new(pk)))
//!                 .fetch_all(ctx).await
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
//! `reverse_one_to_one!` emits an almost-identical shape with two
//! differences: return type is `Result<Option<Source>, DjogiError>` and
//! the terminal is `.first(ctx)` instead of `.fetch_all(ctx)`.
//! # Trait-based emission (GH issue #39)
//! PR 5 switched the emission shape from `impl Target { ... }`
//! (inherent impl, subject to Rust's E0116 coherence rule) to
//! `pub trait TargetMethodReverseRelation { ... }` plus
//! `impl TargetMethodReverseRelation for Target { ... }`. The trait
//! impl is allowed in the trait-defining crate even when `Target` lives
//! upstream, lifting the cross-crate constraint that pre-#39 emission
//! carried.
//! The naming convention is `{Receiver}{Method-pascal}ReverseRelation`
//! for the model-scoped accessor, and
//! `{Receiver}{Scope-pascal}{Method-pascal}VisageReverseRelation` for
//! each `expose(scope -> Peer)` clause. Trait-method dispatch requires
//! the trait to be in scope at the call site — when the macro is
//! invoked at module scope (the canonical form), the trait is visible
//! to call sites in the same module without an explicit `use`. Cross-
//! module / cross-crate consumers add `use ...::TargetMethodReverseRelation;`
//! at the top of files that call `.method` on the receiver.
//! # Terminology note (source vs target)
//! The macro invocation reads `ReceivingType, method -> ReturnedType by
//! via_column`. In this module:
//! - `receiver_type` — the type the accessor method is attached to.
//! Corresponds to the first positional argument in the invocation
//! and to the `source` field in the `ReverseRelationMarker` (because
//! reads "this model is the source of the reverse
//! accessor").
//! - `returned_type` — the model the accessor queries. Corresponds to
//! the arrow's right-hand side and to the `target` field in the
//! `ReverseRelationMarker`.
//! The `source` / `target` field names in `ReverseRelationMarker`
//! match 's projection-generator vocabulary, not the
//! forward-FK vocabulary where "source" means the FK-carrying row.
//! Keep the two terminologies distinct when reading.
//! # Path routing
//! All emitted type references route through `::djogi::*` rather than
//! reaching into `heeranjid` / `time` / `uuid` / `tokio_postgres` directly. Macro
//! output compiles in the user's crate, which depends only on `djogi`;
//! the re-exports in `djogi/src/lib.rs` and `djogi/src/prelude` mean a
//! single dep is sufficient.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{Ident, Path, Result, Token};

/// Parsed form of
/// `reverse_one_to_many!(Receiver, method -> Returned by via [, expose(scope -> PeerVisage)...])`.
/// Shared between both reverse-accessor macros; the only difference is
/// the terminal (`.fetch_all` vs `.first`) and the return type
/// (`Vec<Returned>` vs `Option<Returned>`), so parsing is identical.
/// # `expose(scope -> PeerVisage)` clauses
/// Zero or more `expose(scope -> PeerVisage)` entries may appear after
/// the required `... by via_column` segment, each separated by a comma.
/// Each entry asks the macro to emit an additional inherent method on
/// the receiver's `{scope}` visage (e.g. `DeptPublic`) that returns
/// `Vec<PeerVisage>` (or `Option<PeerVisage>` for the O2O variant). The
/// method delegates to the model-scoped accessor under the hood and
/// converts each fetched row via `<PeerVisage as TryFrom<&Returned>>::try_from`.
/// When no `expose(...)` clauses are supplied the emitter behaves exactly
/// like the pre-T9 form: one method on the receiver model, no visage
/// surface. The clause is additive — the model-scoped accessor is always
/// emitted.
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
    /// Visage exposures declared alongside the reverse relation.
    /// Empty when no `expose(...)` clause is written.
    pub exposures: Vec<ReverseExposure>,
}

/// One `expose(scope -> PeerVisage)` entry on a reverse relation.
/// The `scope` is an identifier naming the built-in visage scope
/// (`public` / `self_view` / `admin` / `export`). The receiver's
/// matching visage (`{Receiver}{Suffix}` with `Suffix` derived from
/// `scope`) is the type the additional inherent method is attached to.
/// `peer` is the full path to the peer visage returned from the accessor
/// (e.g. `EmpPublic` or `crate::visages::EmpPublic`).
#[derive(Clone)]
pub struct ReverseExposure {
    /// Scope identifier — lowered to the matching visage suffix at emit time.
    pub scope: Ident,
    /// Peer visage path the accessor returns collections of.
    pub peer: Path,
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

        // T9 — optional `, expose(scope -> PeerVisage)` clauses, zero or
        // more. Each clause is introduced by a leading comma, followed
        // by the `expose` keyword and a parenthesised body of the form
        // `scope -> PeerPath`. Parsing here lives alongside the core
        // reverse-accessor grammar so the user can scan the whole
        // declaration in one line.
        let mut exposures: Vec<ReverseExposure> = Vec::new();
        while input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            // Trailing comma is accepted (matches the looseness used
            // elsewhere in this module and in `many_to_many!`).
            if input.is_empty() {
                break;
            }
            let expose_kw: Ident = input.parse()?;
            if expose_kw != "expose" {
                return Err(syn::Error::new(
                    expose_kw.span(),
                    "expected `expose(scope -> PeerVisage)` after the comma in \
                     `reverse_one_to_many!(..., expose(...))`; found a different keyword",
                ));
            }
            let body;
            syn::parenthesized!(body in input);
            let scope: Ident = body.parse()?;
            body.parse::<Token![->]>()?;
            let peer: Path = body.parse()?;
            if !body.is_empty() {
                return Err(body.error(
                    "expected exactly `scope -> PeerVisage` inside the `expose(...)` body",
                ));
            }
            exposures.push(ReverseExposure { scope, peer });
        }

        Ok(ReverseRelationInput {
            receiver_type,
            method,
            returned_type,
            via_column,
            exposures,
        })
    }
}

/// Kind of reverse accessor being emitted. Drives the return-type
/// shape, the terminal method on the inner QuerySet, and the
/// `RelationKind` marker discriminator.
#[derive(Clone, Copy)]
enum AccessorKind {
    /// `reverse_one_to_many!` — `.fetch_all` → `Vec<Returned>`, marker
    /// kind `FK`.
    OneToMany,
    /// `reverse_one_to_one!` — `.first` → `Option<Returned>`, marker
    /// kind `O2O`.
    OneToOne,
}

/// Shared expansion for both reverse-accessor macros.
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
        exposures,
    } = parsed;

    // Literals for the inventory marker. Stringify the idents to feed
    // `&'static str` slots — the `ReverseRelationMarker` fields are all
    // `&'static str` so records can live in a const-initialised static.
    let receiver_lit = receiver_type.to_string();
    let method_lit = method.to_string();
    let returned_lit = returned_type.to_string();
    let via_lit = via_column.to_string();

    // `{Returned}Fields::{via_column}` is the typed field handle the
    // emitted closure invokes. `format_ident!` with the raw string
    // preserves raw-ident (`r#type`) prefixes if present; the user's
    // macro invocation sees the exact identifier they wrote.
    let filter_method = format_ident!("{}", via_column);

    // Per-kind variations: terminal, return-type inner shape,
    // RelationKind marker variant, and the relation-wrapper
    // constructor used inside the filter closure.
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
            quote! { fetch_all(ctx) },
            quote! { ::djogi::relation::registry::RelationKind::FK },
            quote! { ::djogi::relation::ForeignKey::<#receiver_type>::new(pk) },
        ),
        AccessorKind::OneToOne => (
            quote! { ::std::option::Option<#returned_type> },
            quote! { first(ctx) },
            quote! { ::djogi::relation::registry::RelationKind::O2O },
            quote! { ::djogi::relation::OneToOneField::<#receiver_type>::new(pk) },
        ),
    };

    // Doc strings assembled once so the emitted `impl` has human-readable
    // documentation on the accessor method. and the admin UI
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

    // The generated trait + impl:
    // * `'ctx` scopes both the `&self` receiver borrow and the
    // `&mut DjogiContext` parameter's borrow, and ties both into the
    // returned future's lifetime. The context threads through to
    // `QuerySet::fetch_all(ctx)` (or `.first(ctx)` for O2O) which
    // pattern-matches on the inner pool / transaction variant at the
    // query dispatch boundary — see the `djogi::context` module for the
    // inline-match rationale.
    // * The `<Self as Model>::Pk` where-clause is dropped entirely.
    // `Model::Pk` already carries `Clone + Send + Sync + 'static` on
    // the trait itself (see `djogi/src/model.rs`); `'static: 'ctx` so
    // the outlive requirement is implied, `Clone` is the only extra
    // capability the closure uses and it is already satisfied for
    // every `Model` implementer.
    // * The `+ Send` on the returned `impl Future` IS necessary — the
    // auto-trait bound on an opaque return type is not inherited
    // from the inner `async move` block, so callers that need to
    // `.await` the future on a multi-threaded executor still require
    // the explicit annotation.
    // GH issue #39 — emission shape switched from inherent `impl
    // Receiver { ... }` to a per-relation trait + trait-impl. Inherent
    // impls are subject to Rust's coherence rule (E0116) and must live
    // in the crate that defines the receiver type, which blocks every
    // multi-crate setup where the parent model lives in one crate and
    // the FK-carrying child entity lives in another. Trait impls only
    // need EITHER the type-defining crate OR the trait-defining crate
    // to host them, so the FK-using crate can declare the reverse
    // accessor next to its own model.
    // Naming: `{Receiver}{Method-pascal}ReverseRelation`. The trait is
    // public so adopters can `use {crate}::{Trait}` to bring the method
    // into scope at the call site. Method-name → PascalCase via the
    // shared `crate::case::snake_to_pascal` helper.
    let trait_ident = format_ident!(
        "{}{}ReverseRelation",
        receiver_type,
        crate::case::snake_to_pascal(&method_lit)
    );
    let trait_doc = format!(
        "Per-relation trait emitted by `djogi::reverse_one_to_{}!` for the `{}::{}` \
         reverse accessor. Trait-based emission (vs an inherent impl on `{}`) \
         is what allows the macro to be invoked in a downstream crate when the \
         receiver type lives upstream — see GH issue #39 for the coherence-rule \
         rationale.\n\n\
         Adopters bring the method into scope with `use ...::{}`. \
         The method delegates to the same query body the pre-#39 inherent-impl \
         form did, so semantics are unchanged.",
        match kind {
            AccessorKind::OneToMany => "many",
            AccessorKind::OneToOne => "one",
        },
        receiver_lit,
        method_lit,
        receiver_lit,
        trait_ident,
    );

    let expanded = quote! {
        #[doc = #trait_doc]
        #[automatically_derived]
        pub trait #trait_ident {
            #[doc = #method_doc]
            fn #method<'ctx>(
                &'ctx self,
                ctx: &'ctx mut ::djogi::context::DjogiContext,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<#return_inner_ty, ::djogi::DjogiError>,
            > + ::std::marker::Send + 'ctx;
        }

        #[automatically_derived]
        impl #trait_ident for #receiver_type {
            #[inline]
            fn #method<'ctx>(
                &'ctx self,
                ctx: &'ctx mut ::djogi::context::DjogiContext,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<#return_inner_ty, ::djogi::DjogiError>,
            > + ::std::marker::Send + 'ctx
            {
                // Capture the pk by value outside the async block so
                // the future does not borrow `self` beyond the outer
                // `'ctx`. The closure passed to `.filter` needs `move`
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

        // Inventory marker — 's projection generator and the
        // `validate_relation_accessor_collisions` cross-kind collision
        // gate (closed by GH #158) walk these records to discover every
        // registered reverse accessor. The marker's `source` field is
        // the receiver (the model the method lives on); `target` is
        // the model the accessor queries. Construction routes through
        // the sealed `__make_reverse_relation_marker` constructor so
        // `name` and `via` are validated against
        // `crate::ident::const_assert_user_supplied_ident` at const-eval
        // time — a downstream crate cannot submit a fabricated marker
        // carrying SQL metacharacters or the reserved `__djogi_*`
        // namespace through the inventory slice.
        // Note: rustc catches the same-suffix accessor-name clash
        // (two `reverse_one_to_*!` invocations agreeing on the
        // `{Receiver}{Method-pascal}ReverseRelation` trait name) at
        // the trait redefinition. It does NOT catch a cross-kind
        // clash with a same-name `many_to_many!` invocation, which
        // emits the disjoint `{Source}{Relation-pascal}ManyToManyRelation`
        // trait. The registry validator above is what closes that gap.
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

    // Visage-scoped reverse accessors.
    // For every `expose(scope -> PeerVisage)` clause, emit an additional
    // inherent method on `{Receiver}{Suffix}` (the receiver's visage at
    // that scope) that delegates to the model-scoped accessor above and
    // converts each fetched row through `<PeerVisage as TryFrom<&Returned>>::try_from`.
    // The method is named the same as the model-scoped accessor — the
    // user never sees two different names for the same relation — and
    // differs only in the scope it lives on and the element type of the
    // returned collection. Boundary semantics fall out naturally: no
    // `expose(...)` clause → no method emitted → `no method named ...`
    // at the call site.
    // The receiver-visage ident is computed by appending the
    // scope-specific suffix to the receiver ident. The mapping mirrors
    // `djogi-macros/src/model/visages.rs::SCOPES` — keep these two
    // sites in sync when either grows a new scope.
    let visage_impls: Vec<TokenStream> = exposures
        .iter()
        .map(|exposure| {
            let scope_ident = &exposure.scope;
            let scope_lit = scope_ident.to_string();
            let suffix = match scope_lit.as_str() {
                "public" => "Public",
                "self_view" => "SelfView",
                "admin" => "Admin",
                "export" => "Export",
                other => {
                    return syn::Error::new(
                        scope_ident.span(),
                        format!(
                            "unknown visage scope `{other}` in `expose({other} -> ...)`; \
                             valid scopes are `public`, `self_view`, `admin`, `export`"
                        ),
                    )
                    .to_compile_error();
                }
            };
            let receiver_visage = format_ident!("{receiver_type}{suffix}");
            let peer = &exposure.peer;

            // Visage-scoped reverse accessors
            // return a narrowed `VisageQuerySet<Peer>` synchronously
            // instead of awaiting a `Vec<Peer>` / `Option<Peer>` over
            // a full-model SELECT and projecting in Rust. Two payoffs:
            // 1. SQL-level SELECT narrowing — the queryset bakes in the
            // peer visage's `columns` slice, so emitted SQL projects
            // only exposed columns. Fewer wire bytes, smaller heap
            // fetches, possible index-only scans.
            // 2. Lazy composition — callers can append `.filter(...)`,
            // `.order_by(...)`, `.limit(n)`, `.count(ctx)`,
            // `.exists(ctx)`, or `.stream(ctx)` on top of the reverse
            // accessor instead of materialising every reverse row.
            // The FK predicate is built via the source model's typed
            // `Model::Fields` accessor (`{Returned}Fields::{filter_method}`)
            // so column-name typos are compile errors at macro-emission
            // time, not runtime SQL bugs. The resulting `Condition`
            // hands off to the visage's hidden
            // `__filter_with_initial_condition` constructor.
            // OneToMany returns `VisageQuerySet<Peer>` and the caller
            // chains `.fetch_all(ctx)`. OneToOne also returns
            // `VisageQuerySet<Peer>` — the caller chains `.first(ctx)`
            // for `Option<Peer>` semantics.
            let _ = kind; // kind no longer differentiates the body shape.

            let visage_doc = match kind {
                AccessorKind::OneToMany => format!(
                    "Visage-scoped reverse one-to-many accessor — returns a \
                     SELECT-narrowed `VisageQuerySet<{peer_name}>`. The queryset \
                     emits SQL that projects only `{peer_name}`'s exposed columns \
                     and applies `{via} = <this {receiver}'s pk>` as the root \
                     predicate. Chain `.filter(...)`, `.order_by(...)`, \
                     `.limit(n)`, etc. and finish with `.fetch_all(ctx)` / \
                     `.count(ctx)` / `.stream(ctx)`. Declared with \
                     `djogi::reverse_one_to_many!({receiver}, {m} -> {returned} by {via}, \
                     expose({scope} -> {peer_name}));`.",
                    receiver = receiver_lit,
                    returned = returned_lit,
                    via = via_lit,
                    m = method_lit,
                    scope = scope_lit,
                    peer_name = peer
                        .segments
                        .last()
                        .map(|s| s.ident.to_string())
                        .unwrap_or_default(),
                ),
                AccessorKind::OneToOne => format!(
                    "Visage-scoped reverse one-to-one accessor — returns a \
                     SELECT-narrowed `VisageQuerySet<{peer_name}>` whose root \
                     predicate is `{via} = <this {receiver}'s pk>`. Chain \
                     `.first(ctx)` for `Option<{peer_name}>` semantics. Declared with \
                     `djogi::reverse_one_to_one!({receiver}, {m} -> {returned} by {via}, \
                     expose({scope} -> {peer_name}));`.",
                    receiver = receiver_lit,
                    returned = returned_lit,
                    via = via_lit,
                    m = method_lit,
                    scope = scope_lit,
                    peer_name = peer
                        .segments
                        .last()
                        .map(|s| s.ident.to_string())
                        .unwrap_or_default(),
                ),
            };

            // GH issue #39 — visage-scoped reverse accessors carry the
            // same coherence-rule constraint as the model-scoped ones:
            // `{Receiver}{Suffix}` (the visage type) is defined in the
            // receiver's crate, so a downstream FK-using crate can't
            // declare an inherent impl on it. Trait-based emission lifts
            // the constraint. Trait name embeds the scope so per-scope
            // traits don't collide:
            // `{Receiver}{Scope-pascal}{Method-pascal}VisageReverseRelation`.
            let visage_trait_ident = format_ident!(
                "{}{}{}VisageReverseRelation",
                receiver_type,
                suffix,
                crate::case::snake_to_pascal(&method_lit)
            );
            let visage_trait_doc = format!(
                "Per-relation visage trait emitted by `djogi::reverse_one_to_{}!` \
                 for the `{}::{}` reverse accessor at the `{}` visage scope. \
                 Adopters bring the method into scope with `use ...::{}`. \
                 See GH issue #39 for the coherence-rule rationale behind the \
                 trait-based emission shape.",
                match kind {
                    AccessorKind::OneToMany => "many",
                    AccessorKind::OneToOne => "one",
                },
                receiver_lit,
                method_lit,
                scope_lit,
                visage_trait_ident,
            );
            quote! {
                #[doc = #visage_trait_doc]
                #[automatically_derived]
                pub trait #visage_trait_ident {
                    #[doc = #visage_doc]
                    #[must_use = "querysets are lazy — dropping one silently omits the query"]
                    fn #method(&self) -> ::djogi::query::VisageQuerySet<#peer>;
                }

                #[automatically_derived]
                impl #visage_trait_ident for #receiver_visage {
                    #[inline]
                    fn #method(&self) -> ::djogi::query::VisageQuerySet<#peer> {
                        // Every scope-emitted visage carries an `id`
                        // framework column whose type mirrors the source
                        // model's PK (see `visages::framework_field_decls`).
                        // Cloning it is cheap — every PK type bounds
                        // `Clone` — and the queryset captures the owned
                        // value as a bind parameter via the typed FK
                        // predicate below.
                        let pk = ::std::clone::Clone::clone(&self.id);
                        // Build the FK predicate via the SOURCE MODEL's
                        // typed `Model::Fields` accessor. A typo in the
                        // FK column name would surface as a compile
                        // error here (`no method named 'foo' on type ...
                        // {Returned}Fields`), not a runtime SQL bug.
                        // : `Model::Fields` accessors return
                        // `DjogiField<M, V>` after the macro flip, so the
                        // FK predicate routes the wrapper through
                        // `IntoSqlField::into_sql_field` to recover the
                        // legacy `FieldRef<M, V>` and emit a
                        // `Condition`-shaped FK predicate the
                        // `VisageQuerySet::__filter_with_initial_condition`
                        // entry point already expects.
                        let __field = <
                            <#returned_type as ::djogi::model::Model>::Fields
                            as ::core::default::Default
                        >::default()
                        .#filter_method();
                        let __sql_field =
                            ::djogi::query::field::IntoSqlField::<#returned_type, _>::into_sql_field(__field);
                        let __cond = ::djogi::query::field::FieldRef::<#returned_type, _>::eq(
                            __sql_field,
                            #wrapper_ctor,
                        );
                        <#peer>::__filter_with_initial_condition(__cond)
                    }
                }
            }
        })
        .collect();

    quote! {
        #expanded
        #(#visage_impls)*
    }
}
