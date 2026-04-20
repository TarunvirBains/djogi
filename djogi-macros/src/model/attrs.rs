//! Attribute parsing for `#[model(table = "...", pk = "...")]`
//! and `#[field(unique, index, max_length = N, renamed_from = "...", on_delete = "...")]`.
//!
//! `ModelAttrs` keeps a hand-rolled parser: the surface is three keys, the
//! error messages from `syn::Error::new_spanned` already carry precise
//! source spans, and there is no incentive to grow it.
//!
//! `FieldAttrs` parses via `darling::FromField`. Per-field attrs grow over
//! time (later phases add `db_column`, `choices`, `validators`, etc.), and
//! darling's declarative derive gives us span-aware errors for unknown
//! keys, type mismatches, and duplicate keys for free — matching the prior
//! hand-rolled behaviour without each new key duplicating the same
//! `Meta::NameValue` match arm.

use darling::FromField;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Expr, ExprLit, Lit, Meta, MetaNameValue, Token};

/// Options extracted from `#[model(table = "...", pk = "...")]`.
// Fields are read by Tasks 4–9 (inject, crud, descriptor, stubs). The
// dead-code lint fires now because those callers are stubs — suppress it.
#[allow(dead_code)]
#[derive(Debug)]
pub struct ModelAttrs {
    /// SQL table name, e.g. `"posts"`.
    pub table: String,
    /// Primary key strategy.
    pub pk: PkStrategy,
    /// When `true`, the macro skips generating the `Default` impl.
    ///
    /// Use `#[model(table = "...", no_default)]` for models that contain
    /// field types that do not implement `Default` (e.g. `time::Date`).
    /// Without this flag the generated `Default` impl would fail to compile.
    /// Users must then initialise all fields explicitly instead of relying
    /// on struct-update syntax (`..Model::default()`).
    pub no_default: bool,
    /// When `true`, this model is a many-to-many through / junction model.
    ///
    /// Set via `#[model(table = "...", through)]`. The flag flows through
    /// to [`ModelDescriptor::is_through`](djogi::descriptor::ModelDescriptor::is_through)
    /// at codegen time, where it acts as a marker that:
    ///
    /// - This table is the junction for a specific `impl ManyToMany<Target> for Source`.
    /// - Phase 6's migration differ may later suppress standalone-model
    ///   admin/routing affordances for through tables (deferred).
    ///
    /// Through models remain ordinary queryable `Model`s — the flag is
    /// documentation and future differentiation, not a structural
    /// constraint. Users still `#[derive(Model)]` them as normal and
    /// query them with the standard `QuerySet` API.
    pub through: bool,
    /// When `true`, this model emits a transactional outbox row on every
    /// successful `create` / `save` / `delete` performed through a
    /// `DjogiContext`.
    ///
    /// Set via `#[model(table = "...", events)]`. The flag flows through
    /// to [`ModelDescriptor::has_outbox`](djogi::descriptor::ModelDescriptor::has_outbox)
    /// at codegen time, where `djogi::outbox::emit_event` keys off it to
    /// decide whether to write to `{table}_outbox` inside the active
    /// transaction. Phase 4 Task 6 lands the CRUD-side emission; macro-
    /// side DDL emission to `target/djogi_outbox/{table}_outbox.sql` (so
    /// the Phase 7 migration differ can consume it) is **deferred**. For
    /// now, downstream crates hand-write the `{table}_outbox` DDL
    /// alongside their own migrations.
    ///
    /// Models without `events` skip the outbox call entirely at
    /// macro-expansion time — there is no runtime cost for opt-out.
    pub events: bool,
}

/// Parsed `pk = "..."` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkStrategy {
    HeerId,
    RanjId,
    Serial,
    None,
}

