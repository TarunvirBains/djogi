//! Shared field metadata for portable SQL emission.
//!
//! Phase 8eta PR2d — both `model::stubs` (root accessor emission and the
//! SQL-only path-aware `{Model}SqlFields` view) and `model::crud` (the
//! macro-generated `Model::__djogi_emit_field_predicate` override) need
//! the same per-field facts: column name, declared Rust type, whether
//! the field is `Option<U>`, and the field's portable kind. Computing
//! that twice would let one consumer drift against the other — a JSONB
//! wrapper detection in stubs.rs that disagreed with crud.rs would
//! silently emit a portable arm that bound the wrong concrete payload
//! type and surface as `ValueTypeMismatch` instead of as a clean
//! `UnsupportedFieldType`.
//!
//! The single source of truth lives here. [`build`] returns a vector of
//! [`PortableFieldEmitInfo`] entries aligned positionally with
//! `struct_item.fields` (i.e. framework fields followed by user fields)
//! after `inject::expand` has run. Both stubs.rs and crud.rs walk the
//! same vector and read the same facts.
//!
//! # Classification rules
//!
//! Every field is sorted into a [`PortableFieldKind`] by inspecting its
//! declared Rust type after stripping a single `Option<>` and any
//! `Tracked<>` layers (matching `descriptor::expand`'s
//! `unwrap_schema_type` channel). The classifier is deliberately
//! conservative — only types whose SQL bind shape is known and whose
//! cached-row parity with the in-memory Sassi evaluator has been
//! validated get a portable kind. Root-column relation wrappers
//! (`ForeignKey<T>` / `OneToOneField<T>`) are portable for equality and
//! membership because their cached representation is the same key wrapper
//! SQL binds through. Everything else (`Jsonb<T>`, `Vec<T>`,
//! `GeoPoint`/`Polygon`/etc., `TsVector`, `#[field(protected(...))]`
//! wrappers, user enums, and unrecognised newtypes) lands in
//! [`PortableFieldKind::Unsupported`] or one of its more specific
//! neighbours so the macro emits a typed
//! `PortablePredicateError::UnsupportedFieldType` arm rather than
//! pretending to support a payload shape that has not been parity-tested.
//!
//! # Why this is not a Sassi-side concern
//!
//! Sassi's `Field<T, V>` works for any `V: PartialEq + Send + Sync +
//! 'static`; SQL emission requires `V: postgres_types::ToSql + Clone +
//! Send + Sync + 'static`. The macro is the only place that knows both
//! the user's declared Rust type AND the column's physical name AND
//! whether the type is one Djogi has parity-tested for portable SQL
//! lowering. Putting the classifier next to the macro emission keeps
//! Sassi free of Djogi-specific bind concerns and keeps the dependency
//! direction `djogi -> sassi`.

use syn::{ItemStruct, Type, TypePath};

use crate::model::attrs::{
    FieldAttrs, ModelAttrs, PkStrategy, detect_relation, unwrap_option, unwrap_tracked,
};

