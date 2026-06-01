//! Auto-emit `impl ::djogi::types::Cacheable for {Model}` and
//! `impl ::djogi::types::DeltaSyncCacheable for {Model}` from
//! `#[derive(Model)]` — 2.
//! # What
//! Every `#[model]` struct (with the single exception of `pk = None`)
//! gets an auto-emitted `Cacheable` implementation. Adopters write zero
//! extra derive attributes — the descriptor pipeline + the `Cacheable`
//! emission live in one place, behind one attribute (`#[model(...)]`).
//! When the adopter sets `#[model(watermark_field = "...")]`, this
//! pass also emits `impl DeltaSyncCacheable` pointing at the named
//! field. The default watermark is `updated_at` (the framework-
//! injected timestamp guaranteed by field injection); the
//! adopter override is for models whose freshness signal is something
//! else — `expires_at`, a `version: i64`, a domain-specific
//! `recorded_at`, etc. Both impl emissions are delegated to
//! `sassi_codegen::generate_cacheable_impl` and
//! `sassi_codegen::generate_delta_sync_cacheable_impl` so the surface
//! stays in lock-step with sassi's own `#[derive(Cacheable)]` macro on
//! a future trait-shape change.
//! # `sassi_path = ::djogi::types`
//! Per `feedback_macro_path_routing.md`, proc-macro-emitted code must
//! route through `::djogi::*` paths only — never `::sassi::*` directly.
//! `sassi-codegen` exposes a `sassi_path: &TokenStream` parameter to
//! every entry point so consumers can target their own re-export
//! surface. Djogi's `types.rs` (T7.1) re-exports `Cacheable`,
//! `DeltaSyncCacheable`, `BasicPredicate`, `MonotonicWatermark` from
//! sassi — passing `sassi_path = ::djogi::types` makes the emitted
//! impls write `impl ::djogi::types::Cacheable for {Model}`, which is
//! the path adopters can resolve without an explicit `sassi` dep in
//! their own `Cargo.toml`.
//! # `pk = None` skip
//! Models with `pk = None` do NOT get a `Model` trait impl (per
//! `crud::expand`'s `Model::Pk: Encode` constraint and
//! `feedback_pk_and_fk_cascades_are_core.md`). They likewise do NOT
//! get a `Cacheable` impl emitted here — the cache surface cannot
//! ride the `QuerySet`-driven cache modifier path (T7.3) when the
//! model has no `Model` impl, so the `Cacheable` impl in isolation
//! has no value, and the `id`-field-required contract on
//! sassi-codegen would force adopters who chose a different PK
//! column name to rename it. Skip emission and defer to a hand-
//! rolled `impl Cacheable` if the adopter genuinely needs cache
//! support on a `pk = None` model. Same shape as `crud::expand`'s
//! `Model` impl gate; cluster 8ε's `filter::expand` IntoQ bridge
//! (PR #116) extends the precedent.
//! # Companion `{Model}Fields` shape — `CacheableFieldsMode::External`
//! Sassi's [`Cacheable`](::djogi::types::Cacheable) trait declares
//! `type Fields: Default + Send + Sync + 'static`
//! (`sassi-reference/sassi/src/cacheable.rs:66`). Djogi already emits
//! its own `{Model}Fields` companion through `model::stubs::expand`
//! a ZST whose accessors return [`DjogiField<Self, V>`](::djogi::query::DjogiField),
//! a wrapper that owns both the portable Sassi field and the SQL-only
//! `FieldRef`. Sassi-codegen's default `CacheableFieldsMode::Generated`
//! arm would emit a second `{Model}Fields` struct with one
//! `sassi::Field<Self, V>` accessor per column, colliding at
//! expand-time (rustc E0428).
//! Resolution: route through `sassi_codegen::generate_cacheable_impl`
//! with [`CacheableFieldsMode::external`](sassi_codegen::CacheableFieldsMode::external),
//! which makes sassi-codegen emit `type Fields = #fields_name` and
//! `fn fields -> Self::Fields { #fields_name::new }` against
//! djogi's already-emitted companion — no second struct, no
//! collision, and the `Cacheable` impl shape stays in lock-step with
//! sassi's own evolution. Adopters reach the field handle through the
//! same accessor surface `QuerySet::filter(|f| ...)` already exposes,
//! so Sassi-side predicate builders (`Punnu::scope(...).filter_basic(...)`)
//! and Djogi-side closures share one DSL.
//! `{Model}Fields::new` is `const` and zero-cost — the ZST has no
//! state to populate. After the auto-emit path covers
//! the common case end-to-end: `Cacheable::Id` resolves to the PK
//! type (so adopter generic bounds `<T: Cacheable>` see the right
//! type), `Cacheable::id(&self)` clones `self.id` (so cache-key
//! derivation works through `Punnu::insert(...)` via T7.3+), and
//! `Cacheable::fields` constructs the ZST through `{Model}Fields::new`
//! so Sassi-side predicate builders compose against the same
//! accessors as djogi querysets.
//! `DeltaSyncCacheable` does NOT depend on `{Model}Fields`, so
//! `generate_delta_sync_cacheable_impl` is called as-is.
//! # `_field_attrs` parameter
//! Threaded through for forward compatibility. A future
//! `#[field(cache_key)]` annotation could promote a non-`id` column
//! to the cache identity (per the v0.2 sassi-codegen `#[cacheable(id)]`
//! comment at `cacheable_impl.rs:152`). Unused today.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DataStruct, DeriveInput, Fields, ItemStruct};