impl ModelAttrs {
    /// Parse `#[model(table = "posts", pk = "heerid")]` from the attribute token stream.
    ///
    /// Duplicate keys are rejected with a span-carrying error pointing at the
    /// second occurrence — last-write-wins silently is a footgun in proc-macro
    /// UX (users can't see which key won without expanding the macro).
    pub fn parse(attr_tokens: proc_macro2::TokenStream) -> syn::Result<Self> {
        let metas = Punctuated::<Meta, Token![,]>::parse_terminated.parse2(attr_tokens)?;

        let mut table: Option<String> = Option::None;
        let mut pk: Option<PkStrategy> = Option::None;
        let mut no_default = false;
        let mut seen_no_default = false;
        let mut through = false;
        let mut seen_through = false;
        let mut events = false;
        let mut seen_events = false;

        for meta in &metas {
            match meta {
                // Flag-only attribute: `no_default`
                Meta::Path(path) if path.is_ident("no_default") => {
                    if seen_no_default {
                        return Err(syn::Error::new_spanned(
                            path,
                            "duplicate `no_default` flag in #[model(...)]",
                        ));
                    }
                    seen_no_default = true;
                    no_default = true;
                }
                // Flag-only attribute: `through`
                Meta::Path(path) if path.is_ident("through") => {
                    if seen_through {
                        return Err(syn::Error::new_spanned(
                            path,
                            "duplicate `through` flag in #[model(...)]",
                        ));
                    }
                    seen_through = true;
                    through = true;
                }
                // Flag-only attribute: `events`
                Meta::Path(path) if path.is_ident("events") => {
                    if seen_events {
                        return Err(syn::Error::new_spanned(
                            path,
                            "duplicate `events` flag in #[model(...)]",
                        ));
                    }
                    seen_events = true;
                    events = true;
                }
                Meta::NameValue(MetaNameValue {
                    path,
                    value:
                        Expr::Lit(ExprLit {
                            lit: Lit::Str(s), ..
                        }),
                    ..
                }) => {
                    if path.is_ident("table") {
                        if table.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `table` key in #[model(...)]",
                            ));
                        }
                        table = Some(s.value());
                    } else if path.is_ident("pk") {
                        if pk.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `pk` key in #[model(...)]",
                            ));
                        }
                        pk = Some(
                            PkStrategy::from_str(&s.value())
                                .map_err(|msg| syn::Error::new_spanned(s, msg))?,
                        );
                    } else {
                        return Err(syn::Error::new_spanned(
                            path,
                            format!(
                                "unknown #[model] attribute `{}`; expected `table`, `pk`, `no_default`, `through`, or `events`",
                                path.get_ident().map(|i| i.to_string()).unwrap_or_default()
                            ),
                        ));
                    }
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "expected `key = \"value\"` attribute or bare flag (`no_default`, `through`, `events`)",
                    ));
                }
            }
        }

        let table = table.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "#[model] requires `table = \"...\"`",
            )
        })?;
        let pk = pk.unwrap_or(PkStrategy::HeerId);

        Ok(ModelAttrs {
            table,
            pk,
            no_default,
            through,
            events,
        })
    }
}

impl PkStrategy {
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "heerid" => Ok(PkStrategy::HeerId),
            "ranjid" => Ok(PkStrategy::RanjId),
            "serial" => Ok(PkStrategy::Serial),
            "none" => Ok(PkStrategy::None),
            other => Err(format!(
                "unknown pk strategy `{other}`; expected one of: heerid, ranjid, serial, none"
            )),
        }
    }
}

