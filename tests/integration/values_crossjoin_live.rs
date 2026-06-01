// Djogi#299 — typed CROSS JOIN VALUES live tests.
//
// Exercises the full `QuerySet::cross_join_values` / `CrossValuesJoinedQuerySet`
// surface against a real Postgres.  Each test seeds rows via `Model::create`,
// then asserts on the Cartesian product produced by `cross_join_values`.
//
// Test model: `Phase85C4bValuesCrossAnimal` — deliberately minimal so the
// CROSS JOIN logic is easy to follow.

use djogi::prelude::*;

// ── Model ─────────────────────────────────────────────────────────────────────

#[model(table = "phase8_5_c4b_cross_animals", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct Phase85C4bValuesCrossAnimal {
    pub name: String,
    pub active: bool,
}

// ── Seed helper ───────────────────────────────────────────────────────────────

async fn seed(ctx: &mut DjogiContext, rows: &[(&str, bool)]) -> Vec<Phase85C4bValuesCrossAnimal> {
    let mut out = Vec::new();
    for (name, active) in rows {
        out.push(
            Phase85C4bValuesCrossAnimal::create(
                ctx,
                Phase85C4bValuesCrossAnimal {
                    name: name.to_string(),
                    active: *active,
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

/// Basic 2-row model × 2-row VALUES → 4 result rows (Cartesian product).
#[djogi::djogi_test(sync_models = [Phase85C4bValuesCrossAnimal])]
async fn cross_join_values_basic_cartesian_product(mut ctx: DjogiContext) {
    seed(&mut ctx, &[("elephant", true), ("lion", true)]).await;

    let labels: InlineValues<(String,)> = InlineValues::new(
        vec![("label_a".to_string(),), ("label_b".to_string(),)],
        "lbl",
        ("tag",),
    )
    .expect("valid InlineValues");

    let pairs: Vec<(Phase85C4bValuesCrossAnimal, (String,))> =
        Phase85C4bValuesCrossAnimal::objects()
            .cross_join_values(labels)
            .fetch_all(&mut ctx)
            .await
            .expect("fetch_all");

    // 2 model rows × 2 VALUES rows = 4 pairs.
    assert_eq!(pairs.len(), 4, "2 × 2 = 4 Cartesian pairs expected");

    // Every model row must appear paired with every VALUES row.
    let elephant_tags: std::collections::BTreeSet<_> = pairs
        .iter()
        .filter(|(a, _)| a.name == "elephant")
        .map(|(_, (tag,))| tag.as_str())
        .collect();
    let lion_tags: std::collections::BTreeSet<_> = pairs
        .iter()
        .filter(|(a, _)| a.name == "lion")
        .map(|(_, (tag,))| tag.as_str())
        .collect();

    assert_eq!(
        elephant_tags,
        ["label_a", "label_b"].into_iter().collect(),
        "elephant paired with both labels"
    );
    assert_eq!(
        lion_tags,
        ["label_a", "label_b"].into_iter().collect(),
        "lion paired with both labels"
    );
}

/// Empty VALUES → empty result (short-circuit, no DB round-trip needed).
#[djogi::djogi_test(sync_models = [Phase85C4bValuesCrossAnimal])]
async fn cross_join_values_empty_values_returns_empty(mut ctx: DjogiContext) {
    seed(&mut ctx, &[("elephant", true)]).await;

    let empty: InlineValues<(String,)> =
        InlineValues::new(vec![], "lbl", ("tag",)).expect("valid empty InlineValues");

    let pairs: Vec<(Phase85C4bValuesCrossAnimal, (String,))> =
        Phase85C4bValuesCrossAnimal::objects()
            .cross_join_values(empty)
            .fetch_all(&mut ctx)
            .await
            .expect("fetch_all");

    assert!(
        pairs.is_empty(),
        "CROSS JOIN with empty VALUES must return zero rows"
    );
}

/// Empty queryset (none()) → empty result (short-circuit).
#[djogi::djogi_test(sync_models = [Phase85C4bValuesCrossAnimal])]
async fn cross_join_values_none_queryset_returns_empty(mut ctx: DjogiContext) {
    seed(&mut ctx, &[("elephant", true)]).await;

    let labels: InlineValues<(String,)> =
        InlineValues::new(vec![("a".to_string(),)], "lbl", ("tag",)).expect("valid");

    let pairs: Vec<(Phase85C4bValuesCrossAnimal, (String,))> =
        Phase85C4bValuesCrossAnimal::objects()
            .none()
            .cross_join_values(labels)
            .fetch_all(&mut ctx)
            .await
            .expect("fetch_all");

    assert!(pairs.is_empty(), "none() queryset → zero Cartesian pairs");
}

/// count() on a Cartesian product equals model_rows × values_rows.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesCrossAnimal])]
async fn cross_join_values_count_equals_cartesian_product(mut ctx: DjogiContext) {
    seed(
        &mut ctx,
        &[("elephant", true), ("lion", true), ("tiger", false)],
    )
    .await;

    let labels: InlineValues<(String,)> = InlineValues::new(
        vec![("x".to_string(),), ("y".to_string(),), ("z".to_string(),)],
        "lbl",
        ("tag",),
    )
    .expect("valid");

    let count = Phase85C4bValuesCrossAnimal::objects()
        .cross_join_values(labels)
        .count(&mut ctx)
        .await
        .expect("count");

    // 3 model rows × 3 VALUES rows = 9.
    assert_eq!(count, 9, "3 × 3 = 9 Cartesian pairs");
}

/// count() short-circuits to 0 when VALUES is empty.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesCrossAnimal])]
async fn cross_join_values_count_empty_values_is_zero(mut ctx: DjogiContext) {
    seed(&mut ctx, &[("elephant", true)]).await;

    let empty: InlineValues<(String,)> = InlineValues::new(vec![], "lbl", ("tag",)).expect("valid");

    let count = Phase85C4bValuesCrossAnimal::objects()
        .cross_join_values(empty)
        .count(&mut ctx)
        .await
        .expect("count");

    assert_eq!(count, 0, "empty VALUES → count = 0");
}

/// A WHERE filter on the model side reduces the Cartesian product correctly.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesCrossAnimal])]
async fn cross_join_values_model_filter_reduces_product(mut ctx: DjogiContext) {
    // Seed 2 active + 1 inactive.
    seed(
        &mut ctx,
        &[("elephant", true), ("lion", true), ("tiger", false)],
    )
    .await;

    let labels: InlineValues<(String,)> = InlineValues::new(
        vec![("x".to_string(),), ("y".to_string(),)],
        "lbl",
        ("tag",),
    )
    .expect("valid");

    let pairs = Phase85C4bValuesCrossAnimal::objects()
        .filter(|f| f.active().eq(true))
        .cross_join_values(labels)
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all");

    // Only 2 active animals, 2 labels → 4 pairs.
    assert_eq!(pairs.len(), 4, "2 active × 2 labels = 4 pairs");
    // tiger (inactive) must not appear.
    assert!(
        pairs.iter().all(|(a, _)| a.name != "tiger"),
        "tiger is filtered out"
    );
}

/// exists() is true when the product is non-empty.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesCrossAnimal])]
async fn cross_join_values_exists_true_when_non_empty(mut ctx: DjogiContext) {
    seed(&mut ctx, &[("elephant", true)]).await;

    let labels: InlineValues<(String,)> =
        InlineValues::new(vec![("a".to_string(),)], "lbl", ("tag",)).expect("valid");

    let exists = Phase85C4bValuesCrossAnimal::objects()
        .cross_join_values(labels)
        .exists(&mut ctx)
        .await
        .expect("exists");

    assert!(exists, "non-empty product → exists = true");
}

/// exists() is false when VALUES is empty.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesCrossAnimal])]
async fn cross_join_values_exists_false_when_empty_values(mut ctx: DjogiContext) {
    seed(&mut ctx, &[("elephant", true)]).await;

    let empty: InlineValues<(String,)> = InlineValues::new(vec![], "lbl", ("tag",)).expect("valid");

    let exists = Phase85C4bValuesCrossAnimal::objects()
        .cross_join_values(empty)
        .exists(&mut ctx)
        .await
        .expect("exists");

    assert!(!exists, "empty VALUES → exists = false");
}

/// Unsupported left state (`.distinct()`) → Validation error at terminal.
#[djogi::djogi_test(sync_models = [Phase85C4bValuesCrossAnimal])]
async fn cross_join_values_rejects_distinct_state(mut ctx: DjogiContext) {
    seed(&mut ctx, &[("elephant", true)]).await;

    let labels: InlineValues<(String,)> =
        InlineValues::new(vec![("a".to_string(),)], "lbl", ("tag",)).expect("valid");

    let err = Phase85C4bValuesCrossAnimal::objects()
        .distinct()
        .cross_join_values(labels)
        .fetch_all(&mut ctx)
        .await
        .unwrap_err();

    assert!(
        matches!(&err, DjogiError::Validation(msg) if msg.contains("DISTINCT")),
        "expected Validation mentioning DISTINCT, got: {err:?}"
    );
}
