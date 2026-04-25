//! Phase 7-Zero-2 T7 — `{Visage}Fields` + `{Visage}Filter` + `DjogiVisageOf<M>`.
//!
//! Each emitted visage gains a sibling `{Visage}Fields` accessor struct
//! (associated functions, one per exposed scalar) and a `{Visage}Filter`
//! placeholder struct. Non-exposed fields are ABSENT by construction — the
//! T7 compile-fail fixtures pin that; here we only assert the accessors are
//! reachable for scopes that DO expose the field.
//!
//! ## Design choice: `FieldRef<Model, V>` over `FieldRef<Visage, V>`
//!
//! The plan sketched `FieldRef<UserPublic, String>` — but `FieldRef<M, V>`
//! carries `M: Model`, and visages are not `Model` impls. Making visages
//! satisfy `Model` would be a large semantic change (visages are
//! projections, not tables). T7 therefore types the accessors on the
//! source model (`FieldRef<User, String>`) and seals the visage ↔ model
//! pairing separately via `DjogiVisageOf<M>`. T8 introduces the
//! visage-scoped traversal combinators; the surface type may evolve then.
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
    // UserPublicFields exists — emitted alongside UserPublic.
    let _public: UserPublicFields = UserPublicFields;

    // `display_name` IS in public scope → accessor is emitted.
    let _dn: FieldRef<User, String> = UserPublicFields::display_name();

    // `email` IS in self_view / admin / export scopes → accessors emitted
    // on those Fields types.
    let _em_sv: FieldRef<User, String> = UserSelfViewFields::email();
    let _em_ad: FieldRef<User, String> = UserAdminFields::email();
    let _em_ex: FieldRef<User, String> = UserExportFields::email();

    // Framework columns (id / created_at / updated_at) are always exposed.
    let _id: FieldRef<User, HeerIdDesc> = UserPublicFields::id();
    let _ca: FieldRef<User, DateTime> = UserPublicFields::created_at();

    // Placeholder {Visage}Filter types also exist.
    let _filter: UserPublicFilter = UserPublicFilter;
    let _filter_sv: UserSelfViewFilter = UserSelfViewFilter;
}
