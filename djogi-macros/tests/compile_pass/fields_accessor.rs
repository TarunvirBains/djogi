// Verifies that `#[model]` emits a real `{Model}Fields` bag with per-column
// `FieldRef` accessors whose `V` generic matches the column's declared Rust
// type. Covers:
//
//   - framework columns (`id`, `created_at`, `updated_at`) return the expected
//     `FieldRef<User, ...>` shapes and support `.eq` / `.is_null` lookups;
//   - user-declared columns round-trip their types verbatim (`String`, `i32`);
//   - string-only lookups (`.contains`) resolve on `FieldRef<User, String>`;
//   - the emission compiles for `pk = "serial"` (different `id` type).
//
// `pk = "none"` is deliberately NOT exercised here: `crud::expand` does not
// emit `impl Model` for those models (see the module docs), and `FieldRef<M,
// V>` requires `M: Model`. `stubs::expand` mirrors that gate and emits only
// the empty `{Model}Fields` shell for pk=none — there are no accessors to
// test. A future phase introducing a composite-PK trait will unlock them.
//
// A compile-fail counterpart for mismatched value types is deferred: the
// existing `FieldRef` unit tests in `djogi/src/query/field.rs` exercise the
// `IntoFilterValue` mismatch paths, and duplicating them here would just
// re-test the same trait bound.

use djogi::prelude::*;
use djogi::query::FieldRef;

#[model(table = "users")]
#[derive(Debug, Clone)]
struct User {
    pub name: String,
    pub email: String,
    pub age: i32,
}

#[model(table = "lookups", pk = "serial")]
#[derive(Debug, Clone)]
struct Lookup {
    pub label: String,
}

fn _framework_accessors_typecheck() {
    let f = UserFields;
    // Framework columns: `id` is HeerId (default pk), timestamps are DateTime.
    let _id: FieldRef<User, HeerId> = f.id();
    let _created: FieldRef<User, DateTime> = f.created_at();
    let _updated: FieldRef<User, DateTime> = f.updated_at();
}

fn _user_accessors_typecheck() {
    let f = UserFields;
    let _name: FieldRef<User, String> = f.name();
    let _email: FieldRef<User, String> = f.email();
    let _age: FieldRef<User, i32> = f.age();
}

fn _lookup_values_compile() {
    // The core acceptance check: a filter closure can build a Condition by
    // chaining a lookup off a `UserFields::default()` accessor. If the
    // accessor's `V` generic is wrong, `.eq` / `.contains` fails to resolve.
    let f = UserFields::default();
    let _: Condition = f.name().eq("alice".to_string());
    let _: Condition = f.email().contains("example.com");
    let _: Condition = f.age().gte(18i32);
    let _: Condition = f.id().is_null();
    let _: Condition = f
        .name()
        .eq("alice".to_string())
        .and_with(f.age().gte(21i32));
}

fn _serial_pk_id_is_i32() {
    let f = LookupFields::default();
    // `pk = "serial"` types `id` as `i32`, not `HeerId`.
    let _id: FieldRef<Lookup, i32> = f.id();
    let _label: FieldRef<Lookup, String> = f.label();
    let _: Condition = f.id().eq(7i32);
}

fn main() {}
