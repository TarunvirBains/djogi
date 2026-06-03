// .4 integration tests: `QuerySet::into_basic_predicate` —
// conservative Q<T>→BasicPredicate<T> extraction for `refresh_into`.
//
// # What this file pins
//
// 1. An **unfiltered** `QuerySet::new()` starts as `Q::Portable(True)` and
//    reduces to `Some(BasicPredicate::True)`.
//
// 2. Querysets built with ordinary portable field closures (`.filter(...)` /
//    `.exclude(...)`) reduce to `BasicPredicate<T>` after the PR3 field
//    accessor flip. SQL-only / legacy `Q::Condition` paths still reduce to
//    `None`.
//
// 3. The `tracing::warn!` emitted for an unreducible legacy path includes the
//    model type name and a description of the unreducible variant.
//
// 4. The `refresh_into` call compiles and returns a `DeltaRefreshHandle<T>`
//    behind the PR4 portable gate for an unfiltered queryset.
//
// # Architectural note on the public filter API
//
// The ordinary `.filter(|f| f.field().eq(...))` path now produces Djogi-owned
// portable predicates and can be used with cache / refresh. Legacy
// `Q::Condition` paths remain unreducible because they would require
// re-parsing type-erased SQL payloads. Raw Sassi ingress was removed
// because it can forge a SQL column/extractor mismatch.
//
// # Why `into_basic_predicate` is `pub`
//
// `into_basic_predicate` is `pub` (not `pub(crate)`) so this integration
// test suite — which compiles as a separate binary against djogi's public
// API — can call it directly. Advanced cache-integration callers also
// benefit from being able to inspect reducibility before reaching for
// `try_portable` / `cache` / `refresh_into`. The method is NOT part of
// the everyday filter API and is documented as an advanced/internal
// framework utility.
//
// # Spec anchor
//
// §3 commit .4.
//
// # Fixture strategy
//
// Tables are provisioned via `#[djogi_test(sync_models = [ExtractRow])]`
// which routes through the same migration engine that production uses.
// The `#[djogi_test]` macro installs HeeRanjID schema, seeds node 1, and sets
// `heer.node_id = '1'` before the test body runs.

use djogi::prelude::*;

// ---------------------------------------------------------------------------
// Fixture model — a tiny table used for into_basic_predicate extraction tests.
// `#[derive(Clone)]` is required for Cacheable + Punnu<T>.
// ---------------------------------------------------------------------------

#[model(table = "extract_rows", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct ExtractRow {
    pub label: String,
    pub active: bool,
}

#[model(table = "filter_bridge_wrap_rows", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct FilterBridgeWrapRow {
    pub tracked_active: Tracked<bool>,
    pub tracked_label: Tracked<String>,
    pub maybe_active: Option<bool>,
    pub maybe_label: Option<String>,
}

// ---------------------------------------------------------------------------
// Test 1 — Unfiltered queryset reduces to Some(BasicPredicate::True).
//
// `QuerySet::new()` sets condition = Q::Portable(PortablePredicate::True).
// `into_basic_predicate` must return Some(BasicPredicate::True) for this
// starting state.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [ExtractRow])]
async fn extracts_unfiltered_queryset_as_true(mut ctx: djogi::DjogiContext) {
    let qs = ExtractRow::objects(); // QuerySet::new() — no filters.
    let result = qs.into_basic_predicate();

    assert!(
        matches!(result, Some(djogi::BasicPredicate::True)),
        "unfiltered QuerySet must reduce to Some(BasicPredicate::True) — \
         the starting condition Q::Portable(True) is the identity and should \
         always be extractable without a warning"
    );

    let _ = &mut ctx;
}

// ---------------------------------------------------------------------------
// Test 2 — Queryset with a portable `.filter(...)` reduces.
//
// The PR3 field accessor flip makes `f.label().eq(...)` return a Djogi-owned
// portable predicate. `into_basic_predicate` must preserve that shape so PR4
// cache / refresh gates can accept ordinary portable filters.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [ExtractRow])]
async fn filtered_queryset_extracts_portable_predicate(mut ctx: djogi::DjogiContext) {
    let qs = ExtractRow::objects().filter(|f| f.label().eq("test".to_string()));

    let result = qs.into_basic_predicate();

    assert!(
        matches!(result, Some(djogi::BasicPredicate::Field(_))),
        "queryset built with portable .filter() must reduce to a field BasicPredicate"
    );

    let _ = &mut ctx;
}

// ---------------------------------------------------------------------------
// Test 3 — Queryset with portable `.exclude(...)` reduces to `Not`.
//
// `.exclude()` pushes negation into the portable predicate when the closure
// returns a portable predicate.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [ExtractRow])]
async fn excluded_queryset_extracts_portable_negation(mut ctx: djogi::DjogiContext) {
    let qs = ExtractRow::objects().exclude(|f| f.label().eq("skip".to_string()));

    let result = qs.into_basic_predicate();

    assert!(
        matches!(result, Some(djogi::BasicPredicate::Not(_))),
        "queryset built with portable .exclude() must reduce to BasicPredicate::Not"
    );

    let _ = &mut ctx;
}

