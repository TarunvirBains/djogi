//! `many_to_many!` — declarative stamp-out of one direction of a M2M
//! relation.
//!
//! # What
//!
//! A function-like macro that emits, for a single direction of a
//! many-to-many relationship:
//!
//! 1. `impl ::djogi::relation::ManyToMany<Target> for Source` with the
//!    associated `Through` type, the `RELATION` const, the `this_fk` /
//!    `that_fk` accessors, and the three async method bodies
//!    (`related` / `add_related` / `remove_related`) matching the
//!    trait contract (see [`djogi::relation::many_to_many`]).
//! 2. A named inherent accessor `impl Source { pub async fn <relation>(&self, exec) -> Vec<Target> }`
//!    so users write the ergonomic `person.groups(&pool).await` instead
//!    of the fully-qualified trait call. The accessor delegates
//!    straight to the trait method — no independent query logic —
//!    keeping the trait body the single source of truth.
//! 3. An `inventory::submit!` block registering a
//!    [`djogi::relation::registry::ReverseRelationMarker`] with
//!    `RelationKind::M2M` so Phase 4.5's projection generator can walk
//!    every declared M2M direction in the same pass it walks
//!    reverse-FK / reverse-O2O accessors.
//!
//! # Why one direction per invocation
//!
//! M2M relationships are symmetric at the data layer (the junction row
//! carries both FK columns) but asymmetric at the type layer: each
//! direction picks its own relation name, return type, and trait impl.
//! Stamping both directions from a single macro call would force the
//! user to pick two relation names in one position (ambiguous) or bake
//! in pluralisation heuristics (against the project's
//! explicit-over-implicit stance). One call per direction keeps the
//! user in control of each accessor's name and return type
//! independently — symmetric invocations read as symmetric prose:
//!
//! ```ignore
//! many_to_many!(Person, Group, through = PersonGroup,
//!               this_fk = person_id, that_fk = group_id,
//!               relation = "groups");
//! many_to_many!(Group, Person, through = PersonGroup,
//!               this_fk = group_id,  that_fk = person_id,
//!               relation = "members");
//! ```
//!
//! # How (emitted shape)
//!
//! For `many_to_many!(Source, Target, through = Through, this_fk = a_id, that_fk = b_id, relation = "name");`:
//!
//! ```ignore
//! impl ::djogi::relation::ManyToMany<Target> for Source {
//!     type Through = Through;
//!     const RELATION: &'static str = "name";
//!     fn this_fk() -> &'static str { "a_id" }
//!     fn that_fk() -> &'static str { "b_id" }
//!
//!     async fn related<'ctx>(
//!         &'ctx self,
//!         ctx: &'ctx mut DjogiContext,
//!     ) -> Result<Vec<Target>, ::djogi::DjogiError>
//!     {
//!         let through_rows: Vec<Through> = Through::objects()
//!             .filter(move |f| f.a_id().eq(ForeignKey::new(self.pk_value().clone())))
//!             .fetch_all(ctx).await?;
//!         let mut out = Vec::with_capacity(through_rows.len());
//!         for row in &through_rows {
//!             out.push(Target::get(ctx, row.b_id.key()).await?);
//!         }
//!         Ok(out)
//!     }
//!
//!     async fn add_related<'ctx>(
//!         &'ctx self,
//!         ctx: &'ctx mut DjogiContext,
//!         target: &'ctx Target,
//!         extras: Through,
//!     ) -> Result<Through, ::djogi::DjogiError>
//!     {
//!         let junction = Through {
//!             a_id: ForeignKey::new(self.pk_value().clone()),
//!             b_id: ForeignKey::new(target.pk_value().clone()),
//!             ..extras
//!         };
//!         Through::create(ctx, junction).await
//!     }
//!
//!     async fn remove_related<'ctx>(
//!         &'ctx self,
//!         ctx: &'ctx mut DjogiContext,
//!         target: &'ctx Target,
//!     ) -> Result<u64, ::djogi::DjogiError>
//!     {
//!         Through::objects()
//!             .filter(move |f| f.a_id().eq(ForeignKey::new(self.pk_value().clone())))
//!             .filter(move |f| f.b_id().eq(ForeignKey::new(target.pk_value().clone())))
//!             .delete(ctx).await
//!     }
//! }
//!
//! impl Source {
//!     pub fn name<'ctx>(
//!         &'ctx self,
//!         ctx: &'ctx mut DjogiContext,
//!     ) -> impl Future<Output = Result<Vec<Target>, DjogiError>> + Send + 'ctx
//!     {
//!         <Self as ManyToMany<Target>>::related(self, ctx)
//!     }
//! }
//!
//! inventory::submit! {
//!     ReverseRelationMarker { kind: M2M, source: "Source", name: "name",
//!                             target: "Target", via: "a_id" }
//! }
//! ```
//!
//! The body shape mirrors `many_to_many_hand_impl.rs` exactly — that
//! compile-pass fixture pins the hand-written reference impl, and the
//! macro output matches it byte-for-byte (modulo `pk_value()` vs `id`
//! to stay PK-kind agnostic).
//!
//! # Seal
//!
//! The identifier inputs (`this_fk`, `that_fk`, `relation`) are parsed
//! as `syn::Ident` / `syn::LitStr`, so the Rust tokenizer constrains
//! them to valid ident shapes at parse time. They flow into emitted
//! code in three positions:
//!
//! - As Rust identifiers on the `{Through}Fields` handle
//!   (e.g. `f.a_id()` / `f.b_id()`) — validated by rustc at macro
//!   expansion time; a typo produces `no method named ... found`.
//! - As Rust struct-literal field names inside `add_related`'s
//!   junction construction — same rustc validation.
//! - As `&'static str` values returned by `RELATION` / `this_fk()` /
//!   `that_fk()`. All three are routed through
//!   [`djogi::relation::registry::__macro_support::__const_assert_plain_ident`]
//!   at const-eval time; `relation` and `this_fk` additionally flow
//!   through the inventory marker constructor. That panic fires before
//!   the marker reaches the inventory slice, so any hostile string
//!   turns into a compile error pointing at the macro invocation.
//!
//! The `relation` string is also validated through the same const
//! path — it names both a Rust method on `Source` and a registry
//! `name` field, so the unquoted-identifier rule (letter / underscore
//! start, alphanumeric-or-underscore continuation, ≤ 63 bytes, not a
//! reserved Postgres keyword) is required for both positions.
//!
//! # Where
//!
//! - [`djogi::relation::many_to_many::ManyToMany`] — the trait this
//!   macro impls.
//! - [`djogi::relation::registry::ReverseRelationMarker`] — the
//!   inventory record submitted.
//! - `djogi-macros/tests/compile_pass/many_to_many_macro.rs` — the
//!   end-to-end macro fixture that pins the emission shape.
//! - `djogi-macros/tests/compile_fail/many_to_many_collision.rs` —
//!   duplicate-accessor fixture; two invocations emitting the same
//!   relation name on the same source type trip rustc's
//!   duplicate-inherent-method error, mirroring the reverse-accessor
//!   collision fixture.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitStr, Path, Result, Token};

