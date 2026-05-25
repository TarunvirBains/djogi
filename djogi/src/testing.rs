//! Test-harness runtime helpers for `#[djogi_test]`.
//!
//! This module provides the per-test-database lifecycle machinery used by the
//! `#[djogi_test]` proc-macro attribute:
//!
//! 1. Connect to the admin database (via `DATABASE_URL`).
//! 2. Create a fresh `djogi_test_<uuid>` database.
//! 3. Run the Phase 0 bootstrap migration via
//!    [`crate::migrate::bootstrap::run_phase_zero`] — installs HeeRanjID
//!    schema, seeds the default node, sets the `heer.node_id` and
//!    `heer.ranj_node_id` GUCs (HeerId and RanjId generators read
//!    separate session variables), and creates any extensions
//!    requested via the `#[djogi_test(extensions = [...])]` attribute
//!    argument.
//! 4. Return a `DjogiContext` backed by a pool pointed at the new database.
//! 5. Drop the database via `teardown_test_db` — called explicitly by the
//!    macro-generated wrapper, both on normal return and after a caught panic.
//!
//! # Substrate
//!
//! Internals use `tokio_postgres` directly (no sqlx) and route the
//! HeeRanjID + extension install through
//! [`crate::migrate::bootstrap`] — the same module the
//! `migrations compose` auto-emit path uses to write Phase 0 to disk
//! and the production `db reset` path replays. Track 0 strategic
//! lockdown: the test harness has NO parallel install path; every test
//! exercises the same bootstrap code adopters hit.
//!
//! # Database name format
//!
//! Each test gets `djogi_test_<uuid-simple>` — 32 hex characters, no
//! hyphens, fully lowercase. The name is always under 63 bytes (Postgres
//! identifier length limit), is safe as a double-quoted identifier, and
//! contains only ASCII alphanumeric characters plus the `djogi_test_` prefix.
//!
//! # Extension name validation
//!
//! Extension names flow from Rust source (attribute arguments) into SQL as
//! quoted identifiers. To keep that path safe even when the attribute is
//! written by hand, `setup_test_db_with_extensions` rejects any name that
//! is not a plain Postgres identifier: ASCII letter or underscore followed
//! by ASCII letters, digits, or underscores, from 1 to 63 bytes total. This
//! matches how Postgres itself tokenizes unquoted identifiers and covers
//! every real extension on PGXN and `contrib/`. Per the Djogi-wide no-regex
//! rule (`docs/spec/decisions.md`), the validator is implemented with
//! stdlib byte primitives only.
//!
//! # Teardown approach
//!
//! The macro-generated wrapper uses `futures::FutureExt::catch_unwind` to
//! intercept panics from the test body, then calls `teardown_test_db` as an
//! ordinary `async` function before resuming the panic. This avoids the
//! "block_on called from async context" panic that a Drop impl would face
//! inside a running Tokio runtime.
//!
//! # Usage
//!
//! This module is always compiled (it depends only on crates that are already
//! in the djogi runtime dep-graph: tokio-postgres, heeranjid, uuid). Only call
//! its functions from test code — the runtime overhead of importing this module
//! in production is negligible, but its entry points are meaningless outside
//! tests. The HMAC installer helper below is additionally gated on
//! `hmac-codec` and is `unsafe` because it mutates process-global
//! environment state; callers must ensure no concurrent environment
//! reads or writes occur process-wide while it runs (or otherwise satisfy
//! the platform-specific stronger requirement). A mutex for only
//! `DJOGI_PRESENTATION_HMAC_KEY` is not sufficient by itself.

#![allow(clippy::disallowed_methods)]

#[cfg(any(test, feature = "testing"))]
pub mod cli;

/// Error returned by the per-visage `assert_derived_parity` inherent
/// method emitted by `#[model]` — Phase 8.5 issue #231.
///
/// The sync per-visage method compares **only** the derived fields
/// between two pre-constructed visages of the same type and returns
/// `DerivedParityError::Drift { visage, field }` on the first
/// mismatch. Framework columns (`id`, `created_at`, `updated_at`)
/// and storage columns are not compared — the lossy `DateTime`
/// nanosecond truncation on round-trip would false-positive every
/// high-precision timestamp regardless of any derived drift, and
/// the helper exists to test the SQL/Rust expression pair, not
/// transport.
///
/// The error type is named `DerivedParityError` (not
/// `DbComputedParityError`) because parity is a property of *both*
/// sides — the SQL-evaluated value and the Rust-evaluated value
/// together — rather than a Postgres-side action. The
/// [`crate::visage::VisageError::DbComputedNullForNonOptional`] /
/// [`crate::visage::VisageError::DbComputedTypeMismatch`] variants
/// mark errors describing Postgres-side outputs specifically; the
/// parity-helper error straddles both sides, so it lands under
/// `Derived*`.
///
/// `#[non_exhaustive]` is intentional — future helper variants that
/// fetch and compare in one call may add their own variants without
/// breaking existing match arms.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum DerivedParityError {
    /// First mismatch detected by `assert_derived_parity`. The
    /// helper short-circuits at the first mismatched derived field;
    /// adopters who want the full diff can call the helper, observe
    /// the field name in the error, and `dbg!(&in_memory, &from_db)`
    /// to inspect.
    #[error("derived field parity drift in `{visage}` at field `{field}`")]
    Drift {
        /// The visage type name (e.g. `"ConsignmentPublic"`).
        visage: &'static str,
        /// The derived field name that mismatched (e.g.
        /// `"facility_site"`).
        field: &'static str,
    },

    /// The async [`assert_derived_parity_fetched`] convenience helper
    /// failed during the fetch step.
    ///
    /// Wraps the underlying [`DjogiError`] — typically
    /// [`DjogiError::Db`] (database / IO surface) or
    /// [`DjogiError::Visage`] (a derived row-decode failure such as
    /// [`crate::visage::VisageError::DbComputedNullForNonOptional`] or
    /// [`crate::visage::VisageError::DbComputedTypeMismatch`]). The
    /// `#[source]` annotation surfaces the inner error in
    /// `std::error::Error::source()` so test runners that walk
    /// `Error::source()` get the full cause chain.
    ///
    /// Only produced by the async helper; the sync per-visage
    /// `assert_derived_parity` inherent method (and the
    /// [`DerivedParity`] trait impl backing it) never returns this
    /// variant.
    #[error("derived parity fetch failed: {source}")]
    Fetch {
        /// The underlying fetch error.
        #[source]
        source: DjogiError,
    },
}

/// Generic parity-comparison trait backing the per-visage
/// `assert_derived_parity` inherent method — Phase 8.5 issue #231.
///
/// The `#[model]` macro emits two parallel surfaces for every
/// generated visage that has at least one derived entry in its scope:
///
/// 1. An **inherent** `pub fn assert_derived_parity` method on the
///    visage struct. This is the ergonomic call-site:
///    `visage.assert_derived_parity(&other)` resolves to the
///    inherent method directly, no trait import required.
/// 2. A **trait impl** `impl DerivedParity for {Visage}` with an
///    identical body. The trait method is reachable from generic
///    code — any helper bounded `where V: DerivedParity` can call
///    `v.assert_derived_parity(&other)` against an unknown visage
///    type.
///
/// Both surfaces share the same body and the same `where Ty:
/// PartialEq` bound per distinct derived type (so the diagnostic
/// for [`E_DJG_VDF_016`] surfaces at the impl block, not at the
/// inner `!=` site).
///
/// [E_DJG_VDF_016]: https://github.com/tarunvir/djogi/blob/main/docs/spec/visage-derived-fields.md#error-taxonomy
///
/// # Seal
///
/// `DerivedParity` is **sealed** — only macro-emitted visages may
/// satisfy it. Adopter crates cannot implement the trait directly
/// because the seal supertrait `__private::DerivedParitySealed`
/// lives in a `#[doc(hidden)]` module. The seal exists for the same
/// reason as [`DjogiVisage`](crate::DjogiVisage)'s
/// `DjogiVisageOf<Self::Model>` supertrait: the trait promise is
/// "this is a macro-emitted projection-aware parity check," and a
/// hand-written impl would defeat the contract.
///
/// # Sync surface
///
/// `DerivedParity` is intentionally **synchronous** — the
/// comparison itself is in-memory. The async convenience that
/// fetches a visage and then delegates to this trait lives at
/// [`assert_derived_parity_fetched`].
///
/// # Visages without derived fields
///
/// Visages with **zero** derived entries in their scope do not get
/// either the inherent method or the trait impl — there is nothing
/// derived to compare, and a no-op impl would obscure that. The
/// macro filters per-scope before emitting either surface.
pub trait DerivedParity: private::DerivedParitySealed {
    /// Compare the derived fields between `self` and `other`. Returns
    /// `Err(DerivedParityError::Drift { ... })` on the first
    /// mismatched derived field; framework columns and storage
    /// columns are never compared.
    ///
    /// # Errors
    ///
    /// Returns `Err(DerivedParityError::Drift)` on first mismatch.
    /// Never returns `DerivedParityError::Fetch` — that variant is
    /// reserved for the async fetching helper
    /// [`assert_derived_parity_fetched`].
    fn assert_derived_parity(&self, other: &Self) -> Result<(), DerivedParityError>;
}

/// Sealed seal-only module for [`DerivedParity`].
///
/// Re-exported through [`::djogi::__private::DerivedParitySealed`]
/// (Phase 8.5 #231 reconciliation) so macro-emitted code can
/// satisfy the seal without naming `djogi::testing::private`
/// directly. Adopter code never touches this module; the
/// `#[doc(hidden)]` keeps it off the published rustdoc surface.
///
/// [`::djogi::__private::DerivedParitySealed`]: crate::__private::DerivedParitySealed
#[doc(hidden)]
pub mod private {
    /// Empty supertrait satisfied only by macro-emitted visages.
    /// Adopter code reaching in is breaking the framework boundary;
    /// the framework reserves the right to evolve the seal shape
    /// without notice. Same convention as `__private::VisageSealed`.
    pub trait DerivedParitySealed {}
}

