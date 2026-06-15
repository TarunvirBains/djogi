// predicate substrate compile-pass.
//
// Locks the public `djogi::query` predicate substrate against a
// macro-emitted model:
//
// 1. The substrate types (`DjogiField`, `DjogiPresentField`,
// `PortablePredicate`, `IntoPortablePredicate`, `SqlEmitContext`,
// `PortablePredicateError`) are reachable from the public
// `djogi::query` (or `__private::query`) re-export tree.
// 2. PR2d's macro-emitted `Model::__djogi_emit_field_predicate`
// override is callable from a downstream test crate with a typed
// `FieldPredicate<Widget>` payload. The compile success proves the
// override expands without unresolved paths or trait gaps and that
// every helper signature threaded through
// `::djogi::__private::query::portable_emit::*` monomorphises for
// the user's declared field types.
// 3. `DjogiField` construction via `__make_djogi_field` produces a
// working portable predicate surface (eq / neq / gt / in_ /
// is_null / some / contains / explicit_pg_predicate routing) over
// a mix of scalar, string, bool, and Option-shaped columns.
//
// The fixture constructs DjogiField directly via the hidden
// `__make_djogi_field` helper because PR2d does NOT flip
// `{Model}Fields` accessors to return `DjogiField` — that is PR3's
// scope. Until then the substrate is reachable through the macro-
// support entry point used by the future flip.
//
// Per the lihaaf compile-fixture contract, every lihaaf fixture has
// `fn main` so the binary still has to link.

use djogi::__private::pg::SqlAccumulator;
use djogi::__private::query::{SqlEmitContext, __make_djogi_field};
use djogi::prelude::*;
use djogi::query::{DjogiField, IntoPortablePredicate, IntoQ, PortablePredicate};
use djogi::types::{BasicPredicate, IntoBasicPredicate};

#[model(table = "phase8eta_substrate_widgets")]
#[derive(Debug, Clone)]
pub struct Widget {
 pub name: String,
 pub price: i64,
 pub estimated_year: Option<i32>,
 pub active: bool,
}

fn main() {
 let name_field: DjogiField<Widget, String> = __make_djogi_field("name", |w| &w.name);
 let price_field: DjogiField<Widget, i64> = __make_djogi_field("price", |w| &w.price);
 let year_field: DjogiField<Widget, Option<i32>> =
  __make_djogi_field("estimated_year", |w| &w.estimated_year);
 let active_field: DjogiField<Widget, bool> = __make_djogi_field("active", |w| &w.active);

 // Portable predicate construction over each portable kind.
 let _eq_pred: PortablePredicate<Widget> = name_field.eq("rust".to_string());
 let _gt_pred: PortablePredicate<Widget> = price_field.gt(100i64);
 let _in_pred: PortablePredicate<Widget> = active_field.in_([true, false]);
 let _null_pred: PortablePredicate<Widget> = year_field.is_null();
 let _not_null_pred: PortablePredicate<Widget> = year_field.is_not_null();
 let _some_pred: PortablePredicate<Widget> = year_field.some().eq(2020);
 let _pattern_pred: PortablePredicate<Widget> = name_field.contains("rust");
 let _starts_pred: PortablePredicate<Widget> = name_field.starts_with("ru");
 let _between_pred: PortablePredicate<Widget> = price_field.between(0i64, 100i64);

 // Mixed boolean composition. `&` binds tighter than `|`; the
 // operator matrix in `query::predicate` walks every pair without
 // forcing the caller to reach for `Q<T>` directly. The pure-
 // portable composition stays in `PortablePredicate<T>`.
 let composed: PortablePredicate<Widget> = price_field.eq(42i64)
  & name_field.eq("rust".to_string())
  | active_field.eq(true) & year_field.is_null();
 // Lift into `Q<T>` through the sealed `IntoQ<T>` impl on
 // `PortablePredicate<T>` (defined in PR2b's predicate substrate).
 // `IntoQ::into_q()` is the explicit lowering surface; `From`/
 // `Into` is not implemented because the seal lives on `IntoQ` to
 // keep raw Sassi predicates out of `Q<T>`.
 let _q: Q<Widget> = composed.into_q();

 // IntoPortablePredicate identity round-trip.
 let portable = name_field.eq("rust".to_string());
 let _again: PortablePredicate<Widget> = portable.into_portable_predicate();

 // Sassi-side conversion through IntoBasicPredicate.
 let portable = price_field.eq(42i64);
 let _basic: BasicPredicate<Widget> = portable.into_basic_predicate();

 // PR2d override invocation — emit the predicate's SQL fragment
 // through the macro-generated `Model::__djogi_emit_field_predicate`.
 // The hidden hook fires from PR2b's direct-`Q<T>` walker; compile-
 // pass coverage here proves the macro-emitted arms wire through
 // every helper signature without an unresolved monomorphisation.
 let portable = price_field.eq(42i64);
 let basic = portable.into_basic_predicate();
 if let BasicPredicate::Field(fp) = basic {
  let mut acc = SqlAccumulator::new("");
  let result = <Widget as Model>::__djogi_emit_field_predicate(
   &mut acc,
   &fp,
   SqlEmitContext::root(),
  );
  // `compile_pass` proves compilation only; consume the result
  // to prevent a `must_use` lint.
  let _ = result;
 }

 // explicit_pg_predicate routing — the SQL-only sibling lives at
 // `f.title().explicit_pg_predicate()` and yields legacy
 // `Condition` values for predicates that 8eta deliberately keeps
 // PostgreSQL-specific (regex / JSONB path / spatial / etc.).
 // The fixture exercises the route compiles; SQL-only methods
 // themselves are tested via the compile-fail sibling fixture.
 let _explicit = name_field.explicit_pg_predicate();
}
