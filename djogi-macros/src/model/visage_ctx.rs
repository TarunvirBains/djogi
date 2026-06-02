//! Shared context and classifier for visage emit passes.
//! The three visage emitters (`visages`, `visage_fields`, `visage_query`) each
//! need the same scope-membership classification for every user field. This
//! module centralises that logic so the classification is computed once per
//! `(field, scope)` pair and re-used across all three emit phases.

use crate::model::attrs::{FieldAttrs, ModelAttrs, RelationExposure, detect_relation};
use crate::model::derived::DerivedAttr;
use quote::format_ident;
use syn::{Ident, ItemStruct};

/// Shared parameters threaded through the three visage emit passes.
/// `visages::expand` builds one `VisageEmitContext` per scope (the four
/// `("public", "Public") | ("self_view", "SelfView") | ...` pairs) and hands
/// the same `&VisageEmitContext` to `visages::emit_projection_for_scope`,
/// `visage_fields::expand`, and `visage_query::expand`. Replaces the
/// 7-positional-parameter signature each used to carry — same fields, one
/// borrow, no swap-bug exposure.
pub(crate) struct VisageEmitContext<'a> {
    /// Source model ident (the `#[model]`-annotated struct).
    pub source: &'a Ident,
    /// Visage type ident — `{Source}{Suffix}` (e.g. `UserPublic`). Owned
    /// because it is freshly formatted per scope iteration.
    pub visage_ident: Ident,
    /// Lowercase scope key (`"public"`, `"self_view"`, `"admin"`,
    /// `"export"`). Used for `expose(scope)` lookups.
    pub scope: &'a str,
    /// The post-injection model struct (framework fields prepended).
    pub struct_item: &'a ItemStruct,
    /// Per-field attributes parsed from `#[field(...)]` annotations.
    /// Indexes line up with `struct_item.fields` after the `n_framework`
    /// prefix (so `field_attrs[0]` describes the first user field).
    pub field_attrs: &'a [FieldAttrs],
    /// Model-level attributes (`#[model(pk = "...", visages = "...", ...)]`).
    pub model_attrs: &'a ModelAttrs,
    /// Count of framework columns the inject pass prepended (3 for normal
    /// PK strategies, 2 for `pk = "none"`). Used to skip past the framework
    /// prefix when iterating user fields.
    pub n_framework: usize,
    /// #231 — every parsed struct-level `#[derived(...)]`
    /// attribute on the source struct, in source order. The visage
    /// emitter filters these per-scope (an attribute contributes a
    /// projection entry iff its `scopes = [...]` list includes
    /// `self.scope`) and surfaces the matching entries as visage
    /// struct fields, SELECT aliases, decode arms, and From/TryFrom
    /// init expressions.
    pub derived_attrs: &'a [DerivedAttr],
}

impl<'a> VisageEmitContext<'a> {
    /// Iterate every parsed `#[derived(...)]` attribute whose
    /// `scopes = [...]` list contains the context's scope. The
    /// iteration order matches source-attribute order, which is the
    /// stable codegen ordering the spec mandates for visage struct
    /// fields and projection-list rendering.
    pub(crate) fn scope_derived(&self) -> impl Iterator<Item = &'a DerivedAttr> + '_ {
        let scope = self.scope;
        self.derived_attrs
            .iter()
            .filter(move |d| d.scopes.iter().any(|s| s.key == scope))
    }
}

/// Whether and how a field participates in a given visage scope.
pub(crate) enum ScopeMembership<'a> {
    /// The field is not included in this scope.
    Absent,
    /// The field is included as a scalar column (no embedded peer).
    Scalar,
    /// The field is included as a relation embed.
    RelationEmbed {
        exposure: &'a RelationExposure,
        nullable: bool,
    },
    /// The combination is invalid (e.g. scalar annotation on a relation field).
    /// The visage emitter will emit a span-precise compile error for these.
    Reject {
        #[allow(dead_code)]
        msg: &'static str,
    },
}