/// Async convenience helper that fetches a visage via the caller's
/// closure and then delegates to its [`DerivedParity::assert_derived_parity`]
/// impl — Phase 8.5 issue #231.
///
/// This is the recommended shape for integration tests that want to
/// assert in-memory ↔ from-DB parity in one call. The fetch step is
/// async; the comparison is sync (no IO re-introduced); errors from
/// either side surface as [`DerivedParityError`].
///
/// # Why a closure for the fetch
///
/// `VisageQuerySet` is the canonical fetch path, but its entry
/// methods (`V::filter(|f| f.id().eq(pk))`) are emitted as **inherent
/// methods on each visage type** — they are not part of the
/// [`crate::DjogiVisage`] trait. A generic free helper cannot name
/// those entry points directly without lifting them into a trait,
/// which would couple [`crate::DjogiVisage`] (a pure metadata trait)
/// to the queryset machinery. The closure parameter lets the caller
/// build whatever fetch shape it needs — `V::filter(...).fetch_one(ctx)`,
/// `V::limit(1).fetch_one(ctx)`, or even a hand-rolled fetch — and
/// the helper handles the await + error-mapping + delegation
/// uniformly.
///
/// # Generic bounds
///
/// - `V: DerivedParity` — the visage carries the trait impl
///   (macro-emitted automatically when the visage has at least one
///   derived field in scope).
/// - `Fetch: FnOnce() -> Fut` — the fetch closure produces the
///   `Future` lazily; the helper drives the await internally.
/// - `Fut: Future<Output = crate::Result<V>>` — `crate::Result<V>`
///   is the canonical `Result<V, DjogiError>` alias; the closure
///   propagates `DjogiError` directly and the helper lifts it into
///   [`DerivedParityError::Fetch`].
///
/// # Errors
///
/// - [`DerivedParityError::Fetch`] when the fetch closure's future
///   yields `Err`.
/// - [`DerivedParityError::Drift`] when the in-memory and fetched
///   visages disagree on any derived field.
///
/// # Recommended usage
///
/// ```no_run
/// use djogi::prelude::*;
/// use djogi::testing::{DerivedParity, assert_derived_parity_fetched};
///
/// # async fn example<V, M>(
/// #     ctx: &mut DjogiContext,
/// #     in_memory: &V,
/// #     fetch_one: impl FnOnce(&mut DjogiContext)
/// #         -> std::pin::Pin<Box<dyn std::future::Future<Output = djogi::Result<V>> + Send + '_>>,
/// # ) -> Result<(), djogi::testing::DerivedParityError>
/// # where V: DerivedParity {
/// // `fetch_one` typically wraps a `V::filter(|f| f.id().eq(pk)).fetch_one(ctx)` call.
/// assert_derived_parity_fetched(in_memory, || fetch_one(ctx)).await
/// # }
/// ```
///
/// See [`DerivedParity`] for the sync per-visage method this helper
/// delegates to.
pub async fn assert_derived_parity_fetched<V, Fetch, Fut>(
    in_memory: &V,
    fetch: Fetch,
) -> Result<(), DerivedParityError>
where
    V: DerivedParity,
    Fetch: FnOnce() -> Fut,
    Fut: std::future::Future<Output = crate::Result<V>>,
{
    let from_db = fetch()
        .await
        .map_err(|source| DerivedParityError::Fetch { source })?;
    in_memory.assert_derived_parity(&from_db)
}

use std::collections::{BTreeMap, BTreeSet};

use crate::__bypass::RawAccessExt as _;
use crate::apps::AppRegistry;
use crate::descriptor::{EnumDescriptor, ModelDescriptor};
use crate::migrate::diff::{Classification, SchemaOperation};
use crate::migrate::projection::{BucketKey, project_from_iters, rfc3339_now_seconds};
use crate::migrate::schema::{AppliedSchema, SNAPSHOT_FORMAT_VERSION};
use crate::migrate::{MigrationPlan, SegmentKind, diff_bucket_maps, plan_delta};
use crate::pg::pool::DjogiPool;
use crate::{DbError, DjogiContext, DjogiError};
use tokio_postgres::NoTls;
use uuid::Uuid;

/// Cleanup token returned by `setup_test_db`.
///
/// Carries the information needed to drop the per-test database. Passed to
/// `teardown_test_db` by the macro-generated wrapper. Not a RAII guard — the
/// cleanup is explicit and async so it runs cleanly inside the Tokio test
/// runtime without hitting the `block_on`-from-async-context constraint.
pub struct TestDbCleanup {
    /// Admin database URL, used to reconnect for `DROP DATABASE`.
    admin_url: String,
    /// The per-test database name — ASCII alphanumeric + underscore,
    /// always double-quoted in SQL.
    db_name: String,
}

impl TestDbCleanup {
    /// The per-test database name — ASCII alphanumeric + underscore.
    ///
    /// Tests that need to open their own [`DjogiPool`] against the
    /// per-test database (rather than reusing the one inside
    /// [`DjogiContext`]) combine this with [`Self::test_url`] to build
    /// a URL pointing at the same DB the harness provisioned. Useful
    /// for verifying pool-level invariants — `post_connect`
    /// invocation count across multiple checkouts, `max_size`
    /// saturation, `with_client` lifecycle assertions — that need a
    /// pool whose size and hooks are under the test's control.
    pub fn db_name(&self) -> &str {
        &self.db_name
    }

    /// Build a Postgres URL pointing at the per-test database.
    ///
    /// Substitutes [`Self::db_name`] for the admin URL's path
    /// component. Equivalent to the URL the harness used internally
    /// to open the [`DjogiPool`] backing the returned `DjogiContext`.
    pub fn test_url(&self) -> Result<String, DjogiError> {
        replace_db_in_url(&self.admin_url, &self.db_name)
    }
}

/// Set up a fresh per-test database and return the cleanup token + context.
///
/// Convenience wrapper over [`setup_test_db_with_extensions`] for callers
/// that do not need any extra Postgres extensions beyond the HeeRanjID
/// schema. Equivalent to passing an empty extensions slice.
///
/// Called by macro-generated code from `#[djogi_test]`-annotated tests that
/// omit the `extensions = [...]` argument. Also available for direct use
/// from hand-written test harnesses that do not go through the attribute.
///
/// # Errors
///
/// Returns `DjogiError::Db` on all setup failures.
pub async fn setup_test_db() -> Result<(TestDbCleanup, DjogiContext), DjogiError> {
    setup_test_db_with_extensions(&[]).await
}

/// Set up a fresh per-test database, provision extra extensions, and return
/// the cleanup token + context.
///
/// Called by macro-generated code from `#[djogi_test(extensions = [...])]`
/// tests. Also available for direct use from hand-written test harnesses.
///
/// # Steps (Track 0 — bootstrap-routed)
///
/// 1. Read `DATABASE_URL` from the environment (same convention as
///    sqlx::test).
/// 2. Connect to the admin database via `tokio_postgres`.
/// 3. Generate a unique database name `djogi_test_<uuid-simple>` and
///    issue `CREATE DATABASE`.
/// 4. Connect to the new database via `tokio_postgres`.
/// 5. Run [`crate::migrate::bootstrap::run_phase_zero`] against the
///    new connection. This is the SAME bootstrap surface
///    `migrations compose` writes to disk and `db reset` replays —
///    no parallel install path. Phase 0 installs HeeRanjID, every
///    requested extension, and seeds both the `heer.node_id` and
///    `heer.ranj_node_id` GUCs at both the database (`ALTER DATABASE`)
///    and session (`SET`) levels.
/// 6. Open a `DjogiPool` (deadpool-postgres) and return it as a
///    `DjogiContext`.
///
/// # Strategic lockdown invariant
///
/// Pre-Track-0, this function had its own SQL-by-hand install path
/// for HeeRanjID + extensions + node-id GUC. That meant `#[djogi_test]`
/// was the ONLY surface that exercised bootstrap; the production CLI
/// / `db reset` paths were uncovered. Track 0 closes that gap by
/// routing every test through `bootstrap::run_phase_zero` — the same
/// code path adopters hit. There is exactly ONE bootstrap path now.
///
/// # Errors
///
/// Returns `DjogiError::Db` on all setup failures. Failure modes that
/// mention the offending extension by name:
///
/// - The extension name does not match the identifier rule (handled
///   in Rust before any SQL is sent — the validator is shared between
///   this module and `bootstrap`).
/// - The Postgres server rejects `CREATE EXTENSION IF NOT EXISTS` —
///   for example, when the named extension is not installed on the
///   server (missing `.control` file). The original `tokio_postgres::Error`
///   is preserved in the `DjogiError::Db` source chain so the adopter
///   can see the full server message.
pub async fn setup_test_db_with_extensions(
    extensions: &[&str],
) -> Result<(TestDbCleanup, DjogiContext), DjogiError> {
    // Validate extension names up front so we fail before CREATE DATABASE
    // when the attribute author made a typo. This means an invalid name
    // does not leak a stray `djogi_test_<uuid>` database.
    for name in extensions {
        validate_extension_name(name)?;
    }

    // Read DATABASE_URL — same env var convention as #[sqlx::test].
    let database_url = std::env::var("DATABASE_URL").map_err(|_| {
        DjogiError::Db(DbError::other(
            "DATABASE_URL env var is not set; djogi_test requires it to connect to Postgres",
        ))
    })?;

    // Connect to the admin database to issue CREATE DATABASE.
    let (admin_client, admin_conn) = tokio_postgres::connect(&database_url, NoTls)
        .await
        .map_err(|e| DjogiError::Db(DbError::other(format!("admin connect failed: {e}"))))?;

    // Spawn the connection driver — must be running while admin_client is alive.
    tokio::spawn(async move {
        if let Err(e) = admin_conn.await {
            eprintln!("[djogi_test] admin connection error: {e}");
        }
    });

    // Generate a unique database name: djogi_test_ + 32 hex chars (UUID v4,
    // simple format, no hyphens). Always fits in 63 bytes.
    let unique_suffix = Uuid::new_v4().simple().to_string();
    let db_name = format!("djogi_test_{unique_suffix}");

    // CREATE DATABASE — double-quoted to handle any future non-alphanumeric
    // characters in the prefix (currently not possible, but defensive).
    let create_sql = format!("CREATE DATABASE {}", quoted(&db_name));
    admin_client
        .batch_execute(&create_sql)
        .await
        .map_err(|e| DjogiError::Db(DbError::other(format!("CREATE DATABASE failed: {e}"))))?;

    // Build the per-test database URL by replacing the database component.
    let test_url = replace_db_in_url(&database_url, &db_name)?;

    // Drop the admin client — Phase 0 runs against the per-test DB,
    // not the admin DB.
    drop(admin_client);

    // Connect to the fresh database for Phase 0 bootstrap.
    let (test_client, test_conn) = tokio_postgres::connect(&test_url, NoTls)
        .await
        .map_err(|e| DjogiError::Db(DbError::other(format!("test DB connect failed: {e}"))))?;

    tokio::spawn(async move {
        if let Err(e) = test_conn.await {
            eprintln!("[djogi_test] test connection error: {e}");
        }
    });

    // Track 0: route through `bootstrap::run_phase_zero` — the SAME
    // bootstrap surface adopters hit via `migrations compose` +
    // `migrations apply` + `db reset`. This is the strategic lockdown
    // — no parallel install path lives in `testing` anymore.
    let extension_set: std::collections::BTreeSet<String> =
        extensions.iter().map(|s| s.to_string()).collect();
    crate::migrate::bootstrap::run_phase_zero(
        &test_client,
        &db_name,
        &extension_set,
        crate::migrate::bootstrap::DEFAULT_NODE_ID,
    )
    .await
    .map_err(|e| DjogiError::Db(DbError::other(format!("phase 0 bootstrap failed: {e}"))))?;

    // The setup client is dropped here — the connection driver task
    // will finish. Phase 0's `ALTER DATABASE ... SET heer.node_id`
    // and the matching `heer.ranj_node_id` write persist for every
    // NEW connection the DjogiPool opens.
    drop(test_client);

    // Build the DjogiPool (tokio-postgres / deadpool) for the app context.
    let app_pool = DjogiPool::connect(&test_url).await?;
    let ctx = DjogiContext::from_pool(app_pool);

    let cleanup = TestDbCleanup {
        admin_url: database_url,
        db_name,
    };

    Ok((cleanup, ctx))
}

