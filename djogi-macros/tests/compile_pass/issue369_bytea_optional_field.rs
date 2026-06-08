// djogi#369, djogi#372 — `Option<Vec<u8>>` maps to a nullable Postgres `BYTEA` column.
//
// Companion to `issue369_bytea_field.rs`. The non-optional fixture proves a
// `Vec<u8>` field lowers to `BYTEA`; this one proves the `Option<Vec<u8>>`
// nullable form lowers to the same column type with `nullable: true`.
//
// The macro's `unwrap_schema_type` strips the `Option<…>` layer before
// `rust_type_to_sql` runs, so the inner `Vec<u8>` resolves to `BYTEA` exactly
// as the non-optional form does, and the descriptor records the column as
// nullable. `Option<Vec<u8>>: ToSql + FromSql` (postgres-types blanket impl
// over `Vec<u8>`), so the macro-generated bind/decode path round-trips a
// nullable binary column with no widening shim.
//
// SQL predicates: `eq`, `neq`, `in_`, `not_in` are available via explicit
// `DjogiField<M, Option<Vec<u8>>>` impl (djogi#372). Additionally:
// - `is_null()` / `is_not_null()` — generic on all `Option<U>` fields
// - `.some().eq(...)` — present-only comparison via `DjogiPresentField<M, Vec<u8>>`
// Portable/closure equality remains unavailable. The field is classified
// `Unsupported` by the portable-predicate emitter, so the closure filter does
// not expose `.eq` through the generic path.
use djogi::prelude::*;

#[model(table = "optional_blobs")]
#[derive(Debug, Clone)]
pub struct OptionalBlob {
    /// `Option<Vec<u8>>` — nullable raw binary payload, lowers to a nullable
    /// `BYTEA` column.
    pub payload: Option<Vec<u8>>,
    pub label: String,
}

fn _check_field_types(blob: &OptionalBlob) {
    // The `Option<Vec<u8>>` shape must be preserved verbatim — the inner
    // byte must not be widened, and the `Option` wrapper must survive.
    let _: &Option<Vec<u8>> = &blob.payload;
    let _: &String = &blob.label;
}

fn _check_model_surface() {
    let _qs = OptionalBlob::objects();
}

// djogi#372 — verify SQL filter predicates compile on the nullable BYTEA field.
fn _check_optional_bytea_filter_surface() {
    let _ = OptionalBlob::objects()
        .filter(|f| f.payload().eq(vec![1, 2]));
    let _ = OptionalBlob::objects()
        .filter(|f| f.payload().is_null());
    let _ = OptionalBlob::objects()
        .filter(|f| f.payload().is_not_null());
    let _ = OptionalBlob::objects()
        .filter(|f| f.payload().some().eq(vec![3, 4]));
}

fn main() {}
