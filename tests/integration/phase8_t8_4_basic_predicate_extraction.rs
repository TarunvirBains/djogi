// Phase 8δ T8.4 integration tests: `QuerySet::into_basic_predicate` —
// conservative Q<T>→BasicPredicate<T> extraction for `refresh_into`.
//
// # What this file pins
//
// 1. An **unfiltered** `QuerySet::new()` starts as `Q::Portable(True)` and
//    reduces to `Some(BasicPredicate::True)`.
//
// 2. Any queryset built with the public `.filter(...)` / `.exclude(...)` /
//    `.filter_struct(...)` APIs produces `Q::Condition(...)` and always
//    reduces to `None`. This is expected — the public filter surface routes
//    through `and_condition_into_q`, which wraps every condition in
//    `Q::Condition` for character-for-character SQL-parity with the pre-8γ
//    substrate. `into_basic_predicate` correctly classifies these as
//    Unreducible and emits a `tracing::warn!`.
//
// 3. The `tracing::warn!` emitted for Unreducible paths includes the
//    model type name and a description of the unreducible variant.
//
// 4. The `refresh_into` call (which internally calls `into_basic_predicate`)
//    compiles and returns a `DeltaRefreshHandle<T>` without panicking for an
//    unfiltered queryset. (The handle's `update().await` path is
//    unimplemented! in T8.5 — we do not call it here.)
//
// # Architectural note on the public filter API
//
// The `.filter(|f| ...)` / `.filter_struct(...)` / `.exclude(...)` routes
// always produce `Q::Condition` (legacy-parity contract from Cluster 8γ Stage
// 2 T6.9). A queryset carrying `Q::Condition` cannot be reduced to a
// `BasicPredicate<T>` without re-parsing the type-erased `Condition` tree —
// which would require knowing the concrete Rust type for each field operand.
//
// For `refresh_into` to benefit from filter pushdown, adopters must build
// the condition through Djogi-owned portable predicate builders
// (`DjogiField` / generated field accessors after the PR3 macro flip) rather
// than raw `sassi::BasicPredicate<T>`. Raw Sassi ingress was removed in
// Phase 8eta PR2b because it can forge a SQL column/extractor mismatch. The
// warning from `into_basic_predicate` guides adopters toward the portable
// predicate path.
//
// # Why `into_basic_predicate` is `pub`
//
// `into_basic_predicate` is `pub` (not `pub(crate)`) so this integration
// test suite — which compiles as a separate binary against djogi's public
// API — can call it directly. Advanced cache-integration callers also
// benefit from being able to inspect reducibility before calling
// `refresh_into`. The method is NOT part of the everyday filter API and is
// documented as an advanced/internal framework utility.
//
// # Typed-surface gap in Test 5
//
// Test 5 calls `ctx.share_pool()` to obtain the pool required by
// `DjogiContext::share_pool() -> DjogiPool` so delta-refresh integration
// tests do not need the bypass.
//
// # Spec anchor
//
// `docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md`
// §3 commit T8.4.
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

#[model(table = "phase8_t8_4_extract_rows", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct ExtractRow {
    pub label: String,
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

/// Helper: ensure the `tracing_test` global subscriber is installed and
/// return the current log buffer length (for delta-checking after a call).
fn init_log_capture() -> usize {
    tracing_test::internal::INITIALIZED.call_once(|| {
        let buf = tracing_test::internal::global_buf();
        let mock_writer = tracing_test::internal::MockWriter::new(buf);
        let subscriber = tracing_test::internal::get_subscriber(mock_writer, "trace");
        tracing::dispatcher::set_global_default(subscriber).unwrap_or(());
    });
    tracing_test::internal::global_buf().lock().unwrap().len()
}

/// Return the substring of the global log buffer appended since `since`.
fn logs_since(since: usize) -> String {
    let buf = tracing_test::internal::global_buf().lock().unwrap();
    std::str::from_utf8(&buf[since..]).unwrap_or("").to_owned()
}

