//! Trybuild driver — phase-split for fast targeted verification.
//!
//! Historically this file held one `compile_pass_tests` and one
//! `compile_fail_tests` function, each globbing all 76 / ~50 fixtures.
//! A single new fixture forced the entire sweep on every change
//! (~15 min), which dominated the dev cycle and burned CI minutes on
//! every PR — both blockers for a Free + private GHA repo with a 2k
//! min/month cap.
//!
//! The fix: one `#[test]` per phase prefix. cargo's default test
//! parallelism runs them concurrently; `cargo test compile_pass_phase7_5`
//! filters down to the ~3 fixtures relevant to a single phase change
//! (~30s instead of 15 min). The phase split mirrors the `phase{N}_*.rs`
//! naming convention adopters already use; new phases add a new
//! `#[test]` and a new glob.
//!
//! Full-sweep wall-clock is somewhat worse than the single-glob baseline
//! because each split function ends with `Drop` on a `TestCases` →
//! one `cargo build` invocation, so 22 functions ⇒ 22 invocations vs.
//! the prior 2. We trade that throughput for the targeted-dev win,
//! which is the dominant workflow; full-sweep runs once at PR time.
//!
//! `unphased` collects fixtures that pre-date the phased naming
//! convention (`basic_inject.rs`, `reverse_one_to_*.rs`, the M2M
//! macro samples, etc.). Those are stable and rarely touched, so they
//! get one bucket.
//!
//! Phase 8-Zero (`phase8_zero_*`) covers tree-recursive query surface —
//! `#[model(tree_edge = "...")]` validation (compile_fail), the
//! `RelationPath<T, T>` self-edge type guard on `tree_descendants` /
//! `tree_ancestors` (compile_fail), and the multi-self-FK no-default
//! shape (compile_pass).
//!
//! Note on globs: trybuild uses the `glob` crate, which does NOT support
//! brace expansion (`{a,b}`). Multiple `t.pass(...)` / `t.compile_fail(...)`
//! calls within the same `TestCases` accumulate the fixture list, so
//! the unphased buckets list each pattern individually.
//!
//! # Memory budget — full sweep
//!
//! Each split test function spawns its own rustc per fixture inside
//! trybuild. cargo's default `--test-threads = num_cpus` would run all
//! 16 functions concurrently, peaking at 16 × ~1 GB rustc RSS — enough
//! to OOM a 16 GB box.
//!
//! For a full sweep, throttle parallelism:
//!     `cargo test --test trybuild_tests -- --test-threads=2`
//!
//! For targeted dev iteration (the common case after editing one
//! fixture), run only that phase's bucket — single-threaded, ~30 s,
//! no OOM risk:
//!     `cargo test --test trybuild_tests compile_pass_phase7_5`

use trybuild::TestCases;

// ── compile_pass — one test per phase prefix ─────────────────────────────

#[test]
fn compile_pass_phase3() {
    TestCases::new().pass("tests/compile_pass/phase3_*.rs");
}

#[test]
fn compile_pass_phase4_5() {
    TestCases::new().pass("tests/compile_pass/phase4_5_*.rs");
}

#[test]
fn compile_pass_phase5() {
    let t = TestCases::new();
    // `phase5_*` covers Tracked, version field, DjogiEnum, JsonbSchema,
    // field index, rationale advisory. Excludes `phase5_5_*` (auth).
    t.pass("tests/compile_pass/phase5_djogi_enum.rs");
    t.pass("tests/compile_pass/phase5_field_index.rs");
    t.pass("tests/compile_pass/phase5_jsonb_schema.rs");
    t.pass("tests/compile_pass/phase5_jsonb_schema_container_rename.rs");
    t.pass("tests/compile_pass/phase5_jsonb_schema_serde_rename.rs");
    t.pass("tests/compile_pass/phase5_rationale_advisory.rs");
    t.pass("tests/compile_pass/phase5_tracked.rs");
    t.pass("tests/compile_pass/phase5_version_field.rs");
}

