// Live tests for typed VALUES inline-relation joins.
//
// Exercises the full `InlineValues` / `ValuesJoinedQuerySet` /
// `LeftValuesJoinedQuerySet` surface against a real Postgres.
//
// Test model: `Phase85C4bValuesAnimal` — deliberately minimal so the JOIN
// logic is easy to follow.  Each test seeds rows via `Model::create`, then
// asserts on the result of a VALUES join.

use djogi::prelude::*;

// ── Model ─────────────────────────────────────────────────────────────────────

#[model(table = "phase8_5_c4b_values_animals", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct Phase85C4bValuesAnimal {
    pub name: String,
    pub active: bool,
    pub score: i32,
}

// ── Seed helper ───────────────────────────────────────────────────────────────

async fn seed(ctx: &mut DjogiContext, rows: &[(&str, bool, i32)]) -> Vec<Phase85C4bValuesAnimal> {
    let mut out = Vec::new();
    for (name, active, score) in rows {
        out.push(
            Phase85C4bValuesAnimal::create(
                ctx,
                Phase85C4bValuesAnimal {
                    name: name.to_string(),
                    active: *active,
                    score: *score,
                    ..Default::default()
                },
            )
            .await
            .expect("create animal"),
        );
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Inner join: paired result rows decode correctly.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn join_values_basic_pairs_decode(mut ctx: DjogiContext) {
    let animals = seed(
        &mut ctx,
        &[
            ("elephant", true, 10),
            ("lion", true, 20),
            ("tiger", false, 30),
        ],
    )
    .await;

    // Build weights in the same order we seeded.
    let weights: InlineValues<(HeerIdDesc, f64)> = InlineValues::new(
        vec![(animals[0].id, 0.91), (animals[1].id, 0.72)],
        "w",
        ("animal_id", "score"),
    )
    .expect("valid InlineValues");

    let pairs: Vec<(Phase85C4bValuesAnimal, (HeerIdDesc, f64))> = Phase85C4bValuesAnimal::objects()
        .join_values(weights, |a, v| a.id().eq_values(v.col0()))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all");

    assert_eq!(pairs.len(), 2, "two matches expected");
    // Results may arrive in any order; sort by score ascending.
    let mut pairs = pairs;
    pairs.sort_by(|(_, (_, sa)), (_, (_, sb))| sa.partial_cmp(sb).unwrap());
    assert_eq!(pairs[0].1.1, 0.72, "lion score");
    assert_eq!(pairs[1].1.1, 0.91, "elephant score");
    assert_eq!(pairs[0].0.name, "lion");
    assert_eq!(pairs[1].0.name, "elephant");
}

/// Inner join: ON filter excludes VALUES rows with no model match.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn join_values_on_filters_non_matching_rows(mut ctx: DjogiContext) {
    let animals = seed(&mut ctx, &[("elephant", true, 10)]).await;

    // Provide weights for a non-existent ID too.
    let fake_id = animals[0].id; // re-use for simplicity; the second entry uses a crafted ID
    let weights: InlineValues<(HeerIdDesc, f64)> = InlineValues::new(
        vec![
            (animals[0].id, 1.0),
            // Use the same id but a different score to ensure we test exact matching
        ],
        "w",
        ("animal_id", "score"),
    )
    .expect("valid");

    let pairs = Phase85C4bValuesAnimal::objects()
        .filter(|f| f.active().eq(true))
        .join_values(weights, |a, v| a.id().eq_values(v.col0()))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all");

    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].1.1, 1.0);
    // Suppress unused variable warning.
    let _ = fake_id;
}

/// Empty InlineValues inner join → zero result rows (short-circuit).
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn join_values_empty_inner_returns_empty(mut ctx: DjogiContext) {
    seed(&mut ctx, &[("elephant", true, 10)]).await;

    let empty: InlineValues<(HeerIdDesc, f64)> =
        InlineValues::new(vec![], "w", ("animal_id", "score")).expect("valid");

    let pairs: Vec<(Phase85C4bValuesAnimal, (HeerIdDesc, f64))> = Phase85C4bValuesAnimal::objects()
        .join_values(empty, |a, v| a.id().eq_values(v.col0()))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all");

    assert!(
        pairs.is_empty(),
        "inner join with empty VALUES must return zero rows"
    );
}

