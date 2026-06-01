//! `{Visage}Fields` + `{Visage}Filter` + `DjogiVisageOf<M>`.
//!
//! Each emitted visage gains a sibling `{Visage}Fields` accessor struct
//! (one inherent method per exposed scalar) and a `{Visage}Filter`
//! placeholder struct. Non-exposed fields are ABSENT by construction — the
//! T7 compile-fail fixtures pin that; here we only assert the accessors are
//! reachable for scopes that DO expose the field.
//!
//! ## T8 shift — state-carrying `{Visage}Fields`
//!
//! T7 emitted `{Visage}Fields` as a unit-struct ZST with *associated
//! functions* (`UserPublicFields::display_name()`). T8 converts the
//! struct to a state-carrying value with *inherent methods*
//! (`fields.display_name()`) so relation-traversal chains can thread a
//! SQL-alias path through the peer's `Fields`. This fixture is updated
//! to the T8 shape: `UserPublicFields::default()` constructs the root
//! handle and the accessors are called via `.method()` on that handle.
//!
//! ## Design choice: `FieldRef<Model, V>` over `FieldRef<Visage, V>`
//!
//! `FieldRef<M, V>` carries `M: Model`, and visages are not `Model`
//! impls. Making visages satisfy `Model` would be a large semantic
//! change (visages are projections, not tables). The accessors are
//! typed on the source model (`FieldRef<User, String>`) and the
//! visage ↔ model pairing is sealed separately via `DjogiVisageOf<M>`.
use djogi::prelude::*;

#[model(table = "users_t7_visage_fields")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public, self_view, admin, export))]
    pub display_name: String,
    #[field(expose(self_view, admin, export))]
    pub email: String,
    #[field(expose(none))]
    pub password_hash: String,
}

fn main() {
    // UserPublicFields exists — emitted alongside UserPublic. Default
    // construction gives the root handle (no SQL-alias path).
    let public: UserPublicFields = UserPublicFields::default();

    // `display_name` IS in public scope → accessor is emitted.
    let _dn: FieldRef<User, String> = public.display_name();

    // `email` IS in self_view / admin / export scopes → accessors emitted
    // on those Fields types. Same method-on-handle shape.
    let _em_sv: FieldRef<User, String> = UserSelfViewFields::default().email();
    let _em_ad: FieldRef<User, String> = UserAdminFields::default().email();
    let _em_ex: FieldRef<User, String> = UserExportFields::default().email();

    // Framework columns (id / created_at / updated_at) are always exposed.
    let _id: FieldRef<User, HeerIdDesc> = public.id();
    let _ca: FieldRef<User, DateTime> = public.created_at();

    // Placeholder {Visage}Filter types also exist.
    let _filter: UserPublicFilter = UserPublicFilter;
    let _filter_sv: UserSelfViewFilter = UserSelfViewFilter;
}
