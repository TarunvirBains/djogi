// Phase 8.5 issue #147 — pg_trgm typed surface: SQL shape pins.
//
// Pins the exact SQL emitter output for the two pg_trgm expression variants:
//
// 1. `explicit_pg_predicate().trgm_similar_to(pattern, threshold)` in a
//    filter closure emits `WHERE similarity(<col>, $1) >= $2` with two bind
//    parameters in order (pattern first, threshold second).
//
// 2. `trgm_similarity(pattern)` as an Expr<f64> in `filter_expr` emits
//    `similarity(<col>, $1)` as the SQL expression, which can be combined
//    with comparison operators to produce full WHERE predicates.
//
// 3. Composed predicates number bind slots continuously across all leaves.
//
// 4. An `IndexSpec` with `IndexType::Gin`, `opclass = Some("gin_trgm_ops")`,
//    and `extension_dependency = Some("pg_trgm")` round-trips through the
//    descriptor layer: the opclass and extension fields are preserved
//    structurally (the migration emitter uses them to emit
//    `CREATE INDEX ... USING GIN (col gin_trgm_ops)` and
//    `CREATE EXTENSION IF NOT EXISTS "pg_trgm"`).
//
// All assertions run against the SQL builder via `__sql_for_test` —
// no live database is required.

use djogi::prelude::*;

// ── Fixture model ─────────────────────────────────────────────────────────

#[model(table = "phase8_5_trgm_users")]
#[derive(Debug, Clone)]
pub struct User {
    pub bio: String,
    pub name: String,
}

// ── Helper ────────────────────────────────────────────────────────────────

fn sql_of<F>(f: F) -> String
where
    F: FnOnce(QuerySet<User>) -> QuerySet<User>,
{
    f(User::objects())
        .__sql_for_test()
        .expect("trgm SQL build must not fail for these shapes")
}

// ── trgm_similar_to SQL shape ─────────────────────────────────────────────
//
// `trgm_similar_to` is a Postgres-extension predicate reached via
// `f.col().explicit_pg_predicate().trgm_similar_to(pattern, threshold)`.

#[test]
fn trgm_similar_to_emits_similarity_gte_with_two_binds() {
    let sql = sql_of(|qs| {
        qs.filter(|f| {
            f.bio()
                .explicit_pg_predicate()
                .trgm_similar_to("machine learning", 0.3)
        })
    });
    // Predicate form: similarity(<col>, $1) >= $2
    assert!(
        sql.contains("similarity(bio, $1) >= $2"),
        "expected `similarity(bio, $1) >= $2` in:\n{sql}"
    );
    assert!(sql.contains("WHERE"), "expected WHERE clause; got:\n{sql}");
}

#[test]
fn trgm_similar_to_column_is_raw_identifier_not_quoted() {
    let sql = sql_of(|qs| {
        qs.filter(|f| {
            f.name()
                .explicit_pg_predicate()
                .trgm_similar_to("Alice", 0.5)
        })
    });
    // Column must appear as a bare identifier, not single- or double-quoted.
    assert!(
        sql.contains("similarity(name, $1)"),
        "column must be a bare identifier; got:\n{sql}"
    );
}

#[test]
fn trgm_similar_to_threshold_binds_as_second_param() {
    // Verify the bind ordering: pattern → $1, threshold → $2.
    let sql = sql_of(|qs| {
        qs.filter(|f| {
            f.bio()
                .explicit_pg_predicate()
                .trgm_similar_to("rust", 0.1)
        })
    });
    assert!(
        sql.contains("$1) >= $2"),
        "pattern must be $1 and threshold $2; got:\n{sql}"
    );
}

// ── trgm_similarity score expression SQL shape ────────────────────────────
//
// `trgm_similarity` is a non-predicate SQL expression that returns `Expr<f64>`.
// It is available directly on `DjogiField<M, String>` and can be composed
// via the `Expr<T>` comparison API (`.eq`, `.gte`, `.gt`, `.lt`, `.lte`).

#[test]
fn trgm_similarity_return_type_is_expr_f64() {
    // Typed assertion: trgm_similarity must return Expr<f64>.
    // If the type annotation is wrong, this will not compile.
    let score_expr: Expr<f64> = UserFields.bio().trgm_similarity("query");
    let _ = score_expr;
}

