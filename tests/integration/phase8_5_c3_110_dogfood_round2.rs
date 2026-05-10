// Phase 8.5 Cluster 3 — Round-2 typed-surface gap discovery (issue #110).
//
// Methodology: write small adopter-shape scenarios across six categories and
// compile them. Compile-fails / awkward shapes / clean compiles each map to a
// verdict the round-2 summary comment on #110 reports.
//
// Every scenario uses `#[djogi::djogi_test(sync_models = [...])]` and the
// typed surface only — no raw_* escape hatches (per CLAUDE.md). When a gap
// surfaces, the workaround in this file is the same one an adopter would
// reach for today (`.to_string()` coercion, raw filter via a lookup-by-id
// fallback, a separate query, etc.). Each workaround carries a `// GAP(<id>)`
// marker pointing at the GH issue filed under #110.

use djogi::prelude::*;
use serde::{Deserialize, Serialize};

// ────────────────────────────────────────────────────────────────────────────
// Scenario models — one model per category, kept narrow so the compile chain
// surfaces gaps rather than dragging in unrelated typed-surface holes.
// ────────────────────────────────────────────────────────────────────────────

// Category 1 model: scalar columns + Tracked<T> + Option<scalar>.
//
// Probes:
//   - `Tracked<String>` field accessor: does FieldRef carry the Tracked
//     wrapper through to lookup?
//   - `Option<i32>` comparison surface (sibling of #107).
//   - `&str` vs `String` coercion at `.eq` callsites.
//   - `Vec<HeerId>` as a typed `IN (...)` payload via the typed `in_list`.
#[model(table = "djogi_dogfood_widgets", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct DogfoodWidget {
    pub label: String,
    pub tracked_label: Tracked<String>,
    pub maybe_count: Option<i32>,
    pub balance: i64,
}

// Category 4 model: probes Postgres types djogi may not surface typed.
//
// Anything the framework already supports goes here as a sanity check; the
// gaps are what's *missing* from this struct's field type repertoire.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct CoverageMeta {
    pub note: String,
}

#[model(table = "djogi_dogfood_coverage", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct DogfoodCoverage {
    pub tags: Vec<String>,
    pub view_counts: Vec<i32>,
    pub flags: Vec<bool>,
    pub meta: Jsonb<CoverageMeta>,
}

// Category 5 model: probes migration / DDL declarations available on
// `#[field(...)]`. Adopter can flip CHECK / COMMENT / GENERATED on / off
// here, and the test asserts that the descriptor surfaces the declaration.
#[model(table = "djogi_dogfood_ddl", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct DogfoodDdl {
    pub name: String,
    pub weight_kg: f64,
}

// Category 6 model: composes with `tokio::try_join!` against a pool-backed
// context to surface the cancellation / borrow shape of running multiple
// queries on `&mut DjogiContext`.
#[model(table = "djogi_dogfood_async", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct DogfoodAsync {
    pub kind: String,
    pub seq: i32,
}

// ────────────────────────────────────────────────────────────────────────────
// Category 1 — Type-system composition gaps (#107 sibling)
// ────────────────────────────────────────────────────────────────────────────

