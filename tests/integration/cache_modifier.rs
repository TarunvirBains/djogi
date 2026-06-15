// Cache modifier integration tests.
//
// What this file pins:
//
// 1. After `.cache(&punnu).fetch_all(&mut ctx).await`, the bound
//  `sassi::Punnu` contains every fetched row — `punnu.len() ==
//  fetched.len()`. The post-fetch hook fires once per row in the
//  materialised `Vec<T>`.
// 2. `.cache(&punnu).first(&mut ctx).await` inserts only the row
//  actually returned to the caller — exactly one entry lands in
//  the pool when `Some(_)` came back, zero on `None`.
// 3. The cache modifier is **purely additive** at the SQL level:
//  a queryset with `.cache(&p)` renders the same SELECT SQL as the
//  same chain without `.cache(&p)`.
//
// # Fixture strategy
//
// Tables are provisioned via `#[djogi_test(sync_models = [CacheRow])]`
// which routes through the same migration engine that production uses.
// The `#[djogi_test]` macro already installs HeeRanjID schema, seeds
// node 1, and sets `heer.node_id = '1'` before the test body runs.
//
// # Why these tests live in `tests/integration/`
//
// Per the workspace convention (every other integration
// test sits here, registered through `djogi/Cargo.toml`'s `[[test]]`
// blocks). The cache modifier surface is reachable through the
// public `djogi` crate API, exactly as adopters consume it.

use djogi::cache::Punnu;
use djogi::prelude::*;

// ---------------------------------------------------------------------------
// Fixture model — a tiny table whose rows we'll feed through the
// cache modifier. `#[derive(Clone)]` is required at the type level
// because `QuerySet::cache(&punnu)` is gated on `T: Cacheable +
// Clone` (see `djogi/src/query/queryset.rs` for the bound
// rationale). Every model that goes through `#[derive(Model)]`
// already carries `#[derive(Clone)]` in the canonical recipe, so the
// bound is satisfied by construction here.
// ---------------------------------------------------------------------------

// Pin the PK strategy explicitly. The macro's "no `pk = ...`" default
// is `PkStrategy::HeerIdDesc` (recency-biased; `attrs.rs` line ~1058),
// not `HeerId` — they share the same Rust `id: HeerId` field shape, so
// the test would compile under either. But a future default flip
// would silently change which Cacheable variant this fixture
// exercises. Anchor on `pk = "heerid"` so the variant is named.
#[model(table = "cache_rows", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct CacheRow {
    pub note: String,
}

// ---------------------------------------------------------------------------
// Test 1 — `.cache(&p).fetch_all()` populates the bound Punnu with
// every fetched row.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [CacheRow])]
async fn cache_modifier_populates_punnu_on_fetch_all(mut ctx: djogi::DjogiContext) {
    // Seed three rows. Row count is the assertion subject.
    for note in ["first", "second", "third"] {
        CacheRow::create(
            &mut ctx,
            CacheRow {
                note: note.into(),
                ..Default::default()
            },
        )
        .await
        .expect("create row should succeed");
    }

    // Construct the Punnu via the builder pattern — sassi's idiomatic
    // empty-pool construction. The default config carries
    // `OnConflict::LastWriteWins` so re-inserts of the same id
    // (which won't happen in this test — every row has a distinct
    // generated id) would not error.
    let pool: Punnu<CacheRow> = Punnu::<CacheRow>::builder().build();
    assert_eq!(pool.len(), 0, "fresh Punnu must start empty",);

    // Run the queryset with the cache binding.
    let rows = CacheRow::objects()
        .cache(&pool)
        .expect("unfiltered queryset must satisfy portable cache gate")
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all should succeed");

    assert_eq!(
        rows.len(),
        3,
        "fetch_all must return every seeded row regardless of cache binding",
    );
    assert_eq!(
        pool.len(),
        rows.len(),
        ".cache(&p).fetch_all(...) must insert each fetched row into the bound Punnu — \
     one Punnu entry per row in the returned Vec<T>",
    );
}

// ---------------------------------------------------------------------------
// Test 2 — `.cache(&p).first()` inserts only the row actually
// returned. The terminal returns `Option<T>`; the cache hook fires on
// the `Some(_)` branch and skips the `None` branch.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [CacheRow])]
async fn cache_modifier_first_inserts_only_returned_row(mut ctx: djogi::DjogiContext) {
    // Seed two rows so `first` has something to pick.
    for note in ["alpha", "beta"] {
        CacheRow::create(
            &mut ctx,
            CacheRow {
                note: note.into(),
                ..Default::default()
            },
        )
        .await
        .expect("create row should succeed");
    }

    let pool: Punnu<CacheRow> = Punnu::<CacheRow>::builder().build();

    let row = CacheRow::objects()
        .cache(&pool)
        .expect("unfiltered queryset must satisfy portable cache gate")
        .first(&mut ctx)
        .await
        .expect("first should succeed");

    assert!(row.is_some(), "first must return Some(_) when rows exist",);
    assert_eq!(
        pool.len(),
        1,
        ".cache(&p).first(...) must insert exactly one row into the bound Punnu — \
     the single row returned to the caller, not every row that matched the filter",
    );
}

// ---------------------------------------------------------------------------
// Test 3 — `.cache(&p)` is purely additive at the SQL-structure level.
//
// NOTE: this test does NOT need a populated table — it never calls
// a terminal. SQL rendering is a pure function of the queryset's
// structural state (filter / order / limit / etc.).
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [CacheRow])]
async fn cache_modifier_does_not_change_sql_emit(mut ctx: djogi::DjogiContext) {
    let pool: Punnu<CacheRow> = Punnu::<CacheRow>::builder().build();

    // Build two structurally identical querysets — same filter, same
    // ordering, same limit — differing only in whether `.cache(&p)`
    // was called. Use a literal filter rather than a closure that
    // captures shared state so the structural shape is provably the
    // same. Use the portable predicate route because PR4 correctly
    // rejects legacy `Q::Condition` filters at the cache boundary.
    let plain = CacheRow::objects()
        .portable_filter(|f| f.note().eq("alpha".to_string()))
        .order_by(|f| f.id().asc())
        .limit(10);
    let cached = CacheRow::objects()
        .portable_filter(|f| f.note().eq("alpha".to_string()))
        .order_by(|f| f.id().asc())
        .limit(10)
        .cache(&pool)
        .expect("typed filter must satisfy portable cache gate");

    let plain_sql = plain
        .render_select_sql_for_testing()
        .expect("plain queryset SQL renders");
    let cached_sql = cached
        .render_select_sql_for_testing()
        .expect("cached queryset SQL renders");
    assert_eq!(
        plain_sql, cached_sql,
        ".cache(&p) must be purely additive — the SELECT SQL must be byte-identical \
     with vs without .cache(...). \
     A diff here means the cache modifier accidentally mutated SQL-shaping state.",
    );

    // Sanity: the rendered SQL is non-trivial and includes the structural
    // clauses this test built above.
    assert!(
        plain_sql.contains("cache_rows")
            && plain_sql.contains("ORDER BY")
            && plain_sql.contains("LIMIT"),
        "testing SQL renderer lost structural clauses; got {plain_sql}",
    );

    // Side-effect-free: neither queryset has been driven through a
    // terminal, so the bound pool stayed empty.
    assert_eq!(
        pool.len(),
        0,
        "constructing a `.cache(&p)` queryset must not insert anything; the hook only \
     fires from a terminal method",
    );

    // Suppress unused-variable warnings — `ctx` is part of the
    // `#[djogi_test]` harness contract.
    let _ = &mut ctx;
}
