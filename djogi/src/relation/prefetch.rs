//! `PrefetchedRow<T>` — the post-prefetch wrapper + its loader machinery.
//! # What
//! [`PrefetchedRow<T>`] pairs a main-query row with the related rows the
//! prefetch layer materialised for it. User code obtains a
//! `Vec<PrefetchedRow<T>>` from
//! [`QuerySet::fetch_all_prefetched`](crate::query::QuerySet::fetch_all_prefetched)
//! and reads resolved relations via [`PrefetchedRow::get`], typed by the same
//! `RelationPath<Source, Target>` that was passed to `.prefetch(...)`.
//! # Why a wrapper and not a mutated row
//! Locked in [`ForeignKey<T>`](crate::relation::ForeignKey) as a
//! PK-only wrapper whose `.resolved()` **always returns `None`** — the type's
//! whole purpose is to carry a foreign-key reference and nothing else, with
//! `Copy` / `Eq` / `Hash` semantics derived from the inner PK. Mutating the
//! row post-fetch to attach cached children would either break that contract
//! (lose `Copy` by adding interior mutability) or silently diverge two wrappers
//! that previously compared equal by PK. Neither is acceptable.
//! Instead, the prefetch layer returns a **new** wrapper type
//! (`PrefetchedRow<T>`) that owns the row by value plus a side table of
//! resolved relations keyed by source column name. The base model type is
//! untouched — a caller who ignores prefetches still reads raw `T` via
//! [`fetch_all`](crate::query::QuerySet::fetch_all).
//! # How the stitching query is shaped
//! For each registered prefetch path, a single SQL round trip fetches the
//! target rows paired with their originating parent PKs via a `LEFT JOIN`:
//! ```sql
//! SELECT t.col_1, t.col_2, …, t.col_N, p.id AS __djogi_parent_id
//! FROM <parent_table> p
//! LEFT JOIN <target_table> t ON t.id = p.<source_column>
//! WHERE p.id IN ($1, $2, …, $N)
//! ```
//! Target columns come FIRST in `FromPgRow::COLUMN_LIST` order so
//! [`FromPgRow::from_pg_row`](crate::pg::decode::FromPgRow::from_pg_row)
//! can decode them positionally (ordinals `0..N_COLS`) without seeing
//! the trailing `__djogi_parent_id` sentinel; the decoder's
//! debug-build drift guard checks ordinals `0..N_COLS` exactly and
//! ignores any trailing columns.
//! `IN (...)` is preferred over `ANY($1)` so the loader does not have
//! to require `Source::Pk: PgHasArrayType` — neither `HeerId` nor
//! `RanjId` implement that trait in the sibling HeeRanjID crate, and
//! the per-distinct-arity prepared plan cost is an accepted trade-off
//! for keeping the framework's type bounds minimal.
//! The target columns are enumerated explicitly (via
//! `<Target as Model>::descriptor().fields`) rather than using `t.*`:
//! that way the `__djogi_parent_id` synthetic alias cannot collide with
//! any real column on the target — if a user declared a column literally
//! named `__djogi_parent_id`, `t.*` would return two `__djogi_parent_id`
//! columns and `try_get` would pick one at the decode layer with no
//! guarantee it is the parent PK. Explicit enumeration removes the
//! conflict surface entirely; the synthetic alias is the only column
//! under that name in the result set by construction.
//! - The `LEFT JOIN` naturally handles nullable FK columns: rows whose
//! `source_column` is `NULL` appear with every enumerated target
//! column null-decoded, which surfaces as `None` on the per-parent
//! result slot.
//! - The aliased `__djogi_parent_id` gives the stitcher a way back to the
//! parent row irrespective of whether target columns were null. The
//! target columns are emitted first so ordinal-decoding of
//! `Target` via `FromPgRow::from_pg_row` remains untouched; the
//! sentinel lands at the last position and is read by name
//! (`row.try_get("__djogi_parent_id")`).
//! - Target rows are not deduplicated at the SQL layer — two parent rows
//! pointing at the same target produce two result rows, each carrying
//! the same target payload under its own parent PK. Stitching preserves
//! that per-parent association; `row.get(...)` on each `PrefetchedRow`
//! returns its own `&Target` reference backed by its own `Box<Target>`.
//! # Loader shape
//! Each prefetch loader returns `Vec<Option<Box<dyn Any + Send + Sync>>>`
//! aligned 1-to-1 with the input parent-PK list. An entry is `Some(Box<Target>)`
//! when the parent's FK resolved to a live target row, and `None`
//! otherwise (NULL FK column or orphan LEFT JOIN miss). The alignment means
//! the stitcher never needs to know `Target` — it just copies the vector
//! into `PrefetchedRow::relations` keyed by `source_column` on the path.
//! Downcast back to `&Target` happens at read time via
//! [`PrefetchedRow::get`], driven by the typed `RelationPath<Source, Target>`.
//! # Type erasure choice
//! `QuerySet<T>` accumulates prefetch paths for arbitrary, heterogeneous
//! target types (`.prefetch(VehicleRelated::owner()).prefetch(VehicleRelated::fuel_type())`
//! stores loaders for `Owner` and `FuelType` side by side). The
//! [`ErasedPrefetch`] handle therefore carries an `fn` pointer whose concrete
//! signature is monomorphised per call-site at `.prefetch(...)` time
//! [`prefetch_loader`] captures both model types via regular generics. The
//! erasure at the `Vec<ErasedPrefetch>` layer is a plain function pointer
//! (`fn`) — no `Box<dyn Trait>` — because every prefetchable shape reuses
//! one free function, just with different `Source` / `Target` substitutions.
//! The per-parent `Option<Target>` is boxed as `Box<dyn Any + Send + Sync>`
//! so the stitcher can carry heterogeneous target types through a single
//! `HashMap`; the downcast at `PrefetchedRow::get` time is infallible by
//! construction (`path.source_column()` plus the `RelationPath` type
//! parameters together pin the concrete `Target`).
//! # Where
//! Consumed by [`crate::query::terminal::QuerySet::fetch_all_prefetched`]
//! and emitted in [`crate::query::queryset::QuerySet::prefetch`].
//! (`select_related`) ships a *separate* JOIN-based path
//! (`JoinedRow<Parent, Child>`) that does not share this loader infrastructure
//! see the header comment on that module for the split rationale.

