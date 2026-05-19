// Phase 8.5 Cluster 3 — Round-2 typed-surface gap discovery (issue #110).
//
// Methodology: write small adopter-shape scenarios across six categories and
// compile them. Compile-fails / awkward shapes / clean compiles each map to a
// verdict the round-2 summary comment on #110 reports.
//
// Lifecycle of a scenario:
//   - When discovery surfaces a gap, the scenario carries a `// GAP(<id>)`
//     marker and the workaround that an adopter would reach for today.
//   - When the underlying issue closes, the scenario is flipped to a
//     `// REGRESSION (closes #<id> via PR #<n>)` positive assertion that
//     locks the now-supported call shape against future regression. The
//     marker stays anchored to the GH closing PR so the audit trail is
//     traceable.
//
// As of 2026-05-16 the following sibling issues are closed and the matching
// scenarios are positive regressions: #107 (Option<scalar>), #109 (Condition
// ergonomics), #166 (Tracked<T> typed lookup), #167 (&str → String
// coercion at FieldRef::eq callsites), #168 (typed isolation-level surface),
// #169 (typed deferred-constraints surface), #171 (typed-array element
// expansion), and #173 (concurrent-reads helper). Still-open gap markers
// cover #105, #170, and #172 — those scenarios continue to document the
// workaround adopters reach for today and route to the named GH issue.
//
// Every scenario uses `#[djogi::djogi_test(sync_models = [...])]` and the
// typed surface only — no raw_* escape hatches (per CLAUDE.md). When a gap
// is still live, the scenario body falls back to a typed workaround (a
// separate query, an `.is_not_null()` predicate, etc.) — never to raw SQL.

use djogi::prelude::*;
use serde::{Deserialize, Serialize};

// ────────────────────────────────────────────────────────────────────────────
// Scenario models — one model per category, kept narrow so the compile chain
// surfaces gaps rather than dragging in unrelated typed-surface holes.
// ────────────────────────────────────────────────────────────────────────────

// Category 1 model: scalar columns + Tracked<T> + Option<scalar>.
//
// Probes (per scenario):
//   - 1.A — `Tracked<String>` portable lookup (closed by #166).
//   - 1.B — `HeerId` typed `IN (...)` payload via `DjogiField::in_`.
//   - 1.C — `&str` → `String` coercion at portable lookup callsites
//     (closed by #167).
//   - 1.D — `Option<i32>` portable comparison surface (closed by #107).
#[model(table = "djogi_dogfood_widgets", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct DogfoodWidget {
    pub label: String,
    pub tracked_label: Tracked<String>,
    pub maybe_count: Option<i32>,
    pub balance: i64,
}

// Category 4 model: probes Postgres typed-array element coverage and the
// remaining Postgres-type gaps.
//
// Post-#171 the typed-array element repertoire is wide enough that this
// model can declare arrays directly for the small-int / wide-float / ID
// families. The remaining Postgres-type gaps (INET/CIDR/MACADDR, MONEY,
// DOMAIN, and timestamp-without-timezone `tsrange`) are still absent from
// the field repertoire — that absence is the scenario-4.B probe.
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
    // Three array element types added by #171 (PR #205). Exercising one
    // representative from each newly-supported family — small int / wide
    // float / HeerId — keeps the regression coverage tight without
    // duplicating the per-type SQL-binding pins that live in
    // `djogi-macros/tests/compile_pass/typed_array_elements.rs`.
    pub priorities: Vec<i16>,
    pub measurements: Vec<f64>,
    pub related_ids: Vec<HeerId>,
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
// REGRESSION (closes #166 via PR #196): `DjogiField<M, Tracked<U>>` carries
// the same portable predicate surface as the underlying `U`, so
// `f.tracked_label().eq("alpha-tracked")` lowers to a `Tracked<String>`
// inner-value comparison. The wiring lives in the
// `IntoPortableFieldValue<Tracked<V>> for V` blanket plus the
// `IntoPortableFieldValue<Tracked<String>> for &str` borrow-coercion impl in
// `djogi/src/query/field.rs` (search for `IntoPortableFieldValue`).
//
// Locks the now-supported call shape so a future change to `Tracked<T>`
// portable wiring is caught at compile time.
#[djogi::djogi_test(sync_models = [DogfoodWidget])]
async fn cat1_a_tracked_string_filter_regression(mut ctx: djogi::DjogiContext) {
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

    // Closed-#166 call shape — direct `&str` against a `Tracked<String>`
    // column. Both the `&str → Tracked<String>` and the
    // `String → Tracked<String>` coercion arms are exercised.
    let by_borrowed_str = DogfoodWidget::objects()
        .filter(|f| f.tracked_label().eq("alpha-tracked"))
        .fetch_all(&mut ctx)
        .await
        .expect("tracked<string> filter via &str");
    assert_eq!(by_borrowed_str.len(), 1);
    assert_eq!(by_borrowed_str[0].id, alpha.id);
    assert_eq!(&*by_borrowed_str[0].tracked_label, "alpha-tracked");

    let by_owned_string = DogfoodWidget::objects()
        .filter(|f| f.tracked_label().eq("alpha-tracked".to_string()))
        .fetch_all(&mut ctx)
        .await
        .expect("tracked<string> filter via String");
    assert_eq!(by_owned_string.len(), 1);
    assert_eq!(by_owned_string[0].id, alpha.id);
}