/// Options extracted from a single `#[field(...)]` annotation on a struct field.
///
/// Parsed via `darling::FromField`. Unknown keys, type mismatches, and
/// duplicate keys are reported by darling with source spans. `ident` and
/// `ty` are darling "magic" fields — the derive auto-populates them from
/// `syn::Field::{ident, ty}` at call time, independent of the attribute
/// list — so [`FieldAttrs::parse`] callers can read them alongside the
/// parsed attrs without threading the `syn::Field` separately.
// Not every field is read on every call site (e.g. `ident`/`ty` are pending
// use by later Phase 2 / Phase 3 codegen). Suppress dead_code at struct
// granularity so new fields don't spuriously re-trip the lint.
#[allow(dead_code)]
#[derive(Debug, FromField)]
#[darling(attributes(field))]
pub struct FieldAttrs {
    /// The struct field's identifier.
    ///
    /// Darling's `FromField` derive auto-populates this from
    /// `syn::Field::ident` by magic field name. Always `Some(_)` for
    /// named-field structs; tuple/unit structs are rejected earlier in
    /// `inject::expand`.
    pub ident: Option<syn::Ident>,
    /// The struct field's Rust type.
    ///
    /// Darling's `FromField` derive auto-populates this from
    /// `syn::Field::ty` by magic field name. The type must be `syn::Type`
    /// (not `Option<syn::Type>`) because the derive emits
    /// `ty: field.ty.clone()` verbatim.
    pub ty: syn::Type,

    /// `#[field(unique)]` — emits a `UNIQUE` constraint in migrations.
    #[darling(default)]
    pub unique: bool,
    /// `#[field(index)]` — emits a `CREATE INDEX` in migrations.
    #[darling(default)]
    pub index: bool,
    /// `#[field(max_length = N)]` — caps `TEXT` columns at `VARCHAR(N)`.
    #[darling(default)]
    pub max_length: Option<u32>,
    /// `#[field(renamed_from = "old_name")]` — column rename hint for migrations.
    #[darling(default)]
    pub renamed_from: Option<String>,
    /// `#[field(on_delete = "...")]` — only valid on `ForeignKey<T>` fields.
    /// Accepted values: cascade, restrict, set_null, set_default, protect,
    /// do_nothing. Darling validates the literal is a string;
    /// [`FieldAttrs::parse`] post-validates the value is in the accepted
    /// set (darling's derive alone cannot constrain a `String` domain).
    #[darling(default)]
    pub on_delete: Option<String>,
    /// `#[field(outbox = "ignore")]` — strip this column from the
    /// transactional outbox payload emitted by models with
    /// `#[model(events)]`.
    ///
    /// Only valid value today is `"ignore"` (the field name appears in
    /// the outbox-exclude set for Phase 4 Task 6). The single-valued
    /// enum shape lives behind a string literal so future additions
    /// (`outbox = "encrypt"`, `outbox = "hash"`, etc.) slot in without
    /// reshaping the macro surface. [`FieldAttrs::parse`] post-validates
    /// the literal against the accepted set.
    #[darling(default)]
    pub outbox: Option<String>,
}