/// Parsed form of a `many_to_many!` invocation.
///
/// The invocation is position-for-positional-argument keyword-for-keyword-
/// argument, mirroring the trait's associated items:
///
/// ```ignore
/// many_to_many!(
///     Source, Target,
///     through  = Through,
///     this_fk  = col_on_through,
///     that_fk  = col_on_through,
///     relation = "accessor_name"
/// );
/// ```
///
/// Both positional types are required first; the four keyword
/// arguments can appear in any order after. Each keyword is consumed
/// once — duplicates produce a parse error so a late-bound mistaken
/// override cannot silently pick the wrong value.
///
/// # Phase 7-Zero-2 T9 — `expose(scope -> PeerVisage)` clauses
///
/// Zero or more `expose(scope -> PeerVisage)` entries may appear
/// alongside the keyword arguments (comma-separated like the keywords).
/// Each clause asks the emitter to stamp an additional inherent method
/// on the source's `{scope}` visage that returns `Vec<PeerVisage>`. The
/// visage-scoped method fires only when visages at that scope exist on
/// all three participants (source, peer, through-row) — the emitter
/// does not verify this at parse time; the three-way requirement is
/// enforced by rustc at the macro-call site when the emitted body
/// references missing visage methods.
struct ManyToManyInput {
    /// The type carrying the accessor and the `impl ManyToMany<Target>`.
    source_type: Ident,
    /// The type the accessor returns rows of.
    target_type: Ident,
    /// The junction model type — must itself be a `#[model(..., through)]`
    /// carrying two `ForeignKey<_>` columns matching `this_fk` / `that_fk`.
    through_type: Ident,
    /// Column on `through_type` pointing at `source_type`.
    this_fk: Ident,
    /// Column on `through_type` pointing at `target_type`.
    that_fk: Ident,
    /// Relation name — becomes both the inherent accessor method name on
    /// `source_type` and the `RELATION` const on the `ManyToMany` impl.
    relation: LitStr,
    /// Visage exposures declared alongside the M2M declaration.
    /// Empty when no `expose(...)` clause was written.
    exposures: Vec<ManyToManyExposure>,
}