#[test]
fn compile_pass_phase5_5() {
    TestCases::new().pass("tests/compile_pass/phase5_5_*.rs");
}

#[test]
fn compile_pass_phase6() {
    let t = TestCases::new();
    // `phase6_*` excludes `phase6_5_*` (grouped aggregation polish).
    t.pass("tests/compile_pass/phase6_spatial_field.rs");
    t.pass("tests/compile_pass/phase6_spatial_query.rs");
}

#[test]
fn compile_pass_phase6_5() {
    TestCases::new().pass("tests/compile_pass/phase6_5_*.rs");
}

#[test]
fn compile_pass_phase7() {
    // Note: globs cannot exclude sub-prefixes, so this can't be expressed
    // as `phase7_*.rs`. Each phase7-only fixture (excluding `phase7_zero*`
    // and `phase7_5_*`) is listed explicitly. There is currently just one.
    TestCases::new().pass("tests/compile_pass/phase7_gap2_deferrable_fk.rs");
}

#[test]
fn compile_pass_phase7_zero() {
    let t = TestCases::new();
    // `phase7_zero_*` covers apps subsystem + indexes substrate.
    // Excludes `phase7_zero2_*` (T-series visage queryset surface).
    t.pass("tests/compile_pass/phase7_zero_apps_basic.rs");
    t.pass("tests/compile_pass/phase7_zero_apps_label_override.rs");
    t.pass("tests/compile_pass/phase7_zero_apps_model_linkage.rs");
    t.pass("tests/compile_pass/phase7_zero_apps_renamed_from.rs");
    t.pass("tests/compile_pass/phase7_zero_apps_renamed_from_cross_database.rs");
    t.pass("tests/compile_pass/phase7_zero_apps_same_label_different_db.rs");
    t.pass("tests/compile_pass/phase7_zero_apps_tombstone.rs");
    t.pass("tests/compile_pass/phase7_zero_apps_visibility_variants.rs");
    t.pass("tests/compile_pass/phase7_zero_field_gin_on_valid_types.rs");
    t.pass("tests/compile_pass/phase7_zero_field_unique_simple.rs");
    t.pass("tests/compile_pass/phase7_zero_model_indexes_composite.rs");
    t.pass("tests/compile_pass/phase7_zero_model_indexes_concurrent.rs");
    t.pass("tests/compile_pass/phase7_zero_model_indexes_covering.rs");
    t.pass("tests/compile_pass/phase7_zero_model_indexes_expression.rs");
    t.pass("tests/compile_pass/phase7_zero_model_indexes_method_opclass.rs");
    t.pass("tests/compile_pass/phase7_zero_model_indexes_mixed_forms.rs");
    t.pass("tests/compile_pass/phase7_zero_model_indexes_nulls_not_distinct.rs");
    t.pass("tests/compile_pass/phase7_zero_model_indexes_per_column_record.rs");
    t.pass("tests/compile_pass/phase7_zero_model_indexes_raw_ident_columns.rs");
    t.pass("tests/compile_pass/phase7_zero_model_indexes_unique_composite.rs");
    t.pass("tests/compile_pass/phase7_zero_model_indexes_unique_concurrent.rs");
    t.pass("tests/compile_pass/phase7_zero_model_indexes_unique_partial.rs");
    t.pass("tests/compile_pass/phase7_zero_pk_heerid_desc.rs");
    t.pass("tests/compile_pass/phase7_zero_pk_ranjid_desc.rs");
}

#[test]
fn compile_pass_phase7_zero2() {
    TestCases::new().pass("tests/compile_pass/phase7_zero2_*.rs");
}

#[test]
fn compile_pass_phase7_5() {
    TestCases::new().pass("tests/compile_pass/phase7_5_*.rs");
}

#[test]
fn compile_pass_phase8_zero() {
    TestCases::new().pass("tests/compile_pass/phase8_zero_*.rs");
}