impl FieldAttrs {
    /// Parse `#[field(...)]` from a struct field.
    ///
    /// Returns an all-default instance if no `#[field]` attr is present
    /// (darling's `#[darling(default)]` container attr handles the no-attr
    /// case). Darling emits span-aware errors for:
    /// - Unknown attribute keys (e.g. `#[field(nonexistent)]`).
    /// - Type mismatches (e.g. `max_length = "x"` where an integer is required).
    /// - Duplicate keys across multiple `#[field(...)]` attrs.
    ///
    /// `on_delete` is a string with a constrained value set that darling's
    /// type-level parsing cannot enforce. We post-validate the value below
    /// and — when rejecting — walk the field's raw `#[field(...)]` attrs
    /// to recover the literal's `Span`, so the error underlines the bad
    /// value rather than the entire field declaration. Matches the pre-
    /// darling hand-rolled behaviour; keeps the surface consistent with
    /// how `pk = "..."` span-points at its own literal in `ModelAttrs`.
    pub fn parse(field: &syn::Field) -> syn::Result<Self> {
        // `darling::Error` carries source spans from the originating
        // attribute tokens; `From<darling::Error> for syn::Error` preserves
        // them, so rely on the built-in conversion rather than collapsing
        // everything onto the whole field with `new_spanned`.
        let attrs = <Self as darling::FromField>::from_field(field).map_err(syn::Error::from)?;

        if let Some(on_delete) = &attrs.on_delete {
            let valid = [
                "cascade",
                "restrict",
                "set_null",
                "set_default",
                "protect",
                "do_nothing",
            ];
            if !valid.contains(&on_delete.as_str()) {
                // Locate the original `on_delete = "..."` literal in the
                // field's raw attribute tokens so the error carries the
                // literal's span, not the whole field's. Falls back to the
                // field span if the structure is unexpected — darling only
                // hands us a `String`, so the only way to recover the span
                // is a second walk. This is cheap: one field has at most a
                // handful of attrs, each with a handful of keys.
                let span = find_on_delete_lit_span(field).unwrap_or_else(|| field.span());
                return Err(syn::Error::new(
                    span,
                    format!(
                        "unknown on_delete value `{on_delete}`; expected one of: {}",
                        valid.join(", ")
                    ),
                ));
            }
        }

        if let Some(outbox) = &attrs.outbox {
            // Only `"ignore"` is accepted today; future Phase 6+ values
            // (`"encrypt"`, `"hash"`) would slot into this list without
            // reshaping the attr surface. Mirrors the on_delete span
            // recovery pattern so the error underlines the literal,
            // not the whole field.
            let valid = ["ignore"];
            if !valid.contains(&outbox.as_str()) {
                let span = find_named_str_lit_span(field, "outbox").unwrap_or_else(|| field.span());
                return Err(syn::Error::new(
                    span,
                    format!(
                        "unknown outbox value `{outbox}`; expected one of: {}",
                        valid.join(", ")
                    ),
                ));
            }
        }

        Ok(attrs)
    }
}

/// Walk the raw `#[field(...)]` attrs on `field` and return the `Span` of
/// the literal bound to `on_delete`, if present.
///
/// Thin wrapper around [`find_named_str_lit_span`] kept for readability at
/// the callsite.
fn find_on_delete_lit_span(field: &syn::Field) -> Option<proc_macro2::Span> {
    find_named_str_lit_span(field, "on_delete")
}

/// Walk the raw `#[field(...)]` attrs on `field` and return the `Span` of
/// the string literal bound to `key`, if present.
///
/// Used by [`FieldAttrs::parse`] to recover the offending literal's span
/// after darling has already reduced the attribute to a `String`. Returns
/// `None` only if the attribute structure does not match the expected
/// `key = "literal"` shape — darling's own parse will have rejected that
/// upstream, so the caller falls back to the field's span as a last
/// resort without losing the error. Shared between `on_delete` and
/// `outbox` validation to keep the span-recovery logic in one place.
fn find_named_str_lit_span(field: &syn::Field, key: &str) -> Option<proc_macro2::Span> {
    for attr in &field.attrs {
        if !attr.path().is_ident("field") {
            continue;
        }
        let metas = attr
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .ok()?;
        for meta in &metas {
            if let Meta::NameValue(MetaNameValue {
                path,
                value:
                    Expr::Lit(ExprLit {
                        lit: lit @ Lit::Str(_),
                        ..
                    }),
                ..
            }) = meta
                && path.is_ident(key)
            {
                return Some(lit.span());
            }
        }
    }
    None
}

