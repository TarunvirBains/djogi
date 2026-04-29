//! `select_related` — single-hop `LEFT JOIN` emission + `JoinedRow<T>`
//! stitching.
//!
//! # What
//!
//! Consumed by
//! [`QuerySet::fetch_all_joined`](crate::query::QuerySet::fetch_all_joined).
//! For each registered path on the queryset, the select_related emitter:
//!
//! 1. Appends a `LEFT JOIN {target_table} rel_{source_column}
//!    ON {parent_table}.{source_column} = rel_{source_column}.id` clause
//!    to the main query.
//! 2. Extends the `SELECT` list with one entry per child column,
//!    aliased as `rel_{source_column}.{col} AS "rel_{source_column}.{col}"`
//!    so the result set carries both sides with no column-name
//!    collisions.
//!
//! After the query runs, the terminal walks each row and decodes the
//! parent plus every registered child from the aliased columns,
//! packaging them into `JoinedRow<T>`.
//!
//! # Why aliased child columns (not `SELECT t.*`)
//!
//! A prefix-aware decoder looks columns up by the exact alias name — `try_get("id")`
//! finds the first column named `id`, which would be ambiguous the
//! moment both parent and child tables contribute an `id`. Aliasing
//! the child columns under `"rel_{source_column}.{col}"` — with a
//! literal dot embedded in the alias — gives every column a unique
//! name in the result set and lets the
//! [`FromJoinedPgRow`](crate::pg::decode::FromJoinedPgRow)
//! decoder use the `"{prefix}{column}"` shape without collision.
//!
//! The quoted-dot alias matches the table-qualified form Postgres would
//! emit if you projected `SELECT t.*` with a named prefix, so the
//! aliases read naturally in raw SQL logs: a reviewer scanning
//! pg_stat_statements sees `"rel_owner_id.id"` and recognises the
//! origin immediately.
//!
//! # Why LEFT JOIN (always)
//!
//! Even for NOT-NULL FKs, the select_related emitter uses `LEFT JOIN`
//! so an orphan FK (the referenced row has been deleted since insert,
//! bypassing the `RESTRICT` / `CASCADE` policy via raw SQL) produces
//! a `None` on the child side rather than omitting the parent row.
//! Dropping the parent row from the result set would silently break
//! the caller's mental model that `select_related` is a
//! performance-only eager-load — the set of matching parent rows must
//! match what `fetch_all` would have returned on the same queryset.
//!
//! `INNER JOIN` is a valid future optimisation for confirmed-non-null
//! columns, but that opt-in lives behind a `.select_related_strict(...)`
//! surface (Phase 4) rather than silently changing the default's
//! semantics.
//!
//! # T2 scope
//!
//! - Single-hop only — no chained `select_related(path_a.path_b)`. Multi-hop
//!   decode lands in T4.
//! - Multi-relation-per-queryset **is** supported (multiple
//!   `.select_related(...)` calls accumulate into a `Vec<ErasedSelectRelated>`,
//!   each producing its own aliased `LEFT JOIN`).
//! - No join-time filtering — filters still target the parent table.
//! - `.select_related(...)` + `.prefetch(...)` can coexist on the same
//!   queryset; the terminal honours both.

use crate::DjogiError;
use crate::context::ContextInner;
use crate::model::Model;
use crate::pg::accumulator::SqlAccumulator;
use crate::pg::decode::FromJoinedPgRow;
use crate::relation::joined_row::JoinedRow;
use std::any::Any;
use std::collections::HashMap;
use tokio_postgres::Row as PgRow;

/// Decoder function type for a single child column in the select_related
/// path. One monomorphised `fn` per `(Parent, Child)` pair — same erasure
/// strategy Task 4's prefetch loader uses.
///
/// Returns `Some(Box<Child>)` when the child row materialised, or
/// `None` on a LEFT JOIN miss. The probe for miss-vs-hit uses
/// `tokio_postgres::Row::try_get::<i64>(id_alias)` on the child's `id`
/// alias — a NULL result indicates a LEFT JOIN miss.
pub(crate) type JoinDecoderFn =
    for<'r> fn(
        row: &'r PgRow,
        prefix: &str,
    ) -> Result<Option<Box<dyn Any + Send + Sync>>, DjogiError>;

