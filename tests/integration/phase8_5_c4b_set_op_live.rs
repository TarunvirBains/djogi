// Phase 8.5 Cluster 4B (#101) — typed set operations: live Postgres tests.
//
// Pairs with the SQL-shape-only `phase8_5_c4b_set_op_sql_shape` test by
// proving the four set operators produce the row sets Postgres
// semantics promise, against a real database:
//
// 1. `union` — de-duplicated union; rows appearing in both arms surface
//    once.
// 2. `union_all` — duplicate-preserving union; row counts add up.
// 3. `intersect` — only rows present in both arms.
// 4. `except` — rows in the left arm not in the right; non-symmetric.
// 5. Outer `ORDER BY` / `LIMIT` / `OFFSET` apply to the combined result.
// 6. `.count()` reports the cardinality of the combined set, not the
//    arms.
// 7. Empty arms (`.none()`) compose correctly through every operator.
// 8. Nested chaining (`a.union(b).intersect(c)`) evaluates
//    left-associatively.
//
// All test bodies use the typed surface only — `Animal::create` for
// seeding, `Animal::objects()` for arms, `SetOpQuerySet::fetch_all` /
// `.count()` / `.first()` for terminals. No raw SQL escape hatches.

use djogi::prelude::*;

#[model(table = "phase8_5_c4b_set_op_animals", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Animal {
    pub name: String,
    pub species: String,
    pub age_months: i32,
    pub adopted: bool,
}

async fn seed_animals(ctx: &mut djogi::DjogiContext) {
    // Mix of adopted vs non-adopted, two species, various ages so the
    // four operators each have a non-empty meaningful result.
    for (name, species, age, adopted) in [
        ("alpha", "dog", 12, true),
        ("beta", "dog", 36, false),
        ("gamma", "cat", 6, true),
        ("delta", "cat", 48, false),
        ("epsilon", "dog", 60, true),
        ("zeta", "rabbit", 18, true),
    ] {
        Animal::create(
            ctx,
            Animal {
                name: name.to_string(),
                species: species.to_string(),
                age_months: age,
                adopted,
                ..Default::default()
            },
        )
        .await
        .expect("seed Animal::create should succeed");
    }
}

#[djogi::djogi_test(sync_models = [Animal])]
async fn set_op_union_deduplicates_overlapping_rows(mut ctx: djogi::DjogiContext) {
    seed_animals(&mut ctx).await;

    // Two arms whose result sets overlap on "alpha" (a dog AND adopted).
    let dogs = Animal::objects().filter(|f| f.species().eq("dog".to_string()));
    let adopted = Animal::objects().filter(|f| f.adopted().eq(true));

    // Union de-duplicates; "alpha" / "epsilon" appear in both arms but
    // surface once each. Expected distinct names: alpha, beta, epsilon
    // (the three dogs) ∪ alpha, gamma, epsilon, zeta (the adopted) =
    // {alpha, beta, epsilon, gamma, zeta} → 5 rows.
    let rows = dogs.union(adopted).fetch_all(&mut ctx).await.unwrap();
    let mut names: Vec<String> = rows.iter().map(|a| a.name.clone()).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["alpha", "beta", "epsilon", "gamma", "zeta"],
        "UNION must de-duplicate; rows in both arms appear once"
    );
}

#[djogi::djogi_test(sync_models = [Animal])]
async fn set_op_union_all_preserves_duplicates(mut ctx: djogi::DjogiContext) {
    seed_animals(&mut ctx).await;

    let dogs = Animal::objects().filter(|f| f.species().eq("dog".to_string()));
    let adopted = Animal::objects().filter(|f| f.adopted().eq(true));

    // 3 dogs + 4 adopted = 7 total rows with UNION ALL (the overlap on
    // alpha + epsilon is preserved, not collapsed).
    let rows = dogs.union_all(adopted).fetch_all(&mut ctx).await.unwrap();
    assert_eq!(
        rows.len(),
        7,
        "UNION ALL must preserve duplicates from both arms"
    );

    // Names occurring twice: alpha (dog + adopted), epsilon (dog + adopted).
    let alpha_count = rows.iter().filter(|r| r.name == "alpha").count();
    let epsilon_count = rows.iter().filter(|r| r.name == "epsilon").count();
    assert_eq!(alpha_count, 2, "alpha must appear in both arms");
    assert_eq!(epsilon_count, 2, "epsilon must appear in both arms");
}