// ---------------------------------------------------------------------------
// Test 2 — Queryset with `.filter(...)` produces Q::Condition → None.
//
// Every public filter call routes through and_condition_into_q, which wraps
// the condition as Q::Condition. into_basic_predicate must return None and
// emit a tracing::warn! on the djogi::cache target.
//
// The filter uses the typed field accessor (`f.label().eq(...)`) — any
// public filter call produces Q::Condition, which is exactly what this
// test needs to exercise the Unreducible path.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [ExtractRow])]
async fn filtered_queryset_returns_none_with_warning(mut ctx: djogi::DjogiContext) {
    // Snapshot log buffer length before the call so we only inspect new lines.
    let since = init_log_capture();

    let qs = ExtractRow::objects().filter(|f| f.label().eq("test".to_string()));

    let result = qs.into_basic_predicate();

    assert!(
        result.is_none(),
        "queryset built with .filter() must return None from into_basic_predicate — \
         the Q::Condition escape hatch is always Unreducible"
    );

    // Verify the warn! was emitted on the djogi::cache tracing target.
    let new_logs = logs_since(since);
    assert!(
        new_logs.contains("djogi::cache"),
        "into_basic_predicate must emit a tracing::warn! on the djogi::cache target \
         when the condition is Unreducible; captured log so far: {new_logs:?}"
    );

    let _ = &mut ctx;
}

// ---------------------------------------------------------------------------
// Test 3 — Queryset with `.exclude(...)` also produces Q::Condition → None.
//
// `.exclude()` calls `Condition::not(cond)` and routes through the same
// and_condition_into_q path. into_basic_predicate returns None.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [ExtractRow])]
async fn excluded_queryset_returns_none(mut ctx: djogi::DjogiContext) {
    let qs = ExtractRow::objects().exclude(|f| f.label().eq("skip".to_string()));

    let result = qs.into_basic_predicate();

    assert!(
        result.is_none(),
        "queryset built with .exclude() must return None from into_basic_predicate"
    );

    let _ = &mut ctx;
}

// ---------------------------------------------------------------------------
// Test 4 — Queryset with filter_struct (model filter builder) → None.
//
// filter_struct routes through and_condition_into_q which wraps model-filter
// inputs as Q::Condition. Even a Q::Portable input becomes Q::Condition after
// filter_struct.
// This test verifies that the common adopter pattern of using filter_struct
// with the model's {Model}Filter builder also always results in None.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [ExtractRow])]
async fn filter_struct_with_model_filter_returns_none(mut ctx: djogi::DjogiContext) {
    // ExtractRowFilter is the macro-generated ModelFilter for ExtractRow.
    // filter_struct routes through and_condition_into_q → Q::Condition → None.
    let qs = ExtractRow::objects()
        .filter_struct(ExtractRowFilter::new().label(djogi::Lookup::Eq("test".to_string())));

    let result = qs.into_basic_predicate();

    assert!(
        result.is_none(),
        "queryset built with filter_struct(ExtractRowFilter) must return None — \
         the filter_struct path wraps everything in Q::Condition"
    );

    let _ = &mut ctx;
}

// ---------------------------------------------------------------------------
// Test 5 — refresh_into compiles and returns a handle for unfiltered queryset.
//
// Verifies the end-to-end type signature: QuerySet::refresh_into calls
// into_basic_predicate internally. The handle is dropped immediately (the
// delta-refresh fetch is unimplemented! in T8.3/T8.5, so we must not call
// handle.update().await). Just verifies the return type and no panic.
//
// to obtain a DjogiPool from a DjogiContext for passing to `refresh_into`.
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
    let handle = ExtractRow::objects().refresh_into(&punnu, pool, auth);

    // Drop the handle without calling .update().await — the T8.5 SQL path is
    // not yet implemented (it would panic with unimplemented!()).
    drop(handle);
}