/// Drop the per-test database created by `setup_test_db`.
///
/// Called by macro-generated code after the test body returns — whether
/// normally or via a caught panic.
pub async fn teardown_test_db(cleanup: TestDbCleanup) {
    let TestDbCleanup { admin_url, db_name } = cleanup;

    // Reconnect to admin database to issue DROP DATABASE.
    match tokio_postgres::connect(&admin_url, NoTls).await {
        Err(e) => {
            eprintln!(
                "[djogi_test] WARNING: failed to connect to admin DB for teardown \
                 (database \"{db_name}\" may need manual cleanup): {e}"
            );
        }
        Ok((admin_client, admin_conn)) => {
            tokio::spawn(async move {
                if let Err(e) = admin_conn.await {
                    eprintln!("[djogi_test] teardown connection error: {e}");
                }
            });

            let sql = format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", quoted(&db_name));
            if let Err(e) = admin_client.batch_execute(&sql).await {
                eprintln!("[djogi_test] WARNING: failed to drop test database \"{db_name}\": {e}");
            }
        }
    }
}

/// Cluster-wide non-superuser role used by RLS-backed integration tests.
///
/// Postgres roles are cluster-scoped (not per-database); the same role is
/// reused across every per-test database. The idempotent CREATE/ALTER
/// pattern in [`connect_test_db_as_non_superuser`] keeps the role's
/// attribute shape consistent across runs.
///
/// The literal name is shaped like a Postgres unquoted identifier so the
/// SQL emitter can rely on the standard double-quoting path
/// ([`quoted`]) without special-casing it.
pub const TEST_NON_SUPERUSER_ROLE: &str = "djogi_test_user";

// djogi-allow-secret: the URL shape in the following rustdoc is a doc-shaped
// illustration of a local-cluster non-superuser, not a real credential.
/// Password literal for [`TEST_NON_SUPERUSER_ROLE`].
///
/// Bound at compile time and only ever used inside the local test cluster
/// — the URL emitted by `connect_test_db_as_non_superuser` follows the
/// shape `postgres://djogi_test_user:djogi_test_user@<host>/<per-test-db>`,
/// which `pg_hba.conf` for development clusters typically accepts via
/// `trust` or `md5`. The shared helpers (`quoted`, `sql_string_literal`)
/// handle the SQL identifier / literal escaping so the constant body
/// itself never needs to be sanitised at the call site.
pub const TEST_NON_SUPERUSER_PASSWORD: &str = "djogi_test_user";

/// Open a [`DjogiContext`] backed by a pool that authenticates as a
/// non-superuser cluster role on the per-test database carried by
/// `cleanup`.
///
/// This is the integration-test escape hatch for RLS-backed coverage:
/// the default `djogi_test` harness opens its pool as the connecting
/// role from `DATABASE_URL`, which in local and CI clusters is
/// usually a Postgres superuser. Superusers unconditionally bypass
/// row security regardless of `FORCE ROW LEVEL SECURITY`, so RLS
/// policies can pass for the wrong reason (every row visible looks
/// like every row hidden when zero rows match the policy). This
/// helper returns a fresh [`DjogiContext`] backed by a pool whose
/// physical connections authenticate as
/// [`TEST_NON_SUPERUSER_ROLE`] — a `LOGIN NOSUPERUSER NOCREATEDB
/// NOCREATEROLE NOREPLICATION NOBYPASSRLS` role — so RLS is
/// observable end-to-end through the typed surface.
///
/// # Lifecycle
///
/// 1. Connect to the admin database via `cleanup`'s admin URL.
/// 2. Idempotently `CREATE ROLE …` (or `ALTER ROLE …` if it pre-exists)
///    so attributes always match the intended shape, even if a previous
///    process drifted them.
/// 3. Reconnect to the per-test database (same admin URL with the path
///    component swapped to `cleanup.db_name()`) and grant the role
///    access to every existing object in `public`:
///    - `GRANT CONNECT ON DATABASE …`
///    - `GRANT USAGE ON SCHEMA public`
///    - `GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public`
///    - `GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA public`
///    - `GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA public`
/// 4. Build a non-superuser URL pointing at the same per-test database
///    and open a fresh [`DjogiPool`].
/// 5. Wrap the pool in a new [`DjogiContext::from_pool`] and return it.
///
/// # Ordering requirement
///
/// The grants in step 3 only cover objects that already exist when they
/// run. Callers MUST invoke this helper **after** schema and seed setup
/// has finished on the admin context — typically after
/// `djogi::testing::sync_models(...)`, the test's RLS DDL bootstrap, and
/// any rows the admin connection seeds before RLS becomes observable.
/// Adding new tables / sequences / functions to `public` after this
/// helper returns leaves the non-superuser without privilege on those
/// objects (a regression test framework can add them, but the new
/// objects need their own GRANT round).
///
/// # Returned context
///
/// The returned [`DjogiContext`] has its **own** `Arc<sassi::Sassi>`
/// registry (built from the global `inventory` walk in
/// [`DjogiContext::from_pool`]). Cache and refresh tests should use
/// `non_super_ctx.punnu::<T>()` and `non_super_ctx.share_pool()` so the
/// cache target and the fetcher pool share a consistent ancestry —
/// crossing punnus between the admin and non-superuser contexts is
/// allowed at the type level but loses the lifecycle invariants the
/// per-context cluster guard pins.
///
/// # Errors
///
/// Returns [`DjogiError::Db`] when the admin connection fails, when
/// any of the role-management or grant statements fail, or when the
/// derived non-superuser URL cannot be parsed back into a pool.
pub async fn connect_test_db_as_non_superuser(
    cleanup: &TestDbCleanup,
) -> Result<DjogiContext, DjogiError> {
    // ── Step 1 — open the admin connection for cluster-level role work ─────
    let (admin_client, admin_conn) = tokio_postgres::connect(&cleanup.admin_url, NoTls)
        .await
        .map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "non-superuser admin connect failed: {e}"
            )))
        })?;
    tokio::spawn(async move {
        if let Err(e) = admin_conn.await {
            eprintln!("[djogi_test] non-superuser admin connection error: {e}");
        }
    });

    // ── Step 2 — idempotent role bootstrap ──────────────────────────────────
    // The `pg_roles` lookup uses parameter binding (no SQL composition with
    // user-controlled data). The role / password constants are compile-time
    // ASCII identifiers, but we still route them through the dedicated
    // identifier and string-literal escapers (`quoted` /
    // `sql_string_literal`) so the SQL composition stays robust if anyone
    // later widens the constants.
    let role_exists: bool = admin_client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)",
            &[&TEST_NON_SUPERUSER_ROLE],
        )
        .await
        .map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "non-superuser role lookup failed: {e}"
            )))
        })?
        .get(0);

    let role_quoted = quoted(TEST_NON_SUPERUSER_ROLE);
    let password_literal = sql_string_literal(TEST_NON_SUPERUSER_PASSWORD);
    let role_sql = if role_exists {
        // Re-assert attributes + password: a prior test process may have
        // ALTERed the role into a different shape. Setting NOSUPERUSER
        // NOBYPASSRLS NOCREATEDB NOCREATEROLE NOREPLICATION explicitly
        // closes the bypass surface even if some external tooling tried to
        // promote the role.
        format!(
            "ALTER ROLE {role_quoted} WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE \
             NOREPLICATION NOBYPASSRLS PASSWORD {password_literal}"
        )
    } else {
        format!(
            "CREATE ROLE {role_quoted} WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE \
             NOREPLICATION NOBYPASSRLS PASSWORD {password_literal}"
        )
    };
    admin_client.batch_execute(&role_sql).await.map_err(|e| {
        DjogiError::Db(DbError::other(format!(
            "non-superuser role provisioning failed: {e}"
        )))
    })?;
    drop(admin_client);

    // ── Step 3 — connect to the per-test DB and grant access ────────────────
    let test_admin_url = replace_db_in_url(&cleanup.admin_url, &cleanup.db_name)?;
    let (test_admin_client, test_admin_conn) = tokio_postgres::connect(&test_admin_url, NoTls)
        .await
        .map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "non-superuser test-db admin connect failed: {e}"
            )))
        })?;
    tokio::spawn(async move {
        if let Err(e) = test_admin_conn.await {
            eprintln!("[djogi_test] non-superuser test-db admin connection error: {e}");
        }
    });

    let db_quoted = quoted(&cleanup.db_name);
    let grants_sql = format!(
        "GRANT CONNECT ON DATABASE {db_quoted} TO {role_quoted};\n\
         GRANT USAGE ON SCHEMA public TO {role_quoted};\n\
         GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO {role_quoted};\n\
         GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA public TO {role_quoted};\n\
         GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA public TO {role_quoted};"
    );
    test_admin_client
        .batch_execute(&grants_sql)
        .await
        .map_err(|e| DjogiError::Db(DbError::other(format!("non-superuser grants failed: {e}"))))?;
    drop(test_admin_client);

    // ── Step 4 — open a non-superuser pool against the per-test database ────
    let non_super_url = build_non_superuser_url(&cleanup.admin_url, &cleanup.db_name)?;
    let pool = DjogiPool::connect(&non_super_url).await?;
    Ok(DjogiContext::from_pool(pool))
}