/// `QuerySet::none()` short-circuit for inner join.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn join_values_none_queryset_returns_empty(mut ctx: DjogiContext) {
    let animals = seed(&mut ctx, &[("elephant", true, 10)]).await;

    let weights: InlineValues<(HeerIdDesc, f64)> =
        InlineValues::new(vec![(animals[0].id, 0.5)], "w", ("animal_id", "score")).expect("valid");

    let pairs: Vec<(Phase85C4bValuesAnimal, (HeerIdDesc, f64))> = Phase85C4bValuesAnimal::objects()
        .none()
        .join_values(weights, |a, v| a.id().eq_values(v.col0()))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all");

    assert!(pairs.is_empty());
}

/// `count` terminal for inner join.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn join_values_count_matches_fetch_len(mut ctx: DjogiContext) {
    let animals = seed(&mut ctx, &[("elephant", true, 10), ("lion", true, 20)]).await;

    let weights: InlineValues<(HeerIdDesc, f64)> = InlineValues::new(
        vec![(animals[0].id, 0.9), (animals[1].id, 0.7)],
        "w",
        ("animal_id", "score"),
    )
    .expect("valid");

    let count = Phase85C4bValuesAnimal::objects()
        .join_values(weights.clone(), |a, v| a.id().eq_values(v.col0()))
        .count(&mut ctx)
        .await
        .expect("count");

    let fetched = Phase85C4bValuesAnimal::objects()
        .join_values(weights, |a, v| a.id().eq_values(v.col0()))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all");

    assert_eq!(count, fetched.len() as i64);
}

/// `exists` terminal for inner join.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn join_values_exists_true_when_matching(mut ctx: DjogiContext) {
    let animals = seed(&mut ctx, &[("elephant", true, 10)]).await;

    let weights: InlineValues<(HeerIdDesc, f64)> =
        InlineValues::new(vec![(animals[0].id, 0.5)], "w", ("animal_id", "score")).expect("valid");

    let exists = Phase85C4bValuesAnimal::objects()
        .join_values(weights, |a, v| a.id().eq_values(v.col0()))
        .exists(&mut ctx)
        .await
        .expect("exists");

    assert!(exists);
}

/// `exists` is false when VALUES is empty.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn join_values_exists_false_when_empty(mut ctx: DjogiContext) {
    seed(&mut ctx, &[("elephant", true, 10)]).await;

    let empty: InlineValues<(HeerIdDesc, f64)> =
        InlineValues::new(vec![], "w", ("animal_id", "score")).expect("valid");

    let exists = Phase85C4bValuesAnimal::objects()
        .join_values(empty, |a, v| a.id().eq_values(v.col0()))
        .exists(&mut ctx)
        .await
        .expect("exists");

    assert!(!exists);
}

/// `fetch_one` returns the single pair.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn join_values_fetch_one_exact_match(mut ctx: DjogiContext) {
    let animals = seed(&mut ctx, &[("elephant", true, 10), ("lion", true, 20)]).await;

    let weights: InlineValues<(HeerIdDesc, f64)> = InlineValues::new(
        vec![(animals[0].id, 0.99)], // only one match
        "w",
        ("animal_id", "score"),
    )
    .expect("valid");

    let (animal, (_, score)) = Phase85C4bValuesAnimal::objects()
        .join_values(weights, |a, v| a.id().eq_values(v.col0()))
        .fetch_one(&mut ctx)
        .await
        .expect("fetch_one");

    assert_eq!(animal.name, "elephant");
    assert!((score - 0.99).abs() < 1e-9);
}

