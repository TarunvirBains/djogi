// `#[derive(JsonbSchema)]` accepts `Option<T>` fields
// (closes GH issue #28).
//
// Before this change, JsonbSchema rejected `Option<T>` because it tried to
// resolve `Option<T>: JsonbSchema` as a trait bound. The macro now peels
// `Option<...>` off at expansion time and treats the inner `T` as the
// effective field type. Postgres JSONB `->>` returns NULL for
// missing-key, JSON-null, or non-stringifiable values identically — so
// path traversal semantics are unchanged. Users who need to distinguish
// "key absent vs JSON null vs scalar" call `.is_null()` / `.is_not_null()`
// on the resulting `JsonbPathRef`.

use djogi::JsonbSchema;
use djogi::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
pub struct Address {
 pub city: String,
 pub postal_code: Option<String>, // optional scalar inside nested schema
}

#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
pub struct ProfileData {
 pub display_name: String,
 pub age: Option<i32>,  // optional scalar — closes #28
 pub bio: Option<String>,  // optional String
 pub address: Option<Address>, // optional nested schema — peels to Address
}

#[model(table = "phase7_zero2_jsonb_option_users")]
#[derive(Debug, Clone)]
pub struct User {
 pub profile: Jsonb<ProfileData>,
}

#[allow(dead_code)]
fn _option_scalar_paths_compile() {
 // `Option<i32>` field: typed accessor returns `JsonbPathRef<User, i32>`
 // — the user filters on the inner type directly. Distinguishing
 // present-vs-absent uses is_null / is_not_null.
 let _f1 = |f: UserFields| f.profile().explicit_pg_predicate().typed().age().eq(30);
 let _f2 = |f: UserFields| f.profile().explicit_pg_predicate().typed().age().is_null();
 let _f3 = |f: UserFields| f.profile().explicit_pg_predicate().typed().age().is_not_null();

 // `Option<String>` similarly.
 let _f4 = |f: UserFields| f.profile().explicit_pg_predicate().typed().bio().eq("hello".to_string());
 let _f5 = |f: UserFields| f.profile().explicit_pg_predicate().typed().bio().is_not_null();
}

#[allow(dead_code)]
fn _option_nested_path_compiles() {
 // `Option<Address>` peels — accessing.address() returns AddressPath<User>
 // and downstream traversals work as if Address were not Option-wrapped.
 let _f1 = |f: UserFields| f.profile().explicit_pg_predicate().typed().address().city().eq("Toronto".to_string());

 // Optional scalar inside the nested schema also works.
 let _f2 = |f: UserFields| {
  f.profile()
  .explicit_pg_predicate().typed()
  .address()
  .postal_code()
  .eq("M5V".to_string())
 };
 let _f3 = |f: UserFields| f.profile().explicit_pg_predicate().typed().address().postal_code().is_null();
}

fn main() {}