use crate::DjogiError;
use crate::context::ContextInner;
use crate::model::Model;
use crate::pg::accumulator::{SqlAccumulator, as_params};
use crate::pg::decode::FromPgRow;
use crate::relation::path::RelationPath;
use postgres_types::ToSql;
use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use tokio_postgres::types::FromSql;

/// Type-erased resolved-target payload. One `Box` per prefetched row
/// slot; `Option` so NULL FKs and orphan LEFT JOIN misses can carry
/// through as "absent" without forcing a sentinel target value.
/// Kept at the module level so both the [`PrefetchLoaderFn`] return
/// signature and the per-path aligned vector in
/// [`apply_prefetches`] name the same type. Prevents the clippy
/// `type_complexity` lint from firing on inline spellings and, more
/// importantly, keeps the erasure shape in a single place so refactors
/// only need to touch one declaration.
pub(crate) type ResolvedTargetSlot = Option<Box<dyn Any + Send + Sync>>;

/// Per-prefetch-path aligned result vector — position `i` carries the
/// resolved-target slot for parent row `i`. The loader produces one of
/// these per registered path; [`apply_prefetches`] zips them row-wise
/// into [`PrefetchedRow::relations`].
pub(crate) type AlignedTargets = Vec<ResolvedTargetSlot>;