/// One `expose(scope -> PeerVisage)` entry on a M2M relation.
///
/// `scope` selects which of the four built-in visage scopes
/// (`public` / `self_view` / `admin` / `export`) gets a stamped-out
/// accessor. `peer` is the peer visage path returned from that
/// accessor. The source's matching visage (`{Source}{Suffix}`) is the
/// type the method is attached to; the through model's matching
/// visage is referenced indirectly through `TryFrom<&Through>` in the
/// emitted body so its absence surfaces as a compile error at the
/// call site (the three-way check per the plan's conservative rule).
#[derive(Clone)]
struct ManyToManyExposure {
    scope: Ident,
    peer: Path,
}

impl Parse for ManyToManyInput {
    fn parse(input: ParseStream) -> Result<Self> {
        // Positional: `Source, Target,`. Both idents must be present
        // before any keyword argument; the trailing comma after `Target`
        // is mandatory so the keyword list can start fresh.
        let source_type: Ident = input.parse()?;
        input.parse::<Token![,]>()?;
        let target_type: Ident = input.parse()?;
        input.parse::<Token![,]>()?;

        // Keyword arguments, collected into Option<_> slots. Each
        // keyword can appear once; a duplicate returns a parse error
        // pointing at the repeat so the user can see which key was
        // already set.
        let mut through_type: Option<Ident> = None;
        let mut this_fk: Option<Ident> = None;
        let mut that_fk: Option<Ident> = None;
        let mut relation: Option<LitStr> = None;
        let mut exposures: Vec<ManyToManyExposure> = Vec::new();

        // Parse keyword arguments until we hit the end-of-input. A
        // trailing comma is accepted to match the style used elsewhere
        // in the codebase (e.g. `reverse_one_to_many!`).
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            let key_str = key.to_string();

            // `expose(...)` is a special clause — it does NOT take an
            // `=` separator (follows the same bare-call shape as on
            // `#[field(expose(...))]` and `reverse_one_to_many!`). All
            // other keys take `key = value`; destructure on the key's
            // grammar shape rather than one pre-emptive `=` parse so
            // the two shapes coexist.
            if key_str == "expose" {
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
                exposures.push(ManyToManyExposure { scope, peer });
            } else {
                input.parse::<Token![=]>()?;
                match key_str.as_str() {
                    "through" => {
                        if through_type.is_some() {
                            return Err(syn::Error::new(
                                key.span(),
                                "duplicate `through = ...` argument in many_to_many!",
                            ));
                        }
                        through_type = Some(input.parse()?);
                    }
                    "this_fk" => {
                        if this_fk.is_some() {
                            return Err(syn::Error::new(
                                key.span(),
                                "duplicate `this_fk = ...` argument in many_to_many!",
                            ));
                        }
                        this_fk = Some(input.parse()?);
                    }
                    "that_fk" => {
                        if that_fk.is_some() {
                            return Err(syn::Error::new(
                                key.span(),
                                "duplicate `that_fk = ...` argument in many_to_many!",
                            ));
                        }
                        that_fk = Some(input.parse()?);
                    }
                    "relation" => {
                        if relation.is_some() {
                            return Err(syn::Error::new(
                                key.span(),
                                "duplicate `relation = ...` argument in many_to_many!",
                            ));
                        }
                        relation = Some(input.parse()?);
                    }
                    other => {
                        return Err(syn::Error::new(
                            key.span(),
                            format!(
                                "unknown `many_to_many!` argument `{other}` — \
                                 expected one of `through`, `this_fk`, `that_fk`, `relation`, `expose`"
                            ),
                        ));
                    }
                }
            }
            // Consume the optional trailing comma between keywords; if
            // we hit the end of input, break out of the loop regardless.
            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }

