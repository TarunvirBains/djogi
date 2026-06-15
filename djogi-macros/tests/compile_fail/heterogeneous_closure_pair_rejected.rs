// typed pair-tuple compile-fail: heterogeneous
// closure-pair join.
//
// `left_join_closure_pair::<C>` is now reachable only on
// `JoinedQuerySet<L, L>` — single-model self-joins. Attempting to call
// it on a heterogeneous pair (`(Animal, Widget)`) constructed via
// `cross_join_with` is a compile error: the method lives in the
// `impl<L: Model> JoinedQuerySet<L, L>` block, so trait resolution
// cannot find it on `JoinedQuerySet<Animal, Widget>` where L ≠ R.
//
// This pin protects against the pre-fix design where the method lived
// on the heterogeneous impl block and a runtime check on table-name
// equality was the only thing keeping a hostile call from emitting
// SQL with bogus alias bindings (`ra.animal_id = r.id` against a
// Widget right side).

use djogi::prelude::*;

#[model(table = "phase8_5_pair_closure_animals_heterogeneous")]
#[derive(Debug, Clone)]
pub struct Animal {
 pub name: String,
}

#[model(table = "phase8_5_pair_closure_widgets_heterogeneous")]
#[derive(Debug, Clone)]
pub struct Widget {
 pub label: String,
}

#[model(table = "phase8_5_pair_closure_animal_ancestries_heterogeneous", no_default)]
#[derive(Debug, Clone)]
pub struct AnimalAncestry {
 pub animal_id: ForeignKey<Animal>,
 pub ancestor_id: ForeignKey<Animal>,
 pub depth: i32,
 pub path_count: i64,
}

impl djogi::query::ClosureModel for AnimalAncestry {
 type Source = Animal;
 fn source_column() -> &'static str {
  "animal_id"
 }
 fn ancestor_column() -> &'static str {
  "ancestor_id"
 }
 fn depth_column() -> &'static str {
  "depth"
 }
 fn path_count_column() -> &'static str {
  "path_count"
 }
}

fn main() {
 // Heterogeneous pair — `cross_join_with` produces
 // `JoinedQuerySet<Animal, Widget>` where L ≠ R. The
 // `left_join_closure_pair::<AnimalAncestry>` call cannot resolve
 // — the method lives only on the L = R self-join impl block.
 let _bad: JoinedQuerySet<Animal, Widget> = Animal::objects()
 .cross_join_with(Widget::objects())
 .left_join_closure_pair::<AnimalAncestry>();
}