/// Categorisation of one model field for portable SQL lowering.
///
/// The variants split into three groups:
///
/// 1. **Portable scalar leaves** — [`Scalar`], [`String`], [`Bool`].
///    Get specific `(field, op)` arms emitted by `crud.rs` for the
///    operators their `DjogiField` surface exposes.
/// 2. **Portable optional leaves** — [`OptionScalar`], [`OptionString`],
///    [`OptionBool`]. Same as above plus null-test arms and option-aware
///    eq/neq/in/not_in arm bodies that try `Option<U>` first and fall
///    back to the inner `U` (matching `DjogiField::eq(None|Some(_))`
///    versus `DjogiField::some().eq(_)`).
/// 3. **Portable root relation leaves** — [`RelationOrVisage`] and
///    [`OptionRelationOrVisage`]. These cover FK/O2O physical columns
///    only; dotted relation traversal stays on the SQL-only field view.
/// 4. **SQL-only / non-portable kinds** — [`Jsonb`], [`Array`],
///    [`Spatial`], [`FtsComputed`], [`Unsupported`]. Get a single catch-all
///    `(field, _) => UnsupportedFieldType { field }` arm. The portable
///    cache/refresh boundary already rejects the constructed predicate
///    upstream of SQL emission for every `Q::Portable` payload that
///    would mention these fields, so emitting the typed error is
///    belt-and-braces against a future macro addition that mistakenly
///    wrapped one of them in `DjogiField`.
///
/// [`Scalar`]: PortableFieldKind::Scalar
/// [`String`]: PortableFieldKind::String
/// [`Bool`]: PortableFieldKind::Bool
/// [`OptionScalar`]: PortableFieldKind::OptionScalar
/// [`OptionString`]: PortableFieldKind::OptionString
/// [`OptionBool`]: PortableFieldKind::OptionBool
/// [`Jsonb`]: PortableFieldKind::Jsonb
/// [`Array`]: PortableFieldKind::Array
/// [`Spatial`]: PortableFieldKind::Spatial
/// [`FtsComputed`]: PortableFieldKind::FtsComputed
/// [`RelationOrVisage`]: PortableFieldKind::RelationOrVisage
/// [`OptionRelationOrVisage`]: PortableFieldKind::OptionRelationOrVisage
/// [`Unsupported`]: PortableFieldKind::Unsupported
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortableFieldKind {
    /// Plain scalar that binds through `postgres_types::ToSql + Clone +
    /// Send + Sync + 'static` and whose Rust ordering matches the SQL
    /// ordering Djogi emits — `i16`/`i32`/`i64`, `Decimal`,
    /// `DateTime`/`Date`, `Uuid`, the `HeerId` / `RanjId` family.
    Scalar,
    /// `String` — same scalar surface plus the LIKE/ILIKE pattern arms
    /// (`Contains`, `IContains`, `StartsWith`, `IStartsWith`,
    /// `EndsWith`, `IEndsWith`, `IExact`).
    String,
    /// `bool` — equality and list arms only (no ordering / pattern
    /// surface).
    Bool,
    /// `Option<U>` where `U` is itself [`Scalar`] — adds null-test arms
    /// and option-aware eq/neq/in/not_in dispatch.
    ///
    /// [`Scalar`]: PortableFieldKind::Scalar
    OptionScalar,
    /// `Option<String>` — same as [`OptionScalar`] but the inner type is
    /// `String`. Pattern arms are not generated because Sassi's
    /// `PresentField<T, String>` does not expose them in 8eta.
    ///
    /// [`OptionScalar`]: PortableFieldKind::OptionScalar
    OptionString,
    /// `Option<bool>` — null tests plus eq/neq/in/not_in.
    OptionBool,
    /// `Vec<T>` for Djogi-supported one-dimensional Postgres array columns.
    /// Equality and membership are portable; array-specific operators remain
    /// SQL-only through `explicit_pg_predicate()`.
    Array,
    /// `Option<Vec<T>>` for Djogi-supported one-dimensional Postgres array
    /// columns. Null tests, equality, and membership are portable.
    OptionArray,
    /// `Jsonb<T>` / `Option<Jsonb<T>>` — SQL-only in 8eta. Routes
    /// through `explicit_pg_predicate()` for database-specific
    /// predicates; portable arms emit
    /// `UnsupportedFieldType { field }`.
    Jsonb,
    /// `GeoPoint` / `LineString` / `Polygon` / `MultiPoint` /
    /// `MultiPolygon` — PostGIS spatial types, SQL-only.
    Spatial,
    /// `TsVector` — full-text search column populated by a
    /// `GENERATED ALWAYS AS` expression; SQL-only by construction.
    FtsComputed,
    /// `ForeignKey<T>` / `OneToOneField<T>` root physical columns.
    /// Equality and membership are portable; ordering and relation
    /// traversal are not.
    RelationOrVisage,
    /// `Option<ForeignKey<T>>` / `Option<OneToOneField<T>>` root
    /// physical columns. Null tests, equality, and membership are
    /// portable; ordering and relation traversal are not.
    OptionRelationOrVisage,
    /// Anything else — user enums that don't satisfy the bind bounds,
    /// unrecognised newtypes, `#[field(protected(...))]` wrappers
    /// whose codec semantics are not yet portable.
    Unsupported,
}