/// Accessor type for the child's `ModelDescriptor`. Monomorphised per
/// `Child` and stored as a plain `fn` pointer on
/// [`ErasedSelectRelated`] so the emitter can read the child's
/// declared column list without naming the concrete type.
///
/// Delegates to `<Child as Model>::descriptor()` via [`child_descriptor`].
pub(crate) type ChildDescriptorFn = fn() -> &'static crate::descriptor::ModelDescriptor;

/// A single registered select_related path on a
/// [`QuerySet<T>`](crate::query::QuerySet).
///
/// Built by
/// [`QuerySet::select_related`](crate::query::QuerySet::select_related)
/// from a typed [`RelationPath<Source, Child>`]. The emitter carries
/// only the dynamic parameters needed to build the join SQL + alias
/// the child columns; the generic `Child` type is captured via the
/// monomorphised [`join_decoder`] fn pointer.
#[derive(Clone)]
pub(crate) struct ErasedSelectRelated {
    /// Source column on the parent table (e.g. `"owner_id"`). Doubles
    /// as the prefix stem — the child alias is `"rel_{source_column}"`
    /// and the column alias is `"rel_{source_column}.{col}"`. The
    /// `source_column` is also the key under which the resolved child
    /// lands in
    /// [`JoinedRow::relations`](crate::relation::JoinedRow), matching
    /// the prefetch wrapper's keying convention.
    pub source_column: &'static str,
    /// Child table name (e.g. `"owners_p3"`). Used in the `LEFT JOIN`
    /// clause.
    pub child_table: &'static str,
    /// Monomorphised decoder that knows how to read one aliased row
    /// into a `Box<dyn Any>` of the concrete child type.
    pub decoder: JoinDecoderFn,
    /// Monomorphised accessor for the child's `ModelDescriptor`. The
    /// SELECT emitter reads `fields[].name` from the descriptor to
    /// alias every child column under the `"rel_{source_column}.{col}"`
    /// shape. Going through a fn pointer (rather than storing a
    /// pre-projected static slice) keeps `ErasedSelectRelated`
    /// construction free of `Vec<&'static str>` allocations while
    /// still giving the emitter access to the full descriptor — a
    /// later phase that needs more than just column names (e.g.
    /// per-column codec selection) can lean on this hook without
    /// reshaping the struct.
    pub child_descriptor: ChildDescriptorFn,
}

/// Monomorphised accessor for `<Child as Model>::descriptor()`. Stored
/// as a plain `fn` pointer on [`ErasedSelectRelated`] so the emitter
/// can read the child's declared column list without naming the
/// concrete type.
pub(crate) fn child_descriptor<Child: Model>() -> &'static crate::descriptor::ModelDescriptor {
    <Child as Model>::descriptor()
}

impl std::fmt::Debug for ErasedSelectRelated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `decoder` and `child_descriptor` are fn-pointers — pointer
        // addresses are non-stable across builds and would make debug
        // output noisy. The source column + child table together
        // identify the join unambiguously.
        f.debug_struct("ErasedSelectRelated")
            .field("source_column", &self.source_column)
            .field("child_table", &self.child_table)
            .finish_non_exhaustive()
    }
}