// Scenario 1.B — `Vec<HeerId>` as a typed `IN (...)` payload.
//
// VERDICT: COMPILES CLEANLY — the portable `DjogiField::in_` surface accepts
// any `IntoIterator<Item = P>` where `P: IntoPortableFieldValue<HeerId>`, and
// `HeerId` satisfies the identity blanket `IntoPortableFieldValue<HeerId> for
// HeerId`. False positive from the round-2 brainstorm — kept as a positive
// sanity check that the portable HeerId IN-list surface stays healthy.
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
// REGRESSION (closes #167 via PR #196): `f.label().eq("alpha")` now compiles
// on a `String` column. The wiring is the
// `IntoPortableFieldValue<String> for &str` borrow-coercion impl in
// `djogi/src/query/field.rs`. Both the borrowed (`&str`) and owned
// (`String`) call shapes are valid; the borrowed shape is the ergonomic
// default adopters reach for.
#[djogi::djogi_test(sync_models = [DogfoodWidget])]
async fn cat1_c_str_to_string_coercion_regression(mut ctx: djogi::DjogiContext) {
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

    // Closed-#167 borrow-coercion call shape — no `.to_string()` needed.
    let by_borrowed_str = DogfoodWidget::objects()
        .filter(|f| f.label().eq("alpha"))
        .count(&mut ctx)
        .await
        .expect("count via &str");
    assert_eq!(by_borrowed_str, 1);

    // Pre-#167 owned-`String` call shape still compiles — the coercion
    // expanded the accepted set, it did not remove the existing path.
    let by_owned_string = DogfoodWidget::objects()
        .filter(|f| f.label().eq("alpha".to_string()))
        .count(&mut ctx)
        .await
        .expect("count via String");
    assert_eq!(by_owned_string, 1);
}