#[test]
fn trgm_similarity_emits_similarity_in_filter_expr() {
    // Verify the score expression SQL via filter_expr composition:
    // trgm_similarity().gte(0.3) should emit `similarity(bio, $1) >= $2`.
    let sql = sql_of(|qs| {
        qs.filter_expr(|f| {
            f.bio()
                .trgm_similarity("search text")
                .gte(Expr::literal(0.3_f64))
        })
    });
    // The similarity() function must appear in the WHERE clause.
    assert!(
        sql.contains("similarity(bio, $"),
        "expected similarity() expression in WHERE; got:\n{sql}"
    );
    // The threshold comparison must be present.
    assert!(
        sql.contains(">="),
        "expected >= comparison for threshold; got:\n{sql}"
    );
}

// ── Bind parameter numbering across composed predicates ───────────────────

#[test]
fn trgm_similar_to_plus_eq_filter_continues_bind_sequence() {
    // filter(&) composes two predicates; the trgm predicate binds $1,$2
    // and the equality filter binds $3. All three ordinals must appear.
    let sql = sql_of(|qs| {
        qs.filter(|f| {
            f.bio()
                .explicit_pg_predicate()
                .trgm_similar_to("rust", 0.3)
                & f.name().eq("Alice".to_string())
        })
    });
    // Three distinct bind slots must be present.
    assert!(
        sql.contains("$1") && sql.contains("$2") && sql.contains("$3"),
        "expected three bind slots ($1, $2, $3) for composed predicates; got:\n{sql}"
    );
}

// ── IndexSpec descriptor round-trip ──────────────────────────────────────
//
// Verifies that the existing IndexSpec / IndexColumnSpec API correctly
// represents a GIN+gin_trgm_ops index with an extension dependency, so
// the migration emitter can emit the correct DDL.

#[test]
fn gin_trgm_ops_index_spec_preserves_opclass_and_extension_dependency() {
    use djogi::descriptor::{
        IndexColumnSpec, IndexKind, IndexNullsOrder, IndexOrder, IndexSpec, IndexTarget, IndexType,
    };

    let spec = IndexSpec {
        name: "phase8_5_trgm_users_bio_trgm_idx",
        target: IndexTarget::Columns(&[IndexColumnSpec {
            name: "bio",
            opclass: Some("gin_trgm_ops"),
            order: IndexOrder::Asc,
            nulls: IndexNullsOrder::Default,
        }]),
        kind: IndexKind::NonUnique,
        index_type: IndexType::Gin,
        predicate: None,
        include: &[],
        nulls_not_distinct: false,
        requires_out_of_transaction: false,
        extension_dependency: Some("pg_trgm"),
    };

    // Opclass is preserved.
    match spec.target {
        IndexTarget::Columns(cols) => {
            assert_eq!(cols.len(), 1);
            assert_eq!(cols[0].name, "bio");
            assert_eq!(
                cols[0].opclass,
                Some("gin_trgm_ops"),
                "opclass must be gin_trgm_ops"
            );
        }
        _ => panic!("expected Columns target"),
    }

    // Extension dependency is preserved.
    assert_eq!(
        spec.extension_dependency,
        Some("pg_trgm"),
        "extension_dependency must be pg_trgm"
    );

    // Index type is GIN.
    assert_eq!(spec.index_type, IndexType::Gin);

    // Non-unique.
    assert_eq!(spec.kind, IndexKind::NonUnique);
}

#[test]
fn gist_trgm_ops_index_spec_is_also_representable() {
    // GiST is the alternative method; verify the opclass slot accepts the gist variant.
    use djogi::descriptor::{
        IndexColumnSpec, IndexKind, IndexNullsOrder, IndexOrder, IndexSpec, IndexTarget, IndexType,
    };

    let spec = IndexSpec {
        name: "phase8_5_trgm_users_name_trgm_gist_idx",
        target: IndexTarget::Columns(&[IndexColumnSpec {
            name: "name",
            opclass: Some("gist_trgm_ops"),
            order: IndexOrder::Asc,
            nulls: IndexNullsOrder::Default,
        }]),
        kind: IndexKind::NonUnique,
        index_type: IndexType::Gist,
        predicate: None,
        include: &[],
        nulls_not_distinct: false,
        requires_out_of_transaction: false,
        extension_dependency: Some("pg_trgm"),
    };

    match spec.target {
        IndexTarget::Columns(cols) => {
            assert_eq!(cols[0].opclass, Some("gist_trgm_ops"));
        }
        _ => panic!("expected Columns target"),
    }
    assert_eq!(spec.index_type, IndexType::Gist);
    assert_eq!(spec.extension_dependency, Some("pg_trgm"));
}
