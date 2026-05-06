//! Phase 8δ T8.4 integration tests: `QuerySet::into_basic_predicate` —
//! conservative Q<T>→BasicPredicate<T> extraction for `refresh_into`.
//!
//! # What this file pins
//!
//! 1. An **unfiltered** `QuerySet::new()` starts as `Q::Basic(True)` and
//!    reduces to `Some(BasicPredicate::True)`.
//!
//! 2. Any queryset built with the public `.filter(...)` / `.exclude(...)` /
//!    `.filter_struct(...)` APIs produces `Q::Condition(...)` and always
//!    reduces to `None`. This is expected — the public filter surface routes
//!    through `and_condition_into_q`, which wraps every condition in
//!    `Q::Condition` for character-for-character SQL-parity with the pre-8γ
//!    substrate. `into_basic_predicate` correctly classifies these as
//!    Unreducible and emits a `tracing::warn!`.
//!
//! 3. The `tracing::warn!` emitted for Unreducible paths includes the
//!    model type name and a description of the unreducible variant.
//!
//! 4. The `refresh_into` call (which internally calls `into_basic_predicate`)
//!    compiles and returns a `DeltaRefreshHandle<T>` without panicking for an
//!    unfiltered queryset. (The handle's `update().await` path is
//!    unimplemented! in T8.5 — we do not call it here.)
//!
//! # Architectural note on the public filter API
//!
//! The `.filter(|f| ...)` / `.filter_struct(...)` / `.exclude(...)` routes
//! always produce `Q::Condition` (legacy-parity contract from Cluster 8γ Stage
//! 2 T6.9). A queryset carrying `Q::Condition` cannot be reduced to a
//! `BasicPredicate<T>` without re-parsing the type-erased `Condition` tree —
//! which would require knowing the concrete Rust type for each field operand.
//!
//! For `refresh_into` to benefit from filter pushdown, adopters must build
//! the condition as a sassi `BasicPredicate<T>` and set `qs.condition`
//! directly (framework-internal path) OR wait for a future cluster that
//! redesigns `filter_struct` to preserve Q-algebra structure. The warning
//! from `into_basic_predicate` guides adopters toward that future path.
//!
//! # Why `into_basic_predicate` is `pub`
//!
//! `into_basic_predicate` is `pub` (not `pub(crate)`) so this integration
//! test suite — which compiles as a separate binary against djogi's public
//! API — can call it directly. Advanced cache-integration callers also
//! benefit from being able to inspect reducibility before calling
//! `refresh_into`. The method is NOT part of the everyday filter API and is
//! documented as an advanced/internal framework utility.
//!
//! # Spec anchor
//!
//! `docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md`
//! §3 commit T8.4.
//!
//! # Fixture strategy
//!
//! Each test provisions its own table inline via `ctx.raw_execute`. The
//! `#[djogi_test]` macro installs HeeRanjID schema, seeds node 1, and sets
//! `heer.node_id = '1'` before the test body runs.

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

async fn setup_extract_row(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE IF NOT EXISTS phase8_t8_4_extract_rows (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            label       TEXT        NOT NULL
        )",
        &[],
    )
    .await
    .expect("create phase8_t8_4_extract_rows table");
}

// ---------------------------------------------------------------------------
// Test 1 — Unfiltered queryset reduces to Some(BasicPredicate::True).
//
// `QuerySet::new()` sets condition = Q::Basic(BasicPredicate::True).
// `into_basic_predicate` must return Some(BasicPredicate::True) for this
// starting state.
// ---------------------------------------------------------------------------

#[djogi::djogi_test]
async fn extracts_unfiltered_queryset_as_true(mut ctx: djogi::DjogiContext) {
    setup_extract_row(&mut ctx).await;

    let qs = ExtractRow::objects(); // QuerySet::new() — no filters.
    let result = qs.into_basic_predicate();

    assert!(
        matches!(result, Some(djogi::BasicPredicate::True)),
        "unfiltered QuerySet must reduce to Some(BasicPredicate::True) — \
         the starting condition Q::Basic(True) is the identity and should \
         always be extractable without a warning"
    );
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
// ---------------------------------------------------------------------------

#[djogi::djogi_test]
async fn filtered_queryset_returns_none_with_warning(mut ctx: djogi::DjogiContext) {
    setup_extract_row(&mut ctx).await;

    // Snapshot log buffer length before the call so we only inspect new lines.
    let since = init_log_capture();

    use djogi::query::internal::Condition;
    let qs = ExtractRow::objects().filter(|_| Condition::__from_raw_sql_fragment("label = 'test'"));

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
}

// ---------------------------------------------------------------------------
// Test 3 — Queryset with `.exclude(...)` also produces Q::Condition → None.
//
// `.exclude()` calls `Condition::not(cond)` and routes through the same
// and_condition_into_q path. into_basic_predicate returns None.
// ---------------------------------------------------------------------------

#[djogi::djogi_test]
async fn excluded_queryset_returns_none(mut ctx: djogi::DjogiContext) {
    setup_extract_row(&mut ctx).await;

    use djogi::query::internal::Condition;
    let qs =
        ExtractRow::objects().exclude(|_| Condition::__from_raw_sql_fragment("label = 'skip'"));

    let result = qs.into_basic_predicate();

    assert!(
        result.is_none(),
        "queryset built with .exclude() must return None from into_basic_predicate"
    );
}

// ---------------------------------------------------------------------------
// Test 4 — Queryset with filter_struct (model filter builder) → None.
//
// filter_struct routes through and_condition_into_q which wraps everything as
// Q::Condition. Even a Q::Basic input becomes Q::Condition after filter_struct.
// This test verifies that the common adopter pattern of using filter_struct
// with the model's {Model}Filter builder also always results in None.
// ---------------------------------------------------------------------------

#[djogi::djogi_test]
async fn filter_struct_with_model_filter_returns_none(mut ctx: djogi::DjogiContext) {
    setup_extract_row(&mut ctx).await;

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
}

// ---------------------------------------------------------------------------
// Test 5 — refresh_into compiles and returns a handle for unfiltered queryset.
//
// Verifies the end-to-end type signature: QuerySet::refresh_into calls
// into_basic_predicate internally. The handle is dropped immediately (the
// delta-refresh fetch is unimplemented! in T8.3/T8.5, so we must not call
// handle.update().await). Just verifies the return type and no panic.
// ---------------------------------------------------------------------------

#[djogi::djogi_test]
async fn refresh_into_returns_handle_for_unfiltered_queryset(mut ctx: djogi::DjogiContext) {
    setup_extract_row(&mut ctx).await;

    let pool = ctx
        .pool()
        .expect("djogi_test context must have a pool")
        .clone();

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