/// Build a Postgres connection URL whose userinfo and database components
/// are swapped for the non-superuser test role + the per-test database
/// name, while preserving the original URL's host, port, and non-identity
/// query parameters.
///
/// The `admin_url` is parsed via the same byte-level scheme the rest of
/// the testing substrate uses (`postgres://` or `postgresql://`,
/// optional `user[:pass]@` userinfo, host[:port], `/database`, optional
/// `?query`). The userinfo is stripped and replaced with
/// `djogi_test_user:djogi_test_user`; the path component is replaced with
/// `db_name`; and query parameters that would override the authenticated
/// user, password, or database (`user`, `password`, `dbname`) are rejected.
///
/// # Errors
///
/// Returns [`DjogiError::Db`] when `admin_url` lacks the `postgres://`
/// or `postgresql://` scheme, when it lacks a `/database` path component
/// (the same shape `replace_db_in_url` rejects), or when its query string
/// contains `user`, `password`, or `dbname` parameters. `tokio-postgres`
/// applies those URL query parameters after parsing authority/path pieces,
/// so preserving them would let a legal admin URL silently override the
/// non-superuser role this helper is supposed to guarantee.
fn build_non_superuser_url(admin_url: &str, db_name: &str) -> Result<String, DjogiError> {
    let body = admin_url
        .strip_prefix("postgres://")
        .or_else(|| admin_url.strip_prefix("postgresql://"))
        .ok_or_else(|| {
            DjogiError::Db(DbError::other(
                "DATABASE_URL must start with `postgres://` or `postgresql://` to derive \
                 the non-superuser test URL",
            ))
        })?;
    let scheme = if admin_url.starts_with("postgres://") {
        "postgres://"
    } else {
        "postgresql://"
    };

    // Walk the body bytes once: authority before the first `/`, path until
    // the first `?`, query string from `?` onward.
    let body_bytes = body.as_bytes();
    let mut idx = 0usize;
    while idx < body_bytes.len() && body_bytes[idx] != b'/' {
        idx += 1;
    }
    if idx >= body_bytes.len() {
        return Err(DjogiError::Db(DbError::other(
            "DATABASE_URL does not contain a database name component; \
             cannot derive the non-superuser test URL",
        )));
    }
    let authority = &body[..idx];
    // Strip any `user[:pass]@` prefix from the authority — the test URL
    // always emits `<role>:<password>@host:port`. Use `rfind('@')` to
    // tolerate '@' inside a password (rare, but matches the rest of
    // libpq-shaped URL parsing in the codebase).
    let host_and_port = match authority.rfind('@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    };

    let path_start = idx + 1;
    let mut path_end = path_start;
    while path_end < body_bytes.len() && body_bytes[path_end] != b'?' {
        path_end += 1;
    }
    let trailing = &body[path_end..]; // includes leading `?` when present.
    reject_identity_overriding_query_params(trailing)?;

    Ok(format!(
        "{scheme}{role}:{password}@{host_and_port}/{db_name}{trailing}",
        role = TEST_NON_SUPERUSER_ROLE,
        password = TEST_NON_SUPERUSER_PASSWORD,
    ))
}

fn reject_identity_overriding_query_params(query_with_marker: &str) -> Result<(), DjogiError> {
    let Some(query) = query_with_marker.strip_prefix('?') else {
        return Ok(());
    };

    for pair in query.split('&') {
        let key = pair.split_once('=').map_or(pair, |(key, _)| key);
        let key = percent_decode_query_key(key)?;
        if matches!(key.as_str(), "user" | "password" | "dbname") {
            return Err(DjogiError::Db(DbError::other(format!(
                "DATABASE_URL query parameter `{key}` would override the non-superuser \
                 test role or per-test database; remove it before calling \
                 connect_test_db_as_non_superuser",
            ))));
        }
    }

    Ok(())
}

fn percent_decode_query_key(key: &str) -> Result<String, DjogiError> {
    let mut out = Vec::with_capacity(key.len());
    let bytes = key.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(DjogiError::Db(DbError::other(
                    "DATABASE_URL query parameter contains an incomplete percent escape",
                )));
            }
            let hi = hex_value(bytes[index + 1]).ok_or_else(|| {
                DjogiError::Db(DbError::other(
                    "DATABASE_URL query parameter contains an invalid percent escape",
                ))
            })?;
            let lo = hex_value(bytes[index + 2]).ok_or_else(|| {
                DjogiError::Db(DbError::other(
                    "DATABASE_URL query parameter contains an invalid percent escape",
                ))
            })?;
            out.push((hi << 4) | lo);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }

    String::from_utf8(out).map_err(|e| {
        DjogiError::Db(DbError::other(format!(
            "DATABASE_URL query parameter key is not valid UTF-8 after percent decoding: {e}",
        )))
    })
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Quote a Postgres string literal for embedding in a DDL statement.
///
/// Postgres uses single-quoted string literals with `''` as the embedded
/// single-quote escape. This helper mirrors [`quoted`] (which handles
/// identifier double-quoting) for the literal side: most call sites pass
/// controlled constants, but emitting the escape here keeps the call
/// sites uniform and future-proof if those constants ever broaden to
/// include `'` bytes.
///
/// `E'...'` is intentionally not used — backslash escapes are off by
/// default on modern Postgres (`standard_conforming_strings = on`),
/// so the single-quote-doubling escape is sufficient and keeps the
/// emitted SQL portable across `standard_conforming_strings` settings.
fn sql_string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push('\'');
            out.push('\'');
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// List every database whose name matches the `djogi_test_*` prefix that is
/// still present on the cluster connected to via `admin_url`.
///
/// Read-only counterpart of [`cleanup_orphaned_test_databases`]. Used by
/// `djogi db cleanup-test-dbs --dry-run` to surface candidates without
/// dropping. The candidate set comes straight from `pg_database` — no
/// filesystem scan, no regex — and is returned in lexicographic order.
///
/// # Errors
///
/// Returns [`DjogiError::Db`] when the admin connection itself fails or
/// the `pg_database` query fails.
pub async fn list_orphaned_test_databases(admin_url: &str) -> Result<Vec<String>, DjogiError> {
    let (client, conn) = tokio_postgres::connect(admin_url, NoTls)
        .await
        .map_err(|e| DjogiError::Db(DbError::other(format!("admin connect failed: {e}"))))?;

    tokio::spawn(async move {
        if let Err(e) = conn.await {
            eprintln!("[djogi_test] list connection error: {e}");
        }
    });

    let rows = client
        .query(
            "SELECT datname FROM pg_database \
             WHERE datname LIKE 'djogi\\_test\\_%' ESCAPE '\\' \
             ORDER BY datname",
            &[],
        )
        .await
        .map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "pg_database query failed during orphan listing: {e}"
            )))
        })?;

    let mut names = Vec::with_capacity(rows.len());
    for row in &rows {
        match row.try_get::<_, String>(0) {
            Ok(name) => names.push(name),
            Err(e) => {
                eprintln!(
                    "[djogi_test] WARNING: failed to decode datname during orphan listing: {e}"
                );
            }
        }
    }
    Ok(names)
}

/// Drop every database whose name matches the `djogi_test_*` prefix that is
/// still present on the cluster connected to via `admin_url`.
///
/// Orphaned test databases accumulate when test processes are killed before
/// `teardown_test_db` can run (e.g. `cargo test` killed mid-run via Ctrl+C,
/// CI timeout, OOM). This helper provides a sweep for that common case.
///
/// **Scope.** Queries `pg_database` for `datname LIKE 'djogi\_test\_%' ESCAPE '\'`
/// (the underscores are escaped because `_` is a single-character LIKE
/// wildcard) and issues `DROP DATABASE … WITH (FORCE)` for each. The
/// `WITH (FORCE)` clause
/// (Postgres 13+; required by Djogi's ≥ Postgres 18 target) terminates any
/// lingering connections before dropping.
///
/// **No filesystem scan.** The cleanup is driven entirely from `pg_database`
/// — no directory listing, no regex.
///
/// Returns the **lexicographically sorted names** of databases that were
/// successfully dropped. A failure to drop an individual database is logged
/// via `eprintln!` and that name is omitted from the returned vector
/// (best-effort, non-fatal). Callers wanting the count call `.len()`.
///
/// # Errors
///
/// Returns `DjogiError::Db` when the admin connection itself fails or the
/// `pg_database` query fails. Per-database DROP failures are not propagated
/// — the caller sees the successful subset and can retry if needed.
///
/// # Note — not tested at unit level
///
/// This function requires a live Postgres cluster. Unit-level testing would
/// require a live connection; that coverage belongs in an integration test.
/// Added but untested at unit level — a future integration-test pass should
/// cover it.
pub async fn cleanup_orphaned_test_databases(admin_url: &str) -> Result<Vec<String>, DjogiError> {
    let (client, conn) = tokio_postgres::connect(admin_url, NoTls)
        .await
        .map_err(|e| DjogiError::Db(DbError::other(format!("admin connect failed: {e}"))))?;

    tokio::spawn(async move {
        if let Err(e) = conn.await {
            eprintln!("[djogi_test] cleanup connection error: {e}");
        }
    });

    // Query pg_database for every `djogi_test_*` database. The `_`
    // byte is a single-character wildcard in `LIKE`, so the prefix
    // matcher must escape the literal underscores via `ESCAPE '\'`
    // — otherwise a database named e.g. `djogiXtestY<anything>`
    // would match the pattern and be considered an orphaned test DB.
    let rows = client
        .query(
            "SELECT datname FROM pg_database \
             WHERE datname LIKE 'djogi\\_test\\_%' ESCAPE '\\' \
             ORDER BY datname",
            &[],
        )
        .await
        .map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "pg_database query failed during orphan cleanup: {e}"
            )))
        })?;

    let mut dropped = Vec::with_capacity(rows.len());
    for row in &rows {
        let datname: String = match row.try_get(0) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "[djogi_test] WARNING: failed to decode datname during orphan cleanup: {e}"
                );
                continue;
            }
        };
        let sql = format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", quoted(&datname));
        match client.batch_execute(&sql).await {
            Ok(()) => dropped.push(datname),
            Err(e) => {
                eprintln!(
                    "[djogi_test] WARNING: failed to drop orphaned test database \
                     \"{datname}\": {e}"
                );
            }
        }
    }

    Ok(dropped)
}

/// Double-quote a Postgres identifier for embedding in SQL.
///
/// Wraps `ident` in double quotes and escapes any embedded `"` byte
/// by doubling it, matching Postgres's identifier-quoting rules.
/// Defense-in-depth: while most call sites pass identifiers that are
/// pre-validated by [`validate_extension_name`] (extensions) or
/// composed locally from controlled inputs (test database names),
/// the orphan-cleanup path reads `datname` values straight from
/// `pg_database`, which can include any quoted identifier the cluster
/// has accepted. Doubling `"` here keeps every embedding site safe
/// regardless of upstream validation strength.
fn quoted(ident: &str) -> String {
    let mut out = String::with_capacity(ident.len() + 2);
    out.push('"');
    for b in ident.bytes() {
        if b == b'"' {
            out.push('"');
            out.push('"');
        } else {
            out.push(b as char);
        }
    }
    out.push('"');
    out
}