// ---------------------------------------------------------------------------
// Test 4 — Queryset with filter_struct over a portable bool field reduces.
//
// The macro-generated `{Model}Filter` builder stores erased clauses, but
// bool equality is safe to reconstruct lazily into a portable predicate at
// consumption time.
// ---------------------------------------------------------------------------

#[test]
fn filter_struct_with_portable_bool_model_filter_extracts() {
    let qs = ExtractRow::objects()
        .filter_struct(ExtractRowFilter::new().active(djogi::Lookup::Eq(true)));

    let result = qs.into_basic_predicate();

    assert!(
        matches!(result, Some(djogi::BasicPredicate::Field(_))),
        "queryset built with filter_struct(ExtractRowFilter::active(true)) must reduce to a field BasicPredicate"
    );
}

#[test]
fn empty_model_filter_extracts_as_true() {
    let qs = ExtractRow::objects().filter_struct(ExtractRowFilter::new());

    let result = qs.into_basic_predicate();

    assert!(
        matches!(result, Some(djogi::BasicPredicate::True)),
        "empty generated filters must fold to portable true"
    );
}

#[test]
fn mixed_model_filter_portable_and_fallback_remains_unreducible() {
    let qs = ExtractRow::objects().filter_struct(
        ExtractRowFilter::new()
            .active(djogi::Lookup::Eq(true))
            .label(djogi::Lookup::Contains("test".to_string())),
    );

    let result = qs.into_basic_predicate();

    assert!(
        result.is_none(),
        "mixed generated portable and fallback clauses must be rejected as a whole"
    );
}

#[test]
fn filter_struct_with_portable_bool_in_not_in_extracts() {
    let in_qs = ExtractRow::objects()
        .filter_struct(ExtractRowFilter::new().active(djogi::Lookup::In(vec![true, false])));
    let not_in_qs = ExtractRow::objects()
        .filter_struct(ExtractRowFilter::new().active(djogi::Lookup::NotIn(vec![true])));

    let in_result = in_qs.into_basic_predicate();
    let not_in_result = not_in_qs.into_basic_predicate();

    assert!(
        matches!(in_result, Some(djogi::BasicPredicate::Field(_))),
        "bool In lookups from generated filters must reconstruct as portable field predicates"
    );
    assert!(
        matches!(not_in_result, Some(djogi::BasicPredicate::Field(_))),
        "bool NotIn lookups from generated filters must reconstruct as portable field predicates"
    );
}

#[test]
fn filter_struct_with_portable_string_in_not_in_extracts() {
    let in_qs = ExtractRow::objects().filter_struct(ExtractRowFilter::new().label(
        djogi::Lookup::In(vec!["alpha".to_string(), "beta".to_string()]),
    ));
    let not_in_qs = ExtractRow::objects().filter_struct(
        ExtractRowFilter::new().label(djogi::Lookup::NotIn(vec!["draft".to_string()])),
    );

    let in_result = in_qs.into_basic_predicate();
    let not_in_result = not_in_qs.into_basic_predicate();

    assert!(
        matches!(in_result, Some(djogi::BasicPredicate::Field(_))),
        "string In lookups from generated filters must reconstruct as portable field predicates"
    );
    assert!(
        matches!(not_in_result, Some(djogi::BasicPredicate::Field(_))),
        "string NotIn lookups from generated filters must reconstruct as portable field predicates"
    );
}

#[test]
fn bool_field_with_mismatched_lookup_shape_falls_back_to_non_portable_q() {
    let qs =
        ExtractRow::objects().filter_struct(
            ExtractRowFilter::new().active(djogi::Lookup::<bool>::Contains("true".to_string())),
        );

    let result = qs.into_basic_predicate();

    assert!(
        result.is_none(),
        "op/value shapes outside the conservative bool/string Eq/Neq/In/NotIn map must fallback to non-portable Q"
    );
}

#[test]
fn filter_struct_bridge_compiles_with_tracked_and_option_bool_string_fields() {
    let qs = FilterBridgeWrapRow::objects().filter_struct(FilterBridgeWrapRowFilter::new());

    let result = qs.into_basic_predicate();

    assert!(
        matches!(result, Some(djogi::BasicPredicate::True)),
        "empty generated filters must compile and fold to portable true even when \
         the model has tracked and optional bool/string fields"
    );
}

// ---------------------------------------------------------------------------
// Test 5 — refresh_into compiles and returns a handle for unfiltered queryset.
//
// Verifies the end-to-end type signature. The handle is dropped immediately;
// runtime SQL behavior is covered by the later .5 / PR4 integration tests.
// `ctx.share_pool()` produces the `DjogiPool` value `refresh_into` consumes.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [ExtractRow])]
async fn refresh_into_returns_handle_for_unfiltered_queryset(mut ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");

    let punnu = ctx
        .punnu::<ExtractRow>()
        .expect("punnu registered for ExtractRow model");

    // Build a minimal AuthContext. refresh_into needs one by value.
    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    // This must not panic. into_basic_predicate returns Some(True) for the
    // unfiltered queryset, so no warning is emitted and the handle is created.
    let handle = ExtractRow::objects()
        .refresh_into(&punnu, pool, auth)
        .expect("unfiltered queryset must satisfy portable refresh gate");

    drop(handle);
}
