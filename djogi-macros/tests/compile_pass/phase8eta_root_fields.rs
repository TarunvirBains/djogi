// Phase 8eta PR3 — generated root fields flip to `DjogiField`.
//
// Locks the post-PR3 macro emission contract:
//
// 1. Macro-generated `{Model}Fields` is a ZST whose accessors return
//    `DjogiField<Self, V>` (not `FieldRef<Self, V>` as before PR3).
//    Closure-based filters compose portable predicates directly:
//
//        Widget::objects().filter(|f| f.active().eq(true) & f.score().gte(10))
//
//    The portable surface returns `PortablePredicate<Widget>`, which
//    flows through both Punnu (for the cache scope) and Djogi's SQL
//    emitter (for the database) from the same closure shape.
// 2. PostgreSQL-specific predicates (regex / spatial / array / JSONB)
//    are reachable through `.explicit_pg_predicate()` from the post-flip
//    `DjogiField`. The fixture exercises the regex route without the
//    `spatial` feature so the default-feature build still locks the
//    routing surface.
// 3. `Punnu::scope(...).filter_basic(...)` accepts portable closures
//    that target the SAME generated `Widget::Fields` accessor surface.
//    PR3 wires `Cacheable::Fields = WidgetFields`, replacing the
//    pre-PR3 `type Fields = ()` placeholder. Adopters never have to
//    choose between Djogi's closure surface and Sassi's predicate
//    DSL — every callsite reaches the same `DjogiField` accessors.
//
// Per the lihaaf compile-fixture contract, every lihaaf fixture has
// `fn main` so the binary still has to link.

use djogi::cache::*;
use djogi::prelude::*;
// `IntoBasicPredicate::into_basic_predicate` is the trusted-portable →
// raw-Sassi conversion route Punnu's `filter_basic` consumes. Re-exported
// at `djogi::types::IntoBasicPredicate` so adopters never name `sassi`
// directly.
use djogi::types::IntoBasicPredicate;

#[model(table = "phase8eta_root_widgets")]
#[derive(Debug, Clone)]
pub struct Widget {
    pub name: String,
    pub score: i64,
    pub active: bool,
    pub estimated_year: Option<i32>,
}

fn main() {
    // (1) Portable closures compose via the operator matrix on
    // `PortablePredicate<Widget>`. The closure return type is
    // `PortablePredicate<Widget>`, lifted into `Q<Widget>` by
    // `IntoQ<Widget>`.
    let _query = Widget::objects().filter(|f| f.active().eq(true) & f.score().gte(10));

    // (2) PostgreSQL-locale `regex` is reached through
    // `explicit_pg_predicate()` — the only root-field route to PG-specific
    // predicates. Returns a `Condition`, lifted into `Q<Widget>` via the
    // legacy `IntoQ` impl on `Condition`.
    let _regex_query = Widget::objects().filter(|f| f.name().explicit_pg_predicate().regex("^a"));

    // (3) Punnu's `scope(...).filter_basic(...)` consumes the SAME
    // generated `Widget::Fields` accessor. After PR3 the closure
    // receives `WidgetFields::default()` (a ZST) and reaches the
    // portable predicate surface through `DjogiField`-bearing
    // accessors. Sassi consumes `BasicPredicate<Widget>`, so the
    // closure converts the trusted Djogi predicate into the Sassi
    // shape via `IntoBasicPredicate::into_basic_predicate()`.
    let punnu = Punnu::<Widget>::builder().build();
    let _scope = punnu
        .scope(Vec::<MemQ<Widget>>::new())
        .filter_basic(|f| {
            (f.active().eq(true) & f.name().contains("alpha"))
                .into_basic_predicate()
        });

    // (4) `Cacheable::fields()` round-trips through the same constructor
    // as the closure-API filter. `<Widget as Cacheable>::Fields ==
    // WidgetFields` after PR3.
    let _fields: <Widget as Cacheable>::Fields = <Widget as Cacheable>::fields();
}