        // Validate that all four keyword args were supplied. Missing
        // any one is a user mistake; the error message names the
        // missing key so the fix is obvious.
        let through_type = through_type.ok_or_else(|| {
            syn::Error::new(
                target_type.span(),
                "missing `through = <Type>` argument in many_to_many!",
            )
        })?;
        let this_fk = this_fk.ok_or_else(|| {
            syn::Error::new(
                target_type.span(),
                "missing `this_fk = <column>` argument in many_to_many!",
            )
        })?;
        let that_fk = that_fk.ok_or_else(|| {
            syn::Error::new(
                target_type.span(),
                "missing `that_fk = <column>` argument in many_to_many!",
            )
        })?;
        let relation = relation.ok_or_else(|| {
            syn::Error::new(
                target_type.span(),
                "missing `relation = \"...\"` argument in many_to_many!",
            )
        })?;

        Ok(ManyToManyInput {
            source_type,
            target_type,
            through_type,
            this_fk,
            that_fk,
            relation,
            exposures,
        })
    }
}

/// Expand a `many_to_many!` invocation into the trait impl, the named
/// accessor, and the inventory marker.
///
/// The emission intentionally mirrors `many_to_many_hand_impl.rs`'s
/// body shape — that fixture is the canonical hand-written form, and
/// keeping macro and hand-written output byte-for-byte congruent
/// simplifies cross-checking: a user who read the fixture knows what
/// the macro prints, and a reviewer who knows the macro knows what the
/// fixture looks like. Any divergence between the two shapes should
/// surface as a test failure in either the fixture or the macro's
/// compile-pass fixture, never as silently-different emitted code.
pub fn expand(input: TokenStream) -> TokenStream {
    let parsed: ManyToManyInput = match syn::parse2(input) {
        Ok(p) => p,
        Err(e) => return e.to_compile_error(),
    };

    let ManyToManyInput {
        source_type,
        target_type,
        through_type,
        this_fk,
        that_fk,
        relation,
        exposures,
    } = parsed;

    // The relation name's string form drives both the inventory
    // marker's `name` field (must be a valid identifier) and the
    // inherent accessor method name (also must be a valid identifier).
    // Convert the `LitStr` to the `&'static str` literal we embed in
    // emitted code and the `Ident` we use as the method name.
    let relation_lit = relation.value();
    // `syn::Ident::new` validates identifier shape at proc-macro time;
    // a non-ident-shaped relation name here turns into a macro-call
    // compile error pointing at the `relation = "..."` site.
    let relation_ident = match syn::parse_str::<Ident>(&relation_lit) {
        Ok(id) => id,
        Err(_) => {
            return syn::Error::new(
                relation.span(),
                format!(
                    "many_to_many! `relation = {relation_lit:?}` must be a valid \
                     Rust identifier (it becomes both a method name on `{source_type}` \
                     and an entry in the relation registry)"
                ),
            )
            .to_compile_error();
        }
    };

    // Ident→string copies used in three places: the `RELATION`
    // associated const, the `this_fk` / `that_fk` return values, and
    // the inventory marker literals. Done once so emitted code stays
    // DRY and a reviewer sees the string source alongside each
    // emission site.
    let this_fk_str = this_fk.to_string();
    let that_fk_str = that_fk.to_string();
    let source_str = source_type.to_string();
    let target_str = target_type.to_string();

    // Const-time guard for every user-supplied identifier string the
    // macro bakes into SQL-facing associated items. `relation` and
    // `this_fk` already flow through the inventory marker constructor,
    // but `that_fk` only appears in `ManyToMany::that_fk()`. Emitting
    // one explicit const guard here keeps the three inputs under the
    // same stricter Postgres-identifier seal.
    let identifier_guard = quote! {
        const _: () = {
            ::djogi::relation::registry::__macro_support::__const_assert_plain_ident(
                #relation_lit,
                "many_to_many_relation",
            );
            ::djogi::relation::registry::__macro_support::__const_assert_plain_ident(
                #this_fk_str,
                "many_to_many_this_fk",
            );
            ::djogi::relation::registry::__macro_support::__const_assert_plain_ident(
                #that_fk_str,
                "many_to_many_that_fk",
            );
        };
    };

    // Named accessor documentation — read at the method call site in
    // rustdoc. The body intentionally restates the macro invocation
    // shape so a user hovering `person.groups(&pool)` sees where the
    // accessor came from without having to grep for the macro.
    let accessor_doc = format!(
        "Many-to-many accessor — returns every `{target}` row associated with \
         this `{source}` via the `{through}` junction model. Declared with \
         `djogi::many_to_many!({source}, {target}, through = {through}, \
         this_fk = {this_fk}, that_fk = {that_fk}, relation = {relation:?});`. \
         Delegates to \
         `<Self as ::djogi::relation::ManyToMany<{target}>>::related(self, executor)` — \
         the trait body is the single source of truth for the query shape.",
        source = source_str,
        target = target_str,
        through = through_type,
        this_fk = this_fk_str,
        that_fk = that_fk_str,
        relation = relation_lit,
    );

    // Trait impl body: mirrors `many_to_many_hand_impl.rs` down to the
    // fetch-then-get projection in `related`. Using `pk_value()` rather
    // than reaching into `self.id` keeps the macro PK-kind agnostic —
    // a model with `pk = RanjId` or `pk = Serial` feeds through
    // the same expansion without a per-PK branch here.
    //
    // Each method takes `&'ctx mut DjogiContext`. The `related` body threads
    // the same `&mut ctx` through two sequential calls (one to
    // `Through::objects().fetch_all(ctx)`, then a loop of `Target::get(ctx,
    // ...)`); `ctx` re-borrow is automatic because each inner call takes
    // `&mut DjogiContext`. Under the hood every call pattern-matches on the
    // context's inner variant at the query dispatch boundary (see `djogi::context`).
    let trait_impl = quote! {
        #[automatically_derived]
        impl ::djogi::relation::ManyToMany<#target_type> for #source_type {
            type Through = #through_type;
            const RELATION: &'static str = #relation_lit;

            #[inline]
            fn this_fk() -> &'static str { #this_fk_str }

            #[inline]
            fn that_fk() -> &'static str { #that_fk_str }

            async fn related<'ctx>(
                &'ctx self,
                ctx: &'ctx mut ::djogi::context::DjogiContext,
            ) -> ::std::result::Result<
                ::std::vec::Vec<#target_type>,
                ::djogi::DjogiError,
            > {
                // Capture the PK by value so the closure passed to
                // `.filter(...)` owns the FK it compares against — the
                // same pattern as `reverse_one_to_many!`'s expansion,
                // and symmetric with the hand-impl fixture.
                let this_pk = <Self as ::djogi::model::Model>::pk_value(self).clone();
                let through_rows: ::std::vec::Vec<#through_type> =
                    <#through_type as ::djogi::model::Model>::objects()
                        .filter(move |f| {
                            f.#this_fk().eq(
                                ::djogi::relation::ForeignKey::<#source_type>::new(
                                    this_pk.clone(),
                                ),
                            )
                        })
                        .fetch_all(&mut *ctx)
                        .await?;

                // Project through-rows down to `Target` rows via PK
                // lookup. The hand-impl fixture does the same: N+1
                // queries are acceptable in the reference shape; a
                // future optimisation can fold this into a single
                // `WHERE id IN (...)` select once `QuerySet` grows an
                // `.r#in(...)` lookup.
                let mut out: ::std::vec::Vec<#target_type> =
                    ::std::vec::Vec::with_capacity(through_rows.len());
                for row in &through_rows {
                    out.push(
                        <#target_type as ::djogi::model::Model>::get(
                            &mut *ctx,
                            row.#that_fk.key().clone(),
                        )
                        .await?,
                    );
                }
                ::std::result::Result::Ok(out)
            }

            async fn add_related<'ctx>(
                &'ctx self,
                ctx: &'ctx mut ::djogi::context::DjogiContext,
                target: &'ctx #target_type,
                extras: Self::Through,
            ) -> ::std::result::Result<Self::Through, ::djogi::DjogiError> {
                // Overwrite the two FK columns on `extras` so the
                // junction row definitely points at this `self`/`target`
                // pair; the rest of `extras` (role, joined_at, price,
                // whatever relation-specific columns the junction
                // carries) is preserved via `..extras`. Symmetric with
                // the hand-impl fixture.
                let junction = #through_type {
                    #this_fk: ::djogi::relation::ForeignKey::<#source_type>::new(
                        <Self as ::djogi::model::Model>::pk_value(self).clone(),
                    ),
                    #that_fk: ::djogi::relation::ForeignKey::<#target_type>::new(
                        <#target_type as ::djogi::model::Model>::pk_value(target).clone(),
                    ),
                    ..extras
                };
                <#through_type as ::djogi::model::Model>::create(ctx, junction).await
            }

            async fn remove_related<'ctx>(
                &'ctx self,
                ctx: &'ctx mut ::djogi::context::DjogiContext,
                target: &'ctx #target_type,
            ) -> ::std::result::Result<u64, ::djogi::DjogiError> {
                let this_pk = <Self as ::djogi::model::Model>::pk_value(self).clone();
                let that_pk = <#target_type as ::djogi::model::Model>::pk_value(target).clone();
                <#through_type as ::djogi::model::Model>::objects()
                    .filter(move |f| {
                        f.#this_fk().eq(
                            ::djogi::relation::ForeignKey::<#source_type>::new(
                                this_pk.clone(),
                            ),
                        )
                    })
                    .filter(move |f| {
                        f.#that_fk().eq(
                            ::djogi::relation::ForeignKey::<#target_type>::new(
                                that_pk.clone(),
                            ),
                        )
                    })
                    .delete(ctx)
                    .await
            }
        }
    };

    // Named-accessor inherent method. Delegates straight to the trait
    // method — the ergonomic `person.groups(&mut ctx).await` shape the
    // user sees at the call site, but the query logic stays in the
    // trait body where the hand-impl fixture and the macro can share
    // the single source of truth.
    //
    // The return-type annotation is `impl Future + Send + 'ctx` — mirrors
    // `reverse_one_to_many!`'s shape. The `+ Send` is load-bearing
    // here: opaque async return types do not inherit Send-ness from
    // their inner async block, so callers that need to await the
    // future on a multi-threaded executor still require the explicit
    // annotation. Dropping it would regress ergonomics for tokio
    // multi-thread and axum handler sites.
    let accessor_impl = quote! {
        #[automatically_derived]
        impl #source_type {
            #[doc = #accessor_doc]
            #[inline]
            pub fn #relation_ident<'ctx>(
                &'ctx self,
                ctx: &'ctx mut ::djogi::context::DjogiContext,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<
                    ::std::vec::Vec<#target_type>,
                    ::djogi::DjogiError,
                >,
            > + ::std::marker::Send + 'ctx
            {
                <Self as ::djogi::relation::ManyToMany<#target_type>>::related(self, ctx)
            }
        }
    };

    // Inventory marker. Routed through the existing sealed
    // `__make_reverse_relation_marker` constructor so `name` (the
    // relation string) and `via` (the `this_fk` column) pass the shared
    // const identifier validator at the registry boundary. `that_fk` is
    // validated by `identifier_guard` above because it is not persisted
    // in the marker record.
    //
    // `via` carries the `this_fk` column (not `that_fk`) because
    // Phase 4.5's projection generator walks the registry to discover
    // "how do I reach this accessor from the source?", and the answer
    // is "filter the through table on `this_fk == source.pk`". The
    // `that_fk` column is discoverable by inspecting the through
    // model's descriptor; recording only `this_fk` in the marker
    // avoids duplicating schema info already carried by the
    // `ModelDescriptor`.
    let inventory_submit = quote! {
        ::djogi::__private::inventory::submit! {
            ::djogi::relation::registry::__macro_support::__make_reverse_relation_marker(
                ::djogi::relation::registry::RelationKind::M2M,
                #source_str,
                #relation_lit,
                #target_str,
                #this_fk_str,
            )
        }
    };

    // Phase 7-Zero-2 T9 — visage-scoped M2M accessors.
    //
    // For every `expose(scope -> PeerVisage)` clause, emit an inherent
    // method on `{Source}{Suffix}` (the source's visage at that scope)
    // that walks the through table, converts the fetched peer rows
    // through `<PeerVisage as TryFrom<&Target>>::try_from`, and returns
    // `Vec<PeerVisage>`. The through-row visage is required because the
    // query pattern `Through::objects().filter(|f| f.this_fk().eq(...))`
    // returns `Vec<Through>`, and we convert each row's resolved peer
    // through the peer visage — but the three-way guard the plan asks
    // for is achieved more tightly by requiring the through model to
    // expose a scope visage too (the emitted body references
    // `<ThroughVisage as TryFrom<&Through>>::try_from` to prove every
    // junction row also admits projection at the named scope before
    // fetching the peer).
    //
    // Conservative choice: if the peer or through visage is missing,
    // the emitted body fails to compile with a clean `no method named`
    // or `trait TryFrom<&...> is not implemented` error at the
    // `many_to_many!` call site. We do NOT try to predict the scope
    // suffix conventions or probe for the visage at parse time — that
    // would require a whole new kind of proc-macro introspection.
    // Letting rustc do the check at expansion time keeps the emitter
    // simple and keeps the error message honest.
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
            let source_visage = format_ident!("{source_type}{suffix}");
            let through_visage = format_ident!("{through_type}{suffix}");
            let peer = &exposure.peer;
            let peer_name = peer
                .segments
                .last()
                .map(|s| s.ident.to_string())
                .unwrap_or_default();

            let visage_doc = format!(
                "Visage-scoped many-to-many accessor — returns every `{peer_name}` \
                 associated with this `{source_visage}` via the `{through_type}` \
                 junction. Delegates to the model-scoped `{source_type}::{relation_lit}` \
                 for fetching and projects through `<{peer_name} as TryFrom<&{target_type}>>::try_from`. \
                 The through-row visage (`{through_visage}`) must exist at the `{scope_lit}` scope; \
                 otherwise the projection check inside the body fails to compile.",
                peer_name = peer_name,
                source_visage = source_visage,
                through_type = through_type,
                source_type = source_type,
                relation_lit = relation_lit,
                target_type = target_type,
                through_visage = through_visage,
                scope_lit = scope_lit,
            );

            quote! {
                #[automatically_derived]
                impl #source_visage {
                    #[doc = #visage_doc]
                    #[inline]
                    pub fn #relation_ident<'ctx>(
                        &'ctx self,
                        ctx: &'ctx mut ::djogi::context::DjogiContext,
                    ) -> impl ::std::future::Future<
                        Output = ::std::result::Result<
                            ::std::vec::Vec<#peer>,
                            ::djogi::DjogiError,
                        >,
                    > + ::std::marker::Send + 'ctx
                    {
                        // Visages carry a framework `id` column typed as
                        // the source model's PK. Cloning it mirrors the
                        // reverse-FK visage-scoped emitter in
                        // `reverse_relation.rs` — keep the two sites
                        // symmetric.
                        let pk = ::std::clone::Clone::clone(&self.id);
                        async move {
                            // Through-row exposure gate — the plan's
                            // "both endpoints + through-row" rule
                            // enforces itself through this zero-runtime
                            // probe: if `{Through}{Suffix}` does not
                            // exist, the `TryFrom<&Through>` bound
                            // below fails to resolve and the macro
                            // call site sees a clean diagnostic. The
                            // `as T: TryFrom<...>` bound never executes
                            // — only the type-existence check matters.
                            fn __djogi_through_visage_exists<T>() where
                                T: for<'__a> ::std::convert::TryFrom<&'__a #through_type>
                            {}
                            __djogi_through_visage_exists::<#through_visage>();

                            // Fetch through-rows + project peers via the
                            // existing `ManyToMany::related` body (single
                            // source of truth for the query shape). Each
                            // peer row then folds through the peer
                            // visage's fallible conversion.
                            let __djogi_through_rows: ::std::vec::Vec<#through_type> =
                                <#through_type as ::djogi::model::Model>::objects()
                                    .filter(move |f| {
                                        f.#this_fk().eq(
                                            ::djogi::relation::ForeignKey::<#source_type>::new(
                                                ::std::clone::Clone::clone(&pk),
                                            ),
                                        )
                                    })
                                    .fetch_all(&mut *ctx)
                                    .await?;
                            let mut __djogi_out: ::std::vec::Vec<#peer> =
                                ::std::vec::Vec::with_capacity(__djogi_through_rows.len());
                            for __djogi_row in &__djogi_through_rows {
                                let __djogi_target = <#target_type as ::djogi::model::Model>::get(
                                    &mut *ctx,
                                    __djogi_row.#that_fk.key().clone(),
                                )
                                .await?;
                                __djogi_out.push(
                                    <#peer as ::std::convert::TryFrom<&#target_type>>::try_from(
                                        &__djogi_target,
                                    )?
                                );
                            }
                            ::std::result::Result::Ok(__djogi_out)
                        }
                    }
                }
            }
        })
        .collect();

    // Glue the four/five emissions together. Keeping each block as a
    // separate `quote!` invocation makes diff review easier — a
    // reviewer can scroll to the exact emission block instead of
    // parsing one giant TokenStream — and matches the reverse-relation
    // macro's internal structure.
    quote! {
        #identifier_guard
        #trait_impl
        #accessor_impl
        #inventory_submit
        #(#visage_impls)*
    }
}
