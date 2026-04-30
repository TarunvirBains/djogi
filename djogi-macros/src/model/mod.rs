//! Orchestrates the `#[model]` attribute macro expansion.
//!
//! Each sub-module handles one concern. Modules that are not yet implemented
//! expose a no-op `expand(...)` that returns an empty `TokenStream`, so the
//! overall pipeline compiles in Task 3 and each subsequent task can replace its
//! stub in isolation without touching this file.

pub mod attrs;
pub mod crud;
pub mod descriptor;
pub mod exclusion;
pub mod filter;
pub mod from_joined_row;
pub mod from_row;
pub mod indexes;
pub mod inject;
pub mod outer_ref;
pub mod protected;
pub mod relations;
pub mod stubs;
pub mod visage_ctx;
pub mod visage_fields;
pub mod visage_query;
pub mod visages;

use attrs::ModelAttrs;
use proc_macro2::TokenStream;
use quote::format_ident;
use syn::{Fields, ItemStruct, parse2};

/// Called from `lib.rs`. Returns the full expanded token stream, or a
/// compile-error token stream on parse/validation failure.
pub fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    expand_inner(attr, item).unwrap_or_else(|e| e.to_compile_error())
}

fn expand_inner(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let model_attrs = ModelAttrs::parse(attr)?;

    // Parse the struct (all user attributes remain intact).
    let mut struct_item: ItemStruct = parse2(item)?;

    // Collect per-field attribute options before we add framework fields.
    let field_attrs: Vec<attrs::FieldAttrs> = struct_item
        .fields
        .iter()
        .map(attrs::FieldAttrs::parse)
        .collect::<syn::Result<_>>()?;

    validate_through_model_shape(&struct_item, &model_attrs)?;
    validate_version_fields(&struct_item, &field_attrs)?;

    // Field names become unquoted SQL column names in the emitted
    // `COLUMN_LIST` — reject any that would break `SELECT` /
    // `RETURNING` text. Runs BEFORE `inject::expand` so the error span
    // lands on the user-declared field, not on an injected one, and
    // BEFORE any token emission so a malformed name produces one crisp
    // diagnostic instead of a cascade of downstream failures. See
    // `crate::ident` for the full rule set and rationale.
    crate::ident::validate_field_column_names(&struct_item)?;

    // Strip `#[field(...)]` attributes from user fields. We already captured
    // their semantics into `field_attrs` above; leaving the raw attribute on
    // the struct surface would confuse rustc, which does not recognise
    // `field` as a helper attribute for the `#[model]` attribute macro
    // (helper attributes only exist on `#[derive(...)]` macros — see the
    // `proc_macro_derive(Model, attributes(field))` declaration in lib.rs,
    // which governs a separate no-op `Model` derive). Stripping here keeps
    // the emitted struct valid Rust without forcing users to also write
    // `#[derive(Model)]` solely to legalise the `#[field(...)]` parsing.
    if let syn::Fields::Named(named) = &mut struct_item.fields {
        for field in &mut named.named {
            field.attrs.retain(|a| !a.path().is_ident("field"));
        }
    }

    // 1. Inject framework fields (`id`, `created_at`, `updated_at`) and emit the
    //    `Default` impl — both are concatenated into a single TokenStream by
    //    `inject::expand`. Returns `syn::Error` for tuple/unit structs and for
    //    user fields that collide with reserved framework names.
    let expanded = inject::expand(&mut struct_item, &model_attrs)?;

    // 2. FromRow impl — Task 5 wires this up.
    let from_row = from_row::expand(&struct_item, &model_attrs, &field_attrs);

    // 2b. FromJoinedPgRow impl — Phase 3 Task 5. Sibling to `from_row` that
    //     accepts a `prefix` parameter; used by `QuerySet::select_related`
    //     to decode both parent (empty prefix) and child
    //     (`"rel_{source_column}."`) from a single joined row. Emitted
    //     for every model so `.select_related(path)` never fails at
    //     compile time for lack of a decoder.
    let from_joined_row = from_joined_row::expand(&struct_item, &model_attrs, &field_attrs);

    // 3. Model trait impl (CRUD) — Tasks 7–9 wire this up.
    let model_impl = crud::expand(&struct_item, &model_attrs, &field_attrs);

    // 4. ModelDescriptor + inventory::submit! — Task 6 wires this up.
    let descriptor = descriptor::expand(&struct_item, &model_attrs, &field_attrs);

    // 5. {Model}Fields — typed closure-API accessors. `stubs::expand` emits
    //    per-column `FieldRef` accessors (Phase 2 Task 4); it needs
    //    `model_attrs` for pk-aware framework-field gating and reads the
    //    field types directly off the post-injection `struct_item`.
    let stubs = stubs::expand(&struct_item, &model_attrs);

    // 6. {Model}Filter — programmatic (closure-free) filter builder.
    //    Separate codegen path: emits a runtime struct carrying a
    //    Vec<FilterClause> with one setter per user field. See
    //    `filter::expand`'s module docs for the typed-vs-erased rationale.
    let filter = filter::expand(&struct_item, &model_attrs, &field_attrs);

    // 7. {Model}Related — typed relation-path constructors (Phase 3 Task 2).
    //    Independent of ModelAttrs/FieldAttrs: the emitter inspects field
    //    types directly via `detect_relation`. Emits a ZST `{Model}Related`
    //    with one method per FK / O2O field — consumed by QuerySet's
    //    prefetch / select_related in Phase 3 Tasks 4 + 5.
    let related = relations::expand(&struct_item);

    // 8. {Model}OuterRef — typed outer-scope column references for
    //    correlated subqueries (Phase 4 Task 5). Same shape as
    //    `{Model}Fields` but the accessors are associated functions
    //    (no receiver) returning `OuterRef<Self, V>` instead of
    //    `FieldRef<Self, V>`. Consumed by `Subquery::new` / `Exists::new`
    //    when building EXISTS / scalar-subquery predicates.
    let outer = outer_ref::expand(&struct_item, &model_attrs);

    // 9. Visage structs + conversion impls (Phase 4.5). Emits
    //    {Model}Public / SelfView / Admin / Export plus scalar `From<&Self>`
    //    (Task 3) / relation-nesting `TryFrom<&Self>` (Task 5) impls.
    //    Reads `FieldAttrs.expose` for scope membership; framework columns
    //    (id / created_at / updated_at) default into every visage (Q13).
    let projections_ts = visages::expand(&struct_item, &model_attrs, &field_attrs);

    // 10. Advisory warnings — Phase 5 Task 11.
    //     Emit a `#[deprecated]` const for each field that carries
    //     `outbox = "ignore"` without a matching `rationale = "..."`.
    //     Stable Rust does not expose `proc_macro::Diagnostic` at warn
    //     level; the idiomatic stable trick is emitting a deprecated const
    //     and immediately referencing it so the compiler fires the
    //     deprecated-use lint at the call site.
    //
    //     `lazy` and `partition_by` advisories are deferred here because
    //     `lazy` has no struct field today and `partition_by` has no
    //     attribute-level parsing. Adding parsers purely to support an
    //     advisory warning would be cart-before-horse — those advisories
    //     land when the attributes become functional features.
    //     (Phase 5 Task 11 — see the Phase 5 implementation plan.)
    let rationale_advisories = emit_rationale_advisories(
        &struct_item,
        // field_attrs was collected before inject::expand mutated
        // struct_item, so indices align with user-declared fields.
        &field_attrs,
    );

    Ok(quote::quote! {
        #expanded
        #from_row
        #from_joined_row
        #model_impl
        #descriptor
        #stubs
        #filter
        #related
        #outer
        #projections_ts
        #rationale_advisories
    })
}