impl PortableFieldKind {
    /// `true` when the macro should emit specific `(field, op)` arms
    /// for this kind. SQL-only / non-portable kinds get a single
    /// catch-all arm instead.
    ///
    /// The introspection helpers ([`Self::is_portable_leaf`],
    /// [`Self::is_optional`], [`Self::supports_string_patterns`],
    /// [`Self::supports_ordering`]) are public so future callers
    /// outside `crud.rs` (e.g. an admin/Maahi UI emitter that needs
    /// to reason about portable predicate surfaces, or a future
    /// tooling pass) can read the same classifier the macro emit
    /// uses. PR2d's emit dispatches directly on the variant via
    /// `match`; these helpers are reserved for future consumers.
    #[allow(dead_code)]
    pub fn is_portable_leaf(self) -> bool {
        matches!(
            self,
            Self::Scalar
                | Self::String
                | Self::Bool
                | Self::OptionScalar
                | Self::OptionString
                | Self::OptionBool
                | Self::Array
                | Self::OptionArray
                | Self::RelationOrVisage
                | Self::OptionRelationOrVisage
        )
    }

    /// `true` for `Option<U>`-shaped portable kinds. Drives the
    /// option-aware eq/neq/in/not_in dispatch and the null-test arm
    /// emission.
    #[allow(dead_code)]
    pub fn is_optional(self) -> bool {
        matches!(
            self,
            Self::OptionScalar
                | Self::OptionString
                | Self::OptionBool
                | Self::OptionArray
                | Self::OptionRelationOrVisage
        )
    }

    /// `true` for kinds whose `DjogiField` surface exposes the LIKE /
    /// ILIKE pattern family (`Contains` / `IContains` / `StartsWith` /
    /// `IStartsWith` / `EndsWith` / `IEndsWith` / `IExact`). 8eta
    /// limits these to the non-Option `String` kind because Sassi's
    /// `PresentField<T, String>` does not yet expose pattern methods
    /// on the optional surface.
    #[allow(dead_code)]
    pub fn supports_string_patterns(self) -> bool {
        matches!(self, Self::String)
    }

    /// `true` when the macro should emit ordering arms (`Gt`, `Gte`,
    /// `Lt`, `Lte`, `Between`). The SQL helpers are emitted
    /// unconditionally for portable scalar/option kinds; the
    /// `DjogiField` / `DjogiPresentField` builder side gates ordering
    /// behind `V: DjogiPortableOrd`, so a non-orderable type fails
    /// upstream of SQL emission rather than at runtime here.
    #[allow(dead_code)]
    pub fn supports_ordering(self) -> bool {
        matches!(self, Self::Scalar | Self::OptionScalar)
    }
}

