// djogi#369, djogi#372 — first-class `Vec<u8>` / Postgres `BYTEA` model field support.
//
// Verifies that `#[derive(Model)]` accepts a `Vec<u8>` field and that the
// full derivation chain (the `Model` trait impl, `{Model}Fields`,
// `{Model}Filter`, `FromPgRow`, and `inventory`-submitted descriptor) builds
// against it. The field lowers to a `BYTEA` column — NOT a `SMALLINT[]`
// array. A *scalar* `u8` field lowers to `SMALLINT`, but a `Vec<u8>` is raw
// binary, recognised ahead of the generic `Vec<T>` array arms in
// `rust_type_to_sql`.
//
// This compile-pass fixture is the type-level gate. Runtime BYTEA round-trip
// (write bytes, read them back unchanged) is exercised by the live
// integration suite, which needs a Postgres connection unavailable here.
//
// SQL predicates: `eq`, `neq`, `in_`, `not_in` are available via the explicit
// `DjogiField<M, Vec<u8>>` impl (djogi#372). Portable/closure equality remains
// unavailable — raw-binary comparison is not portable to in-memory evaluation.
// The field is classified `Unsupported` by the portable-predicate emitter
// (`portable_field_emit::classify_inner`), so the closure filter does not
// expose `.eq` on a BYTEA field through the generic path.
//
// `tokio-postgres` ships the native `ToSql`/`FromSql` codec for `Vec<u8>` ↔
// BYTEA, so the macro-generated bind path (`push_bind`) and decode path
// (`try_get`) take the `BindKind::Direct` route with no widening shim — the
// same route every other directly-mapped scalar uses.
use djogi::prelude::*;

#[model(table = "blobs")]
#[derive(Debug, Clone)]
pub struct Blob {
 /// `Vec<u8>` — raw binary payload, lowers to `BYTEA`.
 pub payload: Vec<u8>,
 /// A plain scalar field alongside the blob, to confirm the byte vector
 /// does not perturb sibling-field lowering.
 pub label: String,
}

fn _check_field_types(blob: &Blob) {
 // The macro must preserve the declared field type verbatim — no
 // widening of the inner `u8` to `i16`, no rewrite to an array element
 // type.
 let _: &Vec<u8> = &blob.payload;
 let _: &String = &blob.label;
}

// The model must expose its full CRUD/query surface — `objects()` returns a
// `QuerySet<Blob>`, which proves the `Model` trait impl, `{Model}Fields`, and
// the descriptor all generated successfully even with a BYTEA field present.
fn _check_model_surface() {
 let _qs = Blob::objects();
}

// djogi#372 — verify SQL filter predicates compile on the BYTEA field.
fn _check_bytea_filter_surface() {
 let _ = Blob::objects()
 .filter(|f| f.payload().eq(vec![1, 2, 3]));
 let _ = Blob::objects()
 .filter(|f| f.payload().neq(vec![0]));
 let _ = Blob::objects()
 .filter(|f| f.payload().in_(vec![vec![1], vec![2]]));
 let _ = Blob::objects()
 .filter(|f| f.payload().not_in(vec![vec![3]]));
}

fn main() {}