/// Post-prefetch wrapper pairing a main-query row with resolved relations.
/// Produced by
/// [`QuerySet::fetch_all_prefetched`](crate::query::QuerySet::fetch_all_prefetched).
/// Access the underlying row via the public [`PrefetchedRow::row`] field
/// and resolved relations via [`PrefetchedRow::get`].
/// # Why the relations map keys on `&'static str` rather than `TypeId`
/// Two prefetch paths can legitimately resolve to the same target type
/// (e.g. `author: ForeignKey<User>` and `editor: ForeignKey<User>` on a
/// `Post` — both point at `User`). A `TypeId` key would collapse the two
/// into a single slot. The source column name is the natural discriminator
/// it is unique per relation by macro-emission rules and available on
/// every `RelationPath` via
/// [`RelationPath::source_column`](crate::relation::RelationPath::source_column).
/// # Ownership shape
/// `Box<dyn Any + Send + Sync>` lets the map carry heterogeneous target
/// types without a variadic generic; `Send + Sync` propagates through so
/// `Vec<PrefetchedRow<T>>` can cross async task boundaries.
pub struct PrefetchedRow<T: Model> {
    /// The main-query row, as returned by the underlying
    /// [`fetch_all`](crate::query::QuerySet::fetch_all) path. Relation
    /// fields on this row remain in their raw (unresolved) shape
    /// prefetch does not mutate them. See the module-level docs for the
    /// rationale.
    pub row: T,
    /// Resolved relations, keyed by
    /// [`RelationPath::source_column`](crate::relation::RelationPath::source_column).
    /// Entries are absent for paths where the FK was NULL or the target
    /// row was missing; in either case [`PrefetchedRow::get`] returns
    /// `None`.
    relations: HashMap<&'static str, Box<dyn Any + Send + Sync>>,
}

impl<T: Model + std::fmt::Debug> std::fmt::Debug for PrefetchedRow<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Render the resolved-relation map as its key set — the map
        // values cannot be formatted generically, but the source-column
        // keys are strings and useful on their own for debugging which
        // prefetches attached to this row.
        let keys: Vec<&&'static str> = self.relations.keys().collect();
        f.debug_struct("PrefetchedRow")
            .field("row", &self.row)
            .field("resolved_relations", &keys)
            .finish()
    }
}

impl<T: Model> PrefetchedRow<T> {
    /// Look up a prefetched relation by its [`RelationPath`].
    /// Returns `Some(&Target)` when the main-query row carried a non-null
    /// FK **and** the target row existed at query time. Returns `None`
    /// for nullable FKs whose column was `NULL`, for FKs pointing at rows
    /// that have since been deleted (LEFT JOIN miss), and for any path
    /// that was never registered via
    /// [`QuerySet::prefetch`](crate::query::QuerySet::prefetch).
    /// The typed `RelationPath<T, Target>` argument means mismatched
    /// target types fail at the type level — a
    /// `RelationPath<Vehicle, Owner>` can only be read back as
    /// `&Owner`, never as `&FuelType`.
    pub fn get<Target: Model + 'static>(&self, path: RelationPath<T, Target>) -> Option<&Target> {
        self.relations
            .get(path.source_column())
            .and_then(|b| b.downcast_ref::<Target>())
    }

    /// Crate-private constructor used by [`apply_prefetches`]. Not part
    /// of the public API — rows originate from the terminal method,
    /// never from user code.
    pub(crate) fn new(
        row: T,
        relations: HashMap<&'static str, Box<dyn Any + Send + Sync>>,
    ) -> Self {
        Self { row, relations }
    }
}

// ---------------------------------------------------------------------------
// Type-erased prefetch handle + loader fn type.
// ---------------------------------------------------------------------------

/// Function signature every prefetch loader satisfies.
/// `parent_pks` arrives as `Box<dyn Any + Send + Sync>` to let one
/// heterogeneous `Vec<ErasedPrefetch>` store loaders for different
/// `Source::Pk` types — the loader downcasts back to its concrete
/// `Source::Pk` on entry. The return vector is erased the same way; the
/// caller in [`apply_prefetches`] stores each `Option<Box<dyn Any>>` in
/// the per-row relations map without ever naming `Target`.
/// The return shape is `Vec<Option<Box<dyn Any + Send + Sync>>>` aligned
/// 1-to-1 with the input `parent_pks` order: index `i` carries the
/// target for parent `i`, or `None` if that parent's FK was null or the
/// target row was absent. Keeping the alignment lets the stitcher be
/// `Target`-agnostic.
/// Using a plain `fn` pointer (not `Box<dyn Fn>` or `&'static dyn Trait`)
/// keeps [`ErasedPrefetch`] allocation-free and avoids a virtual-call
/// per registration. Every call site monomorphises [`prefetch_loader`]
/// once and the resulting `fn` pointer has a fixed, ABI-stable address.
/// The loader takes `&'a mut ContextInner` rather than a bare `&'a PgPool`
/// so prefetch fan-out works over both pool-backed and
/// transaction-backed [`DjogiContext`](crate::context::DjogiContext)
/// variants. Inside the loader body, each query dispatch site matches on the
/// context variant to reach either the pool connection or the open transaction.
/// Without this generalisation, `.prefetch(...)`
/// inside an `atomic()` scope would fail with a
/// `DjogiError::Db` configuration-style error — see for
/// the closure.
pub(crate) type PrefetchLoaderFn = for<'a> fn(
    exec: &'a mut ContextInner,
    parent_table: &'static str,
    source_column: &'static str,
    parent_pks: Vec<Box<dyn Any + Send + Sync>>,
) -> Pin<
    Box<dyn Future<Output = Result<AlignedTargets, DjogiError>> + Send + 'a>,
