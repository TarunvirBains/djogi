// The `FieldRef` constructor is sealed against downstream fabrication.
//
// Prior to the de42874 follow-up, `FieldRef::new` was `pub`
// (with `#[doc(hidden)]`), which let any downstream crate build a ref
// whose `column` string carried SQL-injection payloads or malformed
// identifiers. Those strings then flowed straight into the
// `SqlAccumulator::push_sql` calls inside `djogi/src/query/sql.rs` —
// via `emit_leaf`, `DISTINCT ON`, `ORDER BY`, and `UPDATE... SET`.
//
// This test pins the seal at the type system: downstream code must not
// be able to call the ref's constructor. `FieldRef::new` is now
// `pub(crate)` in the djogi crate, so naming it from outside the crate
// fails to resolve. The proc-macro-emitted `{Model}Fields` accessors
// reach the constructor through a `#[doc(hidden)] pub` helper that
// validates the column name via `djogi::ident::assert_plain_ident`
// before instantiating the ref — see
// `djogi::query::field::__macro_support::__make_field_ref`.
use djogi::prelude::*;
use djogi::query::FieldRef;

#[model(table = "posts_seal_test")]
#[derive(Debug, Clone)]
pub struct Post {
 pub title: String,
}

fn main() {
 // This must not compile — `FieldRef::new` is `pub(crate)` in the
 // djogi crate, so the attempted downstream call resolves to a
 // private associated function. That is the compile-time half of
 // the seal; the runtime half is `__make_field_ref`'s identifier
 // validation (which would panic on the injection payload below
 // even if the caller reached it).
 let _: FieldRef<Post, String> = FieldRef::new("title) OR 1=1 --");
}