#[test]
fn compile_pass_phase8_t7() {
    // Cluster 8δ T7 — Punnu integration. T7.1 lands the
    // `djogi::cache` re-export surface check (every sassi public
    // symbol the cluster consumes must be reachable through
    // `djogi::cache::*` without an adopter-side sassi dep). T7.2
    // adds the `Cacheable` auto-emission surface checks
    // (`phase8_t7_cacheable_default.rs`,
    // `phase8_t7_cacheable_with_watermark_field.rs`) under the same
    // glob — bare `#[derive(Model)]` produces a Cacheable type
    // usable in `Punnu<T>`, and `#[model(watermark_field = "...")]`
    // overrides the default `updated_at` watermark for the
    // emitted `DeltaSyncCacheable` impl.
    TestCases::new().pass("tests/compile_pass/phase8_t7_*.rs");
}

#[test]
fn compile_pass_phase8() {
    // Globs cannot exclude sub-prefixes (no brace expansion), so this
    // can't be expressed as `phase8_*.rs` — that pattern would also
    // sweep up `phase8_zero_*.rs`. List each phase8-only fixture
    // explicitly. Phase 8α T1.3 lands two (attribute opt-in + zero-
    // overhead absent path); T1.7 adds one more — the canonical
    // adopter shape with a real `impl ModelHooks` body. Phase 8β
    // T3.5 adds two more — the basic proxy declaration and two-
    // proxies-of-the-same-parent coexistence. Phase 8γ T6.11 adds
    // two `Q<T>` algebra fixtures (operator precedence and 8-term
    // composition).
    let t = TestCases::new();
    t.pass("tests/compile_pass/phase8_hooks_attribute.rs");
    t.pass("tests/compile_pass/phase8_hooks_basic.rs");
    t.pass("tests/compile_pass/phase8_no_hooks_attribute.rs");
    t.pass("tests/compile_pass/phase8_proxy_basic.rs");
    t.pass("tests/compile_pass/phase8_proxy_two_proxies_same_parent.rs");
    // Phase 8β T4.6 — computed-field surface fixtures.
    t.pass("tests/compile_pass/phase8_computed_basic.rs");
    // Phase 8β T5.5 — `#[djogi::trait_impl]` fixtures.
    t.pass("tests/compile_pass/phase8_trait_impl_basic.rs");
    t.pass("tests/compile_pass/phase8_q_algebra_xor_precedence.rs");
    t.pass("tests/compile_pass/phase8_q_algebra_eight_term_composition.rs");
}

#[test]
fn compile_pass_unphased() {
    let t = TestCases::new();
    // Pre-phased fixtures — stable and rarely touched.
    t.pass("tests/compile_pass/basic_inject.rs");
    t.pass("tests/compile_pass/fields_accessor.rs");
    t.pass("tests/compile_pass/many_to_many_hand_impl.rs");
    t.pass("tests/compile_pass/many_to_many_macro.rs");
    t.pass("tests/compile_pass/no_default_model.rs");
    t.pass("tests/compile_pass/pk_none_with_user_id.rs");
    t.pass("tests/compile_pass/reverse_one_to_many.rs");
    t.pass("tests/compile_pass/reverse_one_to_one.rs");
    t.pass("tests/compile_pass/through_model.rs");
}

// ── compile_fail — one test per phase prefix ─────────────────────────────

#[test]
fn compile_fail_phase6() {
    TestCases::new().compile_fail("tests/compile_fail/phase6_*.rs");
}

#[test]
fn compile_fail_phase7() {
    // Globs cannot exclude sub-prefixes, so this can't be expressed as
    // `phase7_*.rs`. Each phase7-only fixture (excluding `phase7_zero*`
    // and `phase7_5_*`) is listed explicitly. There is currently just one.
    TestCases::new()
        .compile_fail("tests/compile_fail/phase7_gap2_initially_deferred_requires_deferrable.rs");
}

#[test]
fn compile_fail_phase7_zero() {
    let t = TestCases::new();
    t.compile_fail("tests/compile_fail/phase7_zero_*.rs");
}