>;

/// A single type-erased prefetch registration on a
/// [`QuerySet<T>`](crate::query::QuerySet).
/// Built by [`QuerySet::prefetch`](crate::query::QuerySet::prefetch) from
/// a typed [`RelationPath<Source, Target>`]. The `loader` captures both
/// `Source` and `Target` at the type level via monomorphisation of
/// [`prefetch_loader`]; `source_column` / `parent_table` are the dynamic
/// parameters the loader reads from the path at execution time.
#[derive(Clone)]
pub(crate) struct ErasedPrefetch {
    /// Source column name (e.g. `"owner_id"`) — doubles as the map key
    /// under which the resolved target is stored on
    /// [`PrefetchedRow::relations`].
    pub source_column: &'static str,
    /// Parent (source model) table name. Needed inside the loader to
    /// build the `FROM <parent_table>` clause; stored on the handle so
    /// the loader stays free-standing (no `Source` type capture beyond
    /// the fn-pointer monomorphisation).
    pub parent_table: &'static str,
    /// Monomorphised loader fn pointer.
    pub loader: PrefetchLoaderFn,
}

impl std::fmt::Debug for ErasedPrefetch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ErasedPrefetch")
            .field("source_column", &self.source_column)
            .field("parent_table", &self.parent_table)
            // Skip `loader` — fn-pointer printing is ABI-dependent and
            // not stable across builds. The column + table together are
            // enough to identify which prefetch this is in debug logs.
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Monomorphised loader.
// ---------------------------------------------------------------------------