/// Monomorphised join decoder for a concrete `Child`.
///
/// Probes the child `id` column (under the prefixed alias) for NULL to
/// distinguish LEFT JOIN misses (NULL child columns end-to-end) from live
/// child rows. The probe uses `row.try_get::<Option<i64>>(id_alias)` —
/// a `None` result indicates a LEFT JOIN miss (NULL FK or orphan target).
///
/// This works for `HeerId`-keyed models (BIGINT) and is the standard
/// T2 probe. T4 will generalise this to other PK types via the
/// `FromPgRow` trait's column-index probing.
///
/// Returns `Ok(None)` on miss so the caller can omit the child from
/// the row's relation map; returns `Ok(Some(box))` on hit; propagates
/// decode errors on decode failure. A missing `{prefix}id` column
/// surfaces as a conservative `None` (rather than a cryptic decode
/// error) because a schema mismatch at that level is a framework bug
/// the test suite would catch, not a user-facing fault.
pub(crate) fn join_decoder<Child>(
    row: &PgRow,
    prefix: &str,
) -> Result<Option<Box<dyn Any + Send + Sync>>, DjogiError>
where
    Child: Model + FromJoinedPgRow + Send + Sync + 'static,
{
    // Build the id-alias lookup: "{prefix}id". Inlining `format!` here
    // is fine — the hot path is one call per (row, path), which is a
    // negligible allocation next to the row decode itself.
    let id_alias = format!("{prefix}id");

    // Probe for LEFT JOIN miss: if the aliased `id` column is NULL then
    // the join found no child row. `try_get::<Option<i64>>` returns
    // `Ok(None)` for SQL NULL and `Ok(Some(v))` for a live integer.
    // A missing column (schema/aliasing bug) returns `Err` — we treat
    // that conservatively as "no child" and emit a debug assertion so
    // the test suite catches the framework-level mismatch early.
    let id_probe: Result<Option<i64>, _> = row.try_get(id_alias.as_str());
    debug_assert!(
        id_probe.is_ok(),
        "select_related: missing join alias '{id_alias}' on joined row — framework bug (check select_columns emission vs push_joins alias)"
    );
    let child_is_null = id_probe.map(|v| v.is_none()).unwrap_or(true);

    if child_is_null {
        return Ok(None);
    }

    let child = <Child as FromJoinedPgRow>::from_joined_pg_row(row, prefix)?;
    Ok(Some(Box::new(child) as Box<dyn Any + Send + Sync>))
}

/// Append the `LEFT JOIN` clauses for every registered select_related
/// path to `acc`.
///
/// Emits one `LEFT JOIN {child_table} rel_{source_column} ON
/// {parent_table}.{source_column} = rel_{source_column}.id` per path.
/// The aliases (`rel_{source_column}`) are unique per parent column
/// by construction — the macro-emitted `{Model}Related` builders
/// pin one path per source column, and the queryset's dedup check
/// ensures no duplicate registrations.
///
/// All identifiers are `&'static str` literals (baked by the
/// `#[model]` macro), so `push_sql(...)` is safe without quoting.
pub(crate) fn push_joins<T: Model>(acc: &mut SqlAccumulator, paths: &[ErasedSelectRelated]) {
    for path in paths {
        acc.push_sql(" LEFT JOIN ");
        acc.push_sql(path.child_table);
        acc.push_sql(" rel_");
        acc.push_sql(path.source_column);
        acc.push_sql(" ON ");
        acc.push_sql(T::table_name());
        acc.push_sql(".");
        acc.push_sql(path.source_column);
        acc.push_sql(" = rel_");
        acc.push_sql(path.source_column);
        acc.push_sql(".id");
    }
}

/// Build the SELECT column list for a queryset with registered
/// select_related paths.
///
/// Returns a pre-rendered string of the form:
///
/// ```text
/// parent_table.*, rel_col_a.id AS "rel_col_a.id", rel_col_a.name AS "rel_col_a.name", …
/// ```
///
/// The parent columns come through unqualified (their column name
/// surfaces in the result set as just `"id"`, `"make"`, …). Each
/// joined child's columns are aliased as `"rel_{source_column}.{col}"`
/// — the embedded dot matches the shape Postgres uses internally for
/// table-qualified projections and gives the
/// [`FromJoinedPgRow`](crate::pg::decode::FromJoinedPgRow)
/// decoder a stable, collision-free lookup key.
///
/// The returned `String` is pushed onto the builder verbatim — no
/// user-supplied data reaches this path, only `&'static str`
/// literals from the descriptor and from the registered path.
pub(crate) fn select_columns<T: Model>(paths: &[ErasedSelectRelated]) -> String {
    let mut out = String::new();
    out.push_str(T::table_name());
    out.push_str(".*");
    for path in paths {
        let alias = format!("rel_{}", path.source_column);
        let desc = (path.child_descriptor)();
        for field in desc.fields {
            // The Model trait is sealed, so `desc.fields` comes from a
            // `#[derive(Model)]`-emitted descriptor — `field.name` should
            // already satisfy the identifier contract. Re-validate in
            // debug builds so a malformed emission (or a downstream macro
            // pretending to be `#[derive(Model)]`) surfaces as a loud
            // framework-bug panic instead of malformed SQL.
            crate::ident::debug_assert_ident!(field.name, "field_name");
            out.push_str(", ");
            // Source column: `rel_owner_id.id`.
            out.push_str(&alias);
            out.push('.');
            out.push_str(field.name);
            // Destination alias: `"rel_owner_id.id"`. The double-quote
            // wrap preserves the dot as a literal character in the
            // result-set column name (rather than Postgres treating
            // the dot as a qualifier).
            out.push_str(" AS \"");
            out.push_str(&alias);
            out.push('.');
            out.push_str(field.name);
            out.push('"');
        }
    }
    out
}