/// Emit advisory deprecation warnings for fields that carry behaviour-modifying
/// attributes without a `rationale = "..."` annotation.
///
/// # Mechanism
///
/// Stable Rust does not expose `proc_macro::Diagnostic` at warn level. The
/// idiomatic stable-Rust approach is emitting a `#[deprecated]` const that is
/// immediately referenced via a second `const _: () = ...;` expression. The
/// compiler fires the deprecated-use lint at the reference site, which surfaces
/// as a warning in the user's build output without preventing compilation.
///
/// # Trigger today: `outbox = "ignore"` without `rationale`
///
/// When `#[field(outbox = "ignore")]` is present on a field but no
/// `#[field(rationale = "...")]` accompanies it, an advisory warning fires.
/// The message prompts the user to document why the field is excluded from the
/// outbox payload (e.g. PII, derived data).
///
/// # Deferred triggers: `lazy`, `partition_by`
///
/// `lazy` is allowlisted in `VALID_FIELD_KEYS` today but has no struct field
/// and no runtime behaviour. `partition_by` has no attribute-level parsing at
/// all (only `ModelDescriptor::partition_by` exists, always `None`). Adding
/// advisory-only parsers for those keys before the attributes become functional
/// would be cart-before-horse — their advisories land in the same phase the
/// attributes become real features. (Phase 5 Task 11 in the implementation
/// plan.)
///
/// # Const naming
///
/// The emitted const name encodes both the struct name and the field name so
/// the same advisory key can appear on multiple fields across multiple models
/// in the same compilation unit without a const-naming collision. The name uses
/// only ASCII alphanumerics and underscores, so it is always a valid Rust
/// identifier — no byte-level escaping is needed beyond what `format_ident!`
/// provides.
fn emit_rationale_advisories(
    struct_item: &ItemStruct,
    field_attrs: &[attrs::FieldAttrs],
) -> TokenStream {
    let mut ts = TokenStream::new();
    let struct_name = &struct_item.ident;

    let Fields::Named(named) = &struct_item.fields else {
        // Tuple / unit structs are rejected upstream; nothing to do here.
        return ts;
    };

    // Only iterate user-declared fields. `field_attrs` was captured before
    // `inject::expand` added framework columns, so indices stay aligned.
    let user_field_count = named.named.len().saturating_sub(
        // inject::expand prepends 3 framework fields (id, created_at,
        // updated_at) after we snapshot field_attrs, so we need the
        // original user count, which IS field_attrs.len().
        0, // field_attrs.len() == user field count; no subtraction needed.
    );
    let _ = user_field_count; // suppress lint — we iterate field_attrs directly.

    for fa in field_attrs {
        // Only fire when `outbox = "ignore"` is present AND `rationale` is absent.
        if fa.outbox.as_deref() != Some("ignore") || fa.rationale.is_some() {
            continue;
        }

        // Recover the field identifier. `FieldAttrs::ident` is always
        // `Some(_)` for named-field structs (darling's magic field populates
        // it from `syn::Field::ident`); tuple structs are rejected upstream.
        let Some(field_ident) = &fa.ident else {
            continue;
        };

        // Build a unique, stable const name from struct + field.
        // Shape: `__djogi_rationale_outbox_{StructName}_{field_name}`
        // ASCII alphanumeric + underscore only — always a valid Rust ident.
        let const_ident = format_ident!("__djogi_rationale_outbox_{}_{}", struct_name, field_ident);

        let deprecation_msg = format!(
            "field `{field_ident}` on `{struct_name}` uses \
             `#[field(outbox = \"ignore\")]` without a `rationale`. \
             Consider adding `#[field(outbox = \"ignore\", rationale = \"...\")]` \
             to document why this field is excluded from the outbox payload \
             (e.g. PII, derived data, ephemeral value)."
        );

        // Emit the deprecated const and an immediate reference so the lint fires.
        ts.extend(quote::quote! {
            #[deprecated(note = #deprecation_msg)]
            #[allow(non_upper_case_globals)]
            const #const_ident: () = ();
            const _: () = #const_ident;
        });
    }

    ts
}

