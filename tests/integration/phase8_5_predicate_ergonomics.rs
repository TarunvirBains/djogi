use djogi::prelude::*;

#[model(table = "phase85_predicate_probe", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct PredicateProbe {
    pub label: String,
    pub tracked_label: Tracked<String>,
    pub estimated_birth_year: Option<i16>,
    /// `Tracked<Option<U>>` exercises FIX_BEFORE_BETA-1: the macro
    /// classification strips `Tracked` then `Option`, so without the
    /// Tracked-aware `value_as::<Tracked<Option<U>>>` fallback chain
    /// the runtime would return `ValueTypeMismatch` for every
    /// `f.tracked_estimated_birth_year().eq(Some(_))` /
    /// `.eq(None)` / `.in_([_])` / `.neq(_)` / `.not_in([_])` call.
    pub tracked_estimated_birth_year: Tracked<Option<i16>>,
}

fn probe(
    label: &str,
    tracked_label: &str,
    estimated_birth_year: Option<i16>,
    tracked_estimated_birth_year: Option<i16>,
) -> PredicateProbe {
    PredicateProbe {
        label: label.to_owned(),
        tracked_label: Tracked::new(tracked_label.to_owned()),
        estimated_birth_year,
        tracked_estimated_birth_year: Tracked::new(tracked_estimated_birth_year),
        ..Default::default()
    }
}

fn sorted_ids(rows: &[PredicateProbe]) -> Vec<HeerId> {
    let mut ids = rows.iter().map(|row| row.id).collect::<Vec<_>>();
    ids.sort();
    ids
}

#[djogi::djogi_test(sync_models = [PredicateProbe])]
async fn predicate_ergonomics_accept_wrapped_and_borrowed_values(mut ctx: DjogiContext) {
    let alpha = PredicateProbe::create(
        &mut ctx,
        probe("alpha", "matriarch", Some(1980), Some(1980)),
    )
    .await
    .expect("create alpha probe");
    let beta = PredicateProbe::create(&mut ctx, probe("beta", "calf", Some(2020), Some(2020)))
        .await
        .expect("create beta probe");
    let unknown = PredicateProbe::create(&mut ctx, probe("unknown", "matriarch", None, None))
        .await
        .expect("create unknown probe");

    let str_lookup = PredicateProbe::objects()
        .filter(|f| f.label().eq("alpha"))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch by borrowed str");
    assert_eq!(sorted_ids(&str_lookup), vec![alpha.id]);

    let tracked_lookup = PredicateProbe::objects()
        .filter(|f| f.tracked_label().eq("matriarch"))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch by tracked inner str");
    let mut expected_tracked = vec![alpha.id, unknown.id];
    expected_tracked.sort();
    assert_eq!(sorted_ids(&tracked_lookup), expected_tracked);

    let optional_lookup = PredicateProbe::objects()
        .filter(|f| f.estimated_birth_year().lte(1990_i16))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch by optional inner scalar");
    assert_eq!(sorted_ids(&optional_lookup), vec![alpha.id]);

    // Tracked<Option<U>> — the FIX_BEFORE_BETA-1 surface. eq/neq/in_/not_in
    // accept both the inner Some(_)/None scalar and the wrapped
    // `Tracked<Option<U>>` value (via the `IntoPortableFieldValue<Tracked<V>>`
    // for `V` blanket on the field-side surface). Both codepaths must
    // resolve at runtime through the macro-emitted Tracked-aware
    // `value_as::<Tracked<Option<U>>>` fallback.
    let tracked_opt_eq_some = PredicateProbe::objects()
        .filter(|f| f.tracked_estimated_birth_year().eq(Some(1980_i16)))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch tracked optional by Some(_)");
    assert_eq!(sorted_ids(&tracked_opt_eq_some), vec![alpha.id]);

    let tracked_opt_eq_none = PredicateProbe::objects()
        .filter(|f| f.tracked_estimated_birth_year().eq(None::<i16>))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch tracked optional by None");
    assert_eq!(sorted_ids(&tracked_opt_eq_none), vec![unknown.id]);

    let tracked_opt_in = PredicateProbe::objects()
        .filter(|f| {
            f.tracked_estimated_birth_year()
                .in_(vec![Some(1980_i16), Some(2020_i16)])
        })
        .fetch_all(&mut ctx)
        .await
        .expect("fetch tracked optional by in_ list");
    let mut expected_tracked_in = vec![alpha.id, beta.id];
    expected_tracked_in.sort();
    assert_eq!(sorted_ids(&tracked_opt_in), expected_tracked_in);

    let tracked_opt_neq = PredicateProbe::objects()
        .filter(|f| f.tracked_estimated_birth_year().neq(Some(2020_i16)))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch tracked optional by neq");
    // Per `emit_option_neq`'s NULL semantics: `neq(Some(2020))` lowers to
    // `(col IS NULL OR col <> $1)`, so both `alpha` (1980) and `unknown`
    // (NULL) match.
    let mut expected_tracked_neq = vec![alpha.id, unknown.id];
    expected_tracked_neq.sort();
    assert_eq!(sorted_ids(&tracked_opt_neq), expected_tracked_neq);

    let condition_method_lookup = PredicateProbe::objects()
        .filter(|f| {
            f.label()
                .explicit_pg_predicate()
                .eq("alpha")
                .and(f.tracked_label().explicit_pg_predicate().neq("calf"))
        })
        .fetch_all(&mut ctx)
        .await
        .expect("fetch by method-composed conditions");
    assert_eq!(sorted_ids(&condition_method_lookup), vec![alpha.id]);

    let condition_operator_lookup = PredicateProbe::objects()
        .filter(|f| {
            f.label().explicit_pg_predicate().eq("alpha")
                | f.label().explicit_pg_predicate().eq("beta")
        })
        .fetch_all(&mut ctx)
        .await
        .expect("fetch by operator-composed conditions");
    let mut expected_operator = vec![alpha.id, beta.id];
    expected_operator.sort();
    assert_eq!(sorted_ids(&condition_operator_lookup), expected_operator);
}