/// Validate that `name` is a plain Postgres identifier safe to interpolate
/// into a `CREATE EXTENSION IF NOT EXISTS "<name>"` statement.
///
/// Rules (byte-level, no regex per the Djogi-wide no-regex policy):
///
/// - Length between 1 and 63 bytes inclusive (Postgres `NAMEDATALEN` minus
///   the trailing `NUL`).
/// - First byte is an ASCII letter (upper- or lower-case) or underscore.
/// - Every subsequent byte is an ASCII letter, digit, or underscore.
///
/// All real-world extension names on PGXN and in `contrib/` (`postgis`,
/// `pg_trgm`, `pgcrypto`, `uuid-ossp` — wait, that last one contains a
/// hyphen, but it is itself aliased to `"uuid-ossp"` double-quoted) match
/// this rule with one caveat: extensions whose names require double
/// quoting are rejected here. The caveat is documented on the
/// [`setup_test_db_with_extensions`] entry point.
fn validate_extension_name(name: &str) -> Result<(), DjogiError> {
    let bytes = name.as_bytes();

    if bytes.is_empty() {
        return Err(DjogiError::Db(DbError::other(
            "djogi_test: extension name must not be empty",
        )));
    }
    if bytes.len() > 63 {
        return Err(DjogiError::Db(DbError::other(format!(
            "djogi_test: extension name `{name}` exceeds 63-byte Postgres identifier limit",
        ))));
    }

    let first = bytes[0];
    let first_ok = first.is_ascii_alphabetic() || first == b'_';
    if !first_ok {
        return Err(DjogiError::Db(DbError::other(format!(
            "djogi_test: extension name `{name}` must start with an ASCII letter or underscore",
        ))));
    }

    for &b in &bytes[1..] {
        let ok = b.is_ascii_alphanumeric() || b == b'_';
        if !ok {
            return Err(DjogiError::Db(DbError::other(format!(
                "djogi_test: extension name `{name}` contains invalid byte `{c}` \
                 (only ASCII letters, digits, and underscores are allowed)",
                c = b as char,
            ))));
        }
    }

    Ok(())
}

/// Replace the database name component in a Postgres connection URL.
///
/// Delegates to [`crate::migrate::reset::replace_db_in_url`] (the canonical
/// implementation) and wraps the `Option` return in a `DjogiError` so the
/// caller can use `?`.
fn replace_db_in_url(url: &str, new_db: &str) -> Result<String, DjogiError> {
    crate::migrate::reset::replace_db_in_url(url, new_db).ok_or_else(|| {
        DjogiError::Db(DbError::other(
            "DATABASE_URL does not contain a database name component; \
             cannot derive the per-test database URL",
        ))
    })
}

/// Phase 7 T10 — auto-materialise the supplied descriptors on the
/// per-test database (closes issue #18).
///
/// Called by macro-generated code from
/// `#[djogi_test(sync_models = [Type1, Type2, ...])]` after
/// [`setup_test_db_with_extensions`] returns. The macro lowers each
/// `Type` to `<Type as ::djogi::Model>::descriptor()`, so the
/// descriptors arrive here with full `&'static` lifetimes from the
/// `inventory` collectors.
///
/// # What this does
///
/// 1. Walks `descriptors`, derives the `(database, app)` bucket each
///    one belongs in (resolved via [`AppRegistry::all`]).
/// 2. Projects the descriptors into per-bucket [`AppliedSchema`]
///    values via [`project_from_iters`] — same projection layer that
///    production migrations and `compose` use, so any new field type
///    or index annotation propagates automatically.
/// 3. Diffs the projection against an empty per-bucket map via
///    [`diff_bucket_maps`] — every operation is therefore additive
///    (`AddTable`, `AddIndex`, `AddEnum`, `AddForeignKey`).
/// 4. Validates that every FK target table is also present in the
///    same `sync_models` set — a missing target produces a clear
///    runtime error naming the referencing column AND the missing
///    target. Macro-time detection is impossible (only type paths are
///    visible; descriptors are runtime data).
/// 5. Asserts the delta is additive only. If the differ ever
///    produces a destructive op against an empty target the
///    invariant is broken — error rather than silently executing it.
/// 6. Plans each per-bucket delta via [`plan_delta`] (T3's segment
///    planner), then executes every statement via
///    [`DjogiContext::raw_ddl`] in segment + statement order.
///
/// # No advisory lock, no ledger
///
/// Per the v3 plan rationale: per-test databases are ephemeral and
/// have no concurrent writers, so the `apply` orchestration layer
/// (T4 — advisory lock + ledger insertion + snapshot persistence) is
/// intentionally bypassed. The composition primitives (T1 + T2 + T3)
/// remain shared with production.
///
/// # Pre-flight registry gate (GH #158)
///
/// Before step 1 the helper invokes
/// [`crate::relation::registry::validate_global_relation_accessor_registry`]
/// to catch cross-kind reverse / M2M accessor collisions that rustc
/// cannot see. Per-test sync therefore fails loudly with the offending
/// source, accessor, kind, target, and via metadata rather than at a
/// downstream "ambiguous method call" call site — the gate runs whether
/// or not the offending models happen to be in the `sync_models = [...]`
/// set, since the underlying inventory is link-time-global.
///
/// # Bucket ordering
///
/// Buckets are walked in [`BucketKey`] order
/// (`(database, app)` pair, alphabetically). A multi-bucket
/// `sync_models` call therefore applies its DDL in a deterministic,
/// reproducible order across runs.
///
/// # Errors
///
/// Returns [`DjogiError::Db`] for:
/// - Projection failures (unknown app label, duplicate type name,
///   cross-database FK).
/// - SQL emission errors from the migration engine
///   (`Unsupported`, `PkTypeFlipMustRouteToT9` — neither should occur
///   on an empty-target diff in practice).
/// - FK-to-missing-model violations.
/// - Invariant-violation classification (any non-additive op).
/// - Statement-execution failures from `ctx.raw_ddl`.
///
/// All paths preserve the per-test DB drop in the macro-generated
/// wrapper: `sync_models` failures bubble out as `Err`, the macro's
/// `.expect("djogi_test: failed to sync_models on per-test database")`
/// turns that into a panic, and the wrapper's `catch_unwind` +
/// teardown sequence still runs to drop the throwaway database.
pub async fn sync_models(
    ctx: &mut DjogiContext,
    descriptors: &[&'static ModelDescriptor],
) -> Result<(), DjogiError> {
    let plans = build_sync_plans(descriptors)?;
    for plan in &plans {
        execute_plan(ctx, plan).await?;
    }
    Ok(())
}

/// Install the narrow trigger fixture used by the phase4 `save()` integration
/// test.
///
/// This is intentionally not a general-purpose SQL fixture loader. The test
/// body needs to prove that `Model::save` rehydrates trigger-mutated fields
/// through `UPDATE ... RETURNING *`, but creating a Postgres trigger is schema
/// setup rather than typed Djogi behavior. Keeping the raw DDL inside the
/// framework test harness avoids reopening raw SQL escape hatches in ordinary
/// integration tests.
#[doc(hidden)]
pub async fn install_accounts_balance_increment_trigger_for_test(
    ctx: &mut DjogiContext,
) -> Result<(), DjogiError> {
    ctx.raw_ddl(
        "CREATE OR REPLACE FUNCTION accounts_balance_increment_trigger() \
         RETURNS trigger AS $$ \
         BEGIN NEW.balance := NEW.balance + 1; RETURN NEW; END; \
         $$ LANGUAGE plpgsql;",
    )
    .await?;
    ctx.raw_ddl("DROP TRIGGER IF EXISTS t_accounts_balance_increment ON accounts;")
        .await?;
    ctx.raw_ddl(
        "CREATE TRIGGER t_accounts_balance_increment \
         BEFORE UPDATE ON accounts \
         FOR EACH ROW EXECUTE FUNCTION accounts_balance_increment_trigger();",
    )
    .await
}

/// Install a presentation HMAC key for tests that exercise HMAC codecs.
///
/// Sets `DJOGI_PRESENTATION_HMAC_KEY` to the given hex string and calls
/// [`validate_startup_inventory`](crate::presentation::validate_startup_inventory)
/// to prime the key cache. Call this only from a harness that also keeps
/// concurrent environment reads and writes quiescent process-wide (or
/// otherwise satisfies the platform-specific stronger requirement); a
/// mutex that only serializes `DJOGI_PRESENTATION_HMAC_KEY` is not enough
/// by itself.
///
/// # Panics
///
/// Panics if `key` fails startup validation (wrong length, non-lowercase,
/// non-hex characters, etc.). This is intentional: a malformed test key
/// is a test bug, not a runtime condition the caller should handle.
///
/// # Safety
///
/// The `set_var` call is in an `unsafe` block because the process
/// environment is shared global state. Callers must ensure there are no
/// concurrent environment reads or writes process-wide while this function
/// runs (or otherwise satisfy the platform-specific stronger requirement);
/// a mutex for only `DJOGI_PRESENTATION_HMAC_KEY` is not sufficient by
/// itself.
#[cfg(feature = "hmac-codec")]
#[doc(hidden)]
pub unsafe fn install_presentation_hmac_key_for_testing(key: &str) {
    // SAFETY: caller has ensured the process-wide env-mutation invariant
    // required by std::env::set_var, not just key-local serialization.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("DJOGI_PRESENTATION_HMAC_KEY", key);
    }
    crate::presentation::validate_startup_inventory()
        .expect("install_presentation_hmac_key_for_testing: key validation failed");
}

/// Minimal outbox row shape used by integration tests.
///
/// Runtime outbox tables are framework-owned tables, not ordinary
/// `#[model]` tables: they carry `id` and `created_at`, but intentionally do
/// not carry the usual `updated_at` framework column. Tests that need to
/// inspect outbox rows should use this helper shape instead of declaring a
/// fake model for `{table}_outbox`.
#[derive(Debug, Clone)]
#[doc(hidden)]
pub struct OutboxRowForTest {
    /// Primary key of the outbox row itself.
    pub id: crate::types::HeerId,
    /// Primary key of the source row, decoded as text to match worker APIs.
    pub row_id: String,
    /// CRUD action: `create`, `save`, or `delete`.
    pub action: String,
    /// Serialized source-row payload after `outbox = "ignore"` filtering.
    pub payload: serde_json::Value,
    /// Worker state, usually `pending` immediately after emission.
    pub state: String,
}

fn validate_outbox_table_for_test(table: &str) -> Result<(), DjogiError> {
    crate::ident::check_user_supplied_ident(table, false).map_err(|error| {
        DjogiError::Db(DbError::other(format!(
            "invalid outbox table name {table:?}: {error:?}"
        )))
    })
}