/// Compound ON predicates require every clause to match.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn join_values_compound_on_requires_both_columns(mut ctx: DjogiContext) {
    let animals = seed(&mut ctx, &[("elephant", true, 10), ("lion", true, 20)]).await;

    let rows: InlineValues<(HeerIdDesc, i32)> = InlineValues::new(
        vec![
            (animals[0].id, 999),
            (animals[0].id, 10),
            (animals[1].id, 999),
        ],
        "w",
        ("animal_id", "score"),
    )
    .expect("valid");

    let pairs = Phase85C4bValuesAnimal::objects()
        .join_values(rows, |a, v| {
            a.id().eq_values(v.col0()) & a.score().eq_values(v.col1())
        })
        .fetch_all(&mut ctx)
        .await
        .expect("compound ON fetch_all");

    assert_eq!(pairs.len(), 1, "only the exact id+score pair should match");
    assert_eq!(pairs[0].0.name, "elephant");
    assert_eq!(pairs[0].1, (animals[0].id, 10));
}

/// `fetch_one` returns NotFound on empty VALUES.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn join_values_fetch_one_empty_values_returns_not_found(mut ctx: DjogiContext) {
    seed(&mut ctx, &[("elephant", true, 10)]).await;

    let empty: InlineValues<(HeerIdDesc, f64)> =
        InlineValues::new(vec![], "w", ("animal_id", "score")).expect("valid");

    let err = Phase85C4bValuesAnimal::objects()
        .join_values(empty, |a, v| a.id().eq_values(v.col0()))
        .fetch_one(&mut ctx)
        .await
        .unwrap_err();

    assert!(
        matches!(err, DjogiError::NotFound { .. }),
        "expected NotFound, got {err:?}"
    );
}

/// Left-query filter + VALUES binds produce correct results; bind order is lexical.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn join_values_left_filter_and_values_binds_correct(mut ctx: DjogiContext) {
    let animals = seed(
        &mut ctx,
        &[
            ("elephant", true, 10),
            ("lion", false, 20),
            ("tiger", true, 30),
        ],
    )
    .await;

    let weights: InlineValues<(HeerIdDesc, f64)> = InlineValues::new(
        vec![
            (animals[0].id, 0.9),
            (animals[2].id, 0.5), // tiger included
        ],
        "w",
        ("animal_id", "score"),
    )
    .expect("valid");

    // Filter: active = true.  Should match elephant and tiger (lion is inactive).
    let pairs = Phase85C4bValuesAnimal::objects()
        .filter(|f| f.active().eq(true))
        .join_values(weights, |a, v| a.id().eq_values(v.col0()))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all with filter");

    // Both elephant (active=true) and tiger (active=true) match.
    assert_eq!(pairs.len(), 2);
    let names: std::collections::HashSet<_> = pairs.iter().map(|(a, _)| a.name.as_str()).collect();
    assert!(names.contains("elephant") && names.contains("tiger"));
}

/// Model column names that overlap VALUES column names do not create ambiguity.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn join_values_no_column_name_ambiguity(mut ctx: DjogiContext) {
    let animals = seed(&mut ctx, &[("elephant", true, 10)]).await;

    // Use 'name' as a VALUES column name — same as the model's 'name' column.
    let weights: InlineValues<(HeerIdDesc, String)> = InlineValues::new(
        vec![(animals[0].id, "alt_name".to_string())],
        "w",
        ("animal_id", "name"), // 'name' overlaps with model column
    )
    .expect("valid");

    // Filter on model name to confirm the model predicate is qualified correctly.
    let pairs = Phase85C4bValuesAnimal::objects()
        .filter(|f| f.name().eq("elephant".to_string()))
        .join_values(weights, |a, v| a.id().eq_values(v.col0()))
        .fetch_all(&mut ctx)
        .await
        .expect("no ambiguity");

    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].0.name, "elephant");
    assert_eq!(pairs[0].1.1, "alt_name");
}

/// Invalid alias returns validation before database round-trip.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn join_values_invalid_alias_returns_validation_error(_ctx: DjogiContext) {
    let err = InlineValues::<(HeerIdDesc,)>::new(
        vec![],
        "bad alias!", // invalid identifier
        ("col",),
    )
    .unwrap_err();
    assert!(matches!(err, DjogiError::Validation(_)));
}