/// Classify whether and how `field` participates in `scope`, given its
/// resolved `attrs`.
/// This function centralises the three-way `(scalar_hit, relation_hit,
/// is_relation)` dispatch that was previously duplicated inside
/// `visages::emit_projection_for_scope`, `visage_fields::expand`, and
/// `visage_query::expand`. Callers compute the classification once and pass
/// it down rather than re-running `detect_relation` per emitter per field.
pub(crate) fn classify_field_for_scope<'a>(
    field: &syn::Field,
    attrs: &'a FieldAttrs,
    scope: &str,
) -> ScopeMembership<'a> {
    if attrs.expose.suppressed {
        return ScopeMembership::Absent;
    }

    let scalar_hit = attrs.expose.scalar_scopes.contains(scope);
    let relation_hit = attrs.expose.relation_scopes.get(scope);
    let rel_info = detect_relation(&field.ty);
    let is_relation = rel_info.is_some();

    match (scalar_hit, relation_hit, is_relation) {
        (false, None, _) => ScopeMembership::Absent,

        // Scalar form on scalar field — included.
        (true, None, false) => ScopeMembership::Scalar,

        // Scalar form on relation field — invalid.
        (true, None, true) => ScopeMembership::Reject {
            msg: "relation fields require an explicit peer visage name",
        },

        // Relation form on scalar field — invalid.
        (false, Some(_), false) => ScopeMembership::Reject {
            msg: "expose(scope -> ...) is only valid on relation fields",
        },

        // Parser rejects mixed scalar+relation on the same scope upstream.
        (true, Some(_), _) => ScopeMembership::Absent,

        // Relation form on relation field — included as an embed.
        (false, Some(exposure), true) => {
            let nullable = rel_info.map(|i| i.nullable).unwrap_or(false);
            ScopeMembership::RelationEmbed { exposure, nullable }
        }
    }
}

/// Build the path-aware peer fields path from a `RelationExposure`.
/// After, `{Model}Fields` is a ZST whose accessors return
/// `DjogiField<Self, V>` and the struct no longer carries `__djogi_path`
/// or `with_path`. To keep visage relation traversal compiling — which
/// composes a peer fields handle threaded with the FK column as a
/// SQL-alias path prefix — the helper distinguishes two cases by
/// inspecting the field's resolved relation info:
/// 1. **Full peer model**: `expose(scope -> Department)` where the path's
///    last segment matches the relation target ident. After PR3 the
///    full-peer route uses `{Department}SqlFields`, the path-aware
///    sibling that retains `__djogi_path` and `with_path`. Cached root
///    rows do not carry joined relation values, so traversal predicates
///    are SQL-only by construction; routing them through the SQL fields
///    view keeps cache and refresh boundaries free of relation paths
///    that would silently misclassify as portable.
/// 2. **Narrow visage**: `expose(scope -> Department::Public)` where the
///    path's last segment names a narrow visage type. Visage `{Visage}Fields`
///    keeps `__djogi_path` / `with_path` (it is its own struct, not the
///    root portable surface). Suffixing the path's last segment with
///    `Fields` continues to resolve the existing visage struct.
///    `field` is the relation field on the source model, used only to look
///    up the resolved relation target ident through `detect_relation`. When
///    the lookup fails (a non-relation field passed by mistake), the helper
///    falls back to the narrow-visage suffix to preserve the pre-PR3 shape;
///    the calling site (`visage_fields::expand`) already gates relation-form
///    emission behind `ScopeMembership::RelationEmbed`, so the fallback is
///    defensive rather than load-bearing.
pub(crate) fn peer_traversal_fields_path(
    field: &syn::Field,
    exposure: &RelationExposure,
) -> syn::Path {
    let mut path = exposure.peer.clone();
    if let Some(last) = path.segments.last_mut() {
        // Full-peer detection: when the exposure's last path segment
        // matches the relation target ident, the user wrote
        // `expose(scope -> Department)` with `Department` being the same
        // model the FK / O2O resolves to. The full-peer companion is now
        // `{Department}SqlFields` (path-aware SQL fields). Otherwise,
        // assume the user wrote a narrow-visage path
        // (`expose(scope -> Department::Public)`) and continue to suffix
        // with `Fields` to resolve the existing narrow visage's
        // `{NarrowVisage}Fields` struct.
        let suffix = match detect_relation(&field.ty) {
            Some(rel_info) if last.ident == rel_info.target_name => "SqlFields",
            _ => "Fields",
        };
        let fields_ident = format_ident!("{}{}", last.ident, suffix);
        last.ident = fields_ident;
        last.arguments = syn::PathArguments::None;
    }
    path
}

/// Whether the `exposure` targets the full peer model (as opposed to a
/// narrow visage). Used by `visages::emit_projection_for_scope` to decide
/// between a clone and a `TryFrom` dispatch.
pub(crate) fn is_full_peer_for(
    exposure: &RelationExposure,
    info: &crate::model::attrs::RelationInfo,
) -> bool {
    let Some(last) = exposure.peer.segments.last() else {
        return false;
    };
    last.ident == info.target_name
}