/// Per-field metadata consumed by both stubs.rs and crud.rs.
///
/// One entry per post-injection struct field. Indexes line up
/// positionally with `struct_item.fields` so the metadata vector and
/// the field iterator can be zipped without re-deriving. Framework
/// fields (`id`, `created_at`, `updated_at`) come first; user fields
/// follow.
#[derive(Debug, Clone)]
pub struct PortableFieldEmitInfo {
    /// Rust identifier on the post-injection struct. Carries the `r#`
    /// prefix on raw identifiers (e.g. `r#type`).
    pub rust_ident: syn::Ident,
    /// Physical SQL column name. Strips `r#` from raw identifiers so
    /// `r#type` projects to column `type`.
    pub column_name: String,
    /// User's declared Rust type. Used as `V` in macro-emitted
    /// `emit_value::<#name, #ty>` calls when `field_kind` is a
    /// non-Option portable kind, and as the `Option<U>` payload type
    /// when `field_kind` is an `Option*` variant.
    pub rust_type: Type,
    /// `Some(U)` for `Option<U>` fields where `U` is portable. The
    /// macro uses this for the `.some()` payload arm — the
    /// `PresentField<T, U>` surface returns `BasicPredicate`s carrying
    /// `U`, not `Option<U>`, so the macro arm tries `Option<U>` first
    /// (direct Option access) and falls back to `U` (PresentField
    /// access) before returning `ValueTypeMismatch`.
    ///
    /// `None` for non-Option fields and for `Option<NonPortable>`
    /// where the inner type is not in the portable scalar/string/bool
    /// set.
    pub option_inner_type: Option<Type>,
    /// Categorised portability shape. Drives which arms the
    /// `Model::__djogi_emit_field_predicate` override emits for this
    /// field.
    pub field_kind: PortableFieldKind,
    /// `true` when the original declared `rust_type` was wrapped in
    /// `Tracked<…>` (e.g. `Tracked<Option<U>>`, `Tracked<String>`).
    ///
    /// `Tracked<U>` (non-`Option`) fields keep `rust_type =
    /// Tracked<U>`, so the existing scalar arms emit
    /// `emit_value::<M, Tracked<U>>` and the runtime `value_as::<Tracked<U>>`
    /// downcast already matches.
    /// `Tracked<Option<U>>` fields, however, classify as `OptionScalar`
    /// / `OptionString` / `OptionBool` with `option_inner_type = Some(U)`,
    /// and the macro-emitted `option_arms` only attempt `value_as::<Option<U>>`
    /// / `value_as::<U>`. Without an additional `value_as::<Tracked<Option<U>>>`
    /// fallback every predicate built through the supported
    /// `DjogiField<M, Tracked<Option<U>>>::eq` /
    /// `.neq` / `.in_` / `.not_in` API would fail at runtime with
    /// `ValueTypeMismatch`. `crud::option_arms` reads this flag and
    /// emits the additional Tracked-aware fallback chain.
    pub tracked_wrapped: bool,
}

/// Build the per-field metadata vector for a model.
///
/// `struct_item` MUST be the post-`inject::expand` struct (framework
/// fields prepended). `field_attrs` aligns positionally with the
/// user-declared portion of `struct_item.fields[3..]` for
/// non-`pk = None` models — i.e. there are exactly
/// `struct_item.fields.len() - 3` user fields and they line up with
/// `field_attrs`. For `pk = None` models the metadata is empty
/// (matches `crud::expand`'s gate — no `Model` impl, no portable
/// arms).
///
/// The framework field at index 0 is `id` for non-`pk = None` models;
/// indices 1 and 2 are `created_at` / `updated_at`. All three are
/// classified by `rust_type` (the macro caller's `model_attrs.pk`
/// dictates the concrete `id` type at injection time, so the
/// classifier just reads `field.ty`).
pub fn build(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    field_attrs: &[FieldAttrs],
) -> Vec<PortableFieldEmitInfo> {
    // `pk = None` models do not get a `Model` impl, so PR2d does not
    // emit a `Model::__djogi_emit_field_predicate` override for them.
    // Returning an empty vector keeps stubs.rs's accessor emission
    // and crud.rs's predicate-arm emission both inert for these.
    if matches!(model_attrs.pk, PkStrategy::None) {
        return Vec::new();
    }

    let n_framework: usize = 3;
    let mut out: Vec<PortableFieldEmitInfo> = Vec::with_capacity(struct_item.fields.len());

    for (i, field) in struct_item.fields.iter().enumerate() {
        let Some(ident) = field.ident.as_ref() else {
            // Tuple/unit structs are rejected upstream by
            // `inject::expand`; defensive skip keeps the iteration
            // total without panicking on an unreachable shape.
            continue;
        };
        let column_name = crate::syn_util::column_name_from_field(field);
        let rust_type = field.ty.clone();

        // Framework fields (id, created_at, updated_at) at indices
        // [0, n_framework). User fields start at n_framework and align
        // with field_attrs[idx - n_framework].
        let fa_opt: Option<&FieldAttrs> = if i >= n_framework {
            field_attrs.get(i - n_framework)
        } else {
            None
        };

        let (field_kind, option_inner_type) = classify(&rust_type, fa_opt);
        // Detect `Tracked<…>` at the outer layer of the declared type.
        // Required by `crud::option_arms` so `Tracked<Option<U>>` columns
        // emit a `value_as::<Tracked<Option<U>>>` fallback alongside the
        // bare `Option<U>` / `U` attempts. Computing this here keeps the
        // single-source-of-truth contract: every consumer that needs the
        // Tracked-wrapped fact reads the same bit.
        let tracked_wrapped = unwrap_tracked(&rust_type).is_some();

        out.push(PortableFieldEmitInfo {
            rust_ident: ident.clone(),
            column_name,
            rust_type,
            option_inner_type,
            field_kind,
            tracked_wrapped,
        });
    }

    out
}