/// The SQL column type for a Rust type string.
///
/// Returns `None` for `Option<T>` (handled by the caller via `unwrap_option`) and
/// for unrecognized types (the caller should emit a compile error).
///
/// Called by `descriptor::expand` and `inject::expand` starting in Tasks 4–6.
#[allow(dead_code)]
pub fn rust_type_to_sql(ty: &syn::Type) -> Option<&'static str> {
    let s = quote::quote!(#ty).to_string().replace(' ', "");
    match s.as_str() {
        "String" => Some("TEXT"),
        "i16" => Some("SMALLINT"),
        "i32" => Some("INTEGER"),
        "i64" => Some("BIGINT"),
        "f32" => Some("REAL"),
        "f64" => Some("DOUBLE PRECISION"),
        "bool" => Some("BOOLEAN"),
        "DateTime" | "time::OffsetDateTime" | "OffsetDateTime" => Some("TIMESTAMPTZ"),
        "Date" | "time::Date" => Some("DATE"),
        "Decimal" | "rust_decimal::Decimal" => Some("NUMERIC"),
        "Uuid" | "uuid::Uuid" => Some("UUID"),
        "serde_json::Value" | "Value" => Some("JSONB"),
        "Vec<String>" => Some("TEXT[]"),
        "Vec<i32>" => Some("INTEGER[]"),
        "Vec<i64>" => Some("BIGINT[]"),
        "Vec<bool>" => Some("BOOLEAN[]"),
        // Option<T> is handled at call site — strip and recurse via unwrap_option
        _ if s.starts_with("Option<") => None,
        _ => None,
    }
}

/// Convert a pre-validated `on_delete` string into the matching
/// `::djogi::OnDelete` token path.
///
/// Caller contract: `s` must be one of the six validated values — any other
/// input is a bug in the caller, since [`FieldAttrs::parse`] has already
/// rejected out-of-domain strings with a span-carrying error. The function
/// falls back to `OnDelete::Restrict` on an unrecognized value so a caller
/// skipping the validator does not produce a bogus token stream; that
/// matches the framework's cascade-off-by-default posture and keeps the
/// descriptor emitter total.
///
/// Lives here rather than in `descriptor.rs` because `descriptor.rs` can
/// stay schema-focused: the attr crate owns the mapping between
/// `#[field(on_delete = "...")]` string values and runtime enum variants,
/// and descriptor emission calls through this helper.
#[allow(dead_code)]
pub fn on_delete_str_to_tokens(s: &str) -> proc_macro2::TokenStream {
    match s {
        "cascade" => quote::quote! { ::djogi::descriptor::OnDelete::Cascade },
        "restrict" => quote::quote! { ::djogi::descriptor::OnDelete::Restrict },
        "set_null" => quote::quote! { ::djogi::descriptor::OnDelete::SetNull },
        "set_default" => quote::quote! { ::djogi::descriptor::OnDelete::SetDefault },
        "protect" => quote::quote! { ::djogi::descriptor::OnDelete::Protect },
        "do_nothing" => quote::quote! { ::djogi::descriptor::OnDelete::DoNothing },
        // Fallback: anything else is out of the validator's domain and the
        // caller is expected to have rejected it. Emit `Restrict` (the
        // framework default) so the macro output is still well-formed
        // tokens rather than a half-written ident.
        _ => quote::quote! { ::djogi::descriptor::OnDelete::Restrict },
    }
}

/// Relation cardinality as seen from a Rust field type inside the macro crate.
///
/// This enum is the macro-crate mirror of `djogi::relation::RelationKind`:
/// `djogi-macros` cannot depend on `djogi` (the macro is compiled first),
/// so when the macro wants to *reason* about a relation shape before
/// emitting tokens, it works with this local copy. The emitter converts to
/// `::djogi::relation::RelationKind::…` token paths at code-generation
/// time — the two enums only need to stay in sync on the set of variants.
///
/// Phase 3 Task 2 covers `ForeignKey<T>` and `OneToOneField<T>` (plus their
/// `Option<…>` nullable forms). Task 7's `ManyToMany` variant is
/// intentionally absent here: the M2M macro pipeline recognizes M2M
/// through-models by a separate mechanism (a marker trait on the through
/// struct, not a field type on the source struct), so no macro-side
/// detection is needed in this helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    /// `ForeignKey<T>` — many-to-one.
    ForeignKey,
    /// `OneToOneField<T>` — one-to-one, unique-constrained.
    OneToOne,
}