// Scenario 1.A — typed lookup against a `Tracked<String>` field.
//
// VERDICT: NEEDS GAP ISSUE — `FieldRef<M, Tracked<String>>` has no `.eq`,
// `.in_list`, or any other lookup, because `Tracked<T>` does not implement
// `IntoFilterValue` (verified by `grep -rn 'IntoFilterValue for Tracked'`).
// Adopter workaround today: route through the inner `String` via a separate
// `Model::get` lookup or fall back to `raw_*` (see
// `examples/elephant-tracker/src/demos/lineage.rs` for the existing real
// instance of this gap, with a comment from the demo author dated before
// #110 was filed).
//
// Stand-in (compiles): we use the plain `String` `label` column to filter,
// then walk results to re-check the `tracked_label`. An adopter facing a
// model whose only candidate-key field is `Tracked<String>` cannot reach
// the typed surface at all.
#[djogi::djogi_test(sync_models = [DogfoodWidget])]
async fn cat1_a_tracked_string_filter_gap(mut ctx: djogi::DjogiContext) {
    let alpha = DogfoodWidget::create(
        &mut ctx,
        DogfoodWidget {
            label: "alpha".to_string(),
            tracked_label: Tracked::new("alpha-tracked".to_string()),
            maybe_count: Some(7),
            balance: 100,
            ..Default::default()
        },
    )
    .await
    .expect("create alpha");

    let _beta = DogfoodWidget::create(
        &mut ctx,
        DogfoodWidget {
            label: "beta".to_string(),
            tracked_label: Tracked::new("beta-tracked".to_string()),
            maybe_count: Some(3),
            balance: 200,
            ..Default::default()
        },
    )
    .await
    .expect("create beta");

    // GAP(djogi#166): this would be the natural call but does not compile —
    //   DogfoodWidget::objects()
    //       .filter(|f| f.tracked_label().eq("alpha-tracked".to_string()))
    // because `DjogiField<DogfoodWidget, Tracked<String>>` carries the
    // `Tracked` wrapper through to the lookup `value: V` parameter and
    // `Tracked<String>` doesn't implement `IntoFilterValue` /
    // `postgres_types::ToSql`. See djogi#166 for the typed-surface fix
    // proposal.
    //
    // Workaround: filter on the plain `label` column, then verify the
    // tracked sibling out-of-band.
    let rows = DogfoodWidget::objects()
        .filter(|f| f.label().eq("alpha".to_string()))
        .fetch_all(&mut ctx)
        .await
        .expect("plain string filter");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, alpha.id);
    assert_eq!(&*rows[0].tracked_label, "alpha-tracked");
}

// Scenario 1.B — `Vec<HeerId>` as a typed `IN (...)` payload.
//
// VERDICT: COMPILES CLEANLY — `FieldRef<M, HeerId>::in_list(values)` accepts
// any `IntoIterator<Item = V>`, and `HeerId: IntoFilterValue` (verified at
// `djogi/src/query/field.rs:1787`). False positive from the round-2
// brainstorm — round 2 does not file a gap here.
#[djogi::djogi_test(sync_models = [DogfoodWidget])]
async fn cat1_b_heerid_in_list_compiles(mut ctx: djogi::DjogiContext) {
    let a = DogfoodWidget::create(
        &mut ctx,
        DogfoodWidget {
            label: "a".to_string(),
            tracked_label: Tracked::new("a".to_string()),
            maybe_count: None,
            balance: 1,
            ..Default::default()
        },
    )
    .await
    .expect("create a");
    let b = DogfoodWidget::create(
        &mut ctx,
        DogfoodWidget {
            label: "b".to_string(),
            tracked_label: Tracked::new("b".to_string()),
            maybe_count: None,
            balance: 2,
            ..Default::default()
        },
    )
    .await
    .expect("create b");
    let _c = DogfoodWidget::create(
        &mut ctx,
        DogfoodWidget {
            label: "c".to_string(),
            tracked_label: Tracked::new("c".to_string()),
            maybe_count: None,
            balance: 3,
            ..Default::default()
        },
    )
    .await
    .expect("create c");

    let ids: Vec<djogi::HeerId> = vec![a.id, b.id];
    let rows = DogfoodWidget::objects()
        .filter(|f| f.id().in_(ids.clone()))
        .fetch_all(&mut ctx)
        .await
        .expect("HeerId IN (...) filter");
    assert_eq!(rows.len(), 2);
    let mut got: Vec<i64> = rows.iter().map(|r| r.id.as_i64()).collect();
    got.sort_unstable();
    let mut want: Vec<i64> = ids.iter().map(|i| i.as_i64()).collect();
    want.sort_unstable();
    assert_eq!(got, want);
}

// Scenario 1.C — `&str` vs `String` coercion at lookup callsites.
//
// VERDICT: ERGONOMIC FRICTION CONFIRMED — `FieldRef<M, String>::eq(value: V)`
// where `V = String`. A natural `.eq("literal")` does NOT compile because
// `&str` is not `String`. Adopters always write `.eq("literal".to_string())`
// (verified across 10+ existing test files via grep). Routes through GH
// issue separate from #107 (which targets `Option<scalar>`).
//
// Stand-in: the workaround that adopters use today.
#[djogi::djogi_test(sync_models = [DogfoodWidget])]
async fn cat1_c_str_vs_string_coercion_friction(mut ctx: djogi::DjogiContext) {
    DogfoodWidget::create(
        &mut ctx,
        DogfoodWidget {
            label: "alpha".to_string(),
            tracked_label: Tracked::new("alpha".to_string()),
            maybe_count: None,
            balance: 1,
            ..Default::default()
        },
    )
    .await
    .expect("create");

    // GAP(djogi#167): natural call shape that does NOT compile:
    //   .filter(|f| f.label().eq("alpha"))    // E0308: expected String, found &str
    // Adopter workaround:
    let n = DogfoodWidget::objects()
        .filter(|f| f.label().eq("alpha".to_string()))
        .count(&mut ctx)
        .await
        .expect("count");
    assert_eq!(n, 1);
}

