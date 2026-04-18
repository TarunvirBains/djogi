//! Generates `{Model}Related` — the typed relation-path accessor bag.
//!
//! # What
//!
//! For every `#[model]` struct, emit a ZST `{Model}Related` with one inherent
//! method per relation field (`ForeignKey<T>`, `Option<ForeignKey<T>>`,
//! `OneToOneField<T>`, `Option<OneToOneField<T>>`). Each method returns a
//! [`RelationPath<Source, Target>`](::djogi::relation::RelationPath) carrying:
//!
//! - the column name on the source table (`"owner_id"`),
//! - the target table name (via `Target::table_name()` at runtime),
//! - the relation [`RelationKind`](::djogi::relation::RelationKind).
//!
//! Phase 3 Tasks 4 + 5 consume these handles: `QuerySet::prefetch(path)` and
//! `QuerySet::select_related(path)` accept `RelationPath<Self, _>` and emit
//! the appropriate SQL strategy without further reflection on the source
//! struct.
//!
//! # Why a separate module
//!
//! The `{Model}Related` surface is disjoint from `{Model}Fields` and
//! `{Model}Filter`:
//!
//! - `{Model}Fields` covers every column (framework + user) and drives the
//!   closure filter API;
//! - `{Model}Filter` covers user columns only and drives the erased/
//!   programmatic filter API;
//! - `{Model}Related` covers *only* relation-typed fields (FK / O2O) and
//!   drives prefetch / select_related.
//!
//! Keeping each in its own module isolates the codegen surfaces: a future
//! change to relation detection or prefetch-path shape only touches this
//! file; a change to `FieldRef` or `Lookup` never reaches here.
//!
//! # Method-name convention
//!
//! By convention, users name relation columns `{target}_id` (e.g.
//! `owner_id: ForeignKey<Owner>`). This module strips one trailing `_id`
//! when naming the method — the user writes `VehicleRelated::owner()` rather
//! than `VehicleRelated::owner_id()`, matching the target struct's name.
//! Columns that do not end in `_id` keep their full name as the method
//! name — a field like `pub primary: ForeignKey<Owner>` becomes
//! `VehicleRelated::primary()`, which is the identifier the user wrote.
//!
//! # Empty `{Model}Related`
//!
//! Models with no relation fields still get a `{Model}Related` unit struct
//! — with `#[derive(Debug, Clone, Copy, Default)]` and no methods. Emitting
//! an empty-but-present struct keeps the name reserved for later tasks
//! (e.g. a trait impl or a blanket `Related` trait) and gives downstream
//! `use MyModelRelated` imports a stable target regardless of whether the
//! current model carries relations.
//!
//! # Path routing
//!
//! All emitted type references route through `::djogi::relation::*` —
//! matching the project rule that macro-emitted code never reaches into
//! the underlying crates (`heeranjid`, `time`, `uuid`) directly.

use crate::model::attrs::{RelationKind as MacroRelationKind, detect_relation};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::ItemStruct;