/// Read all rows from a framework-owned outbox table for integration tests.
#[doc(hidden)]
pub async fn outbox_rows_for_test(
    ctx: &mut DjogiContext,
    table: &str,
) -> Result<Vec<OutboxRowForTest>, DjogiError> {
    validate_outbox_table_for_test(table)?;
    let sql = format!(
        "SELECT id, row_id::text, action, payload, state \
         FROM {table} \
         ORDER BY created_at, id"
    );
    let rows = ctx.raw_rows(&sql, &[]).await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id_raw: i64 = row
            .try_get(0)
            .map_err(|e| DjogiError::Db(DbError::other(format!("outbox id decode: {e}"))))?;
        let id = crate::types::HeerId::from_i64(id_raw).map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "outbox id is not a valid HeerId {id_raw}: {e}"
            )))
        })?;
        let row_id = row
            .try_get(1)
            .map_err(|e| DjogiError::Db(DbError::other(format!("outbox row_id decode: {e}"))))?;
        let action = row
            .try_get(2)
            .map_err(|e| DjogiError::Db(DbError::other(format!("outbox action decode: {e}"))))?;
        let payload = row
            .try_get(3)
            .map_err(|e| DjogiError::Db(DbError::other(format!("outbox payload decode: {e}"))))?;
        let state = row
            .try_get(4)
            .map_err(|e| DjogiError::Db(DbError::other(format!("outbox state decode: {e}"))))?;

        out.push(OutboxRowForTest {
            id,
            row_id,
            action,
            payload,
            state,
        });
    }

    Ok(out)
}

/// Delete all rows from a framework-owned outbox table for integration tests.
#[doc(hidden)]
pub async fn clear_outbox_for_test(ctx: &mut DjogiContext, table: &str) -> Result<(), DjogiError> {
    validate_outbox_table_for_test(table)?;
    ctx.raw_execute(&format!("DELETE FROM {table}"), &[])
        .await?;
    Ok(())
}

/// Build the additive [`MigrationPlan`]s [`sync_models`] would execute
/// for `descriptors`, without touching any database.
///
/// Steps 0–5 of [`sync_models`]: relation-accessor registry gate,
/// pre-flight FK check, projection, empty-source diff, additive-only
/// invariant check, and per-bucket `plan_delta` lowering. Returns one
/// plan per non-`NoOp` bucket in [`BucketKey`] order. An empty
/// `descriptors` slice returns an empty vec (matching `sync_models`'s
/// zero-DDL no-op contract); the registry gate is also skipped in that
/// case because no schema would be produced anyway.
///
/// Used by [`sync_models`] itself and by parity tests that need to
/// compare the additive sync plan against the production migration
/// runner — surfacing the plans as data is what lets a test feed
/// the same `MigrationPlan` to both `sync_models`'s `execute_plan`
/// and `migrate::apply_plan`, proving the two execution wrappers
/// produce identical schema state.
///
/// # Pre-flight registry gate (GH #158)
///
/// Step 0 invokes
/// [`crate::relation::registry::validate_global_relation_accessor_registry`]
/// before any descriptor work. The gate catches cross-kind reverse /
/// M2M accessor collisions that rustc cannot see (the colliding macros
/// emit different trait suffixes — `…ReverseRelation` vs
/// `…ManyToManyRelation` — so both compile and the clash only
/// manifests at downstream "ambiguous method call" call sites). A
/// failure surfaces as [`DjogiError::Db`] carrying the registry
/// diagnostic, anchoring the fix at the relation registry metadata
/// rather than at an arbitrary call site.
pub fn build_sync_plans(
    descriptors: &[&'static ModelDescriptor],
) -> Result<Vec<MigrationPlan>, DjogiError> {
    if descriptors.is_empty() {
        return Ok(Vec::new());
    }

    // ── Step 0 — relation-accessor registry gate (GH #158). Walks the
    //              link-time-collected `ReverseRelationMarker` inventory
    //              and surfaces any cross-kind `(source, name)` pair
    //              that rustc cannot catch (the colliding macros emit
    //              different trait suffixes, so both compile cleanly).
    //              Run before the per-descriptor work so a hostile
    //              registry never reaches the differ / planner; the
    //              error names every offending pair in one pass.
    wrap_relation_registry_for_sync_models(
        crate::relation::registry::validate_global_relation_accessor_registry(),
    )?;

    // ── Step 1 — pre-flight: every FK target named on a descriptor
    //              must also be in the supplied list. Macro-time
    //              detection is impossible because only type paths
    //              are visible; descriptors are runtime data.
    let supplied_type_names: BTreeSet<&'static str> =
        descriptors.iter().map(|d| d.type_name).collect();
    for d in descriptors {
        for f in d.fields {
            if f.relation_kind.is_none() {
                continue;
            }
            // A field with a relation_kind MUST carry a target_type_name —
            // both ForeignKey and OneToOne point at a concrete model type.
            // A None here signals a broken macro emission rather than a
            // legitimate "no target" case; surface it explicitly so the
            // operator sees a precise error instead of a silent skip.
            let Some(target) = f.target_type_name else {
                return Err(DjogiError::Db(DbError::other(format!(
                    "sync_models: model `{src}`.`{col}` has relation_kind \
                     but no target_type_name — this is a macro emission bug; \
                     please file an issue",
                    src = d.type_name,
                    col = f.name,
                ))));
            };
            if !supplied_type_names.contains(target) {
                return Err(DjogiError::Db(DbError::other(format!(
                    "sync_models: model `{src}`.`{col}` references `{tgt}` via foreign key, \
                     but `{tgt}` is not in sync_models. \
                     Add `{tgt}` to sync_models = [...] alongside `{src}`.",
                    src = d.type_name,
                    col = f.name,
                    tgt = target,
                ))));
            }
        }
    }

    // ── Step 2 — project the supplied descriptors. Enums are
    //              global, so feed the full inventory; the projection
    //              attaches them to every bucket that holds a model.
    //              Apps come from `AppRegistry::all` so model app
    //              labels resolve to their declared database target.
    //
    //              `generated_at` does not influence DDL emission;
    //              the schema's timestamp is informational. Pin it to
    //              `rfc3339_now_seconds` for parity with production.
    let target = project_from_iters(
        descriptors.iter().copied(),
        inventory::iter::<EnumDescriptor>(),
        AppRegistry::all().iter(),
        rfc3339_now_seconds(),
    )
    .map_err(|e| {
        DjogiError::Db(DbError::other(format!(
            "sync_models: descriptor projection failed: {e:?}"
        )))
    })?;

    // ── Step 3 — empty-source diff. Build a `before` map mirroring
    //              the target map's bucket keys; each `before` value
    //              is a fresh empty `AppliedSchema`. The differ then
    //              emits one `AppliedSchema` per bucket, all additive.
    let before: BTreeMap<BucketKey, AppliedSchema> = target
        .keys()
        .map(|bucket| (bucket.clone(), empty_applied_schema()))
        .collect();

    // B-4r: differ now returns Result. The PK-flip cascade-depth
    // contract can fail here even though `sync_models` only ever
    // produces additive deltas (empty before-schema), because the
    // differ runs the closure unconditionally — the empty-target
    // path just happens to always produce `Additive` ops, but the
    // closure still validates the FK shape.
    let deltas = diff_bucket_maps(&before, &target)
        .map_err(|e| DjogiError::Db(DbError::other(format!("sync_models differ failed: {e}"))))?;

    // ── Step 4 — invariant + classification check. The empty-target
    //              path can only produce `Additive` or `NoOp`
    //              (buckets with zero models). Anything else is a
    //              broken invariant in the differ.
    for delta in &deltas {
        match delta.classification {
            Classification::NoOp | Classification::Additive => {}
            ref other => {
                return Err(DjogiError::Db(DbError::other(format!(
                    "sync_models: internal invariant violated — empty-target diff classified as \
                     `{other:?}` for bucket `{db}/{app}`; expected NoOp or Additive only",
                    db = delta.bucket.database,
                    app = delta.bucket.app,
                ))));
            }
        }
        for op in &delta.operations {
            if !is_additive_op(op) {
                return Err(DjogiError::Db(DbError::other(format!(
                    "sync_models: internal invariant violated — empty-target diff produced \
                     destructive op `{kind}` for bucket `{db}/{app}`",
                    kind = additive_op_label(op),
                    db = delta.bucket.database,
                    app = delta.bucket.app,
                ))));
            }
        }
    }

    // ── Step 5 — lower each non-NoOp delta to a plan. Bucket order
    //              is the natural BTreeMap order (alphabetic by
    //              `(database, app)`), so multi-bucket runs are
    //              deterministic.
    let mut plans = Vec::new();
    for delta in &deltas {
        if matches!(delta.classification, Classification::NoOp) {
            continue;
        }
        let plan = plan_delta(delta).map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "sync_models: SQL emission failed for bucket `{db}/{app}`: {e:?}",
                db = delta.bucket.database,
                app = delta.bucket.app,
            )))
        })?;
        plans.push(plan);
    }
    Ok(plans)
}

/// Wrap a [`crate::relation::registry::validate_global_relation_accessor_registry`]
/// result into the [`DjogiError::Db`] surface `build_sync_plans` /
/// `sync_models` use for every other failure mode. Extracted so unit
/// tests can drive the wrapping path with a synthetic registry error
/// without polluting the link-time-collected
/// [`crate::relation::registry::ReverseRelationMarker`] inventory —
/// `inventory::submit!` from a test would persist for every other test
/// in the same binary.
fn wrap_relation_registry_for_sync_models(
    result: Result<(), crate::relation::registry::RelationRegistryError>,
) -> Result<(), DjogiError> {
    result.map_err(|e| {
        DjogiError::Db(DbError::other(format!(
            "sync_models: relation accessor registry contains cross-kind \
             collisions (GH #158); fix the colliding relation macro declaration(s) \
             before re-running sync_models:\n{e}"
        )))
    })
}

/// Build a fresh, empty [`AppliedSchema`] suitable as the "before"
/// state in `sync_models`'s empty-target diff.
///
/// The shape mirrors what [`project_from_iters`] produces for a
/// bucket with zero models — empty `models`, `enums`, `indexes`,
/// `registered_apps`. The differ keys equality off all observable
/// fields (`PartialEq` derive), so leaving `djogi_version` /
/// `generated_at` as the same defaults the projection uses keeps
/// `before == empty_target_bucket` and does not perturb the diff.
fn empty_applied_schema() -> AppliedSchema {
    AppliedSchema {
        djogi_version: env!("CARGO_PKG_VERSION").to_string(),
        enums: BTreeMap::new(),
        format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
        generated_at: rfc3339_now_seconds(),
        indexes: Vec::new(),
        models: BTreeMap::new(),
        registered_apps: Vec::new(),
    }
}

/// Returns `true` for every [`SchemaOperation`] variant that the
/// empty-target path can legitimately produce.
///
/// Additive ops on an empty `before` schema:
/// - `AddTable`, `AddColumn`, `AddIndex`, `AddEnum`, `AddEnumVariant`,
///   `AddForeignKey`.
///
/// Anything else (drops, renames, alters, app moves, PK flips) is
/// structurally impossible from an empty `before` and signals a
/// broken differ invariant.
fn is_additive_op(op: &SchemaOperation) -> bool {
    matches!(
        op,
        SchemaOperation::AddTable(_)
            | SchemaOperation::AddColumn { .. }
            | SchemaOperation::AddIndex(_)
            | SchemaOperation::AddEnum(_)
            | SchemaOperation::AddEnumVariant { .. }
            | SchemaOperation::AddForeignKey { .. }
    )
}

