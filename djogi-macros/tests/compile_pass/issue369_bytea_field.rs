// djogi#369 — first-class `Vec<u8>` / Postgres `BYTEA` model field support.
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
// Filter surface: `Vec<u8>` is intentionally classified `Unsupported` by the
// portable-predicate emitter (`portable_field_emit::classify_inner`), the
// same posture as float arrays, `Jsonb`, spatial, and the network family —
// raw-binary equality is not parity-checked between Rust/Punnu and Postgres,
// so the closure filter does not expose `.eq` on a BYTEA field. The model
// still compiles fully; the field is a first-class storage column. A
// regression that accidentally routed `Vec<u8>` through the `u8` scalar path
// (widening the inner byte to `SMALLINT`) would either change the column type
// or break this fixture's `&Vec<u8>` type assertion.
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

fn main() {}