/// Everything a macro caller needs to know about a detected relation field.
///
/// Returned by [`detect_relation`]. Two facets of the relation target are
/// carried separately because they have different consumers:
///
/// - [`target_name`](Self::target_name) is a short, path-free string used
///   only for the descriptor field `target_type_name` (Phase 6's migration
///   emitter looks models up by this name against the `inventory`-registered
///   descriptors, so keeping it segmented matches that lookup key).
/// - [`target_type`](Self::target_type) is the **full `syn::Type`** the user
///   wrote inside the wrapper (`Owner`, `models::Owner`, `crate::models::Owner`,
///   or even `inner::Widget`). Emitted verbatim into positions such as
///   `RelationPath<#source, #target_type>` and `<#target_type as Model>::…`
///   so fully-qualified paths resolve at the user's macro-call site without
///   requiring an extra `use …;` import.
///
/// Splitting the two lets the emitter use the right form in the right place —
/// collapsing down to just the last-segment ident (as the previous
/// `(RelationKind, String, bool)` tuple did) silently broke codegen for any
/// target type the user spelled with a path prefix.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RelationInfo {
    /// FK vs O2O — drives both the emitted `RelationKind` variant and the
    /// descriptor-side `relation_kind` field.
    pub kind: RelationKind,
    /// Short name for descriptor use (e.g. `"Owner"` from any of:
    /// `Owner`, `models::Owner`, `crate::models::Owner`). Always the last
    /// segment ident of the inner type path — this is the name Phase 6's
    /// migration differ matches against `ModelDescriptor::type_name`.
    pub target_name: String,
    /// Full target type preserved for codegen. Use in `quote! { #target_type }`
    /// positions so fully-qualified paths like
    /// `ForeignKey<crate::models::Owner>` still emit a resolvable
    /// `RelationPath<Self, crate::models::Owner>` without requiring a
    /// separate `use crate::models::Owner;` at the call site.
    pub target_type: syn::Type,
    /// `true` when the outermost wrapper was `Option<…>` (nullable column).
    pub nullable: bool,
}