/// Classify a single field's Rust type into a [`PortableFieldKind`].
///
/// `field_attrs` is `Some(_)` for user fields and `None` for the three
/// framework fields injected at the front of the struct.
///
/// Order matters:
///
/// 1. `#[field(protected(...))]` short-circuits to
///    [`PortableFieldKind::Unsupported`] regardless of the underlying
///    Rust type — protected codecs change the bound shape between
///    plaintext (Punnu) and ciphertext (SQL), which 8eta has not
///    parity-tested.
/// 2. Strip a single `Tracked<...>` layer; the SQL bind operates on
///    the inner type and the macro emission should follow.
/// 3. Strip a single `Option<...>` layer and remember whether one was
///    stripped. The inner type drives the kind; the option flag turns
///    [`Scalar`]/[`String`]/[`Bool`] into their `Option*` siblings.
/// 4. `ForeignKey<T>` / `OneToOneField<T>` root columns lower to
///    [`PortableFieldKind::RelationOrVisage`] or
///    [`PortableFieldKind::OptionRelationOrVisage`]. This is physical
///    column equality/membership only; relation traversal remains
///    SQL-only.
/// 5. Match the inner type's last path segment against the curated
///    portable-scalar set; fall through to [`Unsupported`] for
///    anything else (including user enums, custom newtypes, and any
///    multi-segment path Djogi has not parity-tested).
///
/// [`Scalar`]: PortableFieldKind::Scalar
/// [`String`]: PortableFieldKind::String
/// [`Bool`]: PortableFieldKind::Bool
/// [`Unsupported`]: PortableFieldKind::Unsupported
fn classify(ty: &Type, field_attrs: Option<&FieldAttrs>) -> (PortableFieldKind, Option<Type>) {
    // Protected-field short-circuit — codec semantics are not yet
    // portable, so any predicate against the field becomes a typed
    // `UnsupportedFieldType` regardless of the declared Rust type.
    if let Some(fa) = field_attrs
        && fa.protected.is_some()
    {
        return (PortableFieldKind::Unsupported, None);
    }

    // Strip Tracked<U> — the SQL bind operates on `U`, and the
    // descriptor's `unwrap_schema_type` follows the same convention
    // for column-type derivation.
    let stripped = match unwrap_tracked(ty) {
        Some(inner) => inner,
        None => ty.clone(),
    };

    // Strip Option<U> and remember whether we did so.
    let (inner, was_option) = unwrap_option(&stripped);

    if detect_relation(&inner).is_some() {
        return if was_option {
            (PortableFieldKind::OptionRelationOrVisage, Some(inner))
        } else {
            (PortableFieldKind::RelationOrVisage, None)
        };
    }

    let inner_kind = classify_inner(&inner);

    if was_option {
        match inner_kind {
            PortableFieldKind::Scalar => (PortableFieldKind::OptionScalar, Some(inner)),
            PortableFieldKind::String => (PortableFieldKind::OptionString, Some(inner)),
            PortableFieldKind::Bool => (PortableFieldKind::OptionBool, Some(inner)),
            PortableFieldKind::Array => (PortableFieldKind::OptionArray, Some(inner)),
            // `Option<Jsonb<T>>`, `Option<GeoPoint>`, etc. — keep the inner
            // kind; the option_inner_type is None because the inner kind is
            // itself not portable, so the macro arm body will not branch on it.
            other => (other, None),
        }
    } else {
        (inner_kind, None)
    }
}

