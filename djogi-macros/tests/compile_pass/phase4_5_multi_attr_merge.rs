//! Phase 4.5 — multiple `#[field(expose(...))]` attributes on the same
//! field must merge without error as long as scopes don't collide
//! across the attribute list. This pins the cross-attr merge path in
//! `FieldAttrs::parse` that walks raw `field.attrs` and folds every
//! `expose(...)` meta it finds into a single `ExposeSpec`.
//!
//! If a future refactor breaks the merge (e.g. second attribute
//! overwrites the first instead of merging), this fixture stops
//! compiling and trybuild surfaces it.
use djogi::prelude::*;

// Phase 7-Zero-2 T2 flipped the default PK to `HeerIdRecencyBiased`;
// explicit `pk = HeerId` keeps the `HeerId::from_i64(1)` construction
// below type-compatible with the injected `id` field.
#[model(table = "users_multi_attr_merge", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct User {
    // Two attrs, disjoint scope sets — should merge into {public, admin}.
    #[field(expose(public))]
    #[field(expose(admin))]
    pub display_name: String,

    // Three attrs, disjoint scopes across scalar forms.
    #[field(expose(public))]
    #[field(expose(self_view))]
    #[field(expose(export))]
    pub email: String,
}

fn main() {
    // Round-trip the generated scalar-only projection through the
    // emitted `From<&User>` impl to confirm codegen observed the merged
    // scope set (display_name appears in both UserPublic and UserAdmin).
    let u = User {
        id: HeerId::from_i64(1).expect("valid heerid"),
        created_at: ::djogi::DateTime::UNIX_EPOCH,
        updated_at: ::djogi::DateTime::UNIX_EPOCH,
        display_name: "alice".into(),
        email: "a@b.c".into(),
    };
    let _: UserPublic = (&u).into();
    let _: UserAdmin = (&u).into();
    let _: UserSelfView = (&u).into();
    let _: UserExport = (&u).into();
}