/// Emit `{Model}Related` with one relation-path method per FK / O2O field.
///
/// `struct_item` is the post-injection struct (framework columns at the
/// front), but relation detection only matches `ForeignKey<T>` /
/// `OneToOneField<T>` shapes — framework columns never match, so iterating
/// the full field list is safe.
///
/// The emitter does not re-parse `#[field(...)]` attributes; the
/// [`detect_relation`] helper in `attrs.rs` inspects the field's declared
/// Rust type directly, which is the authoritative signal for "this is an
/// FK / O2O column".
pub fn expand(struct_item: &ItemStruct) -> TokenStream {
    let source_name = &struct_item.ident;
    let related_name = format_ident!("{}Related", source_name);

    let methods: Vec<TokenStream> = struct_item
        .fields
        .iter()
        .filter_map(|field| {
            let ident = field.ident.as_ref()?;
            let info = detect_relation(&field.ty)?;

            // Column literal: raw identifiers (`r#type`) must strip the
            // `r#` prefix, matching the treatment in `stubs.rs` and
            // `descriptor.rs`.
            let raw_name = ident.to_string();
            let column_name = raw_name.strip_prefix("r#").unwrap_or(&raw_name).to_string();

            // Method name: strip one trailing `_id` segment by convention so
            // `owner_id: ForeignKey<Owner>` → `VehicleRelated::owner()`.
            // Columns that do not end in `_id` keep their identifier as the
            // method name, so the user always recognises what they wrote.
            let method_stem = column_name
                .strip_suffix("_id")
                .filter(|s| !s.is_empty())
                .unwrap_or(&column_name);
            // Re-apply the raw-ident escape if the stripped name collides
            // with a Rust keyword. `format_ident!` handles identifiers that
            // are keywords by emitting `r#kw` via the `Ident` constructor;
            // here we use `syn::parse_str::<syn::Ident>` fallback only if
            // `format_ident!` alone can't handle it. In practice the
            // project's models use plain snake_case names — a relation
            // column colliding with a keyword is vanishingly rare — but
            // the escape keeps the emitter total.
            let method_ident = syn::parse_str::<syn::Ident>(method_stem)
                .unwrap_or_else(|_| format_ident!("r#{}", method_stem));

            // Use the *full* target type (not just the last-segment ident) so
            // fully-qualified user spellings like `ForeignKey<crate::models::Owner>`
            // or `ForeignKey<inner::Widget>` still emit a resolvable
            // `RelationPath<Self, crate::models::Owner>` at the macro-call
            // site without requiring a separate `use crate::models::Owner;`.
            // Collapsing down to the last-segment ident here was the Codex-
            // reported blocker: it silently broke codegen for any FK / O2O
            // whose target wasn't imported locally.
            let target_type = &info.target_type;

            let kind_path = match info.kind {
                MacroRelationKind::ForeignKey => {
                    quote! { ::djogi::relation::RelationKind::ForeignKey }
                }
                MacroRelationKind::OneToOne => {
                    quote! { ::djogi::relation::RelationKind::OneToOne }
                }
            };

            Some(quote! {
                /// Typed relation path from `Self` to the related model.
                ///
                /// Pass to `QuerySet::prefetch(...)` / `QuerySet::select_related(...)`
                /// (Phase 3 Tasks 4 + 5) to eager-load the target row(s).
                /// The returned
                /// [`RelationPath`](::djogi::relation::RelationPath) is a
                /// ZST plus three `&'static` members — free to pass around.
                #[inline]
                pub fn #method_ident() -> ::djogi::relation::RelationPath<
                    #source_name,
                    #target_type,
                > {
                    ::djogi::relation::RelationPath::__new(
                        #column_name,
                        <#target_type as ::djogi::model::Model>::table_name(),
                        #kind_path,
                    )
                }
            })
        })
        .collect();

    if methods.is_empty() {
        // No relations declared — still emit the unit struct so downstream
        // `use MyModelRelated` is stable regardless of whether the model
        // currently carries any FK / O2O fields. A later edit that adds
        // such a field populates inherent methods without breaking imports.
        quote! {
            /// Typed relation-path constructors for this model.
            ///
            /// Currently empty — this model has no `ForeignKey<T>` or
            /// `OneToOneField<T>` fields. Adding a relation field to the
            /// struct will surface here as an inherent method that returns
            /// a [`RelationPath`](::djogi::relation::RelationPath).
            #[derive(Debug, Clone, Copy, Default)]
            pub struct #related_name;
        }
    } else {
        quote! {
            /// Typed relation-path constructors for this model.
            ///
            /// Each inherent method corresponds to one `ForeignKey<T>` or
            /// `OneToOneField<T>` field on the struct and returns a
            /// [`RelationPath<Self, Target>`](::djogi::relation::RelationPath)
            /// — a ZST handle proving source/target alignment at the type
            /// level. Consume it via `QuerySet::prefetch(...)` /
            /// `QuerySet::select_related(...)` (Phase 3 Tasks 4 + 5).
            #[derive(Debug, Clone, Copy, Default)]
            pub struct #related_name;

            impl #related_name {
                #(#methods)*
            }
        }
    }
}