/// Monomorphised prefetch loader.
/// One `fn` pointer per unique `(Source, Target)` pair. Captures both
/// model types via regular generics; the `fn`-pointer coercion at the
/// `.prefetch(...)` call site turns it into a [`PrefetchLoaderFn`].
/// Returns a `Vec<Option<Box<dyn Any + Send + Sync>>>` aligned 1-to-1
/// with the input `parent_pks`. Each slot is
/// `Some(Box::new(target) as Box<dyn Any>)` when the parent's FK
/// resolved to a live target row, or `None` otherwise (NULL FK or
/// LEFT JOIN miss).
/// The generic bounds spell out the full set of `postgres_types` / `Any`
/// contracts the runtime path needs:
/// - `Source: Model` — lets us name `Source::Pk` for downcasting.
/// - `Source::Pk: postgres_types::ToSql + FromSql + Eq + Hash + Clone +
/// 'static + Send + Sync` — required to bind the per-parent `IN (...)`
/// arguments, decode the `__djogi_parent_id` column, dedupe the
/// query-side input, and use the PK as a HashMap key for stitching.
/// No array-type bound — the emitter uses `IN (...)` rather than
/// `ANY($1)` precisely so the HeeRanjID PKs slot in unchanged.
/// - `Target: Model + FromPgRow + Clone + Send + Unpin + 'static`
/// the LEFT JOIN returns target columns; `FromPgRow::from_pg_row` decodes them
/// from a `tokio_postgres::Row`, `Any` erases the concrete type for
/// the return channel.
pub(crate) fn prefetch_loader<'a, Source, Target>(
    exec: &'a mut ContextInner,
    parent_table: &'static str,
    source_column: &'static str,
    parent_pks: Vec<Box<dyn Any + Send + Sync>>,
) -> Pin<Box<dyn Future<Output = Result<AlignedTargets, DjogiError>> + Send + 'a>>
where
    Source: Model,
    Source::Pk: ToSql + for<'r> FromSql<'r> + Eq + Hash + Clone + Send + Sync + 'static,
    Target: Model + FromPgRow + Clone + Send + Unpin + 'static,
{
    Box::pin(async move {
        // Down-erase the parent PK list. Every element was `Box::new`'d
        // from a `Source::Pk` in `extract_parent_pks`, so the downcast
        // is infallible by construction; `.expect` signals a framework
        // bug if it ever fires.
        let parent_pks_typed: Vec<Source::Pk> = parent_pks
            .into_iter()
            .map(|b| {
                *b.downcast::<Source::Pk>().expect(
                    "prefetch_loader: parent PK downcast — type mismatch is a framework bug",
                )
            })
            .collect();

        // Deduplicate the parent-PK list for the IN (...) bind. Preserves
        // correctness — duplicates would just make Postgres do redundant
        // index probes — while cutting bind-array size on highly-
        // repetitive parent lists (paginated pages with the same parent
        // showing up across pages, etc.). The downstream alignment pass
        // uses the *original* ordered list to build the return vec, so
        // the dedupe here is query-side only.
        let mut seen: HashMap<Source::Pk, ()> = HashMap::with_capacity(parent_pks_typed.len());
        let mut unique_pks: Vec<Source::Pk> = Vec::with_capacity(parent_pks_typed.len());
        for pk in &parent_pks_typed {
            if seen.insert(pk.clone(), ()).is_none() {
                unique_pks.push(pk.clone());
            }
        }

        // Empty parent set — skip the SQL entirely. Covers the case
        // where the main query returned no rows (although the stitcher
        // short-circuits before even calling the loader in that case)
        // and, defensively, any future caller that might pass an empty
        // list.
        if unique_pks.is_empty() {
            // `vec![None; n]` does not work: `Box<dyn Any>` is not Clone,
            // so the `Option<Box<..>>` element type fails the `vec!`
            // macro's `T: Clone` bound. Build the all-`None` output by
            // hand.
            let mut out: Vec<Option<Box<dyn Any + Send + Sync>>> =
                Vec::with_capacity(parent_pks_typed.len());
            out.resize_with(parent_pks_typed.len(), || None);
            return Ok(out);
        }

        // Build the stitching query using SqlAccumulator:
        // SELECT t.col_1, t.col_2,..., t.col_N, p.id AS __djogi_parent_id
        // FROM <parent_table> p
        // LEFT JOIN <target_table> t ON t.id = p.<source_column>
        // WHERE p.id IN ($1, $2,..., $N)
        // Target columns come FIRST in the canonical `FromPgRow::COLUMN_LIST`
        // order so `FromPgRow::from_pg_row` decodes them positionally
        // (ordinals 0..N_COLS) and the per-column name guards pass. The
        // synthetic `__djogi_parent_id` alias lands at position N_COLS
        // and is read by name (`row.try_get("__djogi_parent_id")`), which
        // is tolerant of its ordinal position shifting if the target
        // gains more columns in a future schema revision.
        // `IN (...)` with one bind per parent PK is used rather than
        // `ANY($1)` — no array-type bound is needed so the HeeRanjID PKs
        // slot in unchanged. The downside is one prepared plan per distinct
        // list arity; this is the accepted trade-off.
        // LEFT JOIN naturally handles nullable FKs and orphan targets:
        // parent rows with NULL `source_column` or whose FK points at a
        // deleted target emit target columns as NULL, which the null
        // probe below surfaces as `None`.
        // Target columns are enumerated explicitly via
        // `Target::descriptor().fields` rather than using `t.*` to keep
        // the wire shape stable across migrations that might list user
        // columns before framework columns (the `accounts` case).
        let target_table = <Target as Model>::table_name();
        let target_fields = <Target as Model>::descriptor().fields;
        let mut acc = SqlAccumulator::new("SELECT ");
        for (i, field) in target_fields.iter().enumerate() {
            // The Model trait is sealed, so `target_fields` comes from a
            // `#[derive(Model)]`-emitted descriptor — `field.name` should
            // already satisfy the identifier contract. Re-validate in
            // debug builds so a malformed emission surfaces as a loud
            // framework-bug panic instead of malformed SQL.
            crate::ident::debug_assert_ident!(field.name, "field_name");
            if i > 0 {
                acc.push_sql(", ");
            }
            acc.push_sql("t.");
            acc.push_sql(field.name);
        }
        acc.push_sql(", p.id AS __djogi_parent_id");
        acc.push_sql(" FROM ");
        acc.push_sql(parent_table);
        acc.push_sql(" p LEFT JOIN ");
        acc.push_sql(target_table);
        acc.push_sql(" t ON t.id = p.");
        acc.push_sql(source_column);
        acc.push_sql(" WHERE p.id IN (");
        acc.push_list_binds(unique_pks.iter().cloned());
        acc.push_sql(")");

        // Inline-match dispatch — `ContextInner::Pool` acquires a fresh
        // connection per call; `ContextInner::Transaction` reuses the
        // outer transaction so the prefetch sees its own uncommitted writes.
        let (sql, binds) = acc.into_parts();
        let params = as_params(&binds);
        let rows = match exec {
            ContextInner::Pool(pool) => {
                let mut conn = pool.get().await?;
                conn.query(&sql, &params).await?
            }
            ContextInner::Transaction(conn) => conn.query(&sql, &params).await?,
        };

        // Build HashMap<Source::Pk, Option<Target>>. Only insert an
        // `Option<Target>` entry once per distinct parent PK — if the
        // LEFT JOIN returns multiple rows for the same parent (shouldn't
        // happen for a FK-to-PK join against a unique target PK, but
        // defend anyway), the first row wins. `None` is stored for
        // parents whose target row was absent (NULL FK or orphan).
        let mut parent_to_target: HashMap<Source::Pk, Option<Target>> =
            HashMap::with_capacity(rows.len());
        for row in rows {
            let parent_pk: Source::Pk =
                row.try_get("__djogi_parent_id").map_err(DjogiError::from)?;

            // Probe the target's `id` column to distinguish a LEFT JOIN
            // miss (all target columns NULL) from a real target row.
            // Decode as `Option<Target::Pk>` — NULL maps to `None` for
            // every supported PK type (HeerId/BIGINT, RanjId/UUID,
            // i32/SERIAL). An earlier spelling decoded as `Option<i64>`,
            // which silently dropped every prefetched target on models
            // with `pk = RanjId` (UUID cannot decode to `i64`) because
            // the decode error was swallowed with `unwrap_or(true)`.
            // The Model trait already bounds `Pk: FromSql<'a>`, so no
            // extra bound is needed on the loader. Target columns live
            // at ordinals `0..N_COLS` in the emitted SELECT and `id`
            // is the first entry per `FromPgRow::COLUMNS`; either name
            // (`"id"`) or ordinal (`0`) access works, and the name
            // path is stable across future descriptor reshufflings.
            // Decode errors propagate via `?` — a failure here is a
            // genuine type mismatch or a corrupted row, neither of
            // which should be silently treated as "target absent".
            let target_pk_opt: Option<Target::Pk> = row.try_get("id").map_err(DjogiError::from)?;
            let target_is_null = target_pk_opt.is_none();

            let slot = if target_is_null {
                None
            } else {
                // Decode via `FromPgRow::from_pg_row` — the canonical public
                // trait. Emitted by `#[model]` with `COLUMN_LIST`, ordinal
                // decode, and per-column debug-mode name guards.
                Some(Target::from_pg_row(&row)?)
            };

            parent_to_target.entry(parent_pk).or_insert(slot);
        }

        // Align back to the original parent-PK order. Each parent whose
        // PK resolved to `Some(Target)` gets a fresh `Box<Target>` so
        // two parents pointing at the same target don't share storage
        // matches the user-facing contract on `PrefetchedRow::get`
        // (returns a `&Target`, not a shared pointer).
        // The `Target: Clone` bound on this fn is what lets us mint a
        // per-parent owned copy from the single HashMap entry; `Model`
        // itself does not require `Clone`, but every `#[model]`-derived
        // struct carries `#[derive(Clone)]` because `create()` returns
        // the inserted row by value and user code routinely needs to
        // pass it around. Pushing the bound onto the loader — rather
        // than onto `Model` — keeps the framework's baseline trait
        // contract minimal.
        let mut out: Vec<Option<Box<dyn Any + Send + Sync>>> =
            Vec::with_capacity(parent_pks_typed.len());
        for pk in &parent_pks_typed {
            match parent_to_target.get(pk) {
                Some(Some(t)) => {
                    out.push(Some(Box::new(t.clone()) as Box<dyn Any + Send + Sync>));
                }
                Some(None) | None => {
                    out.push(None);
                }
            }
        }
        Ok(out)
    })
}