use super::attrs::{FieldAttrs, ModelAttrs, PkStrategy};

/// Emit `impl Cacheable for {Model}` and (when applicable)
/// `impl DeltaSyncCacheable for {Model}`.
/// Returns an empty `TokenStream` for `pk = None` models — see the
/// module docs for the rationale.
pub fn expand(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    _field_attrs: &[FieldAttrs],
) -> TokenStream {
    expand_inner(struct_item, model_attrs).unwrap_or_else(|e| e.to_compile_error())
}

fn expand_inner(struct_item: &ItemStruct, model_attrs: &ModelAttrs) -> syn::Result<TokenStream> {
    // `pk = None` skip — adopter-managed PK lifecycle, no auto-emitted
    // Cacheable. Same shape as `crud::expand`'s `Model` impl gate
    // (the early return for `PkStrategy::None`) and `filter::expand`'s
    // IntoQ bridge gate for pk-less models.
    if matches!(model_attrs.pk, PkStrategy::None) {
        return Ok(TokenStream::new());
    }

    // Sassi-codegen routes emitted-code paths through this prefix. The
    // emitted impl writes `impl ::djogi::types::Cacheable for ...` and
    // `impl ::djogi::types::DeltaSyncCacheable for ...`, never
    // `::sassi::*` directly — adopters consume djogi's re-export
    // surface only.
    let sassi_path: TokenStream = quote! { ::djogi::types };

    // `WatermarkField::new(name, span)` carries both the resolved
    // field name and the source span so a missing field on the
    // adopter struct produces a span-precise error pointing at the
    // attribute literal. The default-`updated_at` branch synthesises
    // `Span::call_site` because there is no source token to point
    // at — and the framework-injection invariant guarantees
    // `updated_at` always exists on every PK strategy except
    // `pk = None` (already skipped above).
    let (watermark_name, watermark_span) = match &model_attrs.watermark_field {
        Some(lit) => (lit.value(), lit.span()),
        None => ("updated_at".to_string(), proc_macro2::Span::call_site()),
    };

    // Field-existence validation. Sassi-codegen also checks this in
    // `generate_delta_sync_cacheable_impl`, but we re-check here so
    // the diagnostic message can name "watermark_field" specifically
    // (sassi-codegen's message says "Cacheable: watermark_field …",
    // which is correct in both contexts but reads less cleanly when
    // the user wrote `#[model(watermark_field = ...)]` rather than
    // `#[cacheable(watermark_field = ...)]`).
    // The check inspects the post-injection struct (`struct_item` is
    // passed into `model::expand_inner` after `inject::expand` ran),
    // so framework-injected `id` / `created_at` / `updated_at` are
    // valid choices — the adopter can name any of them as the
    // watermark, though `updated_at` is the only one that monotonic-
    // ally advances on every save and is therefore the default.
    let field_exists = match &struct_item.fields {
        Fields::Named(named) => named
            .named
            .iter()
            .any(|f| f.ident.as_ref().is_some_and(|id| id == &watermark_name)),
        // `inject::expand` rejects tuple/unit structs upstream; this
        // branch is unreachable but preserves the pattern's totality.
        _ => false,
    };

    if !field_exists {
        return Err(syn::Error::new(
            watermark_span,
            format!(
                "`watermark_field = \"{watermark_name}\"` does not name a field on this model — \
                 the named field must exist on the post-injection struct \
                 (framework-injected fields `id`, `created_at`, `updated_at` \
                 are eligible; user fields are eligible)"
            ),
        ));
    }

    // Convert `ItemStruct` → `DeriveInput` so we can hand both
    // `sassi_codegen::generate_cacheable_impl` and
    // `sassi_codegen::generate_delta_sync_cacheable_impl` the shape
    // they expect. The two types differ only in the `struct_token`
    // placement (top-level on `ItemStruct`, inside `DataStruct` on
    // `DeriveInput::Data::Struct`) and `ItemStruct`'s `semi_token`
    // ordering — same data, different layout. We clone all the
    // shared parts so the original `struct_item` (still being walked
    // by other passes in `model::expand`) is untouched.
    let derive_input = DeriveInput {
        attrs: struct_item.attrs.clone(),
        vis: struct_item.vis.clone(),
        ident: struct_item.ident.clone(),
        generics: struct_item.generics.clone(),
        data: Data::Struct(DataStruct {
            struct_token: struct_item.struct_token,
            fields: struct_item.fields.clone(),
            semi_token: struct_item.semi_token,
        }),
    };

    // `model::stubs::expand` emits the companion as `{Model}Fields`. Reach
    // for the same identifier here so `Cacheable::Fields` lines up with the
    // accessor surface adopters already use through
    // `QuerySet::filter(|f| ...)`. `CacheableFieldsMode::external` instructs
    // sassi-codegen to emit `type Fields = #fields_name` and
    // `fn fields -> Self::Fields { #fields_name::new }` against djogi's
    // already-emitted companion — sidestepping
    // `sassi_codegen::generate_fields_struct` so the `{Model}Fields` name is
    // owned by djogi's stubs pass alone (no E0428 collision).
    let struct_name = &struct_item.ident;
    let fields_name = format_ident!("{}Fields", struct_name);
    let options = sassi_codegen::CacheableDeriveOptions {
        watermark_field: Some(sassi_codegen::WatermarkField::new(
            &watermark_name,
            watermark_span,
        )),
        type_name: None,
        // `wire_portable` is sassi's opt-in postcard wire-portability guard
        // (added in sassi-codegen 0.1.0-beta.3). Djogi does not surface
        // `#[cacheable(wire_portable)]` from `#[derive(Model)]` today — the
        // guard is owned by sassi's own derive surface for consumers writing
        // hand-authored `#[derive(Cacheable)]`. Initialising to `None` keeps
        // djogi-macros forward-compatible with sister-repo field additions
        // without depending on sassi's `..Default::default` semantics for
        // a struct that may add non-Default-implementing fields later.
        wire_portable: None,
        fields: sassi_codegen::CacheableFieldsMode::external(
            quote! { #fields_name },
            quote! { #fields_name::new() },
        ),
    };

    let cacheable_impl =
        sassi_codegen::generate_cacheable_impl(&derive_input, &options, &sassi_path)?;

    let delta_sync_impl =
        sassi_codegen::generate_delta_sync_cacheable_impl(&derive_input, &options, &sassi_path)?;

    // 4 — emit a `SassiBootHook` inventory submission so
    // `DjogiContext::from_pool` (and `from_connection`) can walk the inventory
    // at boot time and register a `Punnu<{Model}>` without adopter glue.
    // Path-routing: every path in this emission goes through `::djogi::*` per
    // `feedback_macro_path_routing.md`. `::djogi::SassiBootHook` is re-exported
    // at the djogi crate root (T7.4). `::djogi::cache::Sassi` and
    // `::djogi::cache::Punnu` are both re-exported through `djogi::cache`
    // (T7.1). `::djogi::__private::inventory::submit!` is already available
    // for macro-emitted code.
    // GH #125 — construction routes through the hidden public
    // `::djogi::SassiBootHook::__djogi_from_model_macro` associated
    // function rather than the
    // tuple-struct constructor. The tuple field on `SassiBootHook` is
    // `pub(crate)` to keep adopter code from reading the field or using
    // the literal constructor; macro-emitted code uses the named
    // constructor so the emission keeps compiling against the narrowed
    // v0.1.0 surface.
    let boot_hook = quote! {
        ::djogi::__private::inventory::submit! {
            ::djogi::SassiBootHook::__djogi_from_model_macro(|sassi: &mut ::djogi::cache::Sassi| {
                let punnu = ::djogi::cache::Punnu::<#struct_name>::builder().build();
                sassi.register::<#struct_name>(::std::sync::Arc::new(punnu));
            })
        }
    };

    // 5 — emit `impl DjogiDeltaSyncMeta for {Model}` so the
    // delta-sync fetcher can retrieve the watermark column name at compile
    // time. `WATERMARK_COLUMN` is the same field name resolved above for the
    // `DeltaSyncCacheable` impl — they are always consistent.
    // Path-routing: `::djogi::cache::DjogiDeltaSyncMeta` per
    // `feedback_macro_path_routing.md`.
    let watermark_name_lit = watermark_name.as_str();
    let delta_sync_meta_impl = quote! {
        impl ::djogi::cache::DjogiDeltaSyncMeta for #struct_name {
            const WATERMARK_COLUMN: &'static str = #watermark_name_lit;
        }
    };

    Ok(quote! {
        #cacheable_impl
        #delta_sync_impl
        #delta_sync_meta_impl
        #boot_hook
    })
}