/// Inspect a field's declared Rust type and decide whether it names a Djogi
/// relation wrapper.
///
/// Returns [`Some(RelationInfo)`](RelationInfo) when the outermost
/// (post-`Option<…>`-strip) type is one of the recognized relation wrappers:
///
/// - `ForeignKey<T>` → `Some(RelationInfo { kind: ForeignKey, …, nullable: false })`
/// - `Option<ForeignKey<T>>` → `Some(RelationInfo { kind: ForeignKey, …, nullable: true })`
/// - `OneToOneField<T>` → `Some(RelationInfo { kind: OneToOne, …, nullable: false })`
/// - `Option<OneToOneField<T>>` → `Some(RelationInfo { kind: OneToOne, …, nullable: true })`
/// - anything else → `None`
///
/// # Robustness
///
/// The check inspects the *last* path segment's ident, which makes it
/// resilient to fully-qualified user spellings like
/// `::djogi::relation::ForeignKey<Owner>` or `djogi::ForeignKey<Owner>` —
/// users who reach for an explicit path instead of `use djogi::prelude::*`
/// still get the correct relation detection. The trade-off: a user-defined
/// type with the literal name `ForeignKey` shadows the detection. That
/// matches how `unwrap_option` already handles `Option`: the macro works
/// from the spelling the user wrote, and the behaviour is consistent and
/// easy to reason about.
///
/// # Target preservation
///
/// Both the short `target_name` and the full `target_type` are preserved.
/// Consumers that emit `type`-position tokens (e.g. `relations::expand`)
/// must use `target_type` so that `ForeignKey<crate::models::Owner>` still
/// emits a resolvable `RelationPath<_, crate::models::Owner>`; consumers
/// that only need the short name for descriptor lookup (`descriptor::expand`)
/// use `target_name`.
#[allow(dead_code)]
pub fn detect_relation(ty: &syn::Type) -> Option<RelationInfo> {
    // First, strip one layer of `Option<…>` using the shared helper. The
    // returned `nullable` flag propagates straight through to the caller —
    // it's the same nullability semantic.
    let (stripped, nullable) = unwrap_option(ty);

    // The stripped type must be a path type like `ForeignKey<T>` or
    // `some::path::ForeignKey<T>`. Anything else (references, tuples,
    // arrays, fn pointers) is a scalar column from the macro's POV.
    let syn::Type::Path(syn::TypePath { path, .. }) = &stripped else {
        return None;
    };

    // Inspect the last segment only — robust to fully-qualified spellings.
    let last = path.segments.last()?;
    let kind = match last.ident.to_string().as_str() {
        "ForeignKey" => RelationKind::ForeignKey,
        "OneToOneField" => RelationKind::OneToOne,
        _ => return None,
    };

    // Extract the single generic argument's type. The segment must carry
    // `<T>`; a bare `ForeignKey` with no type parameter is a user error
    // that earlier parsing (or the later descriptor emission) will surface
    // with a better error — return `None` here rather than panicking the
    // macro.
    let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    let syn::GenericArgument::Type(inner) = args.args.first()? else {
        return None;
    };

    // The target's short name is the last segment of the inner type's path
    // (`Owner` out of `crate::models::Owner`, or just `Owner` out of
    // `Owner`). Non-path inner types (e.g. references) aren't legal relation
    // targets — the `Model` bound on `ForeignKey<T>` would reject them at
    // the type-check stage — so return `None` and let the normal compile
    // error surface in the user's code.
    let syn::Type::Path(syn::TypePath {
        path: inner_path, ..
    }) = inner
    else {
        return None;
    };
    let target_name = inner_path.segments.last()?.ident.to_string();

    // Preserve the full inner `syn::Type` for codegen — emitters that place
    // the target in type position must use this, not the stringified short
    // name, so fully-qualified paths (e.g. `crate::models::Owner`) resolve
    // at the macro-call site without requiring a separate `use` import.
    let target_type = inner.clone();

    Some(RelationInfo {
        kind,
        target_name,
        target_type,
        nullable,
    })
}

/// Strip `Option<T>` → returns the inner type and `nullable = true`.
///
/// Only recognizes the prelude's `Option` — specifically the path forms
/// `Option<T>`, `std::option::Option<T>`, and `core::option::Option<T>`. A user
/// type that happens to be named `Option` in their own module (e.g.
/// `my_crate::Option<T>`) is left unchanged, because treating it as nullable
/// silently would produce wrong migrations. This matches how users actually
/// read the type: `Option<T>` in the prelude means "SQL NULL allowed"; anything
/// else is a user type that must map via `rust_type_to_sql`.
///
/// Non-`Option` types are returned unchanged with `nullable = false`.
///
/// Called by `inject::expand` and `descriptor::expand` starting in Tasks 4–6.
#[allow(dead_code)]
pub fn unwrap_option(ty: &syn::Type) -> (syn::Type, bool) {
    if let syn::Type::Path(syn::TypePath { path, .. }) = ty
        && is_prelude_option_path(path)
        && let Some(last) = path.segments.last()
        && let syn::PathArguments::AngleBracketed(args) = &last.arguments
        && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
    {
        return (inner.clone(), true);
    }
    (ty.clone(), false)
}

/// True if `path` is one of the three canonical prelude `Option` forms:
/// bare `Option`, `std::option::Option`, or `core::option::Option`.
fn is_prelude_option_path(path: &syn::Path) -> bool {
    let segs: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
    match segs.as_slice() {
        [sole] => sole == "Option",
        [root, module, ty] => {
            (root == "std" || root == "core") && module == "option" && ty == "Option"
        }
        _ => false,
    }
}