// ---------------------------------------------------------------------------
// Stitcher — runs after the main query, produces `Vec<PrefetchedRow<T>>`.
// ---------------------------------------------------------------------------

/// Extract the parent-row PKs for passing into prefetch loaders.
/// Clones `T::Pk` out of each row (PKs are cheap — `HeerId` and friends
/// are `Copy`-sized) and type-erases them so the loader-fn signature
/// stays monomorphic across every `(Source, Target)` pair.
pub(crate) fn extract_parent_pks<T: Model>(rows: &[T]) -> Vec<Box<dyn Any + Send + Sync>>
where
    T::Pk: Clone + Send + Sync + 'static,
{
    rows.iter()
        .map(|r| Box::new(r.pk_value().clone()) as Box<dyn Any + Send + Sync>)
        .collect()
}

/// Apply every registered prefetch loader to `rows` and return a
/// `Vec<PrefetchedRow<T>>` preserving input order.
/// Short-circuits on an empty input slice — no loaders are invoked,
/// no extra SQL round trips. With a non-empty input we always issue
/// one SQL query per *distinct* registered prefetch path; the
/// [`QuerySet::prefetch`](crate::query::QuerySet::prefetch) builder
/// dedupes by `source_column` on registration so calling
/// `.prefetch(path)` twice is a no-op on the second call.
pub(crate) async fn apply_prefetches<T>(
    exec: &mut ContextInner,
    prefetches: &[ErasedPrefetch],
    rows: Vec<T>,
) -> Result<Vec<PrefetchedRow<T>>, DjogiError>
where
    T: Model,
    T::Pk: Clone + Send + Sync + 'static,
{
    if rows.is_empty() {
        // Empty main result — skip every prefetch loader entirely.
        return Ok(Vec::new());
    }

    let parent_pks = extract_parent_pks(&rows);

    // Collect per-path aligned result vectors. Each `Vec<Option<Box<..>>>`
    // is aligned 1-to-1 with `rows`; position `i` carries the target
    // boxed-Any for row `i` (or `None`).
    let mut per_path_results: Vec<(&'static str, AlignedTargets)> =
        Vec::with_capacity(prefetches.len());

    for prefetch in prefetches {
        // Clone the erased parent-PK list per loader — each loader
        // consumes its input by downcasting out of the box. Using a
        // fresh clone per loader keeps the caller free of ownership
        // gymnastics and costs one extra `T::Pk::clone` per parent per
        // prefetch, which is negligible for the PK types Djogi ships.
        let parent_pks_for_loader: Vec<Box<dyn Any + Send + Sync>> = parent_pks
            .iter()
            .map(|pk_box| {
                let pk: &T::Pk = pk_box
                    .downcast_ref::<T::Pk>()
                    .expect("extract_parent_pks box is T::Pk — framework invariant");
                Box::new(pk.clone()) as Box<dyn Any + Send + Sync>
            })
            .collect();

        // Re-borrow the context per loader iteration — each call hands
        // the mutable reference back once the fetch completes, so the
        // next prefetch can take a fresh borrow without lifetime
        // overlap. Works transparently for both pool-backed and
        // transaction-backed contexts.
        let aligned = (prefetch.loader)(
            &mut *exec,
            prefetch.parent_table,
            prefetch.source_column,
            parent_pks_for_loader,
        )
        .await?;

        per_path_results.push((prefetch.source_column, aligned));
    }

    // Zip per-path aligned results back into per-row `HashMap`s.
    let mut out: Vec<PrefetchedRow<T>> = Vec::with_capacity(rows.len());
    for (i, row) in rows.into_iter().enumerate() {
        let mut relations: HashMap<&'static str, Box<dyn Any + Send + Sync>> =
            HashMap::with_capacity(per_path_results.len());
        for (source_column, aligned) in per_path_results.iter_mut() {
            // `aligned[i]` is the slot for row `i`. Take it out of the
            // vector so each boxed target is moved into its row exactly
            // once — no cloning of the outer Box, no shared ownership.
            if let Some(slot) = aligned.get_mut(i)
                && let Some(boxed_target) = slot.take()
            {
                relations.insert(*source_column, boxed_target);
            }
        }
        out.push(PrefetchedRow::new(row, relations));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Unit tests — exercise the wrapper in isolation. Live-Postgres integration
// coverage lives in `tests/integration/relations.rs`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::ModelDescriptor;
    use crate::relation::path::{RelationKind, RelationPath};
    use crate::types::HeerId;
    use std::future::Future;

    // Same stub-Model pattern as the sibling modules in `relation/`.
    #[derive(Debug, Clone)]
    struct Parent;
    #[derive(Debug, Clone)]
    struct Child;

    macro_rules! dummy_model {
        ($ty:ty, $table:literal) => {
            impl crate::model::__sealed::Sealed for $ty {}
            #[allow(clippy::manual_async_fn)]
            impl crate::model::Model for $ty {
                type Pk = HeerId;
                type Fields = ();
                fn table_name() -> &'static str {
                    $table
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
        };
    }
    dummy_model!(Parent, "parents");
    dummy_model!(Child, "children");

    #[test]
    fn prefetched_row_get_returns_target_when_present() {
        // Manually construct a PrefetchedRow with a pre-resolved Child.
        // Confirms the source-column-keyed lookup + downcast round-trip
        // works without touching a real DB.
        let row = Parent;
        let child = Child;
        let mut relations: HashMap<&'static str, Box<dyn Any + Send + Sync>> = HashMap::new();
        relations.insert("child_id", Box::new(child) as Box<dyn Any + Send + Sync>);
        let pr: PrefetchedRow<Parent> = PrefetchedRow::new(row, relations);

        let path: RelationPath<Parent, Child> =
            RelationPath::new("child_id", "children", RelationKind::ForeignKey);
        assert!(pr.get(path).is_some(), "resolved child should be present");
    }

    #[test]
    fn prefetched_row_get_returns_none_when_missing() {
        // No entry for the path's source column -> None. Covers the
        // "prefetch was never registered for this path" case at the
        // lookup layer.
        let pr: PrefetchedRow<Parent> = PrefetchedRow::new(Parent, HashMap::new());
        let path: RelationPath<Parent, Child> =
            RelationPath::new("child_id", "children", RelationKind::ForeignKey);
        assert!(pr.get(path).is_none());
    }

    #[test]
    fn prefetched_row_debug_lists_relation_keys() {
        // Debug output should name the resolved-relation columns so
        // operators can see which prefetches attached at a glance.
        let mut relations: HashMap<&'static str, Box<dyn Any + Send + Sync>> = HashMap::new();
        relations.insert("child_id", Box::new(Child) as Box<dyn Any + Send + Sync>);
        let pr: PrefetchedRow<Parent> = PrefetchedRow::new(Parent, relations);
        let debug = format!("{pr:?}");
        assert!(
            debug.contains("child_id"),
            "Debug should list the resolved relation keys, got: {debug}"
        );
    }

    #[test]
    fn erased_prefetch_debug_omits_loader_fn_pointer() {
        // `ErasedPrefetch::Debug` should print identifying metadata but
        // skip the fn pointer — pointer addresses are non-stable across
        // builds and would make debug output noisy.
        fn stub_loader<'a>(
            _exec: &'a mut ContextInner,
            _parent_table: &'static str,
            _source_column: &'static str,
            _parent_pks: Vec<Box<dyn Any + Send + Sync>>,
        ) -> Pin<Box<dyn Future<Output = Result<AlignedTargets, DjogiError>> + Send + 'a>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        let e = ErasedPrefetch {
            source_column: "child_id",
            parent_table: "parents",
            loader: stub_loader,
        };
        let debug = format!("{e:?}");
        assert!(debug.contains("child_id"));
        assert!(debug.contains("parents"));
        // No fn-pointer leak — the `finish_non_exhaustive` marker
        // renders as `..`. Ordering of `..` after the last field is
        // implementation-detail; just assert no numeric pointer-looking
        // substring slipped through.
        assert!(
            !debug.contains("0x"),
            "fn pointer should not leak into Debug, got: {debug}"
        );
    }
}