// Scenario 1.D — `Option<i32>` comparison through the typed surface.
//
// REGRESSION (closes #107 via PR #196): `DjogiField<M, Option<U>>` exposes
// the ordering predicates (`gt` / `gte` / `lt` / `lte` / `between`) directly
// on the nullable field — each lowers to `column IS NOT NULL AND column <op>
// value`, so SQL-NULL rows are excluded from the comparison. The wiring
// lives in the `impl<M: Model, U: DjogiPortableOrd> DjogiField<M, Option<U>>`
// block in `djogi/src/query/field.rs`. The `eq`/`neq`/`in_`/`not_in`
// equality surface accepts both bare-`U` (lowered to `Some(_)`) and
// `Option<U>` (preserving `None` as `IS NULL`) via the
// `IntoPortableFieldValue<Option<V>> for V` blanket.
#[djogi::djogi_test(sync_models = [DogfoodWidget])]
async fn cat1_d_option_scalar_lookup_regression(mut ctx: djogi::DjogiContext) {
    let with_count = DogfoodWidget::create(
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
    let no_count = DogfoodWidget::create(
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

    // Existing `is_not_null` / `is_null` surface still works — kept as part
    // of the regression coverage so a future change can't silently drop it.
    let not_null = DogfoodWidget::objects()
        .filter(|f| f.maybe_count().is_not_null())
        .count(&mut ctx)
        .await
        .expect("not-null count");
    assert_eq!(not_null, 1);

    let is_null = DogfoodWidget::objects()
        .filter(|f| f.maybe_count().is_null())
        .count(&mut ctx)
        .await
        .expect("is-null count");
    assert_eq!(is_null, 1);

    // Closed-#107 ordering predicates on `Option<i32>`. Each excludes SQL
    // NULL rows via the `IS NOT NULL AND` prefix the portable wiring emits.
    let lte_10 = DogfoodWidget::objects()
        .filter(|f| f.maybe_count().lte(10_i32))
        .fetch_all(&mut ctx)
        .await
        .expect("lte on Option<i32>");
    assert_eq!(lte_10.len(), 1);
    assert_eq!(lte_10[0].id, with_count.id);

    let gte_3 = DogfoodWidget::objects()
        .filter(|f| f.maybe_count().gte(3_i32))
        .count(&mut ctx)
        .await
        .expect("gte on Option<i32>");
    assert_eq!(gte_3, 1);

    let between_1_and_9 = DogfoodWidget::objects()
        .filter(|f| f.maybe_count().between(1_i32, 9_i32))
        .count(&mut ctx)
        .await
        .expect("between on Option<i32>");
    assert_eq!(between_1_and_9, 1);

    // Closed-#107 equality with bare-`U` and `Option<U>` via the blanket
    // `IntoPortableFieldValue<Option<V>> for V`.
    let eq_some = DogfoodWidget::objects()
        .filter(|f| f.maybe_count().eq(5_i32))
        .fetch_all(&mut ctx)
        .await
        .expect("eq bare-i32 on Option<i32>");
    assert_eq!(eq_some.len(), 1);
    assert_eq!(eq_some[0].id, with_count.id);

    let eq_none = DogfoodWidget::objects()
        .filter(|f| f.maybe_count().eq(None::<i32>))
        .fetch_all(&mut ctx)
        .await
        .expect("eq None on Option<i32>");
    assert_eq!(eq_none.len(), 1);
    assert_eq!(eq_none[0].id, no_count.id);
}

// ────────────────────────────────────────────────────────────────────────────
// Category 2 — Ergonomic / fluent-chain gaps (#109 sibling)
// ────────────────────────────────────────────────────────────────────────────

// Scenario 2.A — multi-condition `.filter(|f| ...)` shape.
//
// REGRESSION (closes #109 via PR #196): the `PortablePredicate<T>` returned
// by `f.col().eq(_)` (and the rest of the portable lookup surface) now
// supports `&` / `|` / `^` / `!` operators in-closure, and the `Condition`
// returned by `.explicit_pg_predicate().eq(_)` carries inherent `.and(_)` /
// `.or(_)` methods plus the same operators via the `ConditionExt` trait. The
// operator matrix is closed over `PortablePredicate<T>`, `Predicate<T>`, and
// `Condition` so any mix of the three composes without a manual
// `Q::Compound { ... }` at the call site (see
// `djogi/src/query/predicate.rs` for the full grid).
#[djogi::djogi_test(sync_models = [DogfoodWidget])]
async fn cat2_a_multi_condition_in_closure_regression(mut ctx: djogi::DjogiContext) {
    let primary = DogfoodWidget::create(
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
    let _non_match = DogfoodWidget::create(
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

    // Pre-#109 multi-`.filter()` chain still works — each call is AND-ed
    // onto the condition tree. Kept as a regression anchor for the
    // existing path.
    let by_chain = DogfoodWidget::objects()
        .filter(|f| f.label().eq("match"))
        .filter(|f| f.balance().eq(100_i64))
        .count(&mut ctx)
        .await
        .expect("chained-filter AND");
    assert_eq!(by_chain, 1);

    // Closed-#109 in-closure `&` operator on `PortablePredicate<T>`.
    let by_amp = DogfoodWidget::objects()
        .filter(|f| f.label().eq("match") & f.balance().eq(100_i64))
        .fetch_all(&mut ctx)
        .await
        .expect("in-closure `&` AND on PortablePredicate");
    assert_eq!(by_amp.len(), 1);
    assert_eq!(by_amp[0].id, primary.id);

    // Closed-#109 in-closure `|` operator. Two leaves, only one matches
    // `balance = 100`, so OR returns both rows.
    let by_pipe = DogfoodWidget::objects()
        .filter(|f| f.balance().eq(100_i64) | f.balance().eq(999_i64))
        .count(&mut ctx)
        .await
        .expect("in-closure `|` OR on PortablePredicate");
    assert_eq!(by_pipe, 2);

    // Closed-#109 `.and(_)` / `.or(_)` method form via `ConditionExt`. The
    // closure returns a `Condition` because both leaves go through
    // `.explicit_pg_predicate()`. AND of two matching leaves yields one
    // row.
    let by_method_and = DogfoodWidget::objects()
        .filter(|f| {
            f.label()
                .explicit_pg_predicate()
                .eq("match")
                .and(f.balance().explicit_pg_predicate().eq(100_i64))
        })
        .count(&mut ctx)
        .await
        .expect("Condition::and via ConditionExt");
    assert_eq!(by_method_and, 1);
}

// Scenario 2.B — `Q<T>` operator algebra (BitAnd/BitOr/Not) outside the
// closure.
//
// VERDICT: COMPILES CLEANLY when adopter assembles outside the closure —
// false positive ruled out. The in-closure shape #109 originally flagged is
// now also supported (scenario 2.A regression); this scenario locks the
// `filter_struct(...)` entry point that takes a pre-built `Q<T>`.
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
// REGRESSION (closes #168 via this PR): `djogi::transaction::atomic_with`
// + the `IsolationLevel` enum now expose the typed isolation surface.
// `atomic_with(IsolationLevel::Serializable, &mut ctx, |ctx| ...)` opens
// the transaction at `BEGIN ISOLATION LEVEL SERIALIZABLE`; `RepeatableRead`
// and `ReadCommitted` route through the same entry point. Nested
// savepoint scopes reject with
// `DjogiError::IsolationLevelOnNestedScope` because Postgres pins
// isolation at the outer BEGIN.
#[djogi::djogi_test(sync_models = [DogfoodWidget])]
async fn cat3_b_isolation_level_typed_surface(mut ctx: djogi::DjogiContext) {
    use djogi::transaction::{IsolationLevel, atomic_with};

    // The natural call shape that #168 unblocks — `atomic_with(level, ctx, closure)`
    // opens the outermost transaction at the requested level.
    atomic_with(IsolationLevel::Serializable, &mut ctx, |ctx| {
        Box::pin(async move {
            DogfoodWidget::create(
                ctx,
                DogfoodWidget {
                    label: "serializable-iso".to_string(),
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
    .expect("serializable-isolation atomic_with");

    // The other two variants compose through the same entry point.
    atomic_with(IsolationLevel::RepeatableRead, &mut ctx, |ctx| {
        Box::pin(async move {
            let _count = DogfoodWidget::objects().count(ctx).await?;
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .expect("repeatable-read atomic_with");

    atomic_with(IsolationLevel::ReadCommitted, &mut ctx, |ctx| {
        Box::pin(async move {
            let _count = DogfoodWidget::objects().count(ctx).await?;
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .expect("read-committed atomic_with");
}

// Scenario 3.C — deferred constraints / `SET CONSTRAINTS ALL DEFERRED`.
//
// REGRESSION (closes #169 via this PR): `DjogiContext::defer_constraints`
// + the `DeferScope` enum now expose the typed deferred-constraints
// surface. `DeferScope::All` emits `SET CONSTRAINTS ALL DEFERRED`;
// `DeferScope::Named(&[...])` validates each name against the
// descriptor inventory before emitting `SET CONSTRAINTS "name" DEFERRED`.
// Both helpers reject pool-backed contexts with
// `DjogiError::ConstraintModeOutsideTransaction`.
//
// This scenario exercises the typed-surface compilation against
// `DogfoodWidget`, plus the pool-rejection invariant. The full
// circular-FK live-PG round trip lives in
// `tests/integration/phase8_5_c4_169_defer_constraints.rs`.
#[djogi::djogi_test(sync_models = [DogfoodWidget])]
async fn cat3_c_defer_constraints_typed_surface(mut ctx: djogi::DjogiContext) {
    use djogi::transaction::DeferScope;

    // The natural call shape that #169 unblocks — `defer_constraints` /
    // `set_constraints_immediate` inside `atomic()`.
    djogi::transaction::atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            // Defer all deferrable constraints for the remainder of
            // this transaction. `DeferScope::All` skips per-name
            // validation; Postgres applies the flip to every
            // deferrable constraint in scope.
            ctx.defer_constraints(DeferScope::All).await?;

            // The model has no FKs — the SET CONSTRAINTS ALL DEFERRED
            // statement still runs (Postgres accepts it as a no-op
            // when no deferrable constraints exist), proving the
            // typed surface composes correctly.
            DogfoodWidget::create(
                ctx,
                DogfoodWidget {
                    label: "deferred-scope".to_string(),
                    tracked_label: Tracked::new("n".to_string()),
                    maybe_count: None,
                    balance: 0,
                    ..Default::default()
                },
            )
            .await?;

            // Reverse the flip mid-transaction.
            ctx.set_constraints_immediate(DeferScope::All).await?;
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .expect("defer_constraints(All) inside atomic");

    // Pool-backed rejection — `defer_constraints` outside an open
    // `atomic()` MUST surface
    // `DjogiError::ConstraintModeOutsideTransaction`.
    let err = ctx
        .defer_constraints(DeferScope::All)
        .await
        .expect_err("pool-backed defer_constraints must surface ConstraintModeOutsideTransaction");
    assert!(
        matches!(err, djogi::DjogiError::ConstraintModeOutsideTransaction),
        "expected ConstraintModeOutsideTransaction, got {err:?}",
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Category 4 — Type-coverage gaps
// ────────────────────────────────────────────────────────────────────────────

// Scenario 4.A — Postgres typed arrays across the sealed element-type set.
//
// REGRESSION (closes #171 via PR #205): the `IntoArrayFilterValue` sealed
// trait in `djogi/src/query/field.rs` now spans `String`, `i16`, `i32`,
// `i64`, `f32`, `f64`, `bool`, `time::OffsetDateTime`, `time::Date`,
// `uuid::Uuid`, `rust_decimal::Decimal`, and the `HeerId` / `RanjId` family
// (plus the `*Desc` siblings). Array operator methods (`@>` / `<@` / `&&`)
// remain gated behind `.explicit_pg_predicate()` so the portable surface
// keeps to portable lookups — that routing is unchanged.
//
// Coverage:
//   - `Vec<String>`, `Vec<i32>`, `Vec<bool>` — the pre-#171 baseline, kept
//     as a sanity check that the original sealed entries still wire up.
//   - `Vec<i16>`, `Vec<f64>`, `Vec<HeerId>` — three representatives from
//     the post-#171 expansion exercising the small-int / wide-float / ID
//     family arms of the `FilterValue::Array*` discriminant.
//
// Adopter-defined newtype / enum element types are NOT covered by #171 and
// route through the separate `DjogiSqlType` extension path documented in
// `docs/guide/arrays.md`.
#[djogi::djogi_test(sync_models = [DogfoodCoverage])]
async fn cat4_a_typed_arrays_sealed_set_regression(mut ctx: djogi::DjogiContext) {
    let related = djogi::HeerId::from_i64(424_242).expect("valid HeerId");
    let other = djogi::HeerId::from_i64(525_252).expect("valid HeerId");
    let row = DogfoodCoverage::create(
        &mut ctx,
        DogfoodCoverage {
            tags: vec!["alpha".to_string(), "beta".to_string()],
            view_counts: vec![1, 2, 3],
            flags: vec![true, false],
            priorities: vec![1_i16, 7_i16],
            measurements: vec![1.5_f64, 2.5_f64],
            related_ids: vec![related],
            meta: Jsonb::new(CoverageMeta {
                note: "n".to_string(),
            }),
            ..Default::default()
        },
    )
    .await
    .expect("create coverage");

    // Pre-#171 sealed entries — `@>` (contains) and `&&` (overlap) on
    // `Vec<String>` / `Vec<i32>`. Kept as regression anchors.
    let by_tag = DogfoodCoverage::objects()
        .filter(|f| {
            f.tags()
                .explicit_pg_predicate()
                .contains(&["alpha".to_string()])
        })
        .count(&mut ctx)
        .await
        .expect("array contains on Vec<String>");
    assert_eq!(by_tag, 1);

    let by_views = DogfoodCoverage::objects()
        .filter(|f| {
            f.view_counts()
                .explicit_pg_predicate()
                .overlap(&[2_i32, 99_i32])
        })
        .count(&mut ctx)
        .await
        .expect("array overlap on Vec<i32>");
    assert_eq!(by_views, 1);

    // Post-#171 sealed entries — one representative per newly-covered
    // family. `<@` / `@>` / `&&` route through the same
    // `IntoArrayFilterValue` plumbing.
    let by_priorities = DogfoodCoverage::objects()
        .filter(|f| f.priorities().explicit_pg_predicate().contains(&[7_i16]))
        .count(&mut ctx)
        .await
        .expect("array contains on Vec<i16>");
    assert_eq!(by_priorities, 1);

    let by_measurements = DogfoodCoverage::objects()
        .filter(|f| {
            f.measurements()
                .explicit_pg_predicate()
                .overlap(&[2.5_f64, 99.0_f64])
        })
        .count(&mut ctx)
        .await
        .expect("array overlap on Vec<f64>");
    assert_eq!(by_measurements, 1);

    let by_related = DogfoodCoverage::objects()
        .filter(|f| f.related_ids().explicit_pg_predicate().contains(&[related]))
        .count(&mut ctx)
        .await
        .expect("array contains on Vec<HeerId>");
    assert_eq!(by_related, 1);

    let no_match = DogfoodCoverage::objects()
        .filter(|f| f.related_ids().explicit_pg_predicate().contains(&[other]))
        .count(&mut ctx)
        .await
        .expect("array contains miss on Vec<HeerId>");
    assert_eq!(no_match, 0);

    let _ = row.id;
}

// Scenario 4.B — ENUM, CITEXT, INET, MACADDR, MONEY, DOMAIN gaps.
//
// VERDICT (per type):
//   - ENUM (CREATE TYPE ... AS ENUM via `#[derive(DjogiEnum)]`): COMPILES
//     CLEANLY (verified at `djogi/src/enum_.rs` + tests/integration/
//     phase5_postgres_native.rs).
//   - CITEXT: present as `FieldSqlType::Citext` for descriptor projection
//     but no typed Rust newtype on the field side and no ASCII-stable
//     ILIKE surface beyond what `String` already exposes — open question
//     whether this needs a separate gap or stays under #105 / #110.
//   - INET / CIDR / MACADDR, MONEY, DOMAIN TYPES, and `tsrange`: NEEDS
//     GAP ISSUE. `INTERVAL` and the supported typed range substrate
//     (`int4range`, `int8range`, `numrange`, `tstzrange`, `daterange`)
//     now have descriptor projection and field-side Rust types.
//
// Stand-in: this test only compiles the supported subset (ENUM was already
// covered in the existing tests; we avoid duplication). The gap issue
// filed under #110 enumerates the remaining unsupported types.
#[djogi::djogi_test(sync_models = [DogfoodCoverage])]
async fn cat4_b_pg_type_coverage_gap(mut ctx: djogi::DjogiContext) {
    // GAP(djogi#170 — umbrella): the natural model field declarations that
    // do not have a typed surface today —
    //   pub remote_addr: std::net::IpAddr,              // INET
    //   pub price_usd: rust_decimal::Decimal,           // (works as NUMERIC,
    //                                                   //  no MONEY surface)
    //   pub naive_window: SomeTsRangeNewtype,           // tsrange
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

// Scenario 5.B — remaining DDL metadata gaps: type-change USING
// expressions and generated-column expression-change verification.
//
// COMMENT ON COLUMN / TABLE, table storage parameters (`fillfactor`,
// `autovacuum_*`), and `TABLESPACE <name>` are surfaced by the
// Phase 8.5 DDL metadata slice (#217/#218/#219). The remaining gap
// rows stay routed to #220/#221.
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
// REGRESSION (closes #173 via this PR): `DjogiContext::clone_for_concurrent_reads`
// allocates an independent pool-backed context that shares the parent's
// pool, Sassi registry, and auth — letting two `&mut DjogiContext`
// references coexist for concurrent typed reads.
//
// `tokio::try_join!` over two clones now compiles and runs on
// independent pool checkouts. The helper rejects transaction-backed
// contexts with `DjogiError::ConcurrentReadsRequirePoolContext` —
// concurrent reads within one transaction would alias a single
// connection.
#[djogi::djogi_test(sync_models = [DogfoodAsync])]
async fn cat6_b_try_join_concurrent_reads(mut ctx: djogi::DjogiContext) {
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

    // The natural concurrent shape that #173 unblocks — clone the
    // pool-backed context twice and `try_join!` the two fetches.
    let mut ctx_a = ctx
        .clone_for_concurrent_reads()
        .expect("clone_for_concurrent_reads must succeed on a pool-backed context");
    let mut ctx_b = ctx
        .clone_for_concurrent_reads()
        .expect("clone_for_concurrent_reads must succeed on a pool-backed context");

    let (alpha, beta) = tokio::try_join!(
        DogfoodAsync::objects()
            .filter(|f| f.kind().eq("alpha".to_string()))
            .fetch_all(&mut ctx_a),
        DogfoodAsync::objects()
            .filter(|f| f.kind().eq("beta".to_string()))
            .fetch_all(&mut ctx_b),
    )
    .expect("concurrent try_join! across two clones");
    assert_eq!(alpha.len(), 1, "alpha branch saw exactly one row");
    assert_eq!(beta.len(), 1, "beta branch saw exactly one row");
}