/// Operator-facing variant label for the invariant-violation error
/// message. We keep this separate from `is_additive_op` so the error
/// payload names the offending variant precisely without depending on
/// `Debug` formatting (which would also dump the variant's payload).
fn additive_op_label(op: &SchemaOperation) -> &'static str {
    match op {
        SchemaOperation::AddTable(_) => "AddTable",
        SchemaOperation::DropTable(_) => "DropTable",
        SchemaOperation::RenameTable { .. } => "RenameTable",
        SchemaOperation::AddColumn { .. } => "AddColumn",
        SchemaOperation::DropColumn { .. } => "DropColumn",
        SchemaOperation::RenameColumn { .. } => "RenameColumn",
        SchemaOperation::AlterColumn { .. } => "AlterColumn",
        SchemaOperation::AddForeignKey { .. } => "AddForeignKey",
        SchemaOperation::DropForeignKey { .. } => "DropForeignKey",
        SchemaOperation::AddIndex(_) => "AddIndex",
        SchemaOperation::DropIndex(_) => "DropIndex",
        SchemaOperation::AddExclusionConstraint { .. } => "AddExclusionConstraint",
        SchemaOperation::DropExclusionConstraint { .. } => "DropExclusionConstraint",
        SchemaOperation::SetTableComment { .. } => "SetTableComment",
        SchemaOperation::SetStorageParams { .. } => "SetStorageParams",
        SchemaOperation::SetTablespace { .. } => "SetTablespace",
        SchemaOperation::AddEnum(_) => "AddEnum",
        SchemaOperation::DropEnum(_) => "DropEnum",
        SchemaOperation::AddEnumVariant { .. } => "AddEnumVariant",
        SchemaOperation::RenameApp { .. } => "RenameApp",
        SchemaOperation::MoveModelBetweenApps { .. } => "MoveModelBetweenApps",
        SchemaOperation::PkTypeFlip { .. } => "PkTypeFlip",
        SchemaOperation::PkTypeFlipGroup(_) => "PkTypeFlipGroup",
        SchemaOperation::PkTypeFlipMultiGroup(_) => "PkTypeFlipMultiGroup",
        SchemaOperation::Unsupported { .. } => "Unsupported",
    }
}