/// Decode every registered select_related child from one joined row
/// and package the result into a `JoinedRow<T>`.
///
/// Each path contributes one entry to the `relations` map, keyed by
/// `source_column` (matching
/// [`PrefetchedRow`](crate::relation::PrefetchedRow)'s key convention).
/// LEFT JOIN misses — surfaced by `join_decoder` returning
/// `Ok(None)` — skip the map insert so
/// [`JoinedRow::get`](crate::relation::JoinedRow::get) returns `None`
/// for that path.
///
/// `FromJoinedPgRow::from_joined_pg_row(row, "")` decodes the parent
/// with the empty prefix — similar in spirit to what `FromPgRow::from_pg_row`
/// reads for a bare-columns row, but reusing the same trait across
/// both sides keeps the macro emission small (one extra impl per model).
pub(crate) fn decode_joined_row<T: Model + FromJoinedPgRow>(
    row: &PgRow,
    paths: &[ErasedSelectRelated],
) -> Result<JoinedRow<T>, DjogiError> {
    let parent = <T as FromJoinedPgRow>::from_joined_pg_row(row, "")?;
    let mut relations: HashMap<&'static str, Box<dyn Any + Send + Sync>> =
        HashMap::with_capacity(paths.len());
    for path in paths {
        let prefix = format!("rel_{}.", path.source_column);
        if let Some(child_box) = (path.decoder)(row, &prefix)? {
            relations.insert(path.source_column, child_box);
        }
    }
    Ok(JoinedRow::new(parent, relations))
}

/// Terminal-layer helper that runs the select_related decode over
/// every row returned by the main query.
///
/// Short-circuits on an empty row set — no decoder is invoked, no
/// prefetch fan-out happens. Kept separate from the SQL-emission
/// helpers so the emitter can be unit-tested in isolation from the
/// decode path.
pub(crate) fn apply_select_related<T>(
    rows: Vec<PgRow>,
    paths: &[ErasedSelectRelated],
) -> Result<Vec<JoinedRow<T>>, DjogiError>
where
    T: Model + FromJoinedPgRow,
{
    let mut out: Vec<JoinedRow<T>> = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(decode_joined_row::<T>(row, paths)?);
    }
    Ok(out)
}

/// Erased-prefetch stitcher used by the `fetch_all_joined` terminal.
///
/// This is a thin bridge between the prefetch loader (which works on
/// `Vec<T>` + parent PKs) and the `JoinedRow<T>` wrapper type. The
/// terminal calls this when `.select_related(...)` and `.prefetch(...)`
/// are both registered on the same queryset; the prefetched entries
/// land under the same `source_column` keys `select_related`'s joined
/// entries use, so a single call to
/// [`JoinedRow::get`](crate::relation::JoinedRow::get) on a merged
/// row resolves either path transparently.
///
/// Kept separate from [`apply_select_related`] to preserve the
/// one-concern-per-function split: JOIN decoding here, prefetch
/// stitching over there. The terminal orchestrates both.
pub(crate) async fn stitch_prefetches_into_joined<T>(
    mut joined: Vec<JoinedRow<T>>,
    prefetches: &[crate::relation::prefetch::ErasedPrefetch],
    exec: &mut ContextInner,
) -> Result<Vec<JoinedRow<T>>, DjogiError>
where
    T: Model,
    T::Pk: Clone + Send + Sync + 'static,
{
    // Empty prefetch set — short-circuit without extracting PKs.
    // Covers the common `.select_related(...)` without `.prefetch(...)`
    // case; also a safety rail for the terminal-layer dispatch.
    if prefetches.is_empty() || joined.is_empty() {
        return Ok(joined);
    }

    // Fan out each prefetch loader, same shape as
    // `relation::prefetch::apply_prefetches` — we duplicate the
    // orchestration here (rather than reusing that function) because
    // the stitcher writes into `JoinedRow::relations_mut()`, not the
    // `PrefetchedRow::relations` field, and `PrefetchedRow` is a
    // distinct wrapper type whose conversion into `JoinedRow` would
    // cost more than the 20-line fan-out.
    //
    // One fresh cloned PK list per loader — each loader consumes its
    // input by downcasting out of the box, so reusing a single `Vec`
    // across loaders would break on the first downcast. The per-loader
    // clone pays `T::Pk::clone` per parent row per prefetch, which is
    // negligible for the HeerId / RanjId PK types Djogi ships.
    for prefetch in prefetches {
        let parent_pks_for_loader: Vec<Box<dyn Any + Send + Sync>> = joined
            .iter()
            .map(|jr| Box::new(jr.row.pk_value().clone()) as Box<dyn Any + Send + Sync>)
            .collect();
        // Re-borrow the context per loader iteration — see the mirror
        // call site in `prefetch::apply_prefetches` for the full note.
        let aligned = (prefetch.loader)(
            &mut *exec,
            prefetch.parent_table,
            prefetch.source_column,
            parent_pks_for_loader,
        )
        .await?;

        // Zip aligned results into per-row relation maps. Same
        // `take()`-per-slot pattern the prefetch stitcher uses —
        // each boxed target moves into exactly one row.
        for (jr, slot) in joined.iter_mut().zip(aligned) {
            if let Some(child_box) = slot {
                jr.relations_mut().insert(prefetch.source_column, child_box);
            }
        }
    }

    Ok(joined)
}

