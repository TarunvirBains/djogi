//! Pin the soundness of `InsertSelectSource<S, V>` Numeric arithmetic
//! composition: composing two source operands tagged with DIFFERENT
//! source models must fail to compile — otherwise the resulting source
//! projection references columns from a model that is not the queryset's
//! `FROM` table, producing a Postgres "column does not exist" error at
//! execute time.
//!
//! `Add for InsertSelectSource<S, V>` requires both operands to share
//! the same `S`. Mixing `InsertSelectSource<CrossSourceA, i32>` with
//! `InsertSelectSource<CrossSourceB, i32>` does not satisfy the impl's
//! receiver type, so rustc rejects the `+` call.
use djogi::prelude::*;

#[model(table = "is_cross_src_a", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct CrossSourceA {
 pub score: i32,
}

#[model(table = "is_cross_src_b", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct CrossSourceB {
 pub score: i32,
}

#[model(table = "is_cross_src_target", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct CrossSourceTarget {
 pub score: i32,
}

fn main() {
 // Construct two source operands tagged with different source
 // models, then attempt to compose them with `+`. The `Add` impl on
 // `InsertSelectSource<S, V>` requires both sides to share the same
 // `S`, so rustc rejects this composition before any closure /
 // queryset infrastructure even comes into play.
 let a_fields: <CrossSourceA as Model>::Fields = Default::default();
 let b_fields: <CrossSourceB as Model>::Fields = Default::default();
 let _bad = a_fields.score().as_insert_source() + b_fields.score().as_insert_source();
}
