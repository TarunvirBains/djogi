// Phase 8.5 Cluster 4A — typed pair-tuple compile-pass: closure-self-join.
//
// Locks the type-level wiring for the Wright-style kinship shape: a
// single-model self-join augmented with two `LEFT JOIN`s against a
// `ClosureModel` table, where the right-side join carries the
// shared-ancestor semi-join predicate (`ra.ancestor_id =
// la.ancestor_id`). The typed `PairClosureKinshipSum::<Closure>`
// aggregate emits the per-pair `SUM(la.path × ra.path × 0.5^(...))`
// summation.
//
// Pins issue #99's "closure-self-join case" coverage at the type level.
// The runtime end-to-end is the mating-pairs demo's retrofit (substrate
// for #84 lives behind #99 — this fixture contributes to that path).
//
// Uses a hand-rolled `ClosureModel` impl pattern rather than a macro
// because `#[model(closure_for = ...)]` is not in scope for v0.1.0 —
// adopters implement the trait by hand, the way the elephant-tracker
// example's `ElephantAncestry` does today.

use djogi::prelude::*;

#[model(table = "phase8_5_pair_closure_animals")]
#[derive(Debug, Clone)]
pub struct Animal {
    pub name: String,
    #[allow(dead_code)]
    pub mother_id: Option<ForeignKey<Animal>>,
    #[allow(dead_code)]
    pub father_id: Option<ForeignKey<Animal>>,
}

#[model(table = "phase8_5_pair_closure_ancestries", no_default)]
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
    // Plain self-join augmented with the closure-pair LEFT JOINs. The
    // typed surface composes orthogonally — `left_join_closure_pair`
    // does not affect the WHERE / ORDER BY shape on the underlying
    // pair-tuple builder.
    let _pair_with_closure: JoinedQuerySet<Animal, Animal> = Animal::objects()
        .self_pairs()
        .filter_left(|a| a.name().neq("Excluded".to_string()))
        .left_join_closure_pair::<AnimalAncestry>();

    // Annotated form: kinship sum as the sole aggregate. The
    // `PairClosureKinshipSum::<C>` slot routes through the existing
    // `AnnotationSlot` substrate, so it composes naturally with
    // `qualify` on the `JoinedAnnotatedQuerySet`.
    let _kinship_query = Animal::objects()
        .self_pairs()
        .left_join_closure_pair::<AnimalAncestry>()
        .annotate(|_l, _r| PairClosureKinshipSum::<AnimalAncestry>::new());

    // Pair-aware window function: RowNumber partitioned by the left
    // side's id, ordered by the right side's name — emits
    // `OVER (PARTITION BY l.id ORDER BY r.name DESC)` in the SQL.
    let _ranked = Animal::objects().self_pairs().annotate(|left, right| {
        RowNumber::new()
            .partition_by_pair(PairSide::Left, left.id())
            .order_by_pair_desc(PairSide::Right, right.name())
            .alias("rank")
    });
}