#[djogi::djogi_test(sync_models = [Animal])]
async fn set_op_intersect_returns_rows_in_both_arms(mut ctx: djogi::DjogiContext) {
    seed_animals(&mut ctx).await;

    let dogs = Animal::objects().filter(|f| f.species().eq("dog".to_string()));
    let adopted = Animal::objects().filter(|f| f.adopted().eq(true));

    // INTERSECT — only rows whose full tuple appears in both arms.
    // alpha (dog + adopted) and epsilon (dog + adopted) qualify.
    let rows = dogs.intersect(adopted).fetch_all(&mut ctx).await.unwrap();
    let mut names: Vec<String> = rows.iter().map(|a| a.name.clone()).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["alpha", "epsilon"],
        "INTERSECT must return only rows in both arms"
    );
}

#[djogi::djogi_test(sync_models = [Animal])]
async fn set_op_except_returns_left_minus_right(mut ctx: djogi::DjogiContext) {
    seed_animals(&mut ctx).await;

    let dogs = Animal::objects().filter(|f| f.species().eq("dog".to_string()));
    let adopted = Animal::objects().filter(|f| f.adopted().eq(true));

    // EXCEPT — dogs not in adopted = {beta}. alpha + epsilon are
    // adopted dogs and get subtracted.
    let rows = dogs.except(adopted).fetch_all(&mut ctx).await.unwrap();
    let names: Vec<String> = rows.iter().map(|a| a.name.clone()).collect();
    assert_eq!(
        names,
        vec!["beta"],
        "EXCEPT must return left-arm rows absent from the right arm"
    );
}

#[djogi::djogi_test(sync_models = [Animal])]
async fn set_op_except_is_not_symmetric(mut ctx: djogi::DjogiContext) {
    seed_animals(&mut ctx).await;

    let dogs = Animal::objects().filter(|f| f.species().eq("dog".to_string()));
    let adopted = Animal::objects().filter(|f| f.adopted().eq(true));

    // adopted minus dogs = {gamma, zeta} (the adopted non-dogs).
    let rows = adopted.except(dogs).fetch_all(&mut ctx).await.unwrap();
    let mut names: Vec<String> = rows.iter().map(|a| a.name.clone()).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["gamma", "zeta"],
        "EXCEPT is not symmetric: adopted - dogs = adopted non-dogs"
    );
}

#[djogi::djogi_test(sync_models = [Animal])]
async fn set_op_outer_order_by_and_limit_apply_to_combined_result(mut ctx: djogi::DjogiContext) {
    seed_animals(&mut ctx).await;

    let dogs = Animal::objects().filter(|f| f.species().eq("dog".to_string()));
    let cats = Animal::objects().filter(|f| f.species().eq("cat".to_string()));

    // Outer ORDER BY by name, outer LIMIT 3 — pulls the three
    // lexicographically smallest dog-or-cat names.
    let rows = dogs
        .union(cats)
        .order_by(|f| f.name().asc())
        .limit(3)
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    let names: Vec<String> = rows.iter().map(|a| a.name.clone()).collect();
    // dogs + cats = {alpha, beta, gamma, delta, epsilon}; sorted = {alpha,
    // beta, delta, epsilon, gamma}; first 3 = {alpha, beta, delta}.
    assert_eq!(names, vec!["alpha", "beta", "delta"]);
}