#[cfg(test)]
mod tests {
    //! Emitter unit tests — assert on the generated SQL without touching
    //! a real DB. Live-Postgres coverage lives in
    //! `tests/integration/phase3_relations.rs`.

    use super::*;
    use crate::descriptor::{FieldDescriptor, FieldSqlType, ModelDescriptor, PkType};
    use crate::model::Model;
    use crate::pg::accumulator::SqlAccumulator;
    use crate::types::HeerId;
    use std::future::Future;

    // Minimal Model stubs mirroring the pattern in sibling `relation/`
    // modules — enough to satisfy the `Model` bound on `push_joins` /
    // `select_columns` without dragging in the full `#[model]` macro.
    struct Src;
    impl crate::model::__sealed::Sealed for Src {}
    #[allow(clippy::manual_async_fn)]
    impl Model for Src {
        type Pk = HeerId;
        type Fields = ();
        fn table_name() -> &'static str {
            "srcs"
        }
        fn pk_value(&self) -> &HeerId {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!()
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: HeerId,
        ) -> impl Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }
    }

    fn dummy_decoder(
        _row: &PgRow,
        _prefix: &str,
    ) -> Result<Option<Box<dyn Any + Send + Sync>>, DjogiError> {
        // Never invoked by the SQL-emission unit tests — they only
        // exercise `push_joins` / `select_columns`, which never call
        // the decoder.
        unreachable!("dummy decoder should not run in SQL-emission tests")
    }

    // Static two-column "child" descriptor used by the select_columns
    // emitter tests. Must be static so the fn pointer below can return
    // a `&'static ModelDescriptor` without runtime construction.
    static OWNERS_DESC: ModelDescriptor = ModelDescriptor {
        type_name: "Owner",
        table_name: "owners",
        pk_type: PkType::HeerId,
        fields: &[
            FieldDescriptor {
                name: "id",
                sql_type: FieldSqlType::BigInt,
                nullable: false,
                unique: true,
                indexed: true,
                max_length: None,
                renamed_from: None,
                rationale: None,
                outbox_exclude: false,
                sequence_within: None,
                index_type: None,
                relation_kind: None,
                on_delete: None,
                target_type_name: None,
                visage_map: &[],
                protected: None,
                default_volatility_override: None,
            },
            FieldDescriptor {
                name: "name",
                sql_type: FieldSqlType::Text,
                nullable: false,
                unique: false,
                indexed: false,
                max_length: None,
                renamed_from: None,
                rationale: None,
                outbox_exclude: false,
                sequence_within: None,
                index_type: None,
                relation_kind: None,
                on_delete: None,
                target_type_name: None,
                visage_map: &[],
                protected: None,
                default_volatility_override: None,
            },
        ],
        partition_by: None,
        has_outbox: false,
        idempotency_key: None,
        tenant_key: None,
        cache_ttl: None,
        rationale: None,
        indexes: &[],
        is_through: false,
        fts: None,
        app: None,
        moved_from_app: None,
        renamed_from: None,
    };

    static FUEL_TYPES_DESC: ModelDescriptor = ModelDescriptor {
        type_name: "FuelType",
        table_name: "fuel_types",
        pk_type: PkType::HeerId,
        fields: &[FieldDescriptor {
            name: "id",
            sql_type: FieldSqlType::BigInt,
            nullable: false,
            unique: true,
            indexed: true,
            max_length: None,
            renamed_from: None,
            rationale: None,
            outbox_exclude: false,
            sequence_within: None,
            index_type: None,
            relation_kind: None,
            on_delete: None,
            target_type_name: None,
            visage_map: &[],
            protected: None,
            default_volatility_override: None,
        }],
        partition_by: None,
        has_outbox: false,
        idempotency_key: None,
        tenant_key: None,
        cache_ttl: None,
        rationale: None,
        indexes: &[],
        is_through: false,
        fts: None,
        app: None,
        moved_from_app: None,
        renamed_from: None,
    };

    fn owners_descriptor() -> &'static ModelDescriptor {
        &OWNERS_DESC
    }
    fn fuel_types_descriptor() -> &'static ModelDescriptor {
        &FUEL_TYPES_DESC
    }

    #[test]
    fn push_joins_emits_left_join_with_aliased_table() {
        // Single registered path — one `LEFT JOIN {child} rel_{col} ON
        // {parent}.{col} = rel_{col}.id` appended. The parent table
        // name comes from the `Model` bound; the child table and
        // source column come from the erased path.
        let path = ErasedSelectRelated {
            source_column: "owner_id",
            child_table: "owners",
            decoder: dummy_decoder,
            child_descriptor: owners_descriptor,
        };
        let mut acc = SqlAccumulator::new("SELECT * FROM srcs");
        push_joins::<Src>(&mut acc, &[path]);
        let sql = acc.sql();
        assert!(
            sql.contains("LEFT JOIN owners rel_owner_id ON srcs.owner_id = rel_owner_id.id"),
            "expected aliased LEFT JOIN, got: {sql}"
        );
    }

    #[test]
    fn push_joins_emits_one_clause_per_path() {
        // Two paths on the same queryset — two LEFT JOINs, each with
        // its own `rel_{source_column}` alias. The aliases don't
        // collide because source columns are unique per parent model.
        let paths = vec![
            ErasedSelectRelated {
                source_column: "owner_id",
                child_table: "owners",
                decoder: dummy_decoder,
                child_descriptor: owners_descriptor,
            },
            ErasedSelectRelated {
                source_column: "fuel_type_id",
                child_table: "fuel_types",
                decoder: dummy_decoder,
                child_descriptor: fuel_types_descriptor,
            },
        ];
        let mut acc = SqlAccumulator::new("SELECT * FROM srcs");
        push_joins::<Src>(&mut acc, &paths);
        let sql = acc.sql();
        assert!(
            sql.contains("LEFT JOIN owners rel_owner_id"),
            "missing owner join in: {sql}"
        );
        assert!(
            sql.contains("LEFT JOIN fuel_types rel_fuel_type_id"),
            "missing fuel_type join in: {sql}"
        );
    }

    #[test]
    fn select_columns_emits_parent_star_and_aliased_children() {
        let path = ErasedSelectRelated {
            source_column: "owner_id",
            child_table: "owners",
            decoder: dummy_decoder,
            child_descriptor: owners_descriptor,
        };
        let cols = select_columns::<Src>(&[path]);
        // Parent star comes through unqualified — the child columns
        // land under `"rel_owner_id.{col}"` aliases.
        assert!(cols.starts_with("srcs.*"), "got: {cols}");
        assert!(
            cols.contains("rel_owner_id.id AS \"rel_owner_id.id\""),
            "missing aliased id column, got: {cols}"
        );
        assert!(
            cols.contains("rel_owner_id.name AS \"rel_owner_id.name\""),
            "missing aliased name column, got: {cols}"
        );
    }

    #[test]
    fn select_columns_empty_paths_returns_just_parent_star() {
        // No registered paths — the emitter degenerates to just the
        // parent `*` projection, matching what the plain build_select
        // would emit. Keeps the column-list builder safe to call
        // unconditionally.
        let cols = select_columns::<Src>(&[]);
        assert_eq!(cols, "srcs.*");
    }
}