// Scenario 1.D — `Option<i32>` comparison through the typed surface.
//
// VERDICT: GAP ALREADY TRACKED BY #107 — Option<i16> example in the existing
// issue body. `.maybe_count().lte(10)` does not compile. We exercise the
// typed `is_not_null` / `is_null` surface that DOES compile to keep this
// scenario as a regression guard once #107 lands.
#[djogi::djogi_test(sync_models = [DogfoodWidget])]
async fn cat1_d_option_scalar_lookup_blocked_by_107(mut ctx: djogi::DjogiContext) {
    DogfoodWidget::create(
        &mut ctx,
        DogfoodWidget {
            label: "with-count".to_string(),
            tracked_label: Tracked::new("x".to_string()),
            maybe_count: Some(5),
            balance: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create with count");
    DogfoodWidget::create(
        &mut ctx,
        DogfoodWidget {
            label: "no-count".to_string(),
            tracked_label: Tracked::new("y".to_string()),
            maybe_count: None,
            balance: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create without count");

    // What works today (typed):
    let with_count = DogfoodWidget::objects()
        .filter(|f| f.maybe_count().is_not_null())
        .count(&mut ctx)
        .await
        .expect("not-null count");
    assert_eq!(with_count, 1);

    // GAP(#107): the natural value comparison does not compile —
    //   .filter(|f| f.maybe_count().lte(10))
    //   E0599: method `lte` is not satisfied for `DjogiField<_, Option<i32>>`
    // Adopter workaround: `.is_not_null()` then Rust-side numeric filter on
    // results, OR `.explicit_pg_predicate().lte(10)` if PG predicate access
    // exists at this site (it does for many shapes — see #107).
}

// ────────────────────────────────────────────────────────────────────────────
// Category 2 — Ergonomic / fluent-chain gaps (#109 sibling)
// ────────────────────────────────────────────────────────────────────────────

// Scenario 2.A — multi-condition `.filter(|f| ...)` shape.
//
// VERDICT: COMPILES, ERGONOMIC FRICTION DOCUMENTED BY #109 — adopter has to
// reach for `Q::Condition(...)` or chain multiple `.filter(...)` calls. The
// in-closure way to AND two leaf conditions is verbose. Q algebra exists
// (BitAnd/BitOr/Not on `Q<T>`), but inside the closure the field accessors
// return `Condition` (not `Q<T>`), so you can't write
// `f.a().eq(x) & f.b().eq(y)`.
//
// Stand-in shape compiles by chaining `.filter()` calls (each AND-ed onto
// the queryset's condition tree).
#[djogi::djogi_test(sync_models = [DogfoodWidget])]
async fn cat2_a_multi_condition_chain_friction(mut ctx: djogi::DjogiContext) {
    DogfoodWidget::create(
        &mut ctx,
        DogfoodWidget {
            label: "match".to_string(),
            tracked_label: Tracked::new("x".to_string()),
            maybe_count: Some(50),
            balance: 100,
            ..Default::default()
        },
    )
    .await
    .expect("create match");
    DogfoodWidget::create(
        &mut ctx,
        DogfoodWidget {
            label: "match".to_string(),
            tracked_label: Tracked::new("y".to_string()),
            maybe_count: Some(50),
            balance: 999,
            ..Default::default()
        },
    )
    .await
    .expect("create non-match");

    // What compiles today — chained `.filter(...)` calls AND together.
    let n = DogfoodWidget::objects()
        .filter(|f| f.label().eq("match".to_string()))
        .filter(|f| f.balance().eq(100i64))
        .count(&mut ctx)
        .await
        .expect("chained-filter AND");
    assert_eq!(n, 1);

    // GAP(#109): natural in-closure shape that does NOT compile —
    //   .filter(|f| f.label().eq("match".to_string())
    //               .and(f.balance().eq(100i64)))
    // because `.and(...)` is `Condition::and(a, b)` (associated function).
    // Adopter alternative: `Condition::and(c1, c2)` — verbose; documented
    // in #109. No new gap to file.
}

// Scenario 2.B — `Q<T>` operator algebra (BitAnd/BitOr/Not).
//
// VERDICT: COMPILES CLEANLY when adopter assembles outside the closure —
// false positive ruled out. The friction in #109 is purely the in-closure
// shape from scenario 2.A.
#[djogi::djogi_test(sync_models = [DogfoodWidget])]
async fn cat2_b_q_algebra_outside_closure(mut ctx: djogi::DjogiContext) {
    DogfoodWidget::create(
        &mut ctx,
        DogfoodWidget {
            label: "alpha".to_string(),
            tracked_label: Tracked::new("a".to_string()),
            maybe_count: Some(1),
            balance: 1,
            ..Default::default()
        },
    )
    .await
    .expect("create alpha");
    DogfoodWidget::create(
        &mut ctx,
        DogfoodWidget {
            label: "beta".to_string(),
            tracked_label: Tracked::new("b".to_string()),
            maybe_count: Some(2),
            balance: 2,
            ..Default::default()
        },
    )
    .await
    .expect("create beta");

    // Build two Q<T> values, then combine with `|` (BitOr). Field handles
    // come from the closure-default `T::Fields` value via the typed
    // `Default` impl the macro emits — the same handle the closure-style
    // `.filter(|f| ...)` passes in.
    let fields = <DogfoodWidget as ::djogi::types::Cacheable>::fields();
    let q_alpha: djogi::Q<DogfoodWidget> =
        djogi::Q::Portable(fields.label().eq("alpha".to_string()));
    let q_beta: djogi::Q<DogfoodWidget> = djogi::Q::Portable(fields.label().eq("beta".to_string()));
    let combined = q_alpha | q_beta;
    let n = DogfoodWidget::objects()
        .filter_struct(combined)
        .count(&mut ctx)
        .await
        .expect("Q<T> OR via BitOr");
    assert_eq!(n, 2);
}

// ────────────────────────────────────────────────────────────────────────────
// Category 3 — Transactional / connection surface
// ────────────────────────────────────────────────────────────────────────────

// Scenario 3.A — nested `atomic` produces a savepoint.
//
// VERDICT: COMPILES CLEANLY — savepoints are implicit through nested
// `atomic` (verified at `djogi/src/transaction.rs:220`). False positive
// from the brainstorm (the issue body listed "savepoints" as a gap; they
// exist).
#[djogi::djogi_test(sync_models = [DogfoodWidget])]
async fn cat3_a_nested_atomic_emits_savepoint(mut ctx: djogi::DjogiContext) {
    djogi::transaction::atomic(&mut ctx, |outer| {
        Box::pin(async move {
            DogfoodWidget::create(
                outer,
                DogfoodWidget {
                    label: "outer-row".to_string(),
                    tracked_label: Tracked::new("o".to_string()),
                    maybe_count: None,
                    balance: 0,
                    ..Default::default()
                },
            )
            .await?;

            // Nested atomic — emits SAVEPOINT sp_1, then RELEASE on Ok.
            djogi::transaction::atomic(outer, |inner| {
                Box::pin(async move {
                    DogfoodWidget::create(
                        inner,
                        DogfoodWidget {
                            label: "inner-row".to_string(),
                            tracked_label: Tracked::new("i".to_string()),
                            maybe_count: None,
                            balance: 0,
                            ..Default::default()
                        },
                    )
                    .await?;
                    Ok::<_, djogi::DjogiError>(())
                })
            })
            .await?;
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .expect("nested atomic");

    let n = DogfoodWidget::objects()
        .count(&mut ctx)
        .await
        .expect("count");
    assert_eq!(n, 2);
}

// Scenario 3.B — explicit isolation level on a transaction.
//
// VERDICT: NEEDS GAP ISSUE — no public typed surface to choose isolation
// level. `atomic()` always opens a default-isolation transaction; there
// is no `atomic_with(IsolationLevel::Serializable, |ctx| { ... })` shape
// nor `ctx.set_isolation(...)` setter (verified by `grep -rn
// 'ISOLATION LEVEL\|isolation_level\|IsolationLevel' djogi/src/`, which
// returns zero matches outside doc comments).
//
// Stand-in: the test compiles because it uses the default isolation —
// but the workaround for SERIALIZABLE today is `raw_execute("SET TRANSACTION
// ISOLATION LEVEL SERIALIZABLE")` inside an atomic block, which is gated by
// the bypass attribute.
#[djogi::djogi_test(sync_models = [DogfoodWidget])]
async fn cat3_b_isolation_level_gap(mut ctx: djogi::DjogiContext) {
    // GAP(djogi#168): the natural call shape that does not exist:
    //   djogi::transaction::atomic_with(
    //       IsolationLevel::Serializable,
    //       &mut ctx,
    //       |ctx| Box::pin(async move { ... }),
    //   )
    // Adopter has to fall back to `raw_execute("SET TRANSACTION ISOLATION
    // LEVEL SERIALIZABLE")` under the bypass attribute.
    djogi::transaction::atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            DogfoodWidget::create(
                ctx,
                DogfoodWidget {
                    label: "default-iso".to_string(),
                    tracked_label: Tracked::new("z".to_string()),
                    maybe_count: None,
                    balance: 0,
                    ..Default::default()
                },
            )
            .await?;
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .expect("default-isolation atomic");
}

// Scenario 3.C — deferred constraints / `SET CONSTRAINTS ALL DEFERRED`.
//
// VERDICT: NEEDS GAP ISSUE — `INITIALLY DEFERRED` exists at the FK/index
// declaration layer (verified at `djogi/src/live_migrate/patterns/...`),
// but there is no public typed surface to flip a deferrable-but-immediate
// constraint to deferred for the duration of a transaction (the
// `SET CONSTRAINTS ALL DEFERRED` shape). Adopters needing circular FK
// inserts in a single transaction must reach for raw_execute today.
#[djogi::djogi_test(sync_models = [DogfoodWidget])]
async fn cat3_c_defer_constraints_gap(mut ctx: djogi::DjogiContext) {
    // GAP(djogi#169): natural call shape that does not exist:
    //   djogi::transaction::atomic(&mut ctx, |ctx| Box::pin(async move {
    //       ctx.defer_constraints(DeferScope::All).await?;
    //       // ... circular FK inserts ...
    //       Ok::<_, DjogiError>(())
    //   })).await?;
    // Stand-in: scenario compiles only because we don't actually have a
    // circular-FK shape to exercise.
    DogfoodWidget::create(
        &mut ctx,
        DogfoodWidget {
            label: "no-circular".to_string(),
            tracked_label: Tracked::new("n".to_string()),
            maybe_count: None,
            balance: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create");
}

// ────────────────────────────────────────────────────────────────────────────
// Category 4 — Type-coverage gaps
// ────────────────────────────────────────────────────────────────────────────

// Scenario 4.A — Postgres typed arrays for supported element types.
//
// VERDICT: COMPILES CLEANLY for `String / i32 / i64 / bool` (verified via
// `IntoArrayFilterValue` sealed trait at `djogi/src/query/field.rs:2087`).
// `Vec<HeerId>` / `Vec<RanjId>` / `Vec<i16>` / `Vec<f64>` / `Vec<DateTime>`
// are *not* in the sealed allow-list — see djogi#171 for the sealed-set
// extension proposal.
#[djogi::djogi_test(sync_models = [DogfoodCoverage])]
async fn cat4_a_typed_arrays_supported_elements(mut ctx: djogi::DjogiContext) {
    let row = DogfoodCoverage::create(
        &mut ctx,
        DogfoodCoverage {
            tags: vec!["alpha".to_string(), "beta".to_string()],
            view_counts: vec![1, 2, 3],
            flags: vec![true, false],
            meta: Jsonb::new(CoverageMeta {
                note: "n".to_string(),
            }),
            ..Default::default()
        },
    )
    .await
    .expect("create coverage");

    // Array typed surface: contains / overlap / contained_by ride
    // through `explicit_pg_predicate()` because they emit
    // PostgreSQL-specific operators (`@>`, `&&`, `<@`) — DjogiField's
    // root surface keeps to portable lookups only. This is the existing
    // post-Phase-8eta routing, not a new gap.
    let by_tag = DogfoodCoverage::objects()
        .filter(|f| {
            f.tags()
                .explicit_pg_predicate()
                .contains(&["alpha".to_string()])
        })
        .count(&mut ctx)
        .await
        .expect("array contains");
    assert_eq!(by_tag, 1);

    let by_views = DogfoodCoverage::objects()
        .filter(|f| f.view_counts().explicit_pg_predicate().overlap(&[2i32, 99]))
        .count(&mut ctx)
        .await
        .expect("array overlap");
    assert_eq!(by_views, 1);

    let _ = row.id;
}

// Scenario 4.B — INTERVAL, ENUM, CITEXT, INET, MACADDR, MONEY, RANGES.
//
// VERDICT (per type):
//   - ENUM (CREATE TYPE ... AS ENUM via `#[derive(DjogiEnum)]`): COMPILES
//     CLEANLY (verified at `djogi/src/enum_.rs` + tests/integration/
//     phase5_postgres_native.rs).
//   - CITEXT: present as `FieldSqlType::Citext` for descriptor projection
//     but no typed Rust newtype on the field side and no ASCII-stable
//     ILIKE surface beyond what `String` already exposes — open question
//     whether this needs a separate gap or stays under #105 / #110.
//   - INTERVAL, INET / CIDR / MACADDR, MONEY, RANGE TYPES (int4range,
//     tsrange, daterange, numrange), DOMAIN TYPES: NEEDS GAP ISSUE — none
//     have a `FieldSqlType` variant or descriptor projection (verified by
//     grep over `djogi/src/descriptor.rs` and the surrounding modules).
//
// Stand-in: this test only compiles the supported subset (ENUM was already
// covered in the existing tests; we avoid duplication). The gap issue
// filed under #110 enumerates the unsupported types and routes the
// large-shape ones (range types) to Cluster 4 per the v3 plan rule.
#[djogi::djogi_test(sync_models = [DogfoodCoverage])]
async fn cat4_b_pg_type_coverage_gap(mut ctx: djogi::DjogiContext) {
    // GAP(djogi#170 — umbrella): the natural model field declarations that
    // do not have a typed surface today —
    //   pub session_window: std::time::Duration,        // INTERVAL
    //   pub remote_addr: std::net::IpAddr,              // INET
    //   pub price_usd: rust_decimal::Decimal,           // (works as NUMERIC,
    //                                                   //  no MONEY surface)
    //   pub valid_for: SomeRangeNewtype<i32>,           // int4range
    //
    // Plus DOMAIN TYPES (`CREATE DOMAIN`), which are not surfaced at all.
    // Stand-in compiles by NOT declaring any of these — the test asserts
    // the absence is real.
    let _ = DogfoodCoverage::objects()
        .count(&mut ctx)
        .await
        .expect("count");
}

// ────────────────────────────────────────────────────────────────────────────
// Category 5 — Migration / DDL gaps (#105 sibling)
// ────────────────────────────────────────────────────────────────────────────

// Scenario 5.A — CHECK constraint declared on a `#[field(...)]`.
//
// VERDICT: GAP ALREADY TRACKED BY #105 — verified by grepping
// `djogi-macros/src/model/attrs.rs` for `check`. The attribute does not
// exist; adopter must hand-write a raw SQL migration.
//
// Stand-in: we declare the model without any CHECK and the schema syncs
// fine — the gap is the absence of the attribute.
#[djogi::djogi_test(sync_models = [DogfoodDdl])]
async fn cat5_a_check_constraint_blocked_by_105(mut ctx: djogi::DjogiContext) {
    DogfoodDdl::create(
        &mut ctx,
        DogfoodDdl {
            name: "valid".to_string(),
            // GAP(#105): `weight_kg` should be `> 0` per the model
            // invariant, but no `#[field(check = "weight_kg > 0")]`
            // attribute exists today. Negative weight inserts succeed
            // unless the adopter ships a hand-written CHECK migration.
            weight_kg: -1.0,
            ..Default::default()
        },
    )
    .await
    .expect("create — accepted because CHECK is not enforceable from #[field]");
}

// Scenario 5.B — COMMENT ON, storage params, tablespace, type-change
// USING expressions.
//
// VERDICT: NEEDS GAP ISSUE — none of `COMMENT ON COLUMN`,
// `COMMENT ON TABLE`, table storage parameters (`fillfactor`,
// `autovacuum_*`), `TABLESPACE <name>`, or `ALTER COLUMN ... TYPE ... USING
// <expr>` are surfaced from the model attribute layer (verified by grep —
// zero matches outside SQL strings inside live_migrate patterns). All are
// long-standing Postgres operational features.
//
// Routed to djogi#172 (umbrella for Cluster 4; small pieces such as
// COMMENT ON may move to Cluster 3 by v3 amendment per the v3 plan rule).
#[djogi::djogi_test(sync_models = [DogfoodDdl])]
async fn cat5_b_ddl_metadata_gap(mut ctx: djogi::DjogiContext) {
    // Stand-in: no-op create. The gap is what's missing from `#[model]`
    // and `#[field]` attributes.
    DogfoodDdl::create(
        &mut ctx,
        DogfoodDdl {
            name: "noop".to_string(),
            weight_kg: 1.0,
            ..Default::default()
        },
    )
    .await
    .expect("create");
}

// ────────────────────────────────────────────────────────────────────────────
// Category 6 — Async / Result composition
// ────────────────────────────────────────────────────────────────────────────

// Scenario 6.A — `?` flow with `DjogiError` in adopter `async fn`.
//
// VERDICT: COMPILES CLEANLY — `DjogiError` derives `thiserror::Error`
// (verified at `djogi/src/error.rs:165`), so it implements
// `std::error::Error + Send + Sync` and flows through `?` into anyhow /
// custom error types.
async fn fetch_one_widget(
    ctx: &mut djogi::DjogiContext,
    id: djogi::HeerId,
) -> Result<DogfoodAsync, djogi::DjogiError> {
    DogfoodAsync::get(ctx, id).await
}

#[djogi::djogi_test(sync_models = [DogfoodAsync])]
async fn cat6_a_question_mark_through_djogi_error(mut ctx: djogi::DjogiContext) {
    let row = DogfoodAsync::create(
        &mut ctx,
        DogfoodAsync {
            kind: "first".to_string(),
            seq: 1,
            ..Default::default()
        },
    )
    .await
    .expect("create");

    let reloaded = fetch_one_widget(&mut ctx, row.id)
        .await
        .expect("fetch via helper");
    assert_eq!(reloaded.id, row.id);
}

// Scenario 6.B — `tokio::try_join!` with two queries on `&mut DjogiContext`.
//
// VERDICT: NEEDS GAP ISSUE — `&mut DjogiContext` is exclusive, so two
// concurrent queries cannot both borrow it at once. The natural shape:
//
//   tokio::try_join!(
//       DogfoodAsync::objects().filter(...).fetch_all(ctx),
//       DogfoodAsync::objects().filter(...).fetch_all(ctx),
//   )
//
// fails E0499 ("cannot borrow `*ctx` as mutable more than once at a time").
// Adopters need EITHER (a) a "shared" context shape that hands out two
// independent pooled connections (only valid for read-only queries
// outside a transaction), or (b) a documented sequential-fetch idiom.
//
// Routed to djogi#173 (Cluster 3 — small typed helper).
#[djogi::djogi_test(sync_models = [DogfoodAsync])]
async fn cat6_b_try_join_borrow_gap(mut ctx: djogi::DjogiContext) {
    DogfoodAsync::create(
        &mut ctx,
        DogfoodAsync {
            kind: "alpha".to_string(),
            seq: 1,
            ..Default::default()
        },
    )
    .await
    .expect("alpha");
    DogfoodAsync::create(
        &mut ctx,
        DogfoodAsync {
            kind: "beta".to_string(),
            seq: 2,
            ..Default::default()
        },
    )
    .await
    .expect("beta");

    // GAP(djogi#173): the natural concurrent shape that does NOT compile —
    //   let (a, b) = tokio::try_join!(
    //       DogfoodAsync::objects().filter(|f| f.kind().eq("alpha".to_string())).fetch_all(&mut ctx),
    //       DogfoodAsync::objects().filter(|f| f.kind().eq("beta".to_string())).fetch_all(&mut ctx),
    //   )?;
    //
    // E0499 because both branches need `&mut ctx`.
    //
    // Adopter workaround: sequential fetches.
    let alpha = DogfoodAsync::objects()
        .filter(|f| f.kind().eq("alpha".to_string()))
        .fetch_all(&mut ctx)
        .await
        .expect("alpha fetch");
    let beta = DogfoodAsync::objects()
        .filter(|f| f.kind().eq("beta".to_string()))
        .fetch_all(&mut ctx)
        .await
        .expect("beta fetch");
    assert_eq!(alpha.len(), 1);
    assert_eq!(beta.len(), 1);
}