/// Execute every statement in `plan` via `ctx.raw_ddl`. Single
/// connection, no advisory lock, no ledger updates — this is the
/// per-test path; production migrations route through the runner
/// (T4) for the same composition output.
///
/// `MetadataOnly` segments are skipped entirely — they exist for the
/// production runner's folder-rename / app-move bookkeeping and have
/// no DDL to execute. Empty-target diffs cannot produce them, but the
/// guard makes the helper robust against future T2 differ changes.
async fn execute_plan(
    ctx: &mut DjogiContext,
    plan: &crate::migrate::MigrationPlan,
) -> Result<(), DjogiError> {
    for segment in &plan.segments {
        if matches!(segment.kind, SegmentKind::MetadataOnly) {
            continue;
        }
        for op_sql in &segment.statements {
            ctx.raw_ddl(&op_sql.up).await.map_err(|e| {
                // Surface both the operator-facing label AND the SQL
                // text so failures point at the exact statement that
                // tripped Postgres. Without the SQL, debugging
                // descriptor / projection bugs from a test failure
                // is a hunt — every `AddTable` op surfaces only the
                // table name, not the emitted DDL. The tail-`{e}` is
                // the underlying `DjogiError`, which carries the
                // server-side `SqlState` + message in the source
                // chain.
                DjogiError::Db(DbError::other(format!(
                    "sync_models: failed to apply `{label}` on bucket `{db}/{app}`: {e}\n\
                     -- offending SQL --\n{sql}",
                    label = op_sql.label,
                    db = plan.bucket.database,
                    app = plan.bucket.app,
                    sql = op_sql.up,
                )))
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        TEST_NON_SUPERUSER_PASSWORD, TEST_NON_SUPERUSER_ROLE, build_non_superuser_url,
        replace_db_in_url, sql_string_literal, validate_extension_name,
    };

    #[test]
    fn replace_db_preserves_host_and_port() {
        let url = "postgres://user:pass@localhost:5432/old_db";
        let result = replace_db_in_url(url, "new_db").unwrap();
        assert_eq!(result, "postgres://user:pass@localhost:5432/new_db");
    }

    #[test]
    fn replace_db_no_port() {
        let url = "postgres://user:pass@localhost/old_db";
        let result = replace_db_in_url(url, "new_db").unwrap();
        assert_eq!(result, "postgres://user:pass@localhost/new_db");
    }

    #[test]
    fn replace_db_postgresql_scheme() {
        let url = "postgresql://localhost/old_db";
        let result = replace_db_in_url(url, "fresh_db").unwrap();
        assert_eq!(result, "postgresql://localhost/fresh_db");
    }

    #[test]
    fn validate_extension_name_accepts_real_extensions() {
        validate_extension_name("postgis").unwrap();
        validate_extension_name("pg_trgm").unwrap();
        validate_extension_name("pgcrypto").unwrap();
        validate_extension_name("_leading_underscore").unwrap();
        validate_extension_name("WithMixedCase").unwrap();
        validate_extension_name("ext1").unwrap();
    }

    #[test]
    fn validate_extension_name_rejects_empty() {
        let err = validate_extension_name("").unwrap_err();
        assert!(err.to_string().contains("must not be empty"), "{err}");
    }

    #[test]
    fn validate_extension_name_rejects_too_long() {
        let name = "a".repeat(64);
        let err = validate_extension_name(&name).unwrap_err();
        assert!(err.to_string().contains("exceeds 63-byte"), "{err}");
    }

    #[test]
    fn validate_extension_name_rejects_leading_digit() {
        let err = validate_extension_name("1ext").unwrap_err();
        assert!(
            err.to_string().contains("must start with"),
            "error message should mention leading character rule: {err}",
        );
    }

    #[test]
    fn validate_extension_name_rejects_injection_attempts() {
        // These would be catastrophic if interpolated into SQL unvalidated.
        // Each must fail validation before any DB work happens.
        for candidate in [
            "postgis; DROP DATABASE postgres",
            "a\"b",
            "a b",
            "a-b",
            "a.b",
            "a/b",
            "\"postgis\"",
        ] {
            let err = validate_extension_name(candidate).unwrap_err();
            assert!(
                err.to_string().contains("invalid byte")
                    || err.to_string().contains("must start with"),
                "expected validation failure for `{candidate}`, got: {err}",
            );
        }
    }

    // ── sync_models invariants ──────────────────────────────────────
    //
    // These tests pin the empty-target additive-only contract without
    // touching a live database. The invariant guard sits between the
    // differ and the SQL emitter, so we exercise it by walking
    // synthetic operations through the pure helper functions.

    use super::teardown_test_db;
    use super::{additive_op_label, build_sync_plans, is_additive_op, setup_test_db, sync_models};
    use crate::descriptor::{
        FieldDescriptor, FieldSqlType, PkType, field_descriptor, model_descriptor,
    };
    use crate::migrate::diff::SchemaOperation;
    use crate::migrate::schema::{OnDeleteSchema, PkKindSchema, PrimaryKeySchema, TableSchema};

    const NUMERIC_ARRAY_MODEL_FIELDS: &[FieldDescriptor] = &[
        field_descriptor("id", FieldSqlType::BigInt, false),
        field_descriptor("values", FieldSqlType::NumericArray, true),
    ];
    const NUMERIC_ARRAY_MODEL_DESCRIPTOR: crate::descriptor::ModelDescriptor = model_descriptor(
        "NumericArrayFixture",
        "numeric_arrays",
        PkType::HeerId,
        NUMERIC_ARRAY_MODEL_FIELDS,
    );

    /// Build a minimal `TableSchema` so we can construct synthetic
    /// `AddTable` / `DropTable` operations without spinning up the
    /// full descriptor projection. Columns / PK details are not
    /// inspected by [`is_additive_op`] / [`additive_op_label`] — both
    /// dispatch on the variant tag — so the placeholder is enough.
    fn synthetic_table(name: &str) -> TableSchema {
        TableSchema {
            app: None,
            columns: Vec::new(),
            exclusion_constraints: Vec::new(),
            fts: None,
            is_through: false,
            moved_from_app: None,
            partition: None,
            primary_key: PrimaryKeySchema {
                columns: vec!["id".to_string()],
                kind: PkKindSchema::HeerId,
            },
            rationale: None,
            renamed_from: None,
            rls_enabled: false,
            table: name.to_string(),
            table_comment: None,
            storage_params: None,
            tablespace: None,
            tenant_key: None,
        }
    }

    #[test]
    fn is_additive_op_classifies_add_variants_as_additive() {
        // Every `Add*` variant of `SchemaOperation` is additive on an
        // empty-target diff. The synthetic table we build is a stand-in
        // — `is_additive_op` only inspects the variant tag.
        let t = synthetic_table("widgets");
        assert!(is_additive_op(&SchemaOperation::AddTable(t)));
        assert!(is_additive_op(&SchemaOperation::AddEnum(
            crate::migrate::schema::EnumSchema {
                name: "color".to_string(),
                variants: vec!["red".to_string()],
            }
        )));
        assert!(is_additive_op(&SchemaOperation::AddForeignKey {
            table: "widgets".to_string(),
            column: "category_id".to_string(),
            fk: crate::migrate::schema::ForeignKeySchema {
                deferrable: false,
                initially_deferred: false,
                on_delete: OnDeleteSchema::Restrict,
                ref_column: "id".to_string(),
                ref_table: "categories".to_string(),
            },
        }));
    }

    #[test]
    fn is_additive_op_classifies_drop_variants_as_destructive() {
        // The empty-target invariant guard must reject every `Drop*`
        // (and rename / alter / app-move / PK-flip / unsupported)
        // variant — none of them should ever appear on an empty-target
        // diff. If the differ ever produces one, `sync_models`
        // surfaces a clear "invariant violated" error rather than
        // silently executing destructive DDL.
        assert!(!is_additive_op(&SchemaOperation::DropTable(
            "widgets".to_string()
        )));
        assert!(!is_additive_op(&SchemaOperation::DropColumn {
            table: "widgets".to_string(),
            column: "name".to_string(),
        }));
        assert!(!is_additive_op(&SchemaOperation::DropEnum(
            "color".to_string()
        )));
        assert!(!is_additive_op(&SchemaOperation::RenameTable {
            from: "old".to_string(),
            to: "new".to_string(),
        }));
        assert!(!is_additive_op(&SchemaOperation::DropForeignKey {
            table: "widgets".to_string(),
            column: "category_id".to_string(),
            fk: crate::migrate::schema::ForeignKeySchema {
                deferrable: false,
                initially_deferred: false,
                on_delete: OnDeleteSchema::Restrict,
                ref_column: "id".to_string(),
                ref_table: "categories".to_string(),
            },
        }));
    }

    #[test]
    fn additive_op_label_emits_variant_name_for_each_kind() {
        // Sanity-check the operator-facing label emitted in invariant-
        // violation error messages. A few representative variants is
        // enough — the function is a flat match arm so any one
        // mismatch would be a typo, not a logic bug.
        assert_eq!(
            additive_op_label(&SchemaOperation::AddTable(synthetic_table("x"))),
            "AddTable",
        );
        assert_eq!(
            additive_op_label(&SchemaOperation::DropTable("x".to_string())),
            "DropTable",
        );
        assert_eq!(
            additive_op_label(&SchemaOperation::RenameTable {
                from: "a".to_string(),
                to: "b".to_string(),
            }),
            "RenameTable",
        );
    }

    #[test]
    fn build_sync_plans_includes_numeric_array_helper_operation() {
        let plans =
            build_sync_plans(&[&NUMERIC_ARRAY_MODEL_DESCRIPTOR]).expect("plan should build");
        let plan = plans
            .into_iter()
            .next()
            .expect("sync_models for one descriptor should yield one plan");
        let labels: Vec<&str> = plan
            .segments
            .iter()
            .flat_map(|s| s.statements.iter())
            .map(|s| s.label.as_str())
            .collect();
        assert!(
            labels.contains(&"Ensure djogi numeric-array helper"),
            "plan should preload helper prelude for NUMERIC[] checks; labels: {labels:?}"
        );
        assert!(
            plan.segments
                .iter()
                .flat_map(|s| s.statements.iter())
                .any(|op| op
                    .up
                    .contains("djogi.__djogi_numeric_array_is_rust_decimal_v1(")),
            "table DDL from NUMERIC[] descriptor should reference helper in CHECK",
        );
    }

    #[tokio::test]
    async fn sync_models_creates_numeric_array_helper_in_postgres_when_database_url_present() {
        use std::env;
        let database_url = env::var("DATABASE_URL").ok();
        if !matches!(&database_url, Some(url) if !url.is_empty()) {
            return;
        }

        let (cleanup, mut ctx) = setup_test_db()
            .await
            .expect("setup_test_db should provision test database");
        sync_models(&mut ctx, &[&NUMERIC_ARRAY_MODEL_DESCRIPTOR])
            .await
            .expect("sync_models should apply helper-backed NUMERIC[] table");

        let helper_exists = ctx
            .query_one(
                "SELECT EXISTS(\
                    SELECT 1 \
                    FROM pg_catalog.pg_proc p \
                    INNER JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
                    WHERE n.nspname = 'djogi' \
                    AND p.proname = '__djogi_numeric_array_is_rust_decimal_v1'\
                )",
                &[],
            )
            .await
            .expect("helper function existence check should execute")
            .get::<_, bool>(0);
        teardown_test_db(cleanup).await;
        assert!(
            helper_exists,
            "sync_models must create `djogi.__djogi_numeric_array_is_rust_decimal_v1`"
        );
    }

    // ── non-superuser test pool helpers ─────────────────────────────────

    #[test]
    fn sql_string_literal_wraps_in_single_quotes() {
        assert_eq!(sql_string_literal("djogi_test_user"), "'djogi_test_user'");
        assert_eq!(sql_string_literal(""), "''");
    }

    #[test]
    fn sql_string_literal_doubles_embedded_single_quote() {
        // Pre-fix would let `' OR 1=1; DROP …` through unescaped. The
        // doubling rule keeps every embedding site safe regardless of
        // the source of the value.
        assert_eq!(sql_string_literal("a'b"), "'a''b'");
        assert_eq!(sql_string_literal("'leading"), "'''leading'");
        assert_eq!(sql_string_literal("trailing'"), "'trailing'''");
    }

    #[test]
    fn build_non_superuser_url_swaps_userinfo_and_database() {
        // Admin connects as `djogi:djogi` to the maintenance DB; the
        // derived URL points at the per-test DB as the non-superuser
        // role with its compile-time password.
        let url = build_non_superuser_url(
            "postgres://djogi:djogi@localhost:5432/djogi_test",
            "djogi_test_abc123",
        )
        .expect("valid URL must round-trip");
        assert_eq!(
            url,
            format!(
                "postgres://{role}:{pass}@localhost:5432/djogi_test_abc123",
                role = TEST_NON_SUPERUSER_ROLE,
                pass = TEST_NON_SUPERUSER_PASSWORD,
            ),
        );
    }

    #[test]
    fn build_non_superuser_url_preserves_query_string() {
        let url = build_non_superuser_url(
            "postgres://djogi:djogi@localhost:5432/djogi_test?sslmode=disable",
            "djogi_test_xyz",
        )
        .expect("valid URL with query must round-trip");
        assert!(
            url.ends_with("/djogi_test_xyz?sslmode=disable"),
            "query string must be preserved on splice; got: {url}",
        );
    }

    #[test]
    fn build_non_superuser_url_rejects_identity_override_query_params() {
        for key in ["user", "password", "dbname"] {
            let url = format!("postgres://localhost/djogi_test?sslmode=disable&{key}=djogi");
            let err = build_non_superuser_url(&url, "djogi_test_xyz")
                .expect_err("identity-overriding query params must be rejected");
            assert!(
                err.to_string().contains("would override"),
                "error must explain identity override for {key}; got: {err}",
            );
        }
    }

    #[test]
    fn build_non_superuser_url_rejects_percent_encoded_identity_override_keys() {
        let err = build_non_superuser_url(
            "postgres://localhost/djogi_test?%75ser=djogi",
            "djogi_test_xyz",
        )
        .expect_err("percent-encoded `user` key must be rejected");
        assert!(
            err.to_string().contains("would override"),
            "error must explain identity override; got: {err}",
        );
    }

    #[test]
    fn build_non_superuser_url_handles_postgresql_scheme() {
        let url = build_non_superuser_url("postgresql://localhost/djogi_test", "djogi_test_001")
            .expect("postgresql:// scheme accepted");
        assert!(
            url.starts_with("postgresql://"),
            "scheme preserved on splice; got: {url}",
        );
        assert!(
            url.ends_with("/djogi_test_001"),
            "database swapped; got: {url}",
        );
    }

    #[test]
    fn build_non_superuser_url_strips_existing_userinfo() {
        // The admin URL's user is the connecting superuser; the derived
        // non-superuser URL must replace, not append, the userinfo.
        let url = build_non_superuser_url(
            // djogi-allow-secret: synthetic admin URL used to exercise
            // userinfo stripping; `secret` is a placeholder password.
            "postgres://admin:secret@db.local:5432/main",
            "djogi_test_001",
        )
        .expect("admin URL with userinfo accepted");
        assert!(
            !url.contains("admin:secret"),
            "previous userinfo must be stripped; got: {url}",
        );
        assert!(
            url.contains(&format!(
                "{TEST_NON_SUPERUSER_ROLE}:{TEST_NON_SUPERUSER_PASSWORD}@"
            )),
            "non-superuser userinfo must be present; got: {url}",
        );
    }

    #[test]
    fn build_non_superuser_url_rejects_missing_database() {
        let err = build_non_superuser_url("postgres://localhost", "djogi_test_001")
            .expect_err("URL without /db component must be rejected");
        assert!(
            err.to_string().contains("does not contain a database name"),
            "error must explain the missing path; got: {err}",
        );
    }

    #[test]
    fn build_non_superuser_url_rejects_unknown_scheme() {
        let err = build_non_superuser_url("mysql://localhost/main", "djogi_test_001")
            .expect_err("non-postgres schemes must be rejected");
        assert!(
            err.to_string().contains("postgres://"),
            "error must mention the supported schemes; got: {err}",
        );
    }

    #[test]
    fn outbox_table_for_test_rejects_reserved_djogi_prefix() {
        // `outbox_rows_for_test` / `clear_outbox_for_test` are hidden
        // testing helpers, but they still accept a caller-supplied SQL
        // identifier string. Keep them on the same reserved-prefix rule
        // as the production outbox worker table validator.
        let err = super::validate_outbox_table_for_test("__djogi_outbox")
            .expect_err("must reject framework-reserved prefix");
        let msg = err.to_string();
        assert!(msg.contains("ReservedDjogiPrefix"), "got: {msg}");
        assert!(super::validate_outbox_table_for_test("__djogi_").is_err());
        assert!(super::validate_outbox_table_for_test("app_outbox").is_ok());
        assert!(super::validate_outbox_table_for_test("_djogi_outbox").is_ok());
    }

    // ── GH #158 — relation-registry gate inside build_sync_plans ─────────

    #[test]
    fn wrap_relation_registry_for_sync_models_passes_ok_through() {
        // The clean-registry path must not perturb the existing
        // `Result<(), DjogiError>` flow — `Ok(())` in, `Ok(())` out,
        // no string formatting, no allocation.
        super::wrap_relation_registry_for_sync_models(Ok(()))
            .expect("Ok input must propagate unchanged");
    }

    #[test]
    fn wrap_relation_registry_for_sync_models_wraps_collision_into_djogi_error() {
        // Build a synthetic `RelationRegistryError` via the public API
        // so the wrap path sees the exact shape the live walker would
        // emit. Polluting the link-time-collected
        // `inventory::iter::<ReverseRelationMarker>` is intentionally
        // avoided — submitting a colliding marker would persist for
        // every other test in the lib's test binary.
        use crate::relation::registry::{RelationKind, validate_relation_accessor_collisions};
        let make = |kind, source, name, target, via| {
            crate::relation::registry::__macro_support::__make_reverse_relation_marker(
                kind, source, name, target, via,
            )
        };
        let markers = [
            make(RelationKind::FK, "Owner", "cars", "Vehicle", "owner_id"),
            make(RelationKind::M2M, "Owner", "cars", "Garage", "owner_id"),
        ];
        let registry_err = validate_relation_accessor_collisions(markers.iter())
            .expect_err("synthetic FK + M2M markers must collide");

        let err = super::wrap_relation_registry_for_sync_models(Err(registry_err))
            .expect_err("registry error must surface as DjogiError::Db");

        // Diagnostic anchors — the message must direct the operator at
        // the relation metadata + carry the GH issue number, and it
        // must mention `sync_models` so a confused reader scanning a
        // `cargo test` log can connect the dot to this code path
        // rather than to `project_from_inventory` (which uses a
        // different prefix).
        let msg = err.to_string();
        assert!(
            msg.contains("sync_models"),
            "missing entry-point anchor: {msg}"
        );
        assert!(msg.contains("GH #158"), "missing issue anchor: {msg}");
        assert!(msg.contains("Owner"), "missing source: {msg}");
        assert!(msg.contains("cars"), "missing accessor: {msg}");
        assert!(msg.contains("FK"), "missing FK kind: {msg}");
        assert!(msg.contains("M2M"), "missing M2M kind: {msg}");
    }
}