#[djogi::djogi_test(sync_models = [Animal])]
async fn set_op_outer_offset_paginates(mut ctx: djogi::DjogiContext) {
    seed_animals(&mut ctx).await;

    let all = Animal::objects().filter(|f| f.age_months().lte(12i32));
    let young_dogs =
        Animal::objects().filter(|f| f.species().eq("dog".to_string()) & f.age_months().lte(12i32));

    // Same filter, different shape — UNION ALL preserves both copies.
    // Outer offset 1 + outer limit 2 windows into the combined result.
    let rows = all
        .union_all(young_dogs)
        .order_by(|f| f.name().asc())
        .limit(2)
        .offset(1)
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "outer LIMIT 2 caps the combined result");
}

#[djogi::djogi_test(sync_models = [Animal])]
async fn set_op_count_reports_combined_cardinality(mut ctx: djogi::DjogiContext) {
    seed_animals(&mut ctx).await;

    let dogs = Animal::objects().filter(|f| f.species().eq("dog".to_string()));
    let adopted = Animal::objects().filter(|f| f.adopted().eq(true));

    // UNION (dedup) → 5 distinct rows.
    let union_count = dogs
        .clone()
        .union(adopted.clone())
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(union_count, 5);

    // UNION ALL (preserve dups) → 7 rows.
    let union_all_count = dogs
        .clone()
        .union_all(adopted.clone())
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(union_all_count, 7);

    // INTERSECT → 2 rows (alpha, epsilon).
    let intersect_count = dogs
        .clone()
        .intersect(adopted.clone())
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(intersect_count, 2);

    // EXCEPT → 1 row (beta).
    let except_count = dogs.except(adopted).count(&mut ctx).await.unwrap();
    assert_eq!(except_count, 1);
}

#[djogi::djogi_test(sync_models = [Animal])]
async fn set_op_count_strips_outer_limit_offset(mut ctx: djogi::DjogiContext) {
    seed_animals(&mut ctx).await;

    let dogs = Animal::objects().filter(|f| f.species().eq("dog".to_string()));
    let cats = Animal::objects().filter(|f| f.species().eq("cat".to_string()));

    // Outer LIMIT 1 and OFFSET 2 must NOT affect the count — the count
    // emitter strips them so cardinality reflects the full set-op
    // result, not the windowed slice the caller would see in fetch_all.
    let count = dogs
        .union(cats)
        .order_by(|f| f.name().asc())
        .limit(1)
        .offset(2)
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(count, 5, "count ignores outer LIMIT/OFFSET");
}

#[djogi::djogi_test(sync_models = [Animal])]
async fn set_op_first_returns_first_outer_ordered_row(mut ctx: djogi::DjogiContext) {
    seed_animals(&mut ctx).await;

    let dogs = Animal::objects().filter(|f| f.species().eq("dog".to_string()));
    let cats = Animal::objects().filter(|f| f.species().eq("cat".to_string()));

    let row = dogs
        .union(cats)
        .order_by(|f| f.name().asc())
        .first(&mut ctx)
        .await
        .unwrap()
        .expect("first must find a row in non-empty set op");
    assert_eq!(row.name, "alpha", "outer ORDER BY name ASC → alpha");
}

#[djogi::djogi_test(sync_models = [Animal])]
async fn set_op_first_on_empty_intersect_returns_none(mut ctx: djogi::DjogiContext) {
    seed_animals(&mut ctx).await;

    // No dog is a cat — the intersection on the species column is
    // empty. `.first()` returns Ok(None) without erroring.
    let dogs = Animal::objects().filter(|f| f.species().eq("dog".to_string()));
    let cats = Animal::objects().filter(|f| f.species().eq("cat".to_string()));
    let row = dogs.intersect(cats).first(&mut ctx).await.unwrap();
    assert!(
        row.is_none(),
        "intersect of disjoint species sets must be empty"
    );
}