#[test]
fn compile_fail_phase7_zero2() {
    TestCases::new().compile_fail("tests/compile_fail/phase7_zero2_*.rs");
}

#[test]
fn compile_fail_phase7_5() {
    TestCases::new().compile_fail("tests/compile_fail/phase7_5_*.rs");
}

#[test]
fn compile_fail_phase8_zero() {
    TestCases::new().compile_fail("tests/compile_fail/phase8_zero_*.rs");
}

#[test]
fn compile_fail_phase8_t7() {
    // Cluster 8δ T7.2 — `#[model(watermark_field = "...")]` rejects
    // values that do not name a field on the post-injection struct.
    // The diagnostic span points at the offending string literal in
    // the attribute, not at the struct body or at the emitted impl
    // — matching the rest of the model-attr error surface.
    TestCases::new().compile_fail("tests/compile_fail/phase8_t7_*.rs");
}

#[test]
fn compile_fail_phase8() {
    // Globs cannot exclude sub-prefixes (no brace expansion), so this
    // can't be expressed as `phase8_*.rs` — that pattern would also
    // sweep up `phase8_zero_*.rs`. List each phase8-only fixture
    // explicitly. Phase 8α T1.7 lands two: a wrong-receiver-mutability
    // override on `before_create` and a `#[model(hooks)]` model that
    // forgot the sibling `impl ModelHooks for M`. Phase 8β T3.5 adds
    // three more: a runtime-bound RHS in a `default_filter` closure
    // (rejected by the T3.3 SQL lowering pass) and two orphan-
    // attribute guards (`default_order` / `default_filter` without
    // `proxy_for`) — the diagnostic span points at the offending
    // key per the T3.3 VERIFY-1 fixup. Phase 8γ T6.10 adds the
    // no-regex-lift fixture that locks `sassi::LookupOp` against
    // a `Regex` / `IRegex` variant, plus two `Q<T>` mismatched-type
    // fixtures.
    let t = TestCases::new();
    t.compile_fail("tests/compile_fail/phase8_hooks_attr_without_impl.rs");
    t.compile_fail("tests/compile_fail/phase8_hooks_invalid_signature.rs");
    t.compile_fail("tests/compile_fail/phase8_lookup_op_regex_lifted_to_basic_predicate.rs");
    t.compile_fail("tests/compile_fail/phase8_proxy_default_filter_runtime_value.rs");
    t.compile_fail("tests/compile_fail/phase8_proxy_orphan_default_filter.rs");
    t.compile_fail("tests/compile_fail/phase8_proxy_orphan_default_order.rs");
    // Phase 8β T4.6 — computed-field rejection paths.
    t.compile_fail("tests/compile_fail/phase8_computed_stored_deferred.rs");
    t.compile_fail("tests/compile_fail/phase8_computed_empty_sql.rs");
    // Phase 8β T5.5 — `#[djogi::trait_impl]` rejection paths.
    t.compile_fail("tests/compile_fail/phase8_trait_impl_inherent.rs");
    t.compile_fail("tests/compile_fail/phase8_trait_impl_generic.rs");
    t.compile_fail("tests/compile_fail/phase8_q_xor_with_mismatched_types.rs");
    t.compile_fail("tests/compile_fail/phase8_q_and_with_mismatched_types.rs");
}