/// Validate `#[field(version)]` annotations across all user fields.
///
/// Rules enforced here (after `FieldAttrs::parse` accepts the bare `version`
/// flag permissively):
///
/// 1. At most one field per model may carry `#[field(version)]`.
///    A second occurrence produces a span-precise compile error at the
///    second field.
/// 2. The annotated field's type must be exactly `i32` or `i64`. Accepted
///    spellings:
///     - bare `i32` / `i64` (single-segment path);
///     - `std::primitive::i32` / `std::primitive::i64`;
///     - `core::primitive::i32` / `core::primitive::i64`.
///
///    Any other multi-segment path — including user-defined module aliases
///    like `my_mod::i32` — is rejected at macro-expansion time so a
///    misleadingly named type alias cannot silently satisfy the contract.
///    `Option<i32>` (last segment `Option`) is likewise rejected.
///
/// Type resolution is unavailable at macro-expansion time. The validator
/// therefore accepts a small, explicit allowlist of qualified primitive
/// spellings plus the bare form. A user who writes `type i32 = String;`
/// in scope of the annotated field can still fool the check; this is the
/// inherent ceiling of syntactic detection and matches the limitation of
/// every other macro in the ecosystem that inspects field types.
fn validate_version_fields(
    struct_item: &ItemStruct,
    field_attrs: &[attrs::FieldAttrs],
) -> syn::Result<()> {
    let mut first_version_idx: Option<usize> = None;

    for (i, (field, fa)) in struct_item
        .fields
        .iter()
        .zip(field_attrs.iter())
        .enumerate()
    {
        if !fa.version {
            continue;
        }

        // Duplicate check — if we already saw a version field, reject this one.
        if first_version_idx.is_some() {
            return Err(syn::Error::new_spanned(
                field,
                "duplicate #[field(version)]: at most one version field is allowed per model",
            ));
        }

        // Type check — accept bare `i32`/`i64` or explicit qualified
        // spellings via `std::primitive` / `core::primitive`.
        // Other multi-segment paths (including `my_mod::i32`) are rejected.
        let is_valid_type = if let syn::Type::Path(syn::TypePath {
            path, qself: None, ..
        }) = &field.ty
        {
            is_version_primitive_path(path)
        } else {
            false
        };

        if !is_valid_type {
            let ty = &field.ty;
            let type_str = quote::quote!(#ty).to_string().replace(' ', "");
            return Err(syn::Error::new_spanned(
                field,
                format!(
                    "#[field(version)] must be i32 or i64 (got {type_str}); \
                     accepted spellings: bare `i32`/`i64`, \
                     `std::primitive::i32`/`i64`, `core::primitive::i32`/`i64`"
                ),
            ));
        }

        first_version_idx = Some(i);
    }

    Ok(())
}

/// Returns `true` when `path` is one of the accepted version-field type
/// spellings. Accepts only:
/// - single-segment `i32` / `i64`
/// - `std::primitive::i32` / `std::primitive::i64`
/// - `core::primitive::i32` / `core::primitive::i64`
///
/// Every other shape returns `false`, including `my_mod::i32` and other
/// user-module paths that happen to end in `i32` / `i64`.
fn is_version_primitive_path(path: &syn::Path) -> bool {
    // Reject absolute paths that start with `::` unless the allowlist shape
    // below applies (handled uniformly via segment count + idents).
    let segments: Vec<String> = path
        .segments
        .iter()
        .map(|seg| seg.ident.to_string())
        .collect();

    // Reject any segment that carries angle-bracketed or parenthesized args
    // (e.g. a typo like `i32<()>`). Version fields must be exactly the
    // bare primitive type.
    if path
        .segments
        .iter()
        .any(|seg| !matches!(seg.arguments, syn::PathArguments::None))
    {
        return false;
    }

    match segments.len() {
        1 => segments[0] == "i32" || segments[0] == "i64",
        3 => {
            (segments[0] == "std" || segments[0] == "core")
                && segments[1] == "primitive"
                && (segments[2] == "i32" || segments[2] == "i64")
        }
        _ => false,
    }
}

/// `#[model(..., through)]` marks a many-to-many junction model and must
/// therefore carry at least two `ForeignKey<T>` columns.
///
/// The relation macros depend on one FK back to each side of the relation.
/// Treating `through` as a pure marker would let obviously-invalid junction
/// structs compile and only fail much later when Task 7 macros tried to use
/// them. Task 8 pins this earlier with a compile-fail fixture.
fn validate_through_model_shape(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
) -> syn::Result<()> {
    if !model_attrs.through {
        return Ok(());
    }

    let fk_count = match &struct_item.fields {
        Fields::Named(named) => named
            .named
            .iter()
            .filter(|field| {
                matches!(
                    attrs::detect_relation(&field.ty),
                    Some(attrs::RelationInfo {
                        kind: attrs::RelationKind::ForeignKey,
                        ..
                    })
                )
            })
            .count(),
        Fields::Unnamed(_) | Fields::Unit => 0,
    };

    if fk_count >= 2 {
        Ok(())
    } else {
        Err(syn::Error::new_spanned(
            &struct_item.ident,
            "a `#[model(through)]` struct must declare at least two `ForeignKey<T>` fields",
        ))
    }
}
