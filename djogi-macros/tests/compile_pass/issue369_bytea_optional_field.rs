// djogi#369 — `Option<Vec<u8>>` maps to a nullable Postgres `BYTEA` column.
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
// As with the non-optional form, `Option<Vec<u8>>` is classified
// `Unsupported` by the portable-predicate emitter (the `Option` strip exposes
// the `Unsupported` `Vec<u8>` inner, which the classifier preserves), so the
// closure filter does not expose `.eq` / `.is_null` on a BYTEA field. The
// model still compiles fully and the column is first-class.
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

fn main() {}