#[test]
fn compile_fail_unphased() {
    let t = TestCases::new();
    // Macro-foundational error cases (model attrs, jsonb-schema validation,
    // expose grammar, etc.) — pre-phased and rarely touched.
    t.compile_fail("tests/compile_fail/bad_field_attr.rs");
    t.compile_fail("tests/compile_fail/bad_on_delete_value.rs");
    t.compile_fail("tests/compile_fail/bad_relation_path.rs");
    t.compile_fail("tests/compile_fail/djogi_enum_empty.rs");
    t.compile_fail("tests/compile_fail/djogi_enum_tuple_variant.rs");
    t.compile_fail("tests/compile_fail/djogi_test_extensions_duplicate_key.rs");
    t.compile_fail("tests/compile_fail/djogi_test_extensions_non_string_element.rs");
    t.compile_fail("tests/compile_fail/djogi_test_extensions_not_array.rs");
    t.compile_fail("tests/compile_fail/djogi_test_sync_models_duplicate_key.rs");
    t.compile_fail("tests/compile_fail/djogi_test_sync_models_non_path_element.rs");
    t.compile_fail("tests/compile_fail/djogi_test_sync_models_scalar_value.rs");
    t.compile_fail("tests/compile_fail/djogi_test_sync_models_string_element.rs");
    t.compile_fail("tests/compile_fail/djogi_test_unknown_arg.rs");
    t.compile_fail("tests/compile_fail/duplicate_no_default.rs");
    t.compile_fail("tests/compile_fail/expose_empty.rs");
    t.compile_fail("tests/compile_fail/expose_mixed_forms_same_scope.rs");
    t.compile_fail("tests/compile_fail/expose_none_combined.rs");
    t.compile_fail("tests/compile_fail/expose_relation_form_on_scalar.rs");
    t.compile_fail("tests/compile_fail/expose_scalar_form_on_relation.rs");
    t.compile_fail("tests/compile_fail/expose_unknown_scope.rs");
    t.compile_fail("tests/compile_fail/field_index_bool_value.rs");
    t.compile_fail("tests/compile_fail/field_index_int_value.rs");
    t.compile_fail("tests/compile_fail/field_name_raw_keyword_escape.rs");
    t.compile_fail("tests/compile_fail/field_name_reserved_keyword.rs");
    t.compile_fail("tests/compile_fail/grouped_queryset_fetch_without_annotate.rs");
    t.compile_fail("tests/compile_fail/having_on_ungrouped.rs");
    t.compile_fail("tests/compile_fail/insecurely_not_on_plain_model.rs");
    t.compile_fail("tests/compile_fail/invalid_through_model.rs");
    t.compile_fail("tests/compile_fail/jsonb_schema_flatten.rs");
    t.compile_fail("tests/compile_fail/jsonb_schema_tuple_struct.rs");
    t.compile_fail("tests/compile_fail/many_to_many_bad_ident.rs");
    t.compile_fail("tests/compile_fail/many_to_many_bad_that_fk_keyword.rs");
    t.compile_fail("tests/compile_fail/many_to_many_collision.rs");
    t.compile_fail("tests/compile_fail/missing_table.rs");
    t.compile_fail("tests/compile_fail/pk_none_has_no_model_impl.rs");
    t.compile_fail("tests/compile_fail/reserved_field_name.rs");
    t.compile_fail("tests/compile_fail/reserved_id_heerid.rs");
    t.compile_fail("tests/compile_fail/reverse_relation_duplicate_accessor.rs");
    t.compile_fail("tests/compile_fail/reverse_relation_wrong_via_column.rs");
    t.compile_fail("tests/compile_fail/sealed_field_ref_new.rs");
    t.compile_fail("tests/compile_fail/sealed_into_distinct_columns.rs");
    t.compile_fail("tests/compile_fail/sealed_model_hand_impl.rs");
    t.compile_fail("tests/compile_fail/sealed_order_expr_fields.rs");
    t.compile_fail("tests/compile_fail/sealed_relation_path_new.rs");
    t.compile_fail("tests/compile_fail/simplify_followup_apps_seal_token_not_public.rs");
    t.compile_fail("tests/compile_fail/simplify_followup_pk_seal_token_not_public.rs");
    t.compile_fail("tests/compile_fail/tuple_struct.rs");
    t.compile_fail("tests/compile_fail/unknown_index_method.rs");
    t.compile_fail("tests/compile_fail/version_field_alias_path.rs");
    t.compile_fail("tests/compile_fail/version_field_duplicate.rs");
    t.compile_fail("tests/compile_fail/version_field_wrong_type.rs");
}