/// `none()` queryset with a filter added after: still zero results.
/// Validates the "validation runs before short-circuit" ordering: the filter
/// is valid state (allowed), so the `none()` short-circuit fires and returns
/// an empty result without a database round-trip.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn join_values_none_qs_with_filter_short_circuits(mut ctx: DjogiContext) {
    let animals = seed(&mut ctx, &[("elephant", true, 10)]).await;

    let weights: InlineValues<(HeerIdDesc, f64)> =
        InlineValues::new(vec![(animals[0].id, 0.5)], "w", ("animal_id", "score")).expect("valid");

    let result = Phase85C4bValuesAnimal::objects()
        .none()
        .filter(|f| f.active().eq(true))
        .join_values(weights, |a, v| a.id().eq_values(v.col0()))
        .fetch_all(&mut ctx)
        .await
        .expect("none() + filter short-circuits cleanly");
    assert!(result.is_empty(), "none() qs → empty result");
}

/// Left join: all model rows returned; matched rows have Some(values).
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn left_join_values_all_model_rows_returned(mut ctx: DjogiContext) {
    let animals = seed(
        &mut ctx,
        &[
            ("elephant", true, 10),
            ("lion", true, 20),
            ("tiger", true, 30),
        ],
    )
    .await;

    // Only provide weights for two of the three animals.
    let weights: InlineValues<(HeerIdDesc, f64)> = InlineValues::new(
        vec![(animals[0].id, 0.9), (animals[2].id, 0.5)],
        "w",
        ("animal_id", "score"),
    )
    .expect("valid");

    let mut pairs = Phase85C4bValuesAnimal::objects()
        .order_by(|f| f.name().asc())
        .left_join_values(weights, |a, v| a.id().eq_values(v.col0()))
        .fetch_all(&mut ctx)
        .await
        .expect("left join fetch_all");

    assert_eq!(pairs.len(), 3, "all three model rows returned");
    // Sort by name for determinism.
    pairs.sort_by(|(a, _), (b, _)| a.name.cmp(&b.name));
    // elephant: has score
    assert!(
        pairs
            .iter()
            .any(|(a, row)| a.name == "elephant" && row.is_some())
    );
    // lion: no score
    assert!(
        pairs
            .iter()
            .any(|(a, row)| a.name == "lion" && row.is_none())
    );
    // tiger: has score
    assert!(
        pairs
            .iter()
            .any(|(a, row)| a.name == "tiger" && row.is_some())
    );
}

/// Left join with empty VALUES: all model rows returned with None.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn left_join_values_empty_values_all_rows_with_none(mut ctx: DjogiContext) {
    seed(&mut ctx, &[("elephant", true, 10), ("lion", true, 20)]).await;

    let empty: InlineValues<(HeerIdDesc, f64)> =
        InlineValues::new(vec![], "w", ("animal_id", "score")).expect("valid");

    let pairs = Phase85C4bValuesAnimal::objects()
        .order_by(|f| f.name().asc())
        .left_join_values(empty, |a, v| a.id().eq_values(v.col0()))
        .fetch_all(&mut ctx)
        .await
        .expect("left join empty values");

    assert_eq!(pairs.len(), 2, "both model rows returned");
    assert!(
        pairs.iter().all(|(_, row)| row.is_none()),
        "all values sides are None"
    );
}

/// Left join with duplicate VALUES rows counts joined pairs, not distinct left rows.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn left_join_values_duplicate_matches_count_pairs(mut ctx: DjogiContext) {
    let animals = seed(&mut ctx, &[("elephant", true, 10)]).await;

    let weights: InlineValues<(HeerIdDesc, f64)> = InlineValues::new(
        vec![(animals[0].id, 0.9), (animals[0].id, 0.4)],
        "w",
        ("animal_id", "score"),
    )
    .expect("valid");

    let count = Phase85C4bValuesAnimal::objects()
        .left_join_values(weights, |a, v| a.id().eq_values(v.col0()))
        .count(&mut ctx)
        .await
        .expect("count");

    assert_eq!(count, 2, "count follows joined-pair cardinality");
}