#[djogi::djogi_test(sync_models = [Animal])]
async fn set_op_none_arm_composes_correctly(mut ctx: djogi::DjogiContext) {
    seed_animals(&mut ctx).await;

    let dogs = Animal::objects().filter(|f| f.species().eq("dog".to_string()));
    let empty = Animal::objects().none();

    // dogs UNION empty = dogs (3 rows).
    let union = dogs
        .clone()
        .union(empty.clone())
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(union, 3);

    // dogs INTERSECT empty = empty.
    let intersect = dogs
        .clone()
        .intersect(empty.clone())
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(intersect, 0);

    // dogs EXCEPT empty = dogs (3 rows — nothing to subtract).
    let except = dogs.except(empty).count(&mut ctx).await.unwrap();
    assert_eq!(except, 3);
}

#[djogi::djogi_test(sync_models = [Animal])]
async fn set_op_nested_chain_evaluates_left_associatively(mut ctx: djogi::DjogiContext) {
    seed_animals(&mut ctx).await;

    let dogs = Animal::objects().filter(|f| f.species().eq("dog".to_string()));
    let cats = Animal::objects().filter(|f| f.species().eq("cat".to_string()));
    let adopted = Animal::objects().filter(|f| f.adopted().eq(true));

    // (dogs UNION cats) INTERSECT adopted — left-associative:
    //   dogs UNION cats = {alpha, beta, epsilon, gamma, delta}
    //   INTERSECT adopted = {alpha, gamma, epsilon} (the adopted dogs+cats)
    let nested = dogs.union(cats).intersect(adopted);
    let rows = nested.fetch_all(&mut ctx).await.unwrap();
    let mut names: Vec<String> = rows.iter().map(|a| a.name.clone()).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["alpha", "epsilon", "gamma"],
        "(dogs UNION cats) INTERSECT adopted should evaluate left-associatively"
    );
}

#[djogi::djogi_test(sync_models = [Animal])]
async fn set_op_arm_with_lock_rejected_at_terminal(mut ctx: djogi::DjogiContext) {
    seed_animals(&mut ctx).await;

    let plain = Animal::objects().filter(|f| f.species().eq("dog".to_string()));
    let locked = Animal::objects()
        .filter(|f| f.adopted().eq(true))
        .select_for_update();

    // The terminal returns the typed SetOpArmInvalid error WITHOUT
    // issuing any SQL — Postgres would itself reject FOR UPDATE inside
    // a set-op subquery, but djogi surfaces the higher-fidelity error
    // before the round trip.
    let err = plain.union(locked).fetch_all(&mut ctx).await.unwrap_err();
    assert!(
        matches!(err, DjogiError::SetOpArmInvalid { side, .. } if side == "right"),
        "lock on right arm must surface as SetOpArmInvalid{{side=\"right\"}}: {err:?}"
    );
    assert!(
        err.is_terminal(),
        "SetOpArmInvalid is a programming error — not transient"
    );
}

#[djogi::djogi_test(sync_models = [Animal])]
async fn set_op_filter_combined_via_two_disjoint_arms(mut ctx: djogi::DjogiContext) {
    seed_animals(&mut ctx).await;

    // Reproduces the issue's motivating example: "find dogs that are
    // either adopted OR fostered (using a status column proxy)" via
    // two disjoint arms unioned.
    let young = Animal::objects().filter(|f| f.age_months().lte(12i32));
    let elderly = Animal::objects().filter(|f| f.age_months().gte(48i32));

    let rows = young.union(elderly).fetch_all(&mut ctx).await.unwrap();
    let mut names: Vec<String> = rows.iter().map(|a| a.name.clone()).collect();
    names.sort();
    // young (age <= 12): alpha (12), gamma (6) → {alpha, gamma}
    // elderly (age >= 48): delta (48), epsilon (60) → {delta, epsilon}
    // union (disjoint sets) = {alpha, delta, epsilon, gamma}.
    assert_eq!(names, vec!["alpha", "delta", "epsilon", "gamma"]);
}