/// Classify a fully-stripped (no `Option<>`, no `Tracked<>`) type.
///
/// Inspects the last segment of the type's path. Match arms intentionally
/// list the *last segment ident only* so adopters who write
/// fully-qualified spellings (`djogi::types::HeerId`,
/// `crate::Jsonb<MyShape>`, `geo::Polygon`) get the same classification
/// as the bare-ident form.
fn classify_inner(ty: &Type) -> PortableFieldKind {
    let Type::Path(TypePath {
        path, qself: None, ..
    }) = ty
    else {
        // References, tuples, fn pointers, etc. are not portable scalars.
        return PortableFieldKind::Unsupported;
    };
    let Some(last) = path.segments.last() else {
        return PortableFieldKind::Unsupported;
    };
    let ident = last.ident.to_string();

    // Wrappers detected by structural last-segment ident match.
    if ident == "Jsonb" {
        return PortableFieldKind::Jsonb;
    }
    if ident == "Vec" {
        return PortableFieldKind::Array;
    }
    if ident == "TsVector" {
        return PortableFieldKind::FtsComputed;
    }

    // PostGIS spatial — match the same family `rust_type_to_sql`
    // recognises. Last-segment ident is sufficient because users may
    // write the full `geo::Polygon` / `djogi::geo::Polygon` form and
    // we want the classification to follow.
    if matches!(
        ident.as_str(),
        "GeoPoint" | "LineString" | "Polygon" | "MultiPoint" | "MultiPolygon"
    ) {
        return PortableFieldKind::Spatial;
    }

    // Portable scalar primitives. The list mirrors the `ToSql`
    // implementations Djogi already binds for these types and is
    // intentionally curated rather than a blanket
    // `impl<T: ToSql + ...>` so a future user newtype that happens to
    // satisfy the bounds does not silently get a portable arm without
    // parity-testing.
    if ident == "String" {
        return PortableFieldKind::String;
    }
    if ident == "bool" {
        return PortableFieldKind::Bool;
    }
    if matches!(
        ident.as_str(),
        "i8" | "i16"
            | "i32"
            | "i64"
            | "u32"
            | "Decimal"
            | "DateTime"
            | "OffsetDateTime"
            | "Date"
            | "Uuid"
            | "HeerId"
            | "RanjId"
            | "HeerIdDesc"
            | "RanjIdDesc"
            | "HeerIdRecencyBiased"
            | "RanjIdRecencyBiased"
    ) {
        return PortableFieldKind::Scalar;
    }

    // Everything else — user enums (DjogiEnum-derived), custom
    // newtypes, two-segment paths whose last ident is not in the
    // curated list — falls through. The `(field, _) =>
    // UnsupportedFieldType` arm in crud.rs surfaces this as a typed
    // error rather than a silent SQL miscompilation.
    PortableFieldKind::Unsupported
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    #[test]
    fn classify_bare_i32_is_scalar() {
        let ty: Type = parse_quote!(i32);
        let (kind, inner) = classify(&ty, None);
        assert_eq!(kind, PortableFieldKind::Scalar);
        assert!(inner.is_none());
    }

    #[test]
    fn classify_bare_string_is_string() {
        let ty: Type = parse_quote!(String);
        let (kind, inner) = classify(&ty, None);
        assert_eq!(kind, PortableFieldKind::String);
        assert!(inner.is_none());
    }

    #[test]
    fn classify_option_i32_is_option_scalar_and_carries_inner() {
        let ty: Type = parse_quote!(Option<i32>);
        let (kind, inner) = classify(&ty, None);
        assert_eq!(kind, PortableFieldKind::OptionScalar);
        let inner_ty = inner.expect("inner type expected for Option<i32>");
        assert_eq!(quote::quote!(#inner_ty).to_string(), "i32");
    }

    #[test]
    fn classify_option_string_is_option_string() {
        let ty: Type = parse_quote!(Option<String>);
        let (kind, inner) = classify(&ty, None);
        assert_eq!(kind, PortableFieldKind::OptionString);
        let inner_ty = inner.expect("inner type expected for Option<String>");
        assert_eq!(quote::quote!(#inner_ty).to_string(), "String");
    }

    #[test]
    fn classify_option_bool_is_option_bool() {
        let ty: Type = parse_quote!(Option<bool>);
        let (kind, inner) = classify(&ty, None);
        assert_eq!(kind, PortableFieldKind::OptionBool);
        let inner_ty = inner.expect("inner type expected for Option<bool>");
        assert_eq!(quote::quote!(#inner_ty).to_string(), "bool");
    }

    #[test]
    fn classify_jsonb_is_jsonb_and_drops_inner() {
        let ty: Type = parse_quote!(Jsonb<MyShape>);
        let (kind, inner) = classify(&ty, None);
        assert_eq!(kind, PortableFieldKind::Jsonb);
        assert!(inner.is_none());
    }

    #[test]
    fn classify_option_jsonb_drops_inner() {
        let ty: Type = parse_quote!(Option<Jsonb<MyShape>>);
        let (kind, inner) = classify(&ty, None);
        assert_eq!(kind, PortableFieldKind::Jsonb);
        // `Option<Jsonb<T>>` carries no portable inner — the
        // wrapper itself is non-portable.
        assert!(inner.is_none());
    }

    #[test]
    fn classify_vec_is_array() {
        let ty: Type = parse_quote!(Vec<i32>);
        let (kind, _) = classify(&ty, None);
        assert_eq!(kind, PortableFieldKind::Array);
    }

    #[test]
    fn classify_geopoint_is_spatial() {
        let ty: Type = parse_quote!(GeoPoint);
        let (kind, _) = classify(&ty, None);
        assert_eq!(kind, PortableFieldKind::Spatial);
    }

    #[test]
    fn classify_qualified_jsonb_path_still_jsonb() {
        let ty: Type = parse_quote!(djogi::Jsonb<P>);
        let (kind, _) = classify(&ty, None);
        assert_eq!(kind, PortableFieldKind::Jsonb);
    }

    #[test]
    fn classify_qualified_geopoint_path_still_spatial() {
        let ty: Type = parse_quote!(djogi::geo::GeoPoint);
        let (kind, _) = classify(&ty, None);
        assert_eq!(kind, PortableFieldKind::Spatial);
    }

    #[test]
    fn classify_unknown_ident_falls_back_to_unsupported() {
        let ty: Type = parse_quote!(MyCustomEnum);
        let (kind, _) = classify(&ty, None);
        assert_eq!(kind, PortableFieldKind::Unsupported);
    }

    #[test]
    fn classify_float_is_unsupported_until_nan_parity_is_specified() {
        let f32_ty: Type = parse_quote!(f32);
        let f64_ty: Type = parse_quote!(f64);
        let (f32_kind, _) = classify(&f32_ty, None);
        let (f64_kind, _) = classify(&f64_ty, None);
        assert_eq!(f32_kind, PortableFieldKind::Unsupported);
        assert_eq!(f64_kind, PortableFieldKind::Unsupported);
    }

    #[test]
    fn classify_relation_wrapper_is_relation() {
        let ty: Type = parse_quote!(ForeignKey<Owner>);
        let (kind, inner) = classify(&ty, None);
        assert_eq!(kind, PortableFieldKind::RelationOrVisage);
        assert!(inner.is_none());
    }

    #[test]
    fn classify_optional_relation_wrapper_is_relation() {
        let ty: Type = parse_quote!(Option<ForeignKey<Owner>>);
        let (kind, inner) = classify(&ty, None);
        assert_eq!(kind, PortableFieldKind::OptionRelationOrVisage);
        let inner_ty = inner.expect("inner type expected for Option<ForeignKey<Owner>>");
        assert_eq!(quote::quote!(#inner_ty).to_string(), "ForeignKey < Owner >");
    }

    #[test]
    fn classify_tracked_strips_to_inner() {
        let ty: Type = parse_quote!(Tracked<i32>);
        let (kind, _) = classify(&ty, None);
        assert_eq!(kind, PortableFieldKind::Scalar);
    }

    #[test]
    fn classify_tracked_option_strips_layers() {
        // `Tracked<Option<i32>>` strips `Tracked` then sees
        // `Option<i32>`.
        let ty: Type = parse_quote!(Tracked<Option<i32>>);
        let (kind, inner) = classify(&ty, None);
        assert_eq!(kind, PortableFieldKind::OptionScalar);
        let inner_ty = inner.expect("inner type expected for Tracked<Option<i32>>");
        assert_eq!(quote::quote!(#inner_ty).to_string(), "i32");
    }

    #[test]
    fn portable_leaf_helpers_match_kind_classes() {
        assert!(PortableFieldKind::Scalar.is_portable_leaf());
        assert!(PortableFieldKind::String.is_portable_leaf());
        assert!(PortableFieldKind::Bool.is_portable_leaf());
        assert!(PortableFieldKind::OptionScalar.is_portable_leaf());
        assert!(PortableFieldKind::OptionString.is_portable_leaf());
        assert!(PortableFieldKind::OptionBool.is_portable_leaf());
        assert!(PortableFieldKind::RelationOrVisage.is_portable_leaf());
        assert!(PortableFieldKind::OptionRelationOrVisage.is_portable_leaf());
        assert!(!PortableFieldKind::Jsonb.is_portable_leaf());
        assert!(!PortableFieldKind::Array.is_portable_leaf());
        assert!(!PortableFieldKind::Spatial.is_portable_leaf());
        assert!(!PortableFieldKind::FtsComputed.is_portable_leaf());
        assert!(!PortableFieldKind::Unsupported.is_portable_leaf());
    }

    #[test]
    fn optional_helpers_match_option_kinds_only() {
        assert!(!PortableFieldKind::Scalar.is_optional());
        assert!(!PortableFieldKind::String.is_optional());
        assert!(!PortableFieldKind::Bool.is_optional());
        assert!(PortableFieldKind::OptionScalar.is_optional());
        assert!(PortableFieldKind::OptionString.is_optional());
        assert!(PortableFieldKind::OptionBool.is_optional());
        assert!(!PortableFieldKind::RelationOrVisage.is_optional());
        assert!(PortableFieldKind::OptionRelationOrVisage.is_optional());
    }

    #[test]
    fn ordering_helper_matches_djogi_portable_ord_surface() {
        assert!(PortableFieldKind::Scalar.supports_ordering());
        assert!(PortableFieldKind::OptionScalar.supports_ordering());
        assert!(!PortableFieldKind::String.supports_ordering());
        assert!(!PortableFieldKind::Bool.supports_ordering());
        assert!(!PortableFieldKind::OptionString.supports_ordering());
        assert!(!PortableFieldKind::OptionBool.supports_ordering());
        assert!(!PortableFieldKind::RelationOrVisage.supports_ordering());
        assert!(!PortableFieldKind::OptionRelationOrVisage.supports_ordering());
    }
}