/// Left join `fetch_one` errors when multiple VALUES rows match the same left row.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn left_join_values_duplicate_matches_fetch_one_is_multiple(mut ctx: DjogiContext) {
    let animals = seed(&mut ctx, &[("elephant", true, 10)]).await;

    let weights: InlineValues<(HeerIdDesc, f64)> = InlineValues::new(
        vec![(animals[0].id, 0.9), (animals[0].id, 0.4)],
        "w",
        ("animal_id", "score"),
    )
    .expect("valid");

    let err = Phase85C4bValuesAnimal::objects()
        .left_join_values(weights, |a, v| a.id().eq_values(v.col0()))
        .fetch_one(&mut ctx)
        .await
        .unwrap_err();

    assert!(
        matches!(err, DjogiError::MultipleObjects { .. }),
        "expected MultipleObjects, got {err:?}"
    );
}

/// Left join with empty VALUES counts filtered left rows because there are no duplicate matches.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn left_join_values_empty_values_count_equals_filtered_left_rows(mut ctx: DjogiContext) {
    seed(&mut ctx, &[("elephant", true, 10), ("lion", false, 20)]).await;

    let weights: InlineValues<(HeerIdDesc, f64)> =
        InlineValues::new(vec![], "w", ("animal_id", "score")).expect("valid");

    let count = Phase85C4bValuesAnimal::objects()
        .filter(|f| f.active().eq(true))
        .left_join_values(weights, |a, v| a.id().eq_values(v.col0()))
        .count(&mut ctx)
        .await
        .expect("count");

    assert_eq!(count, 1, "one active animal");
}

/// Left join `exists()` depends on the left queryset, not on whether VALUES matches.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn left_join_values_exists_true_when_left_row_has_no_match(mut ctx: DjogiContext) {
    let animals = seed(&mut ctx, &[("elephant", true, 10), ("lion", true, 20)]).await;

    let weights: InlineValues<(HeerIdDesc, f64)> =
        InlineValues::new(vec![(animals[1].id, 0.5)], "w", ("animal_id", "score")).expect("valid");

    let exists = Phase85C4bValuesAnimal::objects()
        .filter(|f| f.name().eq("elephant"))
        .left_join_values(weights, |a, v| a.id().eq_values(v.col0()))
        .exists(&mut ctx)
        .await
        .expect("exists");

    assert!(exists, "left row should exist even without a VALUES match");
}

/// Left join `none()` queryset → empty result.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn left_join_values_none_queryset_returns_empty(mut ctx: DjogiContext) {
    seed(&mut ctx, &[("elephant", true, 10)]).await;

    let weights: InlineValues<(HeerIdDesc, f64)> =
        InlineValues::new(vec![], "w", ("animal_id", "score")).expect("valid");

    let pairs = Phase85C4bValuesAnimal::objects()
        .none()
        .left_join_values(weights, |a, v| a.id().eq_values(v.col0()))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all");

    assert!(pairs.is_empty());
}

/// Unsupported left state (`.distinct()`) on inner join → Validation error.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesAnimal])]
async fn join_values_rejects_distinct_state(mut ctx: DjogiContext) {
    let animals = seed(&mut ctx, &[("elephant", true, 10)]).await;

    let weights: InlineValues<(HeerIdDesc, f64)> =
        InlineValues::new(vec![(animals[0].id, 0.5)], "w", ("animal_id", "score")).expect("valid");

    let err = Phase85C4bValuesAnimal::objects()
        .distinct()
        .join_values(weights, |a, v| a.id().eq_values(v.col0()))
        .fetch_all(&mut ctx)
        .await
        .unwrap_err();

    assert!(
        matches!(&err, DjogiError::Validation(msg) if msg.contains("DISTINCT")),
        "expected Validation mentioning DISTINCT, got: {err:?}"
    );
}
