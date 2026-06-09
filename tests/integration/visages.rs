//! Visage generation and conversion tests.
//!
//! Pure codegen + serde tests: no database, no tokio runtime needed.
//! Verifies:
//!
//! 1. Generated visage structs contain only exposed fields.
//! 2. Framework columns (`id`, `created_at`, `updated_at`) appear in every
//!    visage regardless of user annotations (Q13).
//! 3. `From<&Model>` preserves values for scalar fields.
//! 4. Serde round-trip via JSON keeps the visage shape intact and
//!    excludes non-exposed fields.

use djogi::prelude::*;

//  default flip — pin HeerId for the visage-roundtrip
// tests that construct `User` instances with explicit HeerId values.
#[model(table = "users_task3", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public, self_view, admin, export))]
    pub display_name: String,

    #[field(expose(self_view, admin, export))]
    pub email: String,

    // Absent — default internal; must NOT appear in any visage.
    pub password_hash: String,

    #[field(expose(admin))]
    pub internal_notes: Option<String>,
}

#[test]
fn user_public_shape_is_exhaustive() {
    // Compile-time check: destructuring UserPublic with the expected
    // fields is total. If codegen leaks `email` / `password_hash` /
    // `internal_notes` into UserPublic, rustc raises E0027 non-exhaustive.
    let _ = |u: &UserPublic| {
        let UserPublic {
            id,
            created_at,
            updated_at,
            display_name,
        } = u;
        let _ = (id, created_at, updated_at, display_name);
    };
}

#[test]
fn user_admin_shape_contains_internal_notes() {
    let _ = |u: &UserAdmin| {
        let UserAdmin {
            id,
            created_at,
            updated_at,
            display_name,
            email,
            internal_notes,
        } = u;
        let _ = (
            id,
            created_at,
            updated_at,
            display_name,
            email,
            internal_notes,
        );
    };
}

#[test]
fn user_self_view_excludes_internal_notes() {
    // self_view scope has display_name + email but not internal_notes
    // (which is admin-only). Non-exhaustive destructure with the full
    // expected set.
    let _ = |u: &UserSelfView| {
        let UserSelfView {
            id,
            created_at,
            updated_at,
            display_name,
            email,
        } = u;
        let _ = (id, created_at, updated_at, display_name, email);
    };
}

#[test]
fn from_impl_preserves_values() {
    let user = User::default();
    let public = UserPublic::from(&user);
    assert_eq!(public.display_name, user.display_name);
    assert_eq!(&public.id, &user.id);
    assert_eq!(public.created_at, user.created_at);
}

#[test]
fn serde_round_trip_public_excludes_password() {
    let user = User {
        display_name: "Alice".into(),
        email: "alice@example.com".into(),
        password_hash: "secret".into(),
        internal_notes: None,
        ..User::default()
    };
    let public = UserPublic::from(&user);
    let json = serde_json::to_string(&public).expect("serialize");

    // Non-exposed fields must not leak.
    assert!(!json.contains("secret"), "password_hash value leaked");
    assert!(!json.contains("password_hash"), "password_hash key leaked");
    assert!(
        !json.contains("alice@example.com"),
        "email leaked into public"
    );

    // Exposed fields must be present.
    assert!(json.contains("Alice"));
    assert!(json.contains("display_name"));

    let decoded: UserPublic = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded.display_name, "Alice");
}

#[test]
fn descriptor_projection_map_scalar_entries() {
    let desc = <User as ::djogi::prelude::Model>::descriptor();

    // `display_name` exposed to all 4 scopes — 4 entries, all pointing
    // at the column name.
    let display_name_field = desc
        .fields
        .iter()
        .find(|f| f.name == "display_name")
        .expect("display_name field in descriptor");
    let scopes: Vec<&str> = display_name_field
        .visage_map
        .iter()
        .map(|(s, _)| *s)
        .collect();
    assert!(scopes.contains(&"public"));
    assert!(scopes.contains(&"self_view"));
    assert!(scopes.contains(&"admin"));
    assert!(scopes.contains(&"export"));
    for (_, emit_as) in display_name_field.visage_map {
        assert_eq!(*emit_as, "display_name");
    }

    // `password_hash` has no expose — empty map.
    let pwd = desc
        .fields
        .iter()
        .find(|f| f.name == "password_hash")
        .expect("password_hash field in descriptor");
    assert_eq!(pwd.visage_map.len(), 0);

    // Framework `id` defaults to all 4 scopes (Q13).
    let id_field = desc
        .fields
        .iter()
        .find(|f| f.name == "id")
        .expect("id field in descriptor");
    assert_eq!(id_field.visage_map.len(), 4);
    for (_, emit_as) in id_field.visage_map {
        assert_eq!(*emit_as, "id");
    }
}

// ─────────────────────────────────────────────────────────────────
// Relation-nesting visages
// ─────────────────────────────────────────────────────────────────

#[model(table = "owners_task5", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Owner {
    #[field(expose(public, self_view, admin, export))]
    pub name: String,
}

#[model(table = "vehicles_task5", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Vehicle {
    #[field(expose(public, self_view, admin, export))]
    pub make: String,

    #[field(expose(public = "OwnerPublic", admin = "OwnerAdmin"))]
    pub owner: ForeignKey<Owner>,
}

#[test]
fn relation_nesting_tryfrom_fails_when_unresolved() {
    use djogi::HeerId;
    let v = Vehicle {
        id: HeerId::from_i64(10).expect("valid heerid"),
        created_at: ::djogi::DateTime::UNIX_EPOCH,
        updated_at: ::djogi::DateTime::UNIX_EPOCH,
        make: "Tesla".into(),
        owner: ForeignKey::new(HeerId::from_i64(1).expect("valid heerid")),
    };
    let err = VehiclePublic::try_from(&v).expect_err("expected UnresolvedRelation");
    // VisageError is #[non_exhaustive]; match must carry a wildcard
    // to stay forward-compatible with future additions.
    match err {
        ::djogi::VisageError::UnresolvedRelation {
            model,
            field,
            scope,
        } => {
            assert_eq!(model, "Vehicle");
            assert_eq!(field, "owner");
            assert_eq!(scope, "public");
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

#[test]
fn scalar_peer_has_from_impl_available() {
    let o = Owner {
        name: "Alice".into(),
        ..Owner::default()
    };
    let p = OwnerPublic::from(&o);
    assert_eq!(p.name, "Alice");
}

#[test]
fn serde_round_trip_admin_includes_internal_notes() {
    let user = User {
        display_name: "Bob".into(),
        email: "bob@example.com".into(),
        password_hash: "secret".into(),
        internal_notes: Some("flagged for review".into()),
        ..User::default()
    };
    let admin = UserAdmin::from(&user);
    let json = serde_json::to_string(&admin).expect("serialize");

    assert!(json.contains("Bob"));
    assert!(json.contains("flagged for review"));
    assert!(json.contains("bob@example.com"));
    // password_hash remains excluded even in admin scope.
    assert!(!json.contains("secret"), "password_hash leaked into admin");
}
