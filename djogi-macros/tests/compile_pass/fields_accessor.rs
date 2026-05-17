// Verifies that `#[model]` emits a real `{Model}Fields` bag with per-column
// `DjogiField` accessors whose `V` generic matches the column's declared Rust
// type. Covers:
//
//   - framework columns (`id`, `created_at`, `updated_at`) return the expected
//     `DjogiField<User, ...>` shapes and support portable lookups;
//   - user-declared columns round-trip their types verbatim (`String`, `i32`,
//     `Option<String>`);
//   - string-only lookups (`.contains`) resolve on `DjogiField<User, String>`;
//   - raw identifiers (`r#type`) round-trip as a method name and produce a
//     SQL-safe column string (`"type"`, never `"r#type"`);
//   - the emission compiles for `pk = Serial` (different `id` type).
//
// TODO: add Jsonb + ForeignKey fixtures once those types land (Phase 3+).
//
// `pk = None` is deliberately NOT exercised here: `crud::expand` does not
// emit `impl Model` for those models (see the module docs), and `DjogiField<M,
// V>` requires `M: Model`. `stubs::expand` mirrors that gate and emits only
// the empty `{Model}Fields` shell for pk=none — there are no accessors to
// test. A future phase introducing a composite-PK trait will unlock them.
//
// A compile-fail counterpart for mismatched value types is deferred: the
// existing `DjogiField` / `FieldRef` unit tests in `djogi/src/query/field.rs` exercise the
// `IntoFilterValue` mismatch paths, and duplicating them here would just
// re-test the same trait bound.

use djogi::prelude::*;
use djogi::query::{DjogiField, PortablePredicate};

// Phase 7-Zero-2 T2 flipped the default PK to `HeerIdRecencyBiased`; this
// fixture continues to exercise the ascending-HeerId accessor shape via
// an explicit `pk = HeerId` annotation.
//
// `pub struct` is required so the macro-emitted `pub` visages
// (`UserPublic`, `UserSelfView`, ...) carrying `type Model = User`
// satisfy rustc's private-in-public check — Phase 8.5 #231
// reconciliation pins `type Model: Model` on `DjogiVisage`.
#[model(table = "users", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct User {
    pub name: String,
    pub email: String,
    pub age: i32,
    pub bio: Option<String>,
    // Raw identifier: `type` is a Rust keyword but a common SQL column name.
    // The emitted method must be `r#type()`, but the underlying column
    // literal passed to `FieldRef::new(...)` must be `"type"` (no `r#`).
    pub r#type: String,
}

#[model(table = "lookups", pk = Serial)]
#[derive(Debug, Clone)]
pub struct Lookup {
    pub label: String,
}

fn _framework_accessors_typecheck() {
    let f = UserFields::default();
    // Framework columns: `id` is HeerId (default pk), timestamps are DateTime.
    let _id: DjogiField<User, HeerId> = f.id();
    let _created: DjogiField<User, DateTime> = f.created_at();
    let _updated: DjogiField<User, DateTime> = f.updated_at();
}

fn _user_accessors_typecheck() {
    let f = UserFields::default();
    let _name: DjogiField<User, String> = f.name();
    let _email: DjogiField<User, String> = f.email();
    let _age: DjogiField<User, i32> = f.age();
    // Nullable column: the `V` generic carries the `Option<T>` wrapper
    // verbatim so `.eq(Some(...))` / `.is_null()` type-check as expected.
    let _bio: DjogiField<User, Option<String>> = f.bio();
    // Raw-identifier method: `r#type()` is how the user writes it in Rust.
    // The returned `FieldRef`'s `V` generic is the plain `String` from the
    // field's declared type.
    let _ty: DjogiField<User, String> = f.r#type();
}

fn _raw_ident_column_literal_strips_prefix() {
    // This is the acceptance check for the `r#` stripping fix in
    // `stubs.rs`: `FieldRef::column()` returns the SQL column string
    // passed to `FieldRef::new(...)`. If the macro emits `"r#type"` the
    // assertion fires at runtime; the point of the fixture is to pin the
    // fix so regressing to `ident.to_string()` (unstripped) breaks the
    // test suite loudly rather than silently producing bogus SQL.
    let col = UserFields::default().r#type().column();
    assert_eq!(col, "type");
}

fn _lookup_values_compile() {
    // The core acceptance check: a filter closure can build a PortablePredicate by
    // chaining a lookup off a `UserFields::default()` accessor. If the
    // accessor's `V` generic is wrong, `.eq` / `.contains` fails to resolve.
    let f = UserFields::default();
    let _: PortablePredicate<User> = f.name().eq("alice".to_string());
    let _: PortablePredicate<User> = f.email().contains("example.com");
    let _: PortablePredicate<User> = f.age().gte(18i32);
    let _: PortablePredicate<User> = f.bio().is_null();
    let _: PortablePredicate<User> = f.name().eq("alice".to_string()) & f.age().gte(21i32);
}

fn _serial_pk_id_is_i32() {
    let f = LookupFields::default();
    // `pk = Serial` types `id` as `i32`, not `HeerId`.
    let _id: DjogiField<Lookup, i32> = f.id();
    let _label: DjogiField<Lookup, String> = f.label();
    let _: PortablePredicate<Lookup> = f.id().eq(7i32);
}

fn main() {
    // Runtime-assert the raw-ident column stripping. Compile-pass fixtures
    // are executed by lihaaf as plain binaries, so `fn main()` is a fine
    // place to land the assertion.
    _raw_ident_column_literal_strips_prefix();
}
